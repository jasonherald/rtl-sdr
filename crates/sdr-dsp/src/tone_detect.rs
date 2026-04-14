//! CTCSS sub-audible tone detector (#269 PR 1 of 3).
//!
//! Implements three parallel Goertzel filters — one at the user-
//! selected CTCSS (Continuous Tone-Coded Squelch System) target
//! and one each at the two immediate CTCSS-table neighbors — for a
//! neighbor-dominance gate that rejects adjacent-tone crosstalk,
//! plus a sustained-detection gate that rejects single-window
//! false triggers from low-frequency speech energy overlapping the
//! tone band. Supports all 51 tones in [`CTCSS_TONES_HZ`] (42
//! standard EIA/TIA-603 / Motorola PL tones plus 9 common non-
//! standard additions).
//!
//! # Where this fits
//!
//! CTCSS tones are amplitude-modulated sub-audible (67–254 Hz) signals
//! transmitted alongside voice on analog FM. They're used to give a
//! shared frequency "private" groups — a receiver configured for CTCSS
//! tone X only opens its squelch when it sees that specific tone, even
//! if other users are transmitting voice on the same carrier. Every
//! consumer scanner and every commercial NFM radio supports this.
//!
//! This module is the detector half only — it takes demodulated AF
//! samples at 48 kHz and answers "is the target tone currently
//! sustained?". The squelch wiring, high-pass filter, and UI live in
//! follow-up PRs; this PR 1 is self-contained DSP that can be tested
//! with synthetic signals.
//!
//! # Algorithm
//!
//! [Goertzel](https://en.wikipedia.org/wiki/Goertzel_algorithm) is a
//! second-order IIR resonator tuned to a single target frequency. It
//! computes the DFT magnitude at that one frequency in O(N) time with
//! no multiplications by twiddle factors. Given samples `x[0..N]`,
//! target frequency `f`, and sample rate `fs`:
//!
//! ```text
//! omega  = 2π·f/fs
//! coeff  = 2·cos(omega)
//! s[-1]  = 0
//! s[-2]  = 0
//! for n in 0..N:
//!     s[n] = x[n] + coeff·s[n-1] - s[n-2]
//! mag² = s[N-1]² + s[N-2]² − coeff·s[N-1]·s[N-2]
//! ```
//!
//! Goertzel can target arbitrary (non-bin-aligned) frequencies — the
//! coefficient is computed from the real-valued `omega`, not an
//! integer bin index. CTCSS tones are spaced as finely as 2.5 Hz
//! (e.g. 67.0 / 69.3 Hz) so this matters.
//!
//! # Frequency resolution and neighbor-dominance gating
//!
//! The time-frequency uncertainty relation gives roughly `1/T` Hz of
//! resolution for a window of length `T` seconds — no algorithm can
//! beat that. The **finest** CTCSS spacing is 1.4 Hz
//! (150.0 / 151.4 Hz), and the Goertzel sinc-leakage response of a
//! neighbor at distance `Δf` through a rectangular window of length
//! `T` is `sinc(π·Δf·T)`. At `T = 200 ms` the leakage is
//! `sinc(π·1.4·0.2) ≈ 0.88` — an interfering neighbor produces
//! nearly the same magnitude as a correct target, and an absolute
//! threshold can't distinguish them.
//!
//! The fix is two-part:
//!
//! 1. **Longer window** — [`CTCSS_WINDOW_MS`] = 400 ms gives
//!    `sinc(π·1.4·0.4) ≈ 0.56`. Neighbor magnitude drops to ~56% of
//!    target magnitude for the worst-case close CTCSS pair.
//! 2. **Neighbor-dominance check** — the detector runs Goertzel
//!    filters at the TARGET tone **plus its immediate CTCSS-table
//!    neighbors** (one above and one below, if they exist), and
//!    requires `target_mag ≥ max(neighbor_mags) · DOMINANCE_RATIO`
//!    in addition to the absolute threshold. Target must clearly
//!    beat its neighbors, not just rise above the noise floor.
//!
//! [`CTCSS_DOMINANCE_RATIO`] is set to 1.5 (target ≥ 1.5× loudest
//! neighbor). At 400 ms that rejects the 1.4 Hz-spaced pairs
//! cleanly:
//!
//! - true target: `target_mag = 1.0`, neighbor `= 0.56`, ratio `1.79` ✓
//! - neighbor as target: `target_mag = 0.56`, neighbor `= 1.0`,
//!   ratio `0.56`, FAILS the 1.5× test ✓
//!
//! The combination (absolute threshold AND relative dominance) is
//! what real scanners do. Latency rises from 200 ms per window to
//! 400 ms per window, giving 1.2 s of confirmation time before the
//! sustained gate opens (three 400 ms hits). Matches standard
//! scanner behavior — a bit slower than consumer-grade scanners but
//! much more specific.
//!
//! # Sustained-detection gate
//!
//! The main false-trigger source is voice fundamental energy in
//! 80–250 Hz — a male voice's F0 can sit right in the middle of the
//! CTCSS band. To reject these transients we require [`CTCSS_MIN_HITS`]
//! consecutive detection windows above threshold before the gate
//! opens, and [`CTCSS_MIN_HITS`] consecutive below-threshold windows
//! before it closes (hysteresis). At 400 ms per window (see
//! [`CTCSS_WINDOW_MS`]) and [`CTCSS_MIN_HITS`] = 3, that's ~1.2 s of
//! confirmation latency — slightly slower than consumer-grade
//! scanners but much more specific for adjacent-tone rejection.
//!
//! # What this PR is NOT
//!
//! - **No DCS (digital code squelch) detection.** DCS needs a PLL-
//!   locked baseband plus a Golay decoder and is a much bigger lift;
//!   tracked separately as a follow-up.
//! - **No squelch wiring or UI.** PR 2 integrates this into
//!   `sdr-radio::af_chain` with a high-pass filter on the speaker
//!   path so users don't hear the sub-audible tone. PR 3 adds the
//!   per-bookmark UI.
//! - **No tone-encode / TX path.** Read-only SDR for now.

use sdr_types::DspError;

/// AF chain sample rate in Hz. Matches `sdr-radio::af_chain`'s
/// `DEFAULT_AUDIO_RATE` — if that ever changes, the `CTCSS_WINDOW_MS`
/// math below needs to be revisited.
pub const CTCSS_SAMPLE_RATE_HZ: f32 = 48_000.0;

