use super::*;

const TEST_SAMPLE_RATE: f64 = 48_000.0;

#[test]
fn test_estimate_tap_count() {
    let count = estimate_tap_count(1_000.0, TEST_SAMPLE_RATE).unwrap();
    // TAP_COUNT_FACTOR=3.8 (matches SDR++): 3.8 * 48000 / 1000 = 182.4 → 182 (truncated)
    assert_eq!(count, 182);
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
    // Zero-pad short FIRs up to a much larger FFT length so the
    // stopband / passband assertions sample the frequency
    // response at fine resolution. Without padding, an N-tap
    // filter is sampled at only N frequencies — for the ~30-tap
    // APT filters that's far too coarse to catch between-bin
    // peaks and the spec checks risk false greens. Per CR
    // round 4 on PR #571.
    const MIN_FFT_LEN: usize = 8_192;
    let fft_len = signal.len().max(MIN_FFT_LEN).next_power_of_two();
    let mut buf: Vec<Complex<f64>> = signal
        .iter()
        .map(|&x| Complex::new(f64::from(x), 0.0))
        .collect();
    buf.resize(fft_len, Complex::new(0.0, 0.0));
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_len);
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

/// Magnitude of the FIR response at `freq_hz` by direct DFT.
#[allow(clippy::cast_precision_loss)]
fn magnitude_at(taps: &[f32], freq_hz: f64, fs: f64) -> f64 {
    let w = core::f64::consts::TAU * freq_hz / fs;
    let (re, im) = taps
        .iter()
        .enumerate()
        .fold((0.0_f64, 0.0_f64), |(re, im), (n, &h)| {
            let phase = w * n as f64;
            (
                re + f64::from(h) * phase.cos(),
                im - f64::from(h) * phase.sin(),
            )
        });
    re.hypot(im)
}

/// #776 — the inner DC lowpass was sized for `transition` but cut
/// at `transition/2`, so its DC gain fell short of the main lowpass
/// and the notch never nulled (residual 0.025 @ 12480, 0.061 @
/// 48 kHz, 0.068 @ 250 kHz). The null must hold at every rate the
/// APT path runs at, not just the 12480 fixture.
#[test]
fn test_low_pass_dc_removal_kaiser_nulls_dc_at_every_apt_rate() {
    const DC_NULL_MAX: f32 = 1e-4;
    for fs in [12_480.0, 48_000.0, 250_000.0] {
        let taps = low_pass_dc_removal_kaiser(4_800.0, 1_000.0, 30.0, fs).unwrap();
        let dc_gain: f32 = taps.iter().sum();
        assert!(
            dc_gain.abs() < DC_NULL_MAX,
            "fs={fs}: DC gain should be ~0, got {dc_gain}"
        );
        let passband_gain = magnitude_at(&taps, 2_400.0, fs);
        assert!(
            (passband_gain - 1.0).abs() < 0.1,
            "fs={fs}: passband gain at 2400 Hz should be ~1, got {passband_gain}"
        );
    }
}

#[test]
fn test_low_pass_dc_removal_kaiser_passes_passband() {
    // Inside the guaranteed-flat passband (approximately
    // `transition_width` to `cutoff - transition_width/2`),
    // the response should be ~unity. The lower edge starts at
    // `transition_width` (not `transition_width/2`) because the
    // inner lowpass's transition is centered at
    // `transition_width/2` — that lower band is still inside
    // the inner filter's transition, not the flat passband.
    // Matches the docstring on `low_pass_dc_removal_kaiser`. Per
    // CR round 6 on PR #571.
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
