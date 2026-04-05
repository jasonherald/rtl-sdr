//! IQ frontend — central signal processing hub.
//!
//! Ports SDR++ `IQFrontEnd`. Sits between the source and VFOs, providing:
//! - Power decimation
//! - DC blocking
//! - IQ conjugation (inversion correction)
//! - FFT computation for waterfall display
//! - Fan-out to multiple VFO consumers

use sdr_dsp::correction::DcBlocker;
use sdr_dsp::fft::{self, FftEngine, RustFftEngine};
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

/// IQ frontend processing hub.
///
/// Processes raw IQ from a source through decimation, correction,
/// and FFT computation, then distributes to VFO consumers.
pub struct IqFrontend {
    sample_rate: f64,
    fft_size: usize,
    #[allow(dead_code)]
    fft_window: FftWindow,
    fft_engine: RustFftEngine,
    fft_window_buf: Vec<f32>,
    fft_input: Vec<Complex>,
    fft_output: Vec<f32>,
    dc_blocker: Option<DcBlocker>,
    dc_scratch: Vec<Complex>,
    invert_iq: bool,
}

/// DC blocker convergence rate for the IQ frontend.
const DC_BLOCK_RATE: f64 = 0.001;

impl IqFrontend {
    /// Create a new IQ frontend.
    ///
    /// - `sample_rate`: input sample rate in Hz
    /// - `fft_size`: FFT size for spectrum display
    /// - `fft_window`: window function for FFT
    /// - `dc_blocking`: whether to enable DC blocking
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `fft_size` is 0.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    pub fn new(
        sample_rate: f64,
        fft_size: usize,
        fft_window: FftWindow,
        dc_blocking: bool,
    ) -> Result<Self, DspError> {
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

        let dc_blocker = if dc_blocking {
            Some(DcBlocker::new(DC_BLOCK_RATE)?)
        } else {
            None
        };

        Ok(Self {
            sample_rate,
            fft_size,
            fft_window,
            fft_engine,
            fft_window_buf,
            fft_input: vec![Complex::default(); fft_size],
            fft_output: vec![0.0; fft_size],
            dc_blocker,
            dc_scratch: Vec::new(),
            invert_iq: false,
        })
    }

    /// Get the current sample rate.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
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
            Some(DcBlocker::new(DC_BLOCK_RATE)?)
        } else {
            None
        };
        Ok(())
    }

    /// Process a block of IQ samples through the frontend.
    ///
    /// Applies DC blocking and IQ inversion, then computes FFT.
    /// Returns the processed IQ samples and FFT power spectrum.
    ///
    /// - `input`: raw IQ samples from source
    /// - `output`: processed IQ samples (same length as input)
    /// - `fft_out`: FFT power spectrum in dB (length = `fft_size`)
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if buffers are too small.
    pub fn process(
        &mut self,
        input: &[Complex],
        output: &mut [Complex],
        fft_out: &mut [f32],
    ) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        if fft_out.len() < self.fft_size {
            return Err(DspError::BufferTooSmall {
                need: self.fft_size,
                got: fft_out.len(),
            });
        }

        // Step 1: Copy input to output
        output[..input.len()].copy_from_slice(input);

        // Step 2: IQ inversion (conjugate)
        if self.invert_iq {
            for s in &mut output[..input.len()] {
                s.im = -s.im;
            }
        }

        // Step 3: DC blocking
        if let Some(dc) = &mut self.dc_blocker {
            self.dc_scratch.resize(input.len(), Complex::default());
            self.dc_scratch.copy_from_slice(&output[..input.len()]);
            dc.process(&self.dc_scratch, &mut output[..input.len()])?;
        }

        // Step 4: Compute FFT from the last fft_size samples (or available)
        let fft_samples = input.len().min(self.fft_size);
        let fft_start = input.len().saturating_sub(fft_samples);

        // Clear FFT input and copy windowed samples
        self.fft_input.fill(Complex::default());
        for i in 0..fft_samples {
            let w = self.fft_window_buf[i];
            self.fft_input[i] = output[fft_start + i] * w;
        }

        // Execute FFT
        self.fft_engine.forward(&mut self.fft_input)?;

        // Convert to power spectrum dB
        fft::power_spectrum_db(&self.fft_input, &mut self.fft_output)?;
        fft_out[..self.fft_size].copy_from_slice(&self.fft_output);

        Ok(input.len())
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
        let fe = IqFrontend::new(TEST_SAMPLE_RATE, TEST_FFT_SIZE, FftWindow::Nuttall, true);
        assert!(fe.is_ok());
        let fe = fe.unwrap();
        assert_eq!(fe.fft_size(), TEST_FFT_SIZE);
        assert!((fe.sample_rate() - TEST_SAMPLE_RATE).abs() < 1.0);
    }

    #[test]
    fn test_new_zero_fft() {
        assert!(IqFrontend::new(TEST_SAMPLE_RATE, 0, FftWindow::Nuttall, true).is_err());
    }

    #[test]
    fn test_process_dc_signal() {
        let mut fe =
            IqFrontend::new(TEST_SAMPLE_RATE, TEST_FFT_SIZE, FftWindow::Nuttall, false).unwrap();
        let input = vec![Complex::new(1.0, 0.0); TEST_FFT_SIZE];
        let mut output = vec![Complex::default(); TEST_FFT_SIZE];
        let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

        let count = fe.process(&input, &mut output, &mut fft_out).unwrap();
        assert_eq!(count, TEST_FFT_SIZE);

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
        let mut fe =
            IqFrontend::new(TEST_SAMPLE_RATE, TEST_FFT_SIZE, FftWindow::Nuttall, false).unwrap();

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

        fe.process(&input, &mut output, &mut fft_out).unwrap();

        // Find the peak — should be near the tone bin
        let peak_bin = fft_out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map_or(0, |(i, _)| i);

        // Allow ±2 bins due to windowing
        assert!(
            peak_bin.abs_diff(tone_bin) <= 2,
            "expected peak near bin {tone_bin}, got {peak_bin}"
        );
    }

    #[test]
    fn test_iq_inversion() {
        let mut fe = IqFrontend::new(
            TEST_SAMPLE_RATE,
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
            IqFrontend::new(TEST_SAMPLE_RATE, TEST_FFT_SIZE, FftWindow::Nuttall, true).unwrap();

        // Process DC signal many times — DC blocker should reduce it
        let input = vec![Complex::new(5.0, 3.0); TEST_FFT_SIZE];
        let mut output = vec![Complex::default(); TEST_FFT_SIZE];
        let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

        for _ in 0..10 {
            fe.process(&input, &mut output, &mut fft_out).unwrap();
        }

        // After many blocks, DC should be substantially reduced
        let last = output[TEST_FFT_SIZE - 1];
        assert!(
            last.re.abs() < 2.0,
            "DC should be reduced, got re={}",
            last.re
        );
    }

    #[test]
    fn test_buffer_too_small() {
        let mut fe =
            IqFrontend::new(TEST_SAMPLE_RATE, TEST_FFT_SIZE, FftWindow::Nuttall, false).unwrap();
        let input = vec![Complex::default(); 100];
        let mut output = vec![Complex::default(); 50]; // too small
        let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];
        assert!(fe.process(&input, &mut output, &mut fft_out).is_err());
    }
}
