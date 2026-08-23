use super::*;

// --- FIR filter tests ---

#[test]
fn test_fir_new_empty_taps() {
    assert!(FirFilter::new(vec![]).is_err());
}

#[test]
fn test_fir_identity() {
    // Single tap of 1.0 = identity filter (passthrough)
    let mut fir = FirFilter::new(vec![1.0]).unwrap();
    let input = [1.0, 2.0, 3.0, 4.0, 5.0];
    let mut output = [0.0_f32; 5];
    fir.process_f32(&input, &mut output).unwrap();
    for i in 0..5 {
        assert!(
            (output[i] - input[i]).abs() < 1e-6,
            "identity filter: output[{i}] = {}, expected {}",
            output[i],
            input[i]
        );
    }
}

#[test]
fn test_fir_delay() {
    // Tap at position 1 = one-sample delay: [0, 1]
    let mut fir = FirFilter::new(vec![0.0, 1.0]).unwrap();
    let input = [1.0, 2.0, 3.0, 4.0];
    let mut output = [0.0_f32; 4];
    fir.process_f32(&input, &mut output).unwrap();
    // Output should be delayed by one sample (first output = 0 from delay line)
    assert!((output[0] - 0.0).abs() < 1e-6);
    assert!((output[1] - 1.0).abs() < 1e-6);
    assert!((output[2] - 2.0).abs() < 1e-6);
    assert!((output[3] - 3.0).abs() < 1e-6);
}

#[test]
fn test_fir_averaging() {
    // 3-tap averaging filter [1/3, 1/3, 1/3]
    let mut fir = FirFilter::new(vec![1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0]).unwrap();
    let input = [3.0, 3.0, 3.0, 3.0];
    let mut output = [0.0_f32; 4];
    fir.process_f32(&input, &mut output).unwrap();
    // After the filter fills, output should be 3.0
    assert!((output[2] - 3.0).abs() < 1e-5);
    assert!((output[3] - 3.0).abs() < 1e-5);
}

#[test]
fn test_fir_continuity_across_blocks() {
    // Process in two blocks, verify continuity via delay line
    let mut fir = FirFilter::new(vec![0.0, 1.0]).unwrap();
    let block1 = [1.0, 2.0];
    let block2 = [3.0, 4.0];
    let mut out1 = [0.0_f32; 2];
    let mut out2 = [0.0_f32; 2];
    fir.process_f32(&block1, &mut out1).unwrap();
    fir.process_f32(&block2, &mut out2).unwrap();
    // out2[0] should be last sample of block1 (delayed)
    assert!((out2[0] - 2.0).abs() < 1e-6);
    assert!((out2[1] - 3.0).abs() < 1e-6);
}

#[test]
fn test_complex_fir_identity() {
    let mut fir = ComplexFirFilter::new(vec![1.0]).unwrap();
    let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
    let mut output = [Complex::default(); 2];
    fir.process(&input, &mut output).unwrap();
    assert!((output[0].re - 1.0).abs() < 1e-6);
    assert!((output[0].im - 2.0).abs() < 1e-6);
    assert!((output[1].re - 3.0).abs() < 1e-6);
    assert!((output[1].im - 4.0).abs() < 1e-6);
}

#[test]
fn test_complex_fir_delay() {
    let mut fir = ComplexFirFilter::new(vec![0.0, 1.0]).unwrap();
    let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
    let mut output = [Complex::default(); 2];
    fir.process(&input, &mut output).unwrap();
    assert!((output[0].re).abs() < 1e-6);
    assert!((output[0].im).abs() < 1e-6);
    assert!((output[1].re - 1.0).abs() < 1e-6);
    assert!((output[1].im - 2.0).abs() < 1e-6);
}

#[test]
fn test_complex_fir_set_taps() {
    let mut fir = ComplexFirFilter::new(vec![1.0]).unwrap();
    // Identity -> 2-tap delay filter
    fir.set_taps(vec![0.0, 1.0]).unwrap();
    assert_eq!(fir.tap_count(), 2);
    let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
    let mut output = [Complex::default(); 2];
    fir.process(&input, &mut output).unwrap();
    // First output uses delay line (zero-extended since old had 0 delay taps)
    assert!((output[0].re).abs() < 1e-6);
    assert!((output[1].re - 1.0).abs() < 1e-6);
}

