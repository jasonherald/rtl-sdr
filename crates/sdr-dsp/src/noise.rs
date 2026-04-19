//! Noise reduction and squelch processors.
//!
//! Ports SDR++ `dsp::noise_reduction` namespace.

use rustfft::num_complex::Complex as RustFftComplex;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

use sdr_types::{Complex, DspError};

/// Exponential moving average alpha for noise floor tracking.
///
/// Small values track slowly, avoiding false adaptation to transient signals.
const NOISE_FLOOR_ALPHA: f32 = 0.02;

/// Fast alpha used during the initial convergence period.
///
/// Allows the noise floor to quickly reach the actual noise level from the
/// default initial estimate.
const NOISE_FLOOR_FAST_ALPHA: f32 = 0.3;

/// Initial noise floor estimate in dB (very low so squelch starts open
/// until enough samples have been observed).
const NOISE_FLOOR_INITIAL_DB: f32 = -120.0;

/// Number of blocks required before the noise floor estimate is considered
/// settled and the slow alpha is used.
const NOISE_FLOOR_SETTLE_BLOCKS: u32 = 50;

/// Maximum allowed rise in the noise floor estimate per block during settling (dB).
/// Prevents strong signals at startup from biasing the floor estimate high.
/// Must be large enough that the floor can converge from `NOISE_FLOOR_INITIAL_DB`
/// to a typical noise floor (~-60 dB) within `NOISE_FLOOR_SETTLE_BLOCKS`.
const NOISE_FLOOR_MAX_RISE_DB_PER_BLOCK: f32 = 3.0;

/// Margin above the noise floor (dB) for squelch-open threshold.
const AUTO_SQUELCH_OPEN_MARGIN_DB: f32 = 10.0;

/// Margin above the noise floor (dB) for squelch-close threshold (hysteresis).
///
/// Lower than the open margin so that once a signal opens the squelch,
/// it stays open until it drops closer to the noise floor.
const AUTO_SQUELCH_CLOSE_MARGIN_DB: f32 = 6.0;

/// Attack time constant for the squelch gain envelope, in seconds.
///
/// 2 ms is short enough that a keying-up transmission's leading edge
/// isn't audibly ramped (the user doesn't perceive the fade-in), but
/// long enough that the step discontinuity is smoothed past any
/// downstream audio-sink resampler's ringing (see issue #331: macOS
/// AUHAL's internal 48 kHz → 44.1 kHz SRC rings hard on sub-ms steps
/// and produces the loud pop users reported).
const SQUELCH_ATTACK_SECONDS: f32 = 0.002;

/// Release time constant for the squelch gain envelope, in seconds.
///
/// 5 ms is deliberately longer than the attack so we don't clip the
/// tail of a word when the squelch closes mid-utterance. The attack-
/// release asymmetry is a deliberate ergonomic choice: bright on, soft
/// off — mirrors the SDR++ `PowerSquelch` envelope pattern referenced
/// in issue #331.
const SQUELCH_RELEASE_SECONDS: f32 = 0.005;

/// Compute the one-pole IIR coefficient that reaches ~63% of the gap
/// toward the target in `tau_seconds` at `sample_rate_hz`. Standard
/// exponential-decay form: `α = 1 - exp(-Δt / τ)` where `Δt = 1 /
/// sample_rate_hz`. Returns `1.0` (instant transition) on non-finite,
/// zero, or negative inputs so a misconfigured caller degrades to
/// the pre-envelope hard-gate behavior rather than panicking on
/// divide-by-zero OR poisoning the envelope with NaN / Inf that
/// would propagate into every subsequent `envelope_gain` update.
fn envelope_coefficient(tau_seconds: f32, sample_rate_hz: f32) -> f32 {
    if !sample_rate_hz.is_finite()
        || !tau_seconds.is_finite()
        || sample_rate_hz <= 0.0
        || tau_seconds <= 0.0
    {
        return 1.0;
    }
    (-1.0 / (tau_seconds * sample_rate_hz))
        .exp()
        .mul_add(-1.0, 1.0)
}

/// Per-sample attack/release envelope applied to the **audio
/// output** after FM/AM demodulation to smooth squelch gate
/// transitions. Lives in the AF path (not the IF path) because
/// FM discriminators are amplitude-invariant — scaling the IQ
/// input has zero effect on FM audio output, so the gate-
/// transition pop has to be attenuated post-demod.
///
/// The owning module (`RadioModule` today) drives `target` via
/// [`set_gate_open`] on every process call using the IF-level
/// `PowerSquelch`'s `is_open()` state; this struct's [`process_stereo`]
/// ramps the per-sample gain toward the target and applies it
/// uniformly to both stereo channels so imaging is preserved.
///
/// Attack/release asymmetry is deliberate: fast open (~2 ms) so
/// keying-up leading edges aren't audibly faded, slow close
/// (~5 ms) so tails of dropped words aren't clipped.
pub struct SquelchAudioEnvelope {
    envelope_gain: f32,
    target: f32,
    attack_coeff: f32,
    release_coeff: f32,
}

