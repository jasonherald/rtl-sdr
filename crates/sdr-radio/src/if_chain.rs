//! IF (Intermediate Frequency) processing chain.
//!
//! Applies optional noise blanking, squelch, and FM IF noise reduction
//! to complex IQ samples before demodulation.

use sdr_dsp::noise::{FmIfNoiseReduction, NoiseBlanker, PowerSquelch};
use sdr_types::{Complex, DspError};

/// Default noise blanker tracking rate.
const NB_DEFAULT_RATE: f32 = 0.05;

/// Default noise blanker threshold multiplier.
const NB_DEFAULT_LEVEL: f32 = 5.0;

/// Default squelch threshold in dB.
const SQUELCH_DEFAULT_LEVEL_DB: f32 = -100.0;

/// IF processing chain — applied to complex IQ before demodulation.
///
/// Contains optional processors that can be individually enabled/disabled:
/// 1. Noise blanker — attenuates impulse noise spikes
/// 2. Power squelch — gates signal based on power level
/// 3. FM IF noise reduction — frequency-domain noise removal for FM
pub struct IfChain {
    nb: NoiseBlanker,
    nb_enabled: bool,
    squelch: PowerSquelch,
    squelch_enabled: bool,
    fm_if_nr: FmIfNoiseReduction,
    fm_if_nr_enabled: bool,
    /// Scratch buffer A for ping-pong processing.
    buf_a: Vec<Complex>,
    /// Scratch buffer B for ping-pong processing.
    buf_b: Vec<Complex>,
}

impl IfChain {
    /// Create a new IF chain with all processors disabled.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the noise blanker cannot be created.
    pub fn new() -> Result<Self, DspError> {
        Ok(Self {
            nb: NoiseBlanker::new(NB_DEFAULT_RATE, NB_DEFAULT_LEVEL)?,
            nb_enabled: false,
            squelch: PowerSquelch::new(SQUELCH_DEFAULT_LEVEL_DB),
            squelch_enabled: false,
            fm_if_nr: FmIfNoiseReduction::new()?,
            fm_if_nr_enabled: false,
            buf_a: Vec::new(),
            buf_b: Vec::new(),
        })
    }

    /// Enable or disable the noise blanker.
    pub fn set_nb_enabled(&mut self, enabled: bool) {
        self.nb_enabled = enabled;
    }

    /// Returns whether the noise blanker is enabled.
    pub fn nb_enabled(&self) -> bool {
        self.nb_enabled
    }

    /// Set the noise blanker threshold level.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the level is invalid.
    pub fn set_nb_level(&mut self, level: f32) -> Result<(), DspError> {
        self.nb = NoiseBlanker::new(NB_DEFAULT_RATE, level)?;
        Ok(())
    }

    /// Enable or disable the power squelch.
    pub fn set_squelch_enabled(&mut self, enabled: bool) {
        self.squelch_enabled = enabled;
    }

    /// Returns whether the squelch is enabled.
    pub fn squelch_enabled(&self) -> bool {
        self.squelch_enabled
    }

    /// Set the squelch threshold in dB.
    pub fn set_squelch_level(&mut self, db: f32) {
        self.squelch.set_level(db);
    }

    /// Enable or disable auto-squelch (noise floor tracking).
    ///
    /// When enabled, the squelch threshold is automatically derived from
    /// the tracked noise floor. The manual squelch level is ignored.
    pub fn set_auto_squelch_enabled(&mut self, enabled: bool) {
        self.squelch.set_auto_squelch(enabled);
    }

    /// Returns whether auto-squelch is enabled.
    pub fn auto_squelch_enabled(&self) -> bool {
        self.squelch.auto_squelch_enabled()
    }

    /// Returns whether the squelch is currently open (signal above threshold).
    pub fn squelch_open(&self) -> bool {
        let active = self.squelch_enabled || self.squelch.auto_squelch_enabled();
        !active || self.squelch.is_open()
    }

    /// Enable or disable FM IF noise reduction.
    pub fn set_fm_if_nr_enabled(&mut self, enabled: bool) {
        self.fm_if_nr_enabled = enabled;
    }

    /// Returns whether FM IF noise reduction is enabled.
    pub fn fm_if_nr_enabled(&self) -> bool {
        self.fm_if_nr_enabled
    }

