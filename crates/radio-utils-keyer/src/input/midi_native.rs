//! Native MIDI paddle input using `midir`.
#![cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use midir::{MidiInput, MidiInputConnection};

use crate::config::MidiBinding;
use crate::input::midi::RawMidiEvent;
use crate::input::{PaddleInput, PaddleState};

use super::MidiSharedState as SharedState;

/// Paddle input backed by a MIDI device (native platforms).
pub struct MidiPaddleInput {
    shared: Arc<(Mutex<SharedState>, Condvar)>,
    _connection: MidiInputConnection<()>,
}

impl MidiPaddleInput {
    /// Open the named MIDI port.
    ///
    /// Returns `Ok((input, monitor_rx))` on success, where `monitor_rx` receives
    /// every raw MIDI event seen on the port (for settings UI learn mode).
    /// Returns `Err` if the port is not found or cannot be opened.
    pub fn new(
        device_name: &str,
        dit_binding: Option<MidiBinding>,
        dah_binding: Option<MidiBinding>,
    ) -> Result<(Self, Receiver<RawMidiEvent>), String> {
        let midi_in =
            MidiInput::new("radio-utils-keyer").map_err(|e| format!("MIDI init failed: {e}"))?;

        let port = midi_in
            .ports()
            .into_iter()
            .find(|p| midi_in.port_name(p).as_deref() == Ok(device_name))
            .ok_or_else(|| format!("MIDI port not found: {device_name}"))?;

        let shared = Arc::new((
            Mutex::new(SharedState {
                dit: false,
                dash: false,
                timestamp: Instant::now(),
                generation: 0,
            }),
            Condvar::new(),
        ));
        let shared_cb = Arc::clone(&shared);

        let (monitor_tx, monitor_rx): (Sender<RawMidiEvent>, Receiver<RawMidiEvent>) = unbounded();

        let dit_b = dit_binding;
        let dah_b = dah_binding;

        let connection = midi_in
            .connect(
                &port,
                "radio-utils-paddle",
                move |_stamp, bytes, _| {
                    let Some(ev) = RawMidiEvent::from_bytes(bytes) else {
                        return;
                    };
                    let _ = monitor_tx.try_send(ev);

                    let (lock, cvar) = &*shared_cb;
                    let mut state = lock.lock().unwrap();
                    let mut dit = state.dit;
                    let mut dash = state.dash;
                    if let Some(ref b) = dit_b {
                        if ev.matches(b) {
                            dit = ev.is_down;
                        }
                    }
                    if let Some(ref b) = dah_b {
                        if ev.matches(b) {
                            dash = ev.is_down;
                        }
                    }
                    if state.dit != dit || state.dash != dash {
                        state.dit = dit;
                        state.dash = dash;
                        state.timestamp = Instant::now();
                        state.generation = state.generation.wrapping_add(1);
                        cvar.notify_one();
                    }
                },
                (),
            )
            .map_err(|e| format!("MIDI connect failed: {e}"))?;

        Ok((
            Self {
                shared,
                _connection: connection,
            },
            monitor_rx,
        ))
    }
}

impl PaddleInput for MidiPaddleInput {
    fn wait_for_change(&mut self, timeout: Option<Duration>) -> PaddleState {
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap();
        let seen_gen = state.generation;
        // Cap "no timeout" at 1 s to prevent permanent deadlock if the MIDI
        // callback thread exits abnormally without notifying the condvar.
        let t = timeout.unwrap_or(Duration::from_secs(1));
        let deadline = Instant::now() + t;
        while state.generation == seen_gen {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (g, timed_out) = cvar.wait_timeout(state, remaining).unwrap();
            state = g;
            if timed_out.timed_out() {
                break;
            }
        }
        PaddleState {
            dit: state.dit,
            dash: state.dash,
            timestamp: state.timestamp,
        }
    }

    fn read(&self) -> PaddleState {
        let (lock, _) = &*self.shared;
        let state = lock.lock().unwrap();
        PaddleState {
            dit: state.dit,
            dash: state.dash,
            timestamp: state.timestamp,
        }
    }

    fn describe(&self) -> String {
        "MIDI paddle input".to_string()
    }
}
