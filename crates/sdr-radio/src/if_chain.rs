//! IF (Intermediate Frequency) processing chain.
//!
//! Applies optional noise blanking, squelch, and FM IF noise reduction
//! to complex IQ samples before demodulation.

use sdr_dsp::loops::Agc;
use sdr_dsp::noise::{FmIfNoiseReduction, NoiseBlanker, PowerSquelch};
use sdr_types::{Complex, DspError};

/// Default noise blanker tracking rate.
const NB_DEFAULT_RATE: f32 = 0.05;

/// Default noise blanker threshold multiplier.
const NB_DEFAULT_LEVEL: f32 = 5.0;

/// Default squelch threshold in dB.
const SQUELCH_DEFAULT_LEVEL_DB: f32 = -100.0;

// Software IF AGC parameters. Mirror the AM demod's carrier AGC
// tuning so the envelope behavior is consistent across the two
// AGC sites we run on complex IQ (AM's pre-envelope carrier AGC
// vs. this pre-demod IF AGC). Coefficient units are "EMA alpha"
// — sample-count-based, not time-based — so the effective time
// constant in seconds drifts with the IF sample rate. At the
// common post-decimation rates (~200-500 kHz) this puts attack
// in the ~1 ms ballpark and release ~10 ms, which is fast
// enough to track real RF fades without pumping on voice
// modulation.
/// Software AGC set point (target mean IQ amplitude).
const SOFTWARE_AGC_SET_POINT: f32 = 1.0;
/// Software AGC attack coefficient (1/300 ≈ 300-sample time constant).
const SOFTWARE_AGC_ATTACK: f32 = 0.003_333_333;
/// Software AGC decay coefficient (1/3000 ≈ 3000-sample time constant).
const SOFTWARE_AGC_DECAY: f32 = 0.000_333_333;
/// Software AGC maximum gain ceiling. Prevents noise blow-up on
/// a dead channel where the envelope tracker would otherwise
/// amplify the noise floor to full scale.
const SOFTWARE_AGC_MAX_GAIN: f32 = 1e6;
/// Software AGC maximum output amplitude (look-ahead clipping cap).
/// Matches AM's carrier AGC — leaves ~20 dB of headroom against
/// the default `1.0` set point for transient overshoots.
const SOFTWARE_AGC_MAX_OUTPUT: f32 = 10.0;
/// Software AGC initial gain (pre-settling), neutral 1.0 so the
/// first block before convergence is unity-scaled.
const SOFTWARE_AGC_INIT_GAIN: f32 = 1.0;

/// IF processing chain — applied to complex IQ before demodulation.
///
/// Contains optional processors that can be individually enabled/disabled.
/// Processing order, in sequence:
///
/// 1. **Noise blanker** — attenuates impulse noise spikes on raw IQ.
/// 2. **Power squelch** — gates signal based on raw-IQ mean amplitude.
/// 3. **Software AGC** — normalizes IQ amplitude for downstream demod.
/// 4. **FM IF noise reduction** — frequency-domain noise removal for FM.
///
/// Software AGC sits **after** the squelch so the squelch threshold
/// still reads a non-normalized amplitude and can distinguish signal
/// from noise. If AGC ran first, every block would look "above
/// threshold" and the gate would stay open — same failure mode as
/// the tuner hardware AGC ↔ squelch interaction documented in #332.
/// FM IF NR sits after AGC so the frequency-domain peak-tracking
/// operates on a scale-normalized input, which stabilizes its
/// peak-bin selection across fading.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one enable flag per DSP stage is the cleanest representation — the stages are orthogonal (a user can independently enable NB, squelch, software AGC, and FM IF NR), and grouping them into a bitfield would obscure the process-order documentation above"
)]
pub struct IfChain {
    nb: NoiseBlanker,
    nb_enabled: bool,
    squelch: PowerSquelch,
    squelch_enabled: bool,
    /// Software IF AGC — normalizes IQ amplitude on the DSP side
    /// so downstream demod sees a level-consistent signal regardless
    /// of RF input strength. Independent of the tuner's hardware
    /// AGC (which fights strong signals at the RF stage, producing
    /// overshoots that propagate as audio distortion — see #332 /
    /// #354). Users pick between Off / Hardware / Software via the
    /// UI selector landing in #356 and #357.
    software_agc: Agc,
    software_agc_enabled: bool,
    fm_if_nr: FmIfNoiseReduction,
    fm_if_nr_enabled: bool,
    /// Scratch buffer A for ping-pong processing.
    buf_a: Vec<Complex>,
    /// Scratch buffer B for ping-pong processing.
    buf_b: Vec<Complex>,
}

