use std::fmt;
use std::net::SocketAddr;

use num_complex::Complex;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no devices found")]
    NoDevicesFound,
    #[error("not connected")]
    NotConnected,
    #[error("invalid packet: {0}")]
    InvalidPacket(String),
    #[error("timeout")]
    Timeout,
    #[error("connection lost")]
    ConnectionLost,
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

// ---------------------------------------------------------------------------
// Protocol enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Protocol1,
    Protocol2,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol1 => write!(f, "Protocol 1"),
            Self::Protocol2 => write!(f, "Protocol 2"),
        }
    }
}

// ---------------------------------------------------------------------------
// HpsdrHw enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HpsdrHw {
    Atlas,
    Hermes,
    HermesII,
    Angelia,
    Orion,
    OrionMkII,
    HermesLite,
    Saturn,
    SaturnMkII,
    Unknown(u8),
}

impl HpsdrHw {
    pub fn p1_code(self) -> u8 {
        match self {
            Self::Atlas => 0,
            Self::Hermes => 1,
            Self::HermesII => 2,
            Self::Angelia => 4,
            Self::Orion => 5,
            Self::OrionMkII => 10,
            Self::HermesLite => 6,
            Self::Saturn => 12, // Distinct from OrionMkII (10); Saturn is primarily P2
            Self::SaturnMkII => 11,
            Self::Unknown(c) => c,
        }
    }

    pub fn p2_code(self) -> u8 {
        match self {
            Self::Atlas => 0,
            Self::Hermes => 1,
            Self::HermesII => 2,
            Self::Angelia => 3,
            Self::Orion => 4,
            Self::OrionMkII => 5,
            Self::HermesLite => 6,
            Self::Saturn => 10,
            Self::SaturnMkII => 11,
            Self::Unknown(c) => c,
        }
    }

    pub fn from_p1_code(code: u8) -> Self {
        match code {
            0 => Self::Atlas,
            1 => Self::Hermes,
            2 => Self::HermesII,
            4 => Self::Angelia,
            5 => Self::Orion,
            6 => Self::HermesLite,
            10 => Self::OrionMkII,
            11 => Self::SaturnMkII,
            12 => Self::Saturn,
            c => Self::Unknown(c),
        }
    }

    pub fn from_p2_code(code: u8) -> Self {
        match code {
            0 => Self::Atlas,
            1 => Self::Hermes,
            2 => Self::HermesII,
            3 => Self::Angelia,
            4 => Self::Orion,
            5 => Self::OrionMkII,
            6 => Self::HermesLite,
            10 => Self::Saturn,
            11 => Self::SaturnMkII,
            c => Self::Unknown(c),
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "atlas" => Some(Self::Atlas),
            "hermes" => Some(Self::Hermes),
            "hermesii" => Some(Self::HermesII),
            "angelia" => Some(Self::Angelia),
            "orion" => Some(Self::Orion),
            "orionmkii" => Some(Self::OrionMkII),
            "hermeslite" => Some(Self::HermesLite),
            "saturn" => Some(Self::Saturn),
            "saturnmkii" => Some(Self::SaturnMkII),
            _ => None,
        }
    }

    pub fn all_names() -> &'static [&'static str] {
        &[
            "atlas",
            "hermes",
            "hermesii",
            "angelia",
            "orion",
            "orionmkii",
            "hermeslite",
            "saturn",
            "saturnmkii",
        ]
    }

    pub fn max_ddcs(self) -> u8 {
        match self {
            Self::Atlas => 2,
            Self::Hermes => 4,
            Self::HermesII => 4,
            Self::Angelia => 5,
            Self::Orion => 5,
            Self::OrionMkII => 8,
            Self::HermesLite => 2,
            Self::Saturn => 10,
            Self::SaturnMkII => 10,
            Self::Unknown(_) => 1,
        }
    }

    /// Default RX meter calibration offset: dBm = dBFS + offset.
    ///
    /// Accounts for ADC full-scale reference and typical front-end gain.
    /// Based on Thetis/deskHPSDR calibration approach.
    pub fn rx_meter_cal_offset(self) -> f64 {
        match self {
            Self::Atlas => -20.0,
            Self::Hermes => -20.0,
            Self::HermesII => -20.0,
            Self::Angelia => -20.0,
            Self::Orion => -20.0,
            Self::OrionMkII => -14.0,
            Self::HermesLite => -19.0,
            Self::Saturn => -20.0,
            Self::SaturnMkII => -20.0,
            Self::Unknown(_) => -20.0,
        }
    }
}

