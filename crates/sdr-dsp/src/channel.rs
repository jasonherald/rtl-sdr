//! Channel processing: frequency translation and VFO extraction.
//!
//! Ports SDR++ `dsp::channel` namespace.

use sdr_types::{Complex, DspError};

use crate::filter::ComplexFirFilter;
use crate::math;
use crate::multirate::RationalResampler;
use crate::taps;

/// Frequency translator — shifts a signal in frequency using an NCO.
///
/// Ports SDR++ `dsp::channel::FrequencyXlator`. Multiplies each input
/// sample by a rotating complex phasor at the offset frequency, effectively
/// shifting the spectrum.
pub struct FrequencyXlator {
    phase: f32,
    phase_delta: f32,
}

impl FrequencyXlator {
    /// Create a new frequency translator.
    ///
    /// - `offset`: frequency offset in radians/sample
    pub fn new(offset: f32) -> Self {
        Self {
            phase: 0.0,
            phase_delta: offset,
        }
    }

    /// Create from offset in Hz and sample rate.
    #[allow(clippy::cast_possible_truncation)]
    pub fn from_hz(offset_hz: f64, sample_rate: f64) -> Self {
        Self::new(math::hz_to_rads(offset_hz, sample_rate) as f32)
    }

    /// Update the frequency offset (radians/sample).
    pub fn set_offset(&mut self, offset: f32) {
        self.phase_delta = offset;
    }

    /// Update the frequency offset from Hz.
    #[allow(clippy::cast_possible_truncation)]
    pub fn set_offset_hz(&mut self, offset_hz: f64, sample_rate: f64) {
        self.phase_delta = math::hz_to_rads(offset_hz, sample_rate) as f32;
    }

    /// Reset the NCO phase to zero.
    pub fn reset(&mut self) {
        self.phase = 0.0;
    }

    /// Process complex samples, shifting them in frequency.
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
            let (sin, cos) = self.phase.sin_cos();
            let phasor = Complex::new(cos, sin);
            output[i] = s * phasor;
            self.phase += self.phase_delta;
            // Wrap phase to prevent float precision loss over time
            self.phase = math::normalize_phase(self.phase);
        }
        Ok(input.len())
    }
}

/// Receive VFO — frequency translation + resampling + optional filtering.
///
/// Ports SDR++ `dsp::channel::RxVFO`. Extracts a channel from a wideband
/// IQ stream by:
/// 1. Frequency translating to center the desired signal at DC
/// 2. Resampling to the output sample rate
/// 3. Optionally bandpass filtering if bandwidth < output sample rate
pub struct RxVfo {
    xlator: FrequencyXlator,
    resampler: RationalResampler,
    filter: Option<ComplexFirFilter>,
    in_sample_rate: f64,
    out_sample_rate: f64,
    bandwidth: f64,
    offset: f64,
    xlator_buf: Vec<Complex>,
    resamp_buf: Vec<Complex>,
}

impl RxVfo {
    /// Create a new `RxVfo`.
    ///
    /// - `in_sample_rate`: input sample rate in Hz
    /// - `out_sample_rate`: desired output sample rate in Hz
    /// - `bandwidth`: channel bandwidth in Hz
    /// - `offset`: center frequency offset in Hz (negative = shift down)
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if parameters are invalid.
    pub fn new(
        in_sample_rate: f64,
        out_sample_rate: f64,
        bandwidth: f64,
        offset: f64,
    ) -> Result<Self, DspError> {
        let xlator = FrequencyXlator::from_hz(-offset, in_sample_rate);
        let resampler = RationalResampler::new(in_sample_rate, out_sample_rate)?;

        let filter = if bandwidth < out_sample_rate - 1.0 {
            let filter_width = bandwidth / 2.0;
            let filter_trans = filter_width * 0.1;
            let filter_taps = taps::low_pass(filter_width, filter_trans, out_sample_rate, true)?;
            Some(ComplexFirFilter::new(filter_taps)?)
        } else {
            None
        };

        Ok(Self {
            xlator,
            resampler,
            filter,
            in_sample_rate,
            out_sample_rate,
            bandwidth,
            offset,
            xlator_buf: Vec::new(),
            resamp_buf: Vec::new(),
        })
    }

    /// Update the frequency offset.
    #[allow(clippy::cast_possible_truncation)]
    pub fn set_offset(&mut self, offset: f64) {
        self.offset = offset;
        self.xlator.set_offset_hz(-offset, self.in_sample_rate);
    }

    /// Update the bandwidth. Regenerates the filter if needed.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if bandwidth is invalid.
    pub fn set_bandwidth(&mut self, bandwidth: f64) -> Result<(), DspError> {
        self.bandwidth = bandwidth;
        if bandwidth < self.out_sample_rate - 1.0 {
            let filter_width = bandwidth / 2.0;
            let filter_trans = filter_width * 0.1;
            let filter_taps =
                taps::low_pass(filter_width, filter_trans, self.out_sample_rate, true)?;
            self.filter = Some(ComplexFirFilter::new(filter_taps)?);
        } else {
            self.filter = None;
        }
        Ok(())
    }

    /// Reset all internal state.
    pub fn reset(&mut self) {
        self.xlator.reset();
        self.resampler.reset();
        if let Some(f) = &mut self.filter {
            f.reset();
        }
    }