impl IfChain {
    /// Create a new IF chain with all processors disabled.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the noise blanker cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let software_agc = Agc::new(
            SOFTWARE_AGC_SET_POINT,
            SOFTWARE_AGC_ATTACK,
            SOFTWARE_AGC_DECAY,
            SOFTWARE_AGC_MAX_GAIN,
            SOFTWARE_AGC_MAX_OUTPUT,
            SOFTWARE_AGC_INIT_GAIN,
        )?;
        Ok(Self {
            nb: NoiseBlanker::new(NB_DEFAULT_RATE, NB_DEFAULT_LEVEL)?,
            nb_enabled: false,
            squelch: PowerSquelch::new(SQUELCH_DEFAULT_LEVEL_DB),
            squelch_enabled: false,
            software_agc,
            software_agc_enabled: false,
            fm_if_nr: FmIfNoiseReduction::new()?,
            fm_if_nr_enabled: false,
            buf_a: Vec::new(),
            buf_b: Vec::new(),
        })
    }

    /// Enable or disable the noise blanker.
    pub fn set_nb_enabled(&mut self, enabled: bool) {
        self.nb_enabled = enabled;
    }

    /// Returns whether the noise blanker is enabled.
    pub fn nb_enabled(&self) -> bool {
        self.nb_enabled
    }

    /// Set the noise blanker threshold level.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the level is invalid.
    pub fn set_nb_level(&mut self, level: f32) -> Result<(), DspError> {
        self.nb = NoiseBlanker::new(NB_DEFAULT_RATE, level)?;
        Ok(())
    }

    /// Enable or disable the power squelch.
    pub fn set_squelch_enabled(&mut self, enabled: bool) {
        self.squelch_enabled = enabled;
    }

    /// Returns whether the squelch is enabled.
    pub fn squelch_enabled(&self) -> bool {
        self.squelch_enabled
    }

    /// Set the squelch threshold in dB.
    pub fn set_squelch_level(&mut self, db: f32) {
        self.squelch.set_level(db);
    }

    /// Choose whether a closed squelch zeroes the IQ (default, the
    /// SDR++ behaviour) or only reports the gate state via
    /// [`Self::squelch_open`]. `RadioModule` turns muting off and
    /// applies the exact-zero mute post-demod instead, so the imaging
    /// taps can read ungated audio (#734). The AGC skip while the gate
    /// is closed is keyed on the gate state, not on the zeros, so it is
    /// unaffected.
    pub fn set_squelch_mutes_iq(&mut self, mute: bool) {
        self.squelch.set_mute_closed(mute);
    }

    /// Enable or disable auto-squelch (noise floor tracking).
    ///
    /// When enabled, the squelch threshold is automatically derived from
    /// the tracked noise floor. The manual squelch level is ignored.
    pub fn set_auto_squelch_enabled(&mut self, enabled: bool) {
        self.squelch.set_auto_squelch(enabled);
    }

    /// Re-arm auto-squelch noise-floor tracking without
    /// flipping the enabled state. See
    /// [`PowerSquelch::rearm_auto_squelch`] for context.
    pub fn rearm_auto_squelch(&mut self) {
        self.squelch.rearm_auto_squelch();
    }

    /// Returns whether auto-squelch is enabled.
    pub fn auto_squelch_enabled(&self) -> bool {
        self.squelch.auto_squelch_enabled()
    }

    /// Returns whether the squelch is currently open (signal above threshold).
    pub fn squelch_open(&self) -> bool {
        let active = self.squelch_enabled || self.squelch.auto_squelch_enabled();
        !active || self.squelch.is_open()
    }

    /// Returns whether the squelch is actively gating — i.e.,
    /// manual squelch is enabled OR auto-squelch is enabled.
    /// Downstream consumers (the AF-level squelch envelope in
    /// `RadioModule::process`) skip their per-sample attenuation
    /// when this is `false` because the gate would never close
    /// anyway; running the envelope would mute the initial
    /// audio samples while the envelope ramps up from 0 for no
    /// reason.
    pub fn squelch_active(&self) -> bool {
        self.squelch_enabled || self.squelch.auto_squelch_enabled()
    }

    /// Enable or disable the software IF AGC.
    ///
    /// When enabled, a per-sample envelope follower normalizes IQ
    /// amplitude toward [`SOFTWARE_AGC_SET_POINT`] before the
    /// signal reaches FM IF NR and the demod. Users pick between
    /// this and the tuner's hardware AGC via the Linux / Mac UI
    /// selector shipping in #356 / #357; the engine-level flag
    /// starts at `false` so nothing changes until the UI wires in.
    pub fn set_software_agc_enabled(&mut self, enabled: bool) {
        if self.software_agc_enabled != enabled {
            // Reset the envelope tracker on toggle so a stale
            // gain state from the previous enabled run doesn't
            // bleed into the first post-re-enable block. Gain
            // goes back to the initial neutral value and the
            // envelope reconverges against live input.
            self.software_agc.reset();
        }
        self.software_agc_enabled = enabled;
    }

    /// Returns whether the software AGC is enabled.
    pub fn software_agc_enabled(&self) -> bool {
        self.software_agc_enabled
    }

    /// Enable or disable FM IF noise reduction.
    pub fn set_fm_if_nr_enabled(&mut self, enabled: bool) {
        if self.fm_if_nr_enabled != enabled {
            // The block-based NR holds partial input and queued output;
            // never let a pre-toggle remainder leak into the new session.
            self.fm_if_nr.reset();
        }
        self.fm_if_nr_enabled = enabled;
    }

    /// Returns whether FM IF noise reduction is enabled.
    pub fn fm_if_nr_enabled(&self) -> bool {
        self.fm_if_nr_enabled
    }

    /// End of stream: emit any samples the block-based FM IF noise
    /// reduction still holds (see `FmIfNoiseReduction::flush`). A no-op
    /// returning 0 when the stage is disabled.
    ///
    /// # Errors
    ///
    /// Propagates `DspError::BufferTooSmall` from the NR stage.
    pub fn flush(&mut self, output: &mut [Complex]) -> Result<usize, DspError> {
        if self.fm_if_nr_enabled {
            self.fm_if_nr.flush(output)
        } else {
            Ok(0)
        }
    }

    /// Process complex IF samples through the enabled chain stages.
    ///
    /// Processing order: noise blanker -> squelch -> software AGC ->
    /// FM IF NR. Uses ping-pong buffers to avoid aliasing between
    /// input and output.
    ///
    /// # Errors
    ///
    /// Returns `DspError` on buffer size or processing errors.
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

        let squelch_active = self.squelch_enabled || self.squelch.auto_squelch_enabled();
        let any_enabled =
            self.nb_enabled || squelch_active || self.software_agc_enabled || self.fm_if_nr_enabled;
        if !any_enabled {
            output[..input.len()].copy_from_slice(input);
            return Ok(input.len());
        }

        let mut n = input.len();
        self.buf_a.resize(n, Complex::default());
        self.buf_b.resize(n, Complex::default());

        // Copy input into buf_a as the starting point
        self.buf_a[..n].copy_from_slice(input);
        // Track which buffer holds the current data (true = A, false = B)
        let mut current_is_a = true;

        // Stage 1: Noise blanker (buf_a -> buf_b or buf_b -> buf_a)
        if self.nb_enabled {
            if current_is_a {
                self.nb.process(&self.buf_a[..n], &mut self.buf_b[..n])?;
            } else {
                self.nb.process(&self.buf_b[..n], &mut self.buf_a[..n])?;
            }
            current_is_a = !current_is_a;
        }

        // Stage 2: Squelch (manual or auto)
        if squelch_active {
            // Snapshot before processing so we can log
            // open↔closed transitions when auto-squelch is
            // active — rare (once per voice burst) and useful
            // for diagnosing gate behavior in the field
            // (e.g. issue #348). The snapshot is cheap and
            // gated on `auto_squelch` so manual-only paths pay
            // nothing.
            let pre_snapshot = self.squelch.diagnostic_snapshot();
            if current_is_a {
                self.squelch
                    .process(&self.buf_a[..n], &mut self.buf_b[..n])?;
            } else {
                self.squelch
                    .process(&self.buf_b[..n], &mut self.buf_a[..n])?;
            }
            current_is_a = !current_is_a;

            if pre_snapshot.auto_squelch && pre_snapshot.open != self.squelch.is_open() {
                let post = self.squelch.diagnostic_snapshot();
                tracing::debug!(
                    transition = if post.open { "open" } else { "closed" },
                    measured_db = post.last_measured_db,
                    noise_floor_db = post.noise_floor_db,
                    settle_count = post.settle_count,
                    "auto-squelch gate transition"
                );
            }
        }

        // Stage 3: Software IF AGC. Runs AFTER squelch so the
        // squelch threshold reads a non-normalized amplitude and
        // can still distinguish signal from noise — see the
        // processing-order docstring on `IfChain` for why this
        // ordering matters.
        //
        // Skip the stage entirely when the squelch is closed:
        // the buffer is already all-zero from `PowerSquelch`, so
        // `Agc::process_complex` would hit its `in_amp == 0.0`
        // fast path for every sample and preserve state without
        // modifying it — correct but wasteful. Skipping saves
        // the per-sample loop during silent stretches AND defends
        // against any future `Agc` refactor that loses the fast-
        // path short-circuit (which would otherwise wind the
        // envelope tracker toward `SOFTWARE_AGC_MAX_GAIN` on the
        // zero input, producing a gain burst on squelch reopen).
        //
        // `current_is_a` stays as-is when we skip so the pass-
        // through zeros land in the output buffer at the final
        // copy below.
        let squelch_is_muting = squelch_active && !self.squelch.is_open();
        if self.software_agc_enabled && !squelch_is_muting {
            if current_is_a {
                self.software_agc
                    .process_complex(&self.buf_a[..n], &mut self.buf_b[..n])?;
            } else {
                self.software_agc
                    .process_complex(&self.buf_b[..n], &mut self.buf_a[..n])?;
            }
            current_is_a = !current_is_a;
        }

        // Stage 4: FM IF noise reduction. Block-based: it may hand back
        // fewer samples than it was given (bounded latency), so the
        // running count follows its return value. Per #773.
        if self.fm_if_nr_enabled {
            n = if current_is_a {
                self.fm_if_nr
                    .process(&self.buf_a[..n], &mut self.buf_b[..n])?
            } else {
                self.fm_if_nr
                    .process(&self.buf_b[..n], &mut self.buf_a[..n])?
            };
            current_is_a = !current_is_a;
        }

        // Copy result to output
        let result = if current_is_a {
            &self.buf_a[..n]
        } else {
            &self.buf_b[..n]
        };
        output[..n].copy_from_slice(result);

        Ok(n)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp, clippy::cast_precision_loss)]
mod tests;