#[test]
fn test_fir_set_taps_preserves_delay() {
    // Process a block so delay line has data, then swap taps
    let mut fir = FirFilter::new(vec![0.0, 1.0]).unwrap();
    let input = [10.0, 20.0, 30.0];
    let mut output = [0.0_f32; 3];
    fir.process_f32(&input, &mut output).unwrap();
    // delay line now holds [30.0]

    // Swap to a 3-tap filter — delay line should keep 30.0
    fir.set_taps(vec![0.0, 0.0, 1.0]).unwrap();
    let input2 = [40.0, 50.0];
    let mut output2 = [0.0_f32; 2];
    fir.process_f32(&input2, &mut output2).unwrap();
    // output2[0] should be delay[0] = 0.0 (zero-extended), output2[1] = 30.0 (preserved)
    assert!(
        (output2[0]).abs() < 1e-6,
        "zero-extended position, got {}",
        output2[0]
    );
    assert!(
        (output2[1] - 30.0).abs() < 1e-6,
        "preserved delay sample, got {}",
        output2[1]
    );
}

#[test]
fn test_fir_set_taps_same_length() {
    // Same length swap should keep delay line intact
    let mut fir = FirFilter::new(vec![0.0, 1.0]).unwrap();
    let input = [5.0, 10.0];
    let mut output = [0.0_f32; 2];
    fir.process_f32(&input, &mut output).unwrap();
    // delay line = [10.0]

    // Swap to identity (same 2-tap count) — delay stays [10.0]
    fir.set_taps(vec![1.0, 0.0]).unwrap();
    let input2 = [20.0];
    let mut output2 = [0.0_f32; 1];
    fir.process_f32(&input2, &mut output2).unwrap();
    // With taps [1.0, 0.0]: output = 1.0*20.0 + 0.0*10.0 = 20.0
    assert!(
        (output2[0] - 20.0).abs() < 1e-6,
        "same-length tap swap, got {}",
        output2[0]
    );
}

#[test]
fn test_complex_fir_set_taps_empty() {
    let mut fir = ComplexFirFilter::new(vec![1.0]).unwrap();
    assert!(fir.set_taps(vec![]).is_err());
}

#[test]
fn test_fir_buffer_too_small() {
    let mut fir = FirFilter::new(vec![1.0]).unwrap();
    let input = [1.0, 2.0, 3.0];
    let mut output = [0.0_f32; 2];
    assert!(fir.process_f32(&input, &mut output).is_err());
}

// --- Decimating FIR tests ---

#[test]
fn test_decimating_fir_new_invalid() {
    assert!(DecimatingFirFilter::new(vec![], 2).is_err());
    assert!(DecimatingFirFilter::new(vec![1.0], 0).is_err());
}

#[test]
fn test_decimating_fir_by_2() {
    // Identity taps, decimate by 2
    let mut fir = DecimatingFirFilter::new(vec![1.0], 2).unwrap();
    let input = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
    let mut output = [0.0_f32; 3];
    let count = fir.process_f32(&input, &mut output).unwrap();
    assert_eq!(count, 3);
    assert!((output[0] - 1.0).abs() < 1e-6);
    assert!((output[1] - 3.0).abs() < 1e-6);
    assert!((output[2] - 5.0).abs() < 1e-6);
}

#[test]
fn test_decimating_fir_by_4() {
    let mut fir = DecimatingFirFilter::new(vec![1.0], 4).unwrap();
    let input = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
    let mut output = [0.0_f32; 2];
    let count = fir.process_f32(&input, &mut output).unwrap();
    assert_eq!(count, 2);
    assert!((output[0] - 1.0).abs() < 1e-6);
    assert!((output[1] - 5.0).abs() < 1e-6);
}

// --- Deemphasis filter tests ---

#[test]
fn test_deemphasis_new_invalid() {
    assert!(DeemphasisFilter::new(0.0, 48_000.0).is_err());
    assert!(DeemphasisFilter::new(-1.0, 48_000.0).is_err());
    assert!(DeemphasisFilter::new(75e-6, 0.0).is_err());
    assert!(DeemphasisFilter::new(f64::NAN, 48_000.0).is_err());
}

