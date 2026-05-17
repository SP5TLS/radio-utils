use core::f64::consts::PI;

/// Sidetone oscillator for CW audio feedback.
///
/// Generates a shaped sine tone mixed into the audio output buffer when the key
/// is down.  Uses a raised-cosine (Hann) envelope with a 5 ms rise/fall time —
/// the same shaping used by the TX modulator — to eliminate key clicks.
pub struct Sidetone {
    /// NCO phase accumulator (radians).
    phase: f64,
    /// NCO phase increment per sample (radians).
    phase_inc: f64,
    /// Audio sample rate in Hz.
    sample_rate: f64,
    /// Tone frequency in Hz.
    frequency: f64,
    /// Output volume (0.0 – 1.0).
    volume: f32,
    /// Current keying state.
    key_down: bool,
    /// Current envelope value (computed from `ramp_pos`).
    envelope: f64,
    /// Ramp position (0.0 = fully off, 1.0 = fully on).
    ramp_pos: f64,
    /// Ramp increment per sample (based on 5 ms rise time).
    ramp_inc: f64,
    /// Configurable delay compensation in samples (unused in audio generation,
    /// stored for external scheduling).
    _delay_samples: i32,
}

impl Sidetone {
    /// Create a new sidetone oscillator.
    ///
    /// * `frequency` — tone frequency in Hz (e.g. 600.0)
    /// * `sample_rate` — audio sample rate in Hz (e.g. 48000.0)
    pub fn new(frequency: f64, sample_rate: f64) -> Self {
        let rise_time_secs = 0.005;
        Self {
            phase: 0.0,
            phase_inc: 2.0 * PI * frequency / sample_rate,
            sample_rate,
            frequency,
            volume: 0.5,
            key_down: false,
            envelope: 0.0,
            ramp_pos: 0.0,
            ramp_inc: 1.0 / (rise_time_secs * sample_rate),
            _delay_samples: 0,
        }
    }

    /// Change the tone frequency.
    pub fn set_frequency(&mut self, freq: f64) {
        self.frequency = freq;
        self.phase_inc = 2.0 * PI * freq / self.sample_rate;
    }

    /// Set the output volume (clamped to 0.0 – 1.0).
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }

    /// Set a delay compensation value in milliseconds.
    pub fn set_delay_ms(&mut self, delay_ms: i32) {
        self._delay_samples = ((delay_ms as f64 / 1000.0) * self.sample_rate) as i32;
    }

    /// Signal key-down (start tone).
    pub fn key_down(&mut self) {
        self.key_down = true;
    }

    /// Signal key-up (stop tone).
    pub fn key_up(&mut self) {
        self.key_down = false;
    }

    /// Mix sidetone into `buf` (additive).
    ///
    /// The raised-cosine envelope `0.5 * (1.0 - cos(PI * ramp_pos))` ramps the
    /// amplitude up/down over 5 ms to avoid clicks.
    pub fn process(&mut self, buf: &mut [f32]) {
        for sample in buf.iter_mut() {
            // Advance ramp toward target
            if self.key_down {
                self.ramp_pos = (self.ramp_pos + self.ramp_inc).min(1.0);
            } else {
                self.ramp_pos = (self.ramp_pos - self.ramp_inc).max(0.0);
            }

            // Raised-cosine (Hann) envelope
            self.envelope = 0.5 * (1.0 - libm::cos(PI * self.ramp_pos));

            // Generate tone and mix
            let tone = libm::sin(self.phase) as f32;
            *sample += tone * self.envelope as f32 * self.volume;

            // Advance NCO
            self.phase += self.phase_inc;
            if self.phase >= 2.0 * PI {
                self.phase -= 2.0 * PI;
            }
        }
    }

    /// Update sample rate, recalculating phase_inc and ramp_inc.
    pub fn set_sample_rate(&mut self, rate: f64) {
        self.sample_rate = rate;
        self.phase_inc = 2.0 * PI * self.frequency / rate;
        let rise_time_secs = 0.005;
        self.ramp_inc = 1.0 / (rise_time_secs * rate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f64 = 48_000.0;
    const FREQ: f64 = 600.0;
    const BLOCK: usize = 1024;

    #[test]
    fn sidetone_silent_when_not_keyed() {
        let mut st = Sidetone::new(FREQ, SR);
        let mut buf = vec![0.0f32; BLOCK];
        st.process(&mut buf);

        let max_abs = buf.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(
            max_abs < 1e-6,
            "Expected silence when not keyed, got max abs {max_abs}"
        );
    }

    #[test]
    fn sidetone_produces_tone_when_keyed() {
        let mut st = Sidetone::new(FREQ, SR);
        st.set_volume(1.0);
        st.key_down();

        let mut buf = vec![0.0f32; BLOCK];
        st.process(&mut buf);

        let rms =
            (buf.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / buf.len() as f64).sqrt();
        assert!(rms > 0.1, "Expected audible tone when keyed, got RMS {rms}");
    }

    #[test]
    fn sidetone_ramps_smoothly() {
        let mut st = Sidetone::new(FREQ, SR);
        st.set_volume(1.0);
        st.key_down();

        // Generate enough samples to cover the ramp (5ms = 240 samples at 48kHz)
        let n = 480;
        let mut buf = vec![0.0f32; n];
        st.process(&mut buf);

        // Compute short-term energy in the first 48 samples vs the last 48 samples
        let first_energy: f64 = buf[..48]
            .iter()
            .map(|s| (*s as f64) * (*s as f64))
            .sum::<f64>()
            / 48.0;
        let last_energy: f64 = buf[n - 48..]
            .iter()
            .map(|s| (*s as f64) * (*s as f64))
            .sum::<f64>()
            / 48.0;

        assert!(
            first_energy < last_energy,
            "Ramp should increase: first_energy={first_energy}, last_energy={last_energy}"
        );
    }
}