impl SquelchAudioEnvelope {
    /// Construct an envelope for audio running at
    /// `sample_rate_hz`. Coefficients are seeded from the
    /// `SQUELCH_ATTACK_SECONDS` / `SQUELCH_RELEASE_SECONDS` time
    /// constants; call [`set_sample_rate`] if the audio rate
    /// changes at runtime.
    #[must_use]
    pub fn new(sample_rate_hz: f32) -> Self {
        Self {
            envelope_gain: 0.0,
            target: 0.0,
            attack_coeff: envelope_coefficient(SQUELCH_ATTACK_SECONDS, sample_rate_hz),
            release_coeff: envelope_coefficient(SQUELCH_RELEASE_SECONDS, sample_rate_hz),
        }
    }

    /// Update the target gain: `true` (gate open) ramps to 1.0,
    /// `false` (gate closed) ramps to 0.0. No-op if the target
    /// matches the current setting so repeated calls from a
    /// block-level caller are cheap.
    pub fn set_gate_open(&mut self, open: bool) {
        self.target = if open { 1.0 } else { 0.0 };
    }

    /// Recompute the attack / release coefficients for a new
    /// audio sample rate. Preserves `envelope_gain` so the
    /// recomputation doesn't snap mid-utterance.
    pub fn set_sample_rate(&mut self, sample_rate_hz: f32) {
        self.attack_coeff = envelope_coefficient(SQUELCH_ATTACK_SECONDS, sample_rate_hz);
        self.release_coeff = envelope_coefficient(SQUELCH_RELEASE_SECONDS, sample_rate_hz);
    }

    /// Reset the envelope to the closed-gate state. Use when
    /// swapping the upstream demod (mode change) so stale gain
    /// state from the previous mode doesn't bleed through the
    /// first block of the new one.
    pub fn reset(&mut self) {
        self.envelope_gain = 0.0;
        self.target = 0.0;
    }

    /// Force the envelope to the fully-open state (gain = 1.0,
    /// target = 1.0). Used by `RadioModule::process` when the
    /// user has squelch disabled so the envelope doesn't need
    /// to ramp up from 0 on every block. Keeps state coherent
    /// for when the user re-enables squelch: first post-enable
    /// block starts at full passthrough and the normal open /
    /// close ramping takes over from there.
    pub fn reset_to_open(&mut self) {
        self.envelope_gain = 1.0;
        self.target = 1.0;
    }

    /// Apply the envelope to a stereo audio buffer in place.
    /// Both L and R receive the same per-sample gain so stereo
    /// imaging is preserved across the transition.
    pub fn process_stereo(&mut self, buf: &mut [sdr_types::Stereo]) {
        for s in buf.iter_mut() {
            let coeff = if self.target > self.envelope_gain {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            self.envelope_gain += (self.target - self.envelope_gain) * coeff;
            s.l *= self.envelope_gain;
            s.r *= self.envelope_gain;
        }
    }
}

/// Power squelch — gates signal based on average power level.
///
/// Ports SDR++ `dsp::noise_reduction::PowerSquelch`. Computes the mean
/// power of the input block and compares against a threshold in dB.
/// If below threshold, the entire block is zeroed.
///
/// Supports an auto-squelch mode that tracks the noise floor with an
/// exponential moving average and applies hysteresis margins.
pub struct PowerSquelch {
    level_db: f32,
    open: bool,
    auto_squelch: bool,
    /// Running noise floor estimate in dB (EMA-filtered).
    noise_floor_db: f32,
    /// Number of blocks processed since auto-squelch was enabled.
    /// Used to detect the initial convergence period.
    settle_count: u32,
}

impl PowerSquelch {
    /// Create a new power squelch.
    ///
    /// - `level_db`: threshold in dB (e.g., -50.0). Signal below this is muted.
    pub fn new(level_db: f32) -> Self {
        Self {
            level_db,
            open: false,
            auto_squelch: false,
            noise_floor_db: NOISE_FLOOR_INITIAL_DB,
            settle_count: 0,
        }
    }

    /// Returns whether the squelch is currently open (signal above threshold).
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Update the squelch threshold.
    pub fn set_level(&mut self, level_db: f32) {
        self.level_db = level_db;
    }

    /// Enable or disable auto-squelch (noise floor tracking).
    ///
    /// When enabled, the manual `level_db` is ignored and the threshold
    /// is derived from the tracked noise floor plus a margin.
    pub fn set_auto_squelch(&mut self, enabled: bool) {
        self.auto_squelch = enabled;
        if enabled {
            // Reset the noise floor estimate so it adapts to the current band.
            self.noise_floor_db = NOISE_FLOOR_INITIAL_DB;
            self.settle_count = 0;
        }
    }

    /// Returns whether auto-squelch is enabled.
    pub fn auto_squelch_enabled(&self) -> bool {
        self.auto_squelch
    }

    /// Returns the current noise floor estimate in dB.
    pub fn noise_floor_db(&self) -> f32 {
        self.noise_floor_db
    }

    /// Process complex samples. Passes or zeros the entire block.
    ///
    /// Returns the number of output samples (always `input.len()`).
    /// Use [`is_open`] after processing to check squelch state.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    #[allow(clippy::cast_precision_loss)]
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
        if input.is_empty() {
            self.open = false;
            return Ok(0);
        }