    /// Process complex samples through the VFO chain.
    ///
    /// Returns the number of output samples written.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output` is too small.
    pub fn process(
        &mut self,
        input: &[Complex],
        output: &mut [Complex],
    ) -> Result<usize, DspError> {
        // Step 1: Frequency translate
        self.xlator_buf.resize(input.len(), Complex::default());
        self.xlator.process(input, &mut self.xlator_buf)?;

        // Step 2: Resample — size buffer for worst-case expansion ratio
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ratio = (self.out_sample_rate / self.in_sample_rate).ceil() as usize;
        let resamp_size = input.len() * ratio.max(1) + 16;
        self.resamp_buf.resize(resamp_size, Complex::default());
        let resamp_count = self
            .resampler
            .process(&self.xlator_buf, &mut self.resamp_buf)?;

        // Step 3: Optional filter
        if let Some(filter) = &mut self.filter {
            if output.len() < resamp_count {
                return Err(DspError::BufferTooSmall {
                    need: resamp_count,
                    got: output.len(),
                });
            }
            filter.process(&self.resamp_buf[..resamp_count], output)?;
            Ok(resamp_count)
        } else {
            if output.len() < resamp_count {
                return Err(DspError::BufferTooSmall {
                    need: resamp_count,
                    got: output.len(),
                });
            }
            output[..resamp_count].copy_from_slice(&self.resamp_buf[..resamp_count]);
            Ok(resamp_count)
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::manual_range_contains
)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    #[test]
    fn test_xlator_dc_passthrough() {
        // Zero offset should pass DC signal through unchanged
        let mut xlator = FrequencyXlator::new(0.0);
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        xlator.process(&input, &mut output).unwrap();
        for (i, &s) in output.iter().enumerate() {
            assert!(
                (s.re - 1.0).abs() < 1e-4,
                "sample {i}: expected ~1.0, got {}",
                s.re
            );
        }
    }

    #[test]
    fn test_xlator_shifts_frequency() {
        // Apply a frequency shift and verify the signal rotates
        let offset = 0.1_f32;
        let mut xlator = FrequencyXlator::new(offset);
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        xlator.process(&input, &mut output).unwrap();
        // Output should be a rotating phasor at the offset frequency
        // Check that amplitude is preserved (~1.0)
        for s in &output {
            assert!(
                (s.amplitude() - 1.0).abs() < 1e-4,
                "amplitude should be ~1.0"
            );
        }
        // Phase should advance by offset each sample
        let phase_diff = output[50].phase() - output[49].phase();
        let normalized = math::normalize_phase(phase_diff);
        assert!(
            (normalized - offset).abs() < 0.02,
            "phase advance should be ~{offset}, got {normalized}"
        );
    }

    #[test]
    fn test_xlator_reset() {
        let mut xlator = FrequencyXlator::new(0.5);
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        xlator.process(&input, &mut output).unwrap();
        xlator.reset();
        // After reset, first output should have phase ~0
        xlator.process(&input[..1], &mut output[..1]).unwrap();
        assert!(
            output[0].phase().abs() < 0.01,
            "after reset, phase should be ~0"
        );
    }

    #[test]
    fn test_rxvfo_basic() {
        // 48kHz input, 8kHz output, 6kHz bandwidth, no offset
        let mut vfo = RxVfo::new(48_000.0, 8_000.0, 6_000.0, 0.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 4800];
        let mut output = vec![Complex::default(); 4800];
        let count = vfo.process(&input, &mut output).unwrap();
        // Should produce ~800 samples (4800 * 8000/48000)
        assert!(
            count >= 600 && count <= 1000,
            "expected ~800 samples, got {count}"
        );
    }

    #[test]
    fn test_rxvfo_passthrough_rate() {
        // Same input/output rate, full bandwidth = no filter
        let mut vfo = RxVfo::new(48_000.0, 48_000.0, 48_000.0, 0.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 1000];
        let mut output = vec![Complex::default(); 1100];
        let count = vfo.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
    }

    #[test]
    fn test_rxvfo_with_offset() {
        // Offset should shift the signal
        let mut vfo = RxVfo::new(48_000.0, 48_000.0, 48_000.0, 1_000.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 1000];
        let mut output = vec![Complex::default(); 1100];
        let count = vfo.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
        // Output should be oscillating (shifted from DC)
        let crossings = output[100..count]
            .windows(2)
            .filter(|w| (w[0].re >= 0.0) != (w[1].re >= 0.0))
            .count();
        assert!(
            crossings > 10,
            "offset signal should oscillate, got {crossings} crossings"
        );
    }

    #[test]
    fn test_xlator_buffer_too_small() {
        let mut xlator = FrequencyXlator::new(0.0);
        let input = [Complex::default(); 10];
        let mut output = [Complex::default(); 5];
        assert!(xlator.process(&input, &mut output).is_err());
    }

    #[test]
    fn test_xlator_output_is_unit_amplitude() {
        // Regardless of offset, unit input should produce unit output
        let mut xlator = FrequencyXlator::new(PI / 4.0);
        let input = vec![Complex::new(1.0, 0.0); 500];
        let mut output = vec![Complex::default(); 500];
        xlator.process(&input, &mut output).unwrap();
        for (i, s) in output.iter().enumerate() {
            assert!(
                (s.amplitude() - 1.0).abs() < 1e-3,
                "sample {i}: amplitude {}, expected ~1.0",
                s.amplitude()
            );
        }
    }
}
