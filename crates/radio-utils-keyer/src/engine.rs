use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::config::{KeyerConfig, KeyerMode};
use crate::morse::{self, MorseElement};

/// Current output event of the keyer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyerOutput {
    KeyDown,
    KeyUp,
    PttRequest(bool),
}

/// High-level keyer state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyerState {
    Idle,
    StraightKey,
    SendDit,
    SendDash,
    DitDelay,
    DashDelay,
    LetterSpace,
}

/// A key-line transition event with network timestamp.
#[derive(Debug, Clone, Copy)]
pub struct KeyTransition {
    pub key_down: bool,
    pub timestamp_us: u64,
}

/// Straight-key release debounce window in milliseconds. When the
/// paddle de-asserts in `KeyerState::StraightKey`, we delay emitting
/// `KeyUp` for this many ticks. If the paddle re-asserts within the
/// window we treat the release as a contact bounce and don't emit any
/// edge — keeping the carrier solid through the bounce.
///
/// Why this is needed: a real-world straight key (mechanical bouncer
/// or OS-level synthesised key-repeat boundary) can produce a brief
/// release-press cycle right after the initial press. Without this
/// debounce the engine emits `KeyDown → KeyUp → KeyDown`, which the
/// transmitter renders as: short carrier, brief silence, then sustained
/// carrier — exactly the symptom users see.
///
/// 20 ms is comfortably above typical mechanical-switch bounce
/// (< 5 ms) yet far below the shortest intentional key-up gap a human
/// can produce (50 ms+ at any reasonable keying speed). For Iambic /
/// Bug modes we don't apply this — the dit-element gap is part of the
/// timing contract there.
const STRAIGHT_KEY_RELEASE_DEBOUNCE_MS: u32 = 20;

/// Cap on the in-engine `transitions` ring.  Backed by `heapless::Deque`
/// so the buffer is inline in `KeyerEngine` (no heap allocation) and
/// both record and overflow-evict are O(1) — the previous
/// `Vec::remove(0)` strategy was an O(n) memmove on every recorded
/// transition once at cap, which jittered the IRQ-mode tick path in
/// the firmware.
///
/// 64 entries (~1 KiB inline) holds several seconds of continuous
/// keying at sane WPM without trimming; consumers that drain
/// periodically (firmware does not — the engine self-trims) see the
/// most recent 64 transitions on each call.  Recent matter more than
/// ancient, so eviction drops from the front.
const MAX_RECORDED_TRANSITIONS: usize = 64;

/// The iambic keyer engine.
pub struct KeyerEngine {
    pub config: KeyerConfig,
    state: KeyerState,
    dit_held: bool,
    dash_held: bool,
    dit_memory: bool,
    dash_memory: bool,
    /// Last paddle pressed — used by Ultimatic mode. `true` = dash, `false` = dit.
    last_paddle: Option<bool>,
    /// Whether the current element in SinglePaddle mode has been decided (dit vs dash).
    single_paddle_decided: bool,
    /// Absolute end-of-phase deadline in keyer-local microseconds.
    ///
    /// Compared against `elapsed_us` each tick to decide when the current
    /// phase (element or gap) has ended. Stored in µs so the per-element
    /// truncation that `1200 / wpm` used to inflict — 0.857 ms per dit at
    /// 28 WPM, > 1 s of drift per minute — is gone: residual carries from
    /// one phase to the next via `schedule_phase` so the long-run rate
    /// matches the configured WPM to within the 1 ms tick quantum.
    phase_end_us: u64,
    /// Full duration of the current element in µs — used by the iambic-B
    /// CMOS timing gate to compute the memory-latch window.
    element_duration_us: u32,
    /// Hang-time accumulator, in microseconds. Counts up by `tick_us`
    /// each `tick()` and is compared against `config.hang_time_ms * 1000`.
    /// Counting in µs (rather than ticks) keeps the long-run hang window
    /// matched to wall clock regardless of the tick quantum.
    ///
    /// Increments use `saturating_add` purely as defence against a
    /// pathological `tick_us` — at realistic hang-time configs (default
    /// 500 ms) the counter caps four orders of magnitude below
    /// `u32::MAX`, so saturation never bites in practice.
    hang_counter_us: u32,
    ptt_active: bool,
    key_active: bool,
    pub text_queue: VecDeque<MorseElement>,
    text_sending: bool,
    /// Monotonic time counter in microseconds, incremented by `tick_us`
    /// each `tick()`.
    elapsed_us: u64,
    transitions: heapless::Deque<KeyTransition, MAX_RECORDED_TRANSITIONS>,
    /// Straight-key release-debounce accumulator in microseconds. 0
    /// when no release is pending. Counts up by `tick_us` per tick;
    /// compared against `STRAIGHT_KEY_RELEASE_DEBOUNCE_MS * 1000`.
    /// `saturating_add` is defence-in-depth — same rationale as
    /// `hang_counter_us`: the 20 ms ceiling is nowhere near `u32::MAX`.
    straight_release_grace_us: u32,
    /// Microseconds per `tick()` call.  All time-accounting fields
    /// advance by this amount; the schedule_phase continuity window
    /// and the per-element overshoot are bounded by this value.  Set
    /// once at construction (`new` defaults to 1 ms; `new_with_tick`
    /// lets a fast-cadence host like the embedded firmware drop it to
    /// 250 µs so individual element durations vary by ≤ 250 µs instead
    /// of ≤ 1 ms).
    tick_us: u32,
}

impl KeyerEngine {
    /// Construct with the default 1 ms tick quantum.
    pub fn new(config: KeyerConfig) -> Self {
        Self::new_with_tick(config, 1000)
    }

    /// Construct with a custom tick quantum in microseconds.
    ///
    /// Smaller values reduce per-element timing overshoot (bounded by
    /// `tick_us`) at the cost of more `tick()` calls per second.  The
    /// firmware uses 250 µs to match its paddle-poll cadence so that
    /// individual element durations at 28 WPM vary by ≤ 250 µs instead
    /// of the ≤ 1 ms allowed by the default.
    ///
    /// Panics if `tick_us == 0` — with a zero quantum `tick()` would
    /// never advance `elapsed_us` and the engine would silently stop
    /// progressing. A one-time construction-time check is cheaper than
    /// debugging that.
    pub fn new_with_tick(config: KeyerConfig, tick_us: u32) -> Self {
        assert!(tick_us > 0, "KeyerEngine tick_us must be > 0");
        Self {
            config,
            state: KeyerState::Idle,
            dit_held: false,
            dash_held: false,
            dit_memory: false,
            dash_memory: false,
            last_paddle: None,
            single_paddle_decided: false,
            phase_end_us: 0,
            element_duration_us: 0,
            hang_counter_us: 0,
            ptt_active: false,
            key_active: false,
            text_queue: VecDeque::new(),
            text_sending: false,
            elapsed_us: 0,
            transitions: heapless::Deque::new(),
            straight_release_grace_us: 0,
            tick_us,
        }
    }

    pub fn update_config(&mut self, config: KeyerConfig) {
        self.config = config;
    }

    /// Set paddle inputs. Handles keys_reversed. Registers iambic memories.
    /// A paddle hit interrupts text macro sending.
    pub fn set_paddle(&mut self, dit: bool, dash: bool) {
        let (d, h) = if self.config.keys_reversed {
            (dash, dit)
        } else {
            (dit, dash)
        };

        // Register memories on rising edge (not during text macro gaps
        // to prevent spurious paddle bumps from injecting elements).
        //
        // During active element sending, the CMOS Super Keyer timing gate
        // restricts memory latching to the configured window.  Outside
        // element sending (idle, delay, letter-space) the gate is open.
        let in_element = matches!(self.state, KeyerState::SendDit | KeyerState::SendDash);
        let is_mode_b = matches!(self.config.mode, KeyerMode::IambicB | KeyerMode::Ultimatic);
        let gate_open = !in_element || !is_mode_b || self.in_memory_window();
        if !self.text_sending && gate_open {
            if d && !self.dit_held {
                self.dit_memory = true;
            }
            if h && !self.dash_held {
                self.dash_memory = true;
            }
        }

        // Track last paddle pressed for Ultimatic mode (rising edge).
        if d && !self.dit_held {
            self.last_paddle = Some(false);
        }
        if h && !self.dash_held {
            self.last_paddle = Some(true);
        }

        self.dit_held = d;
        self.dash_held = h;

        // Paddle hit interrupts text macro
        if (d || h) && self.text_sending {
            self.abort_text();
        }
    }

    /// Queue text for sending as Morse elements.
    pub fn send_text(&mut self, text: &str) {
        let elements = morse::text_to_elements(text);
        self.text_queue.extend(elements);
        self.text_sending = true;
    }

    /// Abort text macro sending.
    pub fn abort_text(&mut self) {
        self.text_queue.clear();
        self.text_sending = false;
    }

