//! IQ frontend — central signal processing hub.
//!
//! Ports SDR++ `IQFrontEnd`. Sits between the source and VFOs, providing:
//! - Power decimation (configurable ratio)
//! - DC blocking
//! - IQ conjugation (inversion correction)
//! - FFT computation for waterfall display (with sample accumulation)
//! - Fan-out to multiple VFO consumers

use sdr_dsp::correction::DcBlocker;
use sdr_dsp::fft::{self, FftEngine, RustFftEngine};
use sdr_dsp::multirate::PowerDecimator;
use sdr_dsp::window;
use sdr_types::{Complex, DspError};

/// FFT window function selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FftWindow {
    /// Rectangular (no windowing).
    Rectangular,
    /// Blackman window.
    Blackman,
    /// Nuttall window (default).
    Nuttall,
}

/// DC blocker rate factor: `50.0 / sample_rate`.
/// Matches C++ `genDCBlockRate`.
const DC_BLOCK_RATE_FACTOR: f64 = 50.0;

/// IQ frontend processing hub.
///
/// Processes raw IQ from a source through decimation, correction,
/// and FFT computation, then distributes to VFO consumers.
pub struct IqFrontend {
    sample_rate: f64,
    decim_ratio: u32,
    effective_sample_rate: f64,

    // Pre-processing
    decimator: Option<PowerDecimator>,
    dc_blocker: Option<DcBlocker>,
    invert_iq: bool,

    // FFT
    fft_size: usize,
    fft_engine: RustFftEngine,
    fft_window_buf: Vec<f32>,
    fft_accum: Vec<Complex>,
    fft_accum_count: usize,
    fft_output: Vec<f32>,

    // Scratch buffers
    decim_buf: Vec<Complex>,
    dc_scratch: Vec<Complex>,
    fft_work: Vec<Complex>,
}

impl IqFrontend {
    /// Create a new IQ frontend.
    ///
    /// - `sample_rate`: input sample rate in Hz
    /// - `decim_ratio`: power-of-2 decimation ratio (1 = no decimation)
    /// - `fft_size`: FFT size for spectrum display
    /// - `fft_window`: window function for FFT
    /// - `dc_blocking`: whether to enable DC blocking
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `fft_size` is 0 or `decim_ratio` is invalid.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    pub fn new(
        sample_rate: f64,
        decim_ratio: u32,
        fft_size: usize,
        fft_window: FftWindow,
        dc_blocking: bool,
    ) -> Result<Self, DspError> {
        if decim_ratio == 0 {
            return Err(DspError::InvalidParameter(
                "decimation ratio must be >= 1".to_string(),
            ));
        }
        let fft_engine = RustFftEngine::new(fft_size)?;

        // Pre-compute window coefficients
        let fft_window_buf: Vec<f32> = (0..fft_size)
            .map(|i| {
                let n = i as f64;
                let big_n = fft_size as f64;
                let w = match fft_window {
                    FftWindow::Rectangular => window::rectangular(n, big_n),
                    FftWindow::Blackman => window::blackman(n, big_n),
                    FftWindow::Nuttall => window::nuttall(n, big_n),
                };
                w as f32
            })
            .collect();

        let effective_sample_rate = sample_rate / f64::from(decim_ratio);

        let decimator = if decim_ratio > 1 {
            Some(PowerDecimator::new(decim_ratio)?)
        } else {
            None
        };

        let dc_blocker = if dc_blocking {
            let rate = DC_BLOCK_RATE_FACTOR / effective_sample_rate;
            Some(DcBlocker::new(rate)?)
        } else {
            None
        };

        Ok(Self {
            sample_rate,
            decim_ratio,
            effective_sample_rate,
            decimator,
            dc_blocker,
            invert_iq: false,
            fft_size,
            fft_engine,
            fft_window_buf,
            fft_accum: vec![Complex::default(); fft_size],
            fft_accum_count: 0,
            fft_output: vec![0.0; fft_size],
            decim_buf: Vec::new(),
            dc_scratch: Vec::new(),
            fft_work: vec![Complex::default(); fft_size],
        })
    }

    /// Get the input sample rate.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Get the effective sample rate after decimation.
    ///
    /// Ports `IQFrontEnd::getEffectiveSamplerate`.
    pub fn effective_sample_rate(&self) -> f64 {
        self.effective_sample_rate
    }

