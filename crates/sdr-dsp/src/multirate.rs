//! Sample rate conversion: resampling and decimation.
//!
//! Ports SDR++ `dsp::multirate` namespace.
//!
//! - [`PolyphaseResampler`]: Efficient rational resampling via polyphase filter bank
//! - [`PowerDecimator`]: Power-of-2 decimation using cascaded stages
//! - [`RationalResampler`]: Arbitrary rate conversion combining power decimation + polyphase

use sdr_types::{Complex, DspError};

use crate::decim_taps;
use crate::taps;

/// GCD of two unsigned integers (Euclidean algorithm).
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

/// Maximum power-of-two decimation ratio supported.
const MAX_POWER_DECIM_RATIO: u32 = decim_taps::MAX_RATIO;

/// Transition width as a fraction of filter bandwidth for rational resampler.
const RESAMP_TRANSITION_RATIO: f64 = 0.1;

/// Tolerance in Hz for considering two sample rates equal (passthrough).
const RATE_EQUALITY_TOLERANCE: f64 = 1.0;

// --- Polyphase Resampler ---

/// Polyphase filter bank for efficient rational resampling.
struct PolyphaseBank {
    /// Filter coefficients organized by phase. `phases[i]` is the tap array for phase `i`.
    phases: Vec<Vec<f32>>,
    /// Number of taps per phase.
    taps_per_phase: usize,
    /// Length of the prototype before zero-padding to a multiple of
    /// the phase count. The padding is appended, so it does not move
    /// the impulse response's centre — the group delay is defined by
    /// this length, not by `taps_per_phase × phases` (#774).
    prototype_len: usize,
}

impl PolyphaseBank {
    /// Build a polyphase filter bank from a prototype lowpass filter.
    ///
    /// Distributes `prototype` taps across `phase_count` phases in reverse
    /// phase order, matching SDR++ `buildPolyphaseBank`.
    fn build(prototype: &[f32], phase_count: usize) -> Self {
        let taps_per_phase = prototype.len().div_ceil(phase_count);
        let mut phases = vec![vec![0.0_f32; taps_per_phase]; phase_count];

        for (i, &tap) in prototype.iter().enumerate() {
            let phase_idx = (phase_count - 1) - (i % phase_count);
            let tap_idx = i / phase_count;
            phases[phase_idx][tap_idx] = tap;
        }

        Self {
            phases,
            taps_per_phase,
            prototype_len: prototype.len(),
        }
    }
}

/// Polyphase resampler for rational sample rate conversion.
///
/// Ports SDR++ `dsp::multirate::PolyphaseResampler`. Converts sample rate
/// by a ratio of `interp / decim` using a polyphase filter bank.
pub struct PolyphaseResampler {
    bank: PolyphaseBank,
    interp: usize,
    decim: usize,
    delay_line: Vec<Complex>,
    phase: usize,
    offset: usize,
}

impl PolyphaseResampler {
    /// Create a new polyphase resampler.
    ///
    /// - `interp`: interpolation factor
    /// - `decim`: decimation factor
    /// - `prototype_taps`: lowpass filter taps (length should be multiple of `interp`)
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `interp` or `decim` is 0, or taps are empty.
    pub fn new(interp: usize, decim: usize, prototype_taps: &[f32]) -> Result<Self, DspError> {
        if interp == 0 {
            return Err(DspError::InvalidParameter("interp must be > 0".to_string()));
        }
        if decim == 0 {
            return Err(DspError::InvalidParameter("decim must be > 0".to_string()));
        }
        if prototype_taps.is_empty() {
            return Err(DspError::InvalidParameter(
                "prototype taps must not be empty".to_string(),
            ));
        }

        let bank = PolyphaseBank::build(prototype_taps, interp);
        let delay_line = vec![Complex::default(); bank.taps_per_phase];

        Ok(Self {
            bank,
            interp,
            decim,
            delay_line,
            phase: 0,
            offset: 0,
        })
    }

    /// Reset the resampler state.
    pub fn reset(&mut self) {
        self.delay_line.fill(Complex::default());
        self.phase = 0;
        self.offset = 0;
    }

    /// Group delay of the prototype lowpass in *input* samples:
    /// `(N − 1) / 2` prototype taps at `interp ×` the input rate, with
    /// `N` the un-padded prototype length.
    #[allow(clippy::cast_precision_loss)]
    fn group_delay_input_samples(&self) -> f64 {
        (self.bank.prototype_len.saturating_sub(1)) as f64 / 2.0 / self.interp as f64
    }

