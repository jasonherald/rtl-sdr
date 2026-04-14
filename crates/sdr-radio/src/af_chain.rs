//! AF (Audio Frequency) processing chain.
//!
//! Applies optional deemphasis filtering and sample rate conversion
//! to stereo audio samples after demodulation.

use sdr_dsp::filter::{DeemphasisFilter, NotchFilter};
use sdr_dsp::multirate::RationalResampler;
use sdr_dsp::tone_detect::{CtcssDetector, ctcss_tone_index};
use sdr_types::{Complex, DspError, Stereo};

/// Default audio output sample rate (Hz).
const DEFAULT_AUDIO_RATE: f64 = 48_000.0;

/// Default high-pass cutoff frequency (Hz) for voice modes.
const HIGH_PASS_CUTOFF_HZ: f64 = 300.0;

/// CTCSS sub-audible tone squelch mode.
///
/// `Off` is the default — audio passes through unchanged and the
/// detector is not constructed. `Tone(hz)` must name one of the
/// frequencies in [`sdr_dsp::tone_detect::CTCSS_TONES_HZ`]; any
/// other value is rejected by [`AfChain::set_ctcss_mode`].
///
/// When in `Tone` mode, the AF chain:
///
/// 1. Runs the [`CtcssDetector`] on the post-resample, post-deemph,
///    **pre-high-pass** mono signal so the detector sees the full
///    67–254 Hz sub-audible band.
/// 2. Force-enables the 300 Hz high-pass filter on the speaker path
///    so the user doesn't hear the tone as a low buzz, regardless
///    of the user's explicit high-pass preference.
/// 3. Zeros the output block whenever the detector reports
///    `sustained == false` (no tone confirmed yet, or tone has
///    dropped out for at least `CTCSS_MIN_HITS` consecutive
///    windows).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CtcssMode {
    /// No CTCSS gating. Audio passes through unchanged.
    Off,
    /// Gate the speaker path on the given CTCSS tone frequency.
    Tone(f32),
}

/// Guard padding for resampler output buffer to handle worst-case rounding.
const RESAMPLER_OUTPUT_PADDING: usize = 16;

/// Tolerance in Hz for considering two sample rates equal (skip resampling).
const RATE_EQUALITY_TOLERANCE: f64 = 1.0;

/// AF processing chain — applied to stereo audio after demodulation.
///
/// Contains optional processors:
/// 1. Deemphasis filter — single-pole IIR lowpass for FM deemphasis (L and R)
/// 2. Rational resampler — converts from demod AF rate to audio output rate
///
/// The resampler operates on Complex samples (Stereo -> Complex -> resample -> Stereo)
/// since `RationalResampler` is defined for Complex data.
/// Single-pole IIR high-pass filter for removing low-frequency hum/rumble.
///
/// Uses the Julius O. Smith textbook topology:
/// `y[n] = x[n] - x[n-1] + R * y[n-1]`
/// where `R = 1 - (2π × f_cutoff / sample_rate)`.
/// Has an explicit zero at DC for perfect DC rejection.
struct HighPassFilter {
    r: f32,
    last_in: f32,
    last_out: f32,
}

impl HighPassFilter {
    fn new(cutoff_hz: f64, sample_rate: f64) -> Self {
        #[allow(clippy::cast_possible_truncation)]
        let r = (1.0 - (core::f64::consts::TAU * cutoff_hz / sample_rate)) as f32;
        Self {
            r,
            last_in: 0.0,
            last_out: 0.0,
        }
    }

    #[inline]
    fn process_sample(&mut self, x: f32) -> f32 {
        let y = x - self.last_in + self.r * self.last_out;
        self.last_in = x;
        self.last_out = y;
        y
    }
}

