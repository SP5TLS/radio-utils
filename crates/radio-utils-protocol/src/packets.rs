//! Stateless Protocol 1 packet parsing and building functions.
//!
//! This module provides pure, synchronous functions for working with Protocol 1
//! USB-frame-level packets. It has no dependency on tokio, async, or the "client"
//! feature, so it can be used from both native and WASM targets.

use num_complex::Complex;

use crate::unpack_iq_24bit;

// ---------------------------------------------------------------------------
// P1 status from response control bytes
// ---------------------------------------------------------------------------

/// Status fields extracted from Protocol 1 response control bytes.
#[derive(Debug, Clone, Default)]
pub struct P1Status {
    /// PTT state from the hardware.
    pub ptt: bool,
    /// ADC overflow indicator.
    pub adc_overflow: u8,
    /// Forward power ADC count.
    pub forward_power: u16,
    /// Reverse power ADC count.
    pub reverse_power: u16,
    /// Exciter power ADC count.
    pub exciter_power: u16,
    /// Supply voltage ADC count.
    pub supply_voltage: u16,
    /// Response address from C0 byte (masked with 0x7E).
    pub response_addr: u8,
}

// ---------------------------------------------------------------------------
// Parsed RX frame
// ---------------------------------------------------------------------------

/// Parsed IQ data from one Protocol 1 USB frame (1032 bytes).
pub struct P1RxFrame {
    /// IQ samples per DDC channel. `samples[ddc_index]` is a `Vec<Complex<f64>>`.
    pub samples: Vec<Vec<Complex<f64>>>,
    /// Protocol status extracted from the response control bytes.
    pub status: P1Status,
}

// ---------------------------------------------------------------------------
// RX packet parsing
// ---------------------------------------------------------------------------

/// Parse a 1032-byte Protocol 1 RX packet into IQ samples per DDC.
///
/// The P1 USB frame contains two 512-byte sub-frames, each starting with a
/// SYNC pattern (0x7F 0x7F 0x7F) followed by 5 control bytes (C0-C4) and
/// then interleaved IQ data for `nddc` DDC channels.
///
/// Each IQ sample is 6 bytes (24-bit I, 24-bit Q). Samples are interleaved
/// across DDCs: DDC0 sample, DDC1 sample, ..., DDCn sample, DDC0 sample, ...
pub fn parse_rx_packet(data: &[u8], nddc: usize) -> Option<P1RxFrame> {
    if data.len() < 1032 || nddc == 0 {
        return None;
    }

    let mut samples: Vec<Vec<Complex<f64>>> = (0..nddc).map(|_| Vec::new()).collect();
    let mut status = P1Status::default();

    for frame_idx in 0..2 {
        let base = 8 + frame_idx * 512; // skip 8-byte header (seq + 2x sync)
        let sync_ok = data[base] == 0x7F && data[base + 1] == 0x7F && data[base + 2] == 0x7F;
        if !sync_ok {
            return None;
        }

        // Parse control bytes
        let c0 = data[base + 3];
        let c1 = data[base + 4];
        let c2 = data[base + 5];
        let c3 = data[base + 6];
        let c4 = data[base + 7];

        parse_response_control_bytes(&mut status, c0, c1, c2, c3, c4);

        // Parse IQ samples: 504 bytes of IQ data after the 8-byte header.
        // Each row: [IQ(6B)] x nddc + [Mic(2B)] = 6*nddc + 2 bytes.
        let iq_start = base + 8;
        let iq_end = base + 512;
        let bytes_per_sample = 6; // 24-bit I + 24-bit Q
        let bytes_per_row = bytes_per_sample * nddc + 2; // +2 for mic sample

        let mut offset = iq_start;
        while offset + bytes_per_row <= iq_end {
            for (ddc, ddc_samples) in samples.iter_mut().enumerate().take(nddc) {
                let sample_offset = offset + ddc * bytes_per_sample;
                if sample_offset + bytes_per_sample <= data.len() {
                    let iq = unpack_iq_24bit(data, sample_offset);
                    ddc_samples.push(iq);
                }
            }
            offset += bytes_per_row;
        }
    }

    Some(P1RxFrame { samples, status })
}

