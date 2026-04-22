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
        self.fm_if_nr_enabled = enabled;
    }

    /// Returns whether FM IF noise reduction is enabled.
    pub fn fm_if_nr_enabled(&self) -> bool {
        self.fm_if_nr_enabled
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

        let n = input.len();
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

        // Stage 4: FM IF noise reduction
        if self.fm_if_nr_enabled {
            if current_is_a {
                self.fm_if_nr
                    .process(&self.buf_a[..n], &mut self.buf_b[..n])?;
            } else {
                self.fm_if_nr
                    .process(&self.buf_b[..n], &mut self.buf_a[..n])?;
            }
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
mod tests {
    use super::*;

    #[test]
    fn test_if_chain_passthrough_when_disabled() {
        let mut chain = IfChain::new().unwrap();
        let input = vec![Complex::new(1.0, 2.0); 100];
        let mut output = vec![Complex::default(); 100];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 100);
        assert_eq!(output[0].re, 1.0);
        assert_eq!(output[0].im, 2.0);
    }

    #[test]
    fn test_if_chain_squelch_enabled() {
        let mut chain = IfChain::new().unwrap();
        chain.set_squelch_enabled(true);
        chain.set_squelch_level(10.0); // very high threshold

        let input = vec![Complex::new(0.001, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        chain.process(&input, &mut output).unwrap();

        // Squelch should close on weak signal
        assert!(!chain.squelch_open());
        // Output should be zeroed
        for s in &output {
            assert!(s.re.abs() < 1e-10);
        }
    }

    #[test]
    fn test_if_chain_squelch_opens_on_strong_signal() {
        let mut chain = IfChain::new().unwrap();
        chain.set_squelch_enabled(true);
        chain.set_squelch_level(-50.0);

        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        chain.process(&input, &mut output).unwrap();

        assert!(chain.squelch_open());
    }

    #[test]
    fn test_if_chain_nb_enabled() {
        let mut chain = IfChain::new().unwrap();
        chain.set_nb_enabled(true);
        assert!(chain.nb_enabled());

        let input = vec![Complex::new(1.0, 0.0); 500];
        let mut output = vec![Complex::default(); 500];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 500);
        // Output should be non-zero (normal signal passes)
        assert!(output[499].re.abs() > 0.1);
    }

    #[test]
    fn test_if_chain_fm_if_nr_enabled() {
        let mut chain = IfChain::new().unwrap();
        chain.set_fm_if_nr_enabled(true);
        assert!(chain.fm_if_nr_enabled());

        // Use a signal large enough for the FFT block size (256 default).
        let input = vec![Complex::new(1.0, 0.0); 512];
        let mut output = vec![Complex::default(); 512];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 512);
        // DC signal should mostly survive (peak bin = 0).
        let energy: f32 = output.iter().map(|s| s.re * s.re + s.im * s.im).sum();
        assert!(energy > 0.0, "FM IF NR should produce output");
    }

    #[test]
    fn test_if_chain_all_enabled() {
        let mut chain = IfChain::new().unwrap();
        chain.set_nb_enabled(true);
        chain.set_squelch_enabled(true);
        chain.set_squelch_level(-50.0);
        chain.set_fm_if_nr_enabled(true);

        let input = vec![Complex::new(1.0, 0.0); 512];
        let mut output = vec![Complex::default(); 512];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 512);
    }

    #[test]
    fn test_if_chain_set_nb_level() {
        let mut chain = IfChain::new().unwrap();
        assert!(chain.set_nb_level(10.0).is_ok());
        assert!(chain.set_nb_level(0.5).is_err()); // below minimum of 1.0
    }

    #[test]
    fn test_if_chain_squelch_reports_open_when_disabled() {
        let chain = IfChain::new().unwrap();
        // When squelch is disabled, squelch_open should return true
        assert!(chain.squelch_open());
    }

    #[test]
    fn test_if_chain_buffer_too_small() {
        let mut chain = IfChain::new().unwrap();
        let input = [Complex::default(); 10];
        let mut output = [Complex::default(); 5];
        assert!(chain.process(&input, &mut output).is_err());
    }

    // --- Software IF AGC tests (#354) ---
    //
    // Named test fixtures. Centralized here so a future AGC
    // retune touches exactly one block — the literals are
    // load-bearing against the shipped `SOFTWARE_AGC_ATTACK`
    // and `SOFTWARE_AGC_DECAY` coefficients (1/300 and 1/3000).

    /// Number of samples used by the weak / strong convergence
    /// tests. ~10 decay time constants at the current coefficients,
    /// enough for the envelope tracker to converge close to the
    /// target set point without the test taking forever.
    const AGC_CONVERGENCE_BLOCK_LEN: usize = 30_000;
    /// Short block used by the disable-revert and squelch-interop
    /// tests where full convergence isn't needed.
    const AGC_SHORT_BLOCK_LEN: usize = 1_000;
    /// Input amplitude 40 dB below the `SOFTWARE_AGC_SET_POINT = 1.0`
    /// target. Used by the weak-signal amplification test.
    const AGC_WEAK_INPUT_AMP: f32 = 0.01;
    /// Input amplitude 20 dB above the `SOFTWARE_AGC_SET_POINT = 1.0`
    /// target. Used by the strong-signal attenuation test.
    const AGC_STRONG_INPUT_AMP: f32 = 10.0;
    /// Pass-through sample amplitude used by the disable-revert test.
    /// Arbitrary non-zero value; the assertion is equality to this.
    const AGC_PASSTHROUGH_AMP: f32 = 5.0;
    /// Squelch threshold used by the interop test. Set above the
    /// weak-input `-40 dBFS` level so the gate closes on the test
    /// input, exercising the AGC-after-squelch ordering.
    const AGC_SQUELCH_TEST_THRESHOLD_DB: f32 = -20.0;
    /// Float tolerance for the passthrough assertion. Tight —
    /// scaling by 1.0 should preserve the input exactly in f32.
    const AGC_PASSTHROUGH_EPSILON: f32 = 1e-5;
    /// Float tolerance for the "gate is zeroed" assertion. Tighter
    /// than the passthrough tolerance because the expected value
    /// is literal 0.0, not a scaled input.
    const AGC_ZERO_EPSILON: f32 = 1e-10;
    /// Fraction of the block used as the tail window for
    /// convergence assertions. The last quarter captures steady
    /// state after the envelope has converged.
    const AGC_TAIL_FRACTION: usize = 4;
    /// Minimum gain factor for the weak-signal convergence
    /// assertion. Well below the theoretical `1.0 / 0.01 = 100×`
    /// so the test stays robust against coefficient tweaks.
    const AGC_WEAK_MIN_GAIN: f32 = 5.0;
    /// Maximum residual factor for the strong-signal attenuation
    /// assertion. Well above the theoretical `1.0 / 10.0 = 0.1×`
    /// floor so the test stays robust against coefficient tweaks.
    const AGC_STRONG_MAX_RESIDUAL: f32 = 0.5;

    /// Default state: software AGC is off. A fresh `IfChain` with
    /// no other stages active should passthrough IQ unchanged —
    /// the AGC flag defaults to `false` and the `any_enabled`
    /// short-circuit covers the no-op fast path.
    #[test]
    fn software_agc_off_by_default() {
        let chain = IfChain::new().unwrap();
        assert!(!chain.software_agc_enabled());
    }

    /// With software AGC enabled, a weak constant-envelope input
    /// should see its effective gain rise over time as the
    /// envelope tracker converges toward the `1.0` set point.
    /// Pins the core "AGC actually moves gain toward set point"
    /// contract — a bypassed or broken AGC would leave output =
    /// input, a too-aggressive one would overshoot.
    #[test]
    fn software_agc_amplifies_weak_signal() {
        let mut chain = IfChain::new().unwrap();
        chain.set_software_agc_enabled(true);

        let n = AGC_CONVERGENCE_BLOCK_LEN;
        let input = vec![Complex::new(AGC_WEAK_INPUT_AMP, 0.0); n];
        let mut output = vec![Complex::default(); n];
        chain.process(&input, &mut output).unwrap();

        let tail = &output[n - n / AGC_TAIL_FRACTION..];
        let mean_out: f32 = tail
            .iter()
            .map(|s| (s.re * s.re + s.im * s.im).sqrt())
            .sum::<f32>()
            / tail.len() as f32;
        assert!(
            mean_out > AGC_WEAK_INPUT_AMP * AGC_WEAK_MIN_GAIN,
            "software AGC should amplify weak signal, input = {AGC_WEAK_INPUT_AMP}, mean output = {mean_out}"
        );
    }

    /// With software AGC enabled, a high-amplitude input should
    /// be attenuated toward the set point. Complements the
    /// amplification test.
    #[test]
    fn software_agc_attenuates_strong_signal() {
        let mut chain = IfChain::new().unwrap();
        chain.set_software_agc_enabled(true);

        let n = AGC_CONVERGENCE_BLOCK_LEN;
        let input = vec![Complex::new(AGC_STRONG_INPUT_AMP, 0.0); n];
        let mut output = vec![Complex::default(); n];
        chain.process(&input, &mut output).unwrap();

        let tail = &output[n - n / AGC_TAIL_FRACTION..];
        let mean_out: f32 = tail
            .iter()
            .map(|s| (s.re * s.re + s.im * s.im).sqrt())
            .sum::<f32>()
            / tail.len() as f32;
        assert!(
            mean_out < AGC_STRONG_INPUT_AMP * AGC_STRONG_MAX_RESIDUAL,
            "software AGC should attenuate strong signal, input = {AGC_STRONG_INPUT_AMP}, mean output = {mean_out}"
        );
    }

    /// Toggling the AGC off reverts to IQ passthrough. Pins
    /// that `set_software_agc_enabled(false)` actually takes
    /// effect on the NEXT `process` call — a state-leak bug
    /// would leave the AGC stage silently active.
    #[test]
    fn software_agc_disable_reverts_to_passthrough() {
        let mut chain = IfChain::new().unwrap();
        chain.set_software_agc_enabled(true);

        let n = AGC_SHORT_BLOCK_LEN;
        let input = vec![Complex::new(AGC_PASSTHROUGH_AMP, 0.0); n];
        let mut output = vec![Complex::default(); n];
        chain.process(&input, &mut output).unwrap();

        // Disable, run same input again — output should match
        // input verbatim now (no other stages enabled).
        chain.set_software_agc_enabled(false);
        chain.process(&input, &mut output).unwrap();
        for (i, s) in output.iter().enumerate() {
            assert!(
                (s.re - AGC_PASSTHROUGH_AMP).abs() < AGC_PASSTHROUGH_EPSILON
                    && s.im.abs() < AGC_PASSTHROUGH_EPSILON,
                "sample {i} should be pure passthrough after disable, got ({}, {})",
                s.re,
                s.im
            );
        }
    }

    /// Software AGC + squelch must interoperate: the squelch
    /// reads pre-AGC amplitude (so it can distinguish signal
    /// from noise) and AGC only runs when the gate is open.
    /// Pins the processing order documented on `IfChain`.
    #[test]
    fn software_agc_after_squelch_preserves_gating() {
        let mut chain = IfChain::new().unwrap();
        chain.set_software_agc_enabled(true);
        chain.set_squelch_enabled(true);
        chain.set_squelch_level(AGC_SQUELCH_TEST_THRESHOLD_DB);

        // Quiet input: 0.01 amplitude = -40 dBFS, below the
        // -20 dB threshold. Gate must close.
        let n = AGC_SHORT_BLOCK_LEN;
        let input = vec![Complex::new(AGC_WEAK_INPUT_AMP, 0.0); n];
        let mut output = vec![Complex::default(); n];
        chain.process(&input, &mut output).unwrap();

        assert!(
            !chain.squelch_open(),
            "squelch should still close on quiet pre-AGC signal"
        );
        // When gate is closed, output is IQ-zero regardless of
        // AGC state — AGC skips the block entirely when squelch
        // is muting, and PowerSquelch's zero output propagates
        // through.
        for s in &output {
            assert!(s.re.abs() < AGC_ZERO_EPSILON && s.im.abs() < AGC_ZERO_EPSILON);
        }
    }

    /// AGC state must survive a squelch close/reopen cycle
    /// without winding toward max gain or producing a burst on
    /// reopen. Runs one block of loud signal (AGC converges
    /// toward attenuation), one block of quiet below-threshold
    /// noise (gate closes, AGC skipped), then another loud
    /// block. The first post-reopen sample's magnitude must
    /// stay bounded — a wind-up bug would push it into the
    /// `SOFTWARE_AGC_MAX_OUTPUT = 10.0` look-ahead clipping
    /// cap or beyond.
    #[test]
    fn software_agc_survives_squelch_cycle_without_burst() {
        let mut chain = IfChain::new().unwrap();
        chain.set_software_agc_enabled(true);
        chain.set_squelch_enabled(true);
        chain.set_squelch_level(AGC_SQUELCH_TEST_THRESHOLD_DB);

        let n = AGC_SHORT_BLOCK_LEN;
        let mut output = vec![Complex::default(); n];

        // Block 1: loud signal — gate open, AGC attacks.
        let loud = vec![Complex::new(AGC_STRONG_INPUT_AMP, 0.0); n];
        chain.process(&loud, &mut output).unwrap();
        assert!(chain.squelch_open(), "loud signal should open gate");

        // Block 2: quiet noise — gate closes, AGC should be
        // skipped entirely so its state is frozen at block 1's
        // convergence rather than being fed zeros.
        let quiet = vec![Complex::new(AGC_WEAK_INPUT_AMP, 0.0); n];
        chain.process(&quiet, &mut output).unwrap();
        assert!(!chain.squelch_open(), "quiet signal should close gate");
        for s in &output {
            assert!(
                s.re.abs() < AGC_ZERO_EPSILON && s.im.abs() < AGC_ZERO_EPSILON,
                "gate-closed output should be zero"
            );
        }

        // Block 3: loud signal returns — gate reopens, AGC
        // resumes from block 1's state. First sample amplitude
        // must be bounded by the look-ahead clipping cap; a
        // wind-up bug would push it well above that.
        chain.process(&loud, &mut output).unwrap();
        let first_mag = (output[0].re * output[0].re + output[0].im * output[0].im).sqrt();
        assert!(
            first_mag < AGC_STRONG_INPUT_AMP,
            "first post-reopen sample should not burst above the input level, got mag = {first_mag}"
        );
    }
}
