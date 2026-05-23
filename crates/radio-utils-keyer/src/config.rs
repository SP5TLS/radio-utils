use alloc::string::String;
use serde::{Deserialize, Serialize};

/// Keyer operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyerMode {
    Straight,
    IambicA,
    IambicB,
    Bug,
    Ultimatic,
    /// Short tap = dit, hold past dit-duration = dah. Holding the paddle
    /// continuously after the first dah produces auto-repeating dahs (each
    /// followed by an inter-element gap before the next press is evaluated).
    SinglePaddle,
}

/// Which serial pin carries a paddle signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SerialPin {
    CTS,
    DSR,
    DCD,
    RI,
}

/// How the firmware encodes paddle/keyer activity onto USB transports
/// (serial DCD/DSR + MIDI notes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsbEmitStyle {
    /// cw-adapter-compatible: emit raw paddle press/release edges.  The
    /// host (e.g. vail-adapter) does its own keying.  Two MIDI notes
    /// (dit / dah) and two serial bits (DCD / DSR).
    Paddle,
    /// Send the keyed signal that the on-board engine produces — one
    /// MIDI note + one serial bit toggle per Morse element, including
    /// inter-element gaps.  Useful when you want the host to log or
    /// retransmit the already-decoded CW the radio is hearing.
    Keyed,
}

/// Which kind of MIDI message carries the paddle signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MidiBindKind {
    NoteOn,
    ControlChange,
}

/// A binding between a physical MIDI button and a paddle action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiBinding {
    pub channel: u8,
    pub kind: MidiBindKind,
    pub number: u8,
}

/// Top-level keyer configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyerConfig {
    /// Keyer operating mode.
    pub mode: KeyerMode,
    /// Sending speed in words per minute (5–60).
    pub speed_wpm: u8,
    /// Weighting factor (25–75); 50 = standard 1:3 ratio.
    pub weight: u8,
    /// Sidetone frequency in Hz.
    pub sidetone_freq: u32,
    /// Sidetone volume (0.0–1.0).
    pub sidetone_volume: f32,
    /// Sidetone delay in ms relative to key-down (-20..+20).
    pub sidetone_delay_ms: i32,
    /// Hang time in ms before TX-to-RX transition.
    pub hang_time_ms: u32,
    /// Swap dit/dah paddles.
    pub keys_reversed: bool,
    /// Enforce correct inter-element spacing (iambic modes).
    pub auto_spacing: bool,
    /// Serial port device path (e.g. "/dev/ttyUSB0").
    pub serial_port: Option<String>,
    /// Serial pin for dit paddle.
    pub dit_pin: SerialPin,
    /// Serial pin for dah paddle.
    pub dash_pin: SerialPin,
    /// Keyboard key for dit paddle.
    pub dit_key: Option<char>,
    /// Keyboard key for dah paddle.
    pub dash_key: Option<char>,
    /// MIDI input device name (None = disabled).
    pub midi_device: Option<String>,
    /// MIDI binding for dit paddle.
    pub midi_dit: Option<MidiBinding>,
    /// MIDI binding for dah paddle.
    pub midi_dah: Option<MidiBinding>,
    /// CMOS Super Keyer iambic B timing percentage (0–100).
    ///
    /// Controls when during an element the opposite-paddle memory latch is
    /// active.  The last `iambic_b_timing_percent`% of the element is a
    /// "dead zone" where presses are ignored for memory purposes.
    ///
    /// * **0 %** — memory latches throughout the entire element (classic
    ///   iambic B).
    /// * **33 %** (default) — memory only latches during the first 67 % of
    ///   the element, matching the WB9KZY CMOS Super Keyer feel.
    /// * **100 %** — memory never latches during elements (behaves like
    ///   iambic A).
    ///
    /// Only affects `IambicB` mode; ignored in other modes.
    pub iambic_b_timing_percent: u8,
    /// Farnsworth spacing WPM. 0 = disabled. When active and less than
    /// `speed_wpm`, inter-character and inter-word gaps are stretched to
    /// this slower rate while elements stay at `speed_wpm`.
    pub farnsworth_wpm: u8,
    /// Keying compensation: extend every key-down element by this many ms.
    /// Compensates for transceiver relay switching lag in QSK operation.
    pub keying_compensation_ms: u8,
    /// Enable dynamic dah-to-dit ratio (auto-shorten dahs at high WPM).
    pub dynamic_ratio: bool,
    /// WPM anchor for standard 3.0:1 ratio (lower bound).
    pub dynamic_ratio_low_wpm: u8,
    /// WPM anchor for 2.4:1 ratio (upper bound).
    pub dynamic_ratio_high_wpm: u8,
    /// Eight programmable macro slots.
    pub macros: [String; 8],
    /// How to encode keyer activity onto USB transports.  Default
    /// `Paddle` preserves the cw-adapter wire contract; `Keyed` emits
    /// the engine's keyed signal as a single MIDI note + DCD bit.
    pub usb_emit_style: UsbEmitStyle,
    /// CW decoder display toggle. When true, hosts (e.g. the firmware
    /// OLED) show decoded text from the engine's keyed output stream;
    /// when false, the display is hidden. The decoder itself is cheap
    /// enough that we always run it — only the display is gated.
    pub decoder_enabled: bool,
}

