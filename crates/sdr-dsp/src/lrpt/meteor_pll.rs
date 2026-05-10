//! Meteor-M Costas / carrier-recovery PLL — faithful port of
//! `dbdexter/meteor_demod/dsp/pll.c`.
//!
//! The dbdexter PLL is the gold-standard carrier-recovery loop for
//! Meteor-M LRPT (and is what `SatDump` uses under the hood). It
//! differs from a textbook Costas loop in three key ways:
//!
//! 1. **Separate I-only and Q-only mix paths** — required for OQPSK,
//!    where the in-phase and quadrature symbols land at different
//!    time instants (Tsym/2 apart). Each `mix_i` / `mix_q` call
//!    advances the NCO phase, but only computes the half of the
//!    rotated sample the timing loop actually wants.
//! 2. **Lock detector** — an exponential moving average of the
//!    absolute phase error switches the loop between locked and
//!    unlocked states (hysteresis at 85 / 105). The QPSK Costas in
//!    `costas.rs` doesn't have one.
//! 3. **Free-run frequency sweep** — when unlocked, the loop slews
//!    the frequency estimate up and down at ±1e-6 / call between
//!    `±freq_max`, scanning the carrier-offset range for the actual
//!    signal. Without this, a far-offset signal can leave the loop
//!    stuck at zero hertz forever.
//!
//! For OQPSK, `freq_max` is halved internally (per dbdexter), since
//! `mix_i` and `mix_q` together advance phase twice per symbol —
//! the per-call frequency budget halves to keep the per-symbol
//! tracking range the same.
//!
//! Tuning differs from `Costas` (which uses SDR++'s wide loops):
//! dbdexter ships a much tighter PLL bandwidth (1 Hz at the symbol
//! rate vs SDR++'s ~720 Hz) so the loop averages out more noise
//! once locked. The free-run sweep solves the slow-lock cost of
//! the narrow loop.

use core::f32::consts::PI;

use sdr_types::{Complex, DspError};

/// Default maximum carrier deviation passed to the PLL when the
/// caller doesn't supply one. Same value as dbdexter's `FREQ_MAX`
/// (`pll.c:6`). In radians per `mix*` call before the OQPSK halving.
pub const FREQ_MAX_DEFAULT: f32 = 0.3;

/// Pole frequency of the lock-detector's error-magnitude EMA.
/// Per dbdexter (`pll.c:7`).
const ERR_POLE: f32 = 0.001;

/// Tanh LUT length — covers integer arguments from -16 to +15.
const LUT_TANH_LEN: usize = 32;

/// Index offset so `lut[(val as i32 + LUT_TANH_OFFSET) as usize]`
/// resolves the integer-bin tanh value for `val`.
const LUT_TANH_OFFSET: i32 = 16;

/// Lock-detector hysteresis — when the EMA of `|error|` falls below
/// this threshold the loop is declared locked. Per dbdexter
/// (`pll.c:118`).
const LOCK_THRESHOLD_LOW: f32 = 85.0;

/// Lock-detector hysteresis — when the EMA exceeds this threshold
/// the loop is declared unlocked again. Per dbdexter (`pll.c:121`).
const LOCK_THRESHOLD_HIGH: f32 = 105.0;

/// Per-call frequency-sweep step applied while unlocked. Per
/// dbdexter (`pll.c:126`). At 144 ksps and dbdexter's loop tuning
/// the sweep covers ±0.15 rad in ~150k samples (≈ 1 second of
/// real-time signal), which lines up with the typical lock latency
/// the user perceives at the start of a Meteor pass.
const FREE_RUN_STEP: f32 = 1e-6;

/// Minimum signal-magnitude EMA required before the lock detector
/// is allowed to declare lock. Belt-and-suspenders complement to
/// the AGC stage upstream: if the input is literally zero IQ the
/// AGC's output is also zero (any gain × 0 = 0), so the
/// `compute_error` metric returns 0 each call, and the |error|
/// EMA decays from 1000 toward 0 — silently crossing
/// [`LOCK_THRESHOLD_LOW`] purely from time elapsed. Gating lock on
/// signal-magnitude EMA above this floor (≈ a quarter of the AGC
/// target) closes that loophole. Per CR round 2 on PR #663.
const LOCK_SIG_FLOOR: f32 = 47.5;

