//! FIR filter tap (coefficient) generation.
//!
//! Ports SDR++ `dsp::taps` namespace. All functions return `Vec<f32>` tap
//! vectors suitable for use with FIR filter implementations.

use core::f64::consts::PI;

use sdr_types::DspError;

use crate::math;
use crate::window;

/// Tap count estimation scaling factor (from SDR++ `estimateTapCount`).
const TAP_COUNT_FACTOR: f64 = 3.8;

/// Estimate the number of FIR filter taps needed for a given transition width.
///
/// Ports SDR++ `dsp::taps::estimateTapCount`.
/// Formula: `count = 3.8 * sample_rate / transition_width`
///
/// # Errors
///
/// Returns `DspError::InvalidParameter` if `transition_width` or `sample_rate`
/// are non-positive or non-finite.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn estimate_tap_count(transition_width: f64, sample_rate: f64) -> Result<usize, DspError> {
    validate_positive_finite(transition_width, "transition_width")?;
    validate_positive_finite(sample_rate, "sample_rate")?;
    Ok((TAP_COUNT_FACTOR * sample_rate / transition_width) as usize)
}

/// Generate lowpass FIR filter taps using windowed sinc method.
///
/// Ports SDR++ `dsp::taps::lowPass`. Uses Nuttall window.
///
/// # Errors
///
/// Returns `DspError::InvalidParameter` for non-positive or non-finite parameters.
pub fn low_pass(
    cutoff: f64,
    transition_width: f64,
    sample_rate: f64,
    odd_tap_count: bool,
) -> Result<Vec<f32>, DspError> {
    validate_positive_finite(cutoff, "cutoff")?;
    let mut count = estimate_tap_count(transition_width, sample_rate)?;
    if odd_tap_count && count.is_multiple_of(2) {
        count += 1;
    }
    Ok(windowed_sinc(count, cutoff, sample_rate, window::nuttall))
}

/// Generate highpass FIR filter taps using spectral inversion.
///
/// Ports SDR++ `dsp::taps::highPass`. Uses Nuttall window with
/// alternating sign to shift lowpass response to highpass.
/// Always uses an odd tap count (Type I FIR) to avoid a forced zero at Nyquist.
///
/// # Errors
///
/// Returns `DspError::InvalidParameter` for non-positive or non-finite parameters.
pub fn high_pass(
    cutoff: f64,
    transition_width: f64,
    sample_rate: f64,
) -> Result<Vec<f32>, DspError> {
    validate_positive_finite(cutoff, "cutoff")?;
    let mut count = estimate_tap_count(transition_width, sample_rate)?;
    // Always force odd tap count for highpass (Type I FIR avoids Nyquist zero)
    if count.is_multiple_of(2) {
        count += 1;
    }
    Ok(windowed_sinc(
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
    ))
}

/// Generate bandpass FIR filter taps (real-valued).
///
/// Ports the real-valued path of SDR++ `dsp::taps::bandPass`. Uses Nuttall
/// window with cosine modulation to shift a lowpass prototype to the band center.
///
/// # Errors
///
/// Returns `DspError::InvalidParameter` if `band_stop <= band_start` or
/// parameters are non-positive/non-finite.
#[allow(clippy::cast_precision_loss)]
pub fn band_pass(
    band_start: f64,
    band_stop: f64,
    transition_width: f64,
    sample_rate: f64,
    odd_tap_count: bool,
) -> Result<Vec<f32>, DspError> {
    if band_stop <= band_start {
        return Err(DspError::InvalidParameter(
            "band_stop must be greater than band_start".to_string(),
        ));
    }
    validate_positive_finite(band_start, "band_start")?;
    validate_positive_finite(sample_rate, "sample_rate")?;

    let offset_omega = math::hz_to_rads(f64::midpoint(band_start, band_stop), sample_rate);
    let mut count = estimate_tap_count(transition_width, sample_rate)?;
    if odd_tap_count && count.is_multiple_of(2) {
        count += 1;
    }
    let bandwidth = (band_stop - band_start) / 2.0;
    let half = count as f64 / 2.0;

    // Use the centered tap coordinate for modulation (t = i - half + 0.5),
    // not the window coordinate passed to the closure.
    let omega = math::hz_to_rads(bandwidth, sample_rate);
    let correction = omega / PI;

    #[allow(clippy::cast_possible_truncation)]
    let taps = (0..count)
        .map(|i| {
            let t = i as f64 - half + 0.5;
            let sinc_val = math::sinc(t * omega);
            let win = window::nuttall(t - half, count as f64);
            let modulation = 2.0 * (offset_omega * t).cos();
            (sinc_val * win * correction * modulation) as f32
        })
        .collect();

    Ok(taps)
}

/// Generate root raised cosine filter taps.
///
/// Ports SDR++ `dsp::taps::rootRaisedCosine`.
///
/// # Errors
///
/// Returns `DspError::InvalidParameter` if `count` is 0, `beta` is not in
/// `0.0..=1.0`, or rates are non-positive/non-finite.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn root_raised_cosine(
    count: usize,
    beta: f64,
    symbol_rate: f64,
    sample_rate: f64,
) -> Result<Vec<f32>, DspError> {
    if count == 0 {
        return Err(DspError::InvalidParameter("count must be > 0".to_string()));
    }
    if !(0.0..=1.0).contains(&beta) {
        return Err(DspError::InvalidParameter(format!(
            "beta must be in 0.0..=1.0, got {beta}"
        )));
    }
    validate_positive_finite(symbol_rate, "symbol_rate")?;
    validate_positive_finite(sample_rate, "sample_rate")?;

    let ts = sample_rate / symbol_rate;
    let half = count as f64 / 2.0;
    let limit = ts / (4.0 * beta);

    let taps = (0..count)
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
        .collect();

    Ok(taps)
}