impl Default for KeyerConfig {
    fn default() -> Self {
        Self {
            mode: KeyerMode::IambicB,
            speed_wpm: 18,
            weight: 50,
            sidetone_freq: 600,
            sidetone_volume: 0.7,
            sidetone_delay_ms: 0,
            hang_time_ms: 300,
            keys_reversed: false,
            auto_spacing: false,
            serial_port: None,
            dit_pin: SerialPin::CTS,
            dash_pin: SerialPin::DSR,
            dit_key: Some('z'),
            dash_key: Some('x'),
            midi_device: None,
            midi_dit: None,
            midi_dah: None,
            iambic_b_timing_percent: 33,
            farnsworth_wpm: 0,
            keying_compensation_ms: 0,
            dynamic_ratio: false,
            dynamic_ratio_low_wpm: 30,
            dynamic_ratio_high_wpm: 70,
            macros: Default::default(),
            usb_emit_style: UsbEmitStyle::Paddle,
            decoder_enabled: true,
        }
    }
}

impl KeyerConfig {
    // ---- microsecond-precision timing (used by the engine) -------------
    //
    // The engine schedules every phase off these µs values. Doing the
    // truncation in `_ms` rounds 1200/wpm down (e.g. 28 WPM: 42 ms vs the
    // true 42.857 ms) and accumulates ~1.2 s of drift per minute. The µs
    // resolution keeps the per-element error below the 1 ms tick quantum
    // and bounded long-term — see `engine::KeyerEngine::schedule_phase`.

    /// Duration of a dit element in microseconds.
    pub fn dot_duration_us(&self) -> u32 {
        1_200_000 / self.speed_wpm.max(1) as u32
    }

    /// Duration of a dah element in microseconds, respecting weight or dynamic ratio.
    pub fn dash_duration_us(&self) -> u32 {
        let dot = self.dot_duration_us() as f64;
        if self.dynamic_ratio && self.speed_wpm > self.dynamic_ratio_low_wpm {
            let low = self.dynamic_ratio_low_wpm as f64;
            let high = self
                .dynamic_ratio_high_wpm
                .max(self.dynamic_ratio_low_wpm.saturating_add(1)) as f64;
            let t = ((self.speed_wpm as f64 - low) / (high - low)).clamp(0.0, 1.0);
            let ratio = 3.0 - 0.6 * t;
            (dot * ratio) as u32
        } else {
            let w = self.weight as f64 / 50.0;
            (dot * (1.0 + 2.0 * w)) as u32
        }
    }

    /// Dit duration with keying compensation added (µs).
    pub fn effective_dot_duration_us(&self) -> u32 {
        self.dot_duration_us() + (self.keying_compensation_ms as u32) * 1000
    }

