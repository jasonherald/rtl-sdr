use super::*;
use core::f32::consts::PI;

const TEST_FFT_SIZE: usize = 1024;
const TEST_SAMPLE_RATE: f64 = 48_000.0;

#[test]
fn test_new() {
    let fe = IqFrontend::new(TEST_SAMPLE_RATE, 1, TEST_FFT_SIZE, FftWindow::Nuttall, true);
    assert!(fe.is_ok());
    let fe = fe.unwrap();
    assert_eq!(fe.fft_size(), TEST_FFT_SIZE);
    assert!((fe.sample_rate() - TEST_SAMPLE_RATE).abs() < 1.0);
    assert!((fe.effective_sample_rate() - TEST_SAMPLE_RATE).abs() < 1.0);
}

#[test]
fn test_new_zero_fft() {
    assert!(IqFrontend::new(TEST_SAMPLE_RATE, 1, 0, FftWindow::Nuttall, true).is_err());
}

#[test]
fn test_new_zero_decimation_rejected() {
    assert!(
        IqFrontend::new(
            TEST_SAMPLE_RATE,
            0,
            TEST_FFT_SIZE,
            FftWindow::Nuttall,
            false
        )
        .is_err()
    );
}

#[test]
fn test_set_decimation_zero_rejected() {
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    assert!(fe.set_decimation(0).is_err());
    // State should be unchanged after rejection
    assert_eq!(fe.decim_ratio(), 1);
    assert!((fe.effective_sample_rate() - TEST_SAMPLE_RATE).abs() < 1.0);
}

/// #706 — the FFT accumulator is fed *pre-decimation* input, so the
/// skip budget must always derive from the raw sample rate. Feeding
/// the post-decimation rate made the FFT run `ratio×` too often after
/// the controller's auto-decimation kicked in.
#[test]
fn set_decimation_keeps_fft_skip_budget_on_raw_rate() {
    const AUTO_DECIM_RATIO: u32 = 8;
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    let raw_rate_budget = fe.fft_skip_samples;
    fe.set_decimation(AUTO_DECIM_RATIO).unwrap();
    assert_eq!(
        fe.fft_skip_samples, raw_rate_budget,
        "FFT skip budget must not shrink with decimation"
    );
}

#[test]
fn test_decimation_ratio() {
    let fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        4,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    assert_eq!(fe.decim_ratio(), 4);
    assert!((fe.effective_sample_rate() - 12_000.0).abs() < 1.0);
}

#[test]
fn test_process_dc_signal() {
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    let input = vec![Complex::new(1.0, 0.0); TEST_FFT_SIZE];
    let mut output = vec![Complex::default(); TEST_FFT_SIZE];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

    let (count, fft_ready) = fe.process(&input, &mut output, &mut fft_out).unwrap();
    assert_eq!(count, TEST_FFT_SIZE);
    assert!(fft_ready, "FFT should be ready after fft_size samples");

    // After fftshift, DC signal peaks at center bin (N/2).
    let peak_bin = fft_out
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map_or(0, |(i, _)| i);
    assert_eq!(
        peak_bin,
        TEST_FFT_SIZE / 2,
        "DC signal should peak at the center bin after fftshift"
    );
}

#[test]
fn test_process_tone() {
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();

    // Generate a tone at bin 64
    let tone_bin = 64;
    let input: Vec<Complex> = (0..TEST_FFT_SIZE)
        .map(|i| {
            let phase = 2.0 * PI * (tone_bin as f32) * (i as f32) / (TEST_FFT_SIZE as f32);
            Complex::new(phase.cos(), phase.sin())
        })
        .collect();

    let mut output = vec![Complex::default(); TEST_FFT_SIZE];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

    let (_, fft_ready) = fe.process(&input, &mut output, &mut fft_out).unwrap();
    assert!(fft_ready);

    let peak_bin = fft_out
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map_or(0, |(i, _)| i);

    // After fftshift, positive frequencies in [1..N/2] are
    // moved to [N/2+1..N-1]. A tone generated at bin
    // `tone_bin` (positive) now peaks at `tone_bin + N/2`.
    let expected = tone_bin + TEST_FFT_SIZE / 2;
    assert!(
        peak_bin.abs_diff(expected) <= 2,
        "expected peak near bin {expected} (tone_bin + N/2 after shift), got {peak_bin}"
    );
}

#[test]
fn test_fft_accumulation() {
    // Send samples in chunks smaller than fft_size — FFT should only fire
    // when enough samples accumulate
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    let chunk = vec![Complex::new(1.0, 0.0); 256];
    let mut output = vec![Complex::default(); 256];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

    // First 3 chunks: 768 samples, not enough for 1024 FFT
    for _ in 0..3 {
        let (_, fft_ready) = fe.process(&chunk, &mut output, &mut fft_out).unwrap();
        assert!(!fft_ready, "FFT should not be ready yet");
    }

    // 4th chunk: 1024 total — FFT should fire
    let (_, fft_ready) = fe.process(&chunk, &mut output, &mut fft_out).unwrap();
    assert!(fft_ready, "FFT should be ready after 1024 samples");
}

