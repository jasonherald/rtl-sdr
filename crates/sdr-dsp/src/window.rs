//! Window functions for spectral analysis and FIR filter design.
//!
//! Ports SDR++ `dsp::window` namespace. All functions operate on a single
//! sample index `n` within a window of length `N`.

use core::f64::consts::PI;

// Window coefficient constants.
const HANN_COEFS: [f64; 2] = [0.5, 0.5];
const HAMMING_COEFS: [f64; 2] = [0.54, 0.46];
const BLACKMAN_COEFS: [f64; 3] = [0.42, 0.5, 0.08];

#[allow(clippy::unreadable_literal)]
const BLACKMAN_HARRIS_COEFS: [f64; 4] = [0.35875, 0.48829, 0.14128, 0.01168];

#[allow(clippy::unreadable_literal)]
const BLACKMAN_NUTTALL_COEFS: [f64; 4] = [0.3635819, 0.4891775, 0.1365995, 0.0106411];

#[allow(clippy::unreadable_literal)]
const NUTTALL_COEFS: [f64; 4] = [0.355768, 0.487396, 0.144232, 0.012604];

/// Generalized cosine window — base implementation for most window functions.
///
/// Ports SDR++ `dsp::window::cosine`. Computes:
/// `w(n) = sum_k (-1)^k * coefs[k] * cos(2*pi*k*n/N)`
#[allow(clippy::cast_precision_loss)]
fn cosine(n: f64, big_n: f64, coefs: &[f64]) -> f64 {
    let mut win = 0.0;
    let mut sign = 1.0;
    for (i, &c) in coefs.iter().enumerate() {
        win += sign * c * (i as f64 * 2.0 * PI * n / big_n).cos();
        sign = -sign;
    }
    win
}

/// Rectangular window (no windowing). Always returns 1.0.
#[inline]
pub fn rectangular(_n: f64, _big_n: f64) -> f64 {
    1.0
}

/// Hann window.
#[inline]
pub fn hann(n: f64, big_n: f64) -> f64 {
    cosine(n, big_n, &HANN_COEFS)
}

/// Hamming window.
#[inline]
pub fn hamming(n: f64, big_n: f64) -> f64 {
    cosine(n, big_n, &HAMMING_COEFS)
}

/// Blackman window.
#[inline]
pub fn blackman(n: f64, big_n: f64) -> f64 {
    cosine(n, big_n, &BLACKMAN_COEFS)
}

/// Blackman-Harris window (4-term).
#[inline]
pub fn blackman_harris(n: f64, big_n: f64) -> f64 {
    cosine(n, big_n, &BLACKMAN_HARRIS_COEFS)
}

/// Blackman-Nuttall window (4-term).
#[inline]
pub fn blackman_nuttall(n: f64, big_n: f64) -> f64 {
    cosine(n, big_n, &BLACKMAN_NUTTALL_COEFS)
}

/// Nuttall window (4-term, continuous first derivative).
#[inline]
pub fn nuttall(n: f64, big_n: f64) -> f64 {
    cosine(n, big_n, &NUTTALL_COEFS)
}

/// Modified Bessel function of the first kind, order 0.
///
/// Power-series implementation:
/// `I0(x) = sum_{k=0..} (x/2)^(2k) / (k!)^2`. Converges fast for the
/// arguments Kaiser windows actually use (β·sqrt(1-(2n/N-1)²) where
/// β is at most ~12 for any realistic stopband attenuation), so the
/// convergence guard at 30 iterations is effectively a runtime
/// guarantee — the loop typically exits in 10-15 terms.
///
/// Used by [`kaiser`] but exposed for callers that need Bessel-I0
/// elsewhere (e.g. independent verification tests). Not perf-critical
/// — Kaiser tap design runs once per filter, never per sample.
#[must_use]
pub fn bessel_i0(x: f64) -> f64 {
    let half_x_sq = (x / 2.0).powi(2);
    let mut term = 1.0_f64;
    let mut sum = 1.0_f64;
    // 30 terms is comfortably more than needed for x < 30 (β ≤ 12 →
    // x ≤ 12, well-converged in ~12 terms). Cap exists only to
    // bound runtime if a future caller passes a pathological β.
    for k in 1..30 {
        let k_f = f64::from(k);
        term *= half_x_sq / (k_f * k_f);
        sum += term;
        if term / sum < 1e-12 {
            break;
        }
    }
    sum
}