    /// Dash duration with keying compensation added (µs).
    pub fn effective_dash_duration_us(&self) -> u32 {
        self.dash_duration_us() + (self.keying_compensation_ms as u32) * 1000
    }

    /// Inter-element gap in µs (same as dit duration).
    pub fn element_gap_us(&self) -> u32 {
        self.dot_duration_us()
    }

    /// Gap between letters (µs). Farnsworth-aware.
    pub fn letter_gap_us(&self) -> u32 {
        match self.farnsworth_gap_unit_us() {
            Some(gap_unit) => gap_unit * 3,
            None => self.dot_duration_us() * 3,
        }
    }

    /// Gap between words (µs). Farnsworth-aware.
    pub fn word_gap_us(&self) -> u32 {
        match self.farnsworth_gap_unit_us() {
            Some(gap_unit) => gap_unit * 7,
            None => self.dot_duration_us() * 7,
        }
    }

    /// Farnsworth stretched gap unit in µs. Returns `None` when Farnsworth
    /// is disabled (farnsworth_wpm == 0 or >= speed_wpm).
    fn farnsworth_gap_unit_us(&self) -> Option<u32> {
        if self.farnsworth_wpm == 0 || self.farnsworth_wpm >= self.speed_wpm {
            return None;
        }
        // Both wpm values are guaranteed nonzero by the guard above.
        let t_char = 1_200_000.0 / self.speed_wpm as f64;
        let t_total = 1_200_000.0 / self.farnsworth_wpm as f64;
        let gap_unit = (50.0 * t_total - 31.0 * t_char) / 19.0;
        Some(gap_unit.max(t_char) as u32)
    }

    // ---- millisecond getters (display / UI / legacy tests) ------------
    //
    // Implemented in terms of the µs versions so there's exactly one source
    // of truth. They floor — matching the previous `1200 / wpm` behaviour
    // so existing assertions at integer-millisecond speeds (20, 12, …) keep
    // working.

    /// Duration of a dit element in milliseconds.
    pub fn dot_duration_ms(&self) -> u32 {
        self.dot_duration_us() / 1000
    }

    /// Duration of a dah element in milliseconds.
    pub fn dash_duration_ms(&self) -> u32 {
        self.dash_duration_us() / 1000
    }

    /// Dit duration with keying compensation added (ms).
    pub fn effective_dot_duration_ms(&self) -> u32 {
        self.effective_dot_duration_us() / 1000
    }

    /// Dash duration with keying compensation added (ms).
    pub fn effective_dash_duration_ms(&self) -> u32 {
        self.effective_dash_duration_us() / 1000
    }

    /// Inter-element gap in milliseconds.
    pub fn element_gap_ms(&self) -> u32 {
        self.element_gap_us() / 1000
    }

    /// Gap between letters in milliseconds. Farnsworth-aware.
    pub fn letter_gap_ms(&self) -> u32 {
        self.letter_gap_us() / 1000
    }