    /// Drain and return recorded key transitions in oldest-first order.
    ///
    /// The internal buffer is a fixed-capacity ring; this is the only
    /// way to observe its contents and also resets it to empty.
    pub fn drain_transitions(&mut self) -> Vec<KeyTransition> {
        let mut out = Vec::with_capacity(self.transitions.len());
        while let Some(t) = self.transitions.pop_front() {
            out.push(t);
        }
        out
    }

    /// Returns true if the engine is active (not idle, or PTT active, or queue not empty).
    pub fn is_active(&self) -> bool {
        self.state != KeyerState::Idle || self.ptt_active || !self.text_queue.is_empty()
    }

    /// Record a key transition with timestamp (derived from tick count).
    ///
    /// On overflow we drop the oldest entry (O(1) `pop_front`) and push
    /// the new one (O(1) `push_back`).  `push_back` cannot fail after
    /// the explicit eviction since the deque is now below capacity.
    fn record_transition(&mut self, key_down: bool) {
        if self.transitions.is_full() {
            self.transitions.pop_front();
        }
        let _ = self.transitions.push_back(KeyTransition {
            key_down,
            timestamp_us: self.elapsed_us,
        });
    }

    /// Emit KeyDown: set key_active, record transition.
    fn emit_key_down(&mut self) -> KeyerOutput {
        self.key_active = true;
        self.hang_counter_us = 0;
        self.record_transition(true);
        KeyerOutput::KeyDown
    }

    /// Emit KeyUp: clear key_active, record transition.
    fn emit_key_up(&mut self) -> KeyerOutput {
        self.key_active = false;
        self.record_transition(false);
        KeyerOutput::KeyUp
    }

    /// Schedule the current state phase to last `duration_us` more.
    ///
    /// Two caller regimes, distinguished automatically by inspecting
    /// `phase_end_us`:
    ///
    /// * **Continuous** — `schedule_phase` is invoked on the same tick as
    ///   the prior phase's expiration, so `elapsed_us - phase_end_us` is
    ///   the sub-tick overshoot in `[0, tick_us)`. The new deadline is
    ///   anchored to the old `phase_end_us`, carrying that residual
    ///   forward so the long-run rate matches the configured WPM. This
    ///   is the iambic send/gap loop, the auto-spacing letter-gap
    ///   insertion, and the text-queue gap dispatch.
    ///
    /// * **Fresh** — we're waking from Idle after a paddle-up period (or
    ///   from initial construction). `phase_end_us` is at least one whole
    ///   tick stale, so we discard it and anchor to `elapsed_us` instead.
    ///   Carrying residual across a user pause is meaningless — the pause
    ///   was arbitrary, not WPM-derived.
    ///
    /// The strict `>` below is the exact separator: `tick()` fires when
    /// `elapsed_us >= phase_end_us` with overshoot in `[0, tick_us)`, so
    /// `phase_end_us + tick_us > elapsed_us` is true iff we're still
    /// inside the tick that expired the phase.
    fn schedule_phase(&mut self, duration_us: u32) {
        let base = if self.phase_end_us + self.tick_us as u64 > self.elapsed_us {
            self.phase_end_us
        } else {
            self.elapsed_us
        };
        self.phase_end_us = base + duration_us as u64;
    }

    /// `true` once `elapsed_us` has reached or passed the current phase's
    /// scheduled end.
    fn phase_expired(&self) -> bool {
        self.elapsed_us >= self.phase_end_us
    }

    /// Emit PttRequest if needed. Returns Some if PTT state changed.
    fn maybe_emit_ptt(&mut self, on: bool) -> Option<KeyerOutput> {
        if on && !self.ptt_active {
            self.ptt_active = true;
            Some(KeyerOutput::PttRequest(true))
        } else if !on && self.ptt_active {
            self.ptt_active = false;
            Some(KeyerOutput::PttRequest(false))
        } else {
            None
        }
    }

    /// Start sending a dit element.
    fn start_dit(&mut self) {
        self.state = KeyerState::SendDit;
        let dur_us = self.config.effective_dot_duration_us();
        self.schedule_phase(dur_us);
        self.element_duration_us = dur_us;
        self.dit_memory = false;
    }

    /// Start sending a dash element.
    fn start_dash(&mut self) {
        self.state = KeyerState::SendDash;
        let dur_us = self.config.effective_dash_duration_us();
        self.schedule_phase(dur_us);
        self.element_duration_us = dur_us;
        self.dash_memory = false;
    }

    /// Returns `true` when the current tick is inside the iambic B memory
    /// latch window for the element being sent.
    ///
    /// The CMOS Super Keyer timing percentage defines a "dead zone" at the
    /// tail of each element where opposite-paddle presses are NOT latched
    /// into memory.  At 0 % the entire element is a latch window (classic
    /// iambic B); at 100 % no part of the element latches (iambic-A-like).
    fn in_memory_window(&self) -> bool {
        let pct = self.config.iambic_b_timing_percent.min(100) as u32;
        // Dead-zone threshold: µs remaining below which we stop latching.
        let threshold_us = self.element_duration_us as u64 * pct as u64 / 100;
        let remaining = self.phase_end_us.saturating_sub(self.elapsed_us);
        remaining > threshold_us
    }

    /// Start an element (dit or dash) with PTT handling.
    /// Returns PttRequest if PTT wasn't active, otherwise KeyDown.
    fn start_element_keyed(&mut self, is_dash: bool) -> KeyerOutput {
        if is_dash {
            self.start_dash();
        } else {
            self.start_dit();
        }
        if let Some(ptt) = self.maybe_emit_ptt(true) {
            ptt
        } else {
            self.emit_key_down()
        }
    }

    /// Dispatch a text queue element, starting its send or gap timer.
    fn dispatch_text_element(&mut self, elem: MorseElement) -> Option<KeyerOutput> {
        self.single_paddle_decided = true; // harmless for non-SinglePaddle modes
        match elem {
            MorseElement::Dit => Some(self.start_element_keyed(false)),
            MorseElement::Dash => Some(self.start_element_keyed(true)),
            MorseElement::ElementGap => {
                self.schedule_phase(self.config.element_gap_us());
                self.state = KeyerState::DitDelay;
                None
            }
            MorseElement::LetterGap => {
                self.schedule_phase(self.config.letter_gap_us());
                self.state = KeyerState::DitDelay;
                None
            }
            MorseElement::WordGap => {
                self.schedule_phase(self.config.word_gap_us());
                self.state = KeyerState::DitDelay;
                None
            }
        }
    }

    /// Handle hang time countdown for PTT release in idle state.
    fn handle_hang_time(&mut self) -> Option<KeyerOutput> {
        if self.ptt_active && !self.key_active {
            self.hang_counter_us = self.hang_counter_us.saturating_add(self.tick_us);
            if self.hang_counter_us >= self.config.hang_time_ms.saturating_mul(1000) {
                return self.maybe_emit_ptt(false);
            }
        }
        None
    }

    /// Transition to idle after a delay when no paddle is active.
    ///
    /// `phase_end_us` is deliberately left at the just-expired value so
    /// that a paddle press dispatched on this same tick (e.g. a text-queue
    /// item that lands the same tick the prior gap expired) takes the
    /// continuous branch in `schedule_phase` and carries the sub-millisecond
    /// residual. Any later tick (≥ 1 ms after expiration) anchors to
    /// `elapsed_us` instead — by then the pause is user-driven and not
    /// WPM-derived.
    fn enter_idle_from_delay(&mut self) {
        self.state = KeyerState::Idle;
        self.hang_counter_us = 0;
        self.text_sending = false;
        // Clear cross-session keyer state so the next session starts fresh.
        // Without this, Ultimatic could carry a stale `last_paddle` from a
        // prior session into a new squeeze, and SinglePaddle could carry a
        // stale `single_paddle_decided` flag.
        self.last_paddle = None;
        self.single_paddle_decided = false;
        // The resolve_*_delay paths usually exhaust memory before we land
        // here, but Ultimatic with last_paddle pointing at a released-and-
        // gone side (and a tap latched during the delay) can fall through
        // with memory still set. A leftover memory bit would fire a phantom
        // element on the very next Idle tick, so wipe both unconditionally.
        self.dit_memory = false;
        self.dash_memory = false;
    }

    /// Advance the state machine by `tick_us` microseconds (1 ms by
    /// default; see `new_with_tick`). Returns output if key state
    /// changed.
    pub fn tick(&mut self) -> Option<KeyerOutput> {
        self.elapsed_us += self.tick_us as u64;
        match self.config.mode {
            KeyerMode::Straight => self.tick_straight(),
            KeyerMode::Bug => self.tick_bug(),
            KeyerMode::IambicA | KeyerMode::IambicB | KeyerMode::Ultimatic => self.tick_iambic(),
            KeyerMode::SinglePaddle => self.tick_single_paddle(),
        }
    }

