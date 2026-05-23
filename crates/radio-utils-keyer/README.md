# radio-utils-keyer

CW (Morse) keyer engine in Rust — drives the timing state machine for a
software keyer plus the platform plumbing for actual paddle input.

The crate is intentionally **self-contained**: no other `radio-utils-*`
crate is required, and the engine core is `no_std` + `alloc`, so the same
code runs on desktop, in WebAssembly, on Android, and (with `--no-default-features`)
on bare-metal microcontrollers.

## Features

- **Keyer modes:** iambic A, iambic B (with configurable CMOS Super Keyer
  timing window), straight key, bug, ultimatic, single-paddle (tap-for-dit,
  hold-for-dah with auto-repeat).
- **Timing controls:** WPM (5–60), weighting (25–75 %), Farnsworth spacing,
  keying compensation, dynamic dah-to-dit ratio for high-speed CW.
- **Sidetone:** raised-cosine-shaped tone generator with configurable
  frequency, volume, delay; ramps in/out over 5 ms to eliminate key clicks.
- **Macros:** 8 programmable text-to-CW slots with mid-send abort.
- **Paddle input backends** (selected via Cargo cfg):
  - keyboard (any target except wasm32)
  - serial-port modem lines (CTS / DSR / DCD / RI) on desktop OSes
  - MIDI (`midir` on desktop, WebMIDI in browsers, AMidi via JNI on Android)
  - Android USB-OTG serial via `UsbManager` and JNI
  - Web Serial API in browsers

## Usage

```rust
use radio_utils_keyer::{KeyerConfig, KeyerHandle, KeyerMode};
use radio_utils_keyer::input::keyboard::KeyboardPaddleInput;

let cfg = KeyerConfig {
    mode: KeyerMode::IambicB,
    speed_wpm: 22,
    ..KeyerConfig::default()
};

let (paddle, paddle_writer) = KeyboardPaddleInput::new();
let (mut keyer, output_rx) = KeyerHandle::start(cfg, Box::new(paddle));

// Drive the paddle from your input layer:
paddle_writer.set_dit(true);

// Consume KeyerOutput events (KeyDown, KeyUp, sidetone samples, …):
while let Ok(event) = output_rx.recv() {
    // hand to your TX / sidetone path
}

keyer.send_macro("CQ DE N0CALL K".into());
keyer.stop();
```

The engine itself (`KeyerEngine`) can be driven without a thread if you'd
rather integrate it into your own event loop — `KeyerHandle` is just the
desktop convenience wrapper.

## Building

### Desktop / WebAssembly

```bash
cargo build --release -p radio-utils-keyer
```

`no_std` mode (for embedded targets):

```bash
cargo build -p radio-utils-keyer --no-default-features
```

### Android

The crate's `build.rs` compiles two Java helpers
(`java/MidiHelper.java`, `java/UsbSerialHelper.java`) into a `classes.dex`
that's embedded into the staticlib. This requires the Android SDK
(`platforms/android-26+`, plus `build-tools` for `d8`) and the NDK.

```bash
export ANDROID_HOME=/path/to/android-sdk
export ANDROID_NDK_HOME=/path/to/android-ndk
export HOST=darwin-x86_64   # or linux-x86_64
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=\
  "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$HOST/bin/aarch64-linux-android26-clang"

rustup target add aarch64-linux-android
cargo build --target aarch64-linux-android -p radio-utils-keyer
```

`build.rs` also adds an `rustc-link-search` entry to the NDK's API-29
sysroot so the linker finds `libamidi.so` even when targeting a lower API
level. The same pattern works for `armv7-linux-androideabi`,
`x86_64-linux-android`, and `i686-linux-android` — set the matching
`CARGO_TARGET_<TRIPLE>_LINKER` env var.

#### Running tests on a connected device

```bash
cargo test --target aarch64-linux-android -p radio-utils-keyer --no-run
adb push target/aarch64-linux-android/debug/deps/radio_utils_keyer-* /data/local/tmp/test
adb shell chmod 755 /data/local/tmp/test
adb shell /data/local/tmp/test
adb shell rm /data/local/tmp/test
```

The Android-only modules (`midi_android`, `serial_android`) currently have
no `#[cfg(test)]` blocks — running on a device validates that the
engine / morse / sidetone / merged-input code paths behave identically on
aarch64-android as on desktop, but the JNI bridges still need
app-level end-to-end testing.

## License

Dual MIT / Apache-2.0 — see [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
