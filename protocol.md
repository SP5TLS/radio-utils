# OpenHPSDR Protocol 1 Documentation

This document describes the OpenHPSDR Protocol 1 wire format — the UDP
packet layout, byte structure, command codes and timing used to talk to a
Hermes-class HPSDR radio (or compatible emulator). It is a working
specification compiled from the published OpenHPSDR protocol documents and
interoperability testing against real radios and emulators.

## Overview

OpenHPSDR Protocol 1 is the original UDP wire format used by Hermes-class
hardware. It uses a fixed 1032-byte packet format with a simple frame
structure. Originally designed for USB, it can also run over UDP — which
is what this crate implements.

This codebase targets two boards: the original **Hermes** and the
**Hermes Lite 2**. The wire format is identical between them; what
differs is the filter, attenuator, and PA-enable wiring carried in the
control bytes. The crate dispatches the variant-specific bits based on
`HpsdrHw`. Other HPSDR boards (Atlas, Angelia, Orion, Saturn, …) are not
supported here, even though they speak the same UDP framing.

---

## Connection Details

| Parameter | Value |
|-----------|-------|
| Transport | UDP |
| Default Port | 1024 |
| Discovery Port | 1024 (broadcast) |
| Packet Size | 1032 bytes (fixed) |
| Byte Order | Big-endian |

## Discovery Mechanism

Protocol 1 uses a broadcast-based discovery mechanism.

**Discovery Request Packet** (63 bytes, sent to port 1024):
```
Byte 0:      0xEF (magic byte)
Byte 1:      0xFE (magic byte)
Byte 2:      0x02 (command type = discovery)
Bytes 3-62:  Reserved (zeros)
```

**Discovery Response Packet** (60 bytes received from radio):
```
Offset  Size  Description
------  ----  -----------
0       1     0xEF (magic byte)
1       1     0xFE (magic byte)
2       1     Status: 0x02 = normal, 0x03 = busy
3       6     MAC address (6 bytes, network byte order)
9       1     Firmware code version
10      1     Device type (Hermes = 1, Hermes Lite 2 = 6)
11      1     Protocol version (0 for Protocol 1)
12      2     Reserved
14      1     Mercury Version 0
15      1     Mercury Version 1
16      1     Mercury Version 2
17      1     Mercury Version 3
18      1     Penny Version
19      1     Metis Version
20      1     Number of receivers (numRxs)
21-59   39    Reserved
```

The crate only accepts `protocol_version == 0` (Protocol 1) and the two
supported device-type codes — `1` (Hermes) and `6` (Hermes Lite 2). Other
codes are reported as unsupported.

## Start/Stop Streaming

**IMPORTANT**: The radio does NOT start streaming immediately after discovery. The client must send explicit start/stop command packets.