#[test]
fn test_deemphasis_dc_passthrough() {
    // DC signal should pass through unchanged (IIR converges to input)
    let mut deemph = DeemphasisFilter::new(DEEMPHASIS_TAU_US, 48_000.0).unwrap();
    let input = vec![1.0_f32; 1000];
    let mut output = vec![0.0_f32; 1000];
    deemph.process(&input, &mut output).unwrap();
    // After settling, output should approach 1.0
    assert!(
        (output[999] - 1.0).abs() < 0.01,
        "DC should converge to 1.0, got {}",
        output[999]
    );
}

#[test]
fn test_deemphasis_high_freq_attenuation() {
    // High frequency signal should be attenuated
    let mut deemph = DeemphasisFilter::new(DEEMPHASIS_TAU_US, 48_000.0).unwrap();
    let input: Vec<f32> = (0..1000)
        .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
        .collect();
    let mut output = vec![0.0_f32; 1000];
    deemph.process(&input, &mut output).unwrap();
    // Peak output should be much less than peak input
    let peak_out = output[500..]
        .iter()
        .map(|x| x.abs())
        .fold(0.0_f32, f32::max);
    assert!(
        peak_out < 0.5,
        "high freq should be attenuated, peak = {peak_out}"
    );
}

#[test]
fn test_deemphasis_reset() {
    let mut deemph = DeemphasisFilter::new(DEEMPHASIS_TAU_US, 48_000.0).unwrap();
    let input = [1.0_f32; 100];
    let mut output = [0.0_f32; 100];
    deemph.process(&input, &mut output).unwrap();
    deemph.reset();
    // After reset, processing zeros should give zeros
    let zeros = [0.0_f32; 10];
    let mut out2 = [0.0_f32; 10];
    deemph.process(&zeros, &mut out2).unwrap();
    assert!((out2[0]).abs() < 1e-6, "after reset, output should be 0");
}

#[test]
fn test_deemphasis_buffer_too_small() {
    let mut deemph = DeemphasisFilter::new(DEEMPHASIS_TAU_US, 48_000.0).unwrap();
    let input = [1.0_f32; 10];
    let mut output = [0.0_f32; 5];
    assert!(deemph.process(&input, &mut output).is_err());
}

// --- Notch filter tests ---

#[test]
fn test_notch_new_defaults() {
    let notch = NotchFilter::new(48_000.0);
    assert!(!notch.enabled());
    assert!((notch.frequency() - 60.0).abs() < f32::EPSILON);
}

#[test]
fn test_notch_disabled_passthrough() {
    let mut notch = NotchFilter::new(48_000.0);
    // Filter is disabled by default
    let input = [1.0, 2.0, 3.0, 4.0, 5.0];
    let mut output = [0.0_f32; 5];
    let count = notch.process(&input, &mut output).unwrap();
    assert_eq!(count, 5);
    for i in 0..5 {
        assert!(
            (output[i] - input[i]).abs() < 1e-6,
            "disabled notch should passthrough: output[{i}] = {}, expected {}",
            output[i],
            input[i]
        );
    }
}

#[test]
fn test_notch_buffer_too_small() {
    let mut notch = NotchFilter::new(48_000.0);
    notch.set_enabled(true);
    let input = [1.0_f32; 10];
    let mut output = [0.0_f32; 5];
    assert!(notch.process(&input, &mut output).is_err());
}

#[test]
fn test_notch_coefficients_symmetry() {
    // For a notch filter, b0 == b2 and b1 == a1 (before normalization).
    // After normalization by a0: b0 = 1/a0, b2 = 1/a0, b1 = -2cos(w0)/a0, a1 = -2cos(w0)/a0.
    let notch = NotchFilter::new(48_000.0);
    assert!(
        (notch.b0 - notch.b2).abs() < 1e-6,
        "b0 ({}) should equal b2 ({})",
        notch.b0,
        notch.b2
    );
    assert!(
        (notch.b1 - notch.a1).abs() < 1e-6,
        "b1 ({}) should equal a1 ({})",
        notch.b1,
        notch.a1
    );
}

