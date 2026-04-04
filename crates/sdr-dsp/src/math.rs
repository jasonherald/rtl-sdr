//! DSP math utilities — ports SDR++ `dsp::math` namespace.

use core::f64::consts::PI;

/// Convert frequency in Hz to angular frequency in radians/sample.
///
/// Ports SDR++ `dsp::math::hzToRads`.
#[inline]
pub fn hz_to_rads(freq: f64, sample_rate: f64) -> f64 {
    2.0 * PI * (freq / sample_rate)
}

/// Normalize a phase value to the range `(-pi, pi]`.
///
/// Handles arbitrary input values (not just single-wrap).
/// Based on SDR++ `dsp::math::normalizePhase` but extended for robustness.
#[inline]
pub fn normalize_phase(diff: f32) -> f32 {
    let mut result =
        (diff + core::f32::consts::PI).rem_euclid(core::f32::consts::TAU) - core::f32::consts::PI;
    // rem_euclid can return exactly -PI due to floating point; map to PI
    if result <= -core::f32::consts::PI {
        result += core::f32::consts::TAU;
    }
    result
}

/// Sinc function: `sin(x) / x`, with `sinc(0) = 1`.
///
/// Ports SDR++ `dsp::math::sinc`.
#[inline]
pub fn sinc(x: f64) -> f64 {
    if x == 0.0 { 1.0 } else { x.sin() / x }
}

/// Fast atan2 approximation using rational polynomial.
///
/// Ports SDR++ `dsp::math::fastAtan2`. Note: SDR++ parameter order is
/// `fastAtan2(x, y)` where x=real, y=imag — this matches that convention.
/// Worst-case error ~0.07 radians.
///
/// Note: `Complex::fast_phase` in sdr-types uses the same algorithm inline.
/// Duplication is intentional — sdr-types cannot depend on sdr-dsp, and
/// `Complex::fast_phase` avoids the function call overhead in tight loops.
#[inline]
pub fn fast_atan2(x: f32, y: f32) -> f32 {
    let abs_y = y.abs();
    if x == 0.0 && y == 0.0 {
        return 0.0;
    }
    let angle = if x >= 0.0 {
        let r = (x - abs_y) / (x + abs_y);
        core::f32::consts::FRAC_PI_4 - core::f32::consts::FRAC_PI_4 * r
    } else {
        let r = (x + abs_y) / (abs_y - x);
        3.0 * core::f32::consts::FRAC_PI_4 - core::f32::consts::FRAC_PI_4 * r
    };
    if y < 0.0 { -angle } else { angle }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    const EPS_F32: f32 = 1e-6;
    const EPS_F64: f64 = 1e-10;
    const TEST_SAMPLE_RATE: f64 = 48_000.0;
    const FAST_ATAN2_MAX_ERROR: f32 = 0.08;

    fn approx_eq_f32(a: f32, b: f32) -> bool {
        (a - b).abs() < EPS_F32
    }

    fn approx_eq_f64(a: f64, b: f64) -> bool {
        (a - b).abs() < EPS_F64
    }

    #[test]
    fn test_hz_to_rads() {
        // Nyquist frequency should give pi radians/sample
        assert!(approx_eq_f64(hz_to_rads(24_000.0, TEST_SAMPLE_RATE), PI));
        // DC should give 0
        assert!(approx_eq_f64(hz_to_rads(0.0, TEST_SAMPLE_RATE), 0.0));
        // Quarter sample rate should give pi/2
        assert!(approx_eq_f64(
            hz_to_rads(12_000.0, TEST_SAMPLE_RATE),
            PI / 2.0
        ));
    }

    #[test]
    fn test_normalize_phase() {
        // Already in range
        assert!(approx_eq_f32(normalize_phase(1.0), 1.0));
        assert!(approx_eq_f32(normalize_phase(-1.0), -1.0));
        // Wraps from above pi
        assert!(approx_eq_f32(
            normalize_phase(core::f32::consts::PI + 0.5),
            -core::f32::consts::PI + 0.5
        ));
        // Wraps from below -pi
        assert!(approx_eq_f32(
            normalize_phase(-core::f32::consts::PI - 0.5),
            core::f32::consts::PI - 0.5
        ));
        // Multi-wrap: 5*pi should normalize to pi
        assert!(approx_eq_f32(
            normalize_phase(5.0 * core::f32::consts::PI),
            core::f32::consts::PI
        ));
        // Multi-wrap negative: -5*pi should normalize to pi (not -pi per contract)
        let result = normalize_phase(-5.0 * core::f32::consts::PI);
        assert!(
            approx_eq_f32(result, core::f32::consts::PI),
            "expected pi, got {result}"
        );
        // Zero stays zero
        assert!(approx_eq_f32(normalize_phase(0.0), 0.0));
    }

    #[test]
    fn test_sinc() {
        // sinc(0) = 1
        assert_eq!(sinc(0.0), 1.0);
        // sinc(pi) = 0
        assert!(approx_eq_f64(sinc(PI), 0.0));
        // sinc(x) = sin(x)/x for non-zero
        let x = 1.5;
        assert!(approx_eq_f64(sinc(x), x.sin() / x));
    }

    #[test]
    fn test_fast_atan2() {
        // Compare against standard atan2 — within ~0.08 radians
        let cases: [(f32, f32); 7] = [
            (1.0, 0.0),
            (0.0, 1.0),
            (-1.0, 0.0),
            (0.0, -1.0),
            (1.0, 1.0),
            (-1.0, -1.0),
            (3.0, 4.0),
        ];
        for (x, y) in &cases {
            let fast = fast_atan2(*x, *y);
            let exact = y.atan2(*x);
            let diff = (fast - exact).abs();
            assert!(
                diff < FAST_ATAN2_MAX_ERROR,
                "fast_atan2 error {diff} for ({x}, {y})"
            );
        }
        // Zero returns zero
        assert_eq!(fast_atan2(0.0, 0.0), 0.0);
    }
}
