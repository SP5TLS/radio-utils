# radio-utils

A small Rust workspace of standalone Software Defined Radio and amateur-
radio building blocks. Each crate is intentionally usable in isolation
— pull in just the keyer, just the protocol client, or just the
emulator, with no broader SDR framework to drag along.

## Crates

| Crate | Purpose |
|-------|---------|
| [**radio-utils-keyer**](crates/radio-utils-keyer) | CW (Morse) keyer engine — iambic A/B, straight, bug, ultimatic, single-paddle, with Farnsworth spacing, weighting, dynamic dah ratio, sidetone generation, keyboard / serial-modem / MIDI / WebMIDI / Web-Serial / Android USB-OTG paddle inputs, and text macros. No workspace dependencies; the engine core is `no_std`. |
| [**radio-utils-protocol**](crates/radio-utils-protocol) | Library implementation of the [OpenHPSDR](http://openhpsdr.org/) Protocol 1 (legacy 1032-byte UDP) wire format — discovery, IQ pack/unpack, control commands, async `Protocol1Client`. `no_std`-friendly core types. |
| [**radio-utils-emu**](crates/radio-utils-emu) | Standalone [OpenHPSDR](http://openhpsdr.org/) Protocol 1 Hermes / Hermes Lite 2 emulator. Lets you develop and test SDR clients (Thetis, deskHPSDR, custom code) against a virtual radio without owning hardware. Supports multi-client live-mixed echo for "virtual band" deployments. Depends on `radio-utils-protocol`. |

The [`protocol.md`](protocol.md) document describes the OpenHPSDR Protocol 1
wire format in detail (byte layouts, command codes, port assignments,
timing) — it is what `radio-utils-protocol` and `radio-utils-emu`
implement, and is independent of the keyer crate.

## Quick start

The emulator binary is published on crates.io and installable directly:

```bash
cargo install radio-utils-emu --locked
radio-utils-emu --radio hermeslite
```

From a source checkout:

```bash
# Build everything
cargo build --release

# Run all tests
cargo test --workspace

# Run the Protocol 1 emulator, pretending to be a Hermes Lite 2 on the LAN
cargo run --release -p radio-utils-emu -- --radio hermeslite
```

Then point an HPSDR-compatible client at your machine; it will discover
the emulator on UDP port 1024.

## Cross-compiling for Android

The keyer ships Android paddle backends (USB-OTG modem-line serial via
`UsbManager`, plus AMidi-based MIDI) and a `build.rs` that compiles two
Java helper classes into a DEX bundled into the staticlib. Build it like:

```bash
export ANDROID_HOME=/path/to/android-sdk           # must contain platforms/android-26+
export ANDROID_NDK_HOME=/path/to/android-ndk       # any NDK r25+
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=\
  "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$HOST/bin/aarch64-linux-android26-clang"

rustup target add aarch64-linux-android
cargo build --target aarch64-linux-android --workspace
```

(`$HOST` is e.g. `darwin-x86_64`, `linux-x86_64`. See the keyer README for
running cross-compiled tests on a device via `adb push`.)

## License

Dual-licensed under either of:

- [MIT license](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option. Contributions are accepted under the same dual license.

The protocol / emulator crates target [OpenHPSDR](http://openhpsdr.org/),
a community project, and reference the Hermes / Hermes Lite 2 hardware
names only for protocol compatibility — they are not affiliated with the
OpenHPSDR project. The keyer crate has no relation to OpenHPSDR.