/// Critically-damped 2nd-order loop alpha/beta from a normalized
/// loop bandwidth. Same formula as
/// [`crate::loops::PhaseControlLoop::critically_damped`], duplicated
/// here as a free function so the PLL doesn't pull in the whole
/// PCL just for two coefficients (and so the alpha/beta lifecycle
/// stays in this file alongside the rest of the dbdexter port).
fn critically_damped(bandwidth: f32) -> (f32, f32) {
    let damping = core::f32::consts::FRAC_1_SQRT_2;
    let denom = 1.0 + 2.0 * damping * bandwidth + bandwidth * bandwidth;
    let alpha = 4.0 * damping * bandwidth / denom;
    let beta = 4.0 * bandwidth * bandwidth / denom;
    (alpha, beta)
}

/// Meteor-M Costas / carrier-recovery PLL. Holds the NCO phase /
/// frequency, the lock state, and a tanh LUT used by the Costas
/// error metric.
pub struct MeteorPll {
    freq: f32,
    phase: f32,
    alpha: f32,
    beta: f32,
    /// Exponential moving average of `|phase error|`. Drives the
    /// lock detector. Initialized to a deliberately-large value
    /// (matches dbdexter's `_err = 1000`) so the loop starts
    /// unlocked and the free-run sweep takes over until the EMA
    /// settles.
    err_ema: f32,
    /// Exponential moving average of input signal magnitude.
    /// Compared against [`LOCK_SIG_FLOOR`] to gate lock — see
    /// that constant's docs for the failure mode this prevents.
    sig_ema: f32,
    locked: bool,
    locked_once: bool,
    fmax: f32,
    /// Direction of the current free-run frequency sweep (+1 or
    /// −1). Toggles when the swept frequency hits ±`fmax`.
    sweep_dir: i8,
    /// Pre-computed integer-bin tanh table — `lut[i]` = tanh(i − 16)
    /// for `i ∈ [0, 32)`. Per dbdexter (`pll.c:40`), the cheap
    /// quantization replaces a per-sample `tanhf` call.
    lut_tanh: [f32; LUT_TANH_LEN],
}