/// Detection window length in milliseconds. Drives the frequency
/// resolution of each Goertzel filter — the neighbor leakage through
/// a rectangular window of length `T` is `sinc(π·Δf·T)`. 400 ms
/// gives `sinc(π·1.4·0.4) ≈ 0.56` for the worst-case 1.4 Hz-spaced
/// CTCSS pair (150.0 / 151.4 Hz), which combined with the
/// [`CTCSS_DOMINANCE_RATIO`] relative check below gives clean
/// adjacent-tone rejection.
///
/// 200 ms was tried first but the `sinc(0.88) ≈ 0.88` leakage at
/// that window length left adjacent CTCSS tones nearly indistin-
/// guishable — absolute thresholding alone was a false-positive
/// machine. See the module docstring for the derivation.
pub const CTCSS_WINDOW_MS: f32 = 400.0;

/// Window length in samples, derived from [`CTCSS_WINDOW_MS`] and
/// [`CTCSS_SAMPLE_RATE_HZ`]. Used as the Goertzel block size.
///
/// Integer literal because the const-eval float → usize cast trips
/// clippy's `cast_possible_truncation` / `cast_sign_loss` lints.
/// The value is locked in at `400 ms × 48 kHz ÷ 1000 = 19200` and
/// there's a compile-time assertion below to catch drift if either
/// of the two inputs changes.
pub const CTCSS_WINDOW_SAMPLES: usize = 19_200;

/// How much larger the target-tone Goertzel magnitude must be than
/// the loudest immediate-neighbor Goertzel magnitude for the per-
/// window decision to count as "target tone present". 1.5 means
/// the target has to be 50% larger than the max of the two table
/// neighbors; at [`CTCSS_WINDOW_MS`] = 400 ms this cleanly rejects
/// the worst-case 1.4 Hz-spaced pair while preserving true targets.
///
/// See the module docstring for the leakage math that motivates
/// this value.
pub const CTCSS_DOMINANCE_RATIO: f32 = 1.5;

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp
)]
const _: () = {
    // Compile-time sanity check: if anyone edits `CTCSS_WINDOW_MS`
    // or `CTCSS_SAMPLE_RATE_HZ` without also updating
    // `CTCSS_WINDOW_SAMPLES`, this fails to compile. The `as` cast
    // is safe here — the values are well inside usize range and
    // positive — but we still have to silence the pedantic lints
    // for a const-context expression.
    let derived = (CTCSS_WINDOW_MS * CTCSS_SAMPLE_RATE_HZ / 1000.0) as usize;
    assert!(
        derived == CTCSS_WINDOW_SAMPLES,
        "CTCSS_WINDOW_SAMPLES out of sync with CTCSS_WINDOW_MS / CTCSS_SAMPLE_RATE_HZ"
    );
};

/// Tolerance for matching a caller-supplied `target_hz` against an
/// entry in [`CTCSS_TONES_HZ`]. 0.01 Hz is well below any actual
/// CTCSS spacing (minimum gap is 1.4 Hz) so this won't confuse two
/// table entries, but comfortably larger than f32 round-trip error
/// so a user passing `100.0f32` doesn't miss the `100.0` entry.
const CTCSS_TONE_MATCH_EPSILON_HZ: f32 = 0.01;

/// Tolerance for matching a caller-supplied `sample_rate_hz`
/// against [`CTCSS_SAMPLE_RATE_HZ`]. The detector is calibrated
/// around a hardcoded [`CTCSS_WINDOW_SAMPLES`] block size that
/// only equals [`CTCSS_WINDOW_MS`] at the canonical 48 kHz AF
/// chain rate — at any other rate the same block size would give
/// the wrong effective window duration and re-introduce the
/// adjacent-tone leakage problem this detector's sinc math was
/// specifically calibrated to avoid. Future work: if we ever need
/// multi-rate support, derive [`CTCSS_WINDOW_SAMPLES`] from the
/// runtime sample rate instead of hardcoding it, then drop this
/// check.
const CTCSS_SAMPLE_RATE_MATCH_EPSILON_HZ: f32 = 0.5;

/// Number of consecutive above-threshold windows required before the
/// sustained-detection gate opens, and number of consecutive
/// below-threshold windows required before it closes. Three windows
/// at [`CTCSS_WINDOW_MS`] = 400 ms each give a 1.2 s confirmation
/// time — slightly slower than consumer-grade scanners but much
/// more specific for adjacent-tone rejection in return.
pub const CTCSS_MIN_HITS: usize = 3;

/// Default detection threshold: target-tone magnitude must exceed
/// this multiple of the window's RMS energy to count as a hit. The
/// Goertzel magnitude is in the same units as the input samples, so
/// we normalize by the RMS of the whole window to get a
/// "proportion of signal in this one frequency" measure.
///
/// Empirically 0.1 (10% of window RMS) is a reasonable starting
/// point that catches real tones while rejecting voice transients.
/// The value will likely want to become user-tunable in PR 2 once
/// real-world traffic reveals its behavior.
pub const CTCSS_DEFAULT_THRESHOLD: f32 = 0.1;

/// The 51 CTCSS tones in Hz recognized by this detector: the 42
/// standard tones from EIA/TIA-603 / Motorola PL, plus 9 common
/// non-standard additions (162.2, 167.9, 179.9, 183.5, 189.9,
/// 196.6, 199.5, 206.5, 254.1) that modern scanners also support.
/// Ordered strictly ascending so a future UI dropdown can use the
/// slice directly as its value list.
///
/// A future UI split could expose a "standard 42" vs "extended 51"
/// toggle if users want to avoid the non-standard tones, but all
/// 51 are kept inline today to match what the hardware actually
/// emits on commercial shared channels.
pub const CTCSS_TONES_HZ: &[f32] = &[
    67.0, 69.3, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8, 97.4, 100.0, 103.5, 107.2,
    110.9, 114.8, 118.8, 123.0, 127.3, 131.8, 136.5, 141.3, 146.2, 150.0, 151.4, 156.7, 159.8,
    162.2, 165.5, 167.9, 171.3, 173.8, 177.3, 179.9, 183.5, 186.2, 189.9, 192.8, 196.6, 199.5,
    203.5, 206.5, 210.7, 218.1, 225.7, 229.1, 233.6, 241.8, 250.3, 254.1,
];

/// Look up the index of `target_hz` in [`CTCSS_TONES_HZ`] using a
/// small epsilon tolerance ([`CTCSS_TONE_MATCH_EPSILON_HZ`]).
/// Returns `None` if the frequency isn't a known CTCSS tone. Used
/// by the UI / config layer to validate user input against the
/// table and by the detector constructor to find the target's
/// neighbors for the dominance check.
#[must_use]
pub fn ctcss_tone_index(target_hz: f32) -> Option<usize> {
    CTCSS_TONES_HZ
        .iter()
        .position(|&t| (t - target_hz).abs() < CTCSS_TONE_MATCH_EPSILON_HZ)
}