    /// Process complex samples through the resampler.
    ///
    /// Returns the number of output samples written.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output` is too small.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn process(
        &mut self,
        input: &[Complex],
        output: &mut [Complex],
    ) -> Result<usize, DspError> {
        // Compute exact upper bound on output count from current state
        let remaining = input.len().saturating_sub(self.offset);
        let max_out = if remaining == 0 {
            0
        } else {
            (remaining * self.interp + self.phase) / self.decim + 1
        };
        if output.len() < max_out {
            return Err(DspError::BufferTooSmall {
                need: max_out,
                got: output.len(),
            });
        }

        let tpp = self.bank.taps_per_phase;
        let delay_len = tpp - 1;

        // Convolve directly from delay_line + input without building a
        // concatenated work buffer. Avoids per-call copy of the delay line.
        let mut out_count = 0;

        while self.offset < input.len() {
            let phase_taps = &self.bank.phases[self.phase];
            let buf_start = self.offset;
            let mut acc_re = 0.0_f32;
            let mut acc_im = 0.0_f32;
            for (j, &tap) in phase_taps.iter().enumerate() {
                let idx = buf_start + j;
                let s = if idx < delay_len {
                    self.delay_line[idx]
                } else {
                    input[idx - delay_len]
                };
                acc_re += s.re * tap;
                acc_im += s.im * tap;
            }
            output[out_count] = Complex::new(acc_re, acc_im);
            out_count += 1;

            self.phase += self.decim;
            self.offset += self.phase / self.interp;
            self.phase %= self.interp;
        }

        self.offset -= input.len();

        // Update delay line: keep last (tpp - 1) samples from input
        if delay_len > 0 {
            if input.len() >= delay_len {
                self.delay_line[..delay_len].copy_from_slice(&input[input.len() - delay_len..]);
            } else {
                // Shift existing delay data left, append new input at end
                let keep = delay_len - input.len();
                self.delay_line.copy_within(delay_len - keep..delay_len, 0);
                self.delay_line[keep..delay_len].copy_from_slice(input);
            }
        }

        Ok(out_count)
    }
}

/// Power-of-2 decimator using cascaded FIR stages with precomputed taps.
///
/// Ports SDR++ `dsp::multirate::PowerDecimator` with the same optimized
/// decimation plans and precomputed FIR tap tables.
pub struct PowerDecimator {
    stages: Vec<DecimStage>,
    ratio: u32,
    buf_a: Vec<Complex>,
    buf_b: Vec<Complex>,
}

/// Single decimation stage with delay line and taps.
struct DecimStage {
    taps: Vec<f32>,
    delay_line: Vec<Complex>,
    decimation: usize,
    offset: usize,
}

impl DecimStage {
    fn new(taps: Vec<f32>, decimation: usize) -> Self {
        let delay_len = taps.len().saturating_sub(1);
        Self {
            taps,
            delay_line: vec![Complex::default(); delay_len],
            decimation,
            offset: 0,
        }
    }

    fn reset(&mut self) {
        self.delay_line.fill(Complex::default());
        self.offset = 0;
    }

    /// Process and decimate complex samples. Returns output count.
    fn process(&mut self, input: &[Complex], output: &mut [Complex]) -> usize {
        let delay_len = self.taps.len().saturating_sub(1);
        let mut out_count = 0;

        while self.offset < input.len() {
            let mut acc_re = 0.0_f32;
            let mut acc_im = 0.0_f32;
            for (j, &tap) in self.taps.iter().enumerate() {
                let sample_idx = self.offset + delay_len - j;
                let s = if sample_idx < delay_len {
                    self.delay_line[sample_idx]
                } else {
                    input[sample_idx - delay_len]
                };
                acc_re += s.re * tap;
                acc_im += s.im * tap;
            }
            output[out_count] = Complex::new(acc_re, acc_im);
            out_count += 1;
            self.offset += self.decimation;
        }
        self.offset -= input.len();

        // Update delay line
        if delay_len > 0 {
            if input.len() >= delay_len {
                self.delay_line
                    .copy_from_slice(&input[input.len() - delay_len..]);
            } else {
                let shift = delay_len - input.len();
                self.delay_line.copy_within(input.len().., 0);
                self.delay_line[shift..].copy_from_slice(input);
            }
        }

        out_count
    }
}

impl PowerDecimator {
    /// Create a new power-of-2 decimator.
    ///
    /// `ratio` must be a power of 2 (1, 2, 4, 8, ..., up to 8192).
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `ratio` is 0, not a power of 2,
    /// or exceeds the maximum.
    pub fn new(ratio: u32) -> Result<Self, DspError> {
        if ratio == 0 || !ratio.is_power_of_two() {
            return Err(DspError::InvalidParameter(format!(
                "ratio must be a power of 2, got {ratio}"
            )));
        }
        if ratio > MAX_POWER_DECIM_RATIO {
            return Err(DspError::InvalidParameter(format!(
                "ratio ({ratio}) exceeds maximum ({MAX_POWER_DECIM_RATIO})"
            )));
        }

        let stages = Self::build_stages(ratio)?;
        Ok(Self {
            stages,
            ratio,
            buf_a: Vec::new(),
            buf_b: Vec::new(),
        })
    }