#[test]
fn test_notch_attenuates_target_frequency() {
    // Generate a 60 Hz sine wave at 48 kHz sample rate.
    let sample_rate = 48_000.0_f32;
    let freq = 60.0_f32;
    let num_samples = 48_000; // 1 second of audio
    let input: Vec<f32> = (0..num_samples)
        .map(|i| (core::f32::consts::TAU * freq * (i as f32) / sample_rate).sin())
        .collect();
    let mut output = vec![0.0_f32; num_samples];

    let mut notch = NotchFilter::new(sample_rate);
    notch.set_frequency(freq);
    notch.set_enabled(true);
    notch.process(&input, &mut output).unwrap();

    // Measure RMS of the last half (after settling)
    let rms_in: f32 = (input[num_samples / 2..].iter().map(|x| x * x).sum::<f32>()
        / (num_samples / 2) as f32)
        .sqrt();
    let rms_out: f32 = (output[num_samples / 2..].iter().map(|x| x * x).sum::<f32>()
        / (num_samples / 2) as f32)
        .sqrt();

    // The notch should attenuate the target frequency by at least 25 dB.
    // At Q=30, 60 Hz, 48 kHz sample rate, the biquad achieves ~32 dB rejection.
    let attenuation_db = 20.0 * (rms_out / rms_in).log10();
    assert!(
        attenuation_db < -25.0,
        "60 Hz should be attenuated by >25 dB, got {attenuation_db:.1} dB"
    );
}

#[test]
fn test_notch_passes_other_frequencies() {
    // Generate a 1000 Hz sine wave — should NOT be attenuated by a 60 Hz notch.
    let sample_rate = 48_000.0_f32;
    let freq = 1000.0_f32;
    let num_samples = 48_000;
    let input: Vec<f32> = (0..num_samples)
        .map(|i| (core::f32::consts::TAU * freq * (i as f32) / sample_rate).sin())
        .collect();
    let mut output = vec![0.0_f32; num_samples];

    let mut notch = NotchFilter::new(sample_rate);
    notch.set_frequency(60.0);
    notch.set_enabled(true);
    notch.process(&input, &mut output).unwrap();

    // Measure RMS of the last half
    let rms_in: f32 = (input[num_samples / 2..].iter().map(|x| x * x).sum::<f32>()
        / (num_samples / 2) as f32)
        .sqrt();
    let rms_out: f32 = (output[num_samples / 2..].iter().map(|x| x * x).sum::<f32>()
        / (num_samples / 2) as f32)
        .sqrt();

    // 1000 Hz should pass through with minimal attenuation (< 1 dB)
    let attenuation_db = 20.0 * (rms_out / rms_in).log10();
    assert!(
        attenuation_db > -1.0,
        "1000 Hz should pass with < 1 dB loss, got {attenuation_db:.2} dB"
    );
}

#[test]
fn test_notch_reset_clears_state() {
    let mut notch = NotchFilter::new(48_000.0);
    notch.set_enabled(true);
    // Process some data to build up state
    let input = [1.0_f32; 100];
    let mut output = [0.0_f32; 100];
    notch.process(&input, &mut output).unwrap();

    notch.reset();
    // After reset, processing zeros should produce zeros
    let zeros = [0.0_f32; 10];
    let mut out2 = [0.0_f32; 10];
    notch.process(&zeros, &mut out2).unwrap();
    for (i, &v) in out2.iter().enumerate() {
        assert!(
            v.abs() < 1e-6,
            "after reset, output[{i}] should be ~0, got {v}"
        );
    }
}

#[test]
fn test_notch_set_frequency_updates_coefficients() {
    let mut notch = NotchFilter::new(48_000.0);
    let old_b1 = notch.b1;
    notch.set_frequency(1000.0);
    // Coefficients should change when frequency changes
    assert!(
        (notch.b1 - old_b1).abs() > 1e-6,
        "b1 should change after set_frequency"
    );
    assert!((notch.frequency() - 1000.0).abs() < f32::EPSILON);
}
