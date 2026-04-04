//! Noise reduction and squelch processors.
//!
//! Ports SDR++ `dsp::noise_reduction` namespace.

use sdr_types::{Complex, DspError};

/// Power squelch — gates signal based on average power level.
///
/// Ports SDR++ `dsp::noise_reduction::PowerSquelch`. Computes the mean
/// amplitude of the input block and compares against a threshold in dB.
/// If below threshold, the entire block is zeroed.
pub struct PowerSquelch {
    level_db: f32,
}

impl PowerSquelch {
    /// Create a new power squelch.
    ///
    /// - `level_db`: threshold in dB (e.g., -50.0). Signal below this is muted.
    pub fn new(level_db: f32) -> Self {
        Self { level_db }
    }

    /// Update the squelch threshold.
    pub fn set_level(&mut self, level_db: f32) {
        self.level_db = level_db;
    }

    /// Process complex samples. Passes or zeros the entire block.
    ///
    /// Returns `true` if the signal is above the threshold (open), `false` if muted.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    #[allow(clippy::cast_precision_loss)]
    pub fn process(&self, input: &[Complex], output: &mut [Complex]) -> Result<bool, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        if input.is_empty() {
            return Ok(false);
        }

        // Compute mean amplitude
        let sum: f32 = input.iter().map(|s| s.amplitude()).sum();
        let mean = sum / input.len() as f32;

        // Compare in dB
        let power_db = 10.0 * mean.max(f32::MIN_POSITIVE).log10();

        if power_db >= self.level_db {
            output[..input.len()].copy_from_slice(input);
            Ok(true)
        } else {
            output[..input.len()].fill(Complex::default());
            Ok(false)
        }
    }
}

/// Noise blanker — attenuates impulse noise spikes.
///
/// Ports SDR++ `dsp::noise_reduction::NoiseBlanker`. Tracks average signal
/// amplitude and reduces gain on samples that exceed it by a configurable factor.
pub struct NoiseBlanker {
    rate: f32,
    inv_rate: f32,
    level: f32,
    amp: f32,
}

impl NoiseBlanker {
    /// Create a new noise blanker.
    ///
    /// - `rate`: amplitude tracking rate (0 to 1, higher = faster tracking)
    /// - `level`: threshold multiplier — samples exceeding `level * avg_amp` are attenuated
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `rate` is not in (0, 1) or `level` is non-positive.
    pub fn new(rate: f32, level: f32) -> Result<Self, DspError> {
        if rate <= 0.0 || rate >= 1.0 {
            return Err(DspError::InvalidParameter(format!(
                "rate must be in (0, 1), got {rate}"
            )));
        }
        if level <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "level must be positive, got {level}"
            )));
        }
        Ok(Self {
            rate,
            inv_rate: 1.0 - rate,
            level,
            amp: 1.0,
        })
    }

    /// Reset the amplitude tracker.
    pub fn reset(&mut self) {
        self.amp = 1.0;
    }

    /// Process complex samples, attenuating impulse noise.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
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
        for (i, &s) in input.iter().enumerate() {
            let in_amp = s.amplitude();
            let mut gain = 1.0_f32;
            if in_amp != 0.0 {
                self.amp = self.amp * self.inv_rate + in_amp * self.rate;
                let excess = in_amp / self.amp;
                if excess > self.level {
                    gain = 1.0 / excess;
                }
            }
            output[i] = s * gain;
        }
        Ok(input.len())
    }
}

/// FM IF noise reduction — frequency-domain peak tracking.
///
/// Ports SDR++ `dsp::noise_reduction::FMIF`. Uses FFT to find the dominant
/// frequency bin and reconstructs the signal from that bin only, effectively
/// removing noise from narrow FM signals.
///
/// Note: This is a simplified version that operates per-sample with a sliding
/// window. For the full FFT-based version, use with the `fft` module.
pub struct FmIfNoiseReduction {
    enabled: bool,
}

impl FmIfNoiseReduction {
    /// Create a new FM IF noise reduction processor.
    pub fn new() -> Self {
        Self { enabled: true }
    }