/// Kaiser window with shape parameter β.
///
/// Per Kaiser/Schafer (1980): for an N-tap window indexed `n ∈ [0, N-1]`,
/// `w[n] = I0(β · sqrt(1 - ((2n - (N-1)) / (N-1))²)) / I0(β)`.
/// Endpoints `n = 0` and `n = N-1` both give arg = 0 → value
/// `1 / I0(β)`. Peak at `n = (N-1)/2` gives arg = β → value 1.
/// Larger β = wider main lobe, lower sidelobes.
///
/// The pair `(β, N)` is typically derived from a stopband-attenuation
/// target via the Kaiser design formulas — see [`kaiser_beta`] and
/// the Kaiser-windowed-sinc functions in [`crate::taps`].
///
/// **Argument convention:** unlike the cosine-family windows in this
/// module (`hann`, `nuttall`, etc.) which exploit cos's periodicity
/// to handle off-center arguments, Kaiser is non-periodic and so
/// requires the standard `[0, N-1]` index convention. Callers must
/// pass the raw sample index `i ∈ [0, N-1]` and the tap count `N`
/// directly — do NOT pre-center via `i - N/2`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn kaiser(n: f64, big_n: f64, beta: f64) -> f64 {
    // For a count-tap symmetric window, indices run 0..count-1 and
    // the peak sits at (count-1)/2, not count/2. Using `count - 1`
    // for the normalization ensures `kaiser(0, N) == kaiser(N-1, N)`
    // — the symmetry property that linear-phase FIR design relies on.
    let m = big_n - 1.0;
    if m <= 0.0 {
        // Degenerate — single-tap window has the trivial value 1.
        return 1.0;
    }
    let half = m / 2.0;
    let centered = (n - half) / half;
    let arg = beta * (1.0 - centered * centered).max(0.0).sqrt();
    bessel_i0(arg) / bessel_i0(beta)
}

/// Compute the Kaiser shape parameter β for a target stopband
/// attenuation in dB. Per Kaiser/Schafer (1980):
///
/// * `atten >  50 dB` → β = 0.1102·(atten − 8.7)
/// * `atten >= 21 dB` → β = 0.5842·(atten − 21)^0.4 + 0.07886·(atten − 21)
/// * `atten <  21 dB` → β = 0
///
/// The 21 dB / 50 dB boundaries are where the empirical fits change
/// regimes; below 21 dB Kaiser is just a rectangular window.
#[must_use]
pub fn kaiser_beta(atten_db: f64) -> f64 {
    if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else if atten_db >= 21.0 {
        let a = atten_db - 21.0;
        0.5842 * a.powf(0.4) + 0.07886 * a
    } else {
        0.0
    }
}

/// Estimate Kaiser window length for a target stopband attenuation
/// + transition-band width.
///
/// Per Kaiser/Schafer (1980): `N = ⌈(A − 8) / (2.285·Δω)⌉ + 1` where
/// `Δω` is the transition width in radians/sample.
///
/// Returns an odd value (Type-I FIR has zero phase + symmetric taps).
/// Capped at 1 to prevent zero-length windows when the formula gives
/// pathological values (negative or zero atten input).
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn kaiser_length(atten_db: f64, transition_rad: f64) -> usize {
    let raw = ((atten_db - 8.0) / (2.285 * transition_rad)).ceil();
    let mut n = if raw.is_finite() && raw > 0.0 {
        raw as usize + 1
    } else {
        1
    };
    if n.is_multiple_of(2) {
        n += 1;
    }
    n
}

/// Apply a window function to a buffer in-place.
///
/// `window_fn` is called with `(n, N)` for each sample index.
#[allow(clippy::cast_precision_loss)]
pub fn apply<F>(buf: &mut [f64], window_fn: F)
where
    F: Fn(f64, f64) -> f64,
{
    let big_n = buf.len() as f64;
    for (i, sample) in buf.iter_mut().enumerate() {
        *sample *= window_fn(i as f64, big_n);
    }
}

