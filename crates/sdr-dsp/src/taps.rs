//! FIR filter tap (coefficient) generation.
//!
//! Ports SDR++ `dsp::taps` namespace. All functions return `Vec<f32>` tap
//! vectors suitable for use with FIR filter implementations.

use core::f64::consts::PI;

use sdr_types::DspError;

use crate::math;
use crate::window;

/// Tap count estimation scaling factor.
///
/// SDR++ uses 3.8, which gives ~45 dB stopband with Nuttall window.
/// Fred Harris (1978) recommends ~37 for full 93 dB Nuttall stopband.
/// We use 10.0 as a practical compromise: ~70 dB stopband rejection,
/// good enough for SDR channel filtering without excessive CPU cost.
const TAP_COUNT_FACTOR: f64 = 10.0;

/// Tolerance for detecting singular points in RRC formula.
const RRC_SINGULARITY_EPS: f64 = 1e-12;

/// Maximum allowed tap count to prevent unreasonable allocations.
const MAX_TAP_COUNT: usize = 1_000_000;

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
    let count = (TAP_COUNT_FACTOR * sample_rate / transition_width) as usize;
    if count == 0 {
        return Err(DspError::InvalidParameter(
            "transition_width too large for sample_rate — estimated 0 taps".to_string(),
        ));
    }
    if count > MAX_TAP_COUNT {
        return Err(DspError::InvalidParameter(format!(
            "estimated tap count ({count}) exceeds maximum ({MAX_TAP_COUNT})"
        )));
    }
    Ok(count)
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
    let nyquist = sample_rate / 2.0;
    if cutoff >= nyquist {
        return Err(DspError::InvalidParameter(format!(
            "cutoff ({cutoff}) must be less than Nyquist ({nyquist})"
        )));
    }
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
    let nyquist = sample_rate / 2.0;
    if cutoff >= nyquist {
        return Err(DspError::InvalidParameter(format!(
            "cutoff ({cutoff}) must be less than Nyquist ({nyquist})"
        )));
    }
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
    validate_positive_finite(band_stop, "band_stop")?;
    validate_positive_finite(sample_rate, "sample_rate")?;
    let nyquist = sample_rate / 2.0;
    if band_stop >= nyquist {
        return Err(DspError::InvalidParameter(format!(
            "band_stop ({band_stop}) must be less than Nyquist ({nyquist})"
        )));
    }

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
            } else if (t - limit).abs() < RRC_SINGULARITY_EPS
                || (t + limit).abs() < RRC_SINGULARITY_EPS
            {
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

/// Generate a Kaiser-windowed sinc lowpass FIR filter with explicit
/// stopband-attenuation control. Designed using the Kaiser/Schafer
/// (1980) formulas — `β` is derived from `atten_db` via
/// [`window::kaiser_beta`] and tap count from [`window::kaiser_length`].
///
/// Inspired by noaa-apt's `filters::Lowpass::design`. Reimplemented
/// from first principles to match the canonical Kaiser-windowed-sinc
/// derivation (see Oppenheim & Schafer §7.6).
///
/// Use this instead of [`low_pass`] when you need to dial in a
/// specific stopband attenuation (e.g. for APT decoding where we
/// want a known >30 dB rejection of the `2·f_carrier` rectification
/// harmonic). [`low_pass`] uses a Nuttall window with fixed
/// attenuation pattern (~70 dB at our chosen tap-count factor),
/// which is overkill for some applications and not enough for
/// others.
///
/// # Errors
///
/// Returns [`DspError::InvalidParameter`] for non-positive / non-finite
/// inputs or `cutoff >= Nyquist`.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn low_pass_kaiser(
    cutoff: f64,
    transition_width: f64,
    atten_db: f64,
    sample_rate: f64,
) -> Result<Vec<f32>, DspError> {
    validate_positive_finite(cutoff, "cutoff")?;
    validate_positive_finite(transition_width, "transition_width")?;
    validate_positive_finite(atten_db, "atten_db")?;
    validate_positive_finite(sample_rate, "sample_rate")?;
    let nyquist = sample_rate / 2.0;
    if cutoff >= nyquist {
        return Err(DspError::InvalidParameter(format!(
            "cutoff ({cutoff}) must be less than Nyquist ({nyquist})"
        )));
    }
    // Reject designs whose upper transition edge spills past Nyquist
    // — the response above the cutoff folds back into the passband
    // and the filter no longer meets its stated stopband. Per CR
    // round 2 on PR #571.
    let upper_transition_edge = cutoff + transition_width / 2.0;
    if upper_transition_edge >= nyquist {
        return Err(DspError::InvalidParameter(format!(
            "upper transition edge (cutoff + transition_width/2 = {upper_transition_edge}) \
             must be less than Nyquist ({nyquist}) — cutoff={cutoff}, \
             transition_width={transition_width}"
        )));
    }
    let transition_rad = math::hz_to_rads(transition_width, sample_rate);
    let beta = crate::window::kaiser_beta(atten_db);
    let count = crate::window::kaiser_length(atten_db, transition_rad);
    if count > MAX_TAP_COUNT {
        return Err(DspError::InvalidParameter(format!(
            "Kaiser tap count ({count}) exceeds maximum ({MAX_TAP_COUNT})"
        )));
    }
    let omega = math::hz_to_rads(cutoff, sample_rate);
    let half = count as f64 / 2.0;
    let correction = omega / PI;

    // Inline loop instead of `windowed_sinc` because the existing helper
    // calls window_fn(t - half, count) — that argument convention is
    // safe for cosine-family windows (Nuttall etc.) by trig periodicity,
    // but Kaiser is not periodic so it'd produce asymmetric taps. Using
    // `kaiser(i, count, β)` directly with the standard [0..N] argument
    // convention preserves linear-phase symmetry.
    let taps = (0..count)
        .map(|i| {
            let t = i as f64 - half + 0.5;
            let win = crate::window::kaiser(i as f64, count as f64, beta);
            (math::sinc(t * omega) * win * correction) as f32
        })
        .collect();

    Ok(taps)
}

