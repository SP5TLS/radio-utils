use std::collections::HashMap;
use std::f64::consts::PI;
use std::net::SocketAddr;
use web_time::{Duration, Instant};

use num_complex::Complex;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Normal};

// Re-export shared types from radio-utils-protocol to avoid duplication.
pub use radio_utils_protocol::{
    p1_code_to_sample_rate as code_to_sample_rate, pack_iq_24bit_into,
    sample_rate_to_p1_code as sample_rate_to_code, HpsdrHw, SAMPLE_RATES_P1,
};

// ---------------------------------------------------------------------------
// HwInfo — shared read-only hardware identity
// ---------------------------------------------------------------------------

pub struct HwInfo {
    pub hw: HpsdrHw,
    pub mac: [u8; 6],
    pub firmware_version: u8,
    pub mercury_versions: [u8; 4],
    pub penny_version: u8,
    pub metis_version: u8,
}

impl HwInfo {
    pub fn new(hw: HpsdrHw, mac: [u8; 6]) -> Self {
        Self {
            hw,
            mac,
            firmware_version: 25,
            mercury_versions: [25, 25, 25, 25],
            penny_version: 25,
            metis_version: 25,
        }
    }

    pub fn mac_string(&self) -> String {
        self.mac
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(":")
    }

    pub fn random_mac() -> [u8; 6] {
        let mut rng = StdRng::from_os_rng();
        let mut mac = [0u8; 6];
        rng.fill(&mut mac);
        mac[0] = (mac[0] | 0x02) & 0xFE; // locally-administered, unicast
        mac
    }
}

// ---------------------------------------------------------------------------
// SignalGenerator
// ---------------------------------------------------------------------------

pub struct SignalGenerator {
    pub sample_rate: u32,
    pub noise_level: f64,
    normal: Normal<f64>,
    rng: StdRng,
}

impl SignalGenerator {
    pub fn new(sample_rate: u32, noise_level: f64) -> Self {
        Self {
            sample_rate,
            noise_level,
            normal: Normal::new(0.0, 1.0).unwrap(),
            rng: StdRng::from_os_rng(),
        }
    }