#[test]
fn test_iq_inversion() {
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Rectangular,
        false,
    )
    .unwrap();
    fe.set_invert_iq(true);

    let input = [Complex::new(1.0, 2.0)];
    let mut output = [Complex::default(); 1];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

    fe.process(&input, &mut output, &mut fft_out).unwrap();
    assert!((output[0].im - (-2.0)).abs() < 1e-6, "im should be negated");
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn iq_correction_stage_is_off_by_default_and_toggles() {
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Rectangular,
        false,
    )
    .unwrap();
    assert!(!fe.iq_correction());
    fe.set_iq_correction(true);
    assert!(fe.iq_correction());

    // With the stage engaged, a gain-imbalanced tone is pulled toward
    // balance: the corrector must actually touch the samples.
    let n = 8192;
    let input: Vec<Complex> = (0..n)
        .map(|i| {
            let theta = 2.0 * std::f32::consts::PI * (i as f32) / 16.0;
            Complex::new(1.3 * theta.cos(), theta.sin())
        })
        .collect();
    let mut output = vec![Complex::default(); n];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];
    for _ in 0..8 {
        fe.process(&input, &mut output, &mut fft_out).unwrap();
    }
    let changed = input
        .iter()
        .zip(&output)
        .any(|(a, b)| (a.re - b.re).abs() > 1e-3 || (a.im - b.im).abs() > 1e-3);
    assert!(
        changed,
        "IQ correction stage must modify an imbalanced signal"
    );

    fe.set_iq_correction(false);
    fe.process(&input, &mut output, &mut fft_out).unwrap();
    assert!(
        input
            .iter()
            .zip(&output)
            .all(|(a, b)| (a.re - b.re).abs() < 1e-6 && (a.im - b.im).abs() < 1e-6),
        "disabled stage must pass samples through untouched"
    );
}

#[test]
fn test_dc_blocking() {
    let mut fe =
        IqFrontend::new(TEST_SAMPLE_RATE, 1, TEST_FFT_SIZE, FftWindow::Nuttall, true).unwrap();

    let input = vec![Complex::new(5.0, 3.0); TEST_FFT_SIZE];
    let mut output = vec![Complex::default(); TEST_FFT_SIZE];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

    for _ in 0..10 {
        fe.process(&input, &mut output, &mut fft_out).unwrap();
    }

    let last = output[TEST_FFT_SIZE - 1];
    assert!(
        last.re.abs() < 2.0,
        "DC should be reduced, got re={}",
        last.re
    );
}

#[test]
fn test_buffer_too_small() {
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    let input = vec![Complex::default(); 100];
    let mut output = vec![Complex::default(); 50]; // too small
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];
    assert!(fe.process(&input, &mut output, &mut fft_out).is_err());
}

#[test]
fn test_decimation_reduces_output() {
    let mut fe = IqFrontend::new(96_000.0, 2, TEST_FFT_SIZE, FftWindow::Nuttall, false).unwrap();
    let input = vec![Complex::new(1.0, 0.0); 2048];
    let mut output = vec![Complex::default(); 2048];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

    let (count, _) = fe.process(&input, &mut output, &mut fft_out).unwrap();
    // 2x decimation: ~1024 output from 2048 input
    assert!(
        (900..=1100).contains(&count),
        "expected ~1024 after 2x decim, got {count}"
    );
}

#[test]
fn test_fft_rate_control() {
    // At 48kHz with 1024 FFT, no rate control would produce ~47 FFTs/sec.
    // Set rate to 10 FPS — should produce far fewer FFTs.
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    fe.set_fft_rate(10.0);
    assert!((fe.fft_rate() - 10.0).abs() < f64::EPSILON);

    // Process 48000 samples (1 second) in 1024-sample chunks
    let chunk = vec![Complex::new(1.0, 0.0); TEST_FFT_SIZE];
    let mut output = vec![Complex::default(); TEST_FFT_SIZE];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];
    let mut fft_count = 0;

    // 48000 / 1024 = ~47 chunks
    for _ in 0..47 {
        let (_, fft_ready) = fe.process(&chunk, &mut output, &mut fft_out).unwrap();
        if fft_ready {
            fft_count += 1;
        }
    }

    // At 10 FPS target, should get roughly 10 FFTs from 1 second of data.
    // Allow some tolerance for boundary effects.
    assert_eq!(fft_count, 10, "expected 10 FFTs at 10 FPS, got {fft_count}");
}

