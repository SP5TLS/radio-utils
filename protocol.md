# OpenHPSDR Protocol Documentation

This document describes the OpenHPSDR wire protocols (Protocol 1 and Protocol 2) — the UDP packet formats, byte layouts, command codes and timing used to talk to OpenHPSDR-compatible SDR hardware. It is a working specification compiled from the published OpenHPSDR protocol documents and interoperability testing against real radios and emulators.

## Overview

OpenHPSDR (Open High Performance Software Defined Radio) uses two protocols for communication between a host computer and SDR hardware:

- **Protocol 1 (Legacy/USB)**: Original protocol used by Metis, Hermes, Hermes Lite, and STEMlab
- **Protocol 2 (Modern/Ethernet)**: Enhanced protocol supporting more features, used by Angelia, Orion, Orion2, ANAN series, and newer devices

Both protocols use UDP for data transmission with fixed port assignments.

---

## Protocol 1 (Legacy Protocol)

### Overview

Protocol 1 is the original OpenHPSDR protocol. It uses a fixed 1032-byte packet format with a simple frame structure. Originally designed for USB but can also run over UDP.

### Connection Details

| Parameter | Value |
|-----------|-------|
| Transport | USB Bulk Transfer or UDP |
| Default Port | 1024 |
| Discovery Port | 1024 (broadcast) |
| Packet Size | 1032 bytes (fixed) |
| Byte Order | Big-endian |

### Discovery Mechanism

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
10      1     Device type (see HpsdrHw enum below)
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

**HpsdrHw Device Types** — Protocol 1 and Protocol 2 use different numbering:

| Hardware Type | P1 Code | P2 Code | Notes |
|--------------|---------|---------|-------|
| Atlas | 0 | 0 | |
| Hermes | 1 | 1 | Also: ANAN-10, ANAN-100 |
| HermesII | 2 | 2 | Griffin |
| Angelia | 4 | 3 | |
| Orion | 5 | 4 | |
| OrionMKII | 10 | 5 | |
| HermesLite | 6 | 6 | Also: HermesLite2 maps here |
| Saturn | 10 | 10 | |
| SaturnMKII | 11 | 11 | |

Note: at least one host application maps P2 codes by adding 1000 internally
before dispatch (P1 codes are used as-is). Sending an unknown device code
to such a client typically falls through to a default branch that zeros
power/SWR readings — worth being aware of when emulating an unusual board.

Note: ANAN model names (ANAN-10E, ANAN-100D, ANAN-200D, etc.) are user-facing
`HpsdrModel` names that map to the above `HpsdrHw` hardware types. They do not
have their own device type codes.

### Start/Stop Streaming

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

### Data Packet Format

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

### Sub-Frame Data Format

Each 512-byte sub-frame contains:

```
Offset  Size   Description
------  ----   -----------
0       3      Sync bytes: 0x7F 0x7F 0x7F (SYNC0, SYNC1, SYNC2)
3       5      Control bytes: C0, C1, C2, C3, C4
8       504    Sample data (interleaved I/Q + mic, see below)
```

#### Sample Data Format

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

### Control Commands

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
| 0x12 | TX drive, mic boost, filters | C1-C4: TX drive level, filter settings (device-specific) |
| 0x14 | Preamp, mic PTT/bias, RX step atten | C1: preamp/attenuation settings |
| 0x16 | Step atten ADC1/ADC2, CW keyer | C1-C4: attenuator values, CW keyer settings |
| 0x1C | ADC assignments, TX atten | C1-C4: ADC-to-DDC mapping, TX attenuation |
| 0x1E | CW enable, sidetone, RF delay | C1-C4: CW enable, sidetone level, RF delay |
| 0x20 | CW hang delay, sidetone freq | C1-C4: hang delay time, sidetone frequency |
| 0x22 | EER PWM min/max | C1-C4: envelope PWM parameters |
| 0x24 | BPF2, PureSignal enable | C1-C4: BPF2 settings, PureSignal control |

**Response Format**: The radio echoes status in the response C0 byte as `address | ptt_bit`. Response addresses cycle through 0x00, 0x08, 0x10, 0x18. Bit 7 is **not** set by current radios. Clients vary in how they decode the address: some use `(C0 >> 3) & 0x1F` (which would mis-route if bit 7 were set), others use `C0 & 0x7E` (tolerant of bit 7). Emulators should leave bit 7 clear for maximum compatibility.

