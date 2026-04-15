//! Radio decoder — demodulator selection, IF/AF chains, mode switching.
//!
//! This crate sits between the IQ pipeline and audio output. It applies
//! IF processing (noise blanker, squelch), demodulation, and AF processing
//! (deemphasis, resampling) to convert complex IQ samples into stereo audio.

pub mod af_chain;
pub mod demod;
pub mod if_chain;

use sdr_dsp::filter::{DEEMPHASIS_TAU_EU, DEEMPHASIS_TAU_US};
use sdr_dsp::multirate::RationalResampler;
use sdr_types::{Complex, DemodMode, DspError, Stereo};

use af_chain::{AfChain, CtcssMode};
use demod::{DemodConfig, Demodulator, create_demodulator};
use sdr_dsp::voice_squelch::VoiceSquelchMode;

/// Tolerance for considering two sample rates equal (skip resampling).
const RATE_TOLERANCE: f64 = 1.0;
use if_chain::IfChain;

/// Default audio output sample rate (Hz).
const DEFAULT_AUDIO_SAMPLE_RATE: f64 = 48_000.0;

/// Deemphasis mode for FM broadcast.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeemphasisMode {
    /// US/Japan: 75 microsecond time constant.
    Us75,
    /// Europe/Australia: 50 microsecond time constant.
    Eu50,
    /// No deemphasis.
    None,
}

impl DeemphasisMode {
    /// Get the time constant in seconds for this mode, or 0.0 for None.
    pub fn tau(self) -> f64 {
        match self {
            Self::Us75 => DEEMPHASIS_TAU_US,
            Self::Eu50 => DEEMPHASIS_TAU_EU,
            Self::None => 0.0,
        }
    }
}

/// Errors from radio module operations.
#[derive(Debug, thiserror::Error)]
pub enum RadioError {
    /// A DSP processing error occurred.
    #[error("DSP error: {0}")]
    Dsp(#[from] DspError),

    /// The requested mode switch failed.
    #[error("mode switch failed: {0}")]
    ModeSwitchFailed(String),
}

/// Complete radio decoder module — IF chain, demodulator, AF chain.
///
/// Processes complex IQ samples through the full signal path:
/// 1. IF chain: noise blanker, squelch, FM IF NR
/// 2. Demodulator: mode-specific IQ-to-audio conversion
/// 3. AF chain: deemphasis, sample rate conversion to audio output rate
pub struct RadioModule {
    mode: DemodMode,
    demod: Box<dyn Demodulator + Send>,
    if_chain: IfChain,
    af_chain: AfChain,
    deemp_mode: DeemphasisMode,
    high_pass_enabled: bool,
    notch_enabled: bool,
    notch_frequency: f32,
    /// Persisted CTCSS squelch mode. Reapplied to the new AF chain
    /// on mode switch (when the demod rate changes the AF chain is
    /// rebuilt from scratch, so the CTCSS state has to be restored
    /// the same way deemphasis / notch / high-pass are).
    ctcss_mode: CtcssMode,
    /// Persisted CTCSS detection threshold, paired with
    /// `ctcss_mode`. Same reapply-on-rebuild pattern.
    ctcss_threshold: f32,
    /// Persisted voice-activity squelch mode (Off / Syllabic /
    /// Snr). Reapplied to the new AF chain on mode switch the
    /// same way CTCSS is.
    voice_squelch_mode: VoiceSquelchMode,
    audio_sample_rate: f64,
    /// Input sample rate from the IQ frontend (Hz).
    input_sample_rate: f64,
    /// Resampler from input rate to demod IF rate (None if rates match).
    input_resampler: Option<RationalResampler>,
    /// Scratch buffer for IF chain output (complex, at input sample rate).
    if_buf: Vec<Complex>,
    /// Scratch buffer for resampled IQ (at demod IF rate).
    resamp_buf: Vec<Complex>,
    /// Scratch buffer for demod output (stereo, at AF sample rate).
    demod_buf: Vec<Stereo>,
}

impl RadioModule {
    /// Create a new radio module with default NFM mode.
    ///
    /// - `audio_sample_rate`: target audio output rate (Hz), typically 48 kHz
    ///
    /// # Errors
    ///
    /// Returns `RadioError` if initialization fails.
    pub fn new(audio_sample_rate: f64) -> Result<Self, RadioError> {
        let mode = DemodMode::Wfm;
        let demod = create_demodulator(mode)?;
        let if_chain = IfChain::new()?;
        let af_chain = AfChain::new(demod.config().af_sample_rate, audio_sample_rate)?;

        Ok(Self {
            mode,
            demod,
            if_chain,
            af_chain,
            deemp_mode: DeemphasisMode::None,
            high_pass_enabled: false,
            notch_enabled: false,
            notch_frequency: sdr_dsp::filter::DEFAULT_NOTCH_FREQ_HZ,
            ctcss_mode: CtcssMode::Off,
            ctcss_threshold: sdr_dsp::tone_detect::CTCSS_DEFAULT_THRESHOLD,
            voice_squelch_mode: VoiceSquelchMode::Off,
            audio_sample_rate,
            input_sample_rate: 0.0,
            input_resampler: None,
            if_buf: Vec::new(),
            resamp_buf: Vec::new(),
            demod_buf: Vec::new(),
        })
    }

