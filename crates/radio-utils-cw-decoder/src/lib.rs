#![no_std]
#![forbid(unsafe_code)]

//! Streaming CW (Morse code) decoder.
//!
//! `no_std`, no allocator. All state lives inline in [`Decoder`]: a
//! packed `u8` accumulator for the in-flight character and a
//! `heapless::Deque<u8, 32>` ring of recently decoded ASCII bytes.
//!
//! ## Usage
//!
//! Feed the decoder with [`Decoder::on_transition`] whenever the keyed
//! line flips (key-down → key-up or vice versa), passing the current
//! timestamp in microseconds and the sender's WPM. Call
//! [`Decoder::poll`] periodically (e.g. every UI tick) so the decoder
//! can flush a completed character once the inter-character silence
//! threshold elapses — without [`Decoder::poll`] the trailing character
//! of a transmission would only appear when the *next* key-down arrives.
//!
//! ## Timing model
//!
//! At `wpm` words per minute the canonical dit duration is
//! `1_200_000 / wpm` microseconds. Standard Morse spacing is:
//!
//! | element                | dits   |
//! |------------------------|--------|
//! | dit pulse              | 1      |
//! | dah pulse              | 3      |
//! | intra-character gap    | 1      |
//! | inter-character gap    | 3      |
//! | inter-word gap         | 7      |
//!
//! The decoder uses these thresholds:
//!
//! * pulse ≥ `2 × dit` → dah (otherwise dit)
//! * gap ≥ `2 × dit`   → emit the in-flight character (inter-character)
//! * gap ≥ `6 × dit`   → also emit a space (inter-word; 6 leaves a one-dit
//!   tolerance below the canonical 7-dit boundary so slightly-clipped
//!   sending still produces word breaks)

/// Maximum decoded characters retained in the recent-history ring.
/// Older characters are evicted from the front on overflow.
pub const HISTORY_CAPACITY: usize = 32;

/// Streaming CW decoder. See the module docs for usage.
pub struct Decoder {
    state: State,
    last_transition_us: u64,
    /// In-flight Morse code as a packed bitstring: a leading `1`
    /// sentinel marks "start of code", followed by `0` (dit) or `1`
    /// (dah) per element. Reset to `1` after each emitted character.
    /// `0` is the sentinel for "overflowed / invalid"; that pending
    /// character is silently dropped on the next emission.
    accum: u8,
    /// Set after emitting an inter-word space so a sustained silence
    /// doesn't keep emitting more spaces on every [`Decoder::poll`].
    /// Cleared on the next key-down.
    space_emitted: bool,
    history: heapless::Deque<u8, HISTORY_CAPACITY>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// No transitions yet — initial state at construction.
    Idle,
    /// Key is currently asserted; a pulse is in progress.
    Down,
    /// Key is currently released; a gap is in progress.
    Up,
}

impl Decoder {
    /// Construct an empty decoder. `const` so it can be placed in a
    /// `static` without lazy initialisation.
    pub const fn new() -> Self {
        Self {
            state: State::Idle,
            last_transition_us: 0,
            accum: 1,
            space_emitted: false,
            history: heapless::Deque::new(),
        }
    }

    /// Feed a key-line transition.
    ///
    /// `now_us` is the absolute time of the edge — any monotonic clock
    /// works since only deltas are used. `key_down` is the new state of
    /// the line. `wpm` is the sender's speed; thresholds derive from it.
    ///
    /// O(1) work — safe to call from a high-priority context.
    pub fn on_transition(&mut self, now_us: u64, key_down: bool, wpm: u8) {
        let dit_us = dit_duration_us(wpm) as u64;
        let elapsed = now_us.saturating_sub(self.last_transition_us);
        if key_down {
            if self.state == State::Up {
                self.process_gap(elapsed, dit_us);
            }
            self.state = State::Down;
        } else {
            if self.state == State::Down {
                self.process_pulse(elapsed, dit_us);
            }
            self.state = State::Up;
            self.space_emitted = false;
        }
        self.last_transition_us = now_us;
    }

    /// Poll the decoder so pending characters and inter-word spaces
    /// get flushed during extended silence.
    ///
    /// Call at a regular cadence (e.g. once per UI tick). Idempotent
    /// during a single sustained silence — repeated calls emit at most
    /// one character and one space.
    ///
    /// O(1) work.
    pub fn poll(&mut self, now_us: u64, wpm: u8) {
        if self.state != State::Up {
            return;
        }
        let dit_us = dit_duration_us(wpm) as u64;
        let gap = now_us.saturating_sub(self.last_transition_us);
        self.process_gap(gap, dit_us);
    }

