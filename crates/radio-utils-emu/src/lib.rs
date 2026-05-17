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
//! single-operator-at-a-time use case, but concurrent loop-mode TX is not
//! mixed correctly (see follow-ups below).
//!
//! # Possible follow-ups
//!
//! Things this crate could grow next, roughly ordered by usefulness for the
//! hosted-multi-user roadmap. None of these are blocking the current "echo
//! server on a VPS" use case.
//!
//! ## Capacity & ergonomics
//!
//! * **Configurable echo-buffer length.** `max_duration` in
//!   [`radio::EchoBuffer`] is hard-coded to 10 s. A slow CW operator
//!   working long messages with 5 s+ pauses can outlive the buffer; a
//!   `--echo-max-duration <secs>` CLI flag (and corresponding setter on
//!   `EchoBuffer`) would let operators trade memory for message length.
//!
//! * **Per-client loop recordings, keyed by `SocketAddr` like live mode.**
//!   Today loop mode shares one buffer per frequency — if two operators TX
//!   on the same freq in loop mode, their samples interleave at packet
//!   granularity (the same shape of bug that the live-mode additive-mix
//!   fix already addresses). The rewrite would mirror the
//!   `LiveBuffer.client_offsets` approach and isn't difficult; we just
//!   haven't needed it because real users use live mode for the
//!   virtual-band scenario.
//!
//! * **Tunable `LOOP_TAIL_SILENCE_SEC` and `LOOP_SESSION_GAP`.** Both are
//!   `const` today. Operators of CW practice servers may want longer
//!   inter-iteration silence (1–2 s for slower copying speed) or a
//!   tighter session-gap heuristic.
//!
//! ## Realism
//!
//! * **Soft-clip on the wire pack.** When two strong signals additively
//!   mix in live mode, the listener's wire output is re-packed to 24-bit
//!   IQ; peaks above ±1.0 hard-clip. A simple `tanh` or 1-pole limiter at
//!   the wire-pack step would round those off without changing the
//!   single-station audio.
//!
//! * **Per-band noise floor that responds to band activity.** The
//!   [`radio::SignalGenerator`] currently emits white Gaussian noise at a
//!   constant level. A 1/f tilt and a slight level bump while at least one
//!   transmitter is active would make the "virtual band" feel less sterile.
//!
//! * **Frequency-dependent fading.** Real bands have multipath flutter;
//!   simulating ~0.5–2 dB amplitude wobble at ~1–3 Hz on the played-back
//!   echo would be a small, stylish addition to a CW-practice server.
//!
//! ## Performance & instrumentation
//!
//! * **Telemetry endpoint** (e.g. an HTTP `/stats` next to the UDP port)
//!   reporting active clients, per-freq active recorders, live-buffer
//!   write rate, packets/sec/client, dropped packets. Today operators
//!   have to read `log::info!` output to size capacity. A `--stats-port`
//!   would make `loadtest_p1` and `loadtest_webrtc` measurements
//!   reproducible without screen-scraping.
//!
//! * **Shared discovery response.** Already done — built once at startup.
//!   Mentioned for completeness so this list reads as "remaining work,
//!   not done work."
//!
//! ## Out of scope (not planned)
//!
//! * Real RF effects (band-edge attenuation, AGC capture, FM capture) —
//!   the listener client owns DSP; the emulator just plays back the
//!   summed waveform.

pub mod protocol1;
pub mod radio;