    /// Add white Gaussian noise to an existing IQ buffer.
    /// Used to inject receiver noise into echo-mode signals.
    /// No-op when noise_level is zero.
    pub fn add_noise_into(&mut self, out: &mut [Complex<f64>]) {
        if self.noise_level <= 0.0 {
            return;
        }
        for s in out.iter_mut() {
            *s += Complex::new(
                self.normal.sample(&mut self.rng) * self.noise_level,
                self.normal.sample(&mut self.rng) * self.noise_level,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// EchoBuffer
// ---------------------------------------------------------------------------

const ECHO_ATTENUATION_DB: f64 = 30.0;

/// Minimum samples between live write head and read head (~21 ms at 48 kHz).
const LIVE_DELAY: usize = 1024;

/// Silence appended to a committed loop recording so each iteration is
/// separated by an audible gap instead of cycling tone-to-tone. Combined
/// with the existing 40 ms head/tail crossfade, the listener hears
/// roughly this much real silence between repeats.
const LOOP_TAIL_SILENCE_SEC: f64 = 0.5;

/// Maximum gap between consecutive PTT cycles that still counts as the same
/// loop-mode operator session. Within this window, inter-cycle silence is
/// preserved as zeros so a slow CW operator's whole message survives in the
/// looped buffer; gaps longer than this reset the recording instead.
const LOOP_SESSION_GAP: Duration = Duration::from_secs(30);

/// Echo operating mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EchoMode {
    /// Record during PTT, commit on PTT-off, loop forever.
    Loop,
    /// TX appears on RX in near real-time, plays once (no loop).
    Live,
}

/// Bounded live echo buffer. Drains oldest samples when capacity exceeded
/// to prevent unbounded memory growth during long TX sessions.
///
/// Concurrent transmitters on the same frequency are tracked individually
/// in `client_offsets`. Each TXer's samples land at its own absolute write
/// offset and are additively mixed with any other client's contribution at
/// the same offset, so two operators keying simultaneously superimpose at the
/// listener like real co-channel signals (QRM) instead of being serialised
/// frame-by-frame.
struct LiveBuffer {
    data: Vec<Complex<f64>>,
    /// Total samples drained from the front (absolute start offset).
    drained: usize,
    epoch: u64,
    max_samples: usize,
    /// Per-client absolute write offset (includes drained count). A client's
    /// next `feed_from` lands at this offset; missing entries mean the client
    /// hasn't called `start_client` yet.
    client_offsets: HashMap<SocketAddr, usize>,
}

impl LiveBuffer {
    fn new(max_samples: usize) -> Self {
        Self {
            data: Vec::new(),
            drained: 0,
            epoch: 0,
            max_samples,
            client_offsets: HashMap::new(),
        }
    }

    fn restart_with_epoch(&mut self, epoch: u64) {
        self.data.clear();
        self.drained = 0;
        self.epoch = epoch;
        self.client_offsets.clear();
    }

    /// Register a transmitter's initial write offset at the current write
    /// head, so its samples start at "now" on the timeline rather than at the
    /// beginning of an existing recording.
    fn start_client(&mut self, client_id: SocketAddr) {
        let abs_tail = self.drained + self.data.len();
        self.client_offsets.insert(client_id, abs_tail);
    }

    fn stop_client(&mut self, client_id: SocketAddr) {
        self.client_offsets.remove(&client_id);
    }

    /// Additively mix `samples` from `client_id` into the buffer at that
    /// client's tracked offset. Grows `data` with zeros when writing past the
    /// current end. If the client has fallen so far behind that its offset
    /// landed in already-drained history (extreme jitter), realigns to the
    /// drain head and drops the late slice — those samples can't be heard
    /// anyway.
    fn feed_from(&mut self, client_id: SocketAddr, samples: &[Complex<f64>]) {
        if samples.is_empty() {
            return;
        }
        let Some(abs_offset) = self.client_offsets.get_mut(&client_id) else {
            return;
        };
        if *abs_offset < self.drained {
            *abs_offset = self.drained;
        }
        let cur_abs = *abs_offset;
        let rel_offset = cur_abs - self.drained;
        let needed = rel_offset + samples.len();
        if self.data.len() < needed {
            self.data.resize(needed, Complex::new(0.0, 0.0));
        }
        for (i, &s) in samples.iter().enumerate() {
            self.data[rel_offset + i] += s;
        }
        *abs_offset = cur_abs + samples.len();

        if self.data.len() > self.max_samples {
            let excess = self.data.len() - self.max_samples;
            self.data.copy_within(excess.., 0);
            self.data.truncate(self.max_samples);
            self.drained += excess;
        }
    }
}

/// Per-client echo playback state. Each client tracks its own playback
/// positions and frequency-shift phase accumulators so that multiple clients
/// can independently read from the shared echo data without interfering.
#[derive(Default)]
pub struct EchoPlaybackState {
    playback_pos: HashMap<u32, usize>,
    shift_phase: HashMap<u32, f64>,
    /// Live mode read position per freq (absolute: includes drained count).
    live_pos: HashMap<u32, usize>,
    live_shift_phase: HashMap<u32, f64>,
    /// Last seen live epoch per freq — reset read cursor when epoch changes.
    live_epoch: HashMap<u32, u64>,
    /// Pre-allocated scratch buffer for multi-frequency echo accumulation.
    scratch: Vec<Complex<f64>>,
}

impl EchoPlaybackState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Per-frequency recording state. Tracks how many clients are actively
/// transmitting on this frequency so concurrent TX is handled correctly.
struct FreqRecorder {
    /// Number of clients actively recording on this frequency.
    active_count: u32,
    /// Sample rate (set by first recorder, used for commit/truncation).
    sample_rate: u32,
    /// Loop mode: accumulation buffer committed on last PTT-off. Persists
    /// across PTT cycles so a slow operator's whole CW message — including
    /// inter-element pauses — ends up in the loop, instead of each PTT
    /// release committing only the most recent fragment.
    recording: Vec<Complex<f64>>,
    /// Wall-clock time of the most recent PTT-off (when active_count
    /// dropped to 0). Used in loop mode to decide whether the next PTT-on
    /// continues an existing session (`elapsed < LOOP_SESSION_GAP` →
    /// gap-fill silence and keep the buffer) or starts a fresh one.
    last_ptt_off: Option<Instant>,
}

pub struct EchoBuffer {
    pub max_duration: f64,
    mode: EchoMode,
    attenuation: f64,
    /// Loop mode: committed recordings per frequency (looped on playback).
    echoes: HashMap<u32, Vec<Complex<f64>>>,
    /// Per-frequency recording state with refcounted active transmitters.
    recorders: HashMap<u32, FreqRecorder>,
    /// Bounded live buffers per TX frequency.
    live: HashMap<u32, LiveBuffer>,
    /// Monotonically increasing epoch counter — survives buffer removal so
    /// clients always detect a new recording session.
    next_epoch: u64,
}

impl EchoBuffer {
    pub fn new(mode: EchoMode) -> Self {
        let attenuation = 10.0_f64.powf(-ECHO_ATTENUATION_DB / 20.0);
        Self {
            max_duration: 10.0,
            mode,
            attenuation,
            echoes: HashMap::new(),
            recorders: HashMap::new(),
            live: HashMap::new(),
            next_epoch: 1,
        }
    }

    pub fn mode(&self) -> EchoMode {
        self.mode
    }

    /// Returns true if there is committed echo data available for playback.
    /// When false, the emulator should fall back to generating a test tone.
    pub fn has_data(&self) -> bool {
        match self.mode {
            EchoMode::Loop => !self.echoes.is_empty(),
            EchoMode::Live => !self.live.is_empty(),
        }
    }

    /// Begin recording on `freq` for `client_id`. Multiple clients can record
    /// on the same frequency simultaneously — in live mode their samples are
    /// additively mixed at a shared real-time write head, so two operators
    /// keying at once superimpose like real co-channel QRM. The first client
    /// on a frequency initialises the buffer; subsequent ones just bump the
    /// refcount and register their per-client write offset.
    pub fn start_recording(&mut self, freq: u32, client_id: SocketAddr, sample_rate: u32) {
        let mode = self.mode;
        let max_duration = self.max_duration;
        let rec = self.recorders.entry(freq).or_insert_with(|| FreqRecorder {
            active_count: 0,
            sample_rate,
            recording: Vec::new(),
            last_ptt_off: None,
        });

        if rec.active_count == 0 {
            rec.sample_rate = sample_rate;

            // Loop mode: if the gap since the last PTT-off is short enough
            // that this is plausibly the same operator continuing a CW
            // message, pad the recording with silence for the elapsed time
            // and keep going. Otherwise (long gap, or first-ever session)
            // start fresh.
            //
            // Live mode resets unconditionally — its real-time playback
            // semantics make cross-cycle accumulation meaningless.
            let resume = mode == EchoMode::Loop
                && rec
                    .last_ptt_off
                    .is_some_and(|t| t.elapsed() < LOOP_SESSION_GAP);
            if resume {
                let elapsed = rec.last_ptt_off.unwrap().elapsed();
                let gap_samples = (elapsed.as_secs_f64() * sample_rate as f64) as usize;
                let max_samples = (sample_rate as f64 * max_duration) as usize;
                let pad_len = gap_samples.min(max_samples.saturating_sub(rec.recording.len()));
                rec.recording
                    .extend(std::iter::repeat_n(Complex::new(0.0, 0.0), pad_len));
                log::info!(
                    "Echo: resuming recording on {} Hz (+{} ms silence, total {:.2}s)",
                    freq,
                    elapsed.as_millis(),
                    rec.recording.len() as f64 / sample_rate as f64,
                );
            } else {
                rec.recording.clear();
            }

            if mode == EchoMode::Live {
                let max_samples = (sample_rate as f64 * self.max_duration) as usize;
                let epoch = self.next_epoch;
                self.next_epoch += 1;
                let live_buf = self
                    .live
                    .entry(freq)
                    .or_insert_with(|| LiveBuffer::new(max_samples));
                live_buf.max_samples = max_samples;
                live_buf.restart_with_epoch(epoch);
            }

            log::info!(
                "Echo: recording started on {} Hz @ {} Hz (mode={:?})",
                freq,
                sample_rate,
                self.mode
            );
        } else {
            log::info!(
                "Echo: additional recorder on {} Hz (count={})",
                freq,
                rec.active_count + 1
            );
        }

        rec.active_count += 1;

        if self.mode == EchoMode::Live {
            if let Some(live_buf) = self.live.get_mut(&freq) {
                live_buf.start_client(client_id);
            }
        }
    }

    /// Feed TX IQ samples into the buffer for `freq`, attributed to
    /// `client_id`. In live mode this performs additive mixing at the
    /// client's tracked write offset, so concurrent transmitters superimpose.
    /// In loop mode the samples are appended verbatim (single-TXer assumption
    /// — concurrent TX in loop mode is not supported by this fix).
    pub fn feed(&mut self, freq: u32, client_id: SocketAddr, samples: &[Complex<f64>]) {
        if samples.is_empty() {
            return;
        }
        let is_active = self
            .recorders
            .get(&freq)
            .is_some_and(|r| r.active_count > 0);
        if !is_active {
            return;
        }

        if self.mode == EchoMode::Live {
            let max_samples = self.recorders.get(&freq).map_or(480_000, |r| {
                (r.sample_rate as f64 * self.max_duration) as usize
            });
            let live_buf = self
                .live
                .entry(freq)
                .or_insert_with(|| LiveBuffer::new(max_samples));
            live_buf.feed_from(client_id, samples);
        } else if let Some(rec) = self.recorders.get_mut(&freq) {
            // Loop mode: append, then keep only the most recent max_samples.
            // The buffer needs to grow across multiple PTT cycles to preserve
            // a slow operator's whole CW message, so we drop the OLDEST
            // samples on overflow rather than the newest — opposite of the
            // pre-multi-user behaviour, but only observable when the
            // recording exceeds max_duration.
            let max_samples = (rec.sample_rate as f64 * self.max_duration) as usize;
            rec.recording.extend_from_slice(samples);
            if rec.recording.len() > max_samples {
                let excess = rec.recording.len() - max_samples;
                rec.recording.drain(..excess);
            }
        }
    }

    /// Stop recording on `freq` for `client_id`. When the last client on a
    /// frequency releases PTT the buffer is finalised (loop: committed to
    /// echoes, live: kept until a new recording starts so the LIVE_DELAY tail
    /// can drain).
    pub fn stop_recording(&mut self, freq: u32, client_id: SocketAddr) {
        let count = match self.recorders.get_mut(&freq) {
            Some(rec) if rec.active_count > 0 => {
                rec.active_count -= 1;
                rec.active_count
            }
            _ => return,
        };

        if self.mode == EchoMode::Live {
            if let Some(live_buf) = self.live.get_mut(&freq) {
                live_buf.stop_client(client_id);
            }
        }

        if count == 0 {
            // Stamp the moment the freq became silent — the next PTT-on
            // (loop mode) compares against this to decide between resuming
            // the same operator session and starting fresh. Set BEFORE
            // commit_freq so the recording it publishes already reflects
            // the current cycle's contents.
            if let Some(rec) = self.recorders.get_mut(&freq) {
                rec.last_ptt_off = Some(Instant::now());
            }

            // Last recorder on this frequency.
            if self.mode == EchoMode::Loop {
                self.commit_freq(freq);
            } else {
                // Live mode: keep the buffer so the remaining LIVE_DELAY tail
                // can still be read by active playback clients. The buffer will
                // be overwritten when a new recording starts on this frequency.
            }
            log::info!(
                "Echo: recording stopped on {} Hz (mode={:?})",
                freq,
                self.mode
            );
        } else {
            log::info!("Echo: recorder left {} Hz ({} remain)", freq, count);
        }
    }

    /// Returns true if at least one client is actively recording on `freq`.
    fn is_recording_on(&self, freq: u32) -> bool {
        self.recorders
            .get(&freq)
            .is_some_and(|r| r.active_count > 0)
    }

    fn commit_freq(&mut self, freq: u32) {
        let Some(rec) = self.recorders.get_mut(&freq) else {
            return;
        };
        if rec.recording.is_empty() {
            return;
        }
        if freq == 0 {
            log::debug!("Echo: discarding recording with freq=0");
            rec.recording.clear();
            return;
        }
        let sample_rate = rec.sample_rate;
        let max_samples = (sample_rate as f64 * self.max_duration) as usize;
        // Clone (not take) so the in-progress recording survives this commit
        // and can grow further on the next PTT cycle. That's what lets a
        // slow operator's whole CW message accumulate into the loop across
        // multiple PTT releases instead of being overwritten by the most
        // recent fragment.
        let mut buf = rec.recording.clone();
        buf.truncate(max_samples);
        if buf.is_empty() {
            return;
        }

        // Trim LEADING and TRAILING all-zero samples. Internal zero runs are
        // preserved (so CW inter-element spacing survives the loop), but
        // zeros at the edges get cut. Two classes of edge zeros exist in
        // practice:
        //   1) the native Protocol1Client sends zero-filled subframes when
        //      its TX IQ ringbuf underruns while PTT is still on — this
        //      appears as a long zero tail before the user releases PTT;
        //   2) the operator pressing PTT a moment before (or lifting it a
        //      moment after) the first/last keyed element — same shape at
        //      the boundaries of a real transmission.
        // Either way, trimming them prevents the loop from playing back
        // several seconds of silence between every repeat of the actual
        // signal.
        let first_nz = buf.iter().position(|s| s.re != 0.0 || s.im != 0.0);
        let last_nz = buf.iter().rposition(|s| s.re != 0.0 || s.im != 0.0);
        match (first_nz, last_nz) {
            (Some(f), Some(l)) => {
                if f > 0 || l + 1 < buf.len() {
                    buf = buf[f..=l].to_vec();
                }
            }
            _ => {
                // Recording was entirely silence — nothing to loop.
                log::debug!("Echo: recording on {} Hz was all-zero, discarding", freq);
                return;
            }
        }
        if buf.is_empty() {
            return;
        }

        // Append a fixed silence tail so each loop iteration ends in audible
        // dead air instead of cycling tone-to-tone. Combined with the
        // crossfade below, the listener hears `LOOP_TAIL_SILENCE_SEC` minus
        // the 40 ms blend region between repetitions.
        let tail_pad = (sample_rate as f64 * LOOP_TAIL_SILENCE_SEC).round() as usize;
        let pad_room = max_samples.saturating_sub(buf.len());
        let pad = tail_pad.min(pad_room);
        if pad > 0 {
            buf.extend(std::iter::repeat_n(Complex::new(0.0, 0.0), pad));
        }

        // Cross-fade the tail into the head so the loop splice is seamless.
        // This preserves signal amplitude across the wrap point instead of
        // fading to zero (which creates periodic amplitude dips and phase
        // noise in the looped output).
        //
        // Fade length = 40 ms, capped at 25% of buffer to avoid overlap on
        // very short transmissions.
        let fade_samples = ((sample_rate as f64 * 0.040) as usize).min(buf.len() / 4);
        if fade_samples > 1 {
            let n = fade_samples as f64;
            let buf_len = buf.len();
            for i in 0..fade_samples {
                // Half-Hann rise: 0 at i=0, approaches 1 at i=fade_samples
                let w = 0.5 * (1.0 - (PI * i as f64 / n).cos());
                let tail_idx = buf_len - fade_samples + i;
                // Blend: head fades in, tail fades out
                buf[i] = buf[i] * w + buf[tail_idx] * (1.0 - w);
            }
            // Trim the tail that was blended into the head
            buf.truncate(buf_len - fade_samples);
        }

        let len = buf.len();
        log::info!(
            "Echo: committed {} samples ({:.2}s) on {} Hz",
            len,
            len as f64 / sample_rate as f64,
            freq,
        );
        self.echoes.insert(freq, buf);
    }

    /// Generate looping echo playback directly into `out`.
    /// Uses per-client playback state and a complex oscillator for frequency
    /// shifting. No intermediate heap allocations.
    pub fn generate_echo_into(
        &self,
        ps: &mut EchoPlaybackState,
        out: &mut [Complex<f64>],
        rx_freq: u32,
        sample_rate: u32,
    ) {
        let n_samples = out.len();
        for s in out.iter_mut() {
            *s = Complex::new(0.0, 0.0);
        }

        if self.echoes.is_empty() {
            return;
        }

        // Take scratch buffer out to avoid borrow conflicts with other ps fields.
        let mut scratch = std::mem::take(&mut ps.scratch);
        if scratch.len() < n_samples {
            scratch.resize(n_samples, Complex::new(0.0, 0.0));
        }

        let half_bw = sample_rate as f64 / 2.0;

        for (&freq, echo_buf) in &self.echoes {
            let offset_hz = rx_freq as f64 - freq as f64;
            if offset_hz.abs() > half_bw {
                continue;
            }

            let echo_len = echo_buf.len();
            let mut pos = *ps.playback_pos.get(&freq).unwrap_or(&0) % echo_len;

            // Copy echo data with wrapping into scratch buffer.
            let mut remaining = n_samples;
            let mut write_pos = 0;
            while remaining > 0 {
                let available = remaining.min(echo_len - pos);
                scratch[write_pos..write_pos + available]
                    .copy_from_slice(&echo_buf[pos..pos + available]);
                pos = (pos + available) % echo_len;
                write_pos += available;
                remaining -= available;
            }
            ps.playback_pos.insert(freq, pos);

            // Frequency-shift using complex oscillator (incremental rotation).
            if offset_hz != 0.0 {
                let sr = sample_rate as f64;
                let phase0 = *ps.shift_phase.get(&freq).unwrap_or(&0.0);
                let step = 2.0 * PI * offset_hz / sr;
                let phasor = Complex::new(step.cos(), step.sin());
                let mut osc = Complex::new(phase0.cos(), phase0.sin());
                for s in scratch[..n_samples].iter_mut() {
                    *s *= osc;
                    osc *= phasor;
                }
                let new_phase = (phase0 + step * n_samples as f64).rem_euclid(2.0 * PI);
                ps.shift_phase.insert(freq, new_phase);
            }

            // Accumulate into output.
            for (o, s) in out.iter_mut().zip(scratch[..n_samples].iter()) {
                *o += *s;
            }
        }

        ps.scratch = scratch;

        for s in out.iter_mut() {
            *s *= self.attenuation;
        }
    }

    /// Generate live echo playback directly into `out`. Reads linearly from
    /// the bounded live buffer with a short delay. Returns silence when the
    /// read head catches up to the write head minus LIVE_DELAY.
    pub fn generate_live_echo_into(
        &self,
        ps: &mut EchoPlaybackState,
        out: &mut [Complex<f64>],
        rx_freq: u32,
        sample_rate: u32,
    ) {
        let n_samples = out.len();
        for s in out.iter_mut() {
            *s = Complex::new(0.0, 0.0);
        }

        if self.live.is_empty() {
            return;
        }

        let half_bw = sample_rate as f64 / 2.0;

        let mut scratch = std::mem::take(&mut ps.scratch);
        if scratch.len() < n_samples {
            scratch.resize(n_samples, Complex::new(0.0, 0.0));
        }

        for (&freq, live_buf) in &self.live {
            let offset_hz = rx_freq as f64 - freq as f64;
            if offset_hz.abs() > half_bw {
                continue;
            }

            let data = &live_buf.data;
            let drained = live_buf.drained;
            let write_head = drained + data.len();

            // Reset read cursor if the buffer was restarted (epoch changed)
            // or if this is the first time we see this frequency. Position
            // near the write head so all clients share the same timeline.
            let current_epoch = live_buf.epoch;
            let client_epoch = ps.live_epoch.entry(freq).or_insert(0);
            if *client_epoch != current_epoch {
                *client_epoch = current_epoch;
                ps.live_pos
                    .insert(freq, write_head.saturating_sub(LIVE_DELAY));
                ps.live_shift_phase.insert(freq, 0.0);
            }

            // Absolute read position; catch up if fallen behind drain point.
            let default_pos = write_head.saturating_sub(LIVE_DELAY);
            let mut abs_pos = (*ps.live_pos.get(&freq).unwrap_or(&default_pos)).max(drained);
            let idx = abs_pos - drained;

            // Readable end: leave LIVE_DELAY gap while actively recording this freq.
            let readable_end = if self.is_recording_on(freq) {
                data.len().saturating_sub(LIVE_DELAY)
            } else {
                data.len()
            };

            // Clear scratch and copy available data.
            for s in scratch[..n_samples].iter_mut() {
                *s = Complex::new(0.0, 0.0);
            }
            if idx < readable_end {
                let n = n_samples.min(readable_end - idx);
                scratch[..n].copy_from_slice(&data[idx..idx + n]);
                abs_pos += n;
            }
            ps.live_pos.insert(freq, abs_pos);

            // Frequency-shift using complex oscillator.
            if offset_hz != 0.0 {
                let sr = sample_rate as f64;
                let phase0 = *ps.live_shift_phase.get(&freq).unwrap_or(&0.0);
                let step = 2.0 * PI * offset_hz / sr;
                let phasor = Complex::new(step.cos(), step.sin());
                let mut osc = Complex::new(phase0.cos(), phase0.sin());
                for s in scratch[..n_samples].iter_mut() {
                    *s *= osc;
                    osc *= phasor;
                }
                let new_phase = (phase0 + step * n_samples as f64).rem_euclid(2.0 * PI);
                ps.live_shift_phase.insert(freq, new_phase);
            }

            // Accumulate into output.
            for (o, s) in out.iter_mut().zip(scratch[..n_samples].iter()) {
                *o += *s;
            }
        }

        ps.scratch = scratch;

        for s in out.iter_mut() {
            *s *= self.attenuation;
        }
    }
}

// ---------------------------------------------------------------------------
// IQ packing / unpacking
// ---------------------------------------------------------------------------

/// Unpack Protocol 1 TX IQ from sub-frame data into a pre-allocated buffer.
/// Each 8-byte block: [L(2B) R(2B) I(2B) Q(2B)], big-endian signed.
/// Returns the number of samples written.
pub fn unpack_tx_iq_16bit_into(data: &[u8], out: &mut [Complex<f64>]) -> usize {
    let n_blocks = (data.len() / 8).min(out.len());
    #[allow(clippy::needless_range_loop)]
    for k in 0..n_blocks {
        let off = k * 8;
        let i_val = i16::from_be_bytes([data[off + 4], data[off + 5]]);
        let q_val = i16::from_be_bytes([data[off + 6], data[off + 7]]);
        out[k] = Complex::new(i_val as f64 / 32768.0, q_val as f64 / 32768.0);
    }
    n_blocks
}

#[cfg(test)]
#[allow(clippy::needless_range_loop, clippy::manual_div_ceil)]
mod tests {
    use super::*;

    /// Direct integration test of LiveBuffer + generate_live_echo_into:
    /// feed a known continuous-phase complex tone in 63-sample TX subframes,
    /// alternating with 63-sample RX subframe reads (mirroring the emulator's
    /// real per-subframe pacing), then check that the recovered RX is a clean
    /// tone. If this fails the bug is in the EchoBuffer logic itself, not in
    /// the protocol/timing.
    #[test]
    fn live_echo_passes_continuous_tone_unchanged() {
        let sr: u32 = 48_000;
        let freq: u32 = 7_074_000;
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let tone_hz = 1000.0;
        let amp = 0.5_f64;
        let n_subframes_total = 1500; // ~1.97s of audio
        let sub_n = 63usize;

        let mut echo = EchoBuffer::new(EchoMode::Live);
        echo.start_recording(freq, addr, sr);

        let mut ps = EchoPlaybackState::new();
        let mut rx_out: Vec<Complex<f64>> = Vec::new();
        let mut tx_buf = vec![Complex::new(0.0, 0.0); sub_n];
        let mut rx_buf = vec![Complex::new(0.0, 0.0); sub_n];

        let mut tx_off = 0usize;
        // Each loop iter mimics one Protocol-1 packet: 2 TX subframes feed +
        // 2 RX subframes read, with an arbitrary feed-then-read ordering that
        // matches process_host_frame() / build_data_packet() in protocol1.rs.
        for _ in 0..n_subframes_total / 2 {
            // Two TX subframes (one packet's worth) then two RX subframes.
            for _ in 0..2 {
                for k in 0..sub_n {
                    let phi = 2.0 * PI * tone_hz * (tx_off + k) as f64 / sr as f64;
                    tx_buf[k] = Complex::new(amp * phi.cos(), amp * phi.sin());
                }
                tx_off += sub_n;
                echo.feed(freq, addr, &tx_buf);
            }
            for _ in 0..2 {
                echo.generate_live_echo_into(&mut ps, &mut rx_buf, freq, sr);
                rx_out.extend_from_slice(&rx_buf);
            }
        }

        echo.stop_recording(freq, addr);

        // Skip well past the warm-up region. Each subframe-read produces a
        // 63-sample chunk of `out`; until the live buffer has accumulated
        // LIVE_DELAY samples, those chunks are zero, and the *first*
        // chunk that does deliver real data lands a partial fill (real
        // prefix + zero tail), so the listener time→input time mapping
        // shifts by the partial-fill remainder. Skipping to the first
        // multiple of 63 past 2 × LIVE_DELAY puts the analysis window
        // safely inside the contiguous-mapping region.
        let warmup_end = 2 * LIVE_DELAY;
        let head = ((warmup_end + 62) / 63) * 63;
        let active = &rx_out[head..];
        assert!(
            active.len() >= 8192,
            "active buf too short: {}",
            active.len()
        );
        let buf = &active[..8192];

        // Goertzel power at fundamental and Parseval-style total power.
        let total_power: f64 = buf.iter().map(|s| s.norm_sqr()).sum::<f64>() / buf.len() as f64;
        let fund_mag: f64 = goertzel_mag(buf, tone_hz, sr as f64);
        let fund_power = fund_mag * fund_mag;
        let nad_power = (total_power - fund_power).max(1e-30);
        let sinad_db = 10.0 * (fund_power / nad_power).log10();

        eprintln!(
            "[unit] live-echo: total_power={:.3e} fund_power={:.3e} sinad={:.1} dB \
             fund_mag={:.3e}",
            total_power, fund_power, sinad_db, fund_mag
        );

        // 30 dB attenuation in EchoBuffer brings 0.5 amp → 0.0158 amp →
        // 2.5e-4 power for a clean tone. We accept anything within a factor
        // of 2 of that, plus require SINAD > 30 dB (loop mode achieves 40+).
        assert!(
            sinad_db > 30.0,
            "live-echo unit test SINAD too low: {:.1} dB",
            sinad_db
        );
    }

    /// Tiny Goertzel-style magnitude helper for the unit test.
    fn goertzel_mag(buf: &[Complex<f64>], freq_hz: f64, sample_rate: f64) -> f64 {
        let omega = 2.0 * PI * freq_hz / sample_rate;
        let mut acc = Complex::new(0.0, 0.0);
        for (k, s) in buf.iter().enumerate() {
            let phi = -omega * k as f64;
            acc += *s * Complex::new(phi.cos(), phi.sin());
        }
        acc.norm() / buf.len() as f64
    }

    /// Same as the previous test, but with the host-side voice-TX burst
    /// pattern: feed BURST_FEEDS subframes per RX iteration instead of 2.
    /// This mimics what Protocol1Client does in voice TX mode (high-water
    /// burst until tx_fifo_estimate stabilises). If the tone smears here,
    /// the bug is sensitive to "many writes per read" interleaving.
    #[test]
    fn live_echo_handles_bursty_writes() {
        let sr: u32 = 48_000;
        let freq: u32 = 7_074_000;
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let tone_hz = 1000.0;
        let amp = 0.5_f64;
        let sub_n = 63usize;
        // Feed 8 subframes per RX iteration of 2 reads, simulating a burst.
        const BURST_FEEDS: usize = 8;
        let n_iters = 200;

        let mut echo = EchoBuffer::new(EchoMode::Live);
        echo.start_recording(freq, addr, sr);

        let mut ps = EchoPlaybackState::new();
        let mut rx_out: Vec<Complex<f64>> = Vec::new();
        let mut tx_buf = vec![Complex::new(0.0, 0.0); sub_n];
        let mut rx_buf = vec![Complex::new(0.0, 0.0); sub_n];

        let mut tx_off = 0usize;
        for _ in 0..n_iters {
            for _ in 0..BURST_FEEDS {
                for k in 0..sub_n {
                    let phi = 2.0 * PI * tone_hz * (tx_off + k) as f64 / sr as f64;
                    tx_buf[k] = Complex::new(amp * phi.cos(), amp * phi.sin());
                }
                tx_off += sub_n;
                echo.feed(freq, addr, &tx_buf);
            }
            for _ in 0..2 {
                echo.generate_live_echo_into(&mut ps, &mut rx_buf, freq, sr);
                rx_out.extend_from_slice(&rx_buf);
            }
        }

        echo.stop_recording(freq, addr);

        let head = 2048usize.min(rx_out.len() / 4);
        let active = &rx_out[head..];
        let analysis_len = 8192.min(active.len());
        let buf = &active[..analysis_len];

        let total_power: f64 = buf.iter().map(|s| s.norm_sqr()).sum::<f64>() / buf.len() as f64;
        let fund_mag: f64 = goertzel_mag(buf, tone_hz, sr as f64);
        let fund_power = fund_mag * fund_mag;
        let nad_power = (total_power - fund_power).max(1e-30);
        let sinad_db = 10.0 * (fund_power / nad_power).log10();

        eprintln!(
            "[unit-burst] feeds_per_iter={} reads_per_iter=2 \
             total_power={:.3e} fund_power={:.3e} sinad={:.1} dB",
            BURST_FEEDS, total_power, fund_power, sinad_db
        );

        // With the read trailing the write head by LIVE_DELAY, bursting
        // writes shouldn't change the spectrum: the read still consumes
        // contiguous samples.
        assert!(
            sinad_db > 30.0,
            "live-echo bursty SINAD too low: {:.1} dB (writes:reads = {}:2)",
            sinad_db,
            BURST_FEEDS
        );
    }
}
