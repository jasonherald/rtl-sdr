//! FIR and IIR filter implementations.
//!
//! Ports SDR++ `dsp::filter` namespace. These are stateful processors
//! that maintain internal delay line buffers.

use sdr_types::{Complex, DspError};

/// FIR (Finite Impulse Response) filter with f32 taps.
///
/// Ports SDR++ `dsp::filter::FIR`. Uses a delay line buffer and
/// dot product convolution. Supports f32 and Complex data types
/// through the `FirProcess` trait.
pub struct FirFilter {
    taps: Vec<f32>,
    delay_line: Vec<f32>,
}

impl FirFilter {
    /// Create a new FIR filter with the given taps.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `taps` is empty.
    pub fn new(taps: Vec<f32>) -> Result<Self, DspError> {
        if taps.is_empty() {
            return Err(DspError::InvalidParameter(
                "FIR taps must not be empty".to_string(),
            ));
        }
        let delay_line = vec![0.0; taps.len() - 1];
        Ok(Self { taps, delay_line })
    }

    /// Replace the filter taps. Resets the delay line.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `taps` is empty.
    pub fn set_taps(&mut self, taps: Vec<f32>) -> Result<(), DspError> {
        if taps.is_empty() {
            return Err(DspError::InvalidParameter(
                "FIR taps must not be empty".to_string(),
            ));
        }
        self.delay_line = vec![0.0; taps.len() - 1];
        self.taps = taps;
        Ok(())
    }

    /// Reset the delay line to zero.
    pub fn reset(&mut self) {
        self.delay_line.fill(0.0);
    }

    /// Number of taps (filter order + 1).
    pub fn tap_count(&self) -> usize {
        self.taps.len()
    }

    /// Process f32 samples through the filter.
    ///
    /// `input` and `output` must have the same length. Returns the
    /// number of samples written (always `input.len()`).
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process_f32(&mut self, input: &[f32], output: &mut [f32]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        let tap_count = self.taps.len();
        let delay_len = tap_count - 1;

        #[allow(clippy::needless_range_loop)]
        for i in 0..input.len() {
            // Compute dot product over delay line + current input window
            let mut acc = 0.0_f32;
            for (j, &tap) in self.taps.iter().enumerate() {
                let sample_idx = i + delay_len - j;
                let val = if sample_idx < delay_len {
                    self.delay_line[sample_idx]
                } else {
                    input[sample_idx - delay_len]
                };
                acc += val * tap;
            }
            output[i] = acc;
        }

        // Update delay line: keep the last (tap_count - 1) input samples
        if input.len() >= delay_len {
            self.delay_line
                .copy_from_slice(&input[input.len() - delay_len..]);
        } else {
            // Shift delay line left and append new input
            let shift = delay_len - input.len();
            self.delay_line.copy_within(input.len().., 0);
            self.delay_line[shift..].copy_from_slice(input);
        }

        Ok(input.len())
    }

    /// Process Complex samples through the filter (real taps).
    ///
    /// Each complex sample's re and im are independently convolved with the taps.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process_complex(
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

        // We need a complex delay line — reinterpret our f32 delay line as pairs
        // For simplicity, use a separate complex processing path
        let tap_count = self.taps.len();
        let delay_len = tap_count - 1;

        // Ensure delay line is sized for complex (2 floats per sample)
        let needed = delay_len * 2;
        if self.delay_line.len() != needed {
            self.delay_line.resize(needed, 0.0);
        }

        #[allow(clippy::needless_range_loop)]
        for i in 0..input.len() {
            let mut acc_re = 0.0_f32;
            let mut acc_im = 0.0_f32;
            for (j, &tap) in self.taps.iter().enumerate() {
                let sample_idx = i + delay_len - j;
                let (val_re, val_im) = if sample_idx < delay_len {
                    (
                        self.delay_line[sample_idx * 2],
                        self.delay_line[sample_idx * 2 + 1],
                    )
                } else {
                    let s = input[sample_idx - delay_len];
                    (s.re, s.im)
                };
                acc_re += val_re * tap;
                acc_im += val_im * tap;
            }
            output[i] = Complex::new(acc_re, acc_im);
        }