    /// Create a new radio module with the default audio sample rate (48 kHz).
    ///
    /// # Errors
    ///
    /// Returns `RadioError` if initialization fails.
    pub fn with_default_rate() -> Result<Self, RadioError> {
        Self::new(DEFAULT_AUDIO_SAMPLE_RATE)
    }

    /// Switch to a new demodulation mode.
    ///
    /// This reconfigures the demodulator, IF chain feature flags, and AF chain
    /// (including resampler) to match the new mode's requirements.
    ///
    /// IF chain features (noise blanker, squelch, FM IF NR) are **only disabled**
    /// when the new mode doesn't support them. They are not automatically
    /// re-enabled on mode switch, preserving the user's explicit disable choice.
    /// Call `set_squelch_enabled(true)` etc. to re-enable after switching.
    ///
    /// # Errors
    ///
    /// Returns `RadioError` if the new demodulator or AF chain cannot be created.
    pub fn set_mode(&mut self, mode: DemodMode) -> Result<(), RadioError> {
        let new_demod = create_demodulator(mode).map_err(|e| {
            RadioError::ModeSwitchFailed(format!("failed to create demod for {mode:?}: {e}"))
        })?;

        // Extract config values before moving new_demod
        let af_rate = new_demod.config().af_sample_rate;
        let if_rate = new_demod.config().if_sample_rate;
        let deemp_allowed = new_demod.config().deemp_allowed;
        let fm_if_nr_allowed = new_demod.config().fm_if_nr_allowed;
        let nb_allowed = new_demod.config().nb_allowed;
        let squelch_allowed = new_demod.config().squelch_allowed;
        let high_pass_allowed = new_demod.config().high_pass_allowed;

        // Reconfigure AF chain for the new AF sample rate
        let new_af_chain = AfChain::new(af_rate, self.audio_sample_rate)
            .map_err(|e| RadioError::ModeSwitchFailed(format!("failed to create AF chain: {e}")))?;

        // Reapply persisted AF chain settings to the new chain
        let mut af_chain = new_af_chain;
        if deemp_allowed && self.deemp_mode != DeemphasisMode::None {
            af_chain
                .set_deemp_enabled(true, self.deemp_mode.tau())
                .map_err(|e| {
                    RadioError::ModeSwitchFailed(format!("failed to set deemphasis: {e}"))
                })?;
        }
        if self.high_pass_enabled && high_pass_allowed {
            af_chain.set_high_pass_enabled(true);
        }
        // Always restore notch frequency (even when disabled) so it's
        // correct when the user re-enables after a mode switch.
        af_chain.set_notch_frequency(self.notch_frequency);
        af_chain.set_notch_enabled(self.notch_enabled);
        // Restore CTCSS threshold FIRST so the detector built by
        // set_ctcss_mode picks it up instead of the default.
        // Sustained-gate state intentionally resets to closed on
        // mode switch — a new mode means the user retuned or
        // changed decode, and holding an old "tone confirmed"
        // latch across that transition would let stray audio
        // through before the detector re-confirmed on the new
        // signal. `set_ctcss_mode` rebuilds the detector from
        // scratch so this is the natural behavior.
        af_chain
            .set_ctcss_threshold(self.ctcss_threshold)
            .map_err(|e| {
                RadioError::ModeSwitchFailed(format!("failed to set CTCSS threshold: {e}"))
            })?;
        af_chain
            .set_ctcss_mode(self.ctcss_mode)
            .map_err(|e| RadioError::ModeSwitchFailed(format!("failed to set CTCSS mode: {e}")))?;
        // Restore voice squelch mode. Like CTCSS, the gate state
        // intentionally resets to closed on mode switch — the new
        // AF chain is a fresh detector and has to warm up before
        // opening.
        af_chain
            .set_voice_squelch_mode(self.voice_squelch_mode)
            .map_err(|e| {
                RadioError::ModeSwitchFailed(format!("failed to set voice squelch mode: {e}"))
            })?;

        // Update IF chain feature flags based on new mode capabilities
        if !fm_if_nr_allowed {
            self.if_chain.set_fm_if_nr_enabled(false);
        }
        if !nb_allowed {
            self.if_chain.set_nb_enabled(false);
        }
        if !squelch_allowed {
            self.if_chain.set_squelch_enabled(false);
        }

        self.mode = mode;
        self.demod = new_demod;
        self.af_chain = af_chain;

        // Rebuild input resampler for the new demod's IF rate
        if self.input_sample_rate > 0.0 {
            if (self.input_sample_rate - if_rate).abs() < RATE_TOLERANCE {
                self.input_resampler = None;
            } else {
                self.input_resampler = Some(
                    RationalResampler::new(self.input_sample_rate, if_rate).map_err(|e| {
                        RadioError::ModeSwitchFailed(format!("input resampler: {e}"))
                    })?,
                );
            }
        }

        tracing::debug!("switched to mode {:?}", mode);
        Ok(())
    }

