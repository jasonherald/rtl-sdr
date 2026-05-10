//! Meteor-M AGC — faithful port of `dbdexter/meteor_demod/dsp/agc.c`.
//!
//! dbdexter's PLL is calibrated for an input that's been normalized
//! to **|sample| = 190** by this AGC stage (the constellation rails
//! land at ~134 = 190/√2). All of the carrier-recovery loop's
//! tuning — alpha/beta from the loop bandwidth, the lock detector's
//! 85 / 105 EMA hysteresis, the integer-bin tanh LUT, the free-run
//! frequency-sweep step — assumes that scale.
//!
//! Skipping the AGC and feeding the PLL a unit-magnitude
//! constellation (rails at ±0.707) silently breaks two things:
//!
//! 1. The integer-bin tanh LUT becomes useless — values in (−1, 1)
//!    round to index 16 = `tanh(0)` = 0, so `compute_error` always
//!    returns 0 and the loop never tracks phase.
//! 2. The lock detector's `|error|` EMA decays from `1000` toward
//!    `0` with pole `0.001`, crossing the lock threshold of `85`
//!    after ~2700 update calls — purely from time elapsed. Even a
//!    zero-IQ input "locks" eventually, and the free-run frequency
//!    sweep stops scanning before the carrier is found.
//!
//! With the AGC in place: zero-IQ silence drives the gain up
//! toward saturation, the (post-AGC) noise becomes random with
//! magnitude ~190, the PLL error is non-zero noise (not literal
//! 0), the EMA stays high, and `locked` stays false until a real
//! carrier shows up. dbdexter's tuning works as designed.
//!
//! Per CR round 2 on PR #663.

use sdr_types::Complex;

/// Target post-AGC sample magnitude. Per dbdexter
/// (`agc.c:5` — `FLOAT_TARGET_MAG`).
pub const TARGET_MAG: f32 = 190.0;

/// Pole of the leaky integrator that estimates the DC bias to
/// subtract before applying gain. Per dbdexter (`agc.c:6`).
const BIAS_POLE: f32 = 0.001;

/// Pole of the gain-update loop. Per dbdexter (`agc.c:7`).
const GAIN_POLE: f32 = 0.0001;

/// Single-sample AGC. Tracks DC bias + a gain that drives the
/// post-bias-subtraction sample magnitude toward [`TARGET_MAG`].
pub struct MeteorAgc {
    /// Multiplicative gain applied to the bias-corrected sample.
    /// Per dbdexter `_float_gain` (`agc.c:9`).
    gain: f32,
    /// Running DC-bias estimate. Per dbdexter `_float_bias`
    /// (`agc.c:10`).
    bias: Complex,
}

impl Default for MeteorAgc {
    fn default() -> Self {
        Self::new()
    }
}

impl MeteorAgc {
    /// Build an AGC at unity gain and zero bias. Same initial
    /// state as dbdexter's static globals.
    #[must_use]
    pub fn new() -> Self {
        Self {
            gain: 1.0,
            bias: Complex::new(0.0, 0.0),
        }
    }

    /// Process one sample. Updates DC-bias and gain estimates as
    /// side effects; returns the bias-subtracted, gain-scaled
    /// sample.
    pub fn process(&mut self, sample: Complex) -> Complex {
        // Leaky-integrator DC-bias estimate: subtract before
        // applying gain so a static offset doesn't grow into a
        // huge DC excursion as the gain ramps up.
        self.bias = self.bias * (1.0 - BIAS_POLE) + sample * BIAS_POLE;
        let centered = sample - self.bias;
        // Gain stage. Update is "post-multiply": the current gain
        // applies to *this* sample; the gain update from this
        // sample's magnitude error affects the *next* sample.
        // Mirrors dbdexter `agc.c:20-22`.
        let scaled = centered * self.gain;
        let mag = (scaled.re * scaled.re + scaled.im * scaled.im).sqrt();
        self.gain += GAIN_POLE * (TARGET_MAG - mag);
        if self.gain < 0.0 {
            self.gain = 0.0;
        }
        scaled
    }

    /// Current gain — useful for diagnostics and tests.
    #[must_use]
    pub fn gain(&self) -> f32 {
        self.gain
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::*;

    #[test]
    fn unity_gain_at_construction() {
        let agc = MeteorAgc::new();
        assert!((agc.gain() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn ramps_gain_up_for_small_input() {
        // Input magnitude of 1.0 is far below the 190 target —
        // the gain should grow over time toward whatever value
        // brings the output to ~190. Use a mean-zero alternating
        // pattern so the DC-bias filter tracks ~zero rather than
        // chasing a constant input (which would suppress the
        // signal entirely — that's a separate test below).
        let mut agc = MeteorAgc::new();
        let s_pos = Complex::new(0.707, 0.707);
        let s_neg = Complex::new(-0.707, -0.707);
        for n in 0..100_000 {
            let _ = agc.process(if n % 2 == 0 { s_pos } else { s_neg });
        }
        // After 100k samples, post-AGC magnitude should be near
        // the target. Convergence isn't instant (GAIN_POLE is
        // tiny), so allow generous slack.
        let final_out = agc.process(s_pos);
        let mag = (final_out.re * final_out.re + final_out.im * final_out.im).sqrt();
        assert!(
            (mag - TARGET_MAG).abs() < 30.0,
            "AGC should normalize |sample|=1 to ~{TARGET_MAG}, got {mag}",
        );
    }

    #[test]
    fn ramps_gain_up_on_zero_iq() {
        // Zero IQ should still drive the gain up (toward
        // saturation), because the gain update is `gain += pole *
        // (target - |output|)` and |output| stays at 0. dbdexter's
        // AGC has no upper bound on gain — that's by design, so a
        // signal that fades in late in a pass still gets amplified
        // properly.
        let mut agc = MeteorAgc::new();
        let zero = Complex::new(0.0, 0.0);
        for _ in 0..10_000 {
            let _ = agc.process(zero);
        }
        // Each call: gain += 0.0001 * 190 = 0.019. After 10k
        // calls: gain ≈ 1 + 190 ≈ 191.
        assert!(
            agc.gain() > 50.0,
            "gain should ramp up on zero input (no signal to bring it back down), got {}",
            agc.gain(),
        );
    }

    #[test]
    fn removes_dc_bias() {
        // Constant DC-only input — after the bias filter settles,
        // the output should approach 0 (bias subtracted, no AC
        // content for the gain stage to amplify).
        let mut agc = MeteorAgc::new();
        let dc = Complex::new(5.0, -3.0);
        for _ in 0..50_000 {
            let _ = agc.process(dc);
        }
        let out = agc.process(dc);
        let mag = (out.re * out.re + out.im * out.im).sqrt();
        // The bias estimate converges geometrically; at 50k
        // samples with a pole of 0.001 it's well within 1% of the
        // input value, so the residual should be a small fraction
        // of the original magnitude.
        let dc_mag = (dc.re * dc.re + dc.im * dc.im).sqrt();
        assert!(
            mag < dc_mag,
            "AGC should subtract the DC bias, got residual mag {mag} vs original {dc_mag}",
        );
    }

    #[test]
    fn gain_clamped_non_negative() {
        // Force a scenario where the gain would otherwise drop
        // below zero (huge over-amplitude input). gain should
        // clamp at 0, not overshoot negative.
        let mut agc = MeteorAgc::new();
        let huge = Complex::new(1.0e6, 1.0e6);
        for _ in 0..100 {
            let _ = agc.process(huge);
        }
        assert!(
            agc.gain() >= 0.0,
            "gain must clamp at zero, got {}",
            agc.gain()
        );
    }
}