        // Update delay line
        if input.len() >= delay_len {
            for (k, &s) in input[input.len() - delay_len..].iter().enumerate() {
                self.delay_line[k * 2] = s.re;
                self.delay_line[k * 2 + 1] = s.im;
            }
        } else {
            let shift = delay_len - input.len();
            self.delay_line.copy_within(input.len() * 2.., 0);
            for (k, &s) in input.iter().enumerate() {
                self.delay_line[(shift + k) * 2] = s.re;
                self.delay_line[(shift + k) * 2 + 1] = s.im;
            }
        }

        Ok(input.len())
    }
}

/// Decimating FIR filter — applies FIR filtering with downsampling.
///
/// Ports SDR++ `dsp::filter::DecimatingFIR`. Outputs one sample for every
/// `decimation` input samples, filtered by the FIR taps.
pub struct DecimatingFirFilter {
    taps: Vec<f32>,
    delay_line: Vec<f32>,
    decimation: usize,
    offset: usize,
}

impl DecimatingFirFilter {
    /// Create a new decimating FIR filter.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `taps` is empty or `decimation` is 0.
    pub fn new(taps: Vec<f32>, decimation: usize) -> Result<Self, DspError> {
        if taps.is_empty() {
            return Err(DspError::InvalidParameter(
                "FIR taps must not be empty".to_string(),
            ));
        }
        if decimation == 0 {
            return Err(DspError::InvalidParameter(
                "decimation must be > 0".to_string(),
            ));
        }
        let delay_line = vec![0.0; taps.len() - 1];
        Ok(Self {
            taps,
            delay_line,
            decimation,
            offset: 0,
        })
    }

    /// Reset the delay line and offset.
    pub fn reset(&mut self) {
        self.delay_line.fill(0.0);
        self.offset = 0;
    }