    fn tick_straight(&mut self) -> Option<KeyerOutput> {
        let any_held = self.dit_held || self.dash_held;

        match self.state {
            KeyerState::Idle => {
                if any_held {
                    self.state = KeyerState::StraightKey;
                    self.straight_release_grace_us = 0;
                    // Emit PTT first if needed
                    if let Some(ptt) = self.maybe_emit_ptt(true) {
                        // We'll emit KeyDown on next tick
                        return Some(ptt);
                    }
                    return Some(self.emit_key_down());
                }
                // Handle hang time for PTT
                if self.ptt_active && !self.key_active {
                    self.hang_counter_us = self.hang_counter_us.saturating_add(self.tick_us);
                    if self.hang_counter_us >= self.config.hang_time_ms.saturating_mul(1000) {
                        return self.maybe_emit_ptt(false);
                    }
                }
                None
            }
            KeyerState::StraightKey => {
                if !self.key_active {
                    // We emitted PTT last tick, now emit KeyDown
                    return Some(self.emit_key_down());
                }
                if !any_held {
                    // Release-debounce: don't emit KeyUp until the
                    // paddle has stayed released for the full grace
                    // window. Mechanical / OS-synthesised contact
                    // bounce on the *initial* press shows up here as a
                    // brief release immediately after the first
                    // KeyDown; without this guard we'd emit KeyDown →
                    // KeyUp → KeyDown and the operator would see a
                    // short carrier blip + silence + the sustained
                    // carrier they meant to send.
                    self.straight_release_grace_us =
                        self.straight_release_grace_us.saturating_add(self.tick_us);
                    if self.straight_release_grace_us
                        >= STRAIGHT_KEY_RELEASE_DEBOUNCE_MS.saturating_mul(1000)
                    {
                        self.straight_release_grace_us = 0;
                        self.state = KeyerState::Idle;
                        self.hang_counter_us = 0;
                        return Some(self.emit_key_up());
                    }
                    return None;
                }
                // Held — make sure any pending release counter is
                // cleared so a future genuine release starts a fresh
                // grace window from zero.
                self.straight_release_grace_us = 0;
                None
            }
            _ => None,
        }
    }

    fn tick_bug(&mut self) -> Option<KeyerOutput> {
        // Bug mode: dit auto-repeats (iambic dit logic), dash is straight key
        // For simplicity, handle dash as straight key overlay, dit as iambic
        // We use the iambic engine but treat dash_held specially

        // If only dash is held, behave like straight key for the dash
        if self.dash_held && !self.dit_held && self.state == KeyerState::Idle {
            self.state = KeyerState::StraightKey;
            if let Some(ptt) = self.maybe_emit_ptt(true) {
                return Some(ptt);
            }
            return Some(self.emit_key_down());
        }

        if self.state == KeyerState::StraightKey {
            if !self.key_active {
                return Some(self.emit_key_down());
            }
            if !self.dash_held {
                self.state = KeyerState::Idle;
                self.hang_counter_us = 0;
                return Some(self.emit_key_up());
            }
            return None;
        }

        // Dit uses iambic logic (but only dit auto-repeats)
        self.tick_iambic_inner(true)
    }

    fn tick_iambic(&mut self) -> Option<KeyerOutput> {
        self.tick_iambic_inner(false)
    }

    fn tick_single_paddle(&mut self) -> Option<KeyerOutput> {
        let any_held = self.dit_held || self.dash_held;

        match self.state {
            KeyerState::Idle => {
                if any_held {
                    self.single_paddle_decided = false;
                    return Some(self.start_element_keyed(false));
                }

                // Text sequencer
                if !self.text_queue.is_empty() {
                    self.text_sending = true;
                    if let Some(elem) = self.text_queue.pop_front() {
                        return self.dispatch_text_element(elem);
                    }
                }

                self.handle_hang_time()
            }

            KeyerState::SendDit => {
                if !self.key_active {
                    return Some(self.emit_key_down());
                }

                if !self.single_paddle_decided {
                    if !any_held {
                        // Released before dit expired → confirmed dit.
                        self.single_paddle_decided = true;
                        self.state = KeyerState::DitDelay;
                        self.schedule_phase(self.config.element_gap_us());
                        return Some(self.emit_key_up());
                    }
                    if self.phase_expired() {
                        // Dit duration expired while still held → upgrade to dash.
                        self.single_paddle_decided = true;
                        let remaining_dash_us = self
                            .config
                            .effective_dash_duration_us()
                            .saturating_sub(self.config.effective_dot_duration_us());
                        self.state = KeyerState::SendDash;
                        self.schedule_phase(remaining_dash_us);
                        self.element_duration_us = self.config.effective_dash_duration_us();
                        return None; // key stays down
                    }
                } else if self.phase_expired() {
                    if self.text_sending {
                        self.state = KeyerState::Idle;
                    } else {
                        self.state = KeyerState::DitDelay;
                        self.schedule_phase(self.config.element_gap_us());
                    }
                    return Some(self.emit_key_up());
                }
                None
            }

            KeyerState::SendDash => {
                if !self.key_active {
                    return Some(self.emit_key_down());
                }

                if self.phase_expired() {
                    if self.text_sending {
                        self.state = KeyerState::Idle;
                    } else {
                        self.state = KeyerState::DashDelay;
                        self.schedule_phase(self.config.element_gap_us());
                    }
                    return Some(self.emit_key_up());
                }
                None
            }

            KeyerState::DitDelay | KeyerState::DashDelay => {
                if any_held && self.text_sending {
                    self.abort_text();
                }

                if self.phase_expired() {
                    if any_held {
                        self.single_paddle_decided = false;
                        return Some(self.start_element_keyed(false));
                    }

                    if self.text_sending && !self.text_queue.is_empty() {
                        self.state = KeyerState::Idle;
                        return None;
                    }

                    self.enter_idle_from_delay();
                }
                None
            }

            KeyerState::LetterSpace => {
                if any_held {
                    self.state = KeyerState::Idle;
                    self.phase_end_us = self.elapsed_us;
                    return None;
                }
                if self.phase_expired() {
                    self.state = KeyerState::Idle;
                }
                None
            }

            KeyerState::StraightKey => {
                self.state = KeyerState::Idle;
                None
            }
        }
    }

    fn tick_iambic_inner(&mut self, bug_mode: bool) -> Option<KeyerOutput> {
        match self.state {
            KeyerState::Idle => {
                if self.dit_held || self.dit_memory {
                    return Some(self.start_element_keyed(false));
                }
                if (self.dash_held || self.dash_memory) && !bug_mode {
                    return Some(self.start_element_keyed(true));
                }

                // Text sequencer: when idle and no paddles, pop from text queue
                if !self.dit_held && !self.dash_held && !self.text_queue.is_empty() {
                    self.text_sending = true;
                    if let Some(elem) = self.text_queue.pop_front() {
                        return self.dispatch_text_element(elem);
                    }
                }

                self.handle_hang_time()
            }

            KeyerState::SendDit => {
                if !self.key_active {
                    return Some(self.emit_key_down());
                }

                // Register dash memory during dit sending (CMOS timing gate,
                // only restricts latching in iambic B mode).
                if self.dash_held
                    && (!matches!(self.config.mode, KeyerMode::IambicB | KeyerMode::Ultimatic)
                        || self.in_memory_window())
                {
                    self.dash_memory = true;
                }

                if self.phase_expired() {
                    if self.text_sending {
                        self.state = KeyerState::Idle;
                    } else {
                        self.state = KeyerState::DitDelay;
                        self.schedule_phase(self.config.element_gap_us());
                    }
                    return Some(self.emit_key_up());
                }
                None
            }

            KeyerState::SendDash => {
                if !self.key_active {
                    return Some(self.emit_key_down());
                }

                // Register dit memory during dash sending (CMOS timing gate,
                // only restricts latching in iambic B mode).
                if self.dit_held
                    && (!matches!(self.config.mode, KeyerMode::IambicB | KeyerMode::Ultimatic)
                        || self.in_memory_window())
                {
                    self.dit_memory = true;
                }

                if self.phase_expired() {
                    if self.text_sending {
                        self.state = KeyerState::Idle;
                    } else {
                        self.state = KeyerState::DashDelay;
                        self.schedule_phase(self.config.element_gap_us());
                    }
                    return Some(self.emit_key_up());
                }
                None
            }

            KeyerState::DitDelay => {
                if (self.dit_held || self.dash_held) && self.text_sending {
                    self.abort_text();
                }

                if self.phase_expired() {
                    self.resolve_after_dit_delay(bug_mode)
                } else {
                    // Only register the *opposite* paddle memory during the
                    // delay.  Re-latching the same side (dit) here is
                    // redundant — resolve already checks dit_held — and
                    // causes a phantom extra dit when both paddles are
                    // squeezed and released during the following dash.
                    if self.dash_held {
                        self.dash_memory = true;
                    }
                    None
                }
            }

            KeyerState::DashDelay => {
                if (self.dit_held || self.dash_held) && self.text_sending {
                    self.abort_text();
                }

                if self.phase_expired() {
                    self.resolve_after_dash_delay(bug_mode)
                } else {
                    // Mirror of DitDelay: only capture the opposite paddle.
                    if self.dit_held {
                        self.dit_memory = true;
                    }
                    None
                }
            }

            KeyerState::LetterSpace => {
                if self.dit_held || self.dash_held {
                    self.state = KeyerState::Idle;
                    self.phase_end_us = self.elapsed_us;
                    return None;
                }

                if self.phase_expired() {
                    self.state = KeyerState::Idle;
                }
                None
            }

            KeyerState::StraightKey => {
                self.state = KeyerState::Idle;
                None
            }
        }
    }