impl MeteorPll {
    /// Build a PLL.
    ///
    /// - `bandwidth` — normalized loop bandwidth (radians per
    ///   `mix*` call). For Meteor at 72 ksym/s and dbdexter's
    ///   default `pll_bw = 1` Hz, this is `2π / 72_000 ≈ 8.7e-5`
    ///   for OQPSK and half that for QPSK (one mix per symbol vs
    ///   two).
    /// - `oqpsk` — `true` halves the internal `fmax` so the
    ///   per-symbol tracking range is the same in either mode.
    /// - `freq_max` — maximum carrier-frequency deviation in
    ///   radians per `mix*` call. `None` selects
    ///   [`FREQ_MAX_DEFAULT`] (= 0.3, matching dbdexter).
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `bandwidth` is not
    /// finite or not positive, or if `freq_max` is `Some(value)`
    /// with a non-finite `value` (NaN / ±∞ would propagate
    /// silently into `fmax`, then through the unlocked-state
    /// frequency sweep and `clamp` calls — every comparison
    /// against NaN evaluates `false`, so the loop would corrupt
    /// silently rather than fail loudly).
    pub fn new(bandwidth: f32, oqpsk: bool, freq_max: Option<f32>) -> Result<Self, DspError> {
        if !bandwidth.is_finite() || bandwidth <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "bandwidth must be finite and positive, got {bandwidth}"
            )));
        }
        if let Some(fm) = freq_max
            && !fm.is_finite()
        {
            return Err(DspError::InvalidParameter(format!(
                "freq_max must be finite when provided, got {fm}"
            )));
        }
        // dbdexter (`pll.c:30`): negative `freq_max` selects the
        // default; values > 1 are clamped to 1.
        let raw = freq_max.unwrap_or(FREQ_MAX_DEFAULT);
        let bounded = if raw < 0.0 {
            FREQ_MAX_DEFAULT
        } else {
            raw.min(1.0)
        };
        let fmax = if oqpsk { bounded / 2.0 } else { bounded };

        let (alpha, beta) = critically_damped(bandwidth);

        let mut lut_tanh = [0.0_f32; LUT_TANH_LEN];
        for (i, slot) in lut_tanh.iter_mut().enumerate() {
            #[allow(
                clippy::cast_possible_wrap,
                clippy::cast_possible_truncation,
                clippy::cast_precision_loss,
                reason = "i < LUT_TANH_LEN (= 32) fits in i32 with no truncation, and \
                          (i - 16) is exactly representable in f32"
            )]
            let x = (i as i32 - LUT_TANH_OFFSET) as f32;
            *slot = x.tanh();
        }

        Ok(Self {
            freq: 0.0,
            phase: 0.0,
            alpha,
            beta,
            err_ema: 1000.0,
            sig_ema: 0.0,
            locked: false,
            locked_once: false,
            fmax,
            sweep_dir: 1,
            lut_tanh,
        })
    }

    /// Mix one complex sample with the local oscillator (full
    /// rotation). Use this for QPSK; OQPSK uses the split
    /// [`Self::mix_i`] / [`Self::mix_q`] pair instead.
    ///
    /// Advances the NCO phase as a side effect, even when the
    /// caller discards the rotated sample.
    pub fn mix(&mut self, sample: Complex) -> Complex {
        let (sine, cosine) = (-self.phase).sin_cos();
        let re = sample.re;
        let im = sample.im;
        let mixed = Complex::new(re * cosine - im * sine, re * sine + im * cosine);
        self.advance_phase();
        mixed
    }

    /// Mix one complex sample and return only the real (in-phase)
    /// component. Used for the OQPSK "intersample" tick — the
    /// Q half is sampled half a symbol later via [`Self::mix_q`].
    /// Advances the NCO phase as a side effect.
    pub fn mix_i(&mut self, sample: Complex) -> f32 {
        let (sine, cosine) = (-self.phase).sin_cos();
        let result = sample.re * cosine - sample.im * sine;
        self.advance_phase();
        result
    }

    /// Mix one complex sample and return only the imaginary
    /// (quadrature) component. Used for the OQPSK symbol tick —
    /// the I half was sampled half a symbol earlier via
    /// [`Self::mix_i`]. Advances the NCO phase as a side effect.
    pub fn mix_q(&mut self, sample: Complex) -> f32 {
        let (sine, cosine) = (-self.phase).sin_cos();
        let result = sample.re * sine + sample.im * cosine;
        self.advance_phase();
        result
    }

    /// Update the carrier estimate from a decimated I/Q symbol pair.
    /// In OQPSK the two arguments come from separate [`Self::mix_i`]
    /// and [`Self::mix_q`] calls (sampled Tsym/2 apart); in QPSK
    /// they're the real and imaginary parts of a single
    /// [`Self::mix`] output.
    pub fn update_estimate(&mut self, i: f32, q: f32) {
        let error = self.compute_error(i, q);
        // Track signal magnitude so the lock detector can refuse
        // to declare lock on near-zero input. Same EMA pole as
        // err_ema so the two settle on comparable timescales.
        let mag = (i * i + q * q).sqrt();
        self.sig_ema = self.sig_ema * (1.0 - ERR_POLE) + mag * ERR_POLE;
        self.apply_loop_update(error);
    }

    /// Current NCO frequency estimate, in radians per `mix*` call.
    pub fn frequency(&self) -> f32 {
        self.freq
    }

    /// `true` while the lock detector reports the loop is locked.
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// `true` once the loop has locked at least once since
    /// construction. Sticky.
    pub fn locked_once(&self) -> bool {
        self.locked_once
    }

    fn compute_error(&self, re: f32, im: f32) -> f32 {
        // dbdexter's Costas error metric (`pll.c:147`):
        // `tanh(re)*im - tanh(im)*re`. Quantized via the integer-bin
        // tanh LUT — saturates to ±1 outside the LUT range.
        self.lut_tanh_lookup(re) * im - self.lut_tanh_lookup(im) * re
    }

    fn lut_tanh_lookup(&self, val: f32) -> f32 {
        // Saturation matches dbdexter `pll.c:156`: `> 15 → 1`,
        // `< -16 → -1`. The asymmetric upper bound is faithful — the
        // LUT covers `i = 0..32` mapping to `tanh(-16..15)`, so values
        // in `[15, 16]` clip to 1 rather than indexing past the end.
        if val > 15.0 {
            return 1.0;
        }
        if val < -16.0 {
            return -1.0;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "val is bounded to (-16, 15] so (val + 16) is in (0, 31] — fits in usize"
        )]
        let idx = (val as i32 + LUT_TANH_OFFSET) as usize;
        self.lut_tanh[idx]
    }

    fn apply_loop_update(&mut self, error: f32) {
        // Phase-side update, wrapped to [0, 2π). dbdexter uses
        // C's `fmod` (truncate-toward-zero), but for the bounded
        // `phase + alpha*error` values the loop produces in
        // practice, `rem_euclid` differs only when `phase` has
        // already dipped negative — and gives the [0, 2π)
        // representation that the `mix*` wrap check expects.
        self.phase = (self.phase + self.alpha * error).rem_euclid(2.0 * PI);
        self.freq += self.beta * error;

        // Lock detector — exponential moving average of |error|
        // with hysteresis at the dbdexter thresholds, plus a
        // signal-magnitude floor so a literal-zero input can't
        // trigger lock just by letting the |error| EMA decay
        // (the EMA starts at 1000 and decays toward 0 with pole
        // 0.001; without the floor it crosses LOW after ~2700
        // updates regardless of signal). Per CR round 2 on
        // PR #663.
        self.err_ema = self.err_ema * (1.0 - ERR_POLE) + error.abs() * ERR_POLE;
        let has_signal = self.sig_ema >= LOCK_SIG_FLOOR;
        if self.err_ema < LOCK_THRESHOLD_LOW && has_signal && !self.locked {
            self.locked = true;
            self.locked_once = true;
        } else if (self.err_ema > LOCK_THRESHOLD_HIGH || !has_signal) && self.locked {
            self.locked = false;
        }

        // Free-run sweep while unlocked — slew the frequency
        // estimate back and forth across ±fmax until the lock
        // detector trips.
        if !self.locked {
            self.freq += FREE_RUN_STEP * f32::from(self.sweep_dir);
        }
        if self.freq >= self.fmax {
            self.sweep_dir = -1;
        } else if self.freq <= -self.fmax {
            self.sweep_dir = 1;
        }
        self.freq = self.freq.clamp(-self.fmax, self.fmax);
    }

    fn advance_phase(&mut self) {
        self.phase += self.freq;
        // dbdexter only wraps positive (`pll.c:61`); a negative
        // frequency leaves the phase to grow negative, but
        // `sin_cos` handles that correctly and the bounded sweep
        // keeps the magnitude small enough that float precision
        // doesn't degrade over the length of a pass.
        if self.phase >= 2.0 * PI {
            self.phase -= 2.0 * PI;
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    clippy::unwrap_used,
    clippy::many_single_char_names,
    reason = "test code: short binding names match the C reference's variable names"
)]
mod tests {
    use super::*;