pub struct AfChain {
    deemp_l: Option<DeemphasisFilter>,
    deemp_r: Option<DeemphasisFilter>,
    deemp_enabled: bool,
    hp_l: Option<HighPassFilter>,
    hp_r: Option<HighPassFilter>,
    /// User's explicit high-pass preference, independent of CTCSS.
    /// The effective filter state is `hp_user_enabled || ctcss is
    /// Tone(_)`; CTCSS force-enables the HPF to strip the sub-
    /// audible tone from the speaker path regardless of the user's
    /// setting. Restored to the exact user value when CTCSS returns
    /// to `Off`.
    hp_user_enabled: bool,
    notch_l: NotchFilter,
    notch_r: NotchFilter,
    resampler: Option<RationalResampler>,
    af_sample_rate: f64,
    audio_sample_rate: f64,
    /// Active CTCSS gating mode. `Off` means the detector is not
    /// constructed and audio passes through; `Tone(hz)` means the
    /// detector runs on every block and the output is muted until
    /// the sustained gate opens.
    ctcss_mode: CtcssMode,
    /// CTCSS detector instance, constructed lazily from
    /// [`Self::set_ctcss_mode`] when the mode flips to `Tone(_)`.
    /// Dropped when the mode flips back to `Off`. The detector
    /// owns its own per-window hysteresis state so swapping it out
    /// is the right way to reset between tones.
    ctcss_detector: Option<CtcssDetector>,
    /// Mono downmix scratch buffer fed to the detector. Reused
    /// across calls to avoid per-block allocation on the hot path.
    ctcss_mono_buf: Vec<f32>,
    /// Scratch buffer for deemphasis L input channel.
    deemp_buf_l: Vec<f32>,
    /// Scratch buffer for deemphasis R input channel.
    deemp_buf_r: Vec<f32>,
    /// Scratch buffer for deemphasis L output.
    deemp_out_l: Vec<f32>,
    /// Scratch buffer for deemphasis R output.
    deemp_out_r: Vec<f32>,
    /// Scratch buffer for complex resampler input.
    resamp_in: Vec<Complex>,
    /// Scratch buffer for complex resampler output.
    resamp_out: Vec<Complex>,
    /// Scratch buffers for notch filter (L/R split processing).
    notch_buf_l: Vec<f32>,
    notch_buf_r: Vec<f32>,
    notch_out_l: Vec<f32>,
    notch_out_r: Vec<f32>,
}

impl AfChain {
    /// Create a new AF chain.
    ///
    /// - `af_sample_rate`: sample rate from the demodulator (Hz)
    /// - `audio_sample_rate`: target audio output rate (Hz), typically 48 kHz
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the resampler cannot be created.
    pub fn new(af_sample_rate: f64, audio_sample_rate: f64) -> Result<Self, DspError> {
        let needs_resample = (af_sample_rate - audio_sample_rate).abs() >= RATE_EQUALITY_TOLERANCE;
        let resampler = if needs_resample {
            Some(RationalResampler::new(af_sample_rate, audio_sample_rate)?)
        } else {
            None
        };

        #[allow(clippy::cast_possible_truncation)]
        let audio_rate_f32 = audio_sample_rate as f32;

        Ok(Self {
            deemp_l: None,
            deemp_r: None,
            deemp_enabled: false,
            hp_l: None,
            hp_r: None,
            hp_user_enabled: false,
            notch_l: NotchFilter::new(audio_rate_f32),
            notch_r: NotchFilter::new(audio_rate_f32),
            resampler,
            af_sample_rate,
            audio_sample_rate,
            ctcss_mode: CtcssMode::Off,
            ctcss_detector: None,
            ctcss_mono_buf: Vec::new(),
            deemp_buf_l: Vec::new(),
            deemp_out_l: Vec::new(),
            deemp_out_r: Vec::new(),
            deemp_buf_r: Vec::new(),
            resamp_in: Vec::new(),
            resamp_out: Vec::new(),
            notch_buf_l: Vec::new(),
            notch_buf_r: Vec::new(),
            notch_out_l: Vec::new(),
            notch_out_r: Vec::new(),
        })
    }

    /// Create a new AF chain with the default audio output rate (48 kHz).
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the resampler cannot be created.
    pub fn with_default_rate(af_sample_rate: f64) -> Result<Self, DspError> {
        Self::new(af_sample_rate, DEFAULT_AUDIO_RATE)
    }