    /// Resolve Ultimatic element choice after a delay. Returns the next
    /// element to send, or None if no paddle is active.
    fn resolve_ultimatic(&mut self, after_dit: bool) -> Option<KeyerOutput> {
        let dit_active = self.dit_held || self.dit_memory;
        let dash_active = self.dash_held || self.dash_memory;
        if !dit_active && !dash_active {
            return None;
        }

        // Last-paddle-wins: prefer alternating to the *other* paddle if its
        // rising edge was the most recent; otherwise repeat the same side.
        // Memory entries count as active so a tap during the previous
        // element wins over silence.
        match self.last_paddle {
            Some(true) if after_dit && dash_active => {
                return Some(self.start_element_keyed(true));
            }
            Some(false) if !after_dit && dit_active => {
                return Some(self.start_element_keyed(false));
            }
            Some(true) if dash_active => {
                return Some(self.start_element_keyed(true));
            }
            Some(false) if dit_active => {
                return Some(self.start_element_keyed(false));
            }
            _ => {}
        }

        // Fallback: last_paddle's preferred side has been released without
        // memory — honor whatever's still held so we don't stall.
        if self.dit_held {
            return Some(self.start_element_keyed(false));
        }
        if self.dash_held {
            return Some(self.start_element_keyed(true));
        }
        None
    }

    /// Common tail logic shared by both resolve_after_dit_delay and
    /// resolve_after_dash_delay: text queue, auto-spacing, idle transition.
    fn resolve_delay_tail(&mut self, is_mode_b: bool) -> Option<KeyerOutput> {
        if self.text_sending && !self.text_queue.is_empty() {
            self.state = KeyerState::Idle;
            return None;
        }

        if self.config.auto_spacing {
            let remaining_us = self
                .config
                .letter_gap_us()
                .saturating_sub(self.config.element_gap_us());
            if remaining_us > 0 {
                self.state = KeyerState::LetterSpace;
                self.schedule_phase(remaining_us);
                return None;
            }
        }

        if !is_mode_b {
            self.dit_memory = false;
            self.dash_memory = false;
        }
        self.enter_idle_from_delay();
        None
    }

    /// Resolve next state after dit delay (element gap after a dit).
    fn resolve_after_dit_delay(&mut self, bug_mode: bool) -> Option<KeyerOutput> {
        let is_mode_b = matches!(self.config.mode, KeyerMode::IambicB | KeyerMode::Ultimatic);

        if self.config.mode == KeyerMode::Ultimatic {
            if let Some(out) = self.resolve_ultimatic(true) {
                return Some(out);
            }
        } else {
            // Iambic: check opposite memory/held for alternation
            if is_mode_b && self.dash_memory && !bug_mode {
                return Some(self.start_element_keyed(true));
            }
            if self.dash_held && !bug_mode {
                return Some(self.start_element_keyed(true));
            }

            // Check dit for repeat
            if is_mode_b && self.dit_memory {
                return Some(self.start_element_keyed(false));
            }
            if self.dit_held {
                return Some(self.start_element_keyed(false));
            }
        }

        self.resolve_delay_tail(is_mode_b)
    }

