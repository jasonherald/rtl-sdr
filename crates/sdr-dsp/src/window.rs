//! Window functions for spectral analysis and FIR filter design.
//!
//! Ports SDR++ `dsp::window` namespace. All functions operate on a single
//! sample index `n` within a window of length `N`.

use core::f64::consts::PI;

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
    cosine(n, big_n, &[0.5, 0.5])
}

/// Hamming window.
#[inline]
pub fn hamming(n: f64, big_n: f64) -> f64 {
    cosine(n, big_n, &[0.54, 0.46])
}

/// Blackman window.
#[inline]
pub fn blackman(n: f64, big_n: f64) -> f64 {
    cosine(n, big_n, &[0.42, 0.5, 0.08])
}

/// Blackman-Harris window (4-term).
#[inline]
#[allow(clippy::unreadable_literal)]
pub fn blackman_harris(n: f64, big_n: f64) -> f64 {
    cosine(n, big_n, &[0.35875, 0.48829, 0.14128, 0.01168])
}

/// Blackman-Nuttall window (4-term).
#[inline]
#[allow(clippy::unreadable_literal)]
pub fn blackman_nuttall(n: f64, big_n: f64) -> f64 {
    cosine(n, big_n, &[0.3635819, 0.4891775, 0.1365995, 0.0106411])
}

/// Nuttall window (4-term, continuous first derivative).
#[inline]
#[allow(clippy::unreadable_literal)]
pub fn nuttall(n: f64, big_n: f64) -> f64 {
    cosine(n, big_n, &[0.355768, 0.487396, 0.144232, 0.012604])
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
}