        // Compute mean amplitude (matching C++ volk_32fc_magnitude_32f + accumulate)
        let sum: f32 = input.iter().map(|s| s.amplitude()).sum();
        let mean_amplitude = sum / input.len() as f32;

        // Convert to standard dB: 20*log10(amplitude) = 10*log10(power).
        // This matches the standard dBFS convention used by most SDR tools.
        let measured_db = 20.0 * mean_amplitude.max(f32::MIN_POSITIVE).log10();

        let threshold_db = if self.auto_squelch {
            // During the initial settling period, use a fast alpha so the
            // noise floor converges quickly from the default initial value.
            // After settling, use the slow alpha and only update when the
            // squelch is closed or the level is below the close margin,
            // preventing active signals from corrupting the estimate.
            let settling = self.settle_count < NOISE_FLOOR_SETTLE_BLOCKS;
            if settling {
                self.settle_count = self.settle_count.saturating_add(1);
                // During settling, use fast alpha but cap extreme upward jumps
                // that are likely strong signals rather than noise. Only cap
                // when the measurement is far above the current estimate
                // (more than 2x the open margin).
                let extreme_threshold = self.noise_floor_db + AUTO_SQUELCH_OPEN_MARGIN_DB * 2.0;
                let capped_db = if measured_db > extreme_threshold {
                    self.noise_floor_db + NOISE_FLOOR_MAX_RISE_DB_PER_BLOCK
                } else {
                    measured_db
                };
                self.noise_floor_db = NOISE_FLOOR_FAST_ALPHA.mul_add(
                    capped_db,
                    (1.0 - NOISE_FLOOR_FAST_ALPHA) * self.noise_floor_db,
                );
            } else if !self.open || measured_db < self.noise_floor_db + AUTO_SQUELCH_CLOSE_MARGIN_DB
            {
                self.noise_floor_db = NOISE_FLOOR_ALPHA
                    .mul_add(measured_db, (1.0 - NOISE_FLOOR_ALPHA) * self.noise_floor_db);
            }

            // During settling, keep squelch open (pass audio through) so
            // users don't experience a silent startup period.
            if settling {
                f32::NEG_INFINITY
            } else if self.open {
                // Apply hysteresis: close threshold is lower than open threshold.
                self.noise_floor_db + AUTO_SQUELCH_CLOSE_MARGIN_DB
            } else {
                self.noise_floor_db + AUTO_SQUELCH_OPEN_MARGIN_DB
            }
        } else {
            self.level_db
        };