    /// Loop bandwidth used in the tests below — dbdexter's
    /// default `pll_bw = 1` Hz, normalized to a per-mix-call
    /// bandwidth at 72 ksym/s with one mix per symbol (QPSK
    /// scaling). Wider than what we'd actually deploy but the
    /// tests need to converge in reasonable wall time.
    const TEST_BW: f32 = 0.01;

    #[test]
    fn rejects_invalid_bandwidth() {
        assert!(MeteorPll::new(0.0, false, None).is_err());
        assert!(MeteorPll::new(-0.1, false, None).is_err());
        assert!(MeteorPll::new(f32::NAN, false, None).is_err());
        assert!(MeteorPll::new(f32::INFINITY, false, None).is_err());
    }

    #[test]
    fn rejects_non_finite_freq_max() {
        // Per CR round 1 on PR #663 — without this guard,
        // `Some(NaN)` slipped past the negative-or-clamp branch
        // and poisoned `fmax`, silently breaking the unlocked-
        // state frequency sweep (every `>= fmax` and `<= -fmax`
        // comparison against NaN returns `false`).
        assert!(MeteorPll::new(TEST_BW, false, Some(f32::NAN)).is_err());
        assert!(MeteorPll::new(TEST_BW, false, Some(f32::INFINITY)).is_err());
        assert!(MeteorPll::new(TEST_BW, true, Some(f32::NEG_INFINITY)).is_err());
    }

    #[test]
    fn freq_max_default_used_when_none() {
        let pll = MeteorPll::new(TEST_BW, false, None).unwrap();
        // QPSK: fmax stays at FREQ_MAX_DEFAULT.
        assert!((pll.fmax - FREQ_MAX_DEFAULT).abs() < 1e-6);
    }

    #[test]
    fn oqpsk_halves_fmax() {
        let pll = MeteorPll::new(TEST_BW, true, None).unwrap();
        assert!((pll.fmax - FREQ_MAX_DEFAULT / 2.0).abs() < 1e-6);
    }

    #[test]
    fn freq_max_negative_falls_back_to_default() {
        // dbdexter (`pll.c:30`): negative selects the default.
        let pll = MeteorPll::new(TEST_BW, false, Some(-1.0)).unwrap();
        assert!((pll.fmax - FREQ_MAX_DEFAULT).abs() < 1e-6);
    }

