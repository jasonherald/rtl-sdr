//! Root-raised-cosine matched filter for Meteor LRPT QPSK.
//!
//! Coefficients computed from the standard RRC formula at β = 0.6,
//! span 31 symbols, 2 samples per symbol = 63 taps. Symmetric FIR
//! with circular history buffer; normalized to unity DC gain.
//!
//! Reference (read-only): `original/meteor_demod/dsp/filter.c`
//! (`filter_rrc_init`).

use core::f32::consts::PI;

use sdr_types::Complex;

/// Filter span in symbols. Combined with `SPS_FOR_TAP_COUNT`
/// gives `NUM_TAPS = SPAN_SYMBOLS * sps + 1`.
pub const SPAN_SYMBOLS: usize = 31;

/// Samples-per-symbol assumed at tap-design time. The runtime
/// `samples_per_symbol` argument to `RrcFilter::new` must match
/// this — the filter doesn't currently regenerate taps for other
/// sps values (Meteor's chain is fixed at 2 sps).
pub const SPS_FOR_TAP_COUNT: usize = 2;

/// Number of filter taps. Odd-length so the filter has a single
/// peak tap; derived from span + sps so a future tap-count tweak
/// can't drift away from the documented relationship.
pub const NUM_TAPS: usize = SPAN_SYMBOLS * SPS_FOR_TAP_COUNT + 1;

/// Symbol-rate rolloff factor for Meteor LRPT (β).
pub const ROLLOFF: f32 = 0.6;

/// Root-raised-cosine FIR matched filter. Single-channel, complex
/// in/out (the QPSK signal is complex baseband).
pub struct RrcFilter {
    taps: [f32; NUM_TAPS],
    history: [Complex; NUM_TAPS],
    write_idx: usize,
}

impl RrcFilter {
    /// Build the RRC filter at `samples_per_symbol` (typically 2
    /// for the standard 2 sps QPSK chain).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation,
        reason = "tap-design indices are bounded by NUM_TAPS = 63; \
                  centered loop variable fits trivially in i32 / f32"
    )]
    pub fn new(samples_per_symbol: usize) -> Self {
        let mut taps = [0.0_f32; NUM_TAPS];
        let mid = (NUM_TAPS / 2) as i32;
        for (i, tap) in taps.iter_mut().enumerate() {
            let centered = i as i32 - mid;
            let t = centered as f32 / samples_per_symbol as f32;
            *tap = rrc_impulse(t, ROLLOFF);
        }
        // Normalize to unity DC gain (sum of taps = 1) so the
        // filter doesn't change the signal's average magnitude.
        let sum: f32 = taps.iter().sum();
        if sum.abs() > 1e-6 {
            for tap in &mut taps {
                *tap /= sum;
            }
        }
        Self {
            taps,
            history: [Complex::new(0.0, 0.0); NUM_TAPS],
            write_idx: 0,
        }
    }

    /// Process one complex sample. Returns the filtered sample.
    pub fn process(&mut self, x: Complex) -> Complex {
        self.history[self.write_idx] = x;
        self.write_idx = (self.write_idx + 1) % NUM_TAPS;
        let mut acc = Complex::new(0.0, 0.0);
        for i in 0..NUM_TAPS {
            let idx = (self.write_idx + i) % NUM_TAPS;
            acc += self.history[idx] * self.taps[NUM_TAPS - 1 - i];
        }
        acc
    }
}

/// Continuous-time RRC impulse response. Handles the t = 0 and
/// t = ±T / (4β) singularities by L'Hopital expansion (which the
/// C reference also does).
#[allow(
    clippy::float_cmp,
    reason = "comparing exact-zero and exact-singularity values is intentional"
)]
fn rrc_impulse(t: f32, beta: f32) -> f32 {
    if t.abs() < 1e-6 {
        return 1.0 - beta + 4.0 * beta / PI;
    }
    let denom_singular = (4.0 * beta * t).powi(2);
    if (denom_singular - 1.0).abs() < 1e-6 {
        let s = (PI / (4.0 * beta)).sin();
        let c = (PI / (4.0 * beta)).cos();
        return (beta / 2.0_f32.sqrt()) * ((1.0 + 2.0 / PI) * s + (1.0 - 2.0 / PI) * c);
    }
    let num = (PI * t * (1.0 - beta)).sin() + 4.0 * beta * t * (PI * t * (1.0 + beta)).cos();
    let den = PI * t * (1.0 - denom_singular);
    num / den
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
mod tests {
    use super::*;

    #[test]
    fn rrc_taps_are_symmetric() {
        let f = RrcFilter::new(2);
        for i in 0..(NUM_TAPS / 2) {
            let a = f.taps[i];
            let b = f.taps[NUM_TAPS - 1 - i];
            assert!(
                (a - b).abs() < 1e-5,
                "RRC taps must be symmetric: tap[{i}]={a}, tap[{}]={b}",
                NUM_TAPS - 1 - i,
            );
        }
    }

    #[test]
    fn rrc_passes_dc_with_unity_gain() {
        let mut f = RrcFilter::new(2);
        let mut last = Complex::new(0.0, 0.0);
        for _ in 0..200 {
            last = f.process(Complex::new(1.0, 0.0));
        }
        assert!(
            (last.re - 1.0).abs() < 1e-3,
            "DC response should be unity, got {}",
            last.re,
        );
        assert!(last.im.abs() < 1e-3, "DC response imag should be 0");
    }

    #[test]
    fn rrc_attenuates_at_symbol_rate() {
        // A tone at the symbol rate (alternating ±1 at 2 sps)
        // sits beyond the rolloff region for β=0.6 and should be
        // heavily attenuated.
        let mut f = RrcFilter::new(2);
        let mut max_after_settle = 0.0_f32;
        for n in 0..400 {
            let phase = PI * n as f32; // alternating ±1
            let s = Complex::new(phase.cos(), 0.0);
            let out = f.process(s);
            if n > NUM_TAPS {
                max_after_settle = max_after_settle.max(out.re.abs());
            }
        }
        assert!(
            max_after_settle < 0.2,
            "RRC should attenuate symbol-rate tone, got peak {max_after_settle}",
        );
    }

    #[test]
    fn span_symbols_matches_num_taps() {
        // Pin the relationship so a future tap-count tweak doesn't
        // silently break the convolution loop.
        assert_eq!(NUM_TAPS, SPAN_SYMBOLS * SPS_FOR_TAP_COUNT + 1);
    }
}