    /// Enable deemphasis filtering with the given time constant.
    ///
    /// - `tau`: time constant in seconds (e.g., 75e-6 for US, 50e-6 for EU)
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the filter cannot be created.
    pub fn set_deemp_enabled(&mut self, enabled: bool, tau: f64) -> Result<(), DspError> {
        self.deemp_enabled = enabled;
        if enabled && tau > 0.0 {
            // Deemphasis runs AFTER resampling, so use the audio output rate.
            // C++ SDR++ applies deemphasis at 48 kHz, not the demod AF rate.
            self.deemp_l = Some(DeemphasisFilter::new(tau, self.audio_sample_rate)?);
            self.deemp_r = Some(DeemphasisFilter::new(tau, self.audio_sample_rate)?);
        } else {
            self.deemp_l = None;
            self.deemp_r = None;
        }
        Ok(())
    }

    /// Set the user's high-pass filter preference.
    ///
    /// Removes low-frequency hum and rumble below 300 Hz. The
    /// effective filter state is `user_preference || CTCSS active`
    /// — the CTCSS squelch force-engages the HPF to strip its
    /// sub-audible tone from the speaker path regardless of this
    /// setting, so toggling it off while CTCSS is active only
    /// records the preference for when CTCSS returns to `Off`.
    pub fn set_high_pass_enabled(&mut self, enabled: bool) {
        self.hp_user_enabled = enabled;
        self.apply_effective_high_pass();
    }

    /// Returns the user's high-pass preference. See also
    /// [`Self::effective_high_pass_enabled`] for the state that
    /// actually gates the signal, which can differ when CTCSS is
    /// active.
    pub fn high_pass_enabled(&self) -> bool {
        self.hp_user_enabled
    }

    /// Returns the effective high-pass state — `true` if either
    /// the user has explicitly enabled it OR CTCSS squelch is
    /// active (and therefore force-enables the HPF to strip its
    /// sub-audible tone from the speaker path).
    pub fn effective_high_pass_enabled(&self) -> bool {
        self.hp_user_enabled || matches!(self.ctcss_mode, CtcssMode::Tone(_))
    }

    /// Allocate or drop the HPF biquad state to match the current
    /// effective enabled state. Called from `set_high_pass_enabled`
    /// and `set_ctcss_mode`; the former changes the user preference
    /// and the latter changes the force-on override. Either can
    /// flip the effective state.
    fn apply_effective_high_pass(&mut self) {
        let effective = self.effective_high_pass_enabled();
        if effective && self.hp_l.is_none() {
            self.hp_l = Some(HighPassFilter::new(
                HIGH_PASS_CUTOFF_HZ,
                self.audio_sample_rate,
            ));
            self.hp_r = Some(HighPassFilter::new(
                HIGH_PASS_CUTOFF_HZ,
                self.audio_sample_rate,
            ));
        } else if !effective {
            self.hp_l = None;
            self.hp_r = None;
        }
    }