    /// Get the decimation ratio.
    pub fn decim_ratio(&self) -> u32 {
        self.decim_ratio
    }

    /// Get the FFT size.
    pub fn fft_size(&self) -> usize {
        self.fft_size
    }

    /// Enable or disable IQ inversion correction.
    pub fn set_invert_iq(&mut self, invert: bool) {
        self.invert_iq = invert;
    }

    /// Enable or disable DC blocking.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the DC blocker cannot be created.
    pub fn set_dc_blocking(&mut self, enabled: bool) -> Result<(), DspError> {
        self.dc_blocker = if enabled {
            let rate = DC_BLOCK_RATE_FACTOR / self.effective_sample_rate;
            Some(DcBlocker::new(rate)?)
        } else {
            None
        };
        Ok(())
    }

    /// Set the decimation ratio.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the ratio is invalid.
    pub fn set_decimation(&mut self, ratio: u32) -> Result<(), DspError> {
        if ratio == 0 {
            return Err(DspError::InvalidParameter(
                "decimation ratio must be >= 1".to_string(),
            ));
        }

        // Validate before mutating — construct decimator first
        let new_decimator = if ratio > 1 {
            Some(PowerDecimator::new(ratio)?)
        } else {
            None
        };
        let new_effective_rate = self.sample_rate / f64::from(ratio);

        // Rebuild DC blocker at new rate before committing
        let new_dc_blocker = if self.dc_blocker.is_some() {
            let rate = DC_BLOCK_RATE_FACTOR / new_effective_rate;
            Some(DcBlocker::new(rate)?)
        } else {
            None
        };

        // All validated — commit state atomically
        self.decim_ratio = ratio;
        self.effective_sample_rate = new_effective_rate;
        self.decimator = new_decimator;
        self.dc_blocker = new_dc_blocker;
        // Discard any partially accumulated FFT data from the old rate
        self.fft_accum_count = 0;
        Ok(())
    }

    /// Process a block of IQ samples through the frontend.
    ///
    /// Applies decimation, DC blocking, IQ inversion, and accumulates
    /// FFT data. When enough samples are accumulated for a full FFT,
    /// computes the power spectrum.
    ///
    /// - `input`: raw IQ samples from source
    /// - `output`: processed IQ samples (may be shorter than input due to decimation)
    /// - `fft_out`: FFT power spectrum in dB (length = `fft_size`), updated when ready
    ///
    /// Returns `(processed_count, fft_ready)` — the number of output samples
    /// and whether a new FFT result is available in `fft_out`.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if buffers are too small.
    pub fn process(
        &mut self,
        input: &[Complex],
        output: &mut [Complex],
        fft_out: &mut [f32],
    ) -> Result<(usize, bool), DspError> {
        if fft_out.len() < self.fft_size {
            return Err(DspError::BufferTooSmall {
                need: self.fft_size,
                got: fft_out.len(),
            });
        }

        // Step 1: Decimation
        let processed = if let Some(decim) = &mut self.decimator {
            self.decim_buf.resize(input.len(), Complex::default());
            let count = decim.process(input, &mut self.decim_buf)?;
            if output.len() < count {
                return Err(DspError::BufferTooSmall {
                    need: count,
                    got: output.len(),
                });
            }
            output[..count].copy_from_slice(&self.decim_buf[..count]);
            count
        } else {
            if output.len() < input.len() {
                return Err(DspError::BufferTooSmall {
                    need: input.len(),
                    got: output.len(),
                });
            }
            output[..input.len()].copy_from_slice(input);
            input.len()
        };

        // Step 2: IQ inversion (conjugate)
        if self.invert_iq {
            for s in &mut output[..processed] {
                s.im = -s.im;
            }
        }

        // Step 3: DC blocking
        if let Some(dc) = &mut self.dc_blocker {
            self.dc_scratch.resize(processed, Complex::default());
            self.dc_scratch.copy_from_slice(&output[..processed]);
            dc.process(&self.dc_scratch, &mut output[..processed])?;
        }

        // Step 4: Accumulate samples for FFT
        let mut fft_ready = false;
        let mut pos = 0;
        while pos < processed {
            let remaining_fft = self.fft_size - self.fft_accum_count;
            let available = processed - pos;
            let to_copy = remaining_fft.min(available);

            self.fft_accum[self.fft_accum_count..self.fft_accum_count + to_copy]
                .copy_from_slice(&output[pos..pos + to_copy]);
            self.fft_accum_count += to_copy;
            pos += to_copy;

            if self.fft_accum_count >= self.fft_size {
                // Full FFT buffer — compute spectrum
                self.compute_fft(fft_out)?;
                fft_ready = true;
                self.fft_accum_count = 0;
            }
        }

        Ok((processed, fft_ready))
    }