    /// Gap between words in milliseconds. Farnsworth-aware.
    pub fn word_gap_ms(&self) -> u32 {
        self.word_gap_us() / 1000
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sane_values() {
        let cfg = KeyerConfig::default();
        assert_eq!(cfg.mode, KeyerMode::IambicB);
        assert_eq!(cfg.speed_wpm, 18);
        assert_eq!(cfg.weight, 50);
        assert_eq!(cfg.sidetone_freq, 600);
        assert!((cfg.sidetone_volume - 0.7).abs() < f32::EPSILON);
        assert_eq!(cfg.sidetone_delay_ms, 0);
        assert_eq!(cfg.hang_time_ms, 300);
        assert!(!cfg.keys_reversed);
        assert!(!cfg.auto_spacing);
        assert!(cfg.serial_port.is_none());
        assert_eq!(cfg.dit_pin, SerialPin::CTS);
        assert_eq!(cfg.dash_pin, SerialPin::DSR);
        assert_eq!(cfg.dit_key, Some('z'));
        assert_eq!(cfg.dash_key, Some('x'));
        assert_eq!(cfg.macros.len(), 8);
        assert!(cfg.macros.iter().all(|m| m.is_empty()));
    }

    #[test]
    fn dot_duration_at_standard_speeds() {
        let mut cfg = KeyerConfig::default();

        cfg.speed_wpm = 20;
        assert_eq!(cfg.dot_duration_ms(), 60);

        cfg.speed_wpm = 12;
        assert_eq!(cfg.dot_duration_ms(), 100);
    }

    #[test]
    fn dash_duration_respects_weight() {
        let mut cfg = KeyerConfig::default();
        cfg.speed_wpm = 20; // dot = 60ms

        // Weight 50 → standard ratio: dash = 60 * (1 + 2*1.0) = 180
        cfg.weight = 50;
        assert_eq!(cfg.dash_duration_ms(), 180);

        // Heavier weight → longer dash
        cfg.weight = 75;
        assert_eq!(cfg.dash_duration_ms(), 240); // 60 * (1 + 2*1.5) = 60*4 = 240

        // Lighter weight → shorter dash
        cfg.weight = 25;
        assert_eq!(cfg.dash_duration_ms(), 120); // 60 * (1 + 2*0.5) = 60*2 = 120
    }

    #[test]
    fn midi_config_defaults_to_none() {
        let cfg = KeyerConfig::default();
        assert!(cfg.midi_device.is_none());
        assert!(cfg.midi_dit.is_none());
        assert!(cfg.midi_dah.is_none());
    }

    #[test]
    fn midi_binding_roundtrip() {
        use serde_json;
        let binding = MidiBinding {
            channel: 1,
            kind: MidiBindKind::NoteOn,
            number: 45,
        };
        let json = serde_json::to_string(&binding).unwrap();
        let restored: MidiBinding = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.channel, 1);
        assert_eq!(restored.kind, MidiBindKind::NoteOn);
        assert_eq!(restored.number, 45);
    }

