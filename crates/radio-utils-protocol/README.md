# radio-utils-protocol

`radio-utils-protocol` is a library implementation of the OpenHPSDR Protocol 1 and Protocol 2. It provides the low-level transport and packet logic required to communicate with HPSDR-compatible hardware.

## Features

- **Protocol 1 Support:** Implementation of the classic 1032-byte UDP packet format.
- **Protocol 2 Support:** Support for the newer variable-length, multi-port protocol.
- **Discovery:** Tools for broadcasting and parsing discovery requests on the network.
- **IQ Packing/Unpacking:** Efficient conversion between raw network bytes and complex DSP samples.
- **no_std Support:** Core types and packet structures are compatible with embedded systems.

## Usage

```rust
use std::time::Duration;
use radio_utils_protocol::Protocol1Client;

// Broadcast a discovery probe on every local interface and wait up to 2 s
// for any HPSDR-compatible radio (or emulator) to answer.
let devices = Protocol1Client::discover(Duration::from_secs(2)).await?;
let device = devices.into_iter().next().expect("no HPSDR radio found");

// Open a session, start streaming, and read RX0 IQ samples.
let mut client = Protocol1Client::connect(&device).await?;
let mut rx_iq = client.rx_iq_stream(0);
client.start().await?;

// `client.run()` drives the UDP exchange; spawn it alongside your consumer.
tokio::spawn(async move { let _ = client.run().await; });

while let Some(samples) = rx_iq.recv().await {
    // hand `samples: Vec<Complex<f64>>` to your DSP pipeline
}
```

See [`protocol.md`](../../protocol.md) at the workspace root for the wire-
format reference this crate implements.

## Build

```bash
cargo build --release -p radio-utils-protocol
```

The `no_std`-friendly core types and packet codecs are available with
`--no-default-features` (omits `tokio`, discovery, and the async clients).

## License

Dual MIT / Apache-2.0 — see [LICENSE-MIT](../../LICENSE-MIT) and
[LICENSE-APACHE](../../LICENSE-APACHE).
