//! FIR filter tap (coefficient) generation.
//!
//! Ports SDR++ `dsp::taps` namespace. All functions return `Vec<f32>` tap
//! vectors suitable for use with FIR filter implementations.

use core::f64::consts::PI;

use crate::math;
use crate::window;

/// Tap count estimation scaling factor (from SDR++ `estimateTapCount`).
const TAP_COUNT_FACTOR: f64 = 3.8;

/// Estimate the number of FIR filter taps needed for a given transition width.
///
/// Ports SDR++ `dsp::taps::estimateTapCount`.
/// Formula: `count = 3.8 * sample_rate / transition_width`
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn estimate_tap_count(transition_width: f64, sample_rate: f64) -> usize {
    (TAP_COUNT_FACTOR * sample_rate / transition_width) as usize
}

/// Generate lowpass FIR filter taps using windowed sinc method.
///
/// Ports SDR++ `dsp::taps::lowPass`. Uses Nuttall window.
///
/// - `cutoff`: cutoff frequency in Hz
/// - `transition_width`: transition bandwidth in Hz
/// - `sample_rate`: sample rate in Hz
/// - `odd_tap_count`: if true, ensures an odd number of taps
pub fn low_pass(
    cutoff: f64,
    transition_width: f64,
    sample_rate: f64,
    odd_tap_count: bool,
) -> Vec<f32> {
    let mut count = estimate_tap_count(transition_width, sample_rate);
    if odd_tap_count && count.is_multiple_of(2) {
        count += 1;
    }
    windowed_sinc(count, cutoff, sample_rate, window::nuttall)
}

/// Generate highpass FIR filter taps using spectral inversion.
///
/// Ports SDR++ `dsp::taps::highPass`. Uses Nuttall window with
/// alternating sign to shift lowpass response to highpass.
///
/// - `cutoff`: cutoff frequency in Hz
/// - `transition_width`: transition bandwidth in Hz
/// - `sample_rate`: sample rate in Hz
/// - `odd_tap_count`: if true, ensures an odd number of taps
pub fn high_pass(
    cutoff: f64,
    transition_width: f64,
    sample_rate: f64,
    odd_tap_count: bool,
) -> Vec<f32> {
    let mut count = estimate_tap_count(transition_width, sample_rate);
    if odd_tap_count && count.is_multiple_of(2) {
        count += 1;
    }
    // Highpass = lowpass at (Nyquist - cutoff) with alternating sign (spectral inversion)
    windowed_sinc(
        count,
        (sample_rate / 2.0) - cutoff,
        sample_rate,
        |n, big_n| {
            #[allow(clippy::cast_possible_truncation)]
            let sign = if (n.round() as i64) % 2 != 0 {
                -1.0
            } else {
                1.0
            };
            window::nuttall(n, big_n) * sign
        },
    )
}

/// Generate bandpass FIR filter taps (real-valued).
///
/// Ports the real-valued path of SDR++ `dsp::taps::bandPass`. Uses Nuttall
/// window with cosine modulation to shift a lowpass prototype to the band center.
///
/// - `band_start`: lower edge frequency in Hz
/// - `band_stop`: upper edge frequency in Hz
/// - `transition_width`: transition bandwidth in Hz
/// - `sample_rate`: sample rate in Hz
/// - `odd_tap_count`: if true, ensures an odd number of taps
pub fn band_pass(
    band_start: f64,
    band_stop: f64,
    transition_width: f64,
    sample_rate: f64,
    odd_tap_count: bool,
) -> Vec<f32> {
    assert!(
        band_stop > band_start,
        "band_stop must be greater than band_start"
    );
    let offset_omega = math::hz_to_rads(f64::midpoint(band_start, band_stop), sample_rate);
    let mut count = estimate_tap_count(transition_width, sample_rate);
    if odd_tap_count && count.is_multiple_of(2) {
        count += 1;
    }
    let bandwidth = (band_stop - band_start) / 2.0;
    windowed_sinc(count, bandwidth, sample_rate, |n, big_n| {
        #[allow(clippy::cast_possible_truncation)]
        let modulation = 2.0 * (offset_omega * n).cos();
        modulation * window::nuttall(n, big_n)
    })
}

/// Generate root raised cosine filter taps.
///
/// Ports SDR++ `dsp::taps::rootRaisedCosine`.
///
/// - `count`: number of taps
/// - `beta`: roll-off factor (0.0 to 1.0)
/// - `symbol_rate`: symbol rate in symbols/second
/// - `sample_rate`: sample rate in Hz
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn root_raised_cosine(count: usize, beta: f64, symbol_rate: f64, sample_rate: f64) -> Vec<f32> {
    let ts = sample_rate / symbol_rate;
    let half = count as f64 / 2.0;
    let limit = ts / (4.0 * beta);

    (0..count)
        .map(|i| {
            let t = i as f64 - half + 0.5;
            let val = if t == 0.0 {
                (1.0 + beta * (4.0 / PI - 1.0)) / ts
            } else if (t - limit).abs() < 1e-12 || (t + limit).abs() < 1e-12 {
                let sin_term = (PI / (4.0 * beta)).sin();
                let cos_term = (PI / (4.0 * beta)).cos();
                ((1.0 + 2.0 / PI) * sin_term + (1.0 - 2.0 / PI) * cos_term) * beta
                    / (ts * core::f64::consts::SQRT_2)
            } else {
                let num = ((1.0 - beta) * PI * t / ts).sin()
                    + ((1.0 + beta) * PI * t / ts).cos() * 4.0 * beta * t / ts;
                let den = (1.0 - (4.0 * beta * t / ts).powi(2)) * PI * t / ts;
                (num / den) / ts
            };
            val as f32
        })
        .collect()
}