/// Generate a Kaiser-windowed sinc bandpass FIR filter that doubles
/// as a DC-removal stage: passes the band `[transition_width/2, cutoff]`
/// while suppressing both DC (everything below `transition_width/2`)
/// and stopband (everything above `cutoff + transition_width/2`).
///
/// Implemented as the difference of two lowpass filters:
/// `bandpass = lowpass(cutoff) − lowpass(transition_width/2)`.
/// At DC both filters pass equally so the result nulls; in the
/// passband only the wider lowpass passes; above cutoff both are in
/// stopband.
///
/// Inspired by noaa-apt's `filters::LowpassDcRemoval::design`.
/// Reimplemented from the canonical "lowpass-difference" bandpass
/// derivation rather than copied. APT uses this as its resampling
/// filter so DC bias from the FM demod doesn't leak into the AM
/// envelope detector and warp the brightness baseline.
///
/// # Errors
///
/// Returns [`DspError::InvalidParameter`] for invalid parameters
/// (non-positive, non-finite, or `cutoff >= Nyquist`).
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn low_pass_dc_removal_kaiser(
    cutoff: f64,
    transition_width: f64,
    atten_db: f64,
    sample_rate: f64,
) -> Result<Vec<f32>, DspError> {
    validate_positive_finite(cutoff, "cutoff")?;
    validate_positive_finite(transition_width, "transition_width")?;
    validate_positive_finite(atten_db, "atten_db")?;
    validate_positive_finite(sample_rate, "sample_rate")?;
    // The bandpass is `lowpass(cutoff) − lowpass(transition_width/2)`,
    // so we need `transition_width/2 < cutoff` (equivalently
    // `transition_width < 2·cutoff`) for the difference to describe
    // the documented bandpass-with-DC-notch. At or above the
    // boundary, the inner lowpass would land at or past the outer
    // one and the response collapses or inverts. Per CR round 1 on
    // PR #571.
    if transition_width >= 2.0 * cutoff {
        return Err(DspError::InvalidParameter(format!(
            "transition_width ({transition_width}) must be < 2·cutoff ({}) — \
             at or above this the bandpass response collapses/inverts",
            2.0 * cutoff
        )));
    }
    let nyquist = sample_rate / 2.0;
    if cutoff >= nyquist {
        return Err(DspError::InvalidParameter(format!(
            "cutoff ({cutoff}) must be less than Nyquist ({nyquist})"
        )));
    }
    // Reject designs whose upper transition edge spills past Nyquist
    // — the response above the cutoff folds back into the passband
    // and the filter no longer meets its stated stopband. Per CR
    // round 2 on PR #571.
    let upper_transition_edge = cutoff + transition_width / 2.0;
    if upper_transition_edge >= nyquist {
        return Err(DspError::InvalidParameter(format!(
            "upper transition edge (cutoff + transition_width/2 = {upper_transition_edge}) \
             must be less than Nyquist ({nyquist}) — cutoff={cutoff}, \
             transition_width={transition_width}"
        )));
    }
    let transition_rad = math::hz_to_rads(transition_width, sample_rate);
    let beta = crate::window::kaiser_beta(atten_db);
    let count = crate::window::kaiser_length(atten_db, transition_rad);
    if count > MAX_TAP_COUNT {
        return Err(DspError::InvalidParameter(format!(
            "Kaiser tap count ({count}) exceeds maximum ({MAX_TAP_COUNT})"
        )));
    }
    let omega_main = math::hz_to_rads(cutoff, sample_rate);
    let omega_dc = math::hz_to_rads(transition_width / 2.0, sample_rate);
    let half = count as f64 / 2.0;
    let correction_main = omega_main / PI;
    let correction_dc = omega_dc / PI;

    let taps = (0..count)
        .map(|i| {
            let t = i as f64 - half + 0.5;
            let win = crate::window::kaiser(i as f64, count as f64, beta);
            // Lowpass at the main cutoff, with DC-band lowpass
            // subtracted to create the notch at zero. Both share
            // the same Kaiser window for shape consistency.
            let main = math::sinc(t * omega_main) * correction_main;
            let dc = math::sinc(t * omega_dc) * correction_dc;
            ((main - dc) * win) as f32
        })
        .collect();

    Ok(taps)
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
        // TAP_COUNT_FACTOR=10.0: 10 * 48000 / 1000 = 480
        assert_eq!(count, 480);
    }

    #[test]
    fn test_estimate_tap_count_invalid() {
        assert!(estimate_tap_count(0.0, TEST_SAMPLE_RATE).is_err());
        assert!(estimate_tap_count(-1.0, TEST_SAMPLE_RATE).is_err());
        assert!(estimate_tap_count(1_000.0, 0.0).is_err());
        assert!(estimate_tap_count(f64::NAN, TEST_SAMPLE_RATE).is_err());
        assert!(estimate_tap_count(f64::INFINITY, TEST_SAMPLE_RATE).is_err());
        assert!(estimate_tap_count(f64::NEG_INFINITY, TEST_SAMPLE_RATE).is_err());
        // Large transition_width producing zero taps
        assert!(estimate_tap_count(1_000_000.0, 1_000.0).is_err());
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
    fn test_root_raised_cosine_limit_branch() {
        // beta=0.625, symbol_rate=9600, sample_rate=48000 -> ts=5.0, limit=2.0
        // With count=5, half=2.5, tap indices 0..5 give t = -2.0, -1.0, 0.0, 1.0, 2.0
        // t=±2.0 exactly hits the limit-point singular branch
        let taps = root_raised_cosine(5, 0.625, 9600.0, TEST_SAMPLE_RATE).unwrap();
        assert_eq!(taps.len(), 5);
        // Limit-point taps (first and last) should be finite and non-zero
        assert!(
            taps[0].is_finite() && taps[0] != 0.0,
            "limit tap is {}",
            taps[0]
        );
        assert!(
            taps[4].is_finite() && taps[4] != 0.0,
            "limit tap is {}",
            taps[4]
        );
        // Should be symmetric
        assert_symmetric(&taps);
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

    /// FFT magnitude of a real-valued FIR — used to verify a designed
    /// filter's frequency response across passband / transition /
    /// stopband. Returned values are linear amplitudes in `[0, ~1]`.
    fn abs_fft(signal: &[f32]) -> Vec<f64> {
        use rustfft::FftPlanner;
        use rustfft::num_complex::Complex;
        let mut buf: Vec<Complex<f64>> = signal
            .iter()
            .map(|&x| Complex::new(f64::from(x), 0.0))
            .collect();
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(buf.len());
        fft.process(&mut buf);
        buf.iter().map(|c| c.norm()).collect()
    }

    #[test]
    fn test_low_pass_kaiser_basic() {
        // 5 kHz cutoff at 48 kHz sample rate, 1 kHz transition,
        // 40 dB stopband. Validate the filter is symmetric and
        // produces a reasonable tap count.
        let taps = low_pass_kaiser(5_000.0, 1_000.0, 40.0, TEST_SAMPLE_RATE).unwrap();
        assert_symmetric(&taps);
        assert!(
            !taps.is_empty() && taps.len() % 2 == 1,
            "expected odd non-empty length, got {}",
            taps.len()
        );
    }

    #[test]
    fn test_low_pass_kaiser_dc_gain() {
        // DC gain (sum of taps) should be ~1.0 for a properly
        // normalized lowpass filter.
        let taps = low_pass_kaiser(5_000.0, 1_000.0, 40.0, TEST_SAMPLE_RATE).unwrap();
        let sum: f32 = taps.iter().sum();
        assert!(
            (sum - 1.0).abs() < 0.05,
            "DC gain should be ~1.0, got {sum}"
        );
    }

    #[test]
    fn test_low_pass_kaiser_meets_atten_target() {
        // Frequency-domain sanity: the designed filter's stopband
        // should be at least as deep as the requested attenuation
        // (in linear amplitude units, that's `<= 10^(-A/20)`).
        // Sample at fs=12480 Hz with cutoff=4800 Hz, transition=1000 Hz,
        // atten=30 dB — these are the noaa-apt "standard" profile values
        // for the resampling filter, our reference target.
        let fs = 12_480.0;
        let cutoff = 4_800.0;
        let transition = 1_000.0;
        let atten = 30.0;
        let taps = low_pass_kaiser(cutoff, transition, atten, fs).unwrap();
        let response = abs_fft(&taps);

        // Linear stopband level: 10^(-A/20). For 30 dB this is ~0.0316.
        // Allow 2× margin for FFT-bin discretization and the design
        // formula's "approximately at or below A" guarantee.
        let stopband_threshold = 2.0 * 10_f64.powf(-atten / 20.0);

        // Stopband region: above (cutoff + transition/2). Walk the FFT
        // bins covering [stop_start, fs/2] and assert all are below
        // the linear-amplitude threshold.
        let stop_start_hz = cutoff + transition / 2.0;
        let nyquist_hz = fs / 2.0;
        let n = response.len();
        for (i, mag) in response.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let bin_hz = i as f64 * fs / n as f64;
            if bin_hz > stop_start_hz && bin_hz < nyquist_hz {
                assert!(
                    *mag < stopband_threshold,
                    "stopband ripple at {bin_hz:.0} Hz: {mag} > {stopband_threshold}"
                );
            }
        }
    }

    #[test]
    fn test_low_pass_dc_removal_kaiser_nulls_dc() {
        // The bandpass should kill DC (response near 0 at f=0).
        // Standard noaa-apt resampling filter values.
        let fs = 12_480.0;
        let cutoff = 4_800.0;
        let transition = 1_000.0;
        let atten = 30.0;
        let taps = low_pass_dc_removal_kaiser(cutoff, transition, atten, fs).unwrap();
        // Sum of taps = DC gain. Should be near 0 (DC is suppressed).
        let dc_gain: f32 = taps.iter().sum();
        #[allow(
            clippy::cast_possible_truncation,
            reason = "atten is a test fixture in the 30..50 dB range — well \
                      within f32 precision"
        )]
        let dc_threshold = 2.0 * 10_f32.powf(-atten as f32 / 20.0);
        assert!(
            dc_gain.abs() < dc_threshold,
            "DC gain should be ~0, got {dc_gain} (threshold {dc_threshold})"
        );
    }

    #[test]
    fn test_low_pass_dc_removal_kaiser_passes_passband() {
        // Inside the passband (between transition_width/2 and
        // cutoff - transition_width/2) the response should be ~unity.
        let fs = 12_480.0;
        let cutoff = 4_800.0;
        let transition = 1_000.0;
        let atten = 30.0;
        let taps = low_pass_dc_removal_kaiser(cutoff, transition, atten, fs).unwrap();
        let response = abs_fft(&taps);

        let pass_start_hz = transition; // safely past the DC notch
        let pass_end_hz = cutoff - transition / 2.0;
        let n = response.len();
        // Passband ripple tolerance for 30 dB Kaiser: ±0.05 (≈0.4 dB).
        // We're checking with FFT-bin discretization so we allow a bit more.
        let pass_lo = 0.7;
        let pass_hi = 1.3;
        let mut checked = 0;
        for (i, mag) in response.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let bin_hz = i as f64 * fs / n as f64;
            if bin_hz > pass_start_hz && bin_hz < pass_end_hz {
                assert!(
                    *mag > pass_lo && *mag < pass_hi,
                    "passband ripple at {bin_hz:.0} Hz: {mag} not in [{pass_lo}, {pass_hi}]"
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "no FFT bins fell in passband — test invalid");
    }
}