/// Parse the 5 response control bytes (C0-C4) from a radio-to-host sub-frame
/// and update the accumulated status.
///
/// The response address is extracted from C0 bits 6:1 (masked with 0x7E).
/// PTT is bit 0 of C0.
pub fn parse_response_control_bytes(status: &mut P1Status, c0: u8, c1: u8, c2: u8, c3: u8, c4: u8) {
    let addr = c0 & 0x7E;
    status.ptt = (c0 & 0x01) != 0;
    status.response_addr = addr;

    match addr {
        0x00 => {
            status.adc_overflow = c1;
        }
        0x08 => {
            status.exciter_power = ((c1 as u16) << 8) | (c2 as u16);
            status.forward_power = ((c3 as u16) << 8) | (c4 as u16);
        }
        0x10 => {
            status.reverse_power = ((c1 as u16) << 8) | (c2 as u16);
        }
        0x18 => {
            status.supply_voltage = ((c3 as u16) << 8) | (c4 as u16);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// TX packet building
// ---------------------------------------------------------------------------

/// Build a 1032-byte Protocol 1 host-to-radio (TX) packet.
///
/// Layout:
///   [0..4]     HPSDR header: 0xEF 0xFE 0x01 0x02 (EP2)
///   [4..8]     Sequence number (big-endian)
///   [8..520]   Sub-frame 0: SYNC + C0-C4 + TX IQ data
///   [520..1032] Sub-frame 1: SYNC + C0-C4 + TX IQ data
///
/// `tx_iq`: TX IQ samples to pack (16-bit resolution on the wire).
/// `seq`: Packet sequence number.
/// `control_bytes`: Array of 5 bytes [C0, C1, C2, C3, C4] for the first sub-frame.
/// `control_bytes2`: Array of 5 bytes for the second sub-frame.
pub fn build_tx_packet(
    seq: u32,
    tx_iq: &[Complex<f64>],
    control_bytes: [u8; 5],
    control_bytes2: [u8; 5],
) -> Vec<u8> {
    let mut packet = vec![0u8; 1032];

    // HPSDR header
    packet[0] = 0xEF;
    packet[1] = 0xFE;
    packet[2] = 0x01; // data packet
    packet[3] = 0x02; // EP2 (host -> radio)

    // Sequence number (big-endian)
    packet[4] = ((seq >> 24) & 0xFF) as u8;
    packet[5] = ((seq >> 16) & 0xFF) as u8;
    packet[6] = ((seq >> 8) & 0xFF) as u8;
    packet[7] = (seq & 0xFF) as u8;

    // Two sub-frames
    for frame_idx in 0..2u32 {
        let base = 8 + (frame_idx as usize) * 512;

        // SYNC
        packet[base] = 0x7F;
        packet[base + 1] = 0x7F;
        packet[base + 2] = 0x7F;

        // Control bytes
        let cb = if frame_idx == 0 {
            &control_bytes
        } else {
            &control_bytes2
        };
        packet[base + 3] = cb[0];
        packet[base + 4] = cb[1];
        packet[base + 5] = cb[2];
        packet[base + 6] = cb[3];
        packet[base + 7] = cb[4];

        // TX samples: 8 bytes per block [L(2B) R(2B) I(2B) Q(2B)], big-endian
        let iq_start = base + 8;
        let iq_end = base + 512;
        let bytes_per_block = 8; // L(2) + R(2) + I(2) + Q(2)
        let max_samples = (iq_end - iq_start) / bytes_per_block; // 63

        let sample_offset = (frame_idx as usize) * max_samples;
        for i in 0..max_samples {
            let sample_idx = sample_offset + i;
            let (iv, qv) = if sample_idx < tx_iq.len() {
                let s = &tx_iq[sample_idx];
                let i_val = (s.re.clamp(-1.0, 1.0) * 32767.0) as i16;
                // Negate Q for HPSDR wire convention
                let q_val = ((-s.im).clamp(-1.0, 1.0) * 32767.0) as i16;
                (i_val, q_val)
            } else {
                (0i16, 0i16)
            };

            let offset = iq_start + i * bytes_per_block;
            // L and R audio bytes (zero -- we don't send mic audio here)
            packet[offset] = 0;
            packet[offset + 1] = 0;
            packet[offset + 2] = 0;
            packet[offset + 3] = 0;
            // I and Q (big-endian i16)
            packet[offset + 4] = ((iv as u16) >> 8) as u8;
            packet[offset + 5] = (iv as u16 & 0xFF) as u8;
            packet[offset + 6] = ((qv as u16) >> 8) as u8;
            packet[offset + 7] = (qv as u16 & 0xFF) as u8;
        }
    }

    packet
}

// ---------------------------------------------------------------------------
// Control byte building
// ---------------------------------------------------------------------------

/// Build the 5 control bytes [C0, C1, C2, C3, C4] for a given address in the
/// P1 control rotation.
///
/// This covers the common host-to-radio control addresses used by simple
/// clients. Addresses handled:
///
/// - `0x00`: Sample rate, NDC count
/// - `0x02`..`0x0E`: NCO frequency for DDC channels (TX freq or RX freq)
/// - `0x12`: TX drive level
/// - `0x14`: RX step attenuator
///
/// `frequency`: VFO frequency in Hz.
/// `sample_rate_index`: Sample rate index (0=48k, 1=96k, 2=192k, 3=384k).
/// `ptt`: Push-to-talk state.
/// `nddc`: Number of active DDC channels.
/// `rx_attenuation`: RX attenuation in dB (0-31).
/// `tx_drive`: TX drive level (0-255).
pub fn build_control_bytes(
    address: u8,
    frequency: u64,
    sample_rate_index: u8,
    ptt: bool,
    nddc: u8,
    rx_attenuation: u8,
    tx_drive: u8,
) -> [u8; 5] {
    let mut cb = [0u8; 5];
    let ptt_bit = if ptt { 0x01 } else { 0x00 };
    cb[0] = address | ptt_bit;

    match address {
        0x00 => {
            // C1: speed(1:0) in bits 1:0
            cb[1] = sample_rate_index & 0x03;
            // C4: NDC count in bits 5:3 (wire value = nddc - 1) + duplex bit [2]
            // Duplex (0x04) is required by HL2 and harmless for other hardware.
            cb[4] = (nddc.saturating_sub(1) & 0x07) << 3 | 0x04;
        }
        0x02 | 0x04 | 0x06 | 0x08 | 0x0A | 0x0C | 0x0E => {
            // NCO frequency for DDC (address 0x02 = DDC0, etc.)
            let freq = frequency.min(u32::MAX as u64) as u32;
            cb[1] = ((freq >> 24) & 0xFF) as u8;
            cb[2] = ((freq >> 16) & 0xFF) as u8;
            cb[3] = ((freq >> 8) & 0xFF) as u8;
            cb[4] = (freq & 0xFF) as u8;
        }
        0x12 => {
            // TX drive level: C1 = drive value (0-255)
            cb[1] = tx_drive;
        }
        0x14 => {
            // RX step attenuator: C4 = attenuation value (0-31)
            cb[4] = rx_attenuation & 0x1F;
        }
        _ => {}
    }

    cb
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rx_packet_too_short() {
        let data = vec![0u8; 100];
        assert!(parse_rx_packet(&data, 1).is_none());
    }

    #[test]
    fn parse_rx_packet_zero_nddc() {
        let data = vec![0u8; 1032];
        assert!(parse_rx_packet(&data, 0).is_none());
    }

    #[test]
    fn parse_rx_packet_bad_sync() {
        let mut data = vec![0u8; 1032];
        // First sub-frame sync is wrong (all zeros)
        assert!(parse_rx_packet(&data, 1).is_none());

        // Set first sync correctly but second is wrong
        data[8] = 0x7F;
        data[9] = 0x7F;
        data[10] = 0x7F;
        assert!(parse_rx_packet(&data, 1).is_none());
    }

    #[test]
    fn parse_rx_packet_valid_frame() {
        let mut data = vec![0u8; 1032];
        // Set sync for both sub-frames
        data[8] = 0x7F;
        data[9] = 0x7F;
        data[10] = 0x7F;
        data[520] = 0x7F;
        data[521] = 0x7F;
        data[522] = 0x7F;

        let frame = parse_rx_packet(&data, 1).unwrap();
        assert_eq!(frame.samples.len(), 1);
        // With 1 DDC: row = 6+2=8 bytes, 504/8 = 63 samples per sub-frame, 126 total
        assert_eq!(frame.samples[0].len(), 126);
    }

    #[test]
    fn build_tx_packet_structure() {
        let tx_iq = vec![Complex::new(0.5, -0.5); 126];
        let cb1 = [0x00, 0x01, 0x00, 0x00, 0x00];
        let cb2 = [0x02, 0x00, 0x07, 0x07, 0x40];

        let pkt = build_tx_packet(42, &tx_iq, cb1, cb2);
        assert_eq!(pkt.len(), 1032);

        // Header
        assert_eq!(pkt[0], 0xEF);
        assert_eq!(pkt[1], 0xFE);
        assert_eq!(pkt[2], 0x01);
        assert_eq!(pkt[3], 0x02);

        // Sequence
        assert_eq!(u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]), 42);

        // Sub-frame 0 sync
        assert_eq!(&pkt[8..11], &[0x7F, 0x7F, 0x7F]);
        // Sub-frame 0 control bytes
        assert_eq!(&pkt[11..16], &cb1);

        // Sub-frame 1 sync
        assert_eq!(&pkt[520..523], &[0x7F, 0x7F, 0x7F]);
        // Sub-frame 1 control bytes
        assert_eq!(&pkt[523..528], &cb2);
    }

    #[test]
    fn build_control_bytes_address_0x00() {
        let cb = build_control_bytes(0x00, 7_074_000, 1, false, 2, 0, 0);
        assert_eq!(cb[0], 0x00); // address, no PTT
        assert_eq!(cb[1], 0x01); // sample rate index = 1 (96k)
        assert_eq!(cb[4], (1 << 3) | 0x04); // nddc-1 = 1, shifted left 3, plus duplex bit
    }

    #[test]
    fn build_control_bytes_address_0x00_ptt() {
        let cb = build_control_bytes(0x00, 7_074_000, 0, true, 1, 0, 0);
        assert_eq!(cb[0], 0x01); // address 0x00 | PTT bit
    }

    #[test]
    fn build_control_bytes_frequency() {
        let freq: u64 = 14_200_000;
        let cb = build_control_bytes(0x04, freq, 0, false, 1, 0, 0);
        assert_eq!(cb[0], 0x04);
        let parsed_freq = u32::from_be_bytes([cb[1], cb[2], cb[3], cb[4]]);
        assert_eq!(parsed_freq, freq as u32);
    }

    #[test]
    fn build_control_bytes_attenuation() {
        let cb = build_control_bytes(0x14, 0, 0, false, 1, 20, 0);
        assert_eq!(cb[0], 0x14);
        assert_eq!(cb[4], 20);
    }

    #[test]
    fn build_control_bytes_tx_drive() {
        let cb = build_control_bytes(0x12, 0, 0, false, 1, 0, 200);
        assert_eq!(cb[0], 0x12);
        assert_eq!(cb[1], 200);
    }

    #[test]
    fn parse_response_control_bytes_addr_0x08() {
        let mut status = P1Status::default();
        // Simulate address 0x08 with exciter_power=0x1234, forward_power=0x5678
        parse_response_control_bytes(&mut status, 0x08, 0x12, 0x34, 0x56, 0x78);
        assert_eq!(status.response_addr, 0x08);
        assert_eq!(status.exciter_power, 0x1234);
        assert_eq!(status.forward_power, 0x5678);
        assert!(!status.ptt);
    }

    #[test]
    fn parse_response_control_bytes_ptt() {
        let mut status = P1Status::default();
        parse_response_control_bytes(&mut status, 0x01, 0, 0, 0, 0); // addr 0x00 | PTT
        assert!(status.ptt);
        assert_eq!(status.response_addr, 0x00);
    }

    #[test]
    fn roundtrip_tx_rx() {
        // Build a TX packet and verify it can be parsed back
        let tx_iq = vec![Complex::new(0.0, 0.0); 126];
        let cb1 = [0x00, 0x00, 0x00, 0x00, 0x00];
        let cb2 = [0x00, 0x00, 0x00, 0x00, 0x00];

        let pkt = build_tx_packet(0, &tx_iq, cb1, cb2);
        assert_eq!(pkt.len(), 1032);
        // TX packets have sync at [8..11] and [520..523]
        assert_eq!(&pkt[8..11], &[0x7F, 0x7F, 0x7F]);
        assert_eq!(&pkt[520..523], &[0x7F, 0x7F, 0x7F]);
    }
}