impl fmt::Display for HpsdrHw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Atlas => write!(f, "Atlas"),
            Self::Hermes => write!(f, "Hermes"),
            Self::HermesII => write!(f, "HermesII"),
            Self::Angelia => write!(f, "Angelia"),
            Self::Orion => write!(f, "Orion"),
            Self::OrionMkII => write!(f, "OrionMkII"),
            Self::HermesLite => write!(f, "HermesLite"),
            Self::Saturn => write!(f, "Saturn"),
            Self::SaturnMkII => write!(f, "SaturnMkII"),
            Self::Unknown(c) => write!(f, "Unknown({})", c),
        }
    }
}

// ---------------------------------------------------------------------------
// Sample rate tables
// ---------------------------------------------------------------------------

pub const SAMPLE_RATES_P1: &[(u32, u8)] = &[(48_000, 0), (96_000, 1), (192_000, 2), (384_000, 3)];

pub fn sample_rate_to_p1_code(rate: u32) -> u8 {
    for &(r, c) in SAMPLE_RATES_P1 {
        if r == rate {
            return c;
        }
    }
    0
}

pub fn p1_code_to_sample_rate(code: u8) -> Option<u32> {
    for &(r, c) in SAMPLE_RATES_P1 {
        if c == code {
            return Some(r);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Frequency-to-filter helpers
// ---------------------------------------------------------------------------

/// Alex TX LPF bit for C0=0x12 C4 (standard HPSDR with Alex board).
/// Returns a single-bit value matching the Thetis `_rbpfilter` mapping.
pub fn alex_tx_lpf_for_freq(freq_hz: u32) -> u8 {
    if freq_hz <= 2_500_000 {
        0x08 // 160m
    } else if freq_hz <= 5_000_000 {
        0x04 // 80m
    } else if freq_hz <= 8_000_000 {
        0x02 // 60/40m
    } else if freq_hz <= 16_500_000 {
        0x01 // 30/20m
    } else if freq_hz <= 24_000_000 {
        0x40 // 17/15m
    } else if freq_hz <= 35_600_000 {
        0x20 // 12/10m
    } else {
        0x10 // 6m
    }
}

/// Alex RX HPF bits for C0=0x12 C3 (standard HPSDR with Alex board).
/// Returns the HPF selector bits matching the Thetis mapping.
pub fn alex_rx_hpf_for_freq(freq_hz: u32) -> u8 {
    if freq_hz < 1_500_000 {
        0x20 // bypass
    } else if freq_hz < 6_500_000 {
        0x10 // 1.5 MHz HPF
    } else if freq_hz < 9_500_000 {
        0x08 // 6.5 MHz HPF
    } else if freq_hz < 13_000_000 {
        0x04 // 9.5 MHz HPF
    } else if freq_hz < 20_000_000 {
        0x01 // 13 MHz HPF
    } else if freq_hz < 50_000_000 {
        0x02 // 20 MHz HPF
    } else {
        0x42 // 20 MHz HPF + 6m preamp
    }
}

/// N2ADR OC output value for HL2 (C0=0x00 C2, shifted left by 1 by caller).
///
/// Bit layout (before caller shift): bits [6:1] select LPF/HPF relay
/// combinations on the N2ADR filter board. Bit 0 selects the 160m bypass
/// (no HPF). Higher bits select progressively higher LPF and HPF cutoffs.
/// Mapping per the N2ADR filter-board documentation.
pub fn n2adr_oc_for_freq(freq_hz: u32) -> u8 {
    if freq_hz <= 2_000_000 {
        1 // 160m LPF (no HPF)
    } else if freq_hz <= 4_000_000 {
        66 // 80m LPF + 3 MHz HPF
    } else if freq_hz <= 8_000_000 {
        68 // 60/40m LPF + HPF
    } else if freq_hz <= 15_000_000 {
        72 // 30/20m LPF + HPF
    } else if freq_hz <= 22_000_000 {
        80 // 17/15m LPF + HPF
    } else if freq_hz <= 30_000_000 {
        96 // 12/10m LPF + HPF
    } else {
        64 // HPF only (6m)
    }
}

// ---------------------------------------------------------------------------
// Discovered device
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub addr: SocketAddr,
    pub mac: [u8; 6],
    pub hw_type: HpsdrHw,
    pub firmware_version: u8,
    pub protocol: Protocol,
    pub num_rxs: u8,
    pub status: u8,
}

impl fmt::Display for DiscoveredDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at {} (MAC={}, FW={}, RXs={}, {})",
            self.hw_type,
            self.addr,
            mac_to_string(&self.mac),
            self.firmware_version,
            self.num_rxs,
            self.protocol,
        )
    }
}