    /// Compute FFT from the accumulated buffer.
    fn compute_fft(&mut self, fft_out: &mut [f32]) -> Result<(), DspError> {
        // Copy accumulated samples into pre-allocated work buffer and apply window
        self.fft_work.copy_from_slice(&self.fft_accum);
        for (i, s) in self.fft_work.iter_mut().enumerate() {
            let w = self.fft_window_buf[i];
            s.re *= w;
            s.im *= w;
        }

        // Execute FFT
        self.fft_engine.forward(&mut self.fft_work)?;

        // Convert to power spectrum dB
        fft::power_spectrum_db(&self.fft_work, &mut self.fft_output)?;
        fft_out[..self.fft_size].copy_from_slice(&self.fft_output);

        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    const TEST_FFT_SIZE: usize = 1024;
    const TEST_SAMPLE_RATE: f64 = 48_000.0;

    #[test]
    fn test_new() {
        let fe = IqFrontend::new(TEST_SAMPLE_RATE, 1, TEST_FFT_SIZE, FftWindow::Nuttall, true);
        assert!(fe.is_ok());
        let fe = fe.unwrap();
        assert_eq!(fe.fft_size(), TEST_FFT_SIZE);
        assert!((fe.sample_rate() - TEST_SAMPLE_RATE).abs() < 1.0);
        assert!((fe.effective_sample_rate() - TEST_SAMPLE_RATE).abs() < 1.0);
    }

    #[test]
    fn test_new_zero_fft() {
        assert!(IqFrontend::new(TEST_SAMPLE_RATE, 1, 0, FftWindow::Nuttall, true).is_err());
    }

    #[test]
    fn test_new_zero_decimation_rejected() {
        assert!(
            IqFrontend::new(
                TEST_SAMPLE_RATE,
                0,
                TEST_FFT_SIZE,
                FftWindow::Nuttall,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn test_set_decimation_zero_rejected() {
        let mut fe = IqFrontend::new(
            TEST_SAMPLE_RATE,
            1,
            TEST_FFT_SIZE,
            FftWindow::Nuttall,
            false,
        )
        .unwrap();
        assert!(fe.set_decimation(0).is_err());
        // State should be unchanged after rejection
        assert_eq!(fe.decim_ratio(), 1);
        assert!((fe.effective_sample_rate() - TEST_SAMPLE_RATE).abs() < 1.0);
    }

    #[test]
    fn test_decimation_ratio() {
        let fe = IqFrontend::new(
            TEST_SAMPLE_RATE,
            4,
            TEST_FFT_SIZE,
            FftWindow::Nuttall,
            false,
        )
        .unwrap();
        assert_eq!(fe.decim_ratio(), 4);
        assert!((fe.effective_sample_rate() - 12_000.0).abs() < 1.0);
    }

    #[test]
    fn test_process_dc_signal() {
        let mut fe = IqFrontend::new(
            TEST_SAMPLE_RATE,
            1,
            TEST_FFT_SIZE,
            FftWindow::Nuttall,
            false,
        )
        .unwrap();
        let input = vec![Complex::new(1.0, 0.0); TEST_FFT_SIZE];
        let mut output = vec![Complex::default(); TEST_FFT_SIZE];
        let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

        let (count, fft_ready) = fe.process(&input, &mut output, &mut fft_out).unwrap();
        assert_eq!(count, TEST_FFT_SIZE);
        assert!(fft_ready, "FFT should be ready after fft_size samples");

        // DC signal should have peak at bin 0
        let peak_bin = fft_out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map_or(0, |(i, _)| i);
        assert_eq!(peak_bin, 0, "DC signal should peak at bin 0");
    }

    #[test]
    fn test_process_tone() {
        let mut fe = IqFrontend::new(
            TEST_SAMPLE_RATE,
            1,
            TEST_FFT_SIZE,
            FftWindow::Nuttall,
            false,
        )
        .unwrap();

        // Generate a tone at bin 64
        let tone_bin = 64;
        let input: Vec<Complex> = (0..TEST_FFT_SIZE)
            .map(|i| {
                let phase = 2.0 * PI * (tone_bin as f32) * (i as f32) / (TEST_FFT_SIZE as f32);
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();

        let mut output = vec![Complex::default(); TEST_FFT_SIZE];
        let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

        let (_, fft_ready) = fe.process(&input, &mut output, &mut fft_out).unwrap();
        assert!(fft_ready);

        let peak_bin = fft_out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map_or(0, |(i, _)| i);

        assert!(
            peak_bin.abs_diff(tone_bin) <= 2,
            "expected peak near bin {tone_bin}, got {peak_bin}"
        );
    }

    #[test]
    fn test_fft_accumulation() {
        // Send samples in chunks smaller than fft_size — FFT should only fire
        // when enough samples accumulate
        let mut fe = IqFrontend::new(
            TEST_SAMPLE_RATE,
            1,
            TEST_FFT_SIZE,
            FftWindow::Nuttall,
            false,
        )
        .unwrap();
        let chunk = vec![Complex::new(1.0, 0.0); 256];
        let mut output = vec![Complex::default(); 256];
        let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

        // First 3 chunks: 768 samples, not enough for 1024 FFT
        for _ in 0..3 {
            let (_, fft_ready) = fe.process(&chunk, &mut output, &mut fft_out).unwrap();
            assert!(!fft_ready, "FFT should not be ready yet");
        }

        // 4th chunk: 1024 total — FFT should fire
        let (_, fft_ready) = fe.process(&chunk, &mut output, &mut fft_out).unwrap();
        assert!(fft_ready, "FFT should be ready after 1024 samples");
    }

    #[test]
    fn test_iq_inversion() {
        let mut fe = IqFrontend::new(
            TEST_SAMPLE_RATE,
            1,
            TEST_FFT_SIZE,
            FftWindow::Rectangular,
            false,
        )
        .unwrap();
        fe.set_invert_iq(true);

        let input = [Complex::new(1.0, 2.0)];
        let mut output = [Complex::default(); 1];
        let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

        fe.process(&input, &mut output, &mut fft_out).unwrap();
        assert!((output[0].im - (-2.0)).abs() < 1e-6, "im should be negated");
    }

    #[test]
    fn test_dc_blocking() {
        let mut fe =
            IqFrontend::new(TEST_SAMPLE_RATE, 1, TEST_FFT_SIZE, FftWindow::Nuttall, true).unwrap();

        let input = vec![Complex::new(5.0, 3.0); TEST_FFT_SIZE];
        let mut output = vec![Complex::default(); TEST_FFT_SIZE];
        let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

        for _ in 0..10 {
            fe.process(&input, &mut output, &mut fft_out).unwrap();
        }

        let last = output[TEST_FFT_SIZE - 1];
        assert!(
            last.re.abs() < 2.0,
            "DC should be reduced, got re={}",
            last.re
        );
    }

    #[test]
    fn test_buffer_too_small() {
        let mut fe = IqFrontend::new(
            TEST_SAMPLE_RATE,
            1,
            TEST_FFT_SIZE,
            FftWindow::Nuttall,
            false,
        )
        .unwrap();
        let input = vec![Complex::default(); 100];
        let mut output = vec![Complex::default(); 50]; // too small
        let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];
        assert!(fe.process(&input, &mut output, &mut fft_out).is_err());
    }

    #[test]
    fn test_decimation_reduces_output() {
        let mut fe =
            IqFrontend::new(96_000.0, 2, TEST_FFT_SIZE, FftWindow::Nuttall, false).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 2048];
        let mut output = vec![Complex::default(); 2048];
        let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

        let (count, _) = fe.process(&input, &mut output, &mut fft_out).unwrap();
        // 2x decimation: ~1024 output from 2048 input
        assert!(
            (900..=1100).contains(&count),
            "expected ~1024 after 2x decim, got {count}"
        );
    }
}
