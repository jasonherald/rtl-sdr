//! IQ frontend — central signal processing hub.
//!
//! Ports SDR++ `IQFrontEnd`. Sits between the source and VFOs, providing:
//! - Power decimation (configurable ratio)
//! - DC blocking
//! - IQ conjugation (inversion correction)
//! - FFT computation for waterfall display (with sample accumulation)
//! - Fan-out to multiple VFO consumers

use sdr_dsp::correction::{DcBlocker, IQ_CORRECTION_DEFAULT_RATE, IqCorrector};
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

impl FftWindow {
    /// Coherent gain (DC gain) of the window — first cosine coefficient.
    ///
    /// Used to correct power spectrum magnitudes so windowed signals
    /// display at their true amplitude rather than appearing lower.
    #[must_use]
    pub fn coherent_gain(self) -> f32 {
        match self {
            Self::Rectangular => 1.0,
            Self::Blackman => 0.42,
            Self::Nuttall => 0.355_768,
        }
    }
}

/// DC blocker rate factor: `50.0 / sample_rate`.
/// Matches C++ `genDCBlockRate`.
const DC_BLOCK_RATE_FACTOR: f64 = 50.0;

/// Default target FFT frame rate in Hz (matches typical 60 FPS UI refresh).
const DEFAULT_FFT_RATE: f64 = 60.0;

/// Minimum FFT rate floor (Hz) — prevents division by zero or zero-skip budgets.
const MIN_FFT_RATE_HZ: f64 = 1.0;