**Start Command Packet** (sent to radio's IP, port 1024):
```
Byte 0:  0xEF (magic byte)
Byte 1:  0xFE (magic byte)
Byte 2:  0x04 (command type = start/stop)
Byte 3:  0x01 (start streaming)
Bytes 4+: zeros (pad to full packet)
```

**Stop Command Packet**:
```
Byte 0:  0xEF (magic byte)
Byte 1:  0xFE (magic byte)
Byte 2:  0x04 (command type = start/stop)
Byte 3:  0x00 (stop streaming)
Bytes 4+: zeros (pad to full packet)
```

**Correct Initialization Sequence**:
1. Client sends Discovery Request (broadcast to port 1024)
2. Radio responds with Discovery Response
3. Client sends configuration via control commands (C0-C4) if needed
4. **Client sends Start Command** (0xEF 0xFE 0x04 0x01)
5. Radio starts streaming I/Q data packets
6. To stop: client sends Stop Command (0xEF 0xFE 0x04 0x00)

## Data Packet Format

All data packets follow this fixed 1032-byte structure:

```
Offset  Size   Description
------  ----   -----------
0       2      Magic bytes: 0xEF 0xFE
2       1      Packet type: 0x01 (data)
3       1      Endpoint: endpoint number
4       4      Sequence number (big-endian 32-bit)
8       512    First sub-frame (sub-frame A)
520     512    Second sub-frame (sub-frame B)
```

## Sub-Frame Data Format

Each 512-byte sub-frame contains:

```
Offset  Size   Description
------  ----   -----------
0       3      Sync bytes: 0x7F 0x7F 0x7F (SYNC0, SYNC1, SYNC2)
3       5      Control bytes: C0, C1, C2, C3, C4
8       504    Sample data (interleaved I/Q + mic, see below)
```

### Sample Data Format

The 504-byte sample section contains interleaved 24-bit I/Q samples and 16-bit
microphone samples. The number of I/Q samples depends on the number of active
DDCs (Digital Down Converters):

```
Samples per DDC per sub-frame:  spr = 504 / (6 * nddc + 2)

  nddc=1: spr = 504 / 8  = 63 samples
  nddc=2: spr = 504 / 14 = 36 samples
  nddc=3: spr = 504 / 20 = 25 samples (with 4 bytes unused)
  nddc=4: spr = 504 / 26 = 19 samples (with 10 bytes unused)
```

Each sample block within the 504-byte section repeats `spr` times:
```
For each sample block (6 * nddc + 2 bytes):
  For each DDC (6 bytes per DDC):
    - I:   3 bytes (24-bit signed, big-endian)
    - Q:   3 bytes (24-bit signed, big-endian)
  Microphone:
    - Mic: 2 bytes (16-bit signed, big-endian)
```

Example with 1 DDC (nddc=1), 63 sample blocks of 8 bytes each:
```
[I0_hi][I0_mid][I0_lo][Q0_hi][Q0_mid][Q0_lo][Mic_hi][Mic_lo]  (repeat 63x)
```

Example with 2 DDCs (nddc=2), 36 sample blocks of 14 bytes each:
```
[I0(3B)][Q0(3B)][I1(3B)][Q1(3B)][Mic(2B)]  (repeat 36x)
```

**Sample Conversion** (24-bit to double):
```c
// Reconstruct 24-bit signed integer from 3 bytes (big-endian, sign-extended)
int32_t sample = (buf[0] << 24) | (buf[1] << 16) | (buf[2] << 8);  // shift to upper bits
double float_sample = sample / 2147483648.0;  // divide by 2^31
```

**Microphone sample conversion** (16-bit to double):
```c
int16_t mic = (buf[0] << 8) | buf[1];  // big-endian 16-bit
double mic_sample = mic / 32768.0;     // divide by 2^15
```

**Microphone decimation**: At higher sample rates, mic samples are decimated:
- 48 kHz: decimation factor = 1 (every sample)
- 96 kHz: decimation factor = 2 (every other sample)
- 192 kHz: decimation factor = 4
- 384 kHz: decimation factor = 8

## Control Commands

Control bytes C0-C4 are used to configure the radio. C0 identifies the command type, and C1-C4 contain parameters:

| C0 Value | Command | C1-C4 Format |
|----------|---------|--------------|
| 0x00 | General/sample rate | C1: 0x00=48k, 0x01=96k, 0x02=192k, 0x03=384k |
| 0x02 | TX VFO frequency | C1-C4: 32-bit frequency (Hz, big-endian) |
| 0x04 | RX1 (DDC0) frequency | C1-C4: 32-bit frequency (Hz, big-endian) |
| 0x06 | RX2 (DDC1) frequency | C1-C4: 32-bit frequency (Hz, big-endian) |
| 0x08 | RX3 (DDC2) frequency | C1-C4: 32-bit frequency (Hz, big-endian) |
| 0x0A | RX4 (DDC3) frequency | C1-C4: 32-bit frequency (Hz, big-endian) |
| 0x0C | RX5 (DDC4) frequency | C1-C4: 32-bit frequency (Hz, big-endian) |
| 0x0E | RX6 frequency | C1-C4: 32-bit frequency (Hz, big-endian) |
| 0x10 | RX7 frequency | C1-C4: 32-bit frequency (Hz, big-endian) |
| 0x12 | TX drive, mic boost, filters | C1: TX drive level; C3: Alex RX HPF bits; C4: Alex TX LPF bits |
| 0x14 | Preamp, mic PTT/bias, RX step atten | C4 bit 5: 20 dB attenuator |
| 0x16 | Step atten ADC1/ADC2, CW keyer | C1-C4: attenuator values, CW keyer settings |
| 0x1C | ADC assignments, TX atten | C1-C4: ADC-to-DDC mapping, TX attenuation |
| 0x1E | CW enable, sidetone, RF delay | C1-C4: CW enable, sidetone level, RF delay |
| 0x20 | CW hang delay, sidetone freq | C1-C4: hang delay time, sidetone frequency |
| 0x22 | EER PWM min/max | C1-C4: envelope PWM parameters |
| 0x24 | BPF2, PureSignal enable | C1-C4: BPF2 settings, PureSignal control |

**Response Format**: The radio echoes status in the response C0 byte as `address | ptt_bit`. Response addresses cycle through 0x00, 0x08, 0x10, 0x18. Bit 7 is **not** set by current radios. Clients vary in how they decode the address: some use `(C0 >> 3) & 0x1F` (which would mis-route if bit 7 were set), others use `C0 & 0x7E` (tolerant of bit 7). Emulators should leave bit 7 clear for maximum compatibility.

## Samples per Sub-Frame (by DDC count)

| nddc | Samples/sub-frame | Bytes/block | Total data bytes |
|------|-------------------|-------------|------------------|
| 1 | 63 | 8 | 504 |
| 2 | 36 | 14 | 504 |
| 3 | 25 | 20 | 500 (4 unused) |
| 4 | 19 | 26 | 494 (10 unused) |

Each 1032-byte packet contains 2 sub-frames, so total samples per packet = 2 * spr.

## Timing (1 DDC, 48 kHz)

```
Samples per sub-frame:  63 (for 1 DDC)
Samples per packet:     126 (2 sub-frames)
Sample rate:            48 kHz
Packet period:         2.625 ms (126/48000)
Packets per second:     ~381
```

---

## Implementation Guidelines

### Byte Order

All multi-byte values are transmitted in **big-endian** (network byte order). This applies to:
- Sequence numbers
- Sample data (24-bit, 16-bit)
- Frequency values
- All numeric fields

### Sequence Number Handling

- Every packet includes a 32-bit sequence number
- Sequence numbers increment for each packet sent
- Out-of-order packets should be logged but still processed
- Packet loss can be detected by gaps in sequence numbers

### Sample Rate Configuration

| Aspect | Protocol 1 |
|--------|------------|
| **Method** | Commanded (C0=0x00) |
| **Default** | 48 kHz |
| **Maximum** | 384 kHz |
| **Negotiated** | No |

### Bit Depth Configuration

| Aspect | Protocol 1 |
|--------|------------|
| **RX I/Q Format** | Fixed 24-bit |
| **TX I/Q Format** | Fixed 24-bit (radio side) / 16-bit (host TX subframe IQ slot) |
| **Audio Format** | Fixed 16-bit |
| **Microphone Format** | Fixed 16-bit |

---

## Hardware Targets

| Device         | P1 Code | DDCs | Notes |
|----------------|---------|------|-------|
| Hermes         | 1       | 4    | Alex filter board, C4-bit-5 RX 20 dB attenuator |
| Hermes Lite 2  | 6       | 2    | N2ADR filter board (C0=0x00 C2[7:1]), PA enable on C0=0x12 C2 bit 3, step attenuator in C0=0x14 C4 |

---

## Revision History

| Version | Date | Changes |
|---------|------|---------|
| 1.0 | 2024-01-01 | Initial draft from the OpenHPSDR protocol documents |
| 1.1 | 2025-02-01 | Fixed Protocol 1 discovery format, added Control Command section |
| 2.0 | 2025-02-13 | Reworked packet layouts against on-the-wire captures |
| 3.0 | 2026-02-13 | Verified by interop testing: P1 24-bit I/Q format, interleaved mic+IQ layout, start/stop commands, discovery offsets |
| 4.0 | 2026-05-17 | Trimmed to Hermes + Hermes Lite 2 scope (Protocol 2 and other hardware variants removed) |
