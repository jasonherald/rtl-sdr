//! FFT engine trait and implementations.
//!
//! Provides a trait-based FFT abstraction with a `rustfft` backend.
//! The `IqFrontend` (in sdr-pipeline) uses this for waterfall/spectrum display.

use rustfft::num_complex::Complex as RustFftComplex;
use rustfft::{Fft, FftPlanner};
use sdr_types::{Complex, DspError};
use std::sync::Arc;

/// FFT engine trait — abstracts the FFT implementation.
///
/// Implementations must support forward (time → frequency) complex-to-complex DFT.
pub trait FftEngine: Send {
    /// Perform a forward FFT in-place on the provided buffer.
    ///
    /// `buf` must have exactly `self.size()` elements. After the call,
    /// `buf` contains the frequency-domain representation.
    fn forward(&mut self, buf: &mut [Complex]) -> Result<(), DspError>;

    /// Return the FFT size (number of points).
    fn size(&self) -> usize;
}

/// FFT engine backed by the `rustfft` crate.
///
/// Pure Rust, no system dependencies. Good performance for power-of-2 sizes
/// typical of SDR waterfall displays (1024, 2048, 4096, 8192).
pub struct RustFftEngine {
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<RustFftComplex<f32>>,
    size: usize,
}

impl RustFftEngine {
    /// Create a new FFT engine for the given size.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `size` is 0.
    pub fn new(size: usize) -> Result<Self, DspError> {
        if size == 0 {
            return Err(DspError::InvalidParameter(
                "FFT size must be > 0".to_string(),
            ));
        }
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(size);
        let scratch = vec![RustFftComplex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        Ok(Self { fft, scratch, size })
    }
}

impl FftEngine for RustFftEngine {
    fn forward(&mut self, buf: &mut [Complex]) -> Result<(), DspError> {
        if buf.len() != self.size {
            return Err(DspError::BufferTooSmall {
                need: self.size,
                got: buf.len(),
            });
        }

        // Safety: Complex and RustFftComplex<f32> have identical memory layout
        // (two contiguous f32 fields, both #[repr(C)], same field order: re/im).
        // We reinterpret the slice in-place to avoid copying.
        //
        // This is the standard approach for bridging SDR complex types with
        // rustfft's num_complex::Complex<f32>.
        #[allow(unsafe_code)]
        let fft_buf: &mut [RustFftComplex<f32>] = unsafe {
            std::slice::from_raw_parts_mut(
                buf.as_mut_ptr().cast::<RustFftComplex<f32>>(),
                buf.len(),
            )
        };

        self.fft.process_with_scratch(fft_buf, &mut self.scratch);
        Ok(())
    }

