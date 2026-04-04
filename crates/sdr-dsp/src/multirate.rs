//! Sample rate conversion: resampling and decimation.
//!
//! Ports SDR++ `dsp::multirate` namespace.
//!
//! - [`PolyphaseResampler`]: Efficient rational resampling via polyphase filter bank
//! - [`PowerDecimator`]: Power-of-2 decimation using cascaded stages
//! - [`RationalResampler`]: Arbitrary rate conversion combining power decimation + polyphase

use sdr_types::{Complex, DspError};

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
const MAX_POWER_DECIM_RATIO: u32 = 8192; // 2^13

/// Normalized cutoff frequency for power decimator stages (fraction of sample rate).
const POWER_DECIM_CUTOFF: f64 = 0.25;

/// Normalized transition width for power decimator stages (fraction of sample rate).
const POWER_DECIM_TRANSITION: f64 = 0.1;

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
    work_buf: Vec<Complex>,
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
            work_buf: Vec::new(),
        })
    }

    /// Reset the resampler state.
    pub fn reset(&mut self) {
        self.delay_line.fill(Complex::default());
        self.phase = 0;
        self.offset = 0;
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
        // Estimate max output count
        let max_out =
            ((input.len() as f64) * (self.interp as f64) / (self.decim as f64)) as usize + 2;
        if output.len() < max_out {
            return Err(DspError::BufferTooSmall {
                need: max_out,
                got: output.len(),
            });
        }

        let tpp = self.bank.taps_per_phase;
        let delay_len = tpp - 1;

        // Reuse pre-allocated work buffer: delay_line + input
        self.work_buf.clear();
        self.work_buf.reserve(delay_len + input.len());
        self.work_buf
            .extend_from_slice(&self.delay_line[..delay_len]);
        self.work_buf.extend_from_slice(input);
        let work = &self.work_buf;

        let mut out_count = 0;

        while self.offset < input.len() {
            // Convolve with current phase
            let phase_taps = &self.bank.phases[self.phase];
            let buf_start = self.offset;
            let mut acc_re = 0.0_f32;
            let mut acc_im = 0.0_f32;
            for (j, &tap) in phase_taps.iter().enumerate() {
                let s = work[buf_start + j];
                acc_re += s.re * tap;
                acc_im += s.im * tap;
            }
            output[out_count] = Complex::new(acc_re, acc_im);
            out_count += 1;

            // Advance phase and offset
            self.phase += self.decim;
            self.offset += self.phase / self.interp;
            self.phase %= self.interp;
        }

        self.offset -= input.len();

        // Update delay line: keep last (tpp - 1) samples from work buffer
        let work_len = work.len();
        if work_len >= delay_len {
            self.delay_line[..delay_len].copy_from_slice(&work[work_len - delay_len..]);
        }

        Ok(out_count)
    }
}