pub fn mac_to_string(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(":")
}

// ---------------------------------------------------------------------------
// Radio status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct RadioStatus {
    pub ptt: bool,
    pub adc_overflow: u8,
    pub forward_power: u16,
    pub reverse_power: u16,
    pub exciter_power: u16,
    pub supply_voltage: u16,
    pub pa_current: u16,
}

// ---------------------------------------------------------------------------
// IQ packing / unpacking
// ---------------------------------------------------------------------------

/// Unpack a 24-bit signed big-endian IQ sample from 6 bytes into Complex<f64>.
///
/// Q is negated to convert from the HPSDR wire convention (where the DDC
/// mixer `e^{-jωt}` places USB at negative baseband frequency) to the
/// standard math convention (positive baseband frequency = USB).
#[inline]
pub fn unpack_iq_24bit(buf: &[u8], offset: usize) -> Complex<f64> {
    let i_raw = ((buf[offset] as i32) << 24)
        | ((buf[offset + 1] as i32) << 16)
        | ((buf[offset + 2] as i32) << 8);
    let q_raw = ((buf[offset + 3] as i32) << 24)
        | ((buf[offset + 4] as i32) << 16)
        | ((buf[offset + 5] as i32) << 8);
    Complex::new(
        i_raw as f64 / 2_147_483_648.0,
        -(q_raw as f64 / 2_147_483_648.0),
    )
}

/// Pack a Complex<f64> IQ sample into 6 bytes (24-bit signed big-endian).
///
/// Q is written as-is (no negation). Used on the radio/emulator side where
/// samples are already in wire format.
#[inline]
pub fn pack_iq_24bit_into(buf: &mut [u8], offset: usize, sample: Complex<f64>) -> usize {
    pack_iq_24bit_into_ex(buf, offset, sample, false)
}

/// Pack a Complex<f64> IQ sample into 6 bytes (24-bit signed big-endian).
///
/// Q is negated to match HPSDR wire convention (host→radio direction).
#[inline]
pub fn pack_iq_24bit_into_negate_q(buf: &mut [u8], offset: usize, sample: Complex<f64>) -> usize {
    pack_iq_24bit_into_ex(buf, offset, sample, true)
}

/// Pack a Complex<f64> IQ sample into 6 bytes (24-bit signed big-endian)
/// with configurable Q-sign convention.
///
/// - `negate_q = true`: host-side (negates Q for the wire, matching protocol convention).
/// - `negate_q = false`: radio/emulator-side (Q is already in wire format).
#[inline]
pub fn pack_iq_24bit_into_ex(
    buf: &mut [u8],
    offset: usize,
    sample: Complex<f64>,
    negate_q: bool,
) -> usize {
    let max_val: f64 = 8_388_607.0;
    let iv = (sample.re.clamp(-1.0, 1.0) * max_val) as i32;
    let q = if negate_q { -sample.im } else { sample.im };
    let qv = (q.clamp(-1.0, 1.0) * max_val) as i32;
    let iu = iv as u32 & 0xFF_FFFF;
    let qu = qv as u32 & 0xFF_FFFF;
    buf[offset] = ((iu >> 16) & 0xFF) as u8;
    buf[offset + 1] = ((iu >> 8) & 0xFF) as u8;
    buf[offset + 2] = (iu & 0xFF) as u8;
    buf[offset + 3] = ((qu >> 16) & 0xFF) as u8;
    buf[offset + 4] = ((qu >> 8) & 0xFF) as u8;
    buf[offset + 5] = (qu & 0xFF) as u8;
    offset + 6
}

/// Unpack 16-bit signed big-endian TX IQ from Protocol 1 sub-frame.
/// Each 8-byte block: [L(2B) R(2B) I(2B) Q(2B)].
pub fn unpack_tx_iq_16bit(data: &[u8]) -> Vec<Complex<f64>> {
    let n_blocks = data.len() / 8;
    let mut samples = Vec::with_capacity(n_blocks);
    for k in 0..n_blocks {
        let off = k * 8;
        let i_val = i16::from_be_bytes([data[off + 4], data[off + 5]]);
        let q_val = i16::from_be_bytes([data[off + 6], data[off + 7]]);
        samples.push(Complex::new(i_val as f64 / 32768.0, q_val as f64 / 32768.0));
    }
    samples
}

