use crate::config::{MidiBindKind, MidiBinding};
use web_time::Instant;

/// A raw MIDI event, normalised for paddle use.
///
/// Both `0x80` (explicit Note Off) and `0x90` with velocity 0 (implicit Note Off)
/// produce `is_down = false` with `kind = NoteOn`, so callers never see the distinction.
#[derive(Debug, Clone, Copy)]
pub struct RawMidiEvent {
    pub kind: MidiBindKind,
    pub channel: u8,   // 1-based (1–16)
    pub number: u8,    // note number or CC number
    pub is_down: bool, // true = key/button pressed or CC > 0
}

impl RawMidiEvent {
    /// Construct from a raw MIDI message.
    ///
    /// Returns `None` for:
    /// - messages shorter than 3 bytes
    /// - unsupported message types (SysEx, realtime, channel pressure, etc.)
    ///
    /// Only NoteOn (0x9x), NoteOff (0x8x), and ControlChange (0xBx) are supported.
    /// All three are 3-byte messages; shorter slices are always rejected.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() {
            return None;
        }
        let status = bytes[0];
        let kind_nibble = status >> 4;
        let channel = (status & 0x0F) + 1; // make 1-based

        // Handle 1-byte status-only messages (e.g. USB MIDI CIN 0x0F single-byte).
        // Some CW interfaces send bare NoteOn/NoteOff status with no note/velocity bytes.
        if bytes.len() == 1 {
            return match kind_nibble {
                0x8 => Some(RawMidiEvent {
                    kind: MidiBindKind::NoteOn,
                    channel,
                    number: 0,
                    is_down: false,
                }),
                0x9 => Some(RawMidiEvent {
                    kind: MidiBindKind::NoteOn,
                    channel,
                    number: 0,
                    is_down: true,
                }),
                _ => None,
            };
        }

        if bytes.len() < 3 {
            return None;
        }
        let number = bytes[1];
        let value = bytes[2];

        match kind_nibble {
            0x8 => Some(RawMidiEvent {
                kind: MidiBindKind::NoteOn,
                channel,
                number,
                is_down: false,
            }),
            0x9 => Some(RawMidiEvent {
                kind: MidiBindKind::NoteOn,
                channel,
                number,
                is_down: value > 0,
            }),
            0xB => Some(RawMidiEvent {
                kind: MidiBindKind::ControlChange,
                channel,
                number,
                is_down: value > 0,
            }),
            _ => None,
        }
    }

    /// True if this event matches the given binding (ignores `is_down`).
    pub fn matches(&self, binding: &MidiBinding) -> bool {
        self.kind == binding.kind
            && self.channel == binding.channel
            && self.number == binding.number
    }
}

/// Which paddle (dit or dah) is being learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiLearnTarget {
    Dit,
    Dah,
}

/// Accumulates raw MIDI events during the 500 ms learn window and resolves to a binding.
///
/// # Usage
/// 1. Call [`begin`](Self::begin) to start a learn session for a specific paddle.
/// 2. Feed incoming [`RawMidiEvent`]s via [`feed`](Self::feed).
/// 3. Call [`try_resolve`](Self::try_resolve) each frame; it returns
///    `Some((target, binding))` once the window has elapsed and at least one
///    candidate has been collected. State is cleared on success.
/// 4. Call [`cancel`](Self::cancel) if the user cancels before resolution.
///
/// NoteOn events are preferred over ControlChange when both are seen.
#[derive(Default)]
pub struct MidiLearnState {
    target: Option<MidiLearnTarget>,
    candidates: std::collections::VecDeque<RawMidiEvent>,
    start: Option<Instant>,
}

impl MidiLearnState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new learn session for `target`, discarding any previous state.
    pub fn begin(&mut self, target: MidiLearnTarget) {
        self.target = Some(target);
        self.candidates.clear();
        self.start = None;
    }

    /// Cancel the current learn session and clear accumulated state.
    pub fn cancel(&mut self) {
        self.target = None;
        self.candidates.clear();
        self.start = None;
    }

    /// Feed a raw event into the accumulator. No-op when no session is active.
    ///
    /// Deduplicates by `(kind, number)` — the same control generating repeated
    /// on/off messages is counted only once.
    pub fn feed(&mut self, ev: RawMidiEvent) {
        if self.target.is_none() {
            return;
        }
        if self.start.is_none() {
            self.start = Some(Instant::now());
        }
        let already = self
            .candidates
            .iter()
            .any(|c| c.kind == ev.kind && c.channel == ev.channel && c.number == ev.number);
        if !already {
            self.candidates.push_back(ev);
        }
    }

    /// Returns `Some((target, binding))` after the 500 ms window has elapsed with
    /// at least one candidate collected. Prefer NoteOn over ControlChange.
    /// Clears internal state on success.
    pub fn try_resolve(&mut self) -> Option<(MidiLearnTarget, MidiBinding)> {
        let target = self.target?;
        let start = self.start?;
        if self.candidates.is_empty() || start.elapsed().as_millis() < 500 {
            return None;
        }
        let ev = self
            .candidates
            .iter()
            .find(|e| e.kind == MidiBindKind::NoteOn)
            .or_else(|| self.candidates.front())
            .copied()?;
        let binding = MidiBinding {
            channel: ev.channel,
            kind: ev.kind,
            number: ev.number,
        };
        self.cancel();
        Some((target, binding))
    }

    /// True if a learn session is currently active.
    pub fn is_learning(&self) -> bool {
        self.target.is_some()
    }

    /// The active target, if any.
    pub fn target(&self) -> Option<MidiLearnTarget> {
        self.target
    }
}