/// Output of [`CtcssDetector::accept_samples`] — includes both the
/// raw per-window decision and the sustained-gate state so callers
/// can choose which one to act on. The squelch wiring in PR 2 will
/// consume `sustained` only; tests and future analytics can use the
/// raw `detected` for per-window diagnostics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CtcssDecision {
    /// Goertzel magnitude at the target frequency, normalized by the
    /// window's RMS. Always in `[0, 1]` in exact arithmetic (with a
    /// tiny f32 round-up possible at the boundary). Crossing the
    /// detector's threshold is only the **absolute-floor** half of
    /// the per-window decision — [`CtcssDecision::detected`] also
    /// requires neighbor dominance. This field on its own does not
    /// tell you whether `detected` is true; it's exposed mainly for
    /// test diagnostics and future analytics (e.g. a UI level meter
    /// on the tone band).
    pub normalized_magnitude: f32,
    /// Combined per-window hit decision produced by
    /// `process_window`: the target tone is present in THIS
    /// single [`CTCSS_WINDOW_MS`]-long window (400 ms by default).
    /// True when BOTH the absolute-floor check
    /// (`normalized_magnitude ≥ threshold`) AND the neighbor-
    /// dominance check
    /// (`target_mag ≥ max(neighbor_mags) × CTCSS_DOMINANCE_RATIO`)
    /// pass — see the `above_floor` and `dominates_neighbors`
    /// locals in `process_window` for the exact logic. May flap
    /// on transients over a single window, so squelch wiring
    /// should consume [`CtcssDecision::sustained`] for the
    /// debounced value.
    pub detected: bool,
    /// The sustained-detection gate: the target tone has been
    /// present for at least [`CTCSS_MIN_HITS`] consecutive windows
    /// and the squelch should be open. This is the one the squelch
    /// wiring in PR 2 will consume.
    pub sustained: bool,
}

/// Goertzel CTCSS tone detector with neighbor-dominance gating and
/// sustained-hit debouncing.
///
/// Runs three single-frequency Goertzel filters in parallel: one at
/// the target CTCSS tone, and one each at the two immediate CTCSS
/// neighbors (from [`CTCSS_TONES_HZ`]) — that last pair provides
/// the dominance reference so a 1.4 Hz-offset interferer can't masq-
/// uerade as a true target. First and last entries in the table
/// have only one neighbor each; those stay `None`.
///
/// Feed blocks of 48 kHz demodulated audio via
/// [`Self::accept_samples`] and consume [`CtcssDecision::sustained`]
/// to drive squelch. The internal state keeps a small counter of
/// consecutive hits / misses so the gate's latch is stateful across
/// calls — don't construct a fresh detector per block or you'll
/// lose the hysteresis.
pub struct CtcssDetector {
    /// Target CTCSS frequency in Hz (one of the [`CTCSS_TONES_HZ`]
    /// entries).
    target_hz: f32,
    /// Goertzel feedback coefficient at the target frequency:
    /// `2·cos(2π·f_target/fs)`.
    target_coeff: f32,
    /// Goertzel coefficient for the CTCSS entry just below the
    /// target, or `None` if the target is the first entry in the
    /// table.
    below_coeff: Option<f32>,
    /// Goertzel coefficient for the CTCSS entry just above the
    /// target, or `None` if the target is the last entry in the
    /// table.
    above_coeff: Option<f32>,
    /// Magnitude / RMS ratio above which a window counts as a hit.
    threshold: f32,
    /// Target-vs-neighbor dominance ratio required for a hit. See
    /// [`CTCSS_DOMINANCE_RATIO`].
    dominance_ratio: f32,
    /// Number of consecutive above-threshold windows required to
    /// open / close the sustained gate.
    min_hits: usize,

    /// Current state of the sustained gate. Flipped only when the
    /// hit/miss counter crosses `min_hits`.
    sustained: bool,
    /// Consecutive above-threshold windows since the last flip.
    /// Counter resets on a miss when the gate is closed.
    hit_run: usize,
    /// Consecutive below-threshold windows since the last flip.
    /// Counter resets on a hit when the gate is open.
    miss_run: usize,

    /// Samples waiting to fill the next full [`CTCSS_WINDOW_SAMPLES`]
    /// window. Callers to [`Self::accept_samples`] may feed
    /// arbitrary-length slices (whatever their audio callback
    /// produces); the detector buffers them here and only runs the
    /// Goertzel filters when it has exactly one window's worth of
    /// audio. Empty between windows; drained on [`Self::reset`].
    ///
    /// Buffering at this layer rather than the caller layer is
    /// important because the 400 ms window duration is load-bearing
    /// for the adjacent-tone sinc math (see module docstring). A
    /// detector that silently accepted shorter blocks would reopen
    /// the exact crosstalk bug round 2 fixed.
    pending_samples: Vec<f32>,
}

/// Compute the Goertzel coefficient `2·cos(2π·f/fs)` for a given
/// target frequency. Shared helper so the constructor and any
/// future sample-rate-change path build coefficients the same way.
fn goertzel_coeff(target_hz: f32, sample_rate_hz: f32) -> f32 {
    let omega = core::f32::consts::TAU * target_hz / sample_rate_hz;
    2.0 * omega.cos()
}

