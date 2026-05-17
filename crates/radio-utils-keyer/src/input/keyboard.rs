use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};

use super::{PaddleEvent, PaddleInput, PaddleState};

/// Shared notification primitive so the sender can wake the receiver.
struct Notify {
    mu: Mutex<bool>,
    cv: Condvar,
}

impl Notify {
    fn new() -> Self {
        Self {
            mu: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    /// Wake the waiting side.
    fn notify(&self) {
        let mut flag = self.mu.lock().unwrap();
        *flag = true;
        self.cv.notify_one();
    }

    /// Wait until notified or `timeout` expires.
    /// Returns `true` if notified, `false` on timeout.
    fn wait(&self, timeout: Option<Duration>) -> bool {
        let mut flag = self.mu.lock().unwrap();
        match timeout {
            Some(dur) => {
                let deadline = Instant::now() + dur;
                while !*flag {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let (guard, result) = self.cv.wait_timeout(flag, remaining).unwrap();
                    flag = guard;
                    if result.timed_out() {
                        break;
                    }
                }
            }
            None => {
                while !*flag {
                    flag = self.cv.wait(flag).unwrap();
                }
            }
        }
        let was_notified = *flag;
        *flag = false;
        was_notified
    }
}

/// Keyboard-based paddle input.
///
/// A [`KeyboardPaddleSender`] sends [`PaddleEvent`]s from the UI thread;
/// `KeyboardPaddleInput` receives them and tracks the combined state.
pub struct KeyboardPaddleInput {
    rx: Receiver<PaddleEvent>,
    notify: Arc<Notify>,
    state: PaddleState,
}

/// Clone-able handle for sending paddle events into a [`KeyboardPaddleInput`].
#[derive(Clone)]
pub struct KeyboardPaddleSender {
    tx: Sender<PaddleEvent>,
    notify: Arc<Notify>,
}

impl KeyboardPaddleSender {
    /// Send a paddle event, waking the receiver if it is blocking.
    pub fn send(&self, event: PaddleEvent) {
        // Channel is unbounded so send never fails while the receiver exists.
        let _ = self.tx.send(event);
        self.notify.notify();
    }
}

impl KeyboardPaddleInput {
    /// Create a new keyboard paddle input and its sender half.
    pub fn new() -> (KeyboardPaddleSender, Self) {
        let (tx, rx) = unbounded();
        let notify = Arc::new(Notify::new());
        let sender = KeyboardPaddleSender {
            tx,
            notify: Arc::clone(&notify),
        };
        let input = Self {
            rx,
            notify,
            state: PaddleState::default(),
        };
        (sender, input)
    }

    /// Drain all pending events from the channel into `self.state`.
    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                PaddleEvent::DitDown => self.state.dit = true,
                PaddleEvent::DitUp => self.state.dit = false,
                PaddleEvent::DashDown => self.state.dash = true,
                PaddleEvent::DashUp => self.state.dash = false,
            }
            self.state.timestamp = Instant::now();
        }
    }
}

impl PaddleInput for KeyboardPaddleInput {
    fn wait_for_change(&mut self, timeout: Option<Duration>) -> PaddleState {
        // First, drain anything already in the channel.
        self.drain_events();

        // If there are no pending events, block on the condvar.
        // We only need to wait if the channel was empty after draining.
        // However, the task spec says "block until change or timeout", so we
        // check whether something new arrived; if not, wait.
        if self.rx.is_empty() {
            self.notify.wait(timeout);
        }

        // Drain whatever arrived while we were waiting.
        self.drain_events();
        self.state
    }

    fn read(&self) -> PaddleState {
        // Return the last known state without draining (non-blocking, &self).
        self.state
    }

    fn describe(&self) -> String {
        "Keyboard paddle input".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn keyboard_input_timeout_returns_current_state() {
        let (sender, mut input) = KeyboardPaddleInput::new();

        // Send a dit-down so state is non-default.
        sender.send(PaddleEvent::DitDown);

        let state = input.wait_for_change(Some(Duration::from_millis(50)));
        assert!(state.dit, "dit should be true after DitDown");
        assert!(!state.dash, "dash should still be false");

        // Now wait again with no new events — should timeout and return current state.
        let before = Instant::now();
        let state = input.wait_for_change(Some(Duration::from_millis(50)));
        let elapsed = before.elapsed();

        assert!(state.dit, "dit should remain true");
        assert!(!state.dash);
        // Should have waited roughly the timeout duration.
        assert!(
            elapsed >= Duration::from_millis(30),
            "should have waited near the timeout, but only waited {:?}",
            elapsed
        );
    }

    #[test]
    fn keyboard_input_wakes_on_event() {
        let (sender, mut input) = KeyboardPaddleInput::new();

        let before = Instant::now();

        // Spawn a thread that sends an event after a short delay.
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            sender.send(PaddleEvent::DashDown);
        });

        // Wait with a generous timeout — should wake well before it expires.
        let state = input.wait_for_change(Some(Duration::from_secs(5)));
        let elapsed = before.elapsed();

        assert!(state.dash, "dash should be true after DashDown");
        assert!(
            elapsed < Duration::from_secs(1),
            "should have woken quickly, but took {:?}",
            elapsed
        );
    }
}
