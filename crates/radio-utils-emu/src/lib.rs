//! OpenHPSDR Protocol-1 hardware emulator.
//!
//! Speaks the Hermes / Hermes Lite 2 wire format end-to-end over UDP, so
//! the rest of the workspace (and third-party clients like Thetis,
//! deskHPSDR, the web client) can be developed and tested without real
//! hardware. The CLI binary lives in `src/main.rs` —
//! `cargo run --release -p radio-utils-emu -- --radio hermeslite --echo-live`
//! is the standard "virtual band" launch.
//!
//! # Echo modes
//!
//! [`radio::EchoMode`] selects how the emulator handles transmitted IQ:
//!
//! * `Live` — the bounded per-frequency `LiveBuffer` mixes concurrent TXers
//!   additively at a real-time write head (`feed_from(client_id, samples)`)
//!   and plays back to listeners with a ~21 ms delay (`LIVE_DELAY` in
//!   `radio.rs`). Two operators keying simultaneously on the same frequency
//!   superimpose like real co-channel signals (QRM); the buffer fills at
//!   1× real-time regardless of how many TXers are active. This is the
//!   mode used by hosted multi-user "virtual band" deployments.
//!
//! * `Loop` — TX is recorded into the per-frequency `FreqRecorder`, trimmed
//!   of leading/trailing silence on commit, padded with
//!   `LOOP_TAIL_SILENCE_SEC` (currently 500 ms) of silence so each iteration
//!   is audibly separated, then loops forever with a 40 ms head/tail
//!   crossfade. The recording **persists across PTT cycles** within a
//!   `LOOP_SESSION_GAP` window (currently 30 s): a slow CW operator who
//!   keys "C", pauses, then keys "Q" gets both letters with the actual
//!   inter-cycle silence preserved in the loop, instead of the second
//!   cycle overwriting the first.
//!
//! * `None` — no echo; RX delivers signal-generator noise only.
//!
//! # Multi-client model
//!
//! Each Protocol-1 client connection (identified by `SocketAddr`) gets its
//! own `client_task` running an `tokio::time::interval` at the wire packet
//! cadence (1024 Hz typical). The `MAX_CLIENTS` cap is configurable via
//! [`protocol1::DEFAULT_MAX_CLIENTS`] (32) and can be overridden per process
//! through the binary's `--max-clients` flag.
//!
//! Live-mode TX from N clients on one freq mixes additively: each call to
//! [`radio::EchoBuffer::feed`] dispatches to `LiveBuffer::feed_from` which
//! writes additively at the client's tracked offset. (The `client_id`
//! parameter on `feed` is only consulted in live mode.) Loop mode falls
//! back to a single shared append buffer per freq — fine for the typical
//! single-operator-at-a-time use case; concurrent loop-mode TX is not
//! mixed correctly.

pub mod protocol1;
pub mod radio;