### Samples per Sub-Frame (by DDC count)

| nddc | Samples/sub-frame | Bytes/block | Total data bytes |
|------|-------------------|-------------|------------------|
| 1 | 63 | 8 | 504 |
| 2 | 36 | 14 | 504 |
| 3 | 25 | 20 | 500 (4 unused) |
| 4 | 19 | 26 | 494 (10 unused) |

Each 1032-byte packet contains 2 sub-frames, so total samples per packet = 2 * spr.

### Timing (1 DDC, 48 kHz)

```
Samples per sub-frame:  63 (for 1 DDC)
Samples per packet:     126 (2 sub-frames)
Sample rate:            48 kHz
Packet period:         2.625 ms (126/48000)
Packets per second:     ~381
```

---

## Protocol 2 (Modern Protocol)

### Overview

Protocol 2 is the enhanced OpenHPSDR protocol supporting more receivers, transmitters, and features. It uses variable-length packets with a more flexible structure.

### Connection Details

| Parameter | Value |
|-----------|-------|
| Transport | UDP only |
| Network MTU | 1500 bytes |
| Packet Size | Variable (4-1444 bytes) |

### Port Assignments

#### Host → Radio (Transmit Ports)

| Port | Purpose |
|------|---------|
| 1024 | General commands |
| 1025 | Receiver-specific registers |
| 1026 | Transmitter-specific registers |
| 1027 | High priority commands |
| 1028 | TX audio samples (to radio) |
| 1029 | TX IQ samples |

#### Radio → Host (Receive Ports)

| Port | Purpose |
|------|---------|
| 1024 | Command responses |
| 1025 | High priority data (FROM radio) |
| 1026 | Microphone/line audio |
| 1027 | Wideband data |
| 1035 | DDC IQ 0 |
| 1036 | DDC IQ 1 |
| 1037 | DDC IQ 2 |
| 1038 | DDC IQ 3 |
| 1039 | DDC IQ 4 |
| 1040 | DDC IQ 5 |
| 1041 | DDC IQ 6 |
| 1042 | DDC IQ 7 |

### Discovery Mechanism

**Discovery Request Packet** (60 bytes, sent to port 1024):
```
Bytes 0-3:  0x00 0x00 0x00 0x00 (header)
Byte 4:     0x02 (command type = discovery)
Bytes 5-59: Reserved (zeros)
```

**Discovery Response Packet** (60 bytes):
```
Offset  Size  Description
------  ----  -----------
0       4     0x00 0x00 0x00 0x00 (header)
4       1     Status: 0x02 = normal, 0x03 = busy
5       6     MAC address (6 bytes)
11      1     Board type
12      1     Protocol version
13      1     Firmware version
14      1     Mercury Version 0
15      1     Mercury Version 1
16      1     Mercury Version 2
17      1     Mercury Version 3
18      1     Penny Version
19      1     Metis Version
20      1     Number of receivers (numRxs)
21      1     CIC filter shifts
22      1     Reserved
23      1     Beta version
24-59   36    Reserved
```

### RX IQ Data Packet (Ports 1035-1042)

Received on ports 1035-1042 (one port per DDC):

```
Header (16 bytes):
Offset  Size  Description
------  ----  -----------
0       4     Sequence number
4       8     Timestamp (64-bit, big-endian)
12      2     Bits per sample (big-endian 16-bit, value = 24)
14      2     Samples per frame (big-endian 16-bit, value = 238)

Data (1428 bytes = 238 samples × 6 bytes):
For each sample (6 bytes):
- I:  3 bytes (24-bit signed, big-endian)
- Q:  3 bytes (24-bit signed, big-endian)

Total packet size: 1444 bytes
```

### TX IQ Data Packet (Port 1029)

Sent by host to radio on port 1029:

```
Header (4 bytes):
Bytes 0-3: Sequence number

Data (1440 bytes = 240 samples × 6 bytes):
Same 24-bit I/Q format as RX IQ

Total packet size: 1444 bytes
```