    /// Process complex IF samples through the enabled chain stages.
    ///
    /// Processing order: noise blanker -> squelch -> FM IF NR.
    /// Uses ping-pong buffers to avoid aliasing between input and output.
    ///
    /// # Errors
    ///
    /// Returns `DspError` on buffer size or processing errors.
    pub fn process(
        &mut self,
        input: &[Complex],
        output: &mut [Complex],
    ) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }

        let squelch_active = self.squelch_enabled || self.squelch.auto_squelch_enabled();
        let any_enabled = self.nb_enabled || squelch_active || self.fm_if_nr_enabled;
        if !any_enabled {
            output[..input.len()].copy_from_slice(input);
            return Ok(input.len());
        }

        let n = input.len();
        self.buf_a.resize(n, Complex::default());
        self.buf_b.resize(n, Complex::default());

        // Copy input into buf_a as the starting point
        self.buf_a[..n].copy_from_slice(input);
        // Track which buffer holds the current data (true = A, false = B)
        let mut current_is_a = true;

        // Stage 1: Noise blanker (buf_a -> buf_b or buf_b -> buf_a)
        if self.nb_enabled {
            if current_is_a {
                self.nb.process(&self.buf_a[..n], &mut self.buf_b[..n])?;
            } else {
                self.nb.process(&self.buf_b[..n], &mut self.buf_a[..n])?;
            }
            current_is_a = !current_is_a;
        }

        // Stage 2: Squelch (manual or auto)
        if squelch_active {
            if current_is_a {
                self.squelch
                    .process(&self.buf_a[..n], &mut self.buf_b[..n])?;
            } else {
                self.squelch
                    .process(&self.buf_b[..n], &mut self.buf_a[..n])?;
            }
            current_is_a = !current_is_a;
        }

        // Stage 3: FM IF noise reduction
        if self.fm_if_nr_enabled {
            if current_is_a {
                self.fm_if_nr
                    .process(&self.buf_a[..n], &mut self.buf_b[..n])?;
            } else {
                self.fm_if_nr
                    .process(&self.buf_b[..n], &mut self.buf_a[..n])?;
            }
            current_is_a = !current_is_a;
        }

        // Copy result to output
        let result = if current_is_a {
            &self.buf_a[..n]
        } else {
            &self.buf_b[..n]
        };
        output[..n].copy_from_slice(result);

        Ok(n)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_if_chain_passthrough_when_disabled() {
        let mut chain = IfChain::new().unwrap();
        let input = vec![Complex::new(1.0, 2.0); 100];
        let mut output = vec![Complex::default(); 100];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 100);
        assert_eq!(output[0].re, 1.0);
        assert_eq!(output[0].im, 2.0);
    }

    #[test]
    fn test_if_chain_squelch_enabled() {
        let mut chain = IfChain::new().unwrap();
        chain.set_squelch_enabled(true);
        chain.set_squelch_level(10.0); // very high threshold

        let input = vec![Complex::new(0.001, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        chain.process(&input, &mut output).unwrap();

        // Squelch should close on weak signal
        assert!(!chain.squelch_open());
        // Output should be zeroed
        for s in &output {
            assert!(s.re.abs() < 1e-10);
        }
    }

    #[test]
    fn test_if_chain_squelch_opens_on_strong_signal() {
        let mut chain = IfChain::new().unwrap();
        chain.set_squelch_enabled(true);
        chain.set_squelch_level(-50.0);

        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        chain.process(&input, &mut output).unwrap();

        assert!(chain.squelch_open());
    }

    #[test]
    fn test_if_chain_nb_enabled() {
        let mut chain = IfChain::new().unwrap();
        chain.set_nb_enabled(true);
        assert!(chain.nb_enabled());

        let input = vec![Complex::new(1.0, 0.0); 500];
        let mut output = vec![Complex::default(); 500];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 500);
        // Output should be non-zero (normal signal passes)
        assert!(output[499].re.abs() > 0.1);
    }

    #[test]
    fn test_if_chain_fm_if_nr_enabled() {
        let mut chain = IfChain::new().unwrap();
        chain.set_fm_if_nr_enabled(true);
        assert!(chain.fm_if_nr_enabled());

        // Use a signal large enough for the FFT block size (256 default).
        let input = vec![Complex::new(1.0, 0.0); 512];
        let mut output = vec![Complex::default(); 512];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 512);
        // DC signal should mostly survive (peak bin = 0).
        let energy: f32 = output.iter().map(|s| s.re * s.re + s.im * s.im).sum();
        assert!(energy > 0.0, "FM IF NR should produce output");
    }

    #[test]
    fn test_if_chain_all_enabled() {
        let mut chain = IfChain::new().unwrap();
        chain.set_nb_enabled(true);
        chain.set_squelch_enabled(true);
        chain.set_squelch_level(-50.0);
        chain.set_fm_if_nr_enabled(true);

        let input = vec![Complex::new(1.0, 0.0); 512];
        let mut output = vec![Complex::default(); 512];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 512);
    }

    #[test]
    fn test_if_chain_set_nb_level() {
        let mut chain = IfChain::new().unwrap();
        assert!(chain.set_nb_level(10.0).is_ok());
        assert!(chain.set_nb_level(0.5).is_err()); // below minimum of 1.0
    }

    #[test]
    fn test_if_chain_squelch_reports_open_when_disabled() {
        let chain = IfChain::new().unwrap();
        // When squelch is disabled, squelch_open should return true
        assert!(chain.squelch_open());
    }

    #[test]
    fn test_if_chain_buffer_too_small() {
        let mut chain = IfChain::new().unwrap();
        let input = [Complex::default(); 10];
        let mut output = [Complex::default(); 5];
        assert!(chain.process(&input, &mut output).is_err());
    }
}