/// Core windowed sinc tap generator.
///
/// Ports SDR++ `dsp::taps::windowedSinc`. Generates taps using:
/// `tap[i] = sinc(t * omega) * window(t - half, count) * correction`
/// where `correction = omega / pi` normalizes the DC gain.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn windowed_sinc<F>(count: usize, cutoff: f64, sample_rate: f64, window_fn: F) -> Vec<f32>
where
    F: Fn(f64, f64) -> f64,
{
    let omega = math::hz_to_rads(cutoff, sample_rate);
    let half = count as f64 / 2.0;
    let correction = omega / PI;

    (0..count)
        .map(|i| {
            let t = i as f64 - half + 0.5;
            let val = math::sinc(t * omega) * window_fn(t - half, count as f64) * correction;
            val as f32
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    const TEST_SAMPLE_RATE: f64 = 48_000.0;

    #[test]
    fn test_estimate_tap_count() {
        // 1kHz transition at 48kHz sample rate -> ~182 taps
        let count = estimate_tap_count(1_000.0, TEST_SAMPLE_RATE);
        assert_eq!(count, 182);
    }

    #[test]
    fn test_low_pass_basic() {
        let taps = low_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE, false);
        assert!(!taps.is_empty());
        // Taps should be symmetric (linear phase)
        let n = taps.len();
        for i in 0..n / 2 {
            assert!(
                (taps[i] - taps[n - 1 - i]).abs() < 1e-6,
                "symmetry failed at {i}: {} != {}",
                taps[i],
                taps[n - 1 - i]
            );
        }
    }

    #[test]
    fn test_low_pass_odd_tap_count() {
        let taps = low_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE, true);
        assert!(
            taps.len() % 2 == 1,
            "expected odd tap count, got {}",
            taps.len()
        );
    }

    #[test]
    fn test_low_pass_dc_gain() {
        // Sum of lowpass taps should approximate 1.0 (unity DC gain)
        let taps = low_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE, false);
        let sum: f32 = taps.iter().sum();
        assert!((sum - 1.0).abs() < 0.1, "DC gain should be ~1.0, got {sum}");
    }

    #[test]
    fn test_high_pass_basic() {
        let taps = high_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE, false);
        assert!(!taps.is_empty());
        // Highpass taps sum should be near zero (rejects DC)
        let sum: f32 = taps.iter().sum();
        assert!(sum.abs() < 0.1, "HP DC gain should be ~0, got {sum}");
    }

    #[test]
    fn test_high_pass_odd_tap_count() {
        let taps = high_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE, true);
        assert!(
            taps.len() % 2 == 1,
            "expected odd tap count, got {}",
            taps.len()
        );
    }

    #[test]
    fn test_band_pass_basic() {
        let taps = band_pass(5_000.0, 10_000.0, 1_000.0, TEST_SAMPLE_RATE, false);
        assert!(!taps.is_empty());
        // Bandpass taps sum should be near zero (rejects DC)
        let sum: f32 = taps.iter().sum();
        assert!(sum.abs() < 0.2, "BP DC gain should be ~0, got {sum}");
    }

    #[test]
    #[should_panic(expected = "band_stop must be greater than band_start")]
    fn test_band_pass_invalid_range() {
        band_pass(10_000.0, 5_000.0, 1_000.0, TEST_SAMPLE_RATE, false);
    }

    #[test]
    fn test_root_raised_cosine_basic() {
        let taps = root_raised_cosine(65, 0.35, 9600.0, TEST_SAMPLE_RATE);
        assert_eq!(taps.len(), 65);
        // RRC should be symmetric
        let n = taps.len();
        for i in 0..n / 2 {
            assert!(
                (taps[i] - taps[n - 1 - i]).abs() < 1e-6,
                "RRC symmetry failed at {i}: {} != {}",
                taps[i],
                taps[n - 1 - i]
            );
        }
    }

    #[test]
    fn test_root_raised_cosine_peak() {
        // Peak should be at center tap
        let taps = root_raised_cosine(65, 0.35, 9600.0, TEST_SAMPLE_RATE);
        let center = taps.len() / 2;
        let peak = taps
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map_or(0, |(i, _)| i);
        assert_eq!(peak, center, "peak should be at center tap");
    }

    #[test]
    fn test_windowed_sinc_not_all_zero() {
        let taps = low_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE, false);
        let any_nonzero = taps.iter().any(|&t| t != 0.0);
        assert!(any_nonzero, "taps should not all be zero");
    }
}