    /// Copy the recent decoded history (oldest-first ASCII bytes) into
    /// `out`. Returns the number of bytes written. Does not mutate.
    pub fn snapshot(&self, out: &mut [u8]) -> usize {
        let mut n = 0;
        for &ch in self.history.iter() {
            if n >= out.len() {
                break;
            }
            out[n] = ch;
            n += 1;
        }
        n
    }

    /// Number of characters currently in the recent-history ring.
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// True iff no characters have been decoded yet (or [`Decoder::clear`]
    /// was called).
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    /// Drop the recent history and reset in-flight Morse state. The
    /// next pulse / gap is treated as the start of a fresh stream.
    pub fn clear(&mut self) {
        self.history.clear();
        self.accum = 1;
        self.space_emitted = false;
        self.state = State::Idle;
    }

    fn process_pulse(&mut self, pulse_us: u64, dit_us: u64) {
        // Invariants on `accum` going into this function:
        //   * `accum == 0`  → in-flight char was already invalidated;
        //     skip until the next gap clears it.
        //   * `accum == 1`  → start of a new character (just the sentinel).
        //   * `accum` in `[2, 127]` → 1..6 elements accumulated; sentinel
        //     still in the top half of the u8. Valid Morse is ≤ 6
        //     elements so this is the full legal range emitted to
        //     LOOKUP at gap time.
        //   * `accum` in `[128, 255]` → 7th element would shift the
        //     sentinel out of the u8. Caught below and zeroed.
        if self.accum == 0 {
            return;
        }
        if self.accum & 0b1000_0000 != 0 {
            self.accum = 0;
            return;
        }
        let bit = if pulse_us >= 2 * dit_us { 1 } else { 0 };
        self.accum = (self.accum << 1) | bit;
    }

    fn process_gap(&mut self, gap_us: u64, dit_us: u64) {
        if gap_us >= 6 * dit_us {
            self.emit_pending_char();
            if !self.space_emitted {
                self.push_char(b' ');
                self.space_emitted = true;
            }
        } else if gap_us >= 2 * dit_us {
            self.emit_pending_char();
        }
        // < 2 × dit: intra-character gap; in-flight character is still
        // accumulating.
    }

    fn emit_pending_char(&mut self) {
        if self.accum > 1 && self.accum < 128 {
            let ch = LOOKUP[self.accum as usize];
            if ch != 0 {
                self.push_char(ch);
            }
        }
        self.accum = 1;
    }

