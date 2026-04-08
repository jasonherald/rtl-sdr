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

    /// Replace the filter taps, preserving delay line data for seamless transition.
    ///
    /// Matches C++ SDR++ `FIR::setTaps` — avoids clicks/transients during live
    /// bandwidth adjustments by keeping existing delay line samples.
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
        let new_delay_len = taps.len() - 1;
        let old_delay_len = self.delay_line.len();
        if new_delay_len != old_delay_len {
            let mut new_delay = vec![0.0_f32; new_delay_len];
            // Copy existing delay data, aligned to the end (most recent samples)
            let copy_len = old_delay_len.min(new_delay_len);
            let src_start = old_delay_len.saturating_sub(copy_len);
            let dst_start = new_delay_len - copy_len;
            new_delay[dst_start..].copy_from_slice(&self.delay_line[src_start..]);
            self.delay_line = new_delay;
        }
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
}

/// FIR filter for Complex samples with real (f32) taps.
///
/// Separate from `FirFilter` to avoid state corruption from mixing
/// f32 and Complex processing on the same delay line.
pub struct ComplexFirFilter {
    taps: Vec<f32>,
    delay_line: Vec<Complex>,
}

impl ComplexFirFilter {
    /// Create a new complex FIR filter with the given real taps.
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
        let delay_line = vec![Complex::default(); taps.len() - 1];
        Ok(Self { taps, delay_line })
    }

    /// Replace the filter taps, preserving delay line data for seamless transition.
    ///
    /// Matches C++ SDR++ `FIR::setTaps` — avoids clicks/transients during live
    /// bandwidth adjustments by keeping existing delay line samples.
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
        let new_delay_len = taps.len() - 1;
        let old_delay_len = self.delay_line.len();
        if new_delay_len != old_delay_len {
            let mut new_delay = vec![Complex::default(); new_delay_len];
            let copy_len = old_delay_len.min(new_delay_len);
            let src_start = old_delay_len.saturating_sub(copy_len);
            let dst_start = new_delay_len - copy_len;
            new_delay[dst_start..].copy_from_slice(&self.delay_line[src_start..]);
            self.delay_line = new_delay;
        }
        self.taps = taps;
        Ok(())
    }

    /// Reset the delay line to zero.
    pub fn reset(&mut self) {
        self.delay_line.fill(Complex::default());
    }

    /// Number of taps.
    pub fn tap_count(&self) -> usize {
        self.taps.len()
    }

    /// Process Complex samples through the filter.
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

        let delay_len = self.taps.len() - 1;

        #[allow(clippy::needless_range_loop)]
        for i in 0..input.len() {
            let mut acc_re = 0.0_f32;
            let mut acc_im = 0.0_f32;
            for (j, &tap) in self.taps.iter().enumerate() {
                let sample_idx = i + delay_len - j;
                let val = if sample_idx < delay_len {
                    self.delay_line[sample_idx]
                } else {
                    input[sample_idx - delay_len]
                };
                acc_re += val.re * tap;
                acc_im += val.im * tap;
            }
            output[i] = Complex::new(acc_re, acc_im);
        }

        // Update delay line
        if input.len() >= delay_len {
            self.delay_line
                .copy_from_slice(&input[input.len() - delay_len..]);
        } else {
            let shift = delay_len - input.len();
            self.delay_line.copy_within(input.len().., 0);
            self.delay_line[shift..].copy_from_slice(input);
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
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if parameters are non-positive or non-finite.
    #[allow(clippy::cast_possible_truncation)]
    pub fn set_tau(&mut self, tau: f64, sample_rate: f64) -> Result<(), DspError> {
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
        self.alpha = (dt / (tau + dt)) as f32;
        self.one_minus_alpha = 1.0 - self.alpha;
        Ok(())
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

/// Default notch filter quality factor (narrow notch).
const DEFAULT_NOTCH_Q: f32 = 30.0;

/// Default notch filter frequency in Hz (US power line hum).
pub const DEFAULT_NOTCH_FREQ_HZ: f32 = 60.0;

/// IIR notch (band-reject) filter — second-order biquad.
///
/// Removes a narrow frequency band from the signal. Useful for eliminating
/// interference tones such as 50/60 Hz power line hum or carrier tones.
///
/// Coefficients follow the Audio EQ Cookbook (Robert Bristow-Johnson):
/// ```text
/// w0 = 2*pi*freq/sample_rate
/// alpha = sin(w0) / (2*Q)
/// b0 = 1,  b1 = -2*cos(w0),  b2 = 1
/// a0 = 1 + alpha,  a1 = -2*cos(w0),  a2 = 1 - alpha
/// ```
/// All coefficients are normalized by `a0`.
pub struct NotchFilter {
    // Normalized biquad coefficients (divided by a0).
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    // Filter state (Direct Form I).
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
    // Configuration.
    enabled: bool,
    frequency: f32,
    sample_rate: f32,
    q: f32,
}

impl NotchFilter {
    /// Create a new disabled notch filter for the given sample rate.
    ///
    /// Default frequency is 60 Hz (US power line hum), Q = 30.
    pub fn new(sample_rate: f32) -> Self {
        let mut filter = Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
            enabled: false,
            frequency: DEFAULT_NOTCH_FREQ_HZ,
            sample_rate,
            q: DEFAULT_NOTCH_Q,
        };
        filter.recalculate_coefficients();
        filter
    }

    /// Set the notch frequency in Hz and recalculate coefficients.
    pub fn set_frequency(&mut self, freq: f32) {
        self.frequency = freq;
        self.recalculate_coefficients();
        self.reset();
    }

    /// Enable or disable the notch filter. Resets biquad state on re-enable
    /// to prevent pops from stale history.
    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled && !self.enabled {
            self.reset();
        }
        self.enabled = enabled;
    }

    /// Returns whether the notch filter is currently enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the current notch frequency in Hz.
    pub fn frequency(&self) -> f32 {
        self.frequency
    }

    /// Reset the filter state (delay elements) to zero.
    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }

    /// Process f32 samples through the notch filter.
    ///
    /// When disabled, copies input to output unchanged.
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

        if !self.enabled {
            output[..input.len()].copy_from_slice(input);
            return Ok(input.len());
        }

        for (i, &x) in input.iter().enumerate() {
            let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
                - self.a1 * self.y1
                - self.a2 * self.y2;
            self.x2 = self.x1;
            self.x1 = x;
            self.y2 = self.y1;
            self.y1 = y;
            output[i] = y;
        }

        Ok(input.len())
    }

    /// Recalculate biquad coefficients from frequency, sample rate, and Q.
    fn recalculate_coefficients(&mut self) {
        let w0 = core::f32::consts::TAU * self.frequency / self.sample_rate;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * self.q);

        let a0 = 1.0 + alpha;

        // Normalize all coefficients by a0.
        self.b0 = 1.0 / a0;
        self.b1 = (-2.0 * cos_w0) / a0;
        self.b2 = 1.0 / a0;
        self.a1 = (-2.0 * cos_w0) / a0;
        self.a2 = (1.0 - alpha) / a0;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp, clippy::cast_precision_loss)]
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
    fn test_complex_fir_identity() {
        let mut fir = ComplexFirFilter::new(vec![1.0]).unwrap();
        let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
        let mut output = [Complex::default(); 2];
        fir.process(&input, &mut output).unwrap();
        assert!((output[0].re - 1.0).abs() < 1e-6);
        assert!((output[0].im - 2.0).abs() < 1e-6);
        assert!((output[1].re - 3.0).abs() < 1e-6);
        assert!((output[1].im - 4.0).abs() < 1e-6);
    }

    #[test]
    fn test_complex_fir_delay() {
        let mut fir = ComplexFirFilter::new(vec![0.0, 1.0]).unwrap();
        let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
        let mut output = [Complex::default(); 2];
        fir.process(&input, &mut output).unwrap();
        assert!((output[0].re).abs() < 1e-6);
        assert!((output[0].im).abs() < 1e-6);
        assert!((output[1].re - 1.0).abs() < 1e-6);
        assert!((output[1].im - 2.0).abs() < 1e-6);
    }

    #[test]
    fn test_complex_fir_set_taps() {
        let mut fir = ComplexFirFilter::new(vec![1.0]).unwrap();
        // Identity -> 2-tap delay filter
        fir.set_taps(vec![0.0, 1.0]).unwrap();
        assert_eq!(fir.tap_count(), 2);
        let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
        let mut output = [Complex::default(); 2];
        fir.process(&input, &mut output).unwrap();
        // First output uses delay line (zero-extended since old had 0 delay taps)
        assert!((output[0].re).abs() < 1e-6);
        assert!((output[1].re - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_fir_set_taps_preserves_delay() {
        // Process a block so delay line has data, then swap taps
        let mut fir = FirFilter::new(vec![0.0, 1.0]).unwrap();
        let input = [10.0, 20.0, 30.0];
        let mut output = [0.0_f32; 3];
        fir.process_f32(&input, &mut output).unwrap();
        // delay line now holds [30.0]

        // Swap to a 3-tap filter — delay line should keep 30.0
        fir.set_taps(vec![0.0, 0.0, 1.0]).unwrap();
        let input2 = [40.0, 50.0];
        let mut output2 = [0.0_f32; 2];
        fir.process_f32(&input2, &mut output2).unwrap();
        // output2[0] should be delay[0] = 0.0 (zero-extended), output2[1] = 30.0 (preserved)
        assert!(
            (output2[0]).abs() < 1e-6,
            "zero-extended position, got {}",
            output2[0]
        );
        assert!(
            (output2[1] - 30.0).abs() < 1e-6,
            "preserved delay sample, got {}",
            output2[1]
        );
    }

    #[test]
    fn test_fir_set_taps_same_length() {
        // Same length swap should keep delay line intact
        let mut fir = FirFilter::new(vec![0.0, 1.0]).unwrap();
        let input = [5.0, 10.0];
        let mut output = [0.0_f32; 2];
        fir.process_f32(&input, &mut output).unwrap();
        // delay line = [10.0]

        // Swap to identity (same 2-tap count) — delay stays [10.0]
        fir.set_taps(vec![1.0, 0.0]).unwrap();
        let input2 = [20.0];
        let mut output2 = [0.0_f32; 1];
        fir.process_f32(&input2, &mut output2).unwrap();
        // With taps [1.0, 0.0]: output = 1.0*20.0 + 0.0*10.0 = 20.0
        assert!(
            (output2[0] - 20.0).abs() < 1e-6,
            "same-length tap swap, got {}",
            output2[0]
        );
    }

    #[test]
    fn test_complex_fir_set_taps_empty() {
        let mut fir = ComplexFirFilter::new(vec![1.0]).unwrap();
        assert!(fir.set_taps(vec![]).is_err());
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

    // --- Notch filter tests ---

    #[test]
    fn test_notch_new_defaults() {
        let notch = NotchFilter::new(48_000.0);
        assert!(!notch.enabled());
        assert!((notch.frequency() - 60.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_notch_disabled_passthrough() {
        let mut notch = NotchFilter::new(48_000.0);
        // Filter is disabled by default
        let input = [1.0, 2.0, 3.0, 4.0, 5.0];
        let mut output = [0.0_f32; 5];
        let count = notch.process(&input, &mut output).unwrap();
        assert_eq!(count, 5);
        for i in 0..5 {
            assert!(
                (output[i] - input[i]).abs() < 1e-6,
                "disabled notch should passthrough: output[{i}] = {}, expected {}",
                output[i],
                input[i]
            );
        }
    }

    #[test]
    fn test_notch_buffer_too_small() {
        let mut notch = NotchFilter::new(48_000.0);
        notch.set_enabled(true);
        let input = [1.0_f32; 10];
        let mut output = [0.0_f32; 5];
        assert!(notch.process(&input, &mut output).is_err());
    }

    #[test]
    fn test_notch_coefficients_symmetry() {
        // For a notch filter, b0 == b2 and b1 == a1 (before normalization).
        // After normalization by a0: b0 = 1/a0, b2 = 1/a0, b1 = -2cos(w0)/a0, a1 = -2cos(w0)/a0.
        let notch = NotchFilter::new(48_000.0);
        assert!(
            (notch.b0 - notch.b2).abs() < 1e-6,
            "b0 ({}) should equal b2 ({})",
            notch.b0,
            notch.b2
        );
        assert!(
            (notch.b1 - notch.a1).abs() < 1e-6,
            "b1 ({}) should equal a1 ({})",
            notch.b1,
            notch.a1
        );
    }

    #[test]
    fn test_notch_attenuates_target_frequency() {
        // Generate a 60 Hz sine wave at 48 kHz sample rate.
        let sample_rate = 48_000.0_f32;
        let freq = 60.0_f32;
        let num_samples = 48_000; // 1 second of audio
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (core::f32::consts::TAU * freq * (i as f32) / sample_rate).sin())
            .collect();
        let mut output = vec![0.0_f32; num_samples];

        let mut notch = NotchFilter::new(sample_rate);
        notch.set_frequency(freq);
        notch.set_enabled(true);
        notch.process(&input, &mut output).unwrap();

        // Measure RMS of the last half (after settling)
        let rms_in: f32 = (input[num_samples / 2..].iter().map(|x| x * x).sum::<f32>()
            / (num_samples / 2) as f32)
            .sqrt();
        let rms_out: f32 = (output[num_samples / 2..].iter().map(|x| x * x).sum::<f32>()
            / (num_samples / 2) as f32)
            .sqrt();

        // The notch should attenuate the target frequency by at least 25 dB.
        // At Q=30, 60 Hz, 48 kHz sample rate, the biquad achieves ~32 dB rejection.
        let attenuation_db = 20.0 * (rms_out / rms_in).log10();
        assert!(
            attenuation_db < -25.0,
            "60 Hz should be attenuated by >25 dB, got {attenuation_db:.1} dB"
        );
    }

    #[test]
    fn test_notch_passes_other_frequencies() {
        // Generate a 1000 Hz sine wave — should NOT be attenuated by a 60 Hz notch.
        let sample_rate = 48_000.0_f32;
        let freq = 1000.0_f32;
        let num_samples = 48_000;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (core::f32::consts::TAU * freq * (i as f32) / sample_rate).sin())
            .collect();
        let mut output = vec![0.0_f32; num_samples];

        let mut notch = NotchFilter::new(sample_rate);
        notch.set_frequency(60.0);
        notch.set_enabled(true);
        notch.process(&input, &mut output).unwrap();

        // Measure RMS of the last half
        let rms_in: f32 = (input[num_samples / 2..].iter().map(|x| x * x).sum::<f32>()
            / (num_samples / 2) as f32)
            .sqrt();
        let rms_out: f32 = (output[num_samples / 2..].iter().map(|x| x * x).sum::<f32>()
            / (num_samples / 2) as f32)
            .sqrt();

        // 1000 Hz should pass through with minimal attenuation (< 1 dB)
        let attenuation_db = 20.0 * (rms_out / rms_in).log10();
        assert!(
            attenuation_db > -1.0,
            "1000 Hz should pass with < 1 dB loss, got {attenuation_db:.2} dB"
        );
    }

    #[test]
    fn test_notch_reset_clears_state() {
        let mut notch = NotchFilter::new(48_000.0);
        notch.set_enabled(true);
        // Process some data to build up state
        let input = [1.0_f32; 100];
        let mut output = [0.0_f32; 100];
        notch.process(&input, &mut output).unwrap();

        notch.reset();
        // After reset, processing zeros should produce zeros
        let zeros = [0.0_f32; 10];
        let mut out2 = [0.0_f32; 10];
        notch.process(&zeros, &mut out2).unwrap();
        for (i, &v) in out2.iter().enumerate() {
            assert!(
                v.abs() < 1e-6,
                "after reset, output[{i}] should be ~0, got {v}"
            );
        }
    }

    #[test]
    fn test_notch_set_frequency_updates_coefficients() {
        let mut notch = NotchFilter::new(48_000.0);
        let old_b1 = notch.b1;
        notch.set_frequency(1000.0);
        // Coefficients should change when frequency changes
        assert!(
            (notch.b1 - old_b1).abs() > 1e-6,
            "b1 should change after set_frequency"
        );
        assert!((notch.frequency() - 1000.0).abs() < f32::EPSILON);
    }
}
