#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod config;
mod engine;
mod morse;
mod sidetone;

#[cfg(feature = "std")]
pub mod input;

#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
mod handle;

pub use config::{KeyerConfig, KeyerMode, MidiBindKind, MidiBinding, SerialPin, UsbEmitStyle};
pub use engine::{KeyTransition, KeyerEngine, KeyerOutput, KeyerState};
pub use morse::{char_to_morse, text_to_elements, MorseElement, MORSE_TABLE};
pub use sidetone::Sidetone;

#[cfg(feature = "std")]
pub use input::midi::{MidiLearnState, MidiLearnTarget, RawMidiEvent};
#[cfg(feature = "std")]
pub use input::{PaddleEvent, PaddleInput, PaddleState};

#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub use handle::{KeyerCommand, KeyerHandle};

#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub use input::serial::available_ports;

#[cfg(all(
    feature = "std",
    not(target_arch = "wasm32"),
    not(target_os = "android")
))]
pub use input::midi::list_midi_input_devices;

#[cfg(all(feature = "std", target_os = "android"))]
pub use input::midi_android::list_midi_input_devices;