#[test]
fn test_fft_rate_control_non_aligned_chunks() {
    // Use chunk sizes that don't align with fft_size to exercise
    // mid-block carry-over and tail accumulation.
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    fe.set_fft_rate(10.0);

    let chunk_size = 500; // Not a multiple of 1024
    let chunk = vec![Complex::new(1.0, 0.0); chunk_size];
    let mut output = vec![Complex::default(); chunk_size];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];
    let mut fft_count = 0;

    // 48000 / 500 = 96 chunks for ~1 second
    for _ in 0..96 {
        let (_, fft_ready) = fe.process(&chunk, &mut output, &mut fft_out).unwrap();
        if fft_ready {
            fft_count += 1;
        }
    }

    assert_eq!(
        fft_count, 10,
        "expected 10 FFTs at 10 FPS with non-aligned chunks, got {fft_count}"
    );
}

#[test]
fn test_fft_enabled_default_is_true() {
    // Existing call sites that don't explicitly toggle the gate
    // must keep historical behavior. A `new()` IqFrontend should
    // produce FFTs immediately. Per #646.
    let fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    assert!(fe.fft_enabled(), "fft_enabled defaults to true");
}

#[test]
fn test_fft_disabled_suppresses_fft_ready() {
    // With `fft_enabled = false`, `process()` must skip the
    // accumulator + compute loop entirely — no `fft_ready = true`
    // ever surfaces, even after enough samples for many FFTs to
    // have completed at the current rate. Audio path still runs
    // (output buffer is filled by Step 2). Per #646.
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    fe.set_fft_rate(10.0);
    fe.set_fft_enabled(false);
    assert!(!fe.fft_enabled());

    let chunk = vec![Complex::new(1.0, 0.0); TEST_FFT_SIZE];
    let mut output = vec![Complex::default(); TEST_FFT_SIZE];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];
    let mut fft_count = 0;
    let mut total_processed = 0;

    for _ in 0..47 {
        let (processed, fft_ready) = fe.process(&chunk, &mut output, &mut fft_out).unwrap();
        total_processed += processed;
        if fft_ready {
            fft_count += 1;
        }
    }

    assert_eq!(fft_count, 0, "no FFTs should fire while gate is disabled");
    assert!(
        total_processed > 0,
        "audio / decimation path must still run while FFT is disabled \
         (got 0 processed samples — the gate is leaking past Step 1)",
    );
}

#[test]
fn test_fft_re_enable_resumes_at_configured_rate() {
    // Toggling the gate off then back on must restore the
    // previously-configured FFT rate without a settings round-
    // trip. Counter resets on toggle so re-enable starts a fresh
    // window — preventing a half-accumulated frame from the pre-
    // disable period from being emitted as the first post-enable
    // FFT. Per #646.
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    fe.set_fft_rate(10.0);

    let chunk = vec![Complex::new(1.0, 0.0); TEST_FFT_SIZE];
    let mut output = vec![Complex::default(); TEST_FFT_SIZE];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

    // Half a second disabled (no FFTs).
    fe.set_fft_enabled(false);
    for _ in 0..23 {
        let (_, fft_ready) = fe.process(&chunk, &mut output, &mut fft_out).unwrap();
        assert!(!fft_ready, "FFT should not fire while disabled");
    }

    // Re-enable + half a second of input.
    fe.set_fft_enabled(true);
    let mut post_enable_count = 0;
    for _ in 0..47 {
        let (_, fft_ready) = fe.process(&chunk, &mut output, &mut fft_out).unwrap();
        if fft_ready {
            post_enable_count += 1;
        }
    }
    // ~10 FPS target on 1 second of input ≈ 10 FFTs after re-enable.
    assert_eq!(
        post_enable_count, 10,
        "re-enable should resume at the previously-configured 10 FPS rate, got {post_enable_count}"
    );
}

#[test]
fn test_fft_set_enabled_idempotent() {
    // Setting the gate to its current state is a no-op — must
    // not reset the accumulator or skip counter mid-pass. A
    // chatty UI that re-applies the persisted toggle on every
    // frame would otherwise stall the FFT cadence indefinitely.
    let mut fe = IqFrontend::new(
        TEST_SAMPLE_RATE,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        false,
    )
    .unwrap();
    fe.set_fft_rate(10.0);

    let chunk = vec![Complex::new(1.0, 0.0); TEST_FFT_SIZE];
    let mut output = vec![Complex::default(); TEST_FFT_SIZE];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];
    let mut fft_count = 0;

    for _ in 0..47 {
        // Repeatedly setting to the existing `true` state must not
        // disturb the accumulator. Without the early-return guard
        // in `set_fft_enabled` this would zero `fft_skip_counter`
        // every call and we'd never see an FFT.
        fe.set_fft_enabled(true);
        let (_, fft_ready) = fe.process(&chunk, &mut output, &mut fft_out).unwrap();
        if fft_ready {
            fft_count += 1;
        }
    }
    assert_eq!(
        fft_count, 10,
        "idempotent set_fft_enabled(true) must not reset cadence; got {fft_count}"
    );
}