    /// Build cascaded decimation stages from precomputed tap tables.
    ///
    /// Uses the optimized decimation plans from `decim_taps`, which provide
    /// multi-rate stages with precomputed FIR taps (ported from SDR++).
    fn build_stages(ratio: u32) -> Result<Vec<DecimStage>, DspError> {
        if ratio == 1 {
            return Ok(vec![]);
        }

        // Plans are indexed by log2(ratio) - 1
        let plan_idx = ratio.trailing_zeros() as usize - 1;
        if plan_idx >= decim_taps::PLANS.len() {
            return Err(DspError::InvalidParameter(format!(
                "no decimation plan for ratio {ratio}"
            )));
        }

        let plan = &decim_taps::PLANS[plan_idx];
        let stages = plan
            .stages
            .iter()
            .map(|s| DecimStage::new(s.taps.to_vec(), s.decimation))
            .collect();

        Ok(stages)
    }

    /// Current decimation ratio.
    pub fn ratio(&self) -> u32 {
        self.ratio
    }

    /// Group delay of the cascade in samples at the decimator's input
    /// rate: each stage's `(N − 1) / 2` scaled by the decimation that
    /// precedes it.
    #[allow(clippy::cast_precision_loss)]
    fn group_delay_input_samples(&self) -> f64 {
        let mut scale = 1.0;
        let mut delay = 0.0;
        for stage in &self.stages {
            delay += (stage.taps.len().saturating_sub(1)) as f64 / 2.0 * scale;
            scale *= stage.decimation as f64;
        }
        delay
    }

    /// Reset all stages.
    pub fn reset(&mut self) {
        for stage in &mut self.stages {
            stage.reset();
        }
    }

    /// Process complex samples through the decimation chain.
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
        if self.stages.is_empty() {
            // Ratio = 1, passthrough
            if output.len() < input.len() {
                return Err(DspError::BufferTooSmall {
                    need: input.len(),
                    got: output.len(),
                });
            }
            output[..input.len()].copy_from_slice(input);
            return Ok(input.len());
        }

        let expected_out = input.len().div_ceil(self.ratio as usize);
        if output.len() < expected_out {
            return Err(DspError::BufferTooSmall {
                need: expected_out,
                got: output.len(),
            });
        }

        // Process through cascaded stages using pre-allocated ping-pong buffers
        self.buf_a.clear();
        self.buf_a.extend_from_slice(input);
        self.buf_b.resize(input.len(), Complex::default());

        let mut use_a = true;
        for stage in &mut self.stages {
            let (src, dst) = if use_a {
                (&self.buf_a as &[Complex], &mut self.buf_b)
            } else {
                (&self.buf_b as &[Complex], &mut self.buf_a)
            };
            let count = stage.process(src, dst);
            if use_a {
                self.buf_b.truncate(count);
            } else {
                self.buf_a.truncate(count);
            }
            use_a = !use_a;
        }

        let result = if use_a { &self.buf_a } else { &self.buf_b };
        let out_count = result.len();
        output[..out_count].copy_from_slice(result);
        Ok(out_count)
    }
}

/// Rational sample rate converter combining power decimation and polyphase resampling.
///
/// Ports SDR++ `dsp::multirate::RationalResampler`. Automatically selects
/// the optimal strategy:
/// - Passthrough if rates are equal
/// - Power decimation only if ratio is a power of 2
/// - Polyphase resampling only if input rate <= output rate
/// - Combined power decimation + polyphase for general case
pub struct RationalResampler {
    mode: ResamplerMode,
    decimator: Option<PowerDecimator>,
    resampler: Option<PolyphaseResampler>,
    temp_buf: Vec<Complex>,
}

enum ResamplerMode {
    Passthrough,
    DecimOnly,
    ResampOnly,
    Both,
}

