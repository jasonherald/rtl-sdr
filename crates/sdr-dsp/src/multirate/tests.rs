use super::*;

// --- Polyphase Resampler tests ---

#[test]
fn test_polyphase_new_invalid() {
    assert!(PolyphaseResampler::new(0, 1, &[1.0]).is_err());
    assert!(PolyphaseResampler::new(1, 0, &[1.0]).is_err());
    assert!(PolyphaseResampler::new(1, 1, &[]).is_err());
}

#[test]
fn test_polyphase_passthrough() {
    // interp=1, decim=1 with identity tap -> passthrough
    let mut r = PolyphaseResampler::new(1, 1, &[1.0]).unwrap();
    let input: Vec<Complex> = (0..100).map(|i| Complex::new(i as f32, 0.0)).collect();
    let mut output = vec![Complex::default(); 110];
    let count = r.process(&input, &mut output).unwrap();
    assert_eq!(count, 100);
    for i in 0..100 {
        assert!(
            (output[i].re - i as f32).abs() < 1e-3,
            "passthrough mismatch at {i}"
        );
    }
}

#[test]
fn test_polyphase_upsample_2x() {
    // interp=2, decim=1 -> double the sample rate
    // Use a simple filter: [0.5, 1.0, 0.5] distributed across 2 phases
    let mut r = PolyphaseResampler::new(2, 1, &[0.5, 1.0, 0.5, 0.0]).unwrap();
    let input = vec![Complex::new(1.0, 0.0); 50];
    let mut output = vec![Complex::default(); 110];
    let count = r.process(&input, &mut output).unwrap();
    // Should produce ~100 output samples (2x input)
    assert!(count >= 90 && count <= 110, "expected ~100, got {count}");
}

// --- Power Decimator tests ---

#[test]
fn test_power_decimator_invalid() {
    assert!(PowerDecimator::new(0).is_err());
    assert!(PowerDecimator::new(3).is_err()); // not power of 2
    assert!(PowerDecimator::new(16384).is_err()); // exceeds max
}

#[test]
fn test_power_decimator_passthrough() {
    let mut d = PowerDecimator::new(1).unwrap();
    let input = vec![Complex::new(1.0, 0.0); 100];
    let mut output = vec![Complex::default(); 100];
    let count = d.process(&input, &mut output).unwrap();
    assert_eq!(count, 100);
}

#[test]
fn test_power_decimator_by_2() {
    let mut d = PowerDecimator::new(2).unwrap();
    let input = vec![Complex::new(1.0, 0.0); 100];
    let mut output = vec![Complex::default(); 100];
    let count = d.process(&input, &mut output).unwrap();
    // Should produce ~50 samples
    assert!(count >= 40 && count <= 55, "expected ~50, got {count}");
}

#[test]
fn test_power_decimator_by_4() {
    let mut d = PowerDecimator::new(4).unwrap();
    let input = vec![Complex::new(1.0, 0.0); 200];
    let mut output = vec![Complex::default(); 200];
    let count = d.process(&input, &mut output).unwrap();
    // Should produce ~50 samples
    assert!(count >= 30 && count <= 60, "expected ~50, got {count}");
}

// --- Rational Resampler tests ---

#[test]
fn test_rational_resampler_invalid() {
    assert!(RationalResampler::new(0.0, 48_000.0).is_err());
    assert!(RationalResampler::new(48_000.0, 0.0).is_err());
    assert!(RationalResampler::new(f64::NAN, 48_000.0).is_err());
}

#[test]
fn test_rational_resampler_passthrough() {
    let mut r = RationalResampler::new(48_000.0, 48_000.0).unwrap();
    let input = vec![Complex::new(1.0, 0.0); 100];
    let mut output = vec![Complex::default(); 110];
    let count = r.process(&input, &mut output).unwrap();
    assert_eq!(count, 100);
}

#[test]
fn test_rational_resampler_downsample() {
    // 48kHz -> 8kHz = 6x decimation
    let mut r = RationalResampler::new(48_000.0, 8_000.0).unwrap();
    let input = vec![Complex::new(1.0, 0.0); 600];
    let mut output = vec![Complex::default(); 600];
    let count = r.process(&input, &mut output).unwrap();
    // Should produce ~100 samples (600 / 6)
    assert!(
        count >= 80 && count <= 120,
        "expected ~100 for 6x downsample, got {count}"
    );
}