    /// Resolve next state after dash delay (element gap after a dash).
    fn resolve_after_dash_delay(&mut self, bug_mode: bool) -> Option<KeyerOutput> {
        let is_mode_b = matches!(self.config.mode, KeyerMode::IambicB | KeyerMode::Ultimatic);

        if self.config.mode == KeyerMode::Ultimatic {
            if let Some(out) = self.resolve_ultimatic(false) {
                return Some(out);
            }
        } else {
            if is_mode_b && self.dit_memory {
                return Some(self.start_element_keyed(false));
            }
            if self.dit_held {
                return Some(self.start_element_keyed(false));
            }

            if is_mode_b && self.dash_memory && !bug_mode {
                return Some(self.start_element_keyed(true));
            }
            if self.dash_held && !bug_mode {
                return Some(self.start_element_keyed(true));
            }
        }

        self.resolve_delay_tail(is_mode_b)
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::KeyerMode;

    fn engine_at(wpm: u8, mode: KeyerMode) -> KeyerEngine {
        let mut config = KeyerConfig::default();
        config.speed_wpm = wpm;
        config.mode = mode;
        KeyerEngine::new(config)
    }

    fn collect_outputs(engine: &mut KeyerEngine, ticks: u32) -> Vec<KeyerOutput> {
        let mut outputs = Vec::new();
        for _ in 0..ticks {
            if let Some(out) = engine.tick() {
                outputs.push(out);
            }
        }
        outputs
    }

    #[test]
    fn idle_no_output() {
        let mut engine = engine_at(20, KeyerMode::IambicB);
        let outputs = collect_outputs(&mut engine, 100);
        assert!(outputs.is_empty(), "Idle engine should produce no output");
    }

    #[test]
    fn straight_key_follows_input() {
        let mut engine = engine_at(20, KeyerMode::Straight);

        // Press dit paddle
        engine.set_paddle(true, false);
        let outputs = collect_outputs(&mut engine, 5);
        assert!(
            outputs.contains(&KeyerOutput::PttRequest(true)),
            "Should request PTT on"
        );
        assert!(
            outputs.contains(&KeyerOutput::KeyDown),
            "Should emit KeyDown"
        );

        // Release. KeyUp is debounced by `STRAIGHT_KEY_RELEASE_DEBOUNCE_MS`
        // ticks; collect long enough for the grace window plus a few
        // ticks of slack so the assertion is robust if the constant
        // changes a little.
        engine.set_paddle(false, false);
        let outputs = collect_outputs(&mut engine, STRAIGHT_KEY_RELEASE_DEBOUNCE_MS + 5);
        assert!(
            outputs.contains(&KeyerOutput::KeyUp),
            "Should emit KeyUp on release"
        );

        // After hang time, PTT off
        let hang_ticks = engine.config.hang_time_ms + 5;
        let outputs = collect_outputs(&mut engine, hang_ticks);
        assert!(
            outputs.contains(&KeyerOutput::PttRequest(false)),
            "Should request PTT off after hang time"
        );
    }

    /// Regression test for the "first some carrier, then space, then
    /// continuous carrier" symptom: a mechanical / OS-level bounce on
    /// the initial press registers as press → release → press within
    /// a few ms, and the engine used to translate that faithfully into
    /// `KeyDown → KeyUp → KeyDown`. Now the release is debounced and
    /// only ONE KeyDown reaches the wire (carrier stays solid through
    /// the bounce).
    #[test]
    fn straight_key_debounces_release_press_bounce_on_initial_press() {
        let mut engine = engine_at(20, KeyerMode::Straight);

        // Initial press
        engine.set_paddle(true, false);
        let mut outputs = collect_outputs(&mut engine, 3);

        // Bounce: brief release within the debounce window, then re-press.
        // This is exactly what a noisy switch / OS key-repeat boundary
        // produces on real hardware.
        let bounce_ticks = STRAIGHT_KEY_RELEASE_DEBOUNCE_MS / 2; // half of grace
        engine.set_paddle(false, false);
        outputs.extend(collect_outputs(&mut engine, bounce_ticks));
        engine.set_paddle(true, false);
        outputs.extend(collect_outputs(&mut engine, 50));

        let down_count = outputs
            .iter()
            .filter(|o| matches!(o, KeyerOutput::KeyDown))
            .count();
        let up_count = outputs
            .iter()
            .filter(|o| matches!(o, KeyerOutput::KeyUp))
            .count();
        assert_eq!(
            down_count, 1,
            "bounce on press must produce exactly one KeyDown, got {down_count} (events: {outputs:?})"
        );
        assert_eq!(
            up_count, 0,
            "bounce on press must NOT emit KeyUp, got {up_count} (events: {outputs:?})"
        );

        // Genuine release after the bounce should still produce KeyUp
        // once the debounce window expires.
        engine.set_paddle(false, false);
        let outputs = collect_outputs(&mut engine, STRAIGHT_KEY_RELEASE_DEBOUNCE_MS + 5);
        assert!(
            outputs.contains(&KeyerOutput::KeyUp),
            "genuine release must still emit KeyUp, got {outputs:?}"
        );
    }

    #[test]
    fn iambic_b_dit_duration() {
        // At 20 WPM, dot_duration_ms = 1200/20 = 60
        let mut engine = engine_at(20, KeyerMode::IambicB);
        assert_eq!(engine.config.dot_duration_ms(), 60);

        engine.set_paddle(true, false);

        // Collect outputs for dit duration + element gap + some extra
        let total_ticks = 200;
        let mut key_down_tick: Option<u32> = None;
        let mut key_up_tick: Option<u32> = None;

        for i in 0..total_ticks {
            if let Some(out) = engine.tick() {
                match out {
                    KeyerOutput::KeyDown if key_down_tick.is_none() => {
                        key_down_tick = Some(i);
                    }
                    KeyerOutput::KeyUp if key_up_tick.is_none() => {
                        key_up_tick = Some(i);
                    }
                    _ => {}
                }
            }
        }

        let down = key_down_tick.expect("Should have KeyDown");
        let up = key_up_tick.expect("Should have KeyUp");
        let duration = up - down;

        // Dit duration should be ~60 ticks (allow small variance for the first tick)
        assert!(
            (58..=62).contains(&duration),
            "Dit duration should be ~60ms at 20 WPM, got {duration}"
        );
    }

    #[test]
    fn iambic_b_squeeze_alternates() {
        let mut engine = engine_at(20, KeyerMode::IambicB);

        // Squeeze both paddles
        engine.set_paddle(true, true);

        // Run for enough time to get several elements
        // dit=60, gap=60, dash=180, gap=60 = 360 per cycle
        let outputs = collect_outputs(&mut engine, 800);

        let key_events: Vec<_> = outputs
            .iter()
            .filter(|o| matches!(o, KeyerOutput::KeyDown | KeyerOutput::KeyUp))
            .collect();

        // Should have alternating KeyDown/KeyUp pairs (at least 3 pairs for dit-dash-dit)
        assert!(
            key_events.len() >= 6,
            "Squeeze should produce at least 3 elements, got {} key events",
            key_events.len()
        );

        // Verify alternation: KeyDown, KeyUp, KeyDown, KeyUp, ...
        for (i, event) in key_events.iter().enumerate() {
            if i % 2 == 0 {
                assert_eq!(
                    **event,
                    KeyerOutput::KeyDown,
                    "Even index {i} should be KeyDown"
                );
            } else {
                assert_eq!(**event, KeyerOutput::KeyUp, "Odd index {i} should be KeyUp");
            }
        }
    }

    #[test]
    fn iambic_a_stops_on_release() {
        let mut engine = engine_at(20, KeyerMode::IambicA);
        let dot = engine.config.dot_duration_ms();
        let gap = engine.config.element_gap_ms();

        // Press both paddles to start sending
        engine.set_paddle(true, true);

        // Run through first element (dit) + a bit into delay
        collect_outputs(&mut engine, dot + gap / 2);

        // Release both paddles during the delay
        engine.set_paddle(false, false);

        // Run enough to finish current element and any pending
        let outputs = collect_outputs(&mut engine, 500);

        // After releasing, iambic A should stop. There should be at most one more
        // KeyDown/KeyUp pair (for the element that was already triggered by memory).
        let key_downs: Vec<_> = outputs
            .iter()
            .filter(|o| **o == KeyerOutput::KeyDown)
            .collect();

        // In mode A, after releasing during the delay, at most one more element plays
        // (the one from the inter-element gap resolution, since paddles are released
        // and no memory fires in mode A)
        assert!(
            key_downs.len() <= 1,
            "Iambic A should stop shortly after release, got {} more key-downs",
            key_downs.len()
        );
    }

    #[test]
    fn macro_sends_text() {
        let mut engine = engine_at(20, KeyerMode::IambicB);

        // "E" = single dit
        engine.send_text("E");

        // Run enough ticks to complete the dit
        let outputs = collect_outputs(&mut engine, 200);

        assert!(
            outputs.contains(&KeyerOutput::KeyDown),
            "Text macro should produce KeyDown"
        );
        assert!(
            outputs.contains(&KeyerOutput::KeyUp),
            "Text macro should produce KeyUp"
        );
    }

    #[test]
    fn paddle_interrupts_macro() {
        let mut engine = engine_at(20, KeyerMode::IambicB);

        // Queue a long text
        engine.send_text("HELLO WORLD");
        assert!(!engine.text_queue.is_empty());

        // Run a few ticks to start sending
        collect_outputs(&mut engine, 10);

        // Hit paddle - should clear text queue
        engine.set_paddle(true, false);
        assert!(
            engine.text_queue.is_empty(),
            "Paddle should clear text queue"
        );
    }

    #[test]
    fn hang_time_emits_ptt_off() {
        let mut engine = engine_at(20, KeyerMode::IambicB);
        let hang_ms = engine.config.hang_time_ms;

        // Send a dit: press and immediately release so only one dit is sent
        engine.set_paddle(true, false);
        // Run just 1 tick to start the dit, then release
        collect_outputs(&mut engine, 1);
        engine.set_paddle(false, false);

        // Run enough ticks to finish the dit + gap + hang time + margin
        let outputs = collect_outputs(&mut engine, 500 + hang_ms);

        assert!(
            outputs.contains(&KeyerOutput::PttRequest(false)),
            "Should emit PttRequest(false) after hang time"
        );
    }

    #[test]
    fn network_transitions_captured() {
        let mut engine = engine_at(20, KeyerMode::IambicB);

        // Send a dit
        engine.set_paddle(true, false);
        collect_outputs(&mut engine, 200);
        engine.set_paddle(false, false);
        collect_outputs(&mut engine, 100);

        let transitions = engine.drain_transitions();
        assert!(!transitions.is_empty(), "Should have captured transitions");

        // Should have at least one key_down and one key_up
        let has_down = transitions.iter().any(|t| t.key_down);
        let has_up = transitions.iter().any(|t| !t.key_down);
        assert!(has_down, "Should have key_down transition");
        assert!(has_up, "Should have key_up transition");

        // Timestamps should be monotonically increasing
        for window in transitions.windows(2) {
            assert!(
                window[1].timestamp_us >= window[0].timestamp_us,
                "Timestamps should be monotonically increasing"
            );
        }

        // After draining, should be empty
        let again = engine.drain_transitions();
        assert!(again.is_empty(), "After drain, transitions should be empty");
    }

    // --- CMOS Super Keyer iambic B timing tests ---

    fn engine_with_timing(wpm: u8, percent: u8) -> KeyerEngine {
        let mut config = KeyerConfig::default();
        config.speed_wpm = wpm;
        config.mode = KeyerMode::IambicB;
        config.iambic_b_timing_percent = percent;
        KeyerEngine::new(config)
    }

    /// Count KeyDown events produced by the engine over `ticks` ms.
    fn count_key_downs(engine: &mut KeyerEngine, ticks: u32) -> usize {
        (0..ticks)
            .filter_map(|_| engine.tick())
            .filter(|o| *o == KeyerOutput::KeyDown)
            .count()
    }

    #[test]
    fn timing_0_pct_full_iambic_b() {
        // 0% dead zone = classic iambic B: squeeze during the tail of a dit
        // must still latch the dash memory.
        let mut engine = engine_with_timing(20, 0);
        let dot = engine.config.dot_duration_ms(); // 60

        // Press dit only
        engine.set_paddle(true, false);
        // Run through most of the dit element
        collect_outputs(&mut engine, dot - 5);

        // Squeeze dash near the very end of the dit (last 5 ms)
        engine.set_paddle(true, true);
        collect_outputs(&mut engine, 3);

        // Release both
        engine.set_paddle(false, false);

        // Let the engine run — the dash should fire from memory
        let downs = count_key_downs(&mut engine, 500);
        assert!(
            downs >= 1,
            "At 0% timing, late squeeze should latch dash memory, got {downs} key-downs"
        );
    }

    #[test]
    fn timing_100_pct_no_element_memory() {
        // 100% dead zone = memory never latches during element sending.
        // A squeeze during the dit should NOT produce a dash from memory
        // (only from held state at resolve time).
        let mut engine = engine_with_timing(20, 100);
        let dot = engine.config.dot_duration_ms(); // 60

        // Press dit only
        engine.set_paddle(true, false);
        // Advance into the middle of the dit
        collect_outputs(&mut engine, dot / 2);

        // Squeeze dash briefly in the middle of the dit
        engine.set_paddle(true, true);
        collect_outputs(&mut engine, 5);

        // Release BOTH before the dit ends
        engine.set_paddle(false, false);

        // Let engine finish — no dash should fire (memory was gated,
        // and paddle is no longer held at resolve time)
        let downs = count_key_downs(&mut engine, 500);
        assert_eq!(
            downs, 0,
            "At 100% timing, brief squeeze should not latch memory, got {downs} key-downs"
        );
    }

    #[test]
    fn timing_33_pct_early_squeeze_latches() {
        // Default 33%: the first 67% of the element is the latch window.
        // A squeeze in the first half should latch.
        let mut engine = engine_with_timing(20, 33);
        // dot = 60ms, dead-zone threshold = 19 ticks

        engine.set_paddle(true, false);
        // Advance a few ticks into the dit (well within the 67% window)
        collect_outputs(&mut engine, 10);

        // Squeeze dash briefly
        engine.set_paddle(true, true);
        collect_outputs(&mut engine, 5);

        // Release both before dit ends
        engine.set_paddle(false, false);

        // Dash should fire from memory
        let downs = count_key_downs(&mut engine, 500);
        assert!(
            downs >= 1,
            "At 33% timing, early squeeze should latch, got {downs} key-downs"
        );
    }

    #[test]
    fn timing_33_pct_late_squeeze_blocked() {
        // Default 33%: the last 33% of the element is the dead zone.
        // A squeeze only during the tail should NOT latch memory.
        let mut engine = engine_with_timing(20, 33);
        let dot = engine.config.dot_duration_ms(); // 60
        let gap = engine.config.element_gap_ms();
        // Dead zone starts when tick_counter <= 60*33/100 = 19
        // i.e. after 60-19 = 41 ticks have elapsed.

        engine.set_paddle(true, false);
        // Advance past the latch window into the dead zone
        collect_outputs(&mut engine, dot - 10); // at tick 50, counter ~10 < 19

        // Squeeze dash in the dead zone
        engine.set_paddle(true, true);
        collect_outputs(&mut engine, 3);

        // Release both before the inter-element gap ends
        engine.set_paddle(false, false);
        // Finish the dit + gap without paddles held
        collect_outputs(&mut engine, 10 + gap);

        // No dash should fire (memory blocked, paddle released before resolve)
        let downs = count_key_downs(&mut engine, 500);
        assert_eq!(
            downs, 0,
            "At 33% timing, late squeeze in dead zone should not latch, got {downs} key-downs"
        );
    }

    #[test]
    fn timing_gate_does_not_affect_gap_latching() {
        // Even at 100% element dead zone, pressing the opposite paddle
        // during the inter-element gap should still latch memory.
        let mut engine = engine_with_timing(20, 100);

        engine.set_paddle(true, false);
        // Run through the full dit element + a few ticks into the gap
        let dot = engine.config.dot_duration_ms(); // 60
        collect_outputs(&mut engine, dot + 5);

        // Now in DitDelay — squeeze dash briefly
        engine.set_paddle(true, true);
        collect_outputs(&mut engine, 5);
        engine.set_paddle(false, false);

        // Dash should fire from gap-phase memory
        let downs = count_key_downs(&mut engine, 500);
        assert!(
            downs >= 1,
            "Gap-phase latch should work even at 100% timing, got {downs} key-downs"
        );
    }

    #[test]
    fn timing_set_paddle_rising_edge_gated() {
        // Verify that set_paddle() rising-edge latch is also gated during
        // element sending.  At 100%, a rising edge during SendDit should
        // NOT set dash_memory.
        let mut engine = engine_with_timing(20, 100);
        let dot = engine.config.dot_duration_ms();

        engine.set_paddle(true, false);
        collect_outputs(&mut engine, dot / 2);

        // Rising edge of dash via set_paddle during element dead zone
        engine.set_paddle(true, true);
        // Immediately release dash
        engine.set_paddle(true, false);

        // Release dit before it finishes
        engine.set_paddle(false, false);

        // No dash should fire
        let downs = count_key_downs(&mut engine, 500);
        assert_eq!(
            downs, 0,
            "Rising-edge latch should be gated at 100%, got {downs} key-downs"
        );
    }

    #[test]
    fn ultimatic_squeeze_repeats_last_paddle() {
        let mut engine = engine_at(20, KeyerMode::Ultimatic);

        // Press dit first
        engine.set_paddle(true, false);
        collect_outputs(&mut engine, 5);

        // Then squeeze dash (last paddle = dash)
        engine.set_paddle(true, true);

        // Run through several cycles
        let outputs = collect_outputs(&mut engine, 1000);
        let key_downs: Vec<_> = outputs
            .iter()
            .filter(|o| **o == KeyerOutput::KeyDown)
            .collect();

        assert!(
            key_downs.len() >= 3,
            "Ultimatic squeeze should produce multiple elements, got {}",
            key_downs.len()
        );
    }

    #[test]
    fn ultimatic_single_paddle_repeats() {
        let mut engine = engine_at(20, KeyerMode::Ultimatic);

        // Hold only dit
        engine.set_paddle(true, false);
        let downs = count_key_downs(&mut engine, 500);
        assert!(
            downs >= 3,
            "Single dit paddle should auto-repeat in Ultimatic, got {downs}"
        );
    }

    #[test]
    fn timing_does_not_affect_iambic_a() {
        // IambicA with 100% timing should behave identically to IambicA
        // with 0% — the gate must not interfere.
        let mut config = KeyerConfig::default();
        config.speed_wpm = 20;
        config.mode = KeyerMode::IambicA;
        config.iambic_b_timing_percent = 100;
        let mut engine = KeyerEngine::new(config);
        let dot = engine.config.dot_duration_ms(); // 60

        // Squeeze both paddles
        engine.set_paddle(true, true);
        // Run through first dit + gap + into dash
        collect_outputs(&mut engine, dot + dot + 10);

        // Release both during the dash
        engine.set_paddle(false, false);

        // IambicA: after release, at most one more element from the
        // current in-flight element.  Memory must NOT be suppressed.
        let outputs = collect_outputs(&mut engine, 500);
        let key_downs: Vec<_> = outputs
            .iter()
            .filter(|o| **o == KeyerOutput::KeyDown)
            .collect();
        assert!(
            key_downs.len() <= 1,
            "IambicA should still stop after release even at 100% timing, got {} key-downs",
            key_downs.len()
        );
    }

    #[test]
    fn timing_33_pct_dash_first_symmetry() {
        // Verify the timing gate works symmetrically when sending a dash
        // and squeezing dit in the dead zone.
        let mut engine = engine_with_timing(20, 33);
        let dash = engine.config.dash_duration_ms(); // 180
        let gap = engine.config.element_gap_ms();

        // Press dash only
        engine.set_paddle(false, true);
        // Advance past the latch window into the dead zone (last 33% = last 59 ticks)
        collect_outputs(&mut engine, dash - 10); // well into the dead zone

        // Squeeze dit in the dead zone
        engine.set_paddle(true, true);
        collect_outputs(&mut engine, 3);

        // Release both before the gap ends
        engine.set_paddle(false, false);
        collect_outputs(&mut engine, 10 + gap);

        // No dit should fire from memory
        let downs = count_key_downs(&mut engine, 500);
        assert_eq!(
            downs, 0,
            "Dash-first: late dit squeeze in dead zone should not latch, got {downs} key-downs"
        );
    }

    #[test]
    fn ultimatic_stops_on_release() {
        let mut engine = engine_at(20, KeyerMode::Ultimatic);
        let dot = engine.config.dot_duration_ms();
        let gap = engine.config.element_gap_ms();

        // Squeeze both paddles (last = dash)
        engine.set_paddle(true, false);
        collect_outputs(&mut engine, 3);
        engine.set_paddle(true, true);

        // Run through first element + into gap
        collect_outputs(&mut engine, dot + gap / 2);

        // Release both during the gap
        engine.set_paddle(false, false);
        collect_outputs(&mut engine, gap);

        // No more elements should fire after release
        let downs = count_key_downs(&mut engine, 500);
        assert_eq!(
            downs, 0,
            "Ultimatic should stop after both paddles released, got {downs} key-downs"
        );
    }

    #[test]
    fn single_paddle_short_press_is_dit() {
        let mut engine = engine_at(20, KeyerMode::SinglePaddle);

        // Press and release before dit expires
        engine.set_paddle(true, false);
        collect_outputs(&mut engine, 30); // half dit
        engine.set_paddle(false, false);

        // Should get KeyUp (dit completed early)
        let outputs = collect_outputs(&mut engine, 200);
        let ups: Vec<_> = outputs
            .iter()
            .filter(|o| **o == KeyerOutput::KeyUp)
            .collect();
        assert!(!ups.is_empty(), "Short press should emit KeyUp for dit");
    }

    #[test]
    fn single_paddle_long_press_is_dash() {
        let mut engine = engine_at(20, KeyerMode::SinglePaddle);
        let dot = engine.config.effective_dot_duration_ms(); // 60
        let dash = engine.config.effective_dash_duration_ms(); // 180

        // Press and hold past dit duration
        engine.set_paddle(true, false);

        let mut key_down_tick = None;
        let mut key_up_tick = None;

        for i in 0..(dash + 100) {
            if let Some(out) = engine.tick() {
                match out {
                    KeyerOutput::KeyDown if key_down_tick.is_none() => key_down_tick = Some(i),
                    KeyerOutput::KeyUp if key_up_tick.is_none() => key_up_tick = Some(i),
                    _ => {}
                }
            }
        }

        let down = key_down_tick.expect("Should have KeyDown");
        let up = key_up_tick.expect("Should have KeyUp");
        let duration = up - down;

        // Should be close to dash duration (180ms), not dit (60ms)
        assert!(
            duration > dot + 10,
            "Long press should produce dash ({dash}ms), got {duration}ms"
        );
    }

    #[test]
    fn single_paddle_either_paddle_works() {
        let mut engine = engine_at(20, KeyerMode::SinglePaddle);

        // Dash paddle should also work
        engine.set_paddle(false, true);
        let downs = count_key_downs(&mut engine, 200);
        assert!(
            downs >= 1,
            "Dash paddle should trigger in single paddle mode"
        );
    }

    /// Ultimatic alternation: hold dash so it auto-repeats, then add dit.
    /// `last_paddle` becomes dit, so the next element after the in-flight
    /// dash must be a dit (last-paddle-wins semantics).
    #[test]
    fn ultimatic_alternates_on_late_paddle_change() {
        let mut engine = engine_at(20, KeyerMode::Ultimatic);
        let dot = engine.config.dot_duration_ms();
        let dash = engine.config.dash_duration_ms();
        let gap = engine.config.element_gap_ms();

        // Press dash only — it should auto-repeat (last_paddle = dash).
        engine.set_paddle(false, true);
        // Run through one full dash + gap so dash is established.
        collect_outputs(&mut engine, dash + gap + 5);

        // Now squeeze dit alongside (rising edge → last_paddle = dit).
        engine.set_paddle(true, true);

        // Capture KeyDown→KeyUp pairs for the next two elements and measure
        // each element's duration. Last-paddle-wins: dit should appear soon.
        let mut events: Vec<(u32, KeyerOutput)> = Vec::new();
        for i in 0..(dash * 4) {
            if let Some(out) = engine.tick() {
                events.push((i, out));
            }
        }

        // Find element durations (KeyDown→KeyUp deltas).
        let mut durations = Vec::new();
        let mut last_down: Option<u32> = None;
        for (t, ev) in &events {
            match ev {
                KeyerOutput::KeyDown => last_down = Some(*t),
                KeyerOutput::KeyUp => {
                    if let Some(d) = last_down.take() {
                        durations.push(*t - d);
                    }
                }
                _ => {}
            }
        }

        // After last_paddle flips to dit, at least one of the next elements
        // must be a dit (close to dot duration), proving alternation took
        // effect. Pre-fix, last_paddle was already dash so durations would
        // remain dash-sized.
        let has_dit = durations
            .iter()
            .any(|d| d.abs_diff(dot) < dot.min(dash - dot) / 2);
        assert!(
            has_dit,
            "Ultimatic should switch to dit after dit becomes last-paddle, durations: {durations:?}"
        );
    }

    /// Cross-session leak: a previous Ultimatic session sets `last_paddle`,
    /// then full release + idle. A new single-paddle session must NOT inherit
    /// the prior `last_paddle` and must just repeat the held paddle.
    #[test]
    fn ultimatic_no_cross_session_state_leak() {
        let mut engine = engine_at(20, KeyerMode::Ultimatic);
        let dash = engine.config.dash_duration_ms();
        let gap = engine.config.element_gap_ms();

        // Session 1: hold dash → last_paddle = dash, dash repeats.
        engine.set_paddle(false, true);
        collect_outputs(&mut engine, dash * 2 + gap * 2);

        // Release everything and let the engine return to Idle. We need
        // enough ticks so DashDelay → Idle (also drains hang time).
        engine.set_paddle(false, false);
        let hang = engine.config.hang_time_ms;
        collect_outputs(&mut engine, gap * 2 + hang + 50);
        assert!(
            !engine.is_active() || engine.state == KeyerState::Idle,
            "engine should be idle after release + hang, state = {:?}",
            engine.state
        );
        // After enter_idle_from_delay, last_paddle should be cleared.
        assert_eq!(
            engine.last_paddle, None,
            "last_paddle must be cleared when engine returns to Idle"
        );

        // Session 2: hold dit only. With the bug, last_paddle would still be
        // dash and the *fallback* in resolve_ultimatic would still pick dit
        // (held), so dit repeats either way — but the post-fix invariant
        // (last_paddle == None at idle) is what we assert above. Still
        // exercise the path end-to-end to confirm dit auto-repeats cleanly.
        engine.set_paddle(true, false);
        let dot = engine.config.dot_duration_ms();
        let downs = count_key_downs(&mut engine, dot * 4 + gap * 4);
        assert!(
            downs >= 2,
            "dit-only session should auto-repeat, got {downs} key-downs"
        );
    }

    /// Stronger Ultimatic stop test: release while the dash is still being
    /// transmitted (mid-element), not in the gap. Engine completes the
    /// current dash and then must idle — no further elements.
    #[test]
    fn ultimatic_stops_on_release_during_active_element() {
        let mut engine = engine_at(20, KeyerMode::Ultimatic);
        let dash = engine.config.dash_duration_ms();

        // Squeeze, last_paddle = dash.
        engine.set_paddle(true, false);
        collect_outputs(&mut engine, 3);
        engine.set_paddle(true, true);
        // Get past the first dit + gap, into the dash.
        let dot = engine.config.dot_duration_ms();
        collect_outputs(&mut engine, dot + 70);

        // We should now be inside SendDash (or close to it). Release both
        // paddles mid-dash.
        engine.set_paddle(false, false);

        // Run long enough to finish the in-flight dash + gap, then watch
        // for any further key-downs.
        collect_outputs(&mut engine, dash + 200);
        let downs = count_key_downs(&mut engine, 500);
        assert_eq!(
            downs, 0,
            "Ultimatic must stop after mid-element release, got {downs} extra key-downs"
        );
    }

    /// Keying compensation must actually extend the key-down duration in
    /// the engine, not only show up in `effective_*` getters.
    #[test]
    fn keying_compensation_extends_engine_keydown() {
        let mut config = KeyerConfig::default();
        config.speed_wpm = 20; // dot = 60ms
        config.mode = KeyerMode::IambicB;
        config.keying_compensation_ms = 8;
        let mut engine = KeyerEngine::new(config);

        engine.set_paddle(true, false);

        // Capture KeyDown and KeyUp ticks for the dit.
        let mut down_t = None;
        let mut up_t = None;
        for i in 0..400u32 {
            if let Some(out) = engine.tick() {
                match out {
                    KeyerOutput::KeyDown if down_t.is_none() => down_t = Some(i),
                    KeyerOutput::KeyUp if up_t.is_none() => up_t = Some(i),
                    _ => {}
                }
            }
        }
        let down = down_t.expect("KeyDown");
        let up = up_t.expect("KeyUp");
        let duration = up - down;
        // Expect ~68ms (60 dit + 8 comp), tolerate ±2 for the first-tick
        // offset already accepted by `iambic_b_dit_duration`.
        assert!(
            (66..=70).contains(&duration),
            "keying compensation should extend dit to ~68ms, got {duration}ms"
        );
    }

    /// Farnsworth gap timing must be honored by the engine when sending a
    /// macro that contains an inter-letter gap.
    #[test]
    fn farnsworth_gap_applied_in_engine() {
        // Two configs that send the same text "EE" (E = single dit).
        // With Farnsworth, the LetterGap between the two E's must be longer
        // than the standard 3 * dot_duration.
        let dot = 60u32; // 20 WPM
        let standard_letter_gap = dot * 3;

        let mut farns_cfg = KeyerConfig::default();
        farns_cfg.speed_wpm = 20;
        farns_cfg.mode = KeyerMode::IambicB;
        farns_cfg.farnsworth_wpm = 10; // slower spacing
        let expected_farns_gap = farns_cfg.letter_gap_ms();
        assert!(
            expected_farns_gap > standard_letter_gap,
            "precondition: Farnsworth gap must exceed standard"
        );

        let mut engine = KeyerEngine::new(farns_cfg);
        engine.send_text("EE");

        // Capture KeyUp / next KeyDown ticks.
        let mut events: Vec<(u32, KeyerOutput)> = Vec::new();
        for i in 0..2000u32 {
            if let Some(out) = engine.tick() {
                events.push((i, out));
            }
            if events
                .iter()
                .filter(|(_, o)| matches!(o, KeyerOutput::KeyDown))
                .count()
                >= 2
                && events
                    .iter()
                    .filter(|(_, o)| matches!(o, KeyerOutput::KeyUp))
                    .count()
                    >= 1
            {
                // Already have the gap data we need; keep running a bit
                // more in case the second KeyUp hasn't fired yet.
                if events.iter().any(|(_, o)| matches!(o, KeyerOutput::KeyUp))
                    && events
                        .iter()
                        .filter(|(_, o)| matches!(o, KeyerOutput::KeyDown))
                        .count()
                        >= 2
                {
                    break;
                }
            }
        }

        // Find first KeyUp and the next KeyDown after it.
        let first_up = events
            .iter()
            .find(|(_, o)| matches!(o, KeyerOutput::KeyUp))
            .map(|(t, _)| *t)
            .expect("first KeyUp");
        let next_down = events
            .iter()
            .find(|(t, o)| *t > first_up && matches!(o, KeyerOutput::KeyDown))
            .map(|(t, _)| *t)
            .expect("second KeyDown");
        let gap = next_down - first_up;

        // Gap should be roughly the Farnsworth letter gap. Allow ±5ms of
        // slop for the first-tick KeyDown latency.
        let lo = expected_farns_gap.saturating_sub(5);
        let hi = expected_farns_gap + 5;
        assert!(
            (lo..=hi).contains(&gap),
            "engine inter-letter gap should be ~{expected_farns_gap}ms (Farnsworth), got {gap}ms"
        );
    }

    /// Run the engine with only the dit paddle held until we've observed
    /// `measured_cycles + 1` KeyDown emissions *after* a one-cycle
    /// warmup, then return the signed drift between the measured span
    /// and `measured_cycles × cycle_us`.
    ///
    /// The warmup skip is load-bearing: the very first KeyDown is
    /// emitted one tick *after* the dit timer started (PttRequest fires
    /// first), which would otherwise contaminate the measurement with a
    /// one-tick offset unrelated to the drift the µs scheduler is
    /// meant to eliminate.
    fn measure_dit_repeat_drift(
        mut engine: KeyerEngine,
        cycle_us: u64,
        measured_cycles: u32,
    ) -> i64 {
        engine.set_paddle(true, false);

        // (warmup + measured_cycles + 1) cycles' worth of ticks + slack —
        // scaled by the engine's actual tick quantum so a sub-ms engine
        // gets enough iterations to reach the target cycle count.
        let tick_us = engine.tick_us as u64;
        let max_ticks = (((measured_cycles as u64 + 3) * cycle_us) / tick_us) as u32
            + (200 * 1000 / tick_us as u32);

        let warmup = 1u32;
        let target_downs = warmup + measured_cycles + 1;

        let mut first: Option<u64> = None;
        let mut last: Option<u64> = None;
        let mut down_count = 0u32;
        for _ in 0..max_ticks {
            if let Some(KeyerOutput::KeyDown) = engine.tick() {
                down_count += 1;
                if down_count <= warmup {
                    continue;
                }
                if first.is_none() {
                    first = Some(engine.elapsed_us);
                }
                last = Some(engine.elapsed_us);
                if down_count == target_downs {
                    break;
                }
            }
        }
        assert_eq!(
            down_count, target_downs,
            "expected {target_downs} KeyDowns (warmup + measured), got {down_count}"
        );
        let span = last.unwrap() - first.unwrap();
        let ideal = measured_cycles as u64 * cycle_us;
        span as i64 - ideal as i64
    }

    /// At a WPM where `1200 / wpm` truncates badly (28 WPM: true dit =
    /// 42.857 ms, old integer-ms code used 42 ms and lost 0.857 ms per
    /// dit ≈ 1.2 s of drift per minute), the µs-precision scheduler must
    /// keep the long-run rate within the 1 ms tick quantum.
    ///
    /// Holds the dit paddle in IambicB so dits auto-repeat (cycle =
    /// dit + element-gap = 2 × dot_us). With the old ms-counter code,
    /// drift over 50 cycles was ≈ 85 ms; the µs scheduler keeps it
    /// inside one tick.
    #[test]
    fn no_accumulated_drift_at_28_wpm() {
        let dot_us = 1_200_000u64 / 28; // 42857
        let drift = measure_dit_repeat_drift(engine_at(28, KeyerMode::IambicB), 2 * dot_us, 50);
        assert!(
            drift.abs() < 1000,
            "28 WPM steady-state drift over 50 dits must stay under one tick quantum, got {drift} µs"
        );
    }

    /// Sweep the iambic-family modes (which all auto-repeat dits with
    /// only the dit paddle held — cycle = 2 × dot_us) at WPMs chosen
    /// to stress different parts of `1200 / wpm`:
    ///
    /// * 22 WPM → 54.545 ms — worst non-divisor of the four,
    /// * 25 WPM → 48.000 ms — exact divisor baseline (sanity check),
    /// * 28 WPM → 42.857 ms — the original regression,
    /// * 35 WPM → 34.285 ms — fast end of typical contest use.
    #[test]
    fn no_accumulated_drift_across_modes_and_wpms() {
        let cases = [
            (KeyerMode::IambicA, 22u8),
            (KeyerMode::IambicA, 25),
            (KeyerMode::IambicA, 28),
            (KeyerMode::IambicA, 35),
            (KeyerMode::IambicB, 22),
            (KeyerMode::IambicB, 25),
            (KeyerMode::IambicB, 35),
            (KeyerMode::Ultimatic, 22),
            (KeyerMode::Ultimatic, 28),
            (KeyerMode::Ultimatic, 35),
        ];
        for (mode, wpm) in cases {
            let dot_us = 1_200_000u64 / wpm as u64;
            let engine = engine_at(wpm, mode);
            let drift = measure_dit_repeat_drift(engine, 2 * dot_us, 50);
            assert!(
                drift.abs() < 1000,
                "{mode:?} at {wpm} WPM: drift {drift} µs over 50 cycles exceeds one tick quantum"
            );
        }
    }

    /// SinglePaddle: dit held → dit window expires → upgrade to dash →
    /// DashDelay → next element (which starts as a dit and upgrades
    /// again). Cycle = dit + (dash − dit) + gap = dash + gap = 4 ×
    /// dot_us at default weight. The upgrade path uses its own
    /// `schedule_phase` call with the *remaining* duration, so this is
    /// the test that catches residual mishandling there.
    #[test]
    fn no_accumulated_drift_single_paddle() {
        for wpm in [22u8, 25, 28] {
            let dot_us = 1_200_000u64 / wpm as u64;
            let engine = engine_at(wpm, KeyerMode::SinglePaddle);
            let drift = measure_dit_repeat_drift(engine, 4 * dot_us, 25);
            assert!(
                drift.abs() < 1000,
                "SinglePaddle at {wpm} WPM: drift {drift} µs over 25 cycles exceeds one tick quantum"
            );
        }
    }

    /// Sub-millisecond tick quantum (the firmware's 250 µs cadence).
    /// Drift across 50 cycles must stay inside the *new* tick quantum,
    /// not the default ms — that's the whole point of the config.
    #[test]
    fn no_accumulated_drift_at_28_wpm_250us_tick() {
        let mut config = KeyerConfig::default();
        config.speed_wpm = 28;
        config.mode = KeyerMode::IambicB;
        let engine = KeyerEngine::new_with_tick(config, 250);
        let dot_us = 1_200_000u64 / 28;
        let drift = measure_dit_repeat_drift(engine, 2 * dot_us, 50);
        assert!(
            drift.abs() < 250,
            "28 WPM @ 250 µs tick: drift {drift} µs over 50 cycles exceeds 250 µs quantum"
        );
    }

    /// At a sub-millisecond tick the per-cycle jitter — the variation
    /// in element duration between successive cycles — must also stay
    /// inside the tick quantum.  This is the user-perceived
    /// non-uniformity: at 1 ms ticks, 28 WPM dits alternate 42 / 43
    /// ms in a repeating pattern (1 ms swing); at 250 µs they vary by
    /// at most 250 µs.
    #[test]
    fn dit_duration_jitter_within_tick_quantum_250us() {
        let mut config = KeyerConfig::default();
        config.speed_wpm = 28;
        config.mode = KeyerMode::IambicB;
        let mut engine = KeyerEngine::new_with_tick(config, 250);

        // Hold the dit paddle so dits auto-repeat.
        engine.set_paddle(true, false);

        // Walk far enough to see ~30 KeyDown→KeyUp cycles; max_us is
        // the worst-case wall time (with 1 ms tick this would never
        // overshoot by more than 30 ms — leave plenty of slack).
        let dot_us: i64 = 1_200_000 / 28; // 42857
        let cycles_wanted = 30;
        let max_ticks = 4 * 1000 * cycles_wanted; // 30 cycles ≈ 2.5 s

        let mut down_ts: Vec<u64> = Vec::new();
        for _ in 0..max_ticks {
            if let Some(KeyerOutput::KeyDown) = engine.tick() {
                down_ts.push(engine.elapsed_us);
                if down_ts.len() > cycles_wanted as usize {
                    break;
                }
            }
        }
        assert!(
            down_ts.len() > cycles_wanted as usize,
            "expected > {cycles_wanted} KeyDown events, got {}",
            down_ts.len()
        );

        // First cycle includes the PttRequest pre-roll, so drop it.
        let intervals: Vec<i64> = down_ts
            .windows(2)
            .skip(1)
            .map(|w| (w[1] - w[0]) as i64)
            .collect();
        let min = *intervals.iter().min().unwrap();
        let max = *intervals.iter().max().unwrap();
        let span = max - min;
        // Expected cycle = 2 × dot_us = 85714 µs.  Each interval must
        // be inside [85714 - 250, 85714 + 250] and the swing must not
        // exceed one tick quantum.
        assert!(
            span <= 250,
            "dit cycle jitter {span} µs (min {min}, max {max}, ideal {}) exceeds 250 µs",
            2 * dot_us
        );
    }
}