    #[test]
    fn freq_max_above_one_clamps_to_one() {
        // dbdexter (`pll.c:31`): values > 1 are clamped to 1.
        let pll = MeteorPll::new(TEST_BW, false, Some(2.0)).unwrap();
        assert!((pll.fmax - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mix_preserves_magnitude_at_zero_phase() {
        let mut pll = MeteorPll::new(TEST_BW, false, None).unwrap();
        let s = Complex::new(0.7, -0.3);
        let out = pll.mix(s);
        let mag_in = (s.re * s.re + s.im * s.im).sqrt();
        let mag_out = (out.re * out.re + out.im * out.im).sqrt();
        assert!((mag_in - mag_out).abs() < 1e-6);
    }

    #[test]
    fn mix_i_and_mix_q_match_full_mix_at_same_phase() {
        // mix_i + mix_q should reconstruct the full mix output at
        // the same phase. Per call, however, each advances the
        // phase — so we have to evaluate them at a known starting
        // phase using two fresh PLLs.
        let mut a = MeteorPll::new(TEST_BW, false, None).unwrap();
        let mut b = MeteorPll::new(TEST_BW, false, None).unwrap();
        let s = Complex::new(0.5, 0.2);
        let full = a.mix(s);
        let i = b.mix_i(s);
        // Reset the second PLL's phase so mix_q sees the same
        // starting phase that mix_i did. (mix_i advanced it by
        // `freq` = 0; nothing to undo for a fresh PLL.)
        let q = {
            let mut c = MeteorPll::new(TEST_BW, false, None).unwrap();
            c.mix_q(s)
        };
        assert!((i - full.re).abs() < 1e-6, "mix_i mismatch");
        assert!((q - full.im).abs() < 1e-6, "mix_q mismatch");
    }

    #[test]
    fn lut_tanh_saturates_outside_range() {
        let pll = MeteorPll::new(TEST_BW, false, None).unwrap();
        assert!((pll.lut_tanh_lookup(100.0) - 1.0).abs() < 1e-6);
        assert!((pll.lut_tanh_lookup(-100.0) - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn lut_tanh_in_range_returns_table_value() {
        let pll = MeteorPll::new(TEST_BW, false, None).unwrap();
        // val = 0 → idx = 16 → tanh(0) = 0.
        assert!(pll.lut_tanh_lookup(0.0).abs() < 1e-6);
        // val = 1 → idx = 17 → tanh(1).
        assert!((pll.lut_tanh_lookup(1.0) - 1.0_f32.tanh()).abs() < 1e-6);
    }

    #[test]
    fn freq_clamped_to_fmax_after_update() {
        let mut pll = MeteorPll::new(TEST_BW, false, Some(0.1)).unwrap();
        // Force-feed a huge error to drive freq beyond fmax.
        for _ in 0..10_000 {
            pll.apply_loop_update(1000.0);
        }
        assert!(pll.freq.abs() <= 0.1 + 1e-6);
    }

    #[test]
    fn locks_onto_clean_qpsk_constellation() {
        // Drive the PLL with synthetic clean QPSK at zero offset.
        // Two production-realistic inputs:
        //
        // 1. Constellation pre-scaled to dbdexter's post-AGC
        //    magnitude (190/√2 ≈ 134.35 per rail) so the signal-
        //    magnitude lock gate sees enough power. In
        //    production the upstream [`MeteorAgc`] applies this
        //    scaling automatically; the unit test bypasses AGC.
        // 2. The realistic loop bandwidth (`PROD_BW`) — wider
        //    `TEST_BW = 0.01` overshoots wildly when paired with
        //    AGC-scale errors and the loop oscillates instead of
        //    locking. The production OQPSK PLL bandwidth was
        //    derived to be stable at this input scale; honour it.
        const RAIL: f32 = 134.35;
        const PROD_BW: f32 = 8.726_646e-5; // 2π × 1 Hz / 72_000 Hz, matches OQPSK_PLL_BW
        let mut pll = MeteorPll::new(PROD_BW, false, None).unwrap();
        let symbols = [
            Complex::new(RAIL, RAIL),
            Complex::new(-RAIL, RAIL),
            Complex::new(RAIL, -RAIL),
            Complex::new(-RAIL, -RAIL),
        ];
        // The narrow loop takes longer to converge — pump enough
        // samples that both the |error| EMA and the signal
        // magnitude EMA settle into the lock window.
        for n in 0..200_000 {
            let s = symbols[n % 4];
            let mixed = pll.mix(s);
            pll.update_estimate(mixed.re, mixed.im);
        }
        assert!(
            pll.locked_once(),
            "PLL should lock onto a clean QPSK constellation"
        );
    }
}