    /// Process f32 samples with decimation.
    ///
    /// Returns the number of output samples written (`input.len()` / decimation, approximately).
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output` is too small.
    pub fn process_f32(&mut self, input: &[f32], output: &mut [f32]) -> Result<usize, DspError> {
        let max_out = input.len().div_ceil(self.decimation);
        if output.len() < max_out {
            return Err(DspError::BufferTooSmall {
                need: max_out,
                got: output.len(),
            });
        }

        let tap_count = self.taps.len();
        let delay_len = tap_count - 1;
        let mut out_count = 0;

        while self.offset < input.len() {
            let mut acc = 0.0_f32;
            for (j, &tap) in self.taps.iter().enumerate() {
                let sample_idx = self.offset + delay_len - j;
                let val = if sample_idx < delay_len {
                    self.delay_line[sample_idx]
                } else {
                    input[sample_idx - delay_len]
                };
                acc += val * tap;
            }
            output[out_count] = acc;
            out_count += 1;
            self.offset += self.decimation;
        }
        self.offset -= input.len();

        // Update delay line
        if input.len() >= delay_len {
            self.delay_line
                .copy_from_slice(&input[input.len() - delay_len..]);
        } else {
            let shift = delay_len - input.len();
            self.delay_line.copy_within(input.len().., 0);
            self.delay_line[shift..].copy_from_slice(input);
        }

        Ok(out_count)
    }
}

/// FM deemphasis filter — single-pole IIR lowpass.
///
/// Ports SDR++ `dsp::filter::Deemphasis`. Implements:
/// `y[n] = alpha * x[n] + (1 - alpha) * y[n-1]`
/// where `alpha = dt / (tau + dt)` and `dt = 1 / sample_rate`.
///
/// Standard time constants: 75us (US/Japan) or 50us (Europe/Australia).
pub struct DeemphasisFilter {
    alpha: f32,
    one_minus_alpha: f32,
    last_out: f32,
}

/// Deemphasis time constant for US/Japan FM broadcast (75 microseconds).
pub const DEEMPHASIS_TAU_US: f64 = 75e-6;

/// Deemphasis time constant for Europe/Australia FM broadcast (50 microseconds).
pub const DEEMPHASIS_TAU_EU: f64 = 50e-6;

impl DeemphasisFilter {
    /// Create a new deemphasis filter.
    ///
    /// - `tau`: time constant in seconds (e.g., 75e-6 for US FM)
    /// - `sample_rate`: sample rate in Hz
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if parameters are non-positive or non-finite.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(tau: f64, sample_rate: f64) -> Result<Self, DspError> {
        if !tau.is_finite() || tau <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "tau must be positive and finite, got {tau}"
            )));
        }
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "sample_rate must be positive and finite, got {sample_rate}"
            )));
        }
        let dt = 1.0 / sample_rate;
        let alpha = (dt / (tau + dt)) as f32;
        Ok(Self {
            alpha,
            one_minus_alpha: 1.0 - alpha,
            last_out: 0.0,
        })
    }

    /// Update the time constant.
    #[allow(clippy::cast_possible_truncation)]
    pub fn set_tau(&mut self, tau: f64, sample_rate: f64) {
        let dt = 1.0 / sample_rate;
        self.alpha = (dt / (tau + dt)) as f32;
        self.one_minus_alpha = 1.0 - self.alpha;
    }

    /// Reset the filter state.
    pub fn reset(&mut self) {
        self.last_out = 0.0;
    }

    /// Process f32 samples through the deemphasis filter.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        if input.is_empty() {
            return Ok(0);
        }

        output[0] = self.alpha * input[0] + self.one_minus_alpha * self.last_out;
        for i in 1..input.len() {
            output[i] = self.alpha * input[i] + self.one_minus_alpha * output[i - 1];
        }
        self.last_out = output[input.len() - 1];

        Ok(input.len())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    // --- FIR filter tests ---

    #[test]
    fn test_fir_new_empty_taps() {
        assert!(FirFilter::new(vec![]).is_err());
    }

    #[test]
    fn test_fir_identity() {
        // Single tap of 1.0 = identity filter (passthrough)
        let mut fir = FirFilter::new(vec![1.0]).unwrap();
        let input = [1.0, 2.0, 3.0, 4.0, 5.0];
        let mut output = [0.0_f32; 5];
        fir.process_f32(&input, &mut output).unwrap();
        for i in 0..5 {
            assert!(
                (output[i] - input[i]).abs() < 1e-6,
                "identity filter: output[{i}] = {}, expected {}",
                output[i],
                input[i]
            );
        }
    }

    #[test]
    fn test_fir_delay() {
        // Tap at position 1 = one-sample delay: [0, 1]
        let mut fir = FirFilter::new(vec![0.0, 1.0]).unwrap();
        let input = [1.0, 2.0, 3.0, 4.0];
        let mut output = [0.0_f32; 4];
        fir.process_f32(&input, &mut output).unwrap();
        // Output should be delayed by one sample (first output = 0 from delay line)
        assert!((output[0] - 0.0).abs() < 1e-6);
        assert!((output[1] - 1.0).abs() < 1e-6);
        assert!((output[2] - 2.0).abs() < 1e-6);
        assert!((output[3] - 3.0).abs() < 1e-6);
    }

    #[test]
    fn test_fir_averaging() {
        // 3-tap averaging filter [1/3, 1/3, 1/3]
        let mut fir = FirFilter::new(vec![1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0]).unwrap();
        let input = [3.0, 3.0, 3.0, 3.0];
        let mut output = [0.0_f32; 4];
        fir.process_f32(&input, &mut output).unwrap();
        // After the filter fills, output should be 3.0
        assert!((output[2] - 3.0).abs() < 1e-5);
        assert!((output[3] - 3.0).abs() < 1e-5);
    }

    #[test]
    fn test_fir_continuity_across_blocks() {
        // Process in two blocks, verify continuity via delay line
        let mut fir = FirFilter::new(vec![0.0, 1.0]).unwrap();
        let block1 = [1.0, 2.0];
        let block2 = [3.0, 4.0];
        let mut out1 = [0.0_f32; 2];
        let mut out2 = [0.0_f32; 2];
        fir.process_f32(&block1, &mut out1).unwrap();
        fir.process_f32(&block2, &mut out2).unwrap();
        // out2[0] should be last sample of block1 (delayed)
        assert!((out2[0] - 2.0).abs() < 1e-6);
        assert!((out2[1] - 3.0).abs() < 1e-6);
    }

    #[test]
    fn test_fir_complex() {
        let mut fir = FirFilter::new(vec![1.0]).unwrap();
        let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
        let mut output = [Complex::default(); 2];
        fir.process_complex(&input, &mut output).unwrap();
        assert!((output[0].re - 1.0).abs() < 1e-6);
        assert!((output[0].im - 2.0).abs() < 1e-6);
        assert!((output[1].re - 3.0).abs() < 1e-6);
        assert!((output[1].im - 4.0).abs() < 1e-6);
    }

    #[test]
    fn test_fir_buffer_too_small() {
        let mut fir = FirFilter::new(vec![1.0]).unwrap();
        let input = [1.0, 2.0, 3.0];
        let mut output = [0.0_f32; 2];
        assert!(fir.process_f32(&input, &mut output).is_err());
    }

    // --- Decimating FIR tests ---

    #[test]
    fn test_decimating_fir_new_invalid() {
        assert!(DecimatingFirFilter::new(vec![], 2).is_err());
        assert!(DecimatingFirFilter::new(vec![1.0], 0).is_err());
    }

    #[test]
    fn test_decimating_fir_by_2() {
        // Identity taps, decimate by 2
        let mut fir = DecimatingFirFilter::new(vec![1.0], 2).unwrap();
        let input = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let mut output = [0.0_f32; 3];
        let count = fir.process_f32(&input, &mut output).unwrap();
        assert_eq!(count, 3);
        assert!((output[0] - 1.0).abs() < 1e-6);
        assert!((output[1] - 3.0).abs() < 1e-6);
        assert!((output[2] - 5.0).abs() < 1e-6);
    }

    #[test]
    fn test_decimating_fir_by_4() {
        let mut fir = DecimatingFirFilter::new(vec![1.0], 4).unwrap();
        let input = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let mut output = [0.0_f32; 2];
        let count = fir.process_f32(&input, &mut output).unwrap();
        assert_eq!(count, 2);
        assert!((output[0] - 1.0).abs() < 1e-6);
        assert!((output[1] - 5.0).abs() < 1e-6);
    }

    // --- Deemphasis filter tests ---

    #[test]
    fn test_deemphasis_new_invalid() {
        assert!(DeemphasisFilter::new(0.0, 48_000.0).is_err());
        assert!(DeemphasisFilter::new(-1.0, 48_000.0).is_err());
        assert!(DeemphasisFilter::new(75e-6, 0.0).is_err());
        assert!(DeemphasisFilter::new(f64::NAN, 48_000.0).is_err());
    }

    #[test]
    fn test_deemphasis_dc_passthrough() {
        // DC signal should pass through unchanged (IIR converges to input)
        let mut deemph = DeemphasisFilter::new(DEEMPHASIS_TAU_US, 48_000.0).unwrap();
        let input = vec![1.0_f32; 1000];
        let mut output = vec![0.0_f32; 1000];
        deemph.process(&input, &mut output).unwrap();
        // After settling, output should approach 1.0
        assert!(
            (output[999] - 1.0).abs() < 0.01,
            "DC should converge to 1.0, got {}",
            output[999]
        );
    }

    #[test]
    fn test_deemphasis_high_freq_attenuation() {
        // High frequency signal should be attenuated
        let mut deemph = DeemphasisFilter::new(DEEMPHASIS_TAU_US, 48_000.0).unwrap();
        let input: Vec<f32> = (0..1000)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let mut output = vec![0.0_f32; 1000];
        deemph.process(&input, &mut output).unwrap();
        // Peak output should be much less than peak input
        let peak_out = output[500..]
            .iter()
            .map(|x| x.abs())
            .fold(0.0_f32, f32::max);
        assert!(
            peak_out < 0.5,
            "high freq should be attenuated, peak = {peak_out}"
        );
    }

    #[test]
    fn test_deemphasis_reset() {
        let mut deemph = DeemphasisFilter::new(DEEMPHASIS_TAU_US, 48_000.0).unwrap();
        let input = [1.0_f32; 100];
        let mut output = [0.0_f32; 100];
        deemph.process(&input, &mut output).unwrap();
        deemph.reset();
        // After reset, processing zeros should give zeros
        let zeros = [0.0_f32; 10];
        let mut out2 = [0.0_f32; 10];
        deemph.process(&zeros, &mut out2).unwrap();
        assert!((out2[0]).abs() < 1e-6, "after reset, output should be 0");
    }

    #[test]
    fn test_deemphasis_buffer_too_small() {
        let mut deemph = DeemphasisFilter::new(DEEMPHASIS_TAU_US, 48_000.0).unwrap();
        let input = [1.0_f32; 10];
        let mut output = [0.0_f32; 5];
        assert!(deemph.process(&input, &mut output).is_err());
    }
}