    /// Estimate the maximum output sample count for a given input count.
    ///
    /// Use this to size the `output` buffer before calling [`process()`](Self::process).
    /// Accounts for the AF chain resampling ratio (e.g., CW 3kHz → 48kHz = 16x).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn max_output_samples(&self, input_count: usize) -> usize {
        let cfg = self.demod.config();
        // Account for input resampling (input_rate → IF rate) + AF resampling (AF rate → audio rate)
        let input_ratio = if self.input_sample_rate > 0.0 {
            (cfg.if_sample_rate / self.input_sample_rate).max(1.0)
        } else {
            1.0
        };
        let af_ratio = (self.audio_sample_rate / cfg.af_sample_rate).ceil() as usize;
        #[allow(clippy::cast_precision_loss)]
        let resampled_input = ((input_count as f64) * input_ratio).ceil() as usize + 16;
        resampled_input * af_ratio.max(1) + 16
    }

    /// Process complex IQ samples through the full radio chain.
    ///
    /// Returns the number of stereo audio samples written to `output`.
    /// The output count may differ from `input.len()` due to AF resampling.
    ///
    /// Callers must size `output` using [`max_output_samples()`](Self::max_output_samples)
    /// to accommodate upsampling (e.g., CW 3kHz → 48kHz produces ~16x more samples).
    ///
    /// # Errors
    ///
    /// Returns `RadioError` on processing errors.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn process(
        &mut self,
        input: &[Complex],
        output: &mut [Stereo],
    ) -> Result<usize, RadioError> {
        if input.is_empty() {
            return Ok(0);
        }

        let n = input.len();

        // Stage 1: IF chain
        self.if_buf.resize(n, Complex::default());
        self.if_chain.process(input, &mut self.if_buf)?;

        // Stage 1.5: Resample from input rate to demod IF rate (if needed)
        let demod_input = if let Some(resampler) = &mut self.input_resampler {
            // Estimate output size: input * (if_rate / input_rate) + padding
            let if_rate = self.demod.config().if_sample_rate;
            let ratio = if_rate / self.input_sample_rate;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let est_out = ((n as f64) * ratio).ceil() as usize + 16;
            self.resamp_buf.resize(est_out, Complex::default());
            resampler.process(&self.if_buf[..n], &mut self.resamp_buf)?
        } else {
            n
        };

        let demod_src = if self.input_resampler.is_some() {
            &self.resamp_buf[..demod_input]
        } else {
            &self.if_buf[..n]
        };

        // Stage 2: Demodulation
        self.demod_buf.resize(demod_input, Stereo::default());
        let demod_count = self.demod.process(demod_src, &mut self.demod_buf)?;

        // Stage 3: AF chain (deemphasis + resampling)
        let af_count = self
            .af_chain
            .process(&self.demod_buf[..demod_count], output)?;

        Ok(af_count)
    }

    /// Set the input sample rate from the IQ frontend.
    ///
    /// This configures an internal resampler to convert from the actual
    /// input rate to the demod's expected IF sample rate. Call this whenever
    /// the frontend's effective sample rate changes (decimation, sample rate).
    ///
    /// # Errors
    ///
    /// Returns `RadioError` if the resampler cannot be created.
    pub fn set_input_sample_rate(&mut self, rate: f64) -> Result<(), RadioError> {
        let if_rate = self.demod.config().if_sample_rate;
        let resampler = if (rate - if_rate).abs() < RATE_TOLERANCE {
            None
        } else {
            Some(RationalResampler::new(rate, if_rate).map_err(RadioError::Dsp)?)
        };
        // Commit state only after the resampler is successfully built.
        self.input_sample_rate = rate;
        self.input_resampler = resampler;
        Ok(())
    }

    /// Set the channel bandwidth.
    pub fn set_bandwidth(&mut self, bw: f64) {
        self.demod.set_bandwidth(bw);
    }

    /// Set the squelch threshold in dB.
    pub fn set_squelch(&mut self, level_db: f32) {
        self.if_chain.set_squelch_level(level_db);
    }

    /// Enable or disable the squelch.
    pub fn set_squelch_enabled(&mut self, enabled: bool) {
        self.if_chain.set_squelch_enabled(enabled);
    }

    /// Enable or disable auto-squelch (noise floor tracking).
    ///
    /// When enabled, the squelch threshold is automatically derived from
    /// the tracked noise floor with hysteresis. The manual squelch level
    /// is ignored while auto-squelch is active.
    pub fn set_auto_squelch_enabled(&mut self, enabled: bool) {
        self.if_chain.set_auto_squelch_enabled(enabled);
    }

    /// Set the deemphasis mode.
    ///
    /// # Errors
    ///
    /// Returns `RadioError` if the deemphasis filter cannot be created.
    pub fn set_deemp_mode(&mut self, mode: DeemphasisMode) -> Result<(), RadioError> {
        self.deemp_mode = mode;
        let cfg = self.demod.config();
        if cfg.deemp_allowed && mode != DeemphasisMode::None {
            self.af_chain.set_deemp_enabled(true, mode.tau())?;
        } else {
            self.af_chain.set_deemp_enabled(false, 0.0)?;
        }
        Ok(())
    }

    /// Enable or disable the audio high-pass filter.
    ///
    /// Persists across mode changes — reapplied when the AF chain is rebuilt.
    pub fn set_high_pass_enabled(&mut self, enabled: bool) {
        self.high_pass_enabled = enabled;
        self.af_chain.set_high_pass_enabled(enabled);
    }

    /// Enable or disable the audio notch filter.
    ///
    /// Persists across mode changes — reapplied when the AF chain is rebuilt.
    pub fn set_notch_enabled(&mut self, enabled: bool) {
        self.notch_enabled = enabled;
        self.af_chain.set_notch_enabled(enabled);
    }

    /// Set the audio notch filter frequency in Hz.
    ///
    /// Persists across mode changes — reapplied when the AF chain is rebuilt.
    pub fn set_notch_frequency(&mut self, freq: f32) {
        self.notch_frequency = freq;
        self.af_chain.set_notch_frequency(freq);
    }

    /// Set the CTCSS sub-audible tone squelch mode.
    ///
    /// `CtcssMode::Off` disables the detector and restores the
    /// user's explicit high-pass preference. `CtcssMode::Tone(hz)`
    /// validates `hz` against the standard 51-entry CTCSS table,
    /// constructs a fresh detector at the current audio rate, and
    /// force-enables the 300 Hz speaker-path high-pass filter so
    /// the user doesn't hear the sub-audible tone as a low buzz.
    ///
    /// Persists across mode changes — reapplied when the AF chain
    /// is rebuilt. See [`AfChain::set_ctcss_mode`] for details on
    /// the detector's window / hysteresis behavior.
    ///
    /// # Errors
    ///
    /// Returns [`RadioError::Dsp`] if the frequency isn't a known
    /// CTCSS tone or the detector constructor rejects it.
    pub fn set_ctcss_mode(&mut self, mode: CtcssMode) -> Result<(), RadioError> {
        self.af_chain.set_ctcss_mode(mode)?;
        self.ctcss_mode = mode;
        Ok(())
    }

    /// Returns the current CTCSS squelch mode.
    pub fn ctcss_mode(&self) -> CtcssMode {
        self.ctcss_mode
    }

    /// Returns the CTCSS sustained-gate state: `true` when the
    /// target tone has been confirmed present for at least
    /// [`sdr_dsp::tone_detect::CTCSS_MIN_HITS`] consecutive
    /// windows. Always `false` when CTCSS is `Off`.
    pub fn ctcss_sustained(&self) -> bool {
        self.af_chain.ctcss_sustained()
    }

    /// Set the CTCSS detection threshold (normalized magnitude
    /// ratio, `(0, 1]`). Default is
    /// [`sdr_dsp::tone_detect::CTCSS_DEFAULT_THRESHOLD`] (0.1).
    /// Persists across mode changes.
    ///
    /// # Errors
    ///
    /// Returns [`RadioError::Dsp`] if the value is non-finite or
    /// out of range.
    pub fn set_ctcss_threshold(&mut self, threshold: f32) -> Result<(), RadioError> {
        self.af_chain.set_ctcss_threshold(threshold)?;
        self.ctcss_threshold = threshold;
        Ok(())
    }

    /// Returns the current CTCSS detection threshold.
    pub fn ctcss_threshold(&self) -> f32 {
        self.ctcss_threshold
    }

    /// Set the voice-activity squelch mode. `Off` is the default
    /// (audio passes through unchanged). `Syllabic(threshold)` runs
    /// a ~4 Hz envelope-modulation detector for speech-cadence
    /// detection. `Snr(threshold_db)` runs a voice-band vs out-of-
    /// voice-band power ratio detector. Persists across mode
    /// changes.
    ///
    /// See [`sdr_dsp::voice_squelch`] for the underlying DSP.
    ///
    /// # Errors
    ///
    /// Returns [`RadioError::Dsp`] if the mode carries a non-
    /// finite or otherwise invalid threshold.
    pub fn set_voice_squelch_mode(&mut self, mode: VoiceSquelchMode) -> Result<(), RadioError> {
        self.af_chain.set_voice_squelch_mode(mode)?;
        self.voice_squelch_mode = mode;
        Ok(())
    }

    /// Returns the current voice-squelch mode.
    pub fn voice_squelch_mode(&self) -> VoiceSquelchMode {
        self.voice_squelch_mode
    }

    /// Returns the voice-squelch gate state: `true` when the
    /// detector has opened (speech-like content present) or when
    /// the mode is `Off` (gate permanently open). `false` when
    /// an active detector has the gate closed.
    pub fn voice_squelch_open(&self) -> bool {
        self.af_chain.voice_squelch_open()
    }

    /// Update the voice-squelch threshold. The interpretation of
    /// `threshold` depends on the currently active mode: for
    /// `Syllabic` it's a normalized envelope-ratio value
    /// (positive, unitless), for `Snr` it's dB. No-op when the
    /// mode is `Off`.
    ///
    /// Updates the persisted mode's inline threshold so
    /// subsequent mode reloads (e.g. on `set_mode`) carry the
    /// tuned value forward.
    ///
    /// # Errors
    ///
    /// Returns [`RadioError::Dsp`] if the threshold is non-finite
    /// or (for syllabic) non-positive.
    pub fn set_voice_squelch_threshold(&mut self, threshold: f32) -> Result<(), RadioError> {
        self.af_chain.set_voice_squelch_threshold(threshold)?;
        // Mirror the update into the cached mode so set_mode's
        // reapply picks up the tuned value. `Off` variant has no
        // threshold to update — no-op, matching the AF chain.
        self.voice_squelch_mode = match self.voice_squelch_mode {
            VoiceSquelchMode::Off => VoiceSquelchMode::Off,
            VoiceSquelchMode::Syllabic { .. } => VoiceSquelchMode::Syllabic { threshold },
            VoiceSquelchMode::Snr { .. } => VoiceSquelchMode::Snr {
                threshold_db: threshold,
            },
        };
        Ok(())
    }

    /// Enable or disable WFM stereo decode.
    ///
    /// Only has an effect when the current mode is WFM. For other modes this
    /// is a no-op via the default trait implementation.
    pub fn set_wfm_stereo(&mut self, enabled: bool) {
        self.demod.set_stereo(enabled);
    }

    /// Get the current demodulation mode.
    pub fn current_mode(&self) -> DemodMode {
        self.mode
    }

    /// Get the current demodulator's configuration.
    pub fn demod_config(&self) -> &DemodConfig {
        self.demod.config()
    }

    /// Get a reference to the IF chain for direct configuration.
    pub fn if_chain(&self) -> &IfChain {
        &self.if_chain
    }

    /// Get a mutable reference to the IF chain for direct configuration.
    pub fn if_chain_mut(&mut self) -> &mut IfChain {
        &mut self.if_chain
    }

    /// Get a reference to the AF chain.
    pub fn af_chain(&self) -> &AfChain {
        &self.af_chain
    }

    /// Get a mutable reference to the AF chain.
    pub fn af_chain_mut(&mut self) -> &mut AfChain {
        &mut self.af_chain
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    // ─── CTCSS threshold test fixtures ──────────────────────────
    // Per project convention, test magic numbers (thresholds,
    // tolerances, invalid-input lists) are named constants. These
    // feed `test_radio_module_ctcss_threshold_*` — if the DSP
    // layer's threshold range ever changes, there's one place to
    // tune the test data.

    /// Float tolerance for CTCSS threshold round-trip equality.
    /// `1e-6` comfortably exceeds f32 rounding error for the
    /// single-assignment round-trips the tests exercise.
    const CTCSS_TEST_EPS: f32 = 1e-6;

    /// Non-default value used by the persistence test. Chosen
    /// strictly inside the DSP-layer `(0, 1]` range and clearly
    /// different from the `CTCSS_DEFAULT_THRESHOLD` (0.1) so a
    /// regression that silently reverts to the default fails
    /// loudly.
    const CTCSS_PERSIST_THRESHOLD: f32 = 0.25;

    /// "Last-good" baseline used by the rejection test. Any
    /// in-range value would work; 0.2 is distinct from both the
    /// DSP default (0.1) and the persistence test's 0.25 so
    /// cross-test contamination would be noticeable.
    const CTCSS_LAST_GOOD_THRESHOLD: f32 = 0.2;

    /// Values that `set_ctcss_threshold` must reject. Covers the
    /// boundary cases (0.0, just over 1.0), a sub-zero, and all
    /// three non-finite IEEE-754 values. Used by
    /// `test_radio_module_ctcss_threshold_rejects_invalid`.
    const INVALID_CTCSS_THRESHOLDS: [f32; 6] =
        [0.0, -0.1, 1.001, f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

    #[test]
    fn test_radio_module_default_mode() {
        let radio = RadioModule::with_default_rate().unwrap();
        assert_eq!(radio.current_mode(), DemodMode::Wfm);
    }

    #[test]
    fn test_radio_module_mode_switching() {
        let mut radio = RadioModule::with_default_rate().unwrap();
        let modes = [
            DemodMode::Wfm,
            DemodMode::Nfm,
            DemodMode::Am,
            DemodMode::Usb,
            DemodMode::Lsb,
            DemodMode::Dsb,
            DemodMode::Cw,
            DemodMode::Raw,
        ];
        for mode in modes {
            radio.set_mode(mode).unwrap();
            assert_eq!(radio.current_mode(), mode);
        }
    }

    #[test]
    fn test_radio_module_process_nfm() {
        let mut radio = RadioModule::with_default_rate().unwrap();
        // Generate FM-modulated signal
        let input: Vec<Complex> = (0..1000)
            .map(|i| {
                let phase = 2.0 * PI * 1000.0 * (i as f32) / 50_000.0;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![Stereo::default(); 2000];
        let count = radio.process(&input, &mut output).unwrap();
        // NFM: 50kHz -> 48kHz, so output count should be ~960
        assert!(count > 0, "should produce output");
        assert!(count <= 2000, "should not overflow");
    }

    #[test]
    fn test_radio_module_process_am() {
        let mut radio = RadioModule::with_default_rate().unwrap();
        radio.set_mode(DemodMode::Am).unwrap();

        // AM signal: carrier with amplitude modulation
        let input: Vec<Complex> = (0..1000)
            .map(|i| {
                let amp = 1.0 + 0.5 * (2.0 * PI * 0.01 * i as f32).sin();
                Complex::new(amp, 0.0)
            })
            .collect();
        let mut output = vec![Stereo::default(); 5000];
        let count = radio.process(&input, &mut output).unwrap();
        // AM: 15kHz -> 48kHz, output should be upsampled
        assert!(count > 0, "should produce output");
    }

    #[test]
    fn test_radio_module_process_raw() {
        let mut radio = RadioModule::with_default_rate().unwrap();
        radio.set_mode(DemodMode::Raw).unwrap();

        let input = vec![Complex::new(0.5, -0.3); 100];
        let mut output = vec![Stereo::default(); 200];
        let count = radio.process(&input, &mut output).unwrap();
        // Raw: 48kHz -> 48kHz, no resampling needed
        assert_eq!(count, 100);
        // Should pass through IQ as stereo (after IF chain which is passthrough when disabled)
        assert!((output[0].l - 0.5).abs() < 1e-4);
        assert!((output[0].r - (-0.3)).abs() < 1e-4);
    }

    #[test]
    fn test_radio_module_process_empty() {
        let mut radio = RadioModule::with_default_rate().unwrap();
        let mut output = vec![Stereo::default(); 100];
        let count = radio.process(&[], &mut output).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_radio_module_squelch() {
        let mut radio = RadioModule::with_default_rate().unwrap();
        radio.set_squelch_enabled(true);
        radio.set_squelch(10.0); // very high threshold

        let input = vec![Complex::new(0.001, 0.0); 500];
        let mut output = vec![Stereo::default(); 1000];
        let count = radio.process(&input, &mut output).unwrap();
        assert!(count > 0);
        // All output should be near zero (squelch closed)
        let peak = output[..count]
            .iter()
            .map(|s| s.l.abs().max(s.r.abs()))
            .fold(0.0_f32, f32::max);
        assert!(peak < 0.01, "squelch should mute output, peak = {peak}");
    }

    #[test]
    fn test_radio_module_deemphasis() {
        let mut radio = RadioModule::with_default_rate().unwrap();
        radio.set_mode(DemodMode::Wfm).unwrap();
        // Enable deemphasis
        radio.set_deemp_mode(DeemphasisMode::Eu50).unwrap();
        assert!(radio.demod_config().deemp_allowed);

        // Switch to a mode that doesn't support deemphasis
        radio.set_mode(DemodMode::Am).unwrap();
        assert!(!radio.demod_config().deemp_allowed);
    }

    #[test]
    fn test_radio_module_deemp_mode_tau() {
        assert!((DeemphasisMode::Us75.tau() - 75e-6).abs() < 1e-10);
        assert!((DeemphasisMode::Eu50.tau() - 50e-6).abs() < 1e-10);
        assert!((DeemphasisMode::None.tau() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_radio_module_config_access() {
        let radio = RadioModule::with_default_rate().unwrap();
        let cfg = radio.demod_config();
        assert!(cfg.if_sample_rate > 0.0);
        assert!(cfg.af_sample_rate > 0.0);
    }

    #[test]
    fn test_radio_module_if_chain_access() {
        let mut radio = RadioModule::with_default_rate().unwrap();
        radio.if_chain_mut().set_nb_enabled(true);
        assert!(radio.if_chain().nb_enabled());
    }

    #[test]
    fn test_radio_module_set_bandwidth() {
        let mut radio = RadioModule::with_default_rate().unwrap();
        radio.set_mode(DemodMode::Usb).unwrap();
        // Should not panic or error
        radio.set_bandwidth(3000.0);
    }

    #[test]
    fn test_radio_error_display() {
        let err = RadioError::Dsp(DspError::InvalidParameter("test".to_string()));
        let msg = format!("{err}");
        assert!(msg.contains("DSP error"));

        let err = RadioError::ModeSwitchFailed("test".to_string());
        let msg = format!("{err}");
        assert!(msg.contains("mode switch failed"));
    }

    #[test]
    fn test_radio_module_auto_squelch() {
        let mut radio = RadioModule::with_default_rate().unwrap();
        radio.set_squelch_enabled(true);
        radio.set_auto_squelch_enabled(true);

        // Verify auto-squelch is enabled on the IF chain
        assert!(radio.if_chain().auto_squelch_enabled());

        // Disable and verify
        radio.set_auto_squelch_enabled(false);
        assert!(!radio.if_chain().auto_squelch_enabled());
    }

    #[test]
    fn test_radio_module_mode_switch_preserves_deemp() {
        let mut radio = RadioModule::with_default_rate().unwrap();
        radio.set_mode(DemodMode::Wfm).unwrap();
        radio.set_deemp_mode(DeemphasisMode::Eu50).unwrap();

        // Switch to another FM mode (NFM doesn't support deemp)
        radio.set_mode(DemodMode::Nfm).unwrap();
        // Deemp mode should be preserved in the radio module
        // but disabled in the AF chain since NFM doesn't allow it

        // Switch back to WFM
        radio.set_mode(DemodMode::Wfm).unwrap();
        // The deemp mode is still Eu50 in the radio, and WFM allows it
        assert!(radio.af_chain().deemp_enabled());
    }

    #[test]
    fn test_radio_module_ctcss_threshold_persists_across_set_mode() {
        // RadioModule caches ctcss_threshold and reapplies it to
        // the new AF chain on mode switch. Without the persistence,
        // a mode change would snap the threshold back to the
        // DSP-layer default and silently un-tune the user's setting.
        let mut radio = RadioModule::with_default_rate().unwrap();
        radio.set_ctcss_threshold(CTCSS_PERSIST_THRESHOLD).unwrap();
        assert!((radio.ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS);
        assert!(
            (radio.af_chain().ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS
        );

        // Mode switch rebuilds the AF chain from scratch. The
        // cached threshold must survive AND be reapplied to the
        // new chain, not just stored on the RadioModule.
        radio.set_mode(DemodMode::Nfm).unwrap();
        assert!((radio.ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS);
        assert!(
            (radio.af_chain().ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS
        );

        radio.set_mode(DemodMode::Am).unwrap();
        assert!((radio.ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS);
        assert!(
            (radio.af_chain().ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS
        );
    }

    #[test]
    fn test_radio_module_ctcss_threshold_rejects_invalid() {
        // Invalid values must fail fast at the RadioModule boundary
        // (not deep in the DSP layer) and must NOT corrupt either
        // the cached value OR the live AF-chain detector state.
        // The RadioModule cache advances only after the AF chain
        // accepts the new value, so a correctly-ordered setter
        // leaves both in sync on rejection. Checking both levels
        // pins that invariant — a regression that mutated one
        // without the other (e.g. af_chain storing the bad value
        // before the range check, or cache advancing before
        // validation) would slip past a cache-only assertion.
        let mut radio = RadioModule::with_default_rate().unwrap();
        radio
            .set_ctcss_threshold(CTCSS_LAST_GOOD_THRESHOLD)
            .unwrap();

        // Match on the exact error variant (not just `is_err`) so
        // a future refactor can't mask the failure with a wrong
        // error type (e.g. accidentally promoting to
        // `RadioError::ModeSwitchFailed`).
        for v in INVALID_CTCSS_THRESHOLDS {
            assert!(
                matches!(
                    radio.set_ctcss_threshold(v),
                    Err(RadioError::Dsp(DspError::InvalidParameter(_)))
                ),
                "threshold {v} should produce Err(RadioError::Dsp(DspError::InvalidParameter(_)))"
            );
            // After every single rejection, BOTH the cached value
            // and the AF chain's effective value must still be
            // the last-good baseline. Re-asserting inside the loop
            // (not just after) catches a hypothetical bug where
            // the first rejected value corrupts one layer and
            // subsequent rejected values corrupt the other —
            // a post-loop assertion on the final state would
            // miss that.
            assert!(
                (radio.ctcss_threshold() - CTCSS_LAST_GOOD_THRESHOLD).abs() < CTCSS_TEST_EPS,
                "RadioModule cache drifted after rejected value {v}"
            );
            assert!(
                (radio.af_chain().ctcss_threshold() - CTCSS_LAST_GOOD_THRESHOLD).abs()
                    < CTCSS_TEST_EPS,
                "AF chain effective threshold drifted after rejected value {v}"
            );
        }
    }
}
