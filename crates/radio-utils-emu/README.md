# radio-utils-emu

`radio-utils-emu` is a Software Defined Radio emulator that implements the OpenHPSDR Protocol 1. It allows you to simulate HPSDR hardware on your network, making it possible to test SDR applications like Thetis or deskHPSDR without physical hardware.

## Features

- **Hardware emulation** — presents itself on the LAN as any of: Atlas,
  Hermes, HermesII, Angelia, Orion, OrionMkII, Hermes Lite 2, Saturn,
  Saturn MkII.
- **Signal generation** — Gaussian noise on every DDC at a configurable
  level.
- **Echo modes:**
  - **Loop (`--echo`)** — records the TX signal during PTT, trims
    silence, and loops it back forever on RX with a head/tail crossfade.
    Recordings persist across short PTT cycles (slow CW is preserved as
    one loop, not overwritten).
  - **Live (`--echo-live`)** — feeds TX→RX in near real-time with a
    ~21 ms delay. Multiple concurrent transmitters on the same frequency
    additively mix, so the emulator can host a small "virtual band".
- **Multi-client** — up to 32 simultaneous Protocol-1 sessions by default.
- **Telemetry** — simulates forward/reverse power and supply voltage so
  client SWR meters animate.

## Usage

Run the emulator pretending to be a Hermes Lite 2 on the LAN:
```bash
cargo run --release -p radio-utils-emu -- --radio hermeslite
```

Run it as a single-operator "virtual band" with live TX→RX feedback:
```bash
cargo run --release -p radio-utils-emu -- --radio hermeslite --echo-live
```

Any HPSDR-compatible client (Thetis, deskHPSDR, piHPSDR, your own code)
will discover the emulator via UDP broadcast on port 1024.

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `--radio <TYPE>` | Hardware to emulate. One of: `atlas`, `hermes`, `hermesii`, `angelia`, `orion`, `orionmkii`, `hermeslite`, `saturn`, `saturnmkii`. | (required) |
| `--mac <ADDR>` | MAC address as 6 hex bytes (separators optional, e.g. `00:1c:c0:a2:22:5e` or `001cc0a2225e`). | `02:AA:BB:CC:DD:EE` |
| `--noise <LEVEL>` | Noise floor amplitude as a fraction of full scale. | `1.26e-5` |
| `--echo` | Loop-mode echo: record TX during PTT, replay it on RX in a loop. | off |
| `--echo-live` | Live echo: TX→RX in real time, additively mixed across clients (mutually exclusive with `--echo` — `--echo-live` wins if both set). | off |
| `--bind <IP>` | Local IP to bind the UDP socket to. Useful when clients discover the emulator on the wrong interface (VPN, multi-homed host). | `0.0.0.0` |
| `--max-clients <N>` | Maximum concurrent Protocol-1 client sessions. | `32` |
| `-v`, `--verbose` | Enable `log::debug!` output. | off |
| `-h`, `--help` | Print the auto-generated help and exit. | — |

## Build

```bash
cargo build --release -p radio-utils-emu
```

See [`protocol.md`](../../protocol.md) at the workspace root for the wire-
format reference this emulator implements.

## License

Dual MIT / Apache-2.0 — see [LICENSE-MIT](../../LICENSE-MIT) and
[LICENSE-APACHE](../../LICENSE-APACHE).