impl RationalResampler {
    /// Create a rational resampler for the given input and output sample rates.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if rates are non-positive or non-finite.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn new(in_sample_rate: f64, out_sample_rate: f64) -> Result<Self, DspError> {
        if !in_sample_rate.is_finite() || in_sample_rate <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "in_sample_rate must be positive and finite, got {in_sample_rate}"
            )));
        }
        if !out_sample_rate.is_finite() || out_sample_rate <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "out_sample_rate must be positive and finite, got {out_sample_rate}"
            )));
        }
        // Rates below 1 Hz round to 0 in the GCD reduction, reject them
        if in_sample_rate < 1.0 || out_sample_rate < 1.0 {
            return Err(DspError::InvalidParameter(format!(
                "sample rates must be >= 1.0 Hz for integer GCD reduction (got {in_sample_rate}, {out_sample_rate})"
            )));
        }

        // Check if rates are equal (passthrough)
        if (in_sample_rate - out_sample_rate).abs() < RATE_EQUALITY_TOLERANCE {
            return Ok(Self {
                mode: ResamplerMode::Passthrough,
                decimator: None,
                resampler: None,
                temp_buf: Vec::new(),
            });
        }

        // Calculate pre-decimation (power of 2) if input > output
        let mut intermediate_rate = in_sample_rate;
        let mut decimator = None;

        if in_sample_rate > out_sample_rate {
            let ratio_f = (in_sample_rate / out_sample_rate).log2().floor() as u32;
            let predec_ratio = (1_u32 << ratio_f).min(MAX_POWER_DECIM_RATIO);
            if predec_ratio >= 2 {
                decimator = Some(PowerDecimator::new(predec_ratio)?);
                intermediate_rate = in_sample_rate / f64::from(predec_ratio);
            }
        }

        // Calculate rational ratio via GCD
        let int_sr = intermediate_rate.round() as u64;
        let out_sr = out_sample_rate.round() as u64;
        let g = gcd(int_sr, out_sr);
        let interp = (out_sr / g) as usize;
        let decim = (int_sr / g) as usize;

        if interp == decim {
            // Power decimation alone is sufficient
            return Ok(Self {
                mode: if decimator.is_some() {
                    ResamplerMode::DecimOnly
                } else {
                    ResamplerMode::Passthrough
                },
                decimator,
                resampler: None,
                temp_buf: Vec::new(),
            });
        }

        // Design lowpass filter for the polyphase resampler
        let tap_bandwidth = in_sample_rate.min(out_sample_rate) / 2.0;
        let tap_trans_width = tap_bandwidth * RESAMP_TRANSITION_RATIO;
        let tap_sample_rate = intermediate_rate * interp as f64;
        let mut filter_taps =
            taps::low_pass(tap_bandwidth, tap_trans_width, tap_sample_rate, true)?;

        // Scale taps by interpolation factor
        for tap in &mut filter_taps {
            *tap *= interp as f32;
        }

        let resampler = Some(PolyphaseResampler::new(interp, decim, &filter_taps)?);

        let mode = if decimator.is_some() {
            ResamplerMode::Both
        } else {
            ResamplerMode::ResampOnly
        };

        Ok(Self {
            mode,
            decimator,
            resampler,
            temp_buf: Vec::new(),
        })
    }

    /// Reset the resampler state.
    pub fn reset(&mut self) {
        if let Some(d) = &mut self.decimator {
            d.reset();
        }
        if let Some(r) = &mut self.resampler {
            r.reset();
        }
    }

    /// Total group delay of the resampler chain in *input* samples,
    /// rounded to the nearest sample. An output sample `y[j]` represents
    /// the input at time `j · in_rate / out_rate − delay`. Callers that
    /// resample a slice in isolation use this to prime the delay line
    /// and align the output (#774).
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn group_delay_input_samples(&self) -> usize {
        let predecimation = self
            .decimator
            .as_ref()
            .map_or(1.0, |d| f64::from(d.ratio()));
        let decimator_delay = self
            .decimator
            .as_ref()
            .map_or(0.0, PowerDecimator::group_delay_input_samples);
        let resampler_delay = self
            .resampler
            .as_ref()
            .map_or(0.0, PolyphaseResampler::group_delay_input_samples)
            * predecimation;
        (decimator_delay + resampler_delay).round().max(0.0) as usize
    }

    /// Process complex samples through the resampler.
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
        match self.mode {
            ResamplerMode::Passthrough => {
                if output.len() < input.len() {
                    return Err(DspError::BufferTooSmall {
                        need: input.len(),
                        got: output.len(),
                    });
                }
                output[..input.len()].copy_from_slice(input);
                Ok(input.len())
            }
            ResamplerMode::DecimOnly => {
                let decim = self.decimator.as_mut().ok_or_else(|| {
                    DspError::InvalidParameter("decimator missing in DecimOnly mode".to_string())
                })?;
                decim.process(input, output)
            }
            ResamplerMode::ResampOnly => {
                let resamp = self.resampler.as_mut().ok_or_else(|| {
                    DspError::InvalidParameter("resampler missing in ResampOnly mode".to_string())
                })?;
                resamp.process(input, output)
            }
            ResamplerMode::Both => {
                let decim = self.decimator.as_mut().ok_or_else(|| {
                    DspError::InvalidParameter("decimator missing in Both mode".to_string())
                })?;
                self.temp_buf.resize(input.len(), Complex::default());
                let decim_count = decim.process(input, &mut self.temp_buf)?;

                let resamp = self.resampler.as_mut().ok_or_else(|| {
                    DspError::InvalidParameter("resampler missing in Both mode".to_string())
                })?;
                resamp.process(&self.temp_buf[..decim_count], output)
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::needless_range_loop,
    clippy::manual_range_contains
)]
mod tests;