### Audio/Microphone Data Packet (Port 1026)

Received from radio on port 1026. Both stereo audio and mono microphone share this port:

```
Header (4 bytes):
Bytes 0-3: Sequence number

Byte 4: Content type flag
         0x00 = Stereo audio (LEFT + RIGHT)
         0x01 = Microphone mono

Data:
- Stereo mode (256 bytes = 64 samples × 4 bytes):
  For each sample (4 bytes):
  - LEFT:  2 bytes (16-bit signed, big-endian)
  - RIGHT: 2 bytes (16-bit signed, big-endian)
  Total packet size: 260 bytes

- Microphone mode (128 bytes = 64 samples × 2 bytes):
  For each sample: 2 bytes (16-bit signed, big-endian)
  Total packet size: 132 bytes
```

### General Packet (Port 1024)

General configuration packet sent to port 1024. 60 bytes:

```
Offset  Size  Description
------  ----  -----------
0       4     Sequence number (usually 0)
4       1     Command = 0x00
5       2     RX Specific port (0x04 0x01 = 1025)
7       2     TX Specific port (0x04 0x02 = 1026)
9       2     High priority from PC (0x04 0x03 = 1027)
11      2     High priority to PC (0x04 0x01 = 1025)
13      2     RX Audio port (0x04 0x04 = 1028)
15      2     TX0 I&Q port (0x04 0x05 = 1029)
17      2     RX0 port (0x04 0x07 = 1035)
19      2     Mic samples port (0x04 0x02 = 1026)
21      2     Wideband ADC0 port
22      1     Wideband enable (bits 0-7 = WB0-WB7)
23      2     Wideband samples per packet
25      1     Wideband sample size (bits)
26      1     Wideband update rate (ms)
27      1     Wideband packets per frame
28      4     Reserved
33      2     Envelope PWM max
35      2     Envelope PWM min
37      1     Control bits (0x08 = phase word)
38      1     Watchdog timer
39-55   17    Reserved
56      1     Atlas bus configuration
57      1     10MHz reference source
58      1     PA, Apollo, Mercury, Clock source
59      1     Alex enable
```

### High Priority Packet

#### Host → Radio (Port 1027) - Command/Control

Sent by host to configure radio:

```
Offset  Size  Description
------  ----  -----------
0       4     Sequence number
4       1     Run flag (bit 0), PTT (bit 1), PureSignal (bit 7)
5       1     CWX control (bit 0=CWX, bit 1=dot, bit 2=dash)
9       4     RX0 frequency (Hz, big-endian)
13      4     RX1 frequency (Hz, big-endian)
17      4     RX2 frequency (Hz, big-endian)
21      4     RX3 frequency (Hz, big-endian)
25      4     RX4 frequency (Hz, big-endian)
29      4     RX5 frequency (Hz, big-endian)
33      4     RX6 frequency (Hz, big-endian)
37      4     RX7 frequency (Hz, big-endian)
41      4     RX8 frequency (Hz, big-endian)
45      4     RX9 frequency (Hz, big-endian)
49      4     RX10 frequency (Hz, big-endian)
53      4     RX11 frequency (Hz, big-endian)
329      4     TX0 frequency (Hz, big-endian)
345      1     TX0 drive level
1398     2     CAT over TCP/IP port
1400     1     XVTR enable (bit 0), Audio amp mute (bit 1), ATU tune (bit 2)
1401     1     Open collector outputs
1402     1     User outputs (DB9 pins 1-4)
1403     1     Mercury attenuator (bit 0), Preamp (bit 1)
1428     4     Alex1 filter data (TXANT/RX1)
1432     4     Alex0 filter data (TX/RX0)
1441     1     Step attenuator 2
1442     1     Step attenuator 1
1443     1     Step attenuator 0

Total packet size: 1444 bytes
```

#### Radio → Host (Port 1025) - Status/Response

Received from radio on port 1025. Note: this has a DIFFERENT layout from the
outgoing high-priority packet on port 1027.