/// Core windowed sinc tap generator.
///
/// Generates taps using:
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

/// Validate that a parameter is positive and finite.
fn validate_positive_finite(value: f64, name: &str) -> Result<(), DspError> {
    if !value.is_finite() || value <= 0.0 {
        return Err(DspError::InvalidParameter(format!(
            "{name} must be positive and finite, got {value}"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::unwrap_used)]
mod tests {
    use super::*;

    const TEST_SAMPLE_RATE: f64 = 48_000.0;

    #[test]
    fn test_estimate_tap_count() {
        let count = estimate_tap_count(1_000.0, TEST_SAMPLE_RATE).unwrap();
        assert_eq!(count, 182);
    }

    #[test]
    fn test_estimate_tap_count_invalid() {
        assert!(estimate_tap_count(0.0, TEST_SAMPLE_RATE).is_err());
        assert!(estimate_tap_count(-1.0, TEST_SAMPLE_RATE).is_err());
        assert!(estimate_tap_count(1_000.0, 0.0).is_err());
        assert!(estimate_tap_count(f64::NAN, TEST_SAMPLE_RATE).is_err());
        assert!(estimate_tap_count(f64::INFINITY, TEST_SAMPLE_RATE).is_err());
    }

    #[test]
    fn test_low_pass_basic() {
        let taps = low_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE, false).unwrap();
        assert!(!taps.is_empty());
        assert_symmetric(&taps);
    }

    #[test]
    fn test_low_pass_odd_tap_count() {
        let taps = low_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE, true).unwrap();
        assert!(taps.len() % 2 == 1, "expected odd, got {}", taps.len());
    }

    #[test]
    fn test_low_pass_dc_gain() {
        let taps = low_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE, false).unwrap();
        let sum: f32 = taps.iter().sum();
        assert!((sum - 1.0).abs() < 0.1, "DC gain should be ~1.0, got {sum}");
    }

    #[test]
    fn test_high_pass_basic() {
        let taps = high_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE).unwrap();
        assert!(!taps.is_empty());
        // Always odd tap count
        assert!(taps.len() % 2 == 1, "expected odd, got {}", taps.len());
        // DC gain ~0
        let sum: f32 = taps.iter().sum();
        assert!(sum.abs() < 0.1, "HP DC gain should be ~0, got {sum}");
    }

    #[test]
    fn test_high_pass_symmetry() {
        let taps = high_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE).unwrap();
        assert_symmetric(&taps);
    }

    #[test]
    fn test_band_pass_basic() {
        let taps = band_pass(5_000.0, 10_000.0, 1_000.0, TEST_SAMPLE_RATE, false).unwrap();
        assert!(!taps.is_empty());
        let sum: f32 = taps.iter().sum();
        assert!(sum.abs() < 0.2, "BP DC gain should be ~0, got {sum}");
    }

    #[test]
    fn test_band_pass_symmetry() {
        let taps = band_pass(5_000.0, 10_000.0, 1_000.0, TEST_SAMPLE_RATE, false).unwrap();
        assert_symmetric(&taps);
    }

    #[test]
    fn test_band_pass_invalid_range() {
        let result = band_pass(10_000.0, 5_000.0, 1_000.0, TEST_SAMPLE_RATE, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_root_raised_cosine_basic() {
        let taps = root_raised_cosine(65, 0.35, 9600.0, TEST_SAMPLE_RATE).unwrap();
        assert_eq!(taps.len(), 65);
        assert_symmetric(&taps);
    }

    #[test]
    fn test_root_raised_cosine_peak() {
        let taps = root_raised_cosine(65, 0.35, 9600.0, TEST_SAMPLE_RATE).unwrap();
        let center = taps.len() / 2;
        let peak = taps
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map_or(0, |(i, _)| i);
        assert_eq!(peak, center, "peak should be at center tap");
    }

    #[test]
    fn test_root_raised_cosine_invalid() {
        assert!(root_raised_cosine(0, 0.35, 9600.0, TEST_SAMPLE_RATE).is_err());
        assert!(root_raised_cosine(65, 1.5, 9600.0, TEST_SAMPLE_RATE).is_err());
        assert!(root_raised_cosine(65, -0.1, 9600.0, TEST_SAMPLE_RATE).is_err());
        assert!(root_raised_cosine(65, 0.35, 0.0, TEST_SAMPLE_RATE).is_err());
    }

    #[test]
    fn test_windowed_sinc_not_all_zero() {
        let taps = low_pass(5_000.0, 1_000.0, TEST_SAMPLE_RATE, false).unwrap();
        let any_nonzero = taps.iter().any(|&t| t != 0.0);
        assert!(any_nonzero, "taps should not all be zero");
    }

    /// Assert that taps are symmetric (linear phase FIR).
    fn assert_symmetric(taps: &[f32]) {
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
}