/// Run one Goertzel recurrence over `samples` using the precomputed
/// `coeff` and return the un-normalized magnitude (sqrt of the
/// clamped `|DFT|²`). Factored out so `accept_samples` can call it
/// three times — once for the target and once for each neighbor —
/// without duplicating the inner loop.
fn goertzel_magnitude(samples: &[f32], coeff: f32) -> f32 {
    let mut s1: f32 = 0.0;
    let mut s2: f32 = 0.0;
    for &x in samples {
        let s = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    // Magnitude squared is algebraically `|DFT[f_target]|²` which is
    // non-negative, but f32 rounding at the recurrence boundary can
    // produce tiny negative values — clamp before sqrt to avoid NaN.
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt()
}

impl CtcssDetector {
    /// Build a detector for one of the standard CTCSS tones.
    ///
    /// `target_hz` must match an entry in [`CTCSS_TONES_HZ`] within
    /// [`CTCSS_TONE_MATCH_EPSILON_HZ`] — the lookup determines
    /// which immediate-neighbor tones get used for the dominance
    /// gate. Arbitrary non-CTCSS frequencies are rejected at
    /// construction time rather than silently falling back to
    /// absolute-threshold-only detection, because the absolute-only
    /// path is vulnerable to adjacent-tone false triggers.
    ///
    /// Also validates finiteness of both float inputs (NaN and
    /// ±∞ fail all ordering comparisons by IEEE-754 semantics, so
    /// a `target_hz.is_nan()` would otherwise silently propagate
    /// to `omega.cos()` and produce a NaN coefficient that makes
    /// every window's normalized magnitude NaN).
    ///
    /// Returns [`DspError::InvalidParameter`] on:
    ///
    /// - `target_hz` or `sample_rate_hz` not finite
    /// - `sample_rate_hz` not equal to [`CTCSS_SAMPLE_RATE_HZ`]
    ///   (within [`CTCSS_SAMPLE_RATE_MATCH_EPSILON_HZ`]) — the
    ///   detector's [`CTCSS_WINDOW_SAMPLES`] block size is
    ///   calibrated for 48 kHz; at any other rate the effective
    ///   window duration would change and the adjacent-tone sinc
    ///   math (see module docstring) would stop holding. Future
    ///   work: derive the block size from the runtime rate.
    /// - `target_hz` not present in [`CTCSS_TONES_HZ`]
    ///
    /// Uses [`CTCSS_DEFAULT_THRESHOLD`], [`CTCSS_DOMINANCE_RATIO`],
    /// and [`CTCSS_MIN_HITS`] for the threshold, dominance ratio,
    /// and sustained-gate debounce count. Use [`Self::with_threshold`]
    /// to override the absolute threshold.
    pub fn new(target_hz: f32, sample_rate_hz: f32) -> Result<Self, DspError> {
        if !target_hz.is_finite() || !sample_rate_hz.is_finite() {
            return Err(DspError::InvalidParameter(format!(
                "CTCSS detector requires finite target_hz and sample_rate_hz, \
                 got target_hz={target_hz}, sample_rate_hz={sample_rate_hz}"
            )));
        }

        // Pin the detector to the canonical AF-chain rate. Rather
        // than silently accepting arbitrary rates and producing
        // wrong-duration windows, reject anything that doesn't
        // match CTCSS_SAMPLE_RATE_HZ — a mismatch is always a
        // wiring bug at this layer, and the alternative (derive
        // `window_samples` at runtime) is a bigger refactor than
        // this PR wants to do. Tolerance is generous (0.5 Hz)
        // because the AF chain's rate is configured from an f64
        // that may round-trip through f32 with small drift.
        if (sample_rate_hz - CTCSS_SAMPLE_RATE_HZ).abs() > CTCSS_SAMPLE_RATE_MATCH_EPSILON_HZ {
            return Err(DspError::InvalidParameter(format!(
                "CTCSS detector is calibrated for {CTCSS_SAMPLE_RATE_HZ} Hz AF \
                 chain (the canonical rate — the 400 ms window sinc math for \
                 adjacent-tone rejection breaks at other rates). Got \
                 sample_rate_hz={sample_rate_hz}"
            )));
        }

        let index = ctcss_tone_index(target_hz).ok_or_else(|| {
            DspError::InvalidParameter(format!(
                "CTCSS detector target_hz={target_hz} must match an entry in \
                 CTCSS_TONES_HZ (42 standard tones + 9 extensions, 67.0 - 254.1 Hz)"
            ))
        })?;

        // Look up table neighbors. First tone has no `below`, last
        // has no `above`. Use the canonical table value rather than
        // the caller-supplied `target_hz` (which may differ by up
        // to `CTCSS_TONE_MATCH_EPSILON_HZ`) so the coefficients are
        // reproducible across calls with slightly different inputs.
        let canonical_target = CTCSS_TONES_HZ[index];
        let below_coeff = if index == 0 {
            None
        } else {
            Some(goertzel_coeff(CTCSS_TONES_HZ[index - 1], sample_rate_hz))
        };
        let above_coeff = if index + 1 >= CTCSS_TONES_HZ.len() {
            None
        } else {
            Some(goertzel_coeff(CTCSS_TONES_HZ[index + 1], sample_rate_hz))
        };

        Ok(Self {
            target_hz: canonical_target,
            target_coeff: goertzel_coeff(canonical_target, sample_rate_hz),
            below_coeff,
            above_coeff,
            threshold: CTCSS_DEFAULT_THRESHOLD,
            dominance_ratio: CTCSS_DOMINANCE_RATIO,
            min_hits: CTCSS_MIN_HITS,
            sustained: false,
            hit_run: 0,
            miss_run: 0,
            pending_samples: Vec::with_capacity(CTCSS_WINDOW_SAMPLES),
        })
    }

    /// Build a detector with a custom hit threshold. The default
    /// value is [`CTCSS_DEFAULT_THRESHOLD`]; use this when the
    /// default produces too many / too few hits on your specific
    /// audio. Threshold is the ratio of target-frequency Goertzel
    /// magnitude to window RMS — a dimensionless value.
    ///
    /// Valid range is `(0.0, 1.0]`:
    ///
    /// - `≤ 0.0`: every window trivially passes
    ///   `normalized_magnitude ≥ threshold` (including pure silence)
    ///   and the sustained gate would open after three silent blocks.
    /// - `> 1.0`: impossible to satisfy in exact arithmetic. The
    ///   Goertzel magnitude at the target frequency is bounded above
    ///   by `N · rms`, so `normalized_magnitude = mag / (N · rms)`
    ///   is bounded above by 1.0. A threshold greater than 1.0
    ///   means the detector never fires for any input.
    /// - `NaN` / `±∞`: IEEE-754 ordering comparisons would make
    ///   every window trivially pass or trivially fail depending on
    ///   the sign of infinity, which is always a wiring bug.
    ///
    /// Returns [`DspError::InvalidParameter`] on any of the above.
    pub fn with_threshold(
        target_hz: f32,
        sample_rate_hz: f32,
        threshold: f32,
    ) -> Result<Self, DspError> {
        if !threshold.is_finite() || threshold <= 0.0 || threshold > 1.0 {
            return Err(DspError::InvalidParameter(format!(
                "CTCSS detector threshold must be finite and in (0, 1], got {threshold}"
            )));
        }
        let mut detector = Self::new(target_hz, sample_rate_hz)?;
        detector.threshold = threshold;
        Ok(detector)
    }

    /// Target frequency this detector is tuned to.
    #[must_use]
    pub fn target_hz(&self) -> f32 {
        self.target_hz
    }

    /// Current state of the sustained-detection gate. Equivalent to
    /// the last [`CtcssDecision::sustained`] returned by
    /// [`Self::accept_samples`], except available between blocks if
    /// the caller needs to poll without feeding samples.
    #[must_use]
    pub fn is_sustained(&self) -> bool {
        self.sustained
    }

    /// Reset the sustained-gate counters and re-close the gate.
    /// Also drops any samples buffered in `pending_samples` so the
    /// next `accept_samples` call starts a fresh window alignment.
    /// Called at session start / demod-mode change so stale state
    /// from a previous transmission can't leak into the new one.
    pub fn reset(&mut self) {
        self.sustained = false;
        self.hit_run = 0;
        self.miss_run = 0;
        self.pending_samples.clear();
    }

    /// Feed arbitrary-length samples into the detector. Runs the
    /// Goertzel filters and updates the sustained-hit gate for
    /// every full [`CTCSS_WINDOW_SAMPLES`]-long window that fits
    /// into the pending buffer. Any remaining samples stay buffered
    /// for the next call so a stream of irregular-size audio
    /// blocks still land on aligned window boundaries.
    ///
    /// Returns `Some(decision)` with the MOST RECENT window's
    /// decision if at least one full window was processed during
    /// this call, or `None` if the input was swallowed into the
    /// pending buffer without completing a window. When multiple
    /// windows complete in one call (e.g. a caller that batched
    /// several seconds of audio), the sustained-gate state is
    /// updated for all of them in order, but only the latest
    /// per-window decision is returned — the debounced state is
    /// available via [`Self::is_sustained`] regardless.
    ///
    /// Callers that want per-window diagnostics should feed
    /// exactly [`CTCSS_WINDOW_SAMPLES`]-sized slices, which
    /// produces one `Some(decision)` per call.
    ///
    /// The 400 ms window duration at 48 kHz is load-bearing for
    /// the adjacent-tone sinc math (see module docstring), so
    /// this is the ONLY entry point for running Goertzel on
    /// samples — there is no public API that processes an
    /// arbitrary-length block directly.
    pub fn accept_samples(&mut self, samples: &[f32]) -> Option<CtcssDecision> {
        if samples.is_empty() {
            return None;
        }
        self.pending_samples.extend_from_slice(samples);

        // How many complete windows can we process right now? The
        // answer is the largest multiple of CTCSS_WINDOW_SAMPLES
        // that fits into the pending buffer.
        let ready_len = (self.pending_samples.len() / CTCSS_WINDOW_SAMPLES) * CTCSS_WINDOW_SAMPLES;
        if ready_len == 0 {
            return None;
        }

        // Zero-allocation processing path. `mem::take` swaps a
        // fresh empty Vec into `self.pending_samples` and hands
        // back the original as a local — preserving both its data
        // AND its reserved capacity. We can then iterate the ready
        // prefix with `chunks_exact` and call `&mut self` methods
        // inside the loop without borrow-checker conflicts, since
        // `pending` is now a separate local variable from `self`.
        //
        // After iteration, `drain(..ready_len)` shifts the leftover
        // samples to the front of the Vec with a single memmove
        // and preserves the Vec's reserved capacity — no
        // allocation. The Vec is then put back into
        // `self.pending_samples` so the next call extends into
        // the preserved big-capacity buffer without reallocating.
        //
        // Net cost per call: ZERO allocations + ONE memmove of
        // ≤ `CTCSS_WINDOW_SAMPLES` - 1 leftover samples,
        // regardless of how many windows complete. The previous
        // `drain(..WINDOW).collect()` loop allocated N times per
        // call, and an interim `split_off` approach allocated once
        // per call AND discarded the original Vec's big reserved
        // capacity (forcing the next call to reallocate when
        // `extend_from_slice` grew the small-cap leftover Vec).
        // This path avoids the allocator entirely, which is the
        // crate-level invariant for `sdr-dsp`.
        let mut pending = core::mem::take(&mut self.pending_samples);

        let mut last_decision = None;
        for window in pending[..ready_len].chunks_exact(CTCSS_WINDOW_SAMPLES) {
            last_decision = Some(self.process_window(window));
        }

        // Drop the ready prefix (shift leftover to the front,
        // preserving reserved capacity) and restore the Vec into
        // `self.pending_samples` for the next call.
        pending.drain(..ready_len);
        self.pending_samples = pending;

        last_decision
    }

    /// Internal: process one full [`CTCSS_WINDOW_SAMPLES`]-long
    /// window and update the sustained-hit gate. Called only from
    /// [`Self::accept_samples`], which is responsible for ensuring
    /// the slice is exactly one window long. Kept private so
    /// external callers can't accidentally feed a wrong-length
    /// block and recreate the adjacent-tone leakage bug that the
    /// 400 ms window math was specifically calibrated to avoid.
    ///
    /// Runs three Goertzel filters in parallel: one at the target
    /// frequency and one each at the two immediate CTCSS-table
    /// neighbors (if they exist). A per-window hit requires BOTH:
    ///
    /// 1. The target's normalized magnitude exceeds the absolute
    ///    threshold ([`CTCSS_DEFAULT_THRESHOLD`] by default).
    /// 2. The target's raw magnitude exceeds the loudest neighbor's
    ///    raw magnitude by at least [`CTCSS_DOMINANCE_RATIO`].
    ///
    /// This combination rejects both silence-like noise (via #1)
    /// and adjacent-tone interference (via #2). The sustained-hit
    /// gate is driven by the combined decision, not the raw
    /// magnitude.
    fn process_window(&mut self, samples: &[f32]) -> CtcssDecision {
        debug_assert_eq!(
            samples.len(),
            CTCSS_WINDOW_SAMPLES,
            "process_window must only be called with a full CTCSS window"
        );
        if samples.is_empty() {
            return CtcssDecision {
                normalized_magnitude: 0.0,
                detected: false,
                sustained: self.sustained,
            };
        }

        // Target-frequency Goertzel — this is the one we're really
        // asking about.
        let target_mag = goertzel_magnitude(samples, self.target_coeff);

        // Neighbor-frequency Goertzels for the dominance check.
        // First/last tones in the CTCSS table have only one neighbor
        // each; the other side is `0.0` (a tone that isn't there
        // can't dominate the target).
        let below_mag = self
            .below_coeff
            .map_or(0.0, |c| goertzel_magnitude(samples, c));
        let above_mag = self
            .above_coeff
            .map_or(0.0, |c| goertzel_magnitude(samples, c));
        let max_neighbor_mag = below_mag.max(above_mag);

        // Window RMS for normalizing the target magnitude into a
        // dimensionless "proportion of signal energy in the target
        // bin" — threshold is then gain-independent.
        let sum_sq: f32 = samples.iter().map(|x| x * x).sum();
        #[allow(clippy::cast_precision_loss)]
        let rms = (sum_sq / samples.len() as f32).sqrt();
        // Goertzel magnitude is scaled by N for time-domain units,
        // so divide by N to get it into the same unit as RMS.
        #[allow(clippy::cast_precision_loss)]
        let normalized_magnitude = if rms > f32::EPSILON {
            target_mag / (samples.len() as f32 * rms)
        } else {
            0.0
        };

        // Two-part decision. Absolute threshold rejects silence and
        // low-level noise; relative dominance rejects adjacent-tone
        // interference (see module docstring for the leakage math).
        //
        // Neighbor dominance is checked on the raw Goertzel
        // magnitudes (NOT the normalized one) because normalization
        // is by window RMS — the same for all three bins, so it
        // would cancel out and contribute nothing.
        let above_floor = normalized_magnitude >= self.threshold;
        let dominates_neighbors = target_mag >= max_neighbor_mag * self.dominance_ratio;
        let detected = above_floor && dominates_neighbors;

        // Sustained-gate state machine. `min_hits` controls both
        // the open and close debounce so a brief dropout doesn't
        // flap the squelch.
        if detected {
            self.hit_run = self.hit_run.saturating_add(1);
            self.miss_run = 0;
            if !self.sustained && self.hit_run >= self.min_hits {
                self.sustained = true;
            }
        } else {
            self.miss_run = self.miss_run.saturating_add(1);
            self.hit_run = 0;
            if self.sustained && self.miss_run >= self.min_hits {
                self.sustained = false;
            }
        }

        CtcssDecision {
            normalized_magnitude,
            detected,
            sustained: self.sustained,
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    /// Generate `n` samples of a unit-amplitude sine at `freq_hz`
    /// at [`CTCSS_SAMPLE_RATE_HZ`].
    fn tone(freq_hz: f32, n: usize, amplitude: f32) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / CTCSS_SAMPLE_RATE_HZ;
                amplitude * (core::f32::consts::TAU * freq_hz * t).sin()
            })
            .collect()
    }

    /// Synthetic speech-ish noise: sum of three voice-band tones
    /// (100 Hz fundamental + 450 Hz formant + 1100 Hz formant) with
    /// random per-sample amplitude. Not real speech, but has the
    /// key property of putting energy in the 80–250 Hz CTCSS band
    /// which is the main false-trigger risk.
    fn speech_like(n: usize, amplitude: f32) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / CTCSS_SAMPLE_RATE_HZ;
                let f0 = (core::f32::consts::TAU * 100.0 * t).sin() * 0.5;
                let f1 = (core::f32::consts::TAU * 450.0 * t).sin() * 0.3;
                let f2 = (core::f32::consts::TAU * 1_100.0 * t).sin() * 0.2;
                let envelope = 0.5 + 0.5 * ((i * 13 % 31) as f32 / 31.0);
                amplitude * envelope * (f0 + f1 + f2)
            })
            .collect()
    }

    fn window_samples() -> usize {
        CTCSS_WINDOW_SAMPLES
    }

    #[test]
    fn tone_table_is_ascending_and_unique() {
        // Sanity: if the table ever grows / shrinks we want a test
        // failure rather than a silent ordering change in the UI
        // dropdown OR a silent change in the documented surface
        // (docs currently say "42 standard + 9 extensions = 51
        // tones").
        assert_eq!(
            CTCSS_TONES_HZ.len(),
            51,
            "CTCSS_TONES_HZ must match the documented 42 standard + 9 extension count"
        );
        for w in CTCSS_TONES_HZ.windows(2) {
            assert!(
                w[0] < w[1],
                "CTCSS table must be strictly ascending, got {} >= {}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn ctcss_tone_index_finds_known_entries_and_rejects_unknown() {
        assert_eq!(ctcss_tone_index(67.0), Some(0));
        assert_eq!(ctcss_tone_index(100.0), Some(12));
        assert_eq!(ctcss_tone_index(254.1), Some(CTCSS_TONES_HZ.len() - 1));
        assert_eq!(ctcss_tone_index(60.0), None);
        assert_eq!(ctcss_tone_index(68.5), None);
    }

    #[test]
    fn with_threshold_rejects_out_of_range_or_non_finite_threshold() {
        // Zero / negative thresholds would make every window a
        // hit (including pure silence), which is almost always a
        // wiring bug. Guard at construction time.
        assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 0.0).is_err());
        assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, -0.1).is_err());
        assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, -10.0).is_err());
        // Thresholds above 1.0 are unreachable because
        // normalized_magnitude is bounded by 1.0 in exact
        // arithmetic. Reject them too so a misconfiguration
        // doesn't silently produce a never-fires detector.
        assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 1.0001).is_err());
        assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 2.0).is_err());
        assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 100.0).is_err());
        // NaN and infinity bypass ordering comparisons under
        // IEEE-754 — must be explicitly rejected.
        assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, f32::NAN).is_err());
        assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, f32::INFINITY).is_err());
        assert!(
            CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, f32::NEG_INFINITY).is_err()
        );
        // Positive finite values in (0, 1] are fine.
        assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 0.001).is_ok());
        assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 0.5).is_ok());
        // Exactly 1.0 is the ceiling and should be accepted.
        assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 1.0).is_ok());
    }

    #[test]
    fn constructor_rejects_non_canonical_sample_rates() {
        // The detector's window math is calibrated for 48 kHz;
        // other rates are rejected because CTCSS_WINDOW_SAMPLES is
        // a hardcoded constant and the effective window duration
        // would be wrong. Regression guard against a future caller
        // that thinks it can repurpose this for a 96 kHz or 44.1 kHz
        // AF chain without the follow-up refactor.
        assert!(CtcssDetector::new(100.0, 44_100.0).is_err());
        assert!(CtcssDetector::new(100.0, 48_000.5).is_ok()); // within the 0.5 Hz epsilon
        assert!(CtcssDetector::new(100.0, 48_001.0).is_err()); // just outside
        assert!(CtcssDetector::new(100.0, 96_000.0).is_err());
        assert!(CtcssDetector::new(100.0, 16_000.0).is_err());
    }

    #[test]
    fn constructor_rejects_non_ctcss_or_non_finite_frequencies() {
        // Non-CTCSS frequencies: rejected because the neighbor-
        // dominance gate needs table neighbors to compare against.
        assert!(CtcssDetector::new(0.0, CTCSS_SAMPLE_RATE_HZ).is_err());
        assert!(CtcssDetector::new(-1.0, CTCSS_SAMPLE_RATE_HZ).is_err());
        assert!(CtcssDetector::new(500.0, CTCSS_SAMPLE_RATE_HZ).is_err());
        assert!(CtcssDetector::new(30_000.0, CTCSS_SAMPLE_RATE_HZ).is_err());
        assert!(CtcssDetector::new(68.0, CTCSS_SAMPLE_RATE_HZ).is_err());

        // Non-finite targets and sample rates: NaN and ±∞ fail
        // ordering comparisons under IEEE-754, so they have to be
        // caught by an explicit `.is_finite()` guard.
        assert!(CtcssDetector::new(f32::NAN, CTCSS_SAMPLE_RATE_HZ).is_err());
        assert!(CtcssDetector::new(f32::INFINITY, CTCSS_SAMPLE_RATE_HZ).is_err());
        assert!(CtcssDetector::new(f32::NEG_INFINITY, CTCSS_SAMPLE_RATE_HZ).is_err());
        assert!(CtcssDetector::new(100.0, f32::NAN).is_err());
        assert!(CtcssDetector::new(100.0, f32::INFINITY).is_err());
        assert!(CtcssDetector::new(100.0, 0.0).is_err());
        assert!(CtcssDetector::new(100.0, -48_000.0).is_err());

        // Valid CTCSS tones at the edges of the table are fine.
        assert!(CtcssDetector::new(67.0, CTCSS_SAMPLE_RATE_HZ).is_ok());
        assert!(CtcssDetector::new(254.1, CTCSS_SAMPLE_RATE_HZ).is_ok());
    }

    #[test]
    fn close_neighbor_tones_are_rejected_by_dominance_gate() {
        // Regression for CR round 2 on PR #285: at 200 ms window /
        // 48 kHz sample rate the Goertzel sinc leakage between
        // adjacent CTCSS tones (e.g. 150.0 / 151.4 Hz, Δf = 1.4 Hz)
        // was large enough that an absolute-threshold decision
        // false-fired on the neighbor. Fixed by moving to a 400 ms
        // window AND requiring target magnitude to dominate the
        // immediate neighbors.
        //
        // This test pins the critical pairs CR identified plus the
        // symmetric "target itself still works" cases to make sure
        // the dominance gate didn't overshoot and break real
        // targets.
        let pairs = [(150.0_f32, 151.4_f32), (67.0, 69.3), (159.8, 162.2)];

        for &(low, high) in &pairs {
            // Detector tuned to `low`, fed a pure `high` tone. Must
            // NOT sustain the gate even over many windows.
            let mut det =
                CtcssDetector::new(low, CTCSS_SAMPLE_RATE_HZ).expect("low tone is in table");
            let interferer = tone(high, window_samples(), 1.0);
            for _ in 0..(CTCSS_MIN_HITS + 3) {
                det.accept_samples(&interferer);
            }
            assert!(
                !det.is_sustained(),
                "{low} Hz detector sustained on {high} Hz source (adjacent-tone leakage)"
            );

            // Sanity: the same detector MUST still sustain on its
            // true target. If it doesn't, the dominance gate is too
            // strict and we've traded off real detections.
            let mut det =
                CtcssDetector::new(low, CTCSS_SAMPLE_RATE_HZ).expect("low tone is in table");
            let real = tone(low, window_samples(), 1.0);
            for _ in 0..CTCSS_MIN_HITS {
                det.accept_samples(&real);
            }
            assert!(
                det.is_sustained(),
                "{low} Hz detector failed to sustain on its own target"
            );

            // And the symmetric case: detector tuned to `high` fed
            // a pure `low` source must reject.
            let mut det =
                CtcssDetector::new(high, CTCSS_SAMPLE_RATE_HZ).expect("high tone is in table");
            let interferer = tone(low, window_samples(), 1.0);
            for _ in 0..(CTCSS_MIN_HITS + 3) {
                det.accept_samples(&interferer);
            }
            assert!(
                !det.is_sustained(),
                "{high} Hz detector sustained on {low} Hz source (adjacent-tone leakage)"
            );
        }
    }

    #[test]
    fn pure_target_tone_triggers_sustained_gate_after_min_hits() {
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let block = tone(100.0, window_samples(), 1.0);

        // First block: detected but not yet sustained. Tests feed
        // exactly one full window per call, so `accept_samples`
        // always returns `Some(decision)` — unwrap via `expect`.
        let d1 = det
            .accept_samples(&block)
            .expect("one full window should produce a decision");
        assert!(
            d1.detected,
            "100 Hz tone should be detected in a 100 Hz-tuned window: mag={}",
            d1.normalized_magnitude
        );
        assert!(!d1.sustained, "single window shouldn't flip sustained gate");

        // Second and third blocks: still hitting, not yet sustained
        // until hit count reaches min_hits.
        for _ in 0..(CTCSS_MIN_HITS - 2) {
            let d = det
                .accept_samples(&block)
                .expect("one full window should produce a decision");
            assert!(d.detected && !d.sustained);
        }

        // Third hit in a row: sustained gate opens.
        let dfinal = det
            .accept_samples(&block)
            .expect("one full window should produce a decision");
        assert!(
            dfinal.sustained,
            "sustained gate should open after CTCSS_MIN_HITS"
        );
    }

    #[test]
    fn pure_silence_never_triggers() {
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let silence = vec![0.0_f32; window_samples()];
        for _ in 0..10 {
            let d = det
                .accept_samples(&silence)
                .expect("one full window should produce a decision");
            assert!(!d.detected && !d.sustained);
        }
    }

    #[test]
    fn wrong_tone_does_not_trigger_target_detector() {
        // Detector tuned to 100 Hz, input is a clean 67 Hz tone.
        // Should NOT cross the sustained gate even over many blocks.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let wrong_tone = tone(67.0, window_samples(), 1.0);

        for _ in 0..10 {
            let d = det
                .accept_samples(&wrong_tone)
                .expect("one full window should produce a decision");
            assert!(
                !d.sustained,
                "67 Hz tone should not trigger a 100 Hz detector (mag={})",
                d.normalized_magnitude
            );
        }
    }

    #[test]
    fn speech_like_noise_alone_does_not_sustain() {
        // Pure speech-band content with no CTCSS tone. Voice
        // fundamentals in 100 Hz range may produce occasional hits
        // in a naive per-window check, but the sustained gate
        // should prevent the squelch from flapping open.
        let mut det =
            CtcssDetector::new(127.3, CTCSS_SAMPLE_RATE_HZ).expect("127.3 Hz is a valid target");
        let speech = speech_like(window_samples(), 1.0);

        for _ in 0..10 {
            det.accept_samples(&speech);
        }
        assert!(
            !det.is_sustained(),
            "speech-like signal without 127.3 Hz content should not sustain"
        );
    }

    #[test]
    fn tone_under_speech_still_triggers() {
        // Mixed signal: target tone + speech-band noise. This is
        // the real-world case — a radio transmitting voice with a
        // 100 Hz CTCSS tone mixed in. Detector should still
        // sustain.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let n = window_samples();
        let pure_tone = tone(100.0, n, 0.6);
        let noise = speech_like(n, 0.4);
        let mixed: Vec<f32> = pure_tone
            .iter()
            .zip(noise.iter())
            .map(|(&t, &s)| t + s)
            .collect();

        for _ in 0..CTCSS_MIN_HITS {
            det.accept_samples(&mixed);
        }
        assert!(
            det.is_sustained(),
            "CTCSS tone mixed under speech-band noise should still sustain the gate"
        );
    }

    #[test]
    fn gate_closes_after_tone_drops() {
        // Sustain the gate, then feed silence and verify it closes
        // after CTCSS_MIN_HITS miss windows.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let n = window_samples();
        let block = tone(100.0, n, 1.0);

        for _ in 0..CTCSS_MIN_HITS {
            det.accept_samples(&block);
        }
        assert!(det.is_sustained());

        // Drop the tone.
        let silence = vec![0.0_f32; n];
        for i in 0..CTCSS_MIN_HITS {
            det.accept_samples(&silence);
            // Should stay sustained until the miss-run reaches
            // min_hits; can drop on the final iteration.
            if i < CTCSS_MIN_HITS - 1 {
                assert!(
                    det.is_sustained(),
                    "gate should remain open until miss run reaches min_hits"
                );
            }
        }
        assert!(
            !det.is_sustained(),
            "gate must close after CTCSS_MIN_HITS misses"
        );
    }

    #[test]
    fn brief_dropout_does_not_flap_open_gate() {
        // Sustain the gate, then feed ONE silence window (below
        // min_hits dropouts), then resume tone. Gate should stay
        // open throughout — this is the hysteresis behavior.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let n = window_samples();
        let block = tone(100.0, n, 1.0);

        for _ in 0..CTCSS_MIN_HITS {
            det.accept_samples(&block);
        }
        assert!(det.is_sustained());

        // One miss window, then tone resumes.
        det.accept_samples(&vec![0.0_f32; n]);
        assert!(
            det.is_sustained(),
            "single miss below min_hits should not close the sustained gate"
        );
        det.accept_samples(&block);
        assert!(det.is_sustained());
    }

    #[test]
    fn reset_clears_sustained_state() {
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let n = window_samples();
        let block = tone(100.0, n, 1.0);

        for _ in 0..CTCSS_MIN_HITS {
            det.accept_samples(&block);
        }
        assert!(det.is_sustained());

        det.reset();
        assert!(!det.is_sustained());
    }

    #[test]
    fn empty_block_returns_none_and_does_not_flip_state() {
        // An empty input should be a true no-op: no pending-buffer
        // drift, no state change, and None returned because no
        // window was completed.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        assert!(det.accept_samples(&[]).is_none());
        assert!(!det.is_sustained());
        assert_eq!(det.pending_samples.len(), 0);
    }

    #[test]
    fn sustained_state_visible_via_is_sustained_between_blocks() {
        // Callers may want to poll `is_sustained` without feeding a
        // block — verify it matches the last returned decision.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let block = tone(100.0, window_samples(), 1.0);
        for _ in 0..CTCSS_MIN_HITS {
            let d = det
                .accept_samples(&block)
                .expect("one full window should produce a decision");
            assert_eq!(det.is_sustained(), d.sustained);
        }
        assert!(det.is_sustained());
    }

    #[test]
    fn accept_samples_buffers_partial_windows() {
        // Regression for CR round 6: callers can feed arbitrary-
        // length chunks (e.g. a 4,096-sample audio-callback block)
        // and the detector must NOT run Goertzel on a short buffer,
        // which would change the effective window length and break
        // the adjacent-tone rejection. Instead it should stash the
        // partial window and return None until enough samples have
        // arrived to fill a full CTCSS_WINDOW_SAMPLES block.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        // Pin the hot-path invariant that the detector keeps its
        // reusable sample buffer across partial/full-window calls.
        // A future refactor that replaced `pending_samples` with a
        // fresh Vec (e.g. via `split_off`) would silently reintroduce
        // allocator churn in the real-time DSP path.
        let initial_capacity = det.pending_samples.capacity();
        assert!(
            initial_capacity >= CTCSS_WINDOW_SAMPLES,
            "constructor should pre-reserve at least one full window"
        );
        let block = tone(100.0, window_samples(), 1.0);

        // Feed the first half of a window. No decision yet.
        let half = CTCSS_WINDOW_SAMPLES / 2;
        assert!(det.accept_samples(&block[..half]).is_none());
        assert_eq!(det.pending_samples.len(), half);
        assert!(!det.is_sustained());
        assert_eq!(
            det.pending_samples.capacity(),
            initial_capacity,
            "partial-window feed must not reallocate the pending buffer"
        );

        // Feed the second half. Should complete the window and
        // return Some(decision).
        let d = det
            .accept_samples(&block[half..])
            .expect("remaining half of the window should complete a decision");
        assert!(
            d.detected,
            "combined 100 Hz tone window should detect: mag={}",
            d.normalized_magnitude
        );
        assert_eq!(
            det.pending_samples.len(),
            0,
            "pending buffer should be empty after a complete window"
        );
        assert_eq!(
            det.pending_samples.capacity(),
            initial_capacity,
            "window-completion must preserve the pending buffer's reserved capacity"
        );
    }

    #[test]
    fn accept_samples_processes_multiple_windows_in_one_call() {
        // A caller that batches several seconds of audio into one
        // call should still get proper per-window debounce behavior.
        // Feed MIN_HITS full windows in a single call — the
        // sustained gate should open by the end even though the
        // caller only invoked `accept_samples` once.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let initial_capacity = det.pending_samples.capacity();
        let single_window = tone(100.0, window_samples(), 1.0);
        let mut batched: Vec<f32> = Vec::with_capacity(CTCSS_WINDOW_SAMPLES * CTCSS_MIN_HITS);
        for _ in 0..CTCSS_MIN_HITS {
            batched.extend_from_slice(&single_window);
        }

        let d = det
            .accept_samples(&batched)
            .expect("batched multi-window input should produce a latest-window decision");
        assert!(
            d.sustained,
            "sustained gate should open after processing CTCSS_MIN_HITS windows in one call"
        );
        assert!(det.is_sustained());
        // The batched input is an exact multiple of CTCSS_WINDOW_SAMPLES,
        // so `pending_samples` ends empty; its reserved capacity must
        // still be at least the initial reservation so the next call
        // starts from a warm allocation. `>=` (not `==`) because a
        // caller feeding an oversized block may have legitimately
        // grown the buffer — we only require that growth is not lost.
        assert!(
            det.pending_samples.capacity() >= initial_capacity,
            "multi-window batched call must not shrink the pending buffer below its initial reservation"
        );
    }

    #[test]
    fn reset_clears_pending_sample_buffer() {
        // Regression guard: reset() must drop buffered partial-
        // window samples, otherwise the next session would start
        // with sample alignment carried over from the previous one.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let initial_capacity = det.pending_samples.capacity();
        let partial = vec![0.5_f32; CTCSS_WINDOW_SAMPLES / 3];
        det.accept_samples(&partial);
        assert_eq!(det.pending_samples.len(), partial.len());

        det.reset();
        assert_eq!(det.pending_samples.len(), 0);
        // reset() must clear contents but must not drop the reserved
        // capacity — otherwise the first post-reset `accept_samples`
        // call would have to re-allocate on the real-time path.
        assert_eq!(
            det.pending_samples.capacity(),
            initial_capacity,
            "reset() must preserve the pending buffer's reserved capacity"
        );
    }
}