    /// Set the CTCSS sub-audible tone squelch mode.
    ///
    /// `Off` drops the detector (if any) and restores the user's
    /// high-pass preference. `Tone(hz)` validates `hz` against
    /// [`sdr_dsp::tone_detect::CTCSS_TONES_HZ`], constructs a fresh
    /// detector (with its own per-window hysteresis state), and
    /// force-enables the 300 Hz high-pass filter so the user
    /// doesn't hear the tone.
    ///
    /// Switching between two `Tone(_)` values rebuilds the detector
    /// from scratch — there's no way to retune a
    /// [`CtcssDetector`] in place because its neighbor-dominance
    /// filters are calibrated against the table entry, not the
    /// tone in isolation. Rebuilding is cheap (no allocations on
    /// the hot path; the mono downmix buffer is preserved across
    /// swaps) and matches how a user-driven tone change will look
    /// from the UI in PR 3.
    ///
    /// # Errors
    ///
    /// Returns [`DspError::InvalidParameter`] if `Tone(hz)` names a
    /// frequency that isn't in the CTCSS table, or if the detector
    /// constructor rejects it for any other reason (non-finite
    /// input, etc. — see [`CtcssDetector::new`] for the full list).
    pub fn set_ctcss_mode(&mut self, mode: CtcssMode) -> Result<(), DspError> {
        match mode {
            CtcssMode::Off => {
                self.ctcss_mode = CtcssMode::Off;
                self.ctcss_detector = None;
            }
            CtcssMode::Tone(hz) => {
                if ctcss_tone_index(hz).is_none() {
                    return Err(DspError::InvalidParameter(format!(
                        "CTCSS frequency {hz} Hz is not a known tone"
                    )));
                }
                // Pass the instance's actual audio_sample_rate (not
                // the CTCSS_SAMPLE_RATE_HZ constant) so the
                // detector's internal rate-validation catches a
                // misconfigured AF chain at setter time rather than
                // silently running 19200-sample windows at the
                // wrong duration. CtcssDetector::new enforces
                // equality with CTCSS_SAMPLE_RATE_HZ within a
                // 0.5 Hz tolerance and returns
                // DspError::InvalidParameter on mismatch — that's
                // exactly the contract we want here.
                #[allow(clippy::cast_possible_truncation)]
                let detector = CtcssDetector::new(hz, self.audio_sample_rate as f32)?;
                self.ctcss_mode = CtcssMode::Tone(hz);
                self.ctcss_detector = Some(detector);
            }
        }
        self.apply_effective_high_pass();
        Ok(())
    }

    /// Returns the current CTCSS squelch mode.
    pub fn ctcss_mode(&self) -> CtcssMode {
        self.ctcss_mode
    }

    /// Returns the CTCSS detector's sustained-gate state, or
    /// `false` if CTCSS is currently `Off`. Exposed for UI level-
    /// meter / status-light display in PR 3 and for testing.
    pub fn ctcss_sustained(&self) -> bool {
        self.ctcss_detector
            .as_ref()
            .is_some_and(CtcssDetector::is_sustained)
    }

    /// Enable or disable the notch filter.
    pub fn set_notch_enabled(&mut self, enabled: bool) {
        self.notch_l.set_enabled(enabled);
        self.notch_r.set_enabled(enabled);
    }

    /// Set the notch filter frequency in Hz.
    pub fn set_notch_frequency(&mut self, freq: f32) {
        self.notch_l.set_frequency(freq);
        self.notch_r.set_frequency(freq);
    }

    /// Returns whether the notch filter is enabled.
    pub fn notch_enabled(&self) -> bool {
        self.notch_l.enabled()
    }

    /// Returns the current notch filter frequency in Hz.
    pub fn notch_frequency(&self) -> f32 {
        self.notch_l.frequency()
    }

    /// Returns whether deemphasis is enabled.
    pub fn deemp_enabled(&self) -> bool {
        self.deemp_enabled
    }

    /// Returns the audio output sample rate.
    pub fn audio_sample_rate(&self) -> f64 {
        self.audio_sample_rate
    }

    /// Returns the demod AF sample rate (input rate).
    pub fn af_sample_rate(&self) -> f64 {
        self.af_sample_rate
    }

    /// Process stereo audio through the AF chain.
    ///
    /// Returns the number of output samples written. This may differ from
    /// `input.len()` when resampling is active.
    ///
    /// # Errors
    ///
    /// Returns `DspError` on buffer size or processing errors.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    pub fn process(&mut self, input: &[Stereo], output: &mut [Stereo]) -> Result<usize, DspError> {
        if input.is_empty() {
            return Ok(0);
        }

        let n = input.len();

