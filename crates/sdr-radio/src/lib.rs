//! Radio decoder — demodulator selection, IF/AF chains, mode switching.
//!
//! This crate sits between the IQ pipeline and audio output. It applies
//! IF processing (noise blanker, squelch), demodulation, and AF processing
//! (deemphasis, resampling) to convert complex IQ samples into stereo audio.

pub mod af_chain;
pub mod demod;
pub mod if_chain;

use sdr_dsp::filter::{DEEMPHASIS_TAU_EU, DEEMPHASIS_TAU_US};
use sdr_types::{Complex, DemodMode, DspError, Stereo};

use af_chain::AfChain;
use demod::{DemodConfig, Demodulator, create_demodulator};
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
    audio_sample_rate: f64,
    /// Scratch buffer for IF chain output (complex, at IF sample rate).
    if_buf: Vec<Complex>,
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
        let mode = DemodMode::Nfm;
        let demod = create_demodulator(mode)?;
        let if_chain = IfChain::new()?;
        let af_chain = AfChain::new(demod.config().af_sample_rate, audio_sample_rate)?;

        Ok(Self {
            mode,
            demod,
            if_chain,
            af_chain,
            deemp_mode: DeemphasisMode::None,
            audio_sample_rate,
            if_buf: Vec::new(),
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
    /// # Errors
    ///
    /// Returns `RadioError` if the new demodulator or AF chain cannot be created.
    pub fn set_mode(&mut self, mode: DemodMode) -> Result<(), RadioError> {
        let new_demod = create_demodulator(mode).map_err(|e| {
            RadioError::ModeSwitchFailed(format!("failed to create demod for {mode:?}: {e}"))
        })?;
        let cfg = new_demod.config();

        // Reconfigure AF chain for the new AF sample rate
        let new_af_chain = AfChain::new(cfg.af_sample_rate, self.audio_sample_rate)
            .map_err(|e| RadioError::ModeSwitchFailed(format!("failed to create AF chain: {e}")))?;

        // Apply deemphasis if the mode supports it
        let mut af_chain = new_af_chain;
        if cfg.deemp_allowed && self.deemp_mode != DeemphasisMode::None {
            af_chain
                .set_deemp_enabled(true, self.deemp_mode.tau())
                .map_err(|e| {
                    RadioError::ModeSwitchFailed(format!("failed to set deemphasis: {e}"))
                })?;
        }

        // Update IF chain feature flags based on new mode capabilities
        if !cfg.fm_if_nr_allowed {
            self.if_chain.set_fm_if_nr_enabled(false);
        }
        if !cfg.nb_allowed {
            self.if_chain.set_nb_enabled(false);
        }
        if !cfg.squelch_allowed {
            self.if_chain.set_squelch_enabled(false);
        }

        self.mode = mode;
        self.demod = new_demod;
        self.af_chain = af_chain;

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
        let ratio = (self.audio_sample_rate / cfg.af_sample_rate).ceil() as usize;
        input_count * ratio.max(1) + 16
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

        // Stage 2: Demodulation
        self.demod_buf.resize(n, Stereo::default());
        let demod_count = self.demod.process(&self.if_buf[..n], &mut self.demod_buf)?;

        // Stage 3: AF chain (deemphasis + resampling)
        let af_count = self
            .af_chain
            .process(&self.demod_buf[..demod_count], output)?;

        Ok(af_count)
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    #[test]
    fn test_radio_module_default_mode() {
        let radio = RadioModule::with_default_rate().unwrap();
        assert_eq!(radio.current_mode(), DemodMode::Nfm);
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
}