/// Power-of-2 decimator using cascaded half-band FIR stages.
///
/// Ports SDR++ `dsp::multirate::PowerDecimator`. Instead of pre-computed
/// tap tables, generates lowpass taps dynamically for each stage.
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

    /// Build cascaded decimation stages.
    ///
    /// Decomposes the ratio into stages of 2x decimation each,
    /// generating lowpass taps for each stage.
    fn build_stages(ratio: u32) -> Result<Vec<DecimStage>, DspError> {
        if ratio == 1 {
            return Ok(vec![]);
        }

        let mut stages = Vec::new();
        let mut remaining = ratio;

        while remaining > 1 {
            // Each stage decimates by 2
            let stage_decim = 2;
            remaining /= stage_decim;

            // Generate lowpass taps for this stage
            // Cutoff at 0.25 (half of Nyquist), transition 0.1 of sample rate
            let stage_taps = taps::low_pass(POWER_DECIM_CUTOFF, POWER_DECIM_TRANSITION, 1.0, true)?;
            stages.push(DecimStage::new(stage_taps, stage_decim as usize));
        }

        Ok(stages)
    }

    /// Current decimation ratio.
    pub fn ratio(&self) -> u32 {
        self.ratio
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

        let expected_out = input.len() / self.ratio as usize;
        if output.len() < expected_out.max(1) {
            return Err(DspError::BufferTooSmall {
                need: expected_out.max(1),
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
mod tests {
    use super::*;

    // --- Polyphase Resampler tests ---

    #[test]
    fn test_polyphase_new_invalid() {
        assert!(PolyphaseResampler::new(0, 1, &[1.0]).is_err());
        assert!(PolyphaseResampler::new(1, 0, &[1.0]).is_err());
        assert!(PolyphaseResampler::new(1, 1, &[]).is_err());
    }

    #[test]
    fn test_polyphase_passthrough() {
        // interp=1, decim=1 with identity tap -> passthrough
        let mut r = PolyphaseResampler::new(1, 1, &[1.0]).unwrap();
        let input: Vec<Complex> = (0..100).map(|i| Complex::new(i as f32, 0.0)).collect();
        let mut output = vec![Complex::default(); 110];
        let count = r.process(&input, &mut output).unwrap();
        assert_eq!(count, 100);
        for i in 0..100 {
            assert!(
                (output[i].re - i as f32).abs() < 1e-3,
                "passthrough mismatch at {i}"
            );
        }
    }

    #[test]
    fn test_polyphase_upsample_2x() {
        // interp=2, decim=1 -> double the sample rate
        // Use a simple filter: [0.5, 1.0, 0.5] distributed across 2 phases
        let mut r = PolyphaseResampler::new(2, 1, &[0.5, 1.0, 0.5, 0.0]).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 50];
        let mut output = vec![Complex::default(); 110];
        let count = r.process(&input, &mut output).unwrap();
        // Should produce ~100 output samples (2x input)
        assert!(count >= 90 && count <= 110, "expected ~100, got {count}");
    }

    // --- Power Decimator tests ---

    #[test]
    fn test_power_decimator_invalid() {
        assert!(PowerDecimator::new(0).is_err());
        assert!(PowerDecimator::new(3).is_err()); // not power of 2
        assert!(PowerDecimator::new(16384).is_err()); // exceeds max
    }

    #[test]
    fn test_power_decimator_passthrough() {
        let mut d = PowerDecimator::new(1).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        let count = d.process(&input, &mut output).unwrap();
        assert_eq!(count, 100);
    }

    #[test]
    fn test_power_decimator_by_2() {
        let mut d = PowerDecimator::new(2).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        let count = d.process(&input, &mut output).unwrap();
        // Should produce ~50 samples
        assert!(count >= 40 && count <= 55, "expected ~50, got {count}");
    }

    #[test]
    fn test_power_decimator_by_4() {
        let mut d = PowerDecimator::new(4).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 200];
        let mut output = vec![Complex::default(); 200];
        let count = d.process(&input, &mut output).unwrap();
        // Should produce ~50 samples
        assert!(count >= 30 && count <= 60, "expected ~50, got {count}");
    }

    // --- Rational Resampler tests ---

    #[test]
    fn test_rational_resampler_invalid() {
        assert!(RationalResampler::new(0.0, 48_000.0).is_err());
        assert!(RationalResampler::new(48_000.0, 0.0).is_err());
        assert!(RationalResampler::new(f64::NAN, 48_000.0).is_err());
    }

    #[test]
    fn test_rational_resampler_passthrough() {
        let mut r = RationalResampler::new(48_000.0, 48_000.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 110];
        let count = r.process(&input, &mut output).unwrap();
        assert_eq!(count, 100);
    }

    #[test]
    fn test_rational_resampler_downsample() {
        // 48kHz -> 8kHz = 6x decimation
        let mut r = RationalResampler::new(48_000.0, 8_000.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 600];
        let mut output = vec![Complex::default(); 600];
        let count = r.process(&input, &mut output).unwrap();
        // Should produce ~100 samples (600 / 6)
        assert!(
            count >= 80 && count <= 120,
            "expected ~100 for 6x downsample, got {count}"
        );
    }

    #[test]
    fn test_rational_resampler_upsample() {
        // 8kHz -> 48kHz = 6x interpolation
        let mut r = RationalResampler::new(8_000.0, 48_000.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 700];
        let count = r.process(&input, &mut output).unwrap();
        // Should produce ~600 samples (100 * 6)
        assert!(
            count >= 500 && count <= 700,
            "expected ~600 for 6x upsample, got {count}"
        );
    }

    // --- GCD tests ---

    #[test]
    fn test_gcd() {
        assert_eq!(gcd(48_000, 44_100), 300);
        assert_eq!(gcd(48_000, 8_000), 8_000);
        assert_eq!(gcd(100, 100), 100);
        assert_eq!(gcd(7, 13), 1);
    }
}
