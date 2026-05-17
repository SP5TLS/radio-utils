//! HPSDR Protocol 1 client library.
//!
//! Provides shared protocol types ([`HpsdrHw`], [`DiscoveredDevice`], [`RadioStatus`]),
//! IQ sample pack/unpack routines (24-bit RX, 16-bit TX), and UDP transport abstractions.
//!
//! With the `client` feature enabled, includes [`Protocol1Client`] for
//! discovery, streaming, and control command handling over UDP.

#[cfg(feature = "client")]
mod discovery;
pub mod iq_accumulator;
pub mod packets;
#[cfg(feature = "client")]
mod protocol1;
#[cfg(feature = "client")]
pub mod transport;
pub use iq_accumulator::IqAccumulator;
mod types;

#[cfg(feature = "client")]
pub use protocol1::{P1Command, Protocol1Client};
pub use types::*;
