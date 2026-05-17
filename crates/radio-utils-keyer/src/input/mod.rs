use std::time::{Duration, Instant};

/// Shared paddle state used by the MIDI input backends (native and Android).
/// Lives here to avoid duplicating an identical struct in midi_native and midi_android.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct MidiSharedState {
    pub dit: bool,
    pub dash: bool,
    pub timestamp: Instant,
    /// Bumped on every real state change; lets `wait_for_change` loop past
    /// spurious condvar wakeups without returning a stale state.
    pub generation: u64,
}

#[cfg(not(target_arch = "wasm32"))]
pub mod keyboard;

// Desktop / Linux / Windows / macOS — uses the `serialport` crate.
#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
pub mod serial;

// Android — uses USB Host API via JNI; same public surface (SerialPaddleInput,
// available_ports, format_serial_description) so callers don't need cfg.
#[cfg(target_os = "android")]
#[path = "serial_android.rs"]
pub mod serial;

#[cfg(not(target_arch = "wasm32"))]
pub mod merged;

pub mod midi;

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
pub mod midi_native;

#[cfg(target_os = "android")]
pub mod midi_android;

#[cfg(target_arch = "wasm32")]
pub mod midi_web;

#[cfg(target_arch = "wasm32")]
pub mod web_serial;

/// A discrete paddle event (key down / key up for dit or dash).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PaddleEvent {
    DitDown,
    DitUp,
    DashDown,
    DashUp,
}

/// Snapshot of both paddle contacts at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaddleState {
    pub dit: bool,
    pub dash: bool,
    pub timestamp: Instant,
}

impl Default for PaddleState {
    fn default() -> Self {
        Self {
            dit: false,
            dash: false,
            timestamp: Instant::now(),
        }
    }
}

/// Trait for paddle input backends (serial, keyboard, WebMIDI, etc.).
pub trait PaddleInput: Send + 'static {
    /// Block until the paddle state changes or `timeout` expires.
    /// Returns the current [`PaddleState`] at the moment of wake-up.
    fn wait_for_change(&mut self, timeout: Option<Duration>) -> PaddleState;

    /// Non-blocking snapshot of the current paddle state.
    fn read(&self) -> PaddleState;

    /// Human-readable description of this input source.
    fn describe(&self) -> String;
}