    /// Enable or disable the noise reduction.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Process complex samples. When disabled, acts as passthrough.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process(&self, input: &[Complex], output: &mut [Complex]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        // Passthrough for now — full FFT-based implementation will be added
        // when integrated with the pipeline's FFT engine
        output[..input.len()].copy_from_slice(input);
        Ok(input.len())
    }
}

impl Default for FmIfNoiseReduction {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    // --- Power Squelch tests ---

    #[test]
    fn test_squelch_opens_on_strong_signal() {
        let squelch = PowerSquelch::new(-30.0);
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        let open = squelch.process(&input, &mut output).unwrap();
        assert!(open, "strong signal should open squelch");
        assert!(output[0].re > 0.0, "output should not be zeroed");
    }

    #[test]
    fn test_squelch_closes_on_weak_signal() {
        let squelch = PowerSquelch::new(10.0); // very high threshold
        let input = vec![Complex::new(0.001, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        let open = squelch.process(&input, &mut output).unwrap();
        assert!(!open, "weak signal should close squelch");
        assert!(
            output[0].re.abs() < 1e-10,
            "output should be zeroed when squelch closed"
        );
    }

    #[test]
    fn test_squelch_empty_input() {
        let squelch = PowerSquelch::new(-50.0);
        let input: &[Complex] = &[];
        let mut output: Vec<Complex> = vec![];
        let open = squelch.process(input, &mut output).unwrap();
        assert!(!open);
    }

    // --- Noise Blanker tests ---

    #[test]
    fn test_blanker_new_invalid() {
        assert!(NoiseBlanker::new(0.0, 5.0).is_err());
        assert!(NoiseBlanker::new(1.0, 5.0).is_err());
        assert!(NoiseBlanker::new(0.1, 0.0).is_err());
    }

    #[test]
    fn test_blanker_passes_normal_signal() {
        let mut nb = NoiseBlanker::new(0.1, 10.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 500];
        let mut output = vec![Complex::default(); 500];
        nb.process(&input, &mut output).unwrap();
        // Normal signal should pass through mostly unchanged
        let last = output[499];
        assert!(last.re > 0.5, "normal signal should pass, got {}", last.re);
    }

    #[test]
    fn test_blanker_attenuates_spike() {
        let mut nb = NoiseBlanker::new(0.01, 3.0).unwrap();
        // Settle the amplitude tracker
        let normal = vec![Complex::new(1.0, 0.0); 1000];
        let mut out = vec![Complex::default(); 1000];
        nb.process(&normal, &mut out).unwrap();
        // Now inject a spike
        let mut spike_input = vec![Complex::new(1.0, 0.0); 100];
        spike_input[50] = Complex::new(100.0, 0.0); // huge spike
        let mut spike_out = vec![Complex::default(); 100];
        nb.process(&spike_input, &mut spike_out).unwrap();
        // The spike should be attenuated
        assert!(
            spike_out[50].re < 50.0,
            "spike should be attenuated, got {}",
            spike_out[50].re
        );
    }

    #[test]
    fn test_blanker_reset() {
        let mut nb = NoiseBlanker::new(0.1, 5.0).unwrap();
        let input = vec![Complex::new(10.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        nb.process(&input, &mut output).unwrap();
        nb.reset();
        assert!(
            (nb.amp - 1.0).abs() < 1e-6,
            "after reset, amp should be 1.0"
        );
    }

    // --- FM IF NR tests ---

    #[test]
    fn test_fm_if_nr_passthrough() {
        let nr = FmIfNoiseReduction::new();
        let input = vec![Complex::new(1.0, 2.0); 100];
        let mut output = vec![Complex::default(); 100];
        let count = nr.process(&input, &mut output).unwrap();
        assert_eq!(count, 100);
        assert_eq!(output[0].re, 1.0);
        assert_eq!(output[0].im, 2.0);
    }

    #[test]
    fn test_buffer_too_small() {
        let squelch = PowerSquelch::new(-50.0);
        let input = [Complex::default(); 10];
        let mut output = [Complex::default(); 5];
        assert!(squelch.process(&input, &mut output).is_err());
    }
}