        // Stage 1: Resample from AF rate to audio output rate.
        // C++ SDR++ order: Resample FIRST, then deemphasis at audio rate.
        let (resampled, resamp_count) = if let Some(resampler) = &mut self.resampler {
            self.resamp_in.resize(n, Complex::default());
            for (i, s) in input.iter().enumerate() {
                self.resamp_in[i] = Complex::new(s.l, s.r);
            }

            let ratio = (self.audio_sample_rate / self.af_sample_rate).ceil() as usize;
            let max_out = n * ratio.max(1) + RESAMPLER_OUTPUT_PADDING;
            self.resamp_out.resize(max_out, Complex::default());

            let out_count = resampler.process(&self.resamp_in[..n], &mut self.resamp_out)?;
            (true, out_count)
        } else {
            (false, n)
        };

        if output.len() < resamp_count {
            return Err(DspError::BufferTooSmall {
                need: resamp_count,
                got: output.len(),
            });
        }

        // Write resampled (or passthrough) samples to output.
        if resampled {
            for (out, c) in output
                .iter_mut()
                .zip(self.resamp_out.iter())
                .take(resamp_count)
            {
                *out = Stereo::new(c.re, c.im);
            }
        } else {
            output[..n].copy_from_slice(input);
        }

        // Stage 2: Deemphasis at the audio output rate (48 kHz).
        // Applied AFTER resampling, matching SDR++ signal chain order.
        if self.deemp_enabled
            && let (Some(deemp_l), Some(deemp_r)) = (&mut self.deemp_l, &mut self.deemp_r)
        {
            self.deemp_buf_l.resize(resamp_count, 0.0);
            self.deemp_buf_r.resize(resamp_count, 0.0);

            for (i, s) in output[..resamp_count].iter().enumerate() {
                self.deemp_buf_l[i] = s.l;
                self.deemp_buf_r[i] = s.r;
            }

            self.deemp_out_l.resize(resamp_count, 0.0);
            self.deemp_out_r.resize(resamp_count, 0.0);
            deemp_l.process(
                &self.deemp_buf_l[..resamp_count],
                &mut self.deemp_out_l[..resamp_count],
            )?;
            deemp_r.process(
                &self.deemp_buf_r[..resamp_count],
                &mut self.deemp_out_r[..resamp_count],
            )?;

            for (out, (&l, &r)) in output[..resamp_count].iter_mut().zip(
                self.deemp_out_l[..resamp_count]
                    .iter()
                    .zip(self.deemp_out_r[..resamp_count].iter()),
            ) {
                *out = Stereo::new(l, r);
            }
        }

        // Stage 3a: CTCSS detector tap.
        // Runs BEFORE the high-pass filter so the detector sees
        // the full-bandwidth AF including the sub-audible tone
        // (67–254 Hz). Feeds a mono downmix of the post-deemph
        // signal; the detector buffers internally and only returns
        // a decision once it has a full 400 ms / 19200-sample
        // window. Sustained state is sticky across calls.
        if let Some(detector) = self.ctcss_detector.as_mut() {
            self.ctcss_mono_buf.clear();
            self.ctcss_mono_buf.reserve(resamp_count);
            for s in &output[..resamp_count] {
                // (L + R) / 2 downmix — for NFM (the only mode
                // that uses CTCSS in practice) L and R are
                // identical, but averaging is cheap and safe for
                // any stereo content.
                self.ctcss_mono_buf.push(0.5 * (s.l + s.r));
            }
            let _ = detector.accept_samples(&self.ctcss_mono_buf);
        }

        // Stage 3b: High-pass filter at audio output rate.
        // Removes low-frequency hum and rumble (cutoff ~300 Hz).
        // Effective-enabled bit above also picks up the CTCSS
        // force-on override.
        if let (Some(hp_l), Some(hp_r)) = (&mut self.hp_l, &mut self.hp_r) {
            for s in &mut output[..resamp_count] {
                s.l = hp_l.process_sample(s.l);
                s.r = hp_r.process_sample(s.r);
            }
        }

