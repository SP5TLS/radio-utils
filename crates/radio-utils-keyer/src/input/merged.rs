//! Merged paddle input — combines multiple [`PaddleInput`] sources.
//!
//! Each source runs in its own polling thread ("keyer-input"). On any state
//! change the shared [`PaddleState`] is updated and a condvar is notified so
//! the keyer thread wakes on whichever source fires first.

use super::{PaddleInput, PaddleState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Shared state protected by a mutex + condvar.
struct SharedState {
    /// Current merged paddle state (logical OR would also be valid, but we
    /// simply store the latest update — the keyer reads the authoritative
    /// snapshot via `read()`).
    paddle: PaddleState,
    /// Monotonically increasing generation counter; bumped on every update so
    /// `wait_for_change` can detect new activity.
    generation: u64,
}

/// Merges several [`PaddleInput`] backends into one.
///
/// Each source is polled in a dedicated background thread; the first source to
/// report a change wakes the keyer thread.
pub struct MergedPaddleInput {
    shared: Arc<(Mutex<SharedState>, Condvar)>,
    /// Number of sources (kept for `describe()`).
    source_count: usize,
    /// Last generation seen by `wait_for_change`.
    last_generation: u64,
    /// Stop flag for background threads.
    stop: Arc<AtomicBool>,
    /// Join handles for background threads.
    threads: Vec<JoinHandle<()>>,
}

impl MergedPaddleInput {
    /// Create a new merged input from the given sources.
    ///
    /// Spawns one "keyer-input" thread per source.  Each thread polls its
    /// source with a 100 ms timeout and forwards state changes into the shared
    /// condvar.
    pub fn new(sources: Vec<Box<dyn PaddleInput>>) -> Self {
        let source_count = sources.len();

        let shared = Arc::new((
            Mutex::new(SharedState {
                paddle: PaddleState::default(),
                generation: 0,
            }),
            Condvar::new(),
        ));

        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();

        for mut source in sources {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            let handle = std::thread::Builder::new()
                .name("keyer-input".to_string())
                .spawn(move || {
                    let mut prev = source.read();
                    while !stop.load(Ordering::Acquire) {
                        let state = source.wait_for_change(Some(Duration::from_millis(100)));
                        if state.dit != prev.dit || state.dash != prev.dash {
                            prev = state;
                            let (lock, cv) = &*shared;
                            let mut guard = lock.lock().unwrap();
                            guard.paddle = state;
                            guard.generation += 1;
                            cv.notify_one();
                        }
                    }
                })
                .expect("failed to spawn keyer-input thread");
            threads.push(handle);
        }

        Self {
            shared,
            source_count,
            last_generation: 0,
            stop,
            threads,
        }
    }
}

impl PaddleInput for MergedPaddleInput {
    fn wait_for_change(&mut self, timeout: Option<Duration>) -> PaddleState {
        let (lock, cv) = &*self.shared;
        let mut guard = lock.lock().unwrap();

        // Check if something already changed since last call.
        if guard.generation != self.last_generation {
            self.last_generation = guard.generation;
            return guard.paddle;
        }

        match timeout {
            Some(dur) => {
                let deadline = Instant::now() + dur;
                while guard.generation == self.last_generation {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let (g, result) = cv.wait_timeout(guard, remaining).unwrap();
                    guard = g;
                    if result.timed_out() {
                        break;
                    }
                }
            }
            None => {
                while guard.generation == self.last_generation {
                    guard = cv.wait(guard).unwrap();
                }
            }
        }

        self.last_generation = guard.generation;
        guard.paddle
    }

    fn read(&self) -> PaddleState {
        let (lock, _) = &*self.shared;
        lock.lock().unwrap().paddle
    }

    fn describe(&self) -> String {
        format!("Merged input ({} sources)", self.source_count)
    }
}

impl Drop for MergedPaddleInput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::keyboard::KeyboardPaddleInput;
    use super::super::PaddleEvent;
    use super::*;
    use std::thread;

    #[test]
    fn merged_wakes_on_keyboard_event() {
        let (sender, kb_input) = KeyboardPaddleInput::new();
        let mut merged = MergedPaddleInput::new(vec![Box::new(kb_input)]);

        // Spawn a thread that sends DitDown after a short delay.
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            sender.send(PaddleEvent::DitDown);
        });

        let before = Instant::now();
        let state = merged.wait_for_change(Some(Duration::from_secs(5)));
        let elapsed = before.elapsed();

        assert!(state.dit, "dit should be true after DitDown");
        assert!(
            elapsed < Duration::from_secs(1),
            "should have woken quickly, but took {:?}",
            elapsed
        );
    }

    #[test]
    fn merged_timeout_no_event() {
        let (_sender, kb_input) = KeyboardPaddleInput::new();
        let mut merged = MergedPaddleInput::new(vec![Box::new(kb_input)]);

        let before = Instant::now();
        let state = merged.wait_for_change(Some(Duration::from_millis(10)));
        let elapsed = before.elapsed();

        assert!(!state.dit, "dit should be false with no events");
        assert!(!state.dash, "dash should be false with no events");
        // Should have waited at least close to the timeout.
        assert!(
            elapsed >= Duration::from_millis(5),
            "should have waited near the timeout, but only waited {:?}",
            elapsed
        );
    }
}