    fn push_char(&mut self, ch: u8) {
        if self.history.is_full() {
            let _ = self.history.pop_front();
        }
        let _ = self.history.push_back(ch);
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

fn dit_duration_us(wpm: u8) -> u32 {
    1_200_000 / wpm.max(1) as u32
}

// ── Morse → ASCII lookup ──────────────────────────────────────────
// Indexed by the packed Morse representation: leading 1 sentinel, then
// 0 (dit) or 1 (dah) per element. So E (.) = 0b10 = 2, T (-) = 0b11 = 3,
// A (.-) = 0b101 = 5, and so on. Entries left at 0 are "no character"
// (unallocated codes or codes we choose not to decode).
const LOOKUP: [u8; 128] = {
    let mut t = [0u8; 128];
    // Letters
    t[2] = b'E';
    t[3] = b'T';
    t[4] = b'I';
    t[5] = b'A';
    t[6] = b'N';
    t[7] = b'M';
    t[8] = b'S';
    t[9] = b'U';
    t[10] = b'R';
    t[11] = b'W';
    t[12] = b'D';
    t[13] = b'K';
    t[14] = b'G';
    t[15] = b'O';
    t[16] = b'H';
    t[17] = b'V';
    t[18] = b'F';
    t[20] = b'L';
    t[22] = b'P';
    t[23] = b'J';
    t[24] = b'B';
    t[25] = b'X';
    t[26] = b'C';
    t[27] = b'Y';
    t[28] = b'Z';
    t[29] = b'Q';
    // Digits
    t[32] = b'5';
    t[33] = b'4';
    t[35] = b'3';
    t[39] = b'2';
    t[47] = b'1';
    t[48] = b'6';
    t[56] = b'7';
    t[60] = b'8';
    t[62] = b'9';
    t[63] = b'0';
    // Punctuation
    t[49] = b'='; // -...-   BT (paragraph break / "=")
    t[50] = b'/'; // -..-.
    t[76] = b'?'; // ..--..
    t[85] = b'.'; // .-.-.-
    t[115] = b','; // --..--
    t
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a sequence of (key_down, duration_us) transitions through
    /// a fresh decoder and return its decoded text (oldest-first).
    /// After the final transition, polls once at the same timestamp so
    /// any in-flight character that completed via the last `(false, ...)`
    /// gap gets flushed without adding extra trailing silence.
    fn decode(wpm: u8, durations: &[(bool, u64)]) -> heapless::String<64> {
        let mut d = Decoder::new();
        let mut now = 0u64;
        for &(k, dur) in durations {
            d.on_transition(now, k, wpm);
            now += dur;
        }
        d.poll(now, wpm);
        let mut buf = [0u8; 64];
        let n = d.snapshot(&mut buf);
        let mut out: heapless::String<64> = heapless::String::new();
        for &b in &buf[..n] {
            let _ = out.push(b as char);
        }
        out
    }

    const fn dit_us(wpm: u8) -> u64 {
        1_200_000 / wpm as u64
    }

    #[test]
    fn decodes_single_dit_as_e() {
        let d = dit_us(20);
        // Pulse 1 dit, then 4-dit gap to flush.
        let r = decode(20, &[(true, d), (false, 4 * d)]);
        assert_eq!(r.as_str(), "E");
    }

    #[test]
    fn decodes_single_dah_as_t() {
        let d = dit_us(20);
        let r = decode(20, &[(true, 3 * d), (false, 4 * d)]);
        assert_eq!(r.as_str(), "T");
    }

    #[test]
    fn decodes_paris() {
        let d = dit_us(20);
        let s = 3 * d;
        let g_intra = d;
        let g_inter = 3 * d;
        // Trailing gap < 6 × dit so we don't get a trailing space.
        let g_final = 4 * d;
        let seq = &[
            // P = .--.
            (true, d),
            (false, g_intra),
            (true, s),
            (false, g_intra),
            (true, s),
            (false, g_intra),
            (true, d),
            (false, g_inter),
            // A = .-
            (true, d),
            (false, g_intra),
            (true, s),
            (false, g_inter),
            // R = .-.
            (true, d),
            (false, g_intra),
            (true, s),
            (false, g_intra),
            (true, d),
            (false, g_inter),
            // I = ..
            (true, d),
            (false, g_intra),
            (true, d),
            (false, g_inter),
            // S = ...
            (true, d),
            (false, g_intra),
            (true, d),
            (false, g_intra),
            (true, d),
            (false, g_final),
        ];
        assert_eq!(decode(20, seq).as_str(), "PARIS");
    }

    #[test]
    fn emits_word_space_after_7_dit_gap() {
        let d = dit_us(20);
        let s = 3 * d;
        let seq = &[
            (true, s),
            (false, 7 * d), // T then word break
            (true, d),
            (false, 4 * d), // E + flush
        ];
        assert_eq!(decode(20, seq).as_str(), "T E");
    }

    #[test]
    fn space_at_exact_inter_word_threshold() {
        // Boundary: gap of exactly 6 × dit must emit the space (the
        // `>=` branch in `process_gap`). 7-dit and 5-dit cases are
        // covered by the surrounding tests; this nails the threshold
        // itself.
        let d = dit_us(20);
        let s = 3 * d;
        let seq = &[(true, s), (false, 6 * d), (true, d), (false, 4 * d)];
        assert_eq!(decode(20, seq).as_str(), "T E");
    }

    #[test]
    fn no_space_below_inter_word_threshold() {
        let d = dit_us(20);
        let s = 3 * d;
        // 5 × dit gap is above inter-char (2 ×) but below inter-word
        // (6 ×) — flush the char but no space.
        let seq = &[(true, s), (false, 5 * d), (true, d), (false, 4 * d)];
        assert_eq!(decode(20, seq).as_str(), "TE");
    }

    #[test]
    fn decodes_digits() {
        let d = dit_us(20);
        let s = 3 * d;
        let g_intra = d;
        let g_inter = 3 * d;
        let g_final = 4 * d;
        // "73"
        let seq = &[
            // 7 = --...
            (true, s),
            (false, g_intra),
            (true, s),
            (false, g_intra),
            (true, d),
            (false, g_intra),
            (true, d),
            (false, g_intra),
            (true, d),
            (false, g_inter),
            // 3 = ...--
            (true, d),
            (false, g_intra),
            (true, d),
            (false, g_intra),
            (true, d),
            (false, g_intra),
            (true, s),
            (false, g_intra),
            (true, s),
            (false, g_final),
        ];
        assert_eq!(decode(20, seq).as_str(), "73");
    }

    #[test]
    fn poll_alone_flushes_pending_character() {
        // Don't feed a final inter-char gap — rely on poll to detect
        // the silence and emit. Mirrors the firmware UI pattern where
        // the periodic poll is what produces the last char of a
        // transmission.
        let d = dit_us(20);
        let mut dec = Decoder::new();
        dec.on_transition(0, true, 20); // key down
        dec.on_transition(d, false, 20); // key up after 1 dit
                                         // No further transitions. Poll 4 dits after the last edge.
        dec.poll(d + 4 * d, 20);
        let mut buf = [0u8; 4];
        let n = dec.snapshot(&mut buf);
        assert_eq!(&buf[..n], b"E");
    }

    #[test]
    fn repeated_poll_during_silence_emits_one_space() {
        let d = dit_us(20);
        let mut dec = Decoder::new();
        dec.on_transition(0, true, 20);
        dec.on_transition(d, false, 20);
        // Poll three times deep into the inter-word silence — should
        // produce "E " once, not "E   ".
        for _ in 0..3 {
            dec.poll(d + 10 * d, 20);
        }
        let mut buf = [0u8; 8];
        let n = dec.snapshot(&mut buf);
        assert_eq!(&buf[..n], b"E ");
    }

    #[test]
    fn clear_resets_history_and_in_flight() {
        let d = dit_us(20);
        let mut dec = Decoder::new();
        dec.on_transition(0, true, 20);
        dec.on_transition(d, false, 20);
        dec.poll(d + 4 * d, 20);
        assert_eq!(dec.len(), 1);
        dec.clear();
        assert!(dec.is_empty());
        // After clear, a fresh transmission decodes from scratch.
        dec.on_transition(0, true, 20);
        dec.on_transition(3 * d, false, 20);
        dec.poll(3 * d + 4 * d, 20);
        let mut buf = [0u8; 4];
        let n = dec.snapshot(&mut buf);
        assert_eq!(&buf[..n], b"T");
    }

    #[test]
    fn overflowed_character_silently_dropped() {
        // 10 dits in one "character" — way more than the 6 allowed by
        // valid Morse codes. Decoder should mark in-flight invalid and
        // emit nothing on the next gap.
        let d = dit_us(20);
        let mut dec = Decoder::new();
        let mut now = 0u64;
        let mut last_edge = 0u64;
        for _ in 0..10 {
            dec.on_transition(now, true, 20);
            now += d;
            dec.on_transition(now, false, 20);
            last_edge = now;
            now += d; // intra-char gap
        }
        // Poll at 3 × dit silence after the last edge — enough to
        // trigger inter-char emit (≥ 2 × dit), well below the 6 × dit
        // inter-word threshold so no trailing space is added.
        dec.poll(last_edge + 3 * d, 20);
        assert!(
            dec.is_empty(),
            "expected no decoded chars from overflow, got len={}",
            dec.len()
        );
    }

    #[test]
    fn history_evicts_oldest_on_overflow() {
        // Send 40 dits with inter-char gaps; the ring holds only 32.
        let d = dit_us(20);
        let mut dec = Decoder::new();
        let mut now = 0u64;
        for _ in 0..40 {
            dec.on_transition(now, true, 20);
            now += d;
            dec.on_transition(now, false, 20);
            now += 3 * d;
        }
        dec.poll(now + 2 * d, 20);
        // Capacity is 32 — last 32 'E's retained, first 8 evicted.
        let mut buf = [0u8; 64];
        let n = dec.snapshot(&mut buf);
        assert_eq!(n, HISTORY_CAPACITY);
        assert!(buf[..n].iter().all(|&b| b == b'E'));
    }
}
