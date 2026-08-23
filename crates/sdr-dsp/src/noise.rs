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

/// Slow-creep EMA coefficient used when auto-squelch is open
/// AND the measured signal sits well above the tracked noise
/// floor. Without this, the "only update when closed or near
/// noise" rule below locks `noise_floor_db` at whatever it
/// converged to at the end of the settle window. On a channel
/// with a persistent carrier that settle often overshoots, and
/// the gate stays open forever.
///
/// Picked so a carrier 25 dB above noise pulls the floor close
/// enough to the close-margin in ~7 seconds (25 dB * 0.002 per
/// block * 50 blocks/sec ≈ 2.5 dB/sec → needs ~8s to close the
/// gap to the 6 dB close margin). Slow enough that a 1-2 sec
/// voice burst doesn't materially shift the floor (EMA rise in
/// 2s at this alpha ≈ 0.2 * gap → negligible for short bursts).
/// Per issue #348.
const NOISE_FLOOR_STUCK_RECOVERY_ALPHA: f32 = 0.002;

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
/// Gain below which a closing release ramp is inaudible (−80 dB) and
/// snaps to exact zero — see [`SquelchAudioEnvelope::settle_if_closed`].
const SQUELCH_SETTLED_GAIN: f32 = 1e-4;

pub struct SquelchAudioEnvelope {
    envelope_gain: f32,
    target: f32,
    attack_coeff: f32,
    release_coeff: f32,
}