        // Stage 4: Notch filter at audio output rate.
        // Removes specific interference tones (e.g., 50/60 Hz hum, carrier tones).
        if self.notch_l.enabled() {
            self.notch_buf_l.resize(resamp_count, 0.0);
            self.notch_buf_r.resize(resamp_count, 0.0);
            for (i, s) in output[..resamp_count].iter().enumerate() {
                self.notch_buf_l[i] = s.l;
                self.notch_buf_r[i] = s.r;
            }

            self.notch_out_l.resize(resamp_count, 0.0);
            self.notch_out_r.resize(resamp_count, 0.0);
            self.notch_l.process(
                &self.notch_buf_l[..resamp_count],
                &mut self.notch_out_l[..resamp_count],
            )?;
            self.notch_r.process(
                &self.notch_buf_r[..resamp_count],
                &mut self.notch_out_r[..resamp_count],
            )?;

            for (out, (&l, &r)) in output[..resamp_count].iter_mut().zip(
                self.notch_out_l[..resamp_count]
                    .iter()
                    .zip(self.notch_out_r[..resamp_count].iter()),
            ) {
                *out = Stereo::new(l, r);
            }
        }

        // Stage 5: CTCSS squelch gate.
        // Zeroes the entire block when the detector's sustained
        // gate is closed. This runs LAST so the preceding filters
        // (HPF/notch) still update their state on the real signal
        // — keeping the gate close doesn't desync the filters, so
        // the first block after the gate reopens doesn't have a
        // transient from state that lagged behind the signal.
        //
        // Note the asymmetry: the detector in Stage 3a is fed the
        // pre-HPF signal (it needs to see the sub-audible tone to
        // detect it), while the output we zero here is the post-
        // HPF signal (the user would otherwise hear the tone as a
        // low buzz). Zeroing post-HPF is cheaper than zeroing pre-
        // HPF anyway — a fresh zero passed through an active IIR
        // is not quite zero, so zeroing last skips that subtlety.
        if matches!(self.ctcss_mode, CtcssMode::Tone(_))
            && self
                .ctcss_detector
                .as_ref()
                .is_some_and(|d| !d.is_sustained())
        {
            for s in &mut output[..resamp_count] {
                *s = Stereo::new(0.0, 0.0);
            }
        }

        Ok(resamp_count)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::float_cmp,
    clippy::cast_precision_loss,
    clippy::manual_range_contains
)]
mod tests {
    use super::*;
    use sdr_dsp::filter::DEEMPHASIS_TAU_US;