/// List connected MIDI input device names.
/// Not available on WASM (`midir` is not available there).
#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
pub fn list_midi_input_devices() -> Vec<String> {
    use midir::MidiInput;
    let Ok(midi_in) = MidiInput::new("radio-utils-list") else {
        return Vec::new();
    };
    midi_in
        .ports()
        .iter()
        .filter_map(|p| midi_in.port_name(p).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MidiBindKind, MidiBinding};

    #[test]
    fn note_off_status_byte_is_down_false() {
        let ev = RawMidiEvent::from_bytes(&[0x80, 45, 0]).unwrap();
        assert_eq!(ev.kind, MidiBindKind::NoteOn); // note-off normalises as NoteOn kind
        assert!(!ev.is_down);
        assert_eq!(ev.number, 45);
    }

    #[test]
    fn note_on_velocity_zero_is_down_false() {
        let ev = RawMidiEvent::from_bytes(&[0x90, 45, 0]).unwrap();
        assert_eq!(ev.kind, MidiBindKind::NoteOn);
        assert!(!ev.is_down);
    }

    #[test]
    fn note_on_velocity_nonzero_is_down_true() {
        let ev = RawMidiEvent::from_bytes(&[0x90, 45, 127]).unwrap();
        assert_eq!(ev.kind, MidiBindKind::NoteOn);
        assert!(ev.is_down);
        assert_eq!(ev.number, 45);
        assert_eq!(ev.channel, 1);
    }

    #[test]
    fn control_change_nonzero_is_down_true() {
        let ev = RawMidiEvent::from_bytes(&[0xB1, 64, 127]).unwrap(); // CC on channel 2
        assert_eq!(ev.kind, MidiBindKind::ControlChange);
        assert!(ev.is_down);
        assert_eq!(ev.channel, 2);
        assert_eq!(ev.number, 64);
    }

    #[test]
    fn control_change_zero_is_down_false() {
        let ev = RawMidiEvent::from_bytes(&[0xB0, 64, 0]).unwrap();
        assert!(!ev.is_down);
    }

    #[test]
    fn from_bytes_empty_returns_none() {
        assert!(RawMidiEvent::from_bytes(&[]).is_none());
    }

    #[test]
    fn from_bytes_truncated_two_bytes_returns_none() {
        // 2-byte NoteOn is malformed (status + note, missing velocity) — reject.
        assert!(RawMidiEvent::from_bytes(&[0x90, 45]).is_none());
    }

    #[test]
    fn from_bytes_one_byte_note_on_accepted() {
        // 1-byte status-only NoteOn (USB MIDI CIN 0x0F single-byte) — accepted with number=0.
        let ev = RawMidiEvent::from_bytes(&[0x90]).unwrap();
        assert_eq!(ev.kind, MidiBindKind::NoteOn);
        assert_eq!(ev.channel, 1);
        assert_eq!(ev.number, 0);
        assert!(ev.is_down);
    }

    #[test]
    fn from_bytes_one_byte_note_off_accepted() {
        let ev = RawMidiEvent::from_bytes(&[0x80]).unwrap();
        assert_eq!(ev.kind, MidiBindKind::NoteOn);
        assert_eq!(ev.channel, 1);
        assert_eq!(ev.number, 0);
        assert!(!ev.is_down);
    }

    #[test]
    fn from_bytes_one_byte_realtime_returns_none() {
        // 1-byte realtime messages (0xF8 clock, 0xFE active sensing) — not NoteOn/Off/CC.
        assert!(RawMidiEvent::from_bytes(&[0xF8]).is_none());
        assert!(RawMidiEvent::from_bytes(&[0xFE]).is_none());
    }

    #[test]
    fn from_bytes_unsupported_returns_none() {
        // SysEx (0xF0), clock (0xF8), active sensing (0xFE), program change (0xC0) → None
        assert!(RawMidiEvent::from_bytes(&[0xF0, 0x7E, 0x00]).is_none());
        assert!(RawMidiEvent::from_bytes(&[0xC0, 0x01, 0x00]).is_none()); // program change
    }

    #[test]
    fn matches_binding() {
        let binding = MidiBinding {
            channel: 1,
            kind: MidiBindKind::NoteOn,
            number: 45,
        };
        let ev = RawMidiEvent::from_bytes(&[0x90, 45, 100]).unwrap();
        assert!(ev.matches(&binding));

        let ev2 = RawMidiEvent::from_bytes(&[0x90, 46, 100]).unwrap();
        assert!(!ev2.matches(&binding));
    }

    #[test]
    fn channel_mismatch_does_not_match() {
        let ev = RawMidiEvent::from_bytes(&[0x91, 60, 100]).unwrap(); // ch 2 NoteOn
        let binding = MidiBinding {
            channel: 1,
            kind: MidiBindKind::NoteOn,
            number: 60,
        };
        assert!(!ev.matches(&binding), "different channel should not match");
    }

    #[test]
    fn channel_match_does_match() {
        let ev = RawMidiEvent::from_bytes(&[0x90, 60, 100]).unwrap(); // ch 1 NoteOn
        let binding = MidiBinding {
            channel: 1,
            kind: MidiBindKind::NoteOn,
            number: 60,
        };
        assert!(ev.matches(&binding), "same channel should match");
    }

    // --- MidiLearnState tests ---

    #[test]
    fn learn_state_resolves_after_500ms() {
        let mut state = MidiLearnState::new();
        state.begin(MidiLearnTarget::Dit);

        let ev = RawMidiEvent {
            kind: MidiBindKind::NoteOn,
            channel: 1,
            number: 36,
            is_down: true,
        };

        // Feed event, then backdate the start time to simulate 500ms elapsed.
        state.feed(ev);
        state.start = Some(Instant::now() - std::time::Duration::from_millis(500));

        let result = state.try_resolve();
        assert!(result.is_some(), "should resolve after 500ms");
        let (target, binding) = result.unwrap();
        assert_eq!(target, MidiLearnTarget::Dit);
        assert_eq!(binding.number, 36);
        assert_eq!(binding.kind, MidiBindKind::NoteOn);
        assert!(
            !state.is_learning(),
            "state should be cleared after resolution"
        );
    }

    #[test]
    fn learn_state_does_not_resolve_before_500ms() {
        let mut state = MidiLearnState::new();
        state.begin(MidiLearnTarget::Dah);

        let ev = RawMidiEvent {
            kind: MidiBindKind::NoteOn,
            channel: 1,
            number: 36,
            is_down: true,
        };
        state.feed(ev);
        // Do NOT backdate — elapsed < 500ms

        assert!(
            state.try_resolve().is_none(),
            "should not resolve before 500ms"
        );
        assert!(state.is_learning(), "session should still be active");
    }

    #[test]
    fn learn_state_prefers_noteon_over_cc() {
        let mut state = MidiLearnState::new();
        state.begin(MidiLearnTarget::Dit);

        let cc = RawMidiEvent {
            kind: MidiBindKind::ControlChange,
            channel: 1,
            number: 7,
            is_down: true,
        };
        let note = RawMidiEvent {
            kind: MidiBindKind::NoteOn,
            channel: 1,
            number: 36,
            is_down: true,
        };

        state.feed(cc);
        state.feed(note);
        state.start = Some(Instant::now() - std::time::Duration::from_millis(500));

        let (_, binding) = state.try_resolve().unwrap();
        assert_eq!(
            binding.kind,
            MidiBindKind::NoteOn,
            "NoteOn should win over CC"
        );
    }

    #[test]
    fn learn_state_deduplicates_candidates() {
        let mut state = MidiLearnState::new();
        state.begin(MidiLearnTarget::Dit);

        let ev = RawMidiEvent {
            kind: MidiBindKind::NoteOn,
            channel: 1,
            number: 36,
            is_down: true,
        };
        state.feed(ev);
        state.feed(ev); // duplicate — same kind + channel + number
        state.feed(RawMidiEvent {
            kind: MidiBindKind::NoteOn,
            channel: 1,
            number: 36,
            is_down: false,
        });

        // Should only have one candidate
        assert_eq!(state.candidates.len(), 1);
    }

    #[test]
    fn learn_state_does_not_deduplicate_different_channels() {
        let mut state = MidiLearnState::new();
        state.begin(MidiLearnTarget::Dit);

        // Same note number but different channels — both kept as distinct candidates.
        let ev_ch1 = RawMidiEvent {
            kind: MidiBindKind::NoteOn,
            channel: 1,
            number: 36,
            is_down: true,
        };
        let ev_ch2 = RawMidiEvent {
            kind: MidiBindKind::NoteOn,
            channel: 2,
            number: 36,
            is_down: true,
        };
        state.feed(ev_ch1);
        state.feed(ev_ch2);

        assert_eq!(
            state.candidates.len(),
            2,
            "different channels should not be deduplicated"
        );
    }

    #[test]
    fn learn_state_cancel_clears_everything() {
        let mut state = MidiLearnState::new();
        state.begin(MidiLearnTarget::Dit);
        let ev = RawMidiEvent {
            kind: MidiBindKind::NoteOn,
            channel: 1,
            number: 36,
            is_down: true,
        };
        state.feed(ev);
        state.cancel();

        assert!(!state.is_learning());
        assert!(state.target().is_none());
        assert!(state.try_resolve().is_none());
    }

    #[test]
    fn feed_is_noop_when_inactive() {
        let mut state = MidiLearnState::new();
        let ev = RawMidiEvent {
            kind: MidiBindKind::NoteOn,
            channel: 1,
            number: 36,
            is_down: true,
        };
        state.feed(ev); // no begin() called
        assert!(state.try_resolve().is_none());
        assert!(!state.is_learning());
    }
}
