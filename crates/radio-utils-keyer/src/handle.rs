//! Keyer thread management — only compiled on native targets.
#![cfg(not(target_arch = "wasm32"))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::config::KeyerConfig;
use crate::engine::{KeyerEngine, KeyerOutput};
use crate::input::PaddleInput;

/// Command sent to the keyer thread.
#[derive(Debug)]
pub enum KeyerCommand {
    UpdateConfig(Box<KeyerConfig>),
    SendMacro(String),
    AbortMacro,
    Stop,
}

/// Handle for controlling the keyer from outside its thread.
pub struct KeyerHandle {
    cmd_tx: Sender<KeyerCommand>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl KeyerHandle {
    /// Spawn the "cw-keyer" thread and return a handle plus a clone of the
    /// output receiver.
    pub fn start(
        config: KeyerConfig,
        input: Box<dyn PaddleInput>,
    ) -> (Self, Receiver<KeyerOutput>) {
        let (cmd_tx, cmd_rx) = bounded::<KeyerCommand>(64);
        let (output_tx, output_rx) = bounded::<KeyerOutput>(256);

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        let thread = thread::Builder::new()
            .name("cw-keyer".into())
            .spawn(move || {
                keyer_loop(config, input, cmd_rx, output_tx, running_clone);
            })
            .expect("failed to spawn cw-keyer thread");

        let handle = Self {
            cmd_tx,
            running,
            thread: Some(thread),
        };

        (handle, output_rx)
    }

    /// Send an updated configuration to the keyer thread.
    pub fn update_config(&self, config: KeyerConfig) {
        let _ = self
            .cmd_tx
            .try_send(KeyerCommand::UpdateConfig(Box::new(config)));
    }

    /// Queue a text macro for CW sending.
    pub fn send_macro(&self, text: String) {
        let _ = self.cmd_tx.try_send(KeyerCommand::SendMacro(text));
    }

    /// Abort any in-progress text macro.
    ///
    /// Uses blocking send to ensure the abort is never silently dropped.
    pub fn abort_macro(&self) {
        if let Err(e) = self.cmd_tx.send(KeyerCommand::AbortMacro) {
            log::error!("failed to send abort_macro: {e}");
        }
    }

    /// Stop the keyer thread, joining it to completion.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.cmd_tx.try_send(KeyerCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for KeyerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The keyer thread's main loop.
///
/// Runs until `running` is set to false. Polls the paddle input for state
/// changes, drains commands from `cmd_rx`, and ticks the engine at 1 ms
/// resolution, forwarding any output events through `output_tx`.
fn keyer_loop(
    config: KeyerConfig,
    mut input: Box<dyn PaddleInput>,
    cmd_rx: Receiver<KeyerCommand>,
    output_tx: Sender<KeyerOutput>,
    running: Arc<AtomicBool>,
) {
    // On Android, elevate this thread to THREAD_PRIORITY_AUDIO (-16) so the
    // OS scheduler gives us CPU time within ~1 ms of our condvar timeout, even
    // under ART GC or system load. Without this the thread can be preempted for
    // 4–20 ms, causing the 1 ms cap to silently discard elapsed time and making
    // CW elements run slow.
    #[cfg(target_os = "android")]
    crate::input::midi_android::set_thread_priority_audio();

    // Windows: drop the system timer resolution from its ~15.6 ms default to
    // 1 ms. Without this, `wait_for_change(timeout = 1 ms)` actually sleeps
    // ~15 ms, the per-iteration 4 ms cap below silently discards the rest,
    // and the engine sees only ~4 of every ~15 ms of real wall-clock time —
    // so 60 WPM elements key out at ~16 WPM. `timeBeginPeriod` is
    // process-global; we intentionally leave it raised for the lifetime of
    // the process (the keyer thread runs until exit).
    #[cfg(target_os = "windows")]
    {
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(uPeriod: u32) -> u32;
        }
        // 0 = TIMERR_NOERROR, 97 = TIMERR_NOCANDO. Either way there's nothing
        // useful to do at runtime if it fails — keying will just run slow.
        unsafe {
            timeBeginPeriod(1);
        }
    }

    let mut engine = KeyerEngine::new(config);
    let mut last_tick = Instant::now();

    while running.load(Ordering::SeqCst) {
        // Choose timeout: fast poll when active, slow when idle.
        let timeout = if engine.is_active() {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(100)
        };

        // Block until paddle change or timeout.
        let state = input.wait_for_change(Some(timeout));
        engine.set_paddle(state.dit, state.dash);

        // Drain pending commands.
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                KeyerCommand::UpdateConfig(cfg) => engine.update_config(*cfg),
                KeyerCommand::SendMacro(text) => engine.send_text(&text),
                KeyerCommand::AbortMacro => engine.abort_text(),
                KeyerCommand::Stop => {
                    running.store(false, Ordering::SeqCst);
                }
            }
        }

        // Advance the engine by up to 4 ms of wall-clock time per loop
        // iteration.
        //
        // The 4ms cap prevents the double-element bug: all ticks within a
        // batch run with the same paddle state (set_paddle above), so if the
        // thread was idle for 70ms and a press arrives, an uncapped batch
        // could complete a full dit AND enter DitDelay with dit_held=true,
        // causing a spurious second element on a quick touch.
        //
        // Safety bound: a batch of 4 ticks must be shorter than the shortest
        // dit.  At the maximum practical CW speed of 60 WPM, dit = 20ms, so
        // 4ms is safely under 20%.  The engine's configured WPM ceiling should
        // never be raised above ~60 WPM without revisiting this cap.
        //
        // Raising from the old cap of 1 ms to 4 ms lets the engine recover
        // from Android scheduler jitter (threads can be delayed 2–4 ms under
        // GC load) without accumulating one-way timing drift.
        let now = Instant::now();
        let raw_elapsed = now.duration_since(last_tick).as_millis() as u32;
        let elapsed_ms = raw_elapsed.min(4);
        if elapsed_ms > 0 {
            if raw_elapsed > 4 {
                // Thread was starved beyond the cap — snap forward to avoid
                // unbounded catch-up that would replay stale paddle state.
                last_tick = now;
            } else {
                // Advance by exactly the consumed ticks so sub-millisecond
                // remainders accumulate into the next iteration instead of
                // being silently discarded (which made the keyer run slow).
                last_tick += Duration::from_millis(elapsed_ms as u64);
            }
            for _ in 0..elapsed_ms {
                if let Some(output) = engine.tick() {
                    if output_tx.try_send(output).is_err() {
                        log::warn!("[Keyer] Output channel full, dropping event");
                    }
                }
            }
        }
    }
}