/// Pack TX IQ samples into Protocol 1 format.
/// Each 8-byte block: [L(2B) R(2B) I(2B) Q(2B)], L/R set to 0.
pub fn pack_tx_iq_16bit(samples: &[Complex<f64>]) -> Vec<u8> {
    let mut buf = vec![0u8; samples.len() * 8];
    for (k, s) in samples.iter().enumerate() {
        let off = k * 8;
        // L/R audio = 0 (bytes 0-3)
        let i_val = (s.re.clamp(-1.0, 1.0) * 32767.0) as i16;
        let q_val = (s.im.clamp(-1.0, 1.0) * 32767.0) as i16;
        let i_bytes = i_val.to_be_bytes();
        let q_bytes = q_val.to_be_bytes();
        buf[off + 4] = i_bytes[0];
        buf[off + 5] = i_bytes[1];
        buf[off + 6] = q_bytes[0];
        buf[off + 7] = q_bytes[1];
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- IQ 24-bit pack/unpack roundtrips ----
    // These tests use pack_iq_24bit_into_negate_q paired with unpack_iq_24bit,
    // both of which apply Q-negation, so they form a correct roundtrip.

    #[test]
    fn iq_24bit_roundtrip_zero() {
        let sample = Complex::new(0.0, 0.0);
        let mut buf = [0u8; 6];
        pack_iq_24bit_into_negate_q(&mut buf, 0, sample);
        let out = unpack_iq_24bit(&buf, 0);
        assert!(out.re.abs() < 1e-6);
        assert!(out.im.abs() < 1e-6);
    }

    #[test]
    fn iq_24bit_roundtrip_positive_one() {
        let sample = Complex::new(1.0, 1.0);
        let mut buf = [0u8; 6];
        pack_iq_24bit_into_negate_q(&mut buf, 0, sample);
        let out = unpack_iq_24bit(&buf, 0);
        // Should be close to 1.0 within 24-bit quantization error
        assert!((out.re - 1.0).abs() < 2e-7 + 1.0 / 8_388_607.0);
        assert!((out.im - 1.0).abs() < 2e-7 + 1.0 / 8_388_607.0);
    }

    #[test]
    fn iq_24bit_roundtrip_negative_one() {
        let sample = Complex::new(-1.0, -1.0);
        let mut buf = [0u8; 6];
        pack_iq_24bit_into_negate_q(&mut buf, 0, sample);
        let out = unpack_iq_24bit(&buf, 0);
        assert!((out.re - (-1.0)).abs() < 2e-7 + 1.0 / 8_388_607.0);
        assert!((out.im - (-1.0)).abs() < 2e-7 + 1.0 / 8_388_607.0);
    }

    #[test]
    fn iq_24bit_roundtrip_half() {
        let sample = Complex::new(0.5, -0.5);
        let mut buf = [0u8; 6];
        pack_iq_24bit_into_negate_q(&mut buf, 0, sample);
        let out = unpack_iq_24bit(&buf, 0);
        assert!((out.re - 0.5).abs() < 2e-7);
        assert!((out.im - (-0.5)).abs() < 2e-7);
    }

    #[test]
    fn iq_24bit_known_bit_pattern() {
        // Pack a known value and verify bytes
        let sample = Complex::new(0.0, 0.0);
        let mut buf = [0u8; 6];
        pack_iq_24bit_into(&mut buf, 0, sample);
        assert_eq!(buf, [0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn iq_24bit_clamps_out_of_range() {
        // Values > 1.0 should be clamped
        let sample = Complex::new(2.0, -2.0);
        let mut buf = [0u8; 6];
        pack_iq_24bit_into_negate_q(&mut buf, 0, sample);
        let out = unpack_iq_24bit(&buf, 0);
        assert!((out.re - 1.0).abs() < 2e-7 + 1.0 / 8_388_607.0);
        assert!((out.im - (-1.0)).abs() < 2e-7 + 1.0 / 8_388_607.0);
    }

    #[test]
    fn iq_24bit_offset_packing() {
        // Test with non-zero offset
        let sample = Complex::new(0.25, 0.75);
        let mut buf = [0u8; 12];
        pack_iq_24bit_into_negate_q(&mut buf, 6, sample);
        let out = unpack_iq_24bit(&buf, 6);
        assert!((out.re - 0.25).abs() < 2e-7);
        assert!((out.im - 0.75).abs() < 2e-7);
    }

    // ---- IQ 16-bit roundtrip ----

    #[test]
    fn tx_iq_16bit_roundtrip() {
        let samples = vec![
            Complex::new(0.5, -0.5),
            Complex::new(0.0, 1.0),
            Complex::new(-1.0, 0.0),
        ];
        let packed = pack_tx_iq_16bit(&samples);
        assert_eq!(packed.len(), 24); // 3 * 8 bytes
        let unpacked = unpack_tx_iq_16bit(&packed);
        assert_eq!(unpacked.len(), 3);
        for (orig, recovered) in samples.iter().zip(unpacked.iter()) {
            // 16-bit has ~3e-5 quantization error
            assert!((orig.re - recovered.re).abs() < 5e-5);
            assert!((orig.im - recovered.im).abs() < 5e-5);
        }
    }

    #[test]
    fn tx_iq_16bit_lr_bytes_zero() {
        // L/R audio bytes should always be zero
        let samples = vec![Complex::new(0.5, 0.5)];
        let packed = pack_tx_iq_16bit(&samples);
        assert_eq!(packed[0], 0); // L high
        assert_eq!(packed[1], 0); // L low
        assert_eq!(packed[2], 0); // R high
        assert_eq!(packed[3], 0); // R low
    }

    // ---- HpsdrHw P1 code roundtrips ----

    #[test]
    fn hpsdr_hw_p1_roundtrip() {
        let variants = [
            HpsdrHw::Atlas,
            HpsdrHw::Hermes,
            HpsdrHw::HermesII,
            HpsdrHw::Angelia,
            HpsdrHw::Orion,
            HpsdrHw::OrionMkII,
            HpsdrHw::HermesLite,
            HpsdrHw::Saturn,
            HpsdrHw::SaturnMkII,
        ];
        for hw in &variants {
            let code = hw.p1_code();
            let recovered = HpsdrHw::from_p1_code(code);
            assert_eq!(*hw, recovered, "P1 roundtrip failed for {:?}", hw);
        }
    }

    #[test]
    fn hpsdr_hw_p2_roundtrip() {
        let variants = [
            HpsdrHw::Atlas,
            HpsdrHw::Hermes,
            HpsdrHw::HermesII,
            HpsdrHw::Angelia,
            HpsdrHw::Orion,
            HpsdrHw::OrionMkII,
            HpsdrHw::HermesLite,
            HpsdrHw::Saturn,
            HpsdrHw::SaturnMkII,
        ];
        for hw in &variants {
            let code = hw.p2_code();
            let recovered = HpsdrHw::from_p2_code(code);
            assert_eq!(*hw, recovered, "P2 roundtrip failed for {:?}", hw);
        }
    }

    #[test]
    fn hpsdr_hw_unknown_code() {
        let hw = HpsdrHw::from_p1_code(99);
        assert_eq!(hw, HpsdrHw::Unknown(99));
        assert_eq!(hw.p1_code(), 99);
    }

    // ---- max_ddcs bounds ----

    #[test]
    fn max_ddcs_positive() {
        let variants = [
            HpsdrHw::Atlas,
            HpsdrHw::Hermes,
            HpsdrHw::Angelia,
            HpsdrHw::Orion,
            HpsdrHw::OrionMkII,
            HpsdrHw::HermesLite,
            HpsdrHw::Saturn,
            HpsdrHw::SaturnMkII,
            HpsdrHw::Unknown(42),
        ];
        for hw in &variants {
            assert!(hw.max_ddcs() >= 1, "max_ddcs should be >= 1 for {:?}", hw);
        }
    }

    // ---- Sample rate code roundtrips ----

    #[test]
    fn sample_rate_code_roundtrip() {
        for &(rate, code) in SAMPLE_RATES_P1 {
            assert_eq!(sample_rate_to_p1_code(rate), code);
            assert_eq!(p1_code_to_sample_rate(code), Some(rate));
        }
    }

    #[test]
    fn sample_rate_unknown_returns_zero() {
        assert_eq!(sample_rate_to_p1_code(12345), 0);
    }

    #[test]
    fn p1_code_unknown_returns_none() {
        assert_eq!(p1_code_to_sample_rate(99), None);
    }

    // ---- mac_to_string format ----

    #[test]
    fn mac_to_string_format() {
        let mac = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let s = mac_to_string(&mac);
        assert_eq!(s, "de:ad:be:ef:01:02");
    }

    #[test]
    fn mac_to_string_zeros() {
        let mac = [0x00; 6];
        assert_eq!(mac_to_string(&mac), "00:00:00:00:00:00");
    }

    // ---- RadioStatus defaults ----

    #[test]
    fn radio_status_defaults() {
        let status = RadioStatus::default();
        assert!(!status.ptt);
        assert_eq!(status.adc_overflow, 0);
        assert_eq!(status.forward_power, 0);
        assert_eq!(status.reverse_power, 0);
        assert_eq!(status.exciter_power, 0);
    }
}