    #[test]
    fn test_af_chain_passthrough_same_rate() {
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        let input = vec![Stereo::new(0.5, -0.5); 100];
        let mut output = vec![Stereo::default(); 100];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 100);
        assert_eq!(output[0].l, 0.5);
        assert_eq!(output[0].r, -0.5);
    }

    #[test]
    fn test_af_chain_resample_downsample() {
        // 250kHz (WFM AF rate) -> 48kHz
        let mut chain = AfChain::new(250_000.0, 48_000.0).unwrap();
        let input = vec![Stereo::new(1.0, -1.0); 2500];
        let mut output = vec![Stereo::default(); 2500];
        let count = chain.process(&input, &mut output).unwrap();
        // Should produce roughly 2500 * 48000/250000 = 480 samples
        assert!(
            count >= 350 && count <= 600,
            "expected ~480 samples, got {count}"
        );
    }

    #[test]
    fn test_af_chain_resample_upsample() {
        // 3kHz (CW AF rate) -> 48kHz
        let mut chain = AfChain::new(3_000.0, 48_000.0).unwrap();
        let input = vec![Stereo::new(0.5, 0.5); 300];
        let mut output = vec![Stereo::default(); 6000];
        let count = chain.process(&input, &mut output).unwrap();
        // Should produce roughly 300 * 48000/3000 = 4800 samples
        assert!(
            count >= 4000 && count <= 5600,
            "expected ~4800 samples, got {count}"
        );
    }

    #[test]
    fn test_af_chain_deemphasis_attenuates_high_freq() {
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        chain.set_deemp_enabled(true, DEEMPHASIS_TAU_US).unwrap();
        assert!(chain.deemp_enabled());

        // High frequency alternating signal
        let input: Vec<Stereo> = (0..1000)
            .map(|i| {
                let v = if i % 2 == 0 { 1.0 } else { -1.0 };
                Stereo::new(v, v)
            })
            .collect();
        let mut output = vec![Stereo::default(); 1000];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);

        // Peak output should be attenuated compared to input
        let peak = output[500..]
            .iter()
            .map(|s| s.l.abs())
            .fold(0.0_f32, f32::max);
        assert!(
            peak < 0.5,
            "deemphasis should attenuate high freq, peak = {peak}"
        );
    }

    #[test]
    fn test_af_chain_empty_input() {
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        let mut output = vec![Stereo::default(); 10];
        let count = chain.process(&[], &mut output).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_af_chain_deemphasis_disabled_passthrough() {
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        chain.set_deemp_enabled(false, 0.0).unwrap();
        assert!(!chain.deemp_enabled());

        let input = vec![Stereo::new(0.5, -0.3); 100];
        let mut output = vec![Stereo::default(); 100];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 100);
        assert_eq!(output[0].l, 0.5);
        assert_eq!(output[0].r, -0.3);
    }

    #[test]
    fn test_af_chain_with_default_rate() {
        let chain = AfChain::with_default_rate(24_000.0).unwrap();
        assert!((chain.audio_sample_rate() - 48_000.0).abs() < 1.0);
        assert!((chain.af_sample_rate() - 24_000.0).abs() < 1.0);
    }

    // ─────────────────────────────────────────────────────────────
    // CTCSS tests (PR 2 of #269)
    // ─────────────────────────────────────────────────────────────

    use sdr_dsp::tone_detect::{CTCSS_MIN_HITS, CTCSS_WINDOW_SAMPLES};

    /// Build a stereo block of a pure CTCSS tone at 48 kHz with
    /// amplitude 1.0 on both channels. Enough samples to cover
    /// `windows` full detector windows.
    fn ctcss_tone_block(freq_hz: f32, windows: usize) -> Vec<Stereo> {
        let n = CTCSS_WINDOW_SAMPLES * windows;
        let mut out = Vec::with_capacity(n);
        let dt = 1.0 / 48_000.0_f32;
        for i in 0..n {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32 * dt;
            let v = (core::f32::consts::TAU * freq_hz * t).sin();
            out.push(Stereo::new(v, v));
        }
        out
    }

    #[test]
    fn test_ctcss_off_passes_audio_through_unchanged() {
        // Baseline: with CTCSS off the chain behaves exactly like
        // the existing passthrough. No zeroing, no forced HPF,
        // no detector state.
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        assert_eq!(chain.ctcss_mode(), CtcssMode::Off);
        assert!(!chain.effective_high_pass_enabled());

        let input = vec![Stereo::new(0.5, -0.5); 1000];
        let mut output = vec![Stereo::default(); 1000];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
        assert_eq!(output[0].l, 0.5);
        assert_eq!(output[500].r, -0.5);
    }

    #[test]
    fn test_ctcss_set_mode_rejects_non_table_frequency() {
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        let err = chain.set_ctcss_mode(CtcssMode::Tone(123.456));
        assert!(err.is_err(), "non-CTCSS frequency must be rejected");
        assert_eq!(chain.ctcss_mode(), CtcssMode::Off);
    }

    #[test]
    fn test_ctcss_tone_mode_force_enables_high_pass() {
        // User explicitly has HPF off. CTCSS should still engage
        // the filter to strip the sub-audible tone from the
        // speaker path.
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        chain.set_high_pass_enabled(false);
        assert!(!chain.high_pass_enabled());
        assert!(!chain.effective_high_pass_enabled());

        chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();
        assert!(
            !chain.high_pass_enabled(),
            "user preference must not be silently flipped"
        );
        assert!(
            chain.effective_high_pass_enabled(),
            "CTCSS should force-enable the HPF"
        );
    }

    #[test]
    fn test_ctcss_off_restores_user_high_pass_preference() {
        // User sets HPF off, CTCSS force-engages it, then CTCSS
        // goes back to Off. The user's original preference must
        // re-take effect — no leaked force-on state.
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        chain.set_high_pass_enabled(false);
        chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();
        assert!(chain.effective_high_pass_enabled());

        chain.set_ctcss_mode(CtcssMode::Off).unwrap();
        assert!(!chain.effective_high_pass_enabled());

        // And the opposite: user sets HPF on first, CTCSS toggles
        // around it. User preference survives unchanged.
        let mut chain2 = AfChain::new(48_000.0, 48_000.0).unwrap();
        chain2.set_high_pass_enabled(true);
        chain2.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();
        assert!(chain2.effective_high_pass_enabled());
        chain2.set_ctcss_mode(CtcssMode::Off).unwrap();
        assert!(chain2.effective_high_pass_enabled());
        assert!(chain2.high_pass_enabled());
    }

    #[test]
    fn test_ctcss_wrong_tone_mutes_output() {
        // Detector targets 100 Hz but the audio is a 131.8 Hz
        // tone. After several windows the sustained gate should
        // still be closed and the output should be muted.
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();

        // Feed 4 windows so the detector has had plenty of time
        // to confirm (or reject) a signal. A bit more than
        // `CTCSS_MIN_HITS` gives a margin for the first window
        // which may partially overlap warmup.
        let windows = CTCSS_MIN_HITS + 1;
        let input = ctcss_tone_block(131.8, windows);
        let mut output = vec![Stereo::default(); input.len()];
        chain.process(&input, &mut output).unwrap();

        assert!(!chain.ctcss_sustained(), "wrong-tone must not sustain");
        // Later samples (past the HPF warmup transient) must all
        // be exactly zero — the muting happens in a single pass
        // at the end of process, so every output sample is 0.0.
        let last_window = &output[CTCSS_WINDOW_SAMPLES * CTCSS_MIN_HITS..];
        for (i, s) in last_window.iter().enumerate() {
            assert_eq!(s.l, 0.0, "sample {i} L should be muted, got {}", s.l);
            assert_eq!(s.r, 0.0, "sample {i} R should be muted, got {}", s.r);
        }
    }

    #[test]
    fn test_ctcss_correct_tone_opens_gate_after_min_hits() {
        // Detector targets 100 Hz and the audio is a 100 Hz tone.
        // After `CTCSS_MIN_HITS` windows the sustained gate must
        // open and the output must contain audible signal (post-
        // HPF, so the 100 Hz tone itself is attenuated, but some
        // residual energy remains).
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();

        let windows = CTCSS_MIN_HITS + 2;
        let input = ctcss_tone_block(100.0, windows);
        let mut output = vec![Stereo::default(); input.len()];
        chain.process(&input, &mut output).unwrap();

        assert!(
            chain.ctcss_sustained(),
            "correct-tone must open the sustained gate"
        );
        // With the gate open the muting step is skipped, so the
        // later windows are NOT all zero. The HPF attenuates 100
        // Hz heavily but doesn't null it — we should still see
        // nonzero samples somewhere in the last window.
        let last_window_start = CTCSS_WINDOW_SAMPLES * (windows - 1);
        let any_nonzero = output[last_window_start..]
            .iter()
            .any(|s| s.l.abs() > 1e-6 || s.r.abs() > 1e-6);
        assert!(
            any_nonzero,
            "open gate must pass some signal through (even after HPF)"
        );
    }

    #[test]
    fn test_ctcss_mode_change_resets_detector() {
        // Switching target tones rebuilds the detector from
        // scratch; the sustained state should NOT carry over
        // from the previous tone.
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();

        // Open the gate on 100 Hz.
        let input = ctcss_tone_block(100.0, CTCSS_MIN_HITS + 1);
        let mut output = vec![Stereo::default(); input.len()];
        chain.process(&input, &mut output).unwrap();
        assert!(chain.ctcss_sustained());

        // Switch to 131.8 Hz. Sustained state must reset.
        chain.set_ctcss_mode(CtcssMode::Tone(131.8)).unwrap();
        assert!(!chain.ctcss_sustained());
    }
}