#[test]
fn test_rational_resampler_upsample() {
    // 8kHz -> 48kHz = 6x interpolation
    let mut r = RationalResampler::new(8_000.0, 48_000.0).unwrap();
    let input = vec![Complex::new(1.0, 0.0); 100];
    let mut output = vec![Complex::default(); 700];
    let count = r.process(&input, &mut output).unwrap();
    // Should produce ~600 samples (100 * 6)
    assert!(
        count >= 500 && count <= 700,
        "expected ~600 for 6x upsample, got {count}"
    );
}

#[test]
fn test_rational_resampler_sub_hz_rejected() {
    assert!(RationalResampler::new(0.5, 48_000.0).is_err());
    assert!(RationalResampler::new(48_000.0, 0.5).is_err());
}

#[test]
#[allow(clippy::cast_possible_truncation)]
fn test_rational_resampler_tone_fidelity() {
    // Generate a 1kHz tone at 48kHz, downsample to 8kHz, verify tone is preserved
    use core::f32::consts::PI;
    let in_rate = 48_000.0_f64;
    let out_rate = 8_000.0_f64;
    let tone_freq = 1_000.0_f32; // 1kHz - well below 4kHz Nyquist of output
    let n_in = 4800; // 100ms of input

    let input: Vec<Complex> = (0..n_in)
        .map(|i| {
            let phase = 2.0 * PI * tone_freq * (i as f32) / (in_rate as f32);
            Complex::new(phase.cos(), 0.0)
        })
        .collect();

    let mut r = RationalResampler::new(in_rate, out_rate).unwrap();
    let mut output = vec![Complex::default(); n_in];
    let count = r.process(&input, &mut output).unwrap();

    // Skip initial transient (first 20% of output), check remaining samples
    let skip = count / 5;
    let steady = &output[skip..count];

    // The output should still contain a sinusoidal signal — check it's not all zeros
    let peak = steady.iter().map(|s| s.re.abs()).fold(0.0_f32, f32::max);
    assert!(peak > 0.3, "tone should be preserved, peak = {peak}");

    // Check the signal has oscillations (not DC) by counting zero crossings
    let crossings = steady
        .windows(2)
        .filter(|w| (w[0].re >= 0.0) != (w[1].re >= 0.0))
        .count();
    // 1kHz at 8kHz rate for ~640 samples -> ~160 cycles -> ~320 crossings
    assert!(
        crossings > 50,
        "expected oscillations, got {crossings} zero crossings"
    );
}

// --- GCD tests ---

#[test]
fn test_gcd() {
    assert_eq!(gcd(48_000, 44_100), 300);
    assert_eq!(gcd(48_000, 8_000), 8_000);
    assert_eq!(gcd(100, 100), 100);
    assert_eq!(gcd(7, 13), 1);
}

/// #774 — callers that align a per-line resample need the filter
/// chain's group delay in input samples; measure it with an impulse.
#[test]
fn rational_resampler_reports_its_group_delay() -> Result<(), DspError> {
    /// Impulse peak vs. reported delay: the pre-decimator + polyphase
    /// cascade rounds each stage's half-length, so allow one input
    /// sample per stage plus one for the output-grid quantisation.
    const DOWNSAMPLE_DELAY_TOLERANCE_SAMPLES: usize = 3;
    const IN_RATE: f64 = 12_480.0;
    const OUT_RATE: f64 = 4_160.0;
    /// `IN_RATE / OUT_RATE`: input samples per output sample, derived
    /// from the rates and checked to be integral so a rate change
    /// cannot leave the measurement with a stale factor.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    const INPUT_SAMPLES_PER_OUTPUT: usize = (IN_RATE / OUT_RATE) as usize;
    #[allow(clippy::float_cmp, clippy::cast_precision_loss)]
    const _: () = assert!(
        OUT_RATE * INPUT_SAMPLES_PER_OUTPUT as f64 == IN_RATE,
        "IN_RATE / OUT_RATE must be integral"
    );
    const IMPULSE_AT: usize = 600;
    const LEN: usize = 3_000;
    let mut r = RationalResampler::new(IN_RATE, OUT_RATE)?;
    let mut input = vec![Complex::default(); LEN];
    input[IMPULSE_AT] = Complex::new(1.0, 0.0);
    let mut output = vec![Complex::default(); LEN];
    let n = r.process(&input, &mut output)?;
    let (peak_idx, _) = output[..n]
        .iter()
        .enumerate()
        .map(|(i, s)| (i, s.re.abs()))
        .fold(
            (0, 0.0_f32),
            |best, cur| if cur.1 > best.1 { cur } else { best },
        );
    let measured = peak_idx * INPUT_SAMPLES_PER_OUTPUT - IMPULSE_AT;
    let reported = r.group_delay_input_samples();
    assert!(
        reported.abs_diff(measured) <= DOWNSAMPLE_DELAY_TOLERANCE_SAMPLES,
        "reported {reported}, measured {measured} (peak at output {peak_idx})"
    );
    assert!(reported > 0);
    Ok(())
}