```
Offset  Size  Description
------  ----  -----------
0       4     Sequence number (32-bit, big-endian)
4       1     PTT (bit 0), Dot (bit 1), Dash (bit 2)
5       1     ADC overload flags (bit 0=ADC0, bit 1=ADC1, ... bit 7=ADC7)
6       2     Exciter power (16-bit, big-endian)
14      2     Forward power (16-bit, big-endian)
22      2     Reverse power (16-bit, big-endian)

Total packet size: 60 bytes
```

### RX-Specific Packet (Port 1025, Host → Radio)

Receiver configuration packet sent by host to port 1025. 1444 bytes:

```
Offset        Size  Description
-----------   ----  -----------
0             4     Sequence number
7             1     Enabled receivers (bitmask, bit 0=RX0, bit 1=RX1, ...)
18 + (ddc*6)  2     DDC sample rate in kHz (big-endian 16-bit, e.g. 192 = 192000 Hz)
```

**IMPORTANT**: The sample rate value is in kHz, not Hz. A value of 192 means 192000 Hz.

### Sample Sizes per Packet

| Data Type | Samples per Packet |
|-----------|-------------------|
| RX I/Q samples | 238 |
| TX I/Q samples | 240 |
| Audio LR samples | 64 |
| Microphone samples | 64 |

---

## Configuration Summary: Protocol 1 vs Protocol 2

### Sample Rate Configuration

| Aspect | Protocol 1 | Protocol 2 |
|--------|------------|------------|
| **Method** | Commanded (C0=0x00) | Commanded (per-receiver on port 1025) |
| **Default** | 48 kHz | 192 kHz |
| **Maximum** | 384 kHz | Device-dependent (up to 1.536 MHz) |
| **Negotiated** | No | No |

### Bit Depth Configuration

| Aspect | Protocol 1 | Protocol 2 |
|--------|------------|------------|
| **RX I/Q Format** | Fixed 24-bit | Configurable (commanded per-receiver) |
| **TX I/Q Format** | Fixed 24-bit | Fixed 24-bit |
| **Audio Format** | Fixed 16-bit | Fixed 16-bit |
| **Microphone Format** | Fixed 16-bit | Fixed 16-bit |

---

## Device Support Matrix

| Device        | Protocol | DDCs | Notes |
|--------------|----------|------|-------|
| Metis        | 1        | 2    | Original Protocol 1 device |
| Hermes       | 1 & 2    | 4    | First dual-protocol device |
| Hermes Lite  | 1 & 2    | 2    | Low-cost version |
| Hermes Lite2 | 1 & 2    | 4    | Updated version |
| Angelia      | 2        | 5    | Protocol 2 only |
| Orion        | 2        | 5    | Protocol 2 only |
| Orion2       | 2        | 8    | Extended DDCs |
| ANAN-10E     | 2        | 2    | |
| ANAN-100D    | 2        | 2    | |
| ANAN-200D    | 2        | 2    | |
| ANAN-7000DLE | 2        | 2    | |
| ANAN-8000DLE | 2        | 2    | |
| Saturn       | 2        | 10   | Maximum DDCs |
| STEMlab      | 1 & 2    | 4    | Red Pitaya variant |

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

### Timing Requirements

| Data Type    | Samples | Rate    | Period  | Packets/sec |
|--------------|---------|---------|---------|-------------|
| RX IQ        | 238     | 48 kHz  | 4.96 ms | ~200        |
| TX IQ        | 240     | 192 kHz | 1.25 ms | ~800        |
| Audio        | 64      | 48 kHz  | 1.33 ms | ~750        |
| High Priority| N/A     | 10 Hz   | 100 ms  | 10          |

---

## Revision History

| Version | Date | Changes |
|---------|------|---------|
| 1.0 | 2024     | Initial draft from the OpenHPSDR protocol documents |
| 1.1 | 2025-02  | Fixed Protocol 2 port assignments (1025/1027 swapped), corrected Port 1028 direction |
| 1.2 | 2025-02-10 | Fixed Protocol 1 discovery format, added Control Command section |
| 2.0 | 2025-02-13 | Reworked packet layouts against on-the-wire captures |
| 3.0 | 2026-02-13 | Verified by interop testing: P1 24-bit I/Q format, interleaved mic+IQ layout, start/stop commands, discovery offsets, HpsdrHw device types, P2 discovery request format, P2 high-priority response layout, control command descriptions |