/// Apply a window function to f32 complex samples (multiply re and im).
///
/// `window_fn` is called with `(n, N)` for each sample index.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn apply_complex<F>(buf: &mut [sdr_types::Complex], window_fn: F)
where
    F: Fn(f64, f64) -> f64,
{
    let big_n = buf.len() as f64;
    for (i, sample) in buf.iter_mut().enumerate() {
        let w = window_fn(i as f64, big_n) as f32;
        sample.re *= w;
        sample.im *= w;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-10;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < EPS
    }

    /// Compute the coherent gain (sum of window values) for a window of size N.
    #[allow(clippy::cast_precision_loss)]
    fn window_sum(wf: fn(f64, f64) -> f64, big_n: usize) -> f64 {
        (0..big_n).map(|i| wf(i as f64, big_n as f64)).sum()
    }

    #[test]
    fn test_rectangular() {
        for i in 0..10 {
            assert!(approx_eq(rectangular(f64::from(i), 10.0), 1.0));
        }
    }

    #[test]
    fn test_hann_endpoints() {
        assert!(approx_eq(hann(0.0, 64.0), 0.0));
        assert!(approx_eq(hann(64.0, 64.0), 0.0));
    }

    #[test]
    fn test_hann_peak() {
        assert!(approx_eq(hann(32.0, 64.0), 1.0));
    }

    #[test]
    fn test_hamming_endpoints() {
        assert!(approx_eq(hamming(0.0, 64.0), 0.08));
        assert!(approx_eq(hamming(64.0, 64.0), 0.08));
    }

    #[test]
    fn test_hamming_peak() {
        assert!(approx_eq(hamming(32.0, 64.0), 1.0));
    }

    #[test]
    fn test_blackman_endpoints() {
        assert!(blackman(0.0, 64.0).abs() < 1e-4);
    }

    #[test]
    fn test_blackman_peak() {
        assert!(approx_eq(blackman(32.0, 64.0), 1.0));
    }

    #[test]
    fn test_blackman_harris_peak() {
        assert!(approx_eq(blackman_harris(32.0, 64.0), 1.0));
    }

    #[test]
    fn test_blackman_nuttall_peak() {
        assert!(approx_eq(blackman_nuttall(32.0, 64.0), 1.0));
    }

    #[test]
    fn test_nuttall_peak() {
        assert!(approx_eq(nuttall(32.0, 64.0), 1.0));
    }

    #[test]
    fn test_nuttall_endpoints() {
        assert!(nuttall(0.0, 64.0).abs() < 1e-4);
    }

    #[test]
    fn test_window_symmetry() {
        let big_n = 128.0;
        let fns: Vec<fn(f64, f64) -> f64> = vec![
            hann,
            hamming,
            blackman,
            blackman_harris,
            blackman_nuttall,
            nuttall,
        ];
        for wf in &fns {
            for i in 0..64 {
                let n = f64::from(i);
                let left = wf(n, big_n);
                let right = wf(big_n - n, big_n);
                assert!(
                    approx_eq(left, right),
                    "symmetry failed at n={n}: {left} != {right}"
                );
            }
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss, clippy::type_complexity)]
    fn test_window_coherent_gain() {
        // Coherent gain = sum(w[n]) / N. For each window, verify it matches
        // the expected value (first coefficient of the cosine series).
        let n = 128;
        let cases: [(fn(f64, f64) -> f64, f64); 7] = [
            (rectangular, 1.0),
            (hann, HANN_COEFS[0]),
            (hamming, HAMMING_COEFS[0]),
            (blackman, BLACKMAN_COEFS[0]),
            (blackman_harris, BLACKMAN_HARRIS_COEFS[0]),
            (blackman_nuttall, BLACKMAN_NUTTALL_COEFS[0]),
            (nuttall, NUTTALL_COEFS[0]),
        ];
        #[allow(clippy::cast_precision_loss)]
        for (wf, expected_gain) in &cases {
            let gain = window_sum(*wf, n) / n as f64;
            assert!(
                (gain - expected_gain).abs() < 1e-6,
                "coherent gain mismatch: got {gain}, expected {expected_gain}"
            );
        }
    }

    #[test]
    fn test_apply() {
        let mut buf = vec![1.0; 64];
        apply(&mut buf, hann);
        assert!(buf[0].abs() < 1e-10);
        assert!((buf[32] - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_apply_complex() {
        use sdr_types::Complex;
        let mut buf = vec![Complex::new(1.0, 1.0); 64];
        apply_complex(&mut buf, hann);
        assert!(buf[0].re.abs() < 1e-6);
        assert!(buf[0].im.abs() < 1e-6);
        assert!((buf[32].re - 1.0).abs() < 1e-6);
        assert!((buf[32].im - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_bessel_i0_zero() {
        // I0(0) = 1 by definition.
        assert!(approx_eq(bessel_i0(0.0), 1.0));
    }

    #[test]
    fn test_bessel_i0_known_values() {
        // Cross-check against published values from Abramowitz & Stegun
        // Table 9.8 (I0 to 6 sig figs). These are the values noaa-apt's
        // bessel implementation hits too.
        // I0(1)  ≈ 1.2660658
        // I0(2)  ≈ 2.2795853
        // I0(5)  ≈ 27.239872
        // I0(10) ≈ 2815.7167
        assert!((bessel_i0(1.0) - 1.266_065_877_752_008).abs() < 1e-9);
        assert!((bessel_i0(2.0) - 2.279_585_302_336_067).abs() < 1e-9);
        assert!((bessel_i0(5.0) - 27.239_871_823_604).abs() < 1e-6);
        assert!((bessel_i0(10.0) - 2_815.716_628_466_254).abs() < 1e-3);
    }

    #[test]
    fn test_kaiser_endpoints_and_peak() {
        // For an N-tap symmetric window indexed `n ∈ [0, N-1]`:
        // both endpoints are 1/I0(β), peak (at the middle index)
        // is exactly 1.0. We use an odd N so the peak lands on
        // an integer index; for an even N the peak is between
        // samples and no integer index hits exactly 1.0.
        let beta = 5.0;
        let big_n = 65.0; // odd → peak at integer index (N-1)/2 = 32
        let endpoint_lo = kaiser(0.0, big_n, beta);
        let endpoint_hi = kaiser(big_n - 1.0, big_n, beta);
        let mid = kaiser((big_n - 1.0) / 2.0, big_n, beta);
        assert!((mid - 1.0).abs() < 1e-9, "expected peak == 1.0, got {mid}");
        let expected_endpoint = 1.0 / bessel_i0(beta);
        assert!((endpoint_lo - expected_endpoint).abs() < 1e-9);
        assert!((endpoint_hi - expected_endpoint).abs() < 1e-9);
        assert!(
            (endpoint_lo - endpoint_hi).abs() < 1e-12,
            "endpoints must match exactly for symmetric Kaiser"
        );
    }

    #[test]
    fn test_kaiser_symmetry() {
        // Symmetric windows on `[0, N-1]` have `w[n] == w[(N-1) - n]`
        // — NOT `w[n] == w[N - n]`, which would be off-by-one.
        let beta = 8.0;
        let big_n_i: i32 = 128;
        let big_n = f64::from(big_n_i);
        for i in 0..(big_n_i / 2) {
            let n = f64::from(i);
            let left = kaiser(n, big_n, beta);
            let right = kaiser(big_n - 1.0 - n, big_n, beta);
            assert!(
                approx_eq(left, right),
                "Kaiser symmetry failed at n={n}: {left} != {right}",
            );
        }
    }

    #[test]
    fn test_kaiser_single_tap_is_unity() {
        // Edge case: `big_n = 1` would give a degenerate denominator
        // (m = 0). The implementation falls back to 1.0 — single-tap
        // windows are trivially symmetric and at unity gain.
        assert!((kaiser(0.0, 1.0, 5.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_kaiser_beta_thresholds() {
        // β = 0 below 21 dB (Kaiser collapses to rectangular).
        assert!((kaiser_beta(10.0)).abs() < f64::EPSILON);
        assert!((kaiser_beta(20.99)).abs() < f64::EPSILON);
        // β > 0 in the 21..50 regime.
        assert!(kaiser_beta(30.0) > 0.0);
        assert!(kaiser_beta(50.0) > 0.0);
        // > 50 dB uses the high-atten formula. Worked example from
        // Kaiser/Schafer: atten=60 → β = 0.1102 · (60 - 8.7) = 5.6533
        assert!((kaiser_beta(60.0) - 5.6533_f64).abs() < 1e-3);
    }

    #[test]
    fn test_kaiser_length_is_odd_and_grows_with_atten() {
        // Length is monotonic in attenuation (higher atten target = more taps).
        // Always odd (Type-I FIR symmetric).
        let n_30 = kaiser_length(30.0, 0.1);
        let n_60 = kaiser_length(60.0, 0.1);
        let n_90 = kaiser_length(90.0, 0.1);
        assert_eq!(n_30 % 2, 1, "expected odd length, got {n_30}");
        assert_eq!(n_60 % 2, 1, "expected odd length, got {n_60}");
        assert_eq!(n_90 % 2, 1, "expected odd length, got {n_90}");
        assert!(n_30 < n_60);
        assert!(n_60 < n_90);
    }

    #[test]
    fn test_kaiser_length_pathological_inputs() {
        // Degenerate values shouldn't panic — clamp to 1 (odd).
        assert_eq!(kaiser_length(-5.0, 0.1) % 2, 1);
        assert_eq!(kaiser_length(8.0, 0.1) % 2, 1); // exactly at the formula's zero
        assert_eq!(kaiser_length(60.0, 0.0001) % 2, 1); // very narrow transition
    }
}