/// CR round 3 on PR #801 — `PolyphaseBank::build` zero-pads the
/// prototype up to a multiple of `interp`; the appended zeros do
/// not move the impulse response's centre, so the delay must come
/// from the *prototype* length (457 taps → exactly 38 input
/// samples at 6×), not the padded bank (462 → 38.42).
#[test]
fn polyphase_group_delay_uses_the_prototype_length_not_the_padded_bank() -> Result<(), DspError> {
    /// The delay is an exact rational; only float rounding is allowed.
    const DELAY_EPSILON: f64 = 1e-9;
    const INTERP: usize = 6;
    const DECIM: usize = 1;
    const PROTOTYPE_LEN: usize = 457; // not a multiple of INTERP
    let prototype = vec![1.0_f32; PROTOTYPE_LEN];
    let r = PolyphaseResampler::new(INTERP, DECIM, &prototype)?;
    let expected = (PROTOTYPE_LEN - 1) as f64 / 2.0 / INTERP as f64;
    assert!(
        (r.group_delay_input_samples() - expected).abs() < DELAY_EPSILON,
        "got {}, expected {expected}",
        r.group_delay_input_samples()
    );
    Ok(())
}

/// The same check end to end on the 8 kHz → 48 kHz path the
/// transcription resampler uses, measured with an impulse.
#[test]
fn upsampler_group_delay_matches_an_impulse_measurement() -> Result<(), DspError> {
    /// Single polyphase stage: the impulse peak may sit one input
    /// sample off the rounded delay.
    const UPSAMPLE_DELAY_TOLERANCE_SAMPLES: usize = 1;
    const IN_RATE: f64 = 8_000.0;
    const OUT_RATE: f64 = 48_000.0;
    /// `OUT_RATE / IN_RATE`: output samples per input sample.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    const OUTPUTS_PER_INPUT: usize = (OUT_RATE / IN_RATE) as usize;
    const IMPULSE_AT: usize = 200;
    const LEN: usize = 1_000;
    let mut r = RationalResampler::new(IN_RATE, OUT_RATE)?;
    let mut input = vec![Complex::default(); LEN];
    input[IMPULSE_AT] = Complex::new(1.0, 0.0);
    let mut output = vec![Complex::default(); LEN * OUTPUTS_PER_INPUT + 8];
    let n = r.process(&input, &mut output)?;
    let (peak_idx, _) = output[..n]
        .iter()
        .enumerate()
        .map(|(i, s)| (i, s.re.abs()))
        .fold(
            (0, 0.0_f32),
            |best, cur| if cur.1 > best.1 { cur } else { best },
        );
    // Peak lands at (IMPULSE_AT + delay) · OUTPUTS_PER_INPUT.
    let measured = peak_idx / OUTPUTS_PER_INPUT - IMPULSE_AT;
    let reported = r.group_delay_input_samples();
    assert!(
        reported.abs_diff(measured) <= UPSAMPLE_DELAY_TOLERANCE_SAMPLES,
        "reported {reported}, measured {measured} (peak at output {peak_idx})"
    );
    Ok(())
}