/// Compute FFT skip budget: samples between FFT frames.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn calc_fft_skip_samples(effective_sample_rate: f64, fps: f64) -> usize {
    (effective_sample_rate / fps.max(MIN_FFT_RATE_HZ))
        .round()
        .max(MIN_FFT_RATE_HZ) as usize
}

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
    /// Adaptive I/Q-imbalance corrector (Step 5); `None` when disabled.
    iq_corrector: Option<IqCorrector>,
    invert_iq: bool,

    // FFT
    fft_size: usize,
    fft_engine: RustFftEngine,
    fft_window_buf: Vec<f32>,
    /// Window coherent gain for energy correction in power spectrum.
    window_coherent_gain: f32,
    fft_accum: Vec<Complex>,
    fft_accum_count: usize,
    fft_output: Vec<f32>,

    // FFT rate control (Reshaper) — avoids computing more FFTs than the UI displays.
    // Matches C++ SDR++ Reshaper concept.
    fft_rate: f64,
    /// Samples between FFT frames at the current rate.
    fft_skip_samples: usize,
    /// Counter: samples processed since last FFT output.
    fft_skip_counter: usize,
    /// Whether we're currently accumulating for the next FFT.
    fft_accumulating: bool,
    /// Master FFT compute gate. When `false`, `process()` skips the
    /// entire FFT accumulation + compute loop and returns
    /// `fft_ready = false` unconditionally — saving the per-sample
    /// copy into `fft_accum`, the windowing pass, and the FFT
    /// itself. Used by the UI to suspend the waterfall display when
    /// the user toggles it off (issue #646) or the window is
    /// minimized (#647). Independent of `fft_rate` — rate stays
    /// untouched so re-enabling immediately resumes at the
    /// previously-configured frame rate without a setting round-trip.
    fft_enabled: bool,

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

        // FFT rate control uses raw sample rate — FFT is computed on
        // pre-decimation data to show the full tuner bandwidth.
        let fft_skip_samples = calc_fft_skip_samples(sample_rate, DEFAULT_FFT_RATE);

        Ok(Self {
            sample_rate,
            decim_ratio,
            effective_sample_rate,
            decimator,
            dc_blocker,
            iq_corrector: None,
            invert_iq: false,
            fft_size,
            fft_engine,
            fft_window_buf,
            window_coherent_gain: fft_window.coherent_gain(),
            fft_accum: vec![Complex::default(); fft_size],
            fft_accum_count: 0,
            fft_output: vec![0.0; fft_size],
            fft_rate: DEFAULT_FFT_RATE,
            fft_skip_samples,
            fft_skip_counter: 0,
            fft_accumulating: true,
            // Default-on so existing call sites that don't explicitly
            // toggle the gate keep the historical behavior. The UI is
            // responsible for sending `SetFftEnabled(false)` when the
            // user toggles the waterfall off (#646) or the window is
            // minimized (#647).
            fft_enabled: true,
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

    /// Set the target FFT rate in frames per second.
    ///
    /// Controls how many FFT frames are computed per second, matching
    /// the C++ SDR++ Reshaper concept. FFTs that would exceed the target
    /// rate are skipped to save CPU.
    pub fn set_fft_rate(&mut self, fps: f64) {
        self.fft_rate = fps.max(MIN_FFT_RATE_HZ);
        self.fft_skip_samples = calc_fft_skip_samples(self.sample_rate, self.fft_rate);
        self.fft_accum_count = 0;
        self.fft_skip_counter = 0;
        self.fft_accumulating = true;
    }

    /// Get the current target FFT rate.
    pub fn fft_rate(&self) -> f64 {
        self.fft_rate
    }

    /// Toggle the master FFT compute gate. When `false`, `process()`
    /// skips the entire FFT accumulation + compute loop and returns
    /// `fft_ready = false` unconditionally — the per-sample copy
    /// into `fft_accum`, the windowing pass, and the FFT itself are
    /// all elided. Audio / demod / decimation continue to run
    /// normally — only the spectrum-display path is suspended.
    ///
    /// Used by the UI to suspend the waterfall when the user toggles
    /// it off (#646) or the window is minimized (#647). Also resets
    /// the FFT accumulator so re-enabling starts fresh — a stale
    /// half-frame at the moment of disable would otherwise prepend
    /// the first FFT after re-enable, producing a brief
    /// discontinuity in the waterfall.
    pub fn set_fft_enabled(&mut self, enabled: bool) {
        if self.fft_enabled == enabled {
            return;
        }
        self.fft_enabled = enabled;
        self.fft_accum_count = 0;
        self.fft_skip_counter = 0;
        self.fft_accumulating = true;
    }

    /// Whether the FFT compute gate is currently enabled.
    pub fn fft_enabled(&self) -> bool {
        self.fft_enabled
    }

    /// Enable or disable IQ inversion correction.
    pub fn set_invert_iq(&mut self, invert: bool) {
        self.invert_iq = invert;
    }

    /// Enable or disable adaptive IQ-imbalance correction (image
    /// cancellation). Independent of DC blocking — they fix different
    /// receiver defects and live in different pipeline stages.
    pub fn set_iq_correction(&mut self, enabled: bool) {
        self.iq_corrector = if enabled {
            // Default rate is validated at compile time to be in (0, 1);
            // `new` cannot fail for it, so a failure here is a bug.
            IqCorrector::new(IQ_CORRECTION_DEFAULT_RATE).ok()
        } else {
            None
        };
    }

    /// Whether the IQ-imbalance corrector stage is engaged.
    #[must_use]
    pub fn iq_correction(&self) -> bool {
        self.iq_corrector.is_some()
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
        // The FFT accumulator is fed pre-decimation input (see `new`), so
        // the skip budget stays on the RAW rate — deriving it from the
        // effective rate ran the FFT `ratio×` too often after the
        // controller's auto-decimation (#706).
        self.fft_skip_samples = calc_fft_skip_samples(self.sample_rate, self.fft_rate);
        self.fft_skip_counter = 0;
        self.fft_accumulating = true;
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

        // Step 1: FFT accumulation from raw input (pre-decimation).
        // Shows the full tuner bandwidth in the waterfall/FFT display,
        // matching how SDR++ renders its spectrum.
        //
        // Gated by `fft_enabled` (#646 / #647): when the user toggles
        // the waterfall off, or the window is minimized, the entire
        // accumulator + compute loop is skipped — saving the per-
        // sample memcpy, the windowing pass, and the FFT itself.
        // Audio / demod / decimation below continue to run normally.
        let mut fft_ready = false;
        if self.fft_enabled {
            let mut pos = 0;
            let raw_len = input.len();
            while pos < raw_len {
                if self.fft_accumulating {
                    let remaining_fft = self.fft_size - self.fft_accum_count;
                    let available = raw_len - pos;
                    let to_copy = remaining_fft.min(available);

                    self.fft_accum[self.fft_accum_count..self.fft_accum_count + to_copy]
                        .copy_from_slice(&input[pos..pos + to_copy]);
                    self.fft_accum_count += to_copy;
                    self.fft_skip_counter += to_copy;
                    pos += to_copy;

                    if self.fft_accum_count >= self.fft_size {
                        if !fft_ready {
                            self.compute_fft(fft_out)?;
                            fft_ready = true;
                        }
                        self.fft_accum_count = 0;
                        self.fft_accumulating = false;
                    }
                } else {
                    let remaining_skip =
                        self.fft_skip_samples.saturating_sub(self.fft_skip_counter);
                    let available = raw_len - pos;
                    let to_skip = remaining_skip.min(available);
                    self.fft_skip_counter += to_skip;
                    pos += to_skip;

                    if self.fft_skip_counter >= self.fft_skip_samples {
                        self.fft_accumulating = true;
                        self.fft_skip_counter = 0;
                    }
                }
            }
        }

        // Step 2: Decimation (audio/demod pipeline only — FFT already done)
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

        // Step 3: IQ inversion (conjugate) — demod path only
        if self.invert_iq {
            for s in &mut output[..processed] {
                s.im = -s.im;
            }
        }

        // Step 4: DC blocking — demod path only
        if let Some(dc) = &mut self.dc_blocker {
            self.dc_scratch.resize(processed, Complex::default());
            self.dc_scratch.copy_from_slice(&output[..processed]);
            dc.process(&self.dc_scratch, &mut output[..processed])?;
        }

        // Step 5: IQ-imbalance correction — demod path only. Runs after
        // DC blocking so the LMS estimate is not biased by a DC offset
        // (a DC term is its own conjugate and would pull `c` off).
        if let Some(iq) = &mut self.iq_corrector {
            self.dc_scratch.resize(processed, Complex::default());
            self.dc_scratch.copy_from_slice(&output[..processed]);
            iq.process(&self.dc_scratch, &mut output[..processed])?;
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

        // Convert to power spectrum dB (with window energy correction)
        fft::power_spectrum_db(
            &self.fft_work,
            &mut self.fft_output,
            self.window_coherent_gain,
        )?;

        // Apply fftshift so consumers see the natural ordering:
        // [ -Nyquist ... DC ... +Nyquist-1bin ]
        //
        // Without this, the raw rustfft output has DC at bin 0,
        // positive frequencies in [1..N/2], and negative (aliased)
        // frequencies in [N/2..N-1]. Both UI consumers
        // (`crates/sdr-ui/src/spectrum/fft_plot.rs:317-320` for
        // GTK, the Swift Metal renderer for macOS) map bin index
        // linearly to frequency assuming shifted ordering — so
        // without the shift, strong signals around DC appear at
        // both edges of the display with a dead zone in the
        // middle. Classic symptom, hence the shift.
        // Careful with odd sizes: `half = n / 2` rounds down, so
        // the upper half has `n - half` elements (one more than
        // `half` when n is odd). `RustFftEngine::new` doesn't
        // enforce power-of-2 / even sizes, so defend here rather
        // than relying on upstream validation. `copy_from_slice`
        // panics on length mismatch, not a silent mis-shift.
        let n = self.fft_size;
        let half = n / 2;
        let upper_len = n - half;
        fft_out[..upper_len].copy_from_slice(&self.fft_output[half..n]);
        fft_out[upper_len..n].copy_from_slice(&self.fft_output[..half]);

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
mod tests;