    #[test]
    fn config_serialization_roundtrip() {
        let original = KeyerConfig::default();
        let json = serde_json::to_string_pretty(&original).expect("serialize");
        let restored: KeyerConfig = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.mode, original.mode);
        assert_eq!(restored.speed_wpm, original.speed_wpm);
        assert_eq!(restored.weight, original.weight);
        assert_eq!(restored.sidetone_freq, original.sidetone_freq);
        assert!((restored.sidetone_volume - original.sidetone_volume).abs() < f32::EPSILON);
        assert_eq!(restored.dit_key, original.dit_key);
        assert_eq!(restored.dash_key, original.dash_key);
        assert_eq!(
            restored.iambic_b_timing_percent,
            original.iambic_b_timing_percent
        );
        assert_eq!(restored.macros, original.macros);
    }

    #[test]
    fn config_missing_timing_field_gets_default() {
        // Old configs without iambic_b_timing_percent should deserialize
        // with the default value (33).
        let json = r#"{"mode":"IambicB","speed_wpm":20,"weight":50,
            "sidetone_freq":600,"sidetone_volume":0.7,"sidetone_delay_ms":0,
            "hang_time_ms":300,"keys_reversed":false,"auto_spacing":false,
            "serial_port":null,"dit_pin":"CTS","dash_pin":"DSR",
            "dit_key":"z","dash_key":"x","midi_device":null,
            "midi_dit":null,"midi_dah":null,
            "macros":["","","","","","","",""]}"#;
        let restored: KeyerConfig = serde_json::from_str(json).expect("deserialize");
        assert_eq!(restored.iambic_b_timing_percent, 33);
    }

    #[test]
    fn default_new_fields() {
        let cfg = KeyerConfig::default();
        assert_eq!(cfg.farnsworth_wpm, 0);
        assert_eq!(cfg.keying_compensation_ms, 0);
        assert!(!cfg.dynamic_ratio);
        assert_eq!(cfg.dynamic_ratio_low_wpm, 30);
        assert_eq!(cfg.dynamic_ratio_high_wpm, 70);
    }

    #[test]
    fn farnsworth_disabled_when_zero() {
        let mut cfg = KeyerConfig::default();
        cfg.speed_wpm = 20;
        cfg.farnsworth_wpm = 0;
        assert_eq!(cfg.letter_gap_ms(), 60 * 3);
        assert_eq!(cfg.word_gap_ms(), 60 * 7);
    }

    #[test]
    fn farnsworth_disabled_when_gte_speed() {
        let mut cfg = KeyerConfig::default();
        cfg.speed_wpm = 20;
        cfg.farnsworth_wpm = 20;
        assert_eq!(cfg.letter_gap_ms(), 60 * 3);
        assert_eq!(cfg.word_gap_ms(), 60 * 7);
    }

    #[test]
    fn farnsworth_stretches_gaps() {
        let mut cfg = KeyerConfig::default();
        cfg.speed_wpm = 20;
        cfg.farnsworth_wpm = 10;
        assert!(cfg.letter_gap_ms() > 60 * 3);
        assert!(cfg.word_gap_ms() > 60 * 7);
        assert_eq!(cfg.element_gap_ms(), 60);
    }

    #[test]
    fn keying_compensation_extends_elements() {
        let mut cfg = KeyerConfig::default();
        cfg.speed_wpm = 20;
        cfg.keying_compensation_ms = 5;
        assert_eq!(cfg.effective_dot_duration_ms(), 65);
        assert_eq!(cfg.effective_dash_duration_ms(), 185);
    }

    #[test]
    fn dynamic_ratio_shortens_dash_at_high_wpm() {
        let mut cfg = KeyerConfig::default();
        cfg.dynamic_ratio = true;
        cfg.dynamic_ratio_low_wpm = 30;
        cfg.dynamic_ratio_high_wpm = 70;

        cfg.speed_wpm = 30;
        let dot30 = cfg.dot_duration_ms();
        assert_eq!(cfg.dash_duration_ms(), dot30 * 3);

        cfg.speed_wpm = 70;
        let dot70 = cfg.dot_duration_ms();
        let dash70 = cfg.dash_duration_ms();
        let ratio = dash70 as f64 / dot70 as f64;
        assert!(
            (ratio - 2.4).abs() < 0.2,
            "Ratio at 70 WPM should be ~2.4, got {ratio}"
        );
    }

    #[test]
    fn dynamic_ratio_disabled_uses_weight() {
        let mut cfg = KeyerConfig::default();
        cfg.speed_wpm = 50;
        cfg.weight = 50;
        cfg.dynamic_ratio = false;
        assert_eq!(cfg.dash_duration_ms(), cfg.dot_duration_ms() * 3);
    }

    #[test]
    fn new_fields_serde_backward_compat() {
        let json = r#"{"mode":"IambicB","speed_wpm":20,"weight":50,
            "sidetone_freq":600,"sidetone_volume":0.7,"sidetone_delay_ms":0,
            "hang_time_ms":300,"keys_reversed":false,"auto_spacing":false,
            "serial_port":null,"dit_pin":"CTS","dash_pin":"DSR",
            "dit_key":"z","dash_key":"x","midi_device":null,
            "midi_dit":null,"midi_dah":null,
            "iambic_b_timing_percent":33,
            "macros":["","","","","","","",""]}"#;
        let cfg: KeyerConfig = serde_json::from_str(json).expect("deserialize");
        assert_eq!(cfg.farnsworth_wpm, 0);
        assert_eq!(cfg.keying_compensation_ms, 0);
        assert!(!cfg.dynamic_ratio);
    }

    #[test]
    fn new_modes_serde_roundtrip() {
        let mut cfg = KeyerConfig::default();
        cfg.mode = KeyerMode::Ultimatic;
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: KeyerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.mode, KeyerMode::Ultimatic);

        cfg.mode = KeyerMode::SinglePaddle;
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: KeyerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.mode, KeyerMode::SinglePaddle);
    }
}