impl SquelchAudioEnvelope {
    /// Validate that `sample_rate_hz` is usable for envelope
    /// coefficient math: finite (not NaN / ±Inf) and strictly
    /// positive. The inner `envelope_coefficient` helper already
    /// degrades gracefully to 1.0 on bad inputs, but that
    /// silently reinstates the exact hard-gate behavior this
    /// type was introduced to avoid — surfacing the error at
    /// the public API boundary means upstream misconfiguration
    /// fails loudly instead of appearing as a stale "pop"
    /// regression.
    fn validate_sample_rate(sample_rate_hz: f32) -> Result<(), DspError> {
        if !sample_rate_hz.is_finite() || sample_rate_hz <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "sample_rate_hz must be finite and > 0, got {sample_rate_hz}"
            )));
        }
        Ok(())
    }

    /// Construct an envelope for audio running at
    /// `sample_rate_hz`. Coefficients are seeded from the
    /// `SQUELCH_ATTACK_SECONDS` / `SQUELCH_RELEASE_SECONDS` time
    /// constants; call [`set_sample_rate`] if the audio rate
    /// changes at runtime.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` when `sample_rate_hz`
    /// is non-finite or non-positive.
    pub fn new(sample_rate_hz: f32) -> Result<Self, DspError> {
        Self::validate_sample_rate(sample_rate_hz)?;
        Ok(Self {
            envelope_gain: 0.0,
            target: 0.0,
            attack_coeff: envelope_coefficient(SQUELCH_ATTACK_SECONDS, sample_rate_hz),
            release_coeff: envelope_coefficient(SQUELCH_RELEASE_SECONDS, sample_rate_hz),
        })
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
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` when `sample_rate_hz`
    /// is non-finite or non-positive. The existing `envelope_gain`
    /// and coefficients are left untouched on error so a bad
    /// rate update can't corrupt the envelope.
    pub fn set_sample_rate(&mut self, sample_rate_hz: f32) -> Result<(), DspError> {
        Self::validate_sample_rate(sample_rate_hz)?;
        self.attack_coeff = envelope_coefficient(SQUELCH_ATTACK_SECONDS, sample_rate_hz);
        self.release_coeff = envelope_coefficient(SQUELCH_RELEASE_SECONDS, sample_rate_hz);
        Ok(())
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

    /// Once the release ramp has decayed below [`SQUELCH_SETTLED_GAIN`]
    /// while the gate is closed, snap the gain to exactly zero and
    /// report `true`; the caller can then hard-zero the block instead
    /// of multiplying by a vanishing gain forever. The exponential
    /// release never reaches zero on its own (#738).
    pub fn settle_if_closed(&mut self) -> bool {
        if self.target > 0.0 {
            return false;
        }
        if self.envelope_gain <= SQUELCH_SETTLED_GAIN {
            self.envelope_gain = 0.0;
            return true;
        }
        false
    }

    /// Apply the envelope to a stereo audio buffer in place.
    /// Both L and R receive the same per-sample gain so stereo
    /// imaging is preserved across the transition.
    pub fn process_stereo(&mut self, buf: &mut [sdr_types::Stereo]) {
        // `target` is stable for the call, and the rising /
        // falling direction depends on `target > envelope_gain`.
        // Within one call `envelope_gain` only crosses the target
        // asymptotically (IIR, never overshoots by construction)
        // so the direction — and therefore the coefficient — is
        // constant for the whole buffer. Hoisting the branch out
        // of the hot loop removes a per-sample compare.
        let coeff = if self.target > self.envelope_gain {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        for s in buf.iter_mut() {
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
    /// Last `measured_db` this processor computed. Exposed via
    /// `diagnostic_snapshot` for issue #348 investigation; no
    /// behavioral effect.
    last_measured_db: f32,
    /// Zero the block while the gate is closed (SDR++ behaviour,
    /// the default). `false` keeps tracking the gate but passes the
    /// block through so a downstream stage can keep an ungated copy
    /// and apply the mute itself (#734).
    mute_closed: bool,
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
            last_measured_db: NOISE_FLOOR_INITIAL_DB,
            mute_closed: true,
        }
    }

    /// Choose whether a closed gate zeroes the block (`true`, the
    /// SDR++ default) or passes it through while still reporting
    /// [`Self::is_open`] as `false`. See the `mute_closed` field.
    pub fn set_mute_closed(&mut self, mute: bool) {
        self.mute_closed = mute;
    }

    /// Returns whether the squelch is currently open (signal above threshold).
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Update the squelch threshold.
    pub fn set_level(&mut self, level_db: f32) {
        self.level_db = level_db;
    }

    /// Reset the auto-squelch tracking state — noise floor back
    /// to the initial sentinel, settle counter to zero — so the
    /// next block starts converging from scratch. Shared between
    /// `set_auto_squelch(true)` (enabling the tracker) and
    /// `rearm_auto_squelch` (re-converging on a new band without
    /// flipping the enabled flag).
    fn reset_auto_squelch_tracking(&mut self) {
        self.noise_floor_db = NOISE_FLOOR_INITIAL_DB;
        self.settle_count = 0;
    }

    /// Enable or disable auto-squelch (noise floor tracking).
    ///
    /// When enabled, the manual `level_db` is ignored and the threshold
    /// is derived from the tracked noise floor plus a margin.
    pub fn set_auto_squelch(&mut self, enabled: bool) {
        self.auto_squelch = enabled;
        if enabled {
            // Reset the noise floor estimate so it adapts to the current band.
            self.reset_auto_squelch_tracking();
        }
    }

    /// Re-arm auto-squelch: reset the noise floor estimate and
    /// settle counter without flipping the enabled state.
    ///
    /// Call this whenever the tuning context changes — a retune
    /// to a new frequency, a demod-mode switch, or a bandwidth
    /// change — so the auto-squelch tracker re-converges on the
    /// new band's floor instead of carrying the previous
    /// channel's settled estimate into a potentially very
    /// different noise environment. No-op when auto-squelch is
    /// disabled (manual threshold doesn't track floor).
    pub fn rearm_auto_squelch(&mut self) {
        if self.auto_squelch {
            self.reset_auto_squelch_tracking();
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
            // Reset the diagnostic field so an empty-input
            // call that closes the gate doesn't surface the
            // previous block's power in downstream logs. Per
            // CodeRabbit round 1 on PR #350.
            self.last_measured_db = f32::NEG_INFINITY;
            return Ok(0);
        }

        // Mean amplitude, as SDR++ measures it (`volk_32fc_magnitude_32f`
        // + accumulate). The dB conversion deliberately differs: SDR++
        // tests `10·log10(mean_amplitude) >= level`, which is neither
        // amplitude nor power dB; this port uses the dBFS amplitude
        // convention `20·log10(amplitude)` so the threshold the user
        // sets matches the spectrum display. The default level and the
        // panel range are calibrated to this scale (−100 dB default
        // versus SDR++'s −50), so the two are not interchangeable (#775).
        let sum: f32 = input.iter().map(|s| s.amplitude()).sum();
        let mean_amplitude = sum / input.len() as f32;

        // A non-finite sample (an overloaded or broken source) must not
        // reach the noise-floor tracker: +inf would pin the floor at
        // +inf for good and NaN drags it off during settling. Close the
        // gate for this block and leave every estimate untouched (#775).
        if !mean_amplitude.is_finite() {
            self.last_measured_db = f32::NAN;
            output[..input.len()].fill(Complex::default());
            self.open = false;
            return Ok(input.len());
        }
        let measured_db = 20.0 * mean_amplitude.max(f32::MIN_POSITIVE).log10();
        self.last_measured_db = measured_db;

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
                // Near-noise regime: gate is closed or the
                // signal is within close-margin of the tracked
                // floor. Track at the normal alpha; this is the
                // hot path whenever the channel is actually
                // quiet.
                self.noise_floor_db = NOISE_FLOOR_ALPHA
                    .mul_add(measured_db, (1.0 - NOISE_FLOOR_ALPHA) * self.noise_floor_db);
            } else {
                // Gate is open AND signal is well above the
                // tracked floor — typical mid-transmission
                // case. Previous behavior: skip the update
                // entirely (keeps the floor from being dragged
                // up by a brief signal). Problem: on a channel
                // with a persistent carrier, OR when the
                // settle window happened to capture live
                // signal, the floor never recovers and the
                // gate is stuck open forever. Fix: creep
                // upward at a much slower alpha so a truly
                // persistent signal pulls the floor close
                // enough to the close margin within ~7
                // seconds, restoring normal gate dynamics.
                // Brief voice bursts barely move the floor at
                // this rate. Per issue #348.
                self.noise_floor_db = NOISE_FLOOR_STUCK_RECOVERY_ALPHA.mul_add(
                    measured_db,
                    (1.0 - NOISE_FLOOR_STUCK_RECOVERY_ALPHA) * self.noise_floor_db,
                );
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

        if measured_db >= threshold_db || !self.mute_closed {
            output[..input.len()].copy_from_slice(input);
            self.open = measured_db >= threshold_db;
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

    /// Snapshot of squelch internals for callers that want to
    /// log state at their layer (tracing belongs in the crate
    /// that already depends on it — `sdr-dsp` stays pure).
    pub fn diagnostic_snapshot(&self) -> PowerSquelchDiagnostic {
        PowerSquelchDiagnostic {
            auto_squelch: self.auto_squelch,
            open: self.open,
            noise_floor_db: self.noise_floor_db,
            settle_count: self.settle_count,
            level_db: self.level_db,
            last_measured_db: self.last_measured_db,
        }
    }
}

/// Snapshot of [`PowerSquelch`] internals for diagnostic logging
/// by higher-level crates that already depend on `tracing`.
/// Never correlated with audio content — just gate state.
#[derive(Debug, Clone, Copy)]
pub struct PowerSquelchDiagnostic {
    pub auto_squelch: bool,
    pub open: bool,
    pub noise_floor_db: f32,
    pub settle_count: u32,
    pub level_db: f32,
    pub last_measured_db: f32,
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
    /// Processed samples not yet handed to a caller. Every input sample is
    /// emitted exactly once, in order, from here — never as a raw tail copy
    /// (which double-emitted and reordered the stream, #773). Latency is
    /// bounded by two FFT blocks.
    ready: std::collections::VecDeque<Complex>,
    /// Scratch for one processed block before it is queued.
    block_out: Vec<Complex>,
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
            ready: std::collections::VecDeque::with_capacity(2 * fft_size),
            block_out: vec![Complex::default(); fft_size],
        })
    }

    /// Discard buffered input and queued output (e.g. on retune).
    pub fn reset(&mut self) {
        self.overlap_count = 0;
        self.ready.clear();
    }

    /// End of stream: process the partially filled block (zero-padded),
    /// keeping only its real samples, and hand out everything still
    /// queued. After this every sample ever passed to `process` has been
    /// emitted exactly once. Returns the number of samples written;
    /// calling it again emits nothing.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output` cannot hold all
    /// pending samples (at most `2 * fft_size`).
    pub fn flush(&mut self, output: &mut [Complex]) -> Result<usize, DspError> {
        if self.overlap_count > 0 {
            let valid = self.overlap_count;
            self.overlap_buf[valid..].fill(Complex::default());
            let mut block = std::mem::take(&mut self.block_out);
            self.process_block(&mut block);
            self.ready.extend(block[..valid].iter().copied());
            self.block_out = block;
            self.overlap_count = 0;
        }
        let pending = self.ready.len();
        if output.len() < pending {
            return Err(DspError::BufferTooSmall {
                need: pending,
                got: output.len(),
            });
        }
        for (dst, src) in output[..pending].iter_mut().zip(self.ready.drain(..)) {
            *dst = src;
        }
        Ok(pending)
    }

    /// Process complex samples through FFT-based noise reduction.
    ///
    /// Input is accumulated into FFT-size blocks; each completed block is
    /// processed and queued, and as many queued samples as fit are written
    /// to `output`. The returned count may be smaller than `input.len()`
    /// (up to two blocks of latency) but the output stream is always the
    /// processed signal, in order, with every sample emitted exactly once,
    /// independent of how the input is chunked. Call [`Self::flush`] at
    /// end of stream to emit the final partial block.
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

        let mut in_pos = 0;
        while in_pos < input.len() {
            let can_take = (self.fft_size - self.overlap_count).min(input.len() - in_pos);
            self.overlap_buf[self.overlap_count..self.overlap_count + can_take]
                .copy_from_slice(&input[in_pos..in_pos + can_take]);
            self.overlap_count += can_take;
            in_pos += can_take;

            if self.overlap_count == self.fft_size {
                let mut block = std::mem::take(&mut self.block_out);
                self.process_block(&mut block);
                self.ready.extend(block.iter().copied());
                self.block_out = block;
                self.overlap_count = 0;
            }
        }

        let emit = self.ready.len().min(output.len());
        for (dst, src) in output[..emit].iter_mut().zip(self.ready.drain(..emit)) {
            *dst = src;
        }
        Ok(emit)
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
mod tests;