        if measured_db >= threshold_db {
            output[..input.len()].copy_from_slice(input);
            self.open = true;
        } else {
            // Hard-zero the IQ on closed state — FM discriminators
            // are amplitude-invariant (they read atan2 of the phase
            // delta between consecutive samples, which ignores a
            // common scaling), so a "smooth" amplitude envelope on
            // IQ would have zero effect on FM audio output. The
            // exact zero here gives `atan2(0, 0) = 0`, producing
            // genuinely silent FM demod output. The transition-
            // pop smoothing lives in the AF path
            // (`SquelchAudioEnvelope` in `sdr-radio::lib::RadioModule`)
            // where it can act on post-demod audio regardless of
            // modulation type.
            output[..input.len()].fill(Complex::default());
            self.open = false;
        }
        Ok(input.len())
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
        if !rate.is_finite() || rate <= 0.0 || rate >= 1.0 {
            return Err(DspError::InvalidParameter(format!(
                "rate must be finite and in (0, 1), got {rate}"
            )));
        }
        if !level.is_finite() || level < 1.0 {
            return Err(DspError::InvalidParameter(format!(
                "level must be finite and >= 1.0, got {level}"
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
            // Only update EMA for non-zero samples (matches C++ behavior).
            // This prevents the baseline from decaying to zero during silence,
            // which would cause false spike detections on the next real signal.
            if in_amp != 0.0 {
                self.amp = self.amp * self.inv_rate + in_amp * self.rate;
            }
            let gain = if self.amp > f32::MIN_POSITIVE {
                let excess = in_amp / self.amp;
                if excess > self.level {
                    1.0 / excess
                } else {
                    1.0
                }
            } else {
                1.0
            };
            output[i] = s * gain;
        }
        Ok(input.len())
    }
}

/// Default FFT size for FM IF noise reduction.
const FM_IF_NR_FFT_SIZE: usize = 256;

/// FM IF noise reduction — frequency-domain peak tracking.
///
/// Ports SDR++ `dsp::noise_reduction::FMIF`. Uses FFT to find the dominant
/// frequency bin and reconstructs the signal from that bin only, effectively
/// removing noise from narrow FM signals.
///
/// Matches C++ implementation:
/// - Nuttall window applied before FFT (reduces spectral leakage)
/// - Keeps exactly 1 peak bin (most selective noise rejection)
/// - Block-based processing with internal buffering
pub struct FmIfNoiseReduction {
    fft_forward: Arc<dyn Fft<f32>>,
    fft_inverse: Arc<dyn Fft<f32>>,
    fft_size: usize,
    fft_buf: Vec<RustFftComplex<f32>>,
    scratch: Vec<RustFftComplex<f32>>,
    /// Precomputed Nuttall window coefficients.
    window: Vec<f32>,
    /// Input accumulation buffer.
    overlap_buf: Vec<Complex>,
    overlap_count: usize,
}

impl FmIfNoiseReduction {
    /// Create a new FM IF noise reduction processor.
    ///
    /// Uses a default FFT size of 256 points.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if internal FFT setup fails.
    pub fn new() -> Result<Self, DspError> {
        Self::with_fft_size(FM_IF_NR_FFT_SIZE)
    }

    /// Create with a custom FFT size.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `fft_size` is 0.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    pub fn with_fft_size(fft_size: usize) -> Result<Self, DspError> {
        if fft_size == 0 {
            return Err(DspError::InvalidParameter(
                "FFT size must be > 0".to_string(),
            ));
        }
        let mut planner = FftPlanner::new();
        let fft_forward = planner.plan_fft_forward(fft_size);
        let fft_inverse = planner.plan_fft_inverse(fft_size);
        let scratch_len = fft_forward
            .get_inplace_scratch_len()
            .max(fft_inverse.get_inplace_scratch_len());
        let scratch = vec![RustFftComplex::new(0.0, 0.0); scratch_len];
        let fft_buf = vec![RustFftComplex::new(0.0, 0.0); fft_size];

        // Precompute Nuttall window — matches C++ per-sample sliding window approach.
        let window: Vec<f32> = (0..fft_size)
            .map(|i| crate::window::nuttall(i as f64, fft_size as f64) as f32)
            .collect();

        Ok(Self {
            fft_forward,
            fft_inverse,
            fft_size,
            fft_buf,
            scratch,
            window,
            overlap_buf: vec![Complex::default(); fft_size],
            overlap_count: 0,
        })
    }

    /// Process complex samples through FFT-based noise reduction.
    ///
    /// Processes complete FFT-size blocks. Remaining samples are buffered
    /// internally for the next call.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    #[allow(clippy::cast_precision_loss)]
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

        let mut in_pos = 0;
        let mut out_pos = 0;

        while in_pos < input.len() {
            // Fill the overlap buffer from input.
            let can_take = (self.fft_size - self.overlap_count).min(input.len() - in_pos);
            self.overlap_buf[self.overlap_count..self.overlap_count + can_take]
                .copy_from_slice(&input[in_pos..in_pos + can_take]);
            self.overlap_count += can_take;
            in_pos += can_take;

            // Process a complete block when we have enough samples.
            if self.overlap_count == self.fft_size {
                if out_pos + self.fft_size <= output.len() {
                    self.process_block(&mut output[out_pos..out_pos + self.fft_size]);
                    out_pos += self.fft_size;
                    self.overlap_count = 0;
                } else {
                    // Output too small for this block — keep it buffered for next call.
                    break;
                }
            }
        }

        // Pass through unprocessed tail samples so output count matches input.
        // These samples are buffered in overlap_buf for the next FFT block;
        // copy the original input (not overlap_buf which may contain stale data
        // from prior calls) to maintain correct signal flow.
        if out_pos < input.len() {
            let remaining = input.len() - out_pos;
            output[out_pos..out_pos + remaining].copy_from_slice(&input[input.len() - remaining..]);
            out_pos += remaining;
        }

        Ok(out_pos)
    }