    fn size(&self) -> usize {
        self.size
    }
}

/// Compute power spectrum in dB from complex FFT output.
///
/// Converts each complex bin to `10 * log10(|X[k]|² / (N * cg)²)` where `cg` is
/// the window's coherent gain. This corrects for the energy loss from windowing
/// so that signal amplitudes display at their true level.
///
/// # Errors
///
/// Returns `DspError::BufferTooSmall` if `output.len() < fft_output.len()`.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn power_spectrum_db(
    fft_output: &[Complex],
    output: &mut [f32],
    window_coherent_gain: f32,
) -> Result<(), DspError> {
    if output.len() < fft_output.len() {
        return Err(DspError::BufferTooSmall {
            need: fft_output.len(),
            got: output.len(),
        });
    }
    let size = fft_output.len() as f32;
    // Normalization: divide by (N * coherent_gain)² to correct for both
    // FFT scaling and window energy loss.
    let norm = size * window_coherent_gain;
    let norm_sq = norm * norm;
    for (i, bin) in fft_output.iter().enumerate() {
        let power = (bin.re * bin.re + bin.im * bin.im) / norm_sq;
        output[i] = 10.0 * power.max(f32::MIN_POSITIVE).log10();
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    const FFT_SIZE: usize = 1024;

    #[test]
    fn test_new_valid() {
        let engine = RustFftEngine::new(FFT_SIZE).unwrap();
        assert_eq!(engine.size(), FFT_SIZE);
    }

    #[test]
    fn test_new_zero_size() {
        assert!(RustFftEngine::new(0).is_err());
    }

    #[test]
    fn test_forward_wrong_size() {
        let mut engine = RustFftEngine::new(FFT_SIZE).unwrap();
        let mut buf = vec![Complex::default(); 512];
        assert!(engine.forward(&mut buf).is_err());
    }

    #[test]
    fn test_forward_dc_signal() {
        // DC signal (all ones) should produce energy only in bin 0
        let mut engine = RustFftEngine::new(FFT_SIZE).unwrap();
        let mut buf = vec![Complex::new(1.0, 0.0); FFT_SIZE];
        engine.forward(&mut buf).unwrap();

        // Bin 0 should have magnitude = FFT_SIZE
        let dc_magnitude = buf[0].amplitude();
        assert!(
            (dc_magnitude - FFT_SIZE as f32).abs() < 1.0,
            "DC bin magnitude should be ~{FFT_SIZE}, got {dc_magnitude}"
        );

        // Other bins should be near zero
        for (i, bin) in buf.iter().enumerate().skip(1) {
            assert!(
                bin.amplitude() < 1e-3,
                "bin {i} should be ~0, got {}",
                bin.amplitude()
            );
        }
    }

    #[test]
    fn test_forward_single_tone() {
        // Single tone at bin 8 -> complex exponential at frequency 8/N
        let mut engine = RustFftEngine::new(FFT_SIZE).unwrap();
        let tone_bin = 8;
        let mut buf: Vec<Complex> = (0..FFT_SIZE)
            .map(|i| {
                let phase = 2.0 * PI * (tone_bin as f32) * (i as f32) / (FFT_SIZE as f32);
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();

        engine.forward(&mut buf).unwrap();

        // Peak should be at the tone bin
        let peak_bin = buf
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.amplitude().partial_cmp(&b.1.amplitude()).unwrap())
            .map_or(0, |(i, _)| i);
        assert_eq!(peak_bin, tone_bin, "peak should be at bin {tone_bin}");

        // Peak magnitude should be ~FFT_SIZE
        let peak_mag = buf[tone_bin].amplitude();
        assert!(
            (peak_mag - FFT_SIZE as f32).abs() < 1.0,
            "peak magnitude should be ~{FFT_SIZE}, got {peak_mag}"
        );
    }

    #[test]
    fn test_power_spectrum_db() {
        let mut engine = RustFftEngine::new(FFT_SIZE).unwrap();
        let mut buf = vec![Complex::new(1.0, 0.0); FFT_SIZE];
        engine.forward(&mut buf).unwrap();

        let mut output = vec![0.0_f32; FFT_SIZE];
        // coherent_gain = 1.0 for rectangular window (no correction)
        power_spectrum_db(&buf, &mut output, 1.0).unwrap();

        // DC bin should be 0 dB (power = 1.0 after normalization by N^2)
        assert!(
            output[0].abs() < 0.1,
            "DC power should be ~0 dB, got {}",
            output[0]
        );

        // Noise floor bins should be very negative dB
        for (i, &db) in output.iter().enumerate().skip(1) {
            assert!(db < -60.0, "bin {i} should be << 0 dB, got {db}");
        }
    }

    #[test]
    fn test_power_spectrum_db_buffer_too_small() {
        let buf = vec![Complex::default(); 64];
        let mut output = vec![0.0_f32; 32];
        assert!(power_spectrum_db(&buf, &mut output, 1.0).is_err());
    }

    #[test]
    fn test_complex_layout_compatibility() {
        // Verify our Complex and rustfft's Complex<f32> have the same size/alignment
        assert_eq!(
            std::mem::size_of::<Complex>(),
            std::mem::size_of::<RustFftComplex<f32>>()
        );
        assert_eq!(
            std::mem::align_of::<Complex>(),
            std::mem::align_of::<RustFftComplex<f32>>()
        );

        // Verify field order matches: re at offset 0, im at offset 1 (in f32 units)
        let c = Complex::new(1.0, 2.0);
        let c_ptr = std::ptr::from_ref(&c).cast::<f32>();
        let re_ptr = std::ptr::from_ref(&c.re);
        let im_ptr = std::ptr::from_ref(&c.im);
        assert_eq!(re_ptr, c_ptr, "re should be first field");
        assert_eq!(im_ptr, c_ptr.wrapping_add(1), "im should be second field");
    }
}
