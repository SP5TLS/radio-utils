# radio-utils-cw-decoder

Streaming CW (Morse code) decoder in Rust — `no_std`, no allocator,
suitable for use inside an interrupt-priority task on bare-metal
microcontrollers.

All state lives inline in [`Decoder`]: a packed `u8` accumulator for the
in-flight character and a 32-entry `heapless::Deque` of recently decoded
ASCII bytes. The recent-history ring evicts oldest-first on overflow.

## Features

- **Streaming, edge-driven:** feed every key-line transition; the
  decoder updates in O(1) per edge.
- **Sender-WPM aware:** dit / dah / inter-character / inter-word
  thresholds derive from the sender's WPM passed in on each call, so
  the same `Decoder` adapts as the sender changes speed.
- **Poll-flushed:** a periodic [`Decoder::poll`] call drains the last
  character of a transmission once the inter-character silence
  threshold elapses — without it the trailing character would only
  appear when the next key-down arrives.
- **Idempotent during sustained silence:** repeated `poll`s while the
  key is up emit at most one character and at most one inter-word
  space.
- **No panics:** invalid / overflowed in-flight characters are silently
  dropped; out-of-range gap measurements saturate.

## Usage

```rust
use radio_utils_cw_decoder::Decoder;

let mut d = Decoder::new();
let wpm = 20;

// On every key-line edge:
d.on_transition(now_us, /* key_down = */ true,  wpm);
// … key held for a dit / dah …
d.on_transition(now_us, /* key_down = */ false, wpm);

// Periodically (e.g. from a UI tick) so the trailing character of a
// transmission flushes during silence:
d.poll(now_us, wpm);

// Read out the recent-history ring:
let mut buf = [0u8; 32];
let n = d.snapshot(&mut buf);
let decoded = &buf[..n]; // ASCII, oldest-first
```

The decoder is `const fn new()`, so it can be placed in a `static` and
shared between an interrupt-priority writer and a low-priority reader
through a `blocking_mutex<CriticalSectionRawMutex, RefCell<Decoder>>`
without lazy initialisation.

## Building

```bash
cargo build --release -p radio-utils-cw-decoder
```

`no_std` is the only mode — no Cargo features.

## License

Dual MIT / Apache-2.0 — see [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