    /// Process a single FFT-size block: window, FFT, single-peak select, IFFT.
    #[allow(clippy::cast_precision_loss)]
    fn process_block(&mut self, output: &mut [Complex]) {
        let n = self.fft_size;
        let inv_n = 1.0 / n as f32;

        // Apply Nuttall window and copy to FFT buffer.
        // Window reduces spectral leakage for more precise peak detection.
        for (i, s) in self.overlap_buf[..n].iter().enumerate() {
            let w = self.window[i];
            self.fft_buf[i] = RustFftComplex::new(s.re * w, s.im * w);
        }

        // Forward FFT.
        self.fft_forward
            .process_with_scratch(&mut self.fft_buf, &mut self.scratch);

        // Find the single bin with maximum magnitude — matches C++ keeping exactly 1 bin.
        let mut peak_bin = 0;
        let mut peak_mag = 0.0_f32;
        for (i, bin) in self.fft_buf.iter().enumerate() {
            let mag = bin.re * bin.re + bin.im * bin.im;
            if mag > peak_mag {
                peak_mag = mag;
                peak_bin = i;
            }
        }

        // Zero all bins except the single peak — most selective noise rejection.
        let peak_val = self.fft_buf[peak_bin];
        for bin in &mut self.fft_buf {
            *bin = RustFftComplex::new(0.0, 0.0);
        }
        self.fft_buf[peak_bin] = peak_val;

        // Inverse FFT.
        self.fft_inverse
            .process_with_scratch(&mut self.fft_buf, &mut self.scratch);

        // Write normalized result to output.
        for (i, bin) in self.fft_buf[..n].iter().enumerate() {
            output[i] = Complex::new(bin.re * inv_n, bin.im * inv_n);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use rustfft::FftPlanner;

    /// Minimum energy ratio for NR tone preservation test.
    const MIN_ENERGY_RATIO: f32 = 0.05;

    // Squelch dB regression constants: amplitude 0.1 → -20 dBFS.
    const SQUELCH_REG_AMPLITUDE: f32 = 0.1;
    const SQUELCH_REG_BLOCK_LEN: usize = 100;
    const SQUELCH_REG_CLOSE_DB: f32 = -15.0;
    const SQUELCH_REG_OPEN_DB: f32 = -25.0;

    // --- Envelope test constants ---
    //
    // Canonical AF rate for envelope timing assertions. All block-
    // size and convergence-sample constants below are derived from
    // this rate paired with `SQUELCH_ATTACK_SECONDS` /
    // `SQUELCH_RELEASE_SECONDS` — keeping them centralized makes it
    // obvious which value to update if the time constants change.
    const ENV_TEST_SAMPLE_RATE_HZ: f32 = 48_000.0;
    /// Short block length used for "fresh envelope stays muted" and
    /// "tail preserved across close" assertions — ~2 ms at 48 kHz.
    const ENV_SHORT_BLOCK_SAMPLES: usize = 100;
    /// Long block length used to let the attack envelope converge
    /// fully before the convergence-sample assertion — 20 ms at
    /// 48 kHz covers ~10 attack time constants.
    const ENV_LONG_BLOCK_SAMPLES: usize = 1_000;
    /// Index at which the attack envelope is expected to have
    /// converged to within ~1% of the target. 500 samples at
    /// 48 kHz = 10 ms, which is 5 attack time constants
    /// (`SQUELCH_ATTACK_SECONDS = 0.002 s`) → `exp(-5) ≈ 0.0067`
    /// residual, well under the 0.99 gain assertion.
    const ENV_CONVERGENCE_SAMPLE_IDX: usize = 500;
    /// Numerical tolerance for "silenced" assertions. Envelope
    /// gain starts at literal 0.0 so output is exactly 0.0 by
    /// construction; the epsilon is pro-forma against future
    /// float-math drift.
    const ENV_SILENCE_EPSILON: f32 = 1e-6;
    /// Stereo sample amplitude used in envelope tests — arbitrary
    /// unit value so gain multiplication reads directly as the
    /// envelope state.
    const ENV_STEREO_SAMPLE_AMP: f32 = 1.0;

    // --- Power Squelch tests ---

    #[test]
    fn test_squelch_opens_on_strong_signal() {
        let mut squelch = PowerSquelch::new(-30.0);
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        squelch.process(&input, &mut output).unwrap();
        assert!(squelch.is_open(), "strong signal should open squelch");
        assert!(output[0].re > 0.0, "output should not be zeroed");
    }

    #[test]
    fn test_squelch_closes_on_weak_signal() {
        let mut squelch = PowerSquelch::new(10.0); // very high threshold
        let input = vec![Complex::new(0.001, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        squelch.process(&input, &mut output).unwrap();
        assert!(!squelch.is_open(), "weak signal should close squelch");
        assert!(
            output[0].re.abs() < 1e-10,
            "output should be zeroed when squelch closed"
        );
    }

    #[test]
    fn test_squelch_empty_input() {
        let mut squelch = PowerSquelch::new(-50.0);
        let input: &[Complex] = &[];
        let mut output: Vec<Complex> = vec![];
        let count = squelch.process(input, &mut output).unwrap();
        assert_eq!(count, 0);
        assert!(!squelch.is_open());
    }

    #[test]
    fn test_squelch_db_scale_regression() {
        // Pin the 20*log10(amplitude) scale: amplitude 0.1 → -20 dBFS.
        // A threshold at -15 dB should close, -25 dB should open.
        let input = vec![Complex::new(SQUELCH_REG_AMPLITUDE, 0.0); SQUELCH_REG_BLOCK_LEN];

        let mut squelch_close = PowerSquelch::new(SQUELCH_REG_CLOSE_DB);
        let mut output = vec![Complex::default(); SQUELCH_REG_BLOCK_LEN];
        squelch_close.process(&input, &mut output).unwrap();
        assert!(
            !squelch_close.is_open(),
            "amplitude 0.1 (-20 dB) should be below -15 dB threshold"
        );

        let mut squelch_open = PowerSquelch::new(SQUELCH_REG_OPEN_DB);
        squelch_open.process(&input, &mut output).unwrap();
        assert!(
            squelch_open.is_open(),
            "amplitude 0.1 (-20 dB) should be above -25 dB threshold"
        );
    }

    // --- Squelch audio envelope tests (#331) ---

    /// Fresh envelope starts muted: gate target 0.0, gain 0.0.
    /// A stereo buffer processed without any `set_gate_open` call
    /// should stay silent.
    #[test]
    fn squelch_audio_envelope_starts_muted() {
        use sdr_types::Stereo;
        let mut env = SquelchAudioEnvelope::new(ENV_TEST_SAMPLE_RATE_HZ);
        // Use a non-zero input so the "output is 0" assertion is
        // actually exercising the envelope (an all-zero input
        // would pass even if the envelope were broken).
        let mut buf = vec![Stereo { l: 0.5, r: 0.5 }; ENV_SHORT_BLOCK_SAMPLES];
        env.process_stereo(&mut buf);
        for (i, s) in buf.iter().enumerate() {
            assert!(
                s.l.abs() < ENV_SILENCE_EPSILON,
                "sample {i} L not silenced: {}",
                s.l
            );
            assert!(
                s.r.abs() < ENV_SILENCE_EPSILON,
                "sample {i} R not silenced: {}",
                s.r
            );
        }
    }

    /// Attack ramp: on gate open, the per-sample gain rises
    /// smoothly toward 1.0. After several ms the envelope has
    /// converged and the output matches the input.
    #[test]
    fn squelch_audio_envelope_ramps_up_on_gate_open() {
        use sdr_types::Stereo;
        let mut env = SquelchAudioEnvelope::new(ENV_TEST_SAMPLE_RATE_HZ);
        env.set_gate_open(true);
        let mut buf = vec![
            Stereo {
                l: ENV_STEREO_SAMPLE_AMP,
                r: ENV_STEREO_SAMPLE_AMP
            };
            ENV_LONG_BLOCK_SAMPLES
        ];
        env.process_stereo(&mut buf);
        // First sample: partially ramped up. Not the full input
        // (that would be a hard gate), not zero (that would be
        // stuck muted).
        assert!(buf[0].l > 0.0 && buf[0].l < 0.5);
        // By `ENV_CONVERGENCE_SAMPLE_IDX` (5 attack time constants
        // at 48 kHz) the envelope is within ~1% of the target.
        assert!(
            buf[ENV_CONVERGENCE_SAMPLE_IDX].l > 0.99,
            "envelope should converge by sample {ENV_CONVERGENCE_SAMPLE_IDX}, got {}",
            buf[ENV_CONVERGENCE_SAMPLE_IDX].l
        );
    }

    /// Release ramp on gate close: envelope decays smoothly
    /// toward 0 across ~5 ms. Tail of the previous block's audio
    /// is NOT clipped on the first sample after close.
    #[test]
    fn squelch_audio_envelope_preserves_tail_on_close() {
        use sdr_types::Stereo;
        let mut env = SquelchAudioEnvelope::new(ENV_TEST_SAMPLE_RATE_HZ);
        // Open and converge.
        env.set_gate_open(true);
        let mut ramp = vec![
            Stereo {
                l: ENV_STEREO_SAMPLE_AMP,
                r: ENV_STEREO_SAMPLE_AMP
            };
            ENV_LONG_BLOCK_SAMPLES
        ];
        env.process_stereo(&mut ramp);

        // Close and feed another block — first sample should
        // be close to full amplitude (tail preserved), not 0.
        env.set_gate_open(false);
        let mut buf = vec![
            Stereo {
                l: ENV_STEREO_SAMPLE_AMP,
                r: ENV_STEREO_SAMPLE_AMP
            };
            ENV_SHORT_BLOCK_SAMPLES
        ];
        env.process_stereo(&mut buf);
        assert!(
            buf[0].l > 0.9,
            "first post-close sample should preserve the tail, got {}",
            buf[0].l
        );
    }

    /// Reset snaps back to the closed-gate state — used by
    /// `RadioModule` on demod-mode change to prevent cross-
    /// pipeline gain bleed.
    #[test]
    fn squelch_audio_envelope_reset_clears_gain() {
        use sdr_types::Stereo;
        let mut env = SquelchAudioEnvelope::new(ENV_TEST_SAMPLE_RATE_HZ);
        env.set_gate_open(true);
        let mut buf = vec![
            Stereo {
                l: ENV_STEREO_SAMPLE_AMP,
                r: ENV_STEREO_SAMPLE_AMP
            };
            ENV_LONG_BLOCK_SAMPLES
        ];
        env.process_stereo(&mut buf);

        env.reset();
        // First post-reset block should be silent (gain back to
        // 0, target back to 0).
        let mut buf = vec![
            Stereo {
                l: ENV_STEREO_SAMPLE_AMP,
                r: ENV_STEREO_SAMPLE_AMP
            };
            ENV_SHORT_BLOCK_SAMPLES
        ];
        env.process_stereo(&mut buf);
        for s in &buf {
            assert!(s.l.abs() < ENV_SILENCE_EPSILON);
        }
    }

    /// `envelope_coefficient` pathological-input guards —
    /// non-finite, zero, or negative rate / tau fall back to
    /// instant-transition `1.0` so misconfiguration degrades to
    /// the hard-gate behavior rather than panicking on divide-
    /// by-zero OR poisoning the envelope with NaN / Inf.
    #[test]
    fn envelope_coefficient_handles_pathological_inputs() {
        let coeff = envelope_coefficient(SQUELCH_ATTACK_SECONDS, ENV_TEST_SAMPLE_RATE_HZ);
        assert!(coeff > 0.0 && coeff < 1.0);
        assert_eq!(envelope_coefficient(SQUELCH_ATTACK_SECONDS, 0.0), 1.0);
        assert_eq!(
            envelope_coefficient(SQUELCH_ATTACK_SECONDS, -ENV_TEST_SAMPLE_RATE_HZ),
            1.0
        );
        assert_eq!(envelope_coefficient(0.0, ENV_TEST_SAMPLE_RATE_HZ), 1.0);
        assert_eq!(
            envelope_coefficient(-SQUELCH_ATTACK_SECONDS, ENV_TEST_SAMPLE_RATE_HZ),
            1.0
        );
        // Non-finite guards: NaN and ±Inf on either input must
        // fall back to 1.0, not propagate into downstream
        // `envelope_gain` math where they'd poison every block.
        assert_eq!(envelope_coefficient(f32::NAN, ENV_TEST_SAMPLE_RATE_HZ), 1.0);
        assert_eq!(envelope_coefficient(SQUELCH_ATTACK_SECONDS, f32::NAN), 1.0);
        assert_eq!(
            envelope_coefficient(f32::INFINITY, ENV_TEST_SAMPLE_RATE_HZ),
            1.0
        );
        assert_eq!(
            envelope_coefficient(SQUELCH_ATTACK_SECONDS, f32::INFINITY),
            1.0
        );
    }

    // --- Noise Blanker tests ---

    #[test]
    fn test_blanker_new_invalid() {
        assert!(NoiseBlanker::new(0.0, 5.0).is_err());
        assert!(NoiseBlanker::new(1.0, 5.0).is_err());
        assert!(NoiseBlanker::new(0.1, 0.5).is_err()); // level < 1.0
        assert!(NoiseBlanker::new(f32::NAN, 5.0).is_err());
        assert!(NoiseBlanker::new(0.1, f32::NAN).is_err());
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
    fn test_blanker_skips_ema_on_zero_samples() {
        let mut nb = NoiseBlanker::new(0.5, 3.0).unwrap();
        // Settle the EMA with a known amplitude.
        let normal = vec![Complex::new(1.0, 0.0); 100];
        let mut out = vec![Complex::default(); 100];
        nb.process(&normal, &mut out).unwrap();
        let amp_before = nb.amp;

        // Feed zero samples — EMA should NOT decay.
        let zeros = vec![Complex::default(); 100];
        nb.process(&zeros, &mut out).unwrap();
        assert!(
            (nb.amp - amp_before).abs() < 1e-6,
            "EMA should not change on zero samples, was {amp_before}, now {}",
            nb.amp
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
    fn test_fm_if_nr_preserves_tone() {
        use core::f32::consts::PI;

        // Generate a pure tone at bin 8 of a 256-point FFT — it should survive NR.
        let fft_size = 256;
        let mut nr = FmIfNoiseReduction::with_fft_size(fft_size).unwrap();
        let tone_bin = 8;
        let input: Vec<Complex> = (0..fft_size)
            .map(|i| {
                let phase = 2.0 * PI * (tone_bin as f32) * (i as f32) / (fft_size as f32);
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![Complex::default(); fft_size];
        let count = nr.process(&input, &mut output).unwrap();
        assert_eq!(count, fft_size);

        // Verify the dominant output bin matches the input tone bin.
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);
        let mut spectrum: Vec<RustFftComplex<f32>> = output
            .iter()
            .map(|s| RustFftComplex::new(s.re, s.im))
            .collect();
        fft.process(&mut spectrum);
        let dominant_bin = spectrum
            .iter()
            .enumerate()
            .max_by(|a, b| {
                let ma = a.1.re * a.1.re + a.1.im * a.1.im;
                let mb = b.1.re * b.1.re + b.1.im * b.1.im;
                ma.partial_cmp(&mb).unwrap()
            })
            .map_or(0, |(i, _)| i);
        assert_eq!(
            dominant_bin, tone_bin,
            "recovered dominant bin should match tone_bin"
        );

        // Energy should be above a minimum floor (Nuttall window + single-bin
        // selection reduces passthrough to ~10-15%).
        let energy: f32 = output.iter().map(|s| s.re * s.re + s.im * s.im).sum();
        let input_energy: f32 = input.iter().map(|s| s.re * s.re + s.im * s.im).sum();
        assert!(
            energy > input_energy * MIN_ENERGY_RATIO,
            "tone energy ratio {} below MIN_ENERGY_RATIO {MIN_ENERGY_RATIO}",
            energy / input_energy
        );
    }

    #[test]
    fn test_fm_if_nr_reduces_noise() {
        // A tone + broadband noise: output energy should be less than input energy
        // because NR zeroes the noise bins.
        let fft_size = 256;
        let mut nr = FmIfNoiseReduction::with_fft_size(fft_size).unwrap();

        // Deterministic "noise": many tones across the spectrum.
        let input: Vec<Complex> = (0..fft_size)
            .map(|i| {
                let tone = (2.0 * core::f32::consts::PI * 8.0 * (i as f32) / fft_size as f32).cos();
                // Add energy at many other bins (pseudo-noise).
                let noise = (0.3 * (i as f32 * 1.7).sin())
                    + (0.2 * (i as f32 * 3.1).cos())
                    + (0.15 * (i as f32 * 7.3).sin());
                Complex::new(tone + noise, 0.0)
            })
            .collect();
        let mut output = vec![Complex::default(); fft_size];
        nr.process(&input, &mut output).unwrap();

        let input_energy: f32 = input.iter().map(|s| s.re * s.re + s.im * s.im).sum();
        let output_energy: f32 = output.iter().map(|s| s.re * s.re + s.im * s.im).sum();
        assert!(
            output_energy < input_energy * 0.9,
            "NR should reduce broadband energy, ratio = {}",
            output_energy / input_energy
        );
    }

    #[test]
    fn test_fm_if_nr_invalid_size() {
        assert!(FmIfNoiseReduction::with_fft_size(0).is_err());
    }

    #[test]
    fn test_fm_if_nr_buffer_too_small() {
        let mut nr = FmIfNoiseReduction::new().unwrap();
        let input = [Complex::default(); 300];
        let mut output = [Complex::default(); 100];
        assert!(nr.process(&input, &mut output).is_err());
    }

    #[test]
    fn test_buffer_too_small() {
        let mut squelch = PowerSquelch::new(-50.0);
        let input = [Complex::default(); 10];
        let mut output = [Complex::default(); 5];
        assert!(squelch.process(&input, &mut output).is_err());
    }

    // --- Auto-squelch tests ---

    // Auto-squelch test constants.
    const AUTO_SETTLE_ITERS: usize = 200;
    const TEST_BLOCK_LEN: usize = 100;
    const NOISE_AMP: f32 = 0.001;
    const STRONG_AMP: f32 = 1.0;
    const BORDERLINE_AMP: f32 = 0.003;

    #[test]
    fn test_auto_squelch_tracks_noise_floor() {
        let mut squelch = PowerSquelch::new(-100.0);
        squelch.set_auto_squelch(true);
        assert!(squelch.auto_squelch_enabled());

        // Feed many blocks of low-level noise to settle the noise floor estimate.
        let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
        let mut output = vec![Complex::default(); 100];
        for _ in 0..AUTO_SETTLE_ITERS {
            squelch.process(&noise, &mut output).unwrap();
        }

        // Noise floor should have settled near the noise level (-60 dBFS for 0.001).
        let floor = squelch.noise_floor_db();
        assert!(
            floor > -70.0 && floor < -50.0,
            "noise floor should be near -60 dB, got {floor}"
        );
    }

    #[test]
    fn test_auto_squelch_opens_on_signal() {
        let mut squelch = PowerSquelch::new(-100.0);
        squelch.set_auto_squelch(true);

        // Settle noise floor with weak signal.
        let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
        let mut output = vec![Complex::default(); 100];
        for _ in 0..AUTO_SETTLE_ITERS {
            squelch.process(&noise, &mut output).unwrap();
        }
        assert!(!squelch.is_open(), "should be closed on noise-only");

        // Inject a strong signal — should open.
        let signal = vec![Complex::new(STRONG_AMP, 0.0); TEST_BLOCK_LEN];
        squelch.process(&signal, &mut output).unwrap();
        assert!(squelch.is_open(), "should open on strong signal");
    }

    #[test]
    fn test_auto_squelch_hysteresis() {
        let mut squelch = PowerSquelch::new(-100.0);
        squelch.set_auto_squelch(true);

        // Settle noise floor.
        let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
        let mut output = vec![Complex::default(); 100];
        for _ in 0..AUTO_SETTLE_ITERS {
            squelch.process(&noise, &mut output).unwrap();
        }

        // Open squelch with a strong signal.
        let strong = vec![Complex::new(STRONG_AMP, 0.0); TEST_BLOCK_LEN];
        squelch.process(&strong, &mut output).unwrap();
        assert!(squelch.is_open());

        // A borderline signal just above the close margin should stay open
        // (hysteresis: close margin is lower than open margin).
        // Noise floor is ~-60 dB, close margin is +6 dB = -54 dB.
        // Amplitude of 0.003 ≈ -50.5 dB, which is above -54 dB.
        let borderline = vec![Complex::new(BORDERLINE_AMP, 0.0); TEST_BLOCK_LEN];
        squelch.process(&borderline, &mut output).unwrap();
        assert!(
            squelch.is_open(),
            "borderline signal should keep squelch open due to hysteresis"
        );
    }

    #[test]
    fn test_auto_squelch_ignores_manual_level() {
        let mut squelch = PowerSquelch::new(100.0); // impossibly high manual threshold
        squelch.set_auto_squelch(true);

        // Settle noise floor.
        let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
        let mut output = vec![Complex::default(); 100];
        for _ in 0..AUTO_SETTLE_ITERS {
            squelch.process(&noise, &mut output).unwrap();
        }

        // Strong signal should still open despite manual level of 100 dB.
        let strong = vec![Complex::new(STRONG_AMP, 0.0); TEST_BLOCK_LEN];
        squelch.process(&strong, &mut output).unwrap();
        assert!(squelch.is_open(), "auto-squelch should ignore manual level");
    }

    #[test]
    fn test_auto_squelch_disable_reverts_to_manual() {
        let mut squelch = PowerSquelch::new(100.0); // impossibly high manual threshold
        squelch.set_auto_squelch(true);

        // Settle noise floor and open with signal.
        let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
        let mut output = vec![Complex::default(); 100];
        for _ in 0..AUTO_SETTLE_ITERS {
            squelch.process(&noise, &mut output).unwrap();
        }
        let strong = vec![Complex::new(STRONG_AMP, 0.0); TEST_BLOCK_LEN];
        squelch.process(&strong, &mut output).unwrap();
        assert!(squelch.is_open());

        // Disable auto-squelch — should revert to manual 100 dB threshold.
        squelch.set_auto_squelch(false);
        squelch.process(&strong, &mut output).unwrap();
        assert!(
            !squelch.is_open(),
            "with auto-squelch off, manual 100 dB threshold should close squelch"
        );
    }
}
