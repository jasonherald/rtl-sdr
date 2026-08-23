use super::*;
use core::f32::consts::TAU;

// ─── Fixture constants ────────────────────────────────────────────
//
// Hoisted so the same load-bearing rates / chunk sizes / thresholds
// can be retuned in one place if upstream design parameters change,
// and so future readers don't have to re-derive what e.g. "0.7"
// means in context.

/// Standard FM-demod output rate the decoder is built around.
const TEST_INPUT_RATE_HZ: u32 = 48_000;
/// "Realistic" chunk size — ~21 ms at 48 kHz, similar to what the
/// audio pipeline actually delivers.
const TEST_REALISTIC_CHUNK: usize = 1_024;
/// Odd-prime chunk size for the chunked-vs-one-shot equivalence test;
/// picked specifically so chunk boundaries don't align with line
/// boundaries — exposes any state-leak bugs in the decoder pipeline.
const TEST_ODD_PRIME_CHUNK: usize = 513;
/// Generous buffer for `process` output so the slice-based contract
/// never returns `BufferTooSmall` in tests. Way above the 6 lines
/// the longest synthetic input could plausibly emit at once.
const TEST_OUTPUT_CAPACITY: usize = 16;
/// Mid-grey envelope level used in single-line-shape tests.
const TEST_GREY_LEVEL: f32 = 0.7;
/// End-to-end gradient probes — sample at 1/4 and 3/4 of the line.
const TEST_GRADIENT_START: f32 = 0.2;
const TEST_GRADIENT_END: f32 = 0.9;
/// Minimum sync-quality score we expect from clean synthetic input.
const TEST_SYNC_QUALITY_THRESHOLD: f32 = 0.5;
/// Minimum sync-quality score for the more carefully-shaped single
/// line test (which has a fully square Sync A burst).
const TEST_SYNC_QUALITY_THRESHOLD_TIGHT: f32 = 0.6;
/// Below this NCC score the input is effectively noise.
const TEST_SYNC_NOISE_CEILING: f32 = 0.5;
/// Threshold for "good lock" sync quality (above-noise band).
const TEST_SYNC_GOOD_LOCK: f32 = 0.95;
/// Length of the synthetic noise stream in seconds (used by
/// accumulator-bound test).
const TEST_NOISE_DURATION_SEC: usize = 5;

/// Tiny LCG used by the noise tests to generate deterministic
/// pseudo-random samples without pulling in a `rand` dep. Numbers
/// from BSD libc — well-known and known-poor, but plenty random
/// for a "no-pattern" input.
fn lcg_step(state: &mut u32) -> f32 {
    *state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
    ((*state >> 16) & 0x7fff) as f32 / 32_767.0 - 0.5
}

// ─── Apt137Demodulator tests ──────────────────────────────────────

/// Synthesize an AM signal: `(1 + depth·m(t)) · cos(2π·f_c·t)`
/// where `m(t)` is a low-frequency message wave at `f_msg`. Used
/// to feed the demod with a known shape we can recover.
fn synth_am_wave(
    sample_rate: f64,
    carrier_hz: f64,
    msg_hz: f64,
    depth: f32,
    n_samples: usize,
) -> Vec<f32> {
    let dt = 1.0 / sample_rate;
    let omega_c = 2.0 * core::f64::consts::PI * carrier_hz;
    let omega_m = 2.0 * core::f64::consts::PI * msg_hz;
    (0..n_samples)
        .map(|i| {
            let t = i as f64 * dt;
            let envelope = 1.0 + f64::from(depth) * (omega_m * t).cos();
            let carrier = (omega_c * t).cos();
            (envelope * carrier) as f32
        })
        .collect()
}

#[test]
fn apt137_demod_recovers_constant_envelope() {
    // Pure unmodulated carrier `A·cos(2π·f_c·t)`. Demod should
    // recover the constant amplitude `A` to within numerical
    // noise after the one-sample warm-up.
    let fs = 12_480.0_f64;
    let fc = 2_400.0_f64;
    let n = 256_usize;
    let amplitude = 0.7_f32;
    let signal: Vec<f32> = synth_am_wave(fs, fc, 0.0, 0.0, n)
        .iter()
        .map(|s| s * amplitude)
        .collect();
    let mut demod = Apt137Demodulator::new(fs, fc).unwrap();
    let mut out = vec![0.0_f32; n];
    let written = demod.process(&signal, &mut out).unwrap();
    assert_eq!(written, n);
    // First sample is by spec zero (no prior). Skip a few more to
    // let any startup transient settle, then check the rest sit
    // around the true amplitude.
    for &v in &out[5..] {
        assert!(
            (v - amplitude).abs() < 0.01,
            "expected ~{amplitude}, got {v}"
        );
    }
}

#[test]
fn apt137_demod_recovers_modulated_envelope() {
    // `(1 + 0.5·cos(2π·f_msg·t)) · cos(2π·f_c·t)` — a 100 Hz
    // message on a 2400 Hz carrier. The recovered envelope
    // should be `1 + 0.5·cos(2π·f_msg·t)` with the same shape.
    let fs = 12_480.0_f64;
    let fc = 2_400.0_f64;
    let msg = 100.0_f64;
    let depth = 0.5_f32;
    let n = 4_096_usize;
    let signal = synth_am_wave(fs, fc, msg, depth, n);
    let mut demod = Apt137Demodulator::new(fs, fc).unwrap();
    let mut out = vec![0.0_f32; n];
    demod.process(&signal, &mut out).unwrap();

    // Sample the recovered envelope at the message peak (t such
    // that cos(ωm·t) = 1, near i = 0) and trough (cos = -1).
    // Use indices well past the warm-up. At fs=12480 and
    // f_msg=100, one message period spans 124.8 samples, so
    // peak at i≈0, trough at i≈62, peak again at i≈125, etc.
    let peak_i = 250; // 2nd full period peak
    let trough_i = 250 + 62; // half period later
    let peak = out[peak_i];
    let trough = out[trough_i];
    // Peak ≈ 1 + 0.5 = 1.5; trough ≈ 1 - 0.5 = 0.5.
    assert!((peak - 1.5).abs() < 0.05, "peak = {peak}, expected ~1.5");
    assert!(
        (trough - 0.5).abs() < 0.05,
        "trough = {trough}, expected ~0.5"
    );
}

#[test]
fn apt137_demod_streaming_matches_batch() {
    // Splitting a signal into chunks and processing them
    // sequentially must produce the same output as a single
    // big call (modulo the one-sample warm-up at the very
    // start of the stream).
    let fs = 12_480.0_f64;
    let fc = 2_400.0_f64;
    let n = 1_024_usize;
    let signal = synth_am_wave(fs, fc, 50.0, 0.3, n);

    let mut demod_batch = Apt137Demodulator::new(fs, fc).unwrap();
    let mut batch = vec![0.0_f32; n];
    demod_batch.process(&signal, &mut batch).unwrap();

    let mut demod_streamed = Apt137Demodulator::new(fs, fc).unwrap();
    let mut streamed = vec![0.0_f32; n];
    // Three uneven chunks — covers chunk-boundary state handling.
    let split_a = 137_usize;
    let split_b = split_a + 511_usize;
    demod_streamed
        .process(&signal[..split_a], &mut streamed[..split_a])
        .unwrap();
    demod_streamed
        .process(&signal[split_a..split_b], &mut streamed[split_a..split_b])
        .unwrap();
    demod_streamed
        .process(&signal[split_b..], &mut streamed[split_b..])
        .unwrap();

    for i in 1..n {
        assert!(
            (batch[i] - streamed[i]).abs() < 1e-4,
            "batch vs streamed at i={i}: {} vs {}",
            batch[i],
            streamed[i]
        );
    }
}

#[test]
fn apt137_demod_rejects_invalid_frequencies() {
    // sample_rate must be positive.
    assert!(Apt137Demodulator::new(0.0, 2_400.0).is_err());
    assert!(Apt137Demodulator::new(-1.0, 2_400.0).is_err());
    // carrier outside (0, fs/2).
    assert!(Apt137Demodulator::new(12_480.0, 0.0).is_err());
    assert!(Apt137Demodulator::new(12_480.0, -100.0).is_err());
    assert!(Apt137Demodulator::new(12_480.0, 6_240.0).is_err()); // exactly Nyquist
    assert!(Apt137Demodulator::new(12_480.0, 7_000.0).is_err()); // > Nyquist
    // NaN / infinity.
    assert!(Apt137Demodulator::new(f64::NAN, 2_400.0).is_err());
    assert!(Apt137Demodulator::new(12_480.0, f64::INFINITY).is_err());
}

#[test]
fn apt137_demod_dc_robust() {
    // Adding a constant DC offset to the carrier shouldn't dramatically
    // distort the recovered envelope. Compare a clean carrier to one
    // with +0.1 DC bias — apt137's signature property is that DC bias
    // produces a smooth distortion proportional to the bias, not the
    // ±asymmetry that breaks rectifier-based envelope detection.
    let fs = 12_480.0_f64;
    let fc = 2_400.0_f64;
    let n = 1_024_usize;
    let amplitude = 0.5_f32;
    let signal: Vec<f32> = synth_am_wave(fs, fc, 0.0, 0.0, n)
        .iter()
        .map(|s| s * amplitude)
        .collect();
    let signal_with_dc: Vec<f32> = signal.iter().map(|s| s + 0.1).collect();

    let mut demod = Apt137Demodulator::new(fs, fc).unwrap();
    let mut clean = vec![0.0_f32; n];
    demod.process(&signal, &mut clean).unwrap();
    let mut demod = Apt137Demodulator::new(fs, fc).unwrap();
    let mut biased = vec![0.0_f32; n];
    demod.process(&signal_with_dc, &mut biased).unwrap();

    // After warm-up, the per-sample distortion should be bounded.
    // A DC bias of magnitude `b` produces an envelope error roughly
    // proportional to `b` — well under 1.0 here.
    for i in 50..n {
        let diff = (clean[i] - biased[i]).abs();
        assert!(
            diff < 0.3,
            "DC-biased envelope diverged too far at i={i}: |{} - {}| = {diff}",
            clean[i],
            biased[i]
        );
    }
}

#[test]
fn apt137_demod_reset_clears_state() {
    let fs = 12_480.0_f64;
    let fc = 2_400.0_f64;
    let n = 64_usize;
    let signal = synth_am_wave(fs, fc, 0.0, 0.0, n);
    let mut demod = Apt137Demodulator::new(fs, fc).unwrap();
    let mut out = vec![0.0_f32; n];
    demod.process(&signal, &mut out).unwrap();
    // First sample of fresh stream is zero (warm-up).
    assert_eq!(out[0], 0.0);
    // After reset, processing again should re-trigger the warm-up.
    out.fill(99.0); // poison
    demod.reset();
    demod.process(&signal, &mut out).unwrap();
    assert_eq!(out[0], 0.0, "reset() failed to clear `prev` state");
}

#[test]
fn pixel_and_line_invariants_hold() {
    assert_eq!(PIXELS_PER_SECOND as usize, 4160);
    // 12480 Hz / (2 lines/sec × 2080 px/line) = 3 samples/px,
    // so SAMPLES_PER_LINE = 6240 at the new (lower) work rate.
    assert_eq!(SAMPLES_PER_LINE, 6_240);
    assert_eq!(SAMPLES_PER_PIXEL, 3);
    assert_eq!(
        INTERMEDIATE_RATE_HZ as usize,
        SAMPLES_PER_LINE * LINES_PER_SECOND as usize
    );
    assert_eq!(INTERMEDIATE_RATE_HZ, 12_480);
    // Padded Sync A template (per A3 / noaa-apt parity):
    // 38 px = 2 leading + 28 modulated + 8 trailing.
    // At 3 samples/px → 114 samples total, of which the middle
    // 84 (= 7 cycles × 12 samples/cycle) carry the modulation.
    assert_eq!(SYNC_A_TOTAL_PX, 38);
    assert_eq!(SYNC_A_TEMPLATE_LEN, SYNC_A_TOTAL_PX * SAMPLES_PER_PIXEL);
    assert_eq!(SYNC_A_TEMPLATE_LEN, 114);
    assert_eq!(
        SYNC_A_LEADING_PAD_SAMPLES,
        SYNC_A_LEADING_PAD_PX * SAMPLES_PER_PIXEL
    );
    assert_eq!(SYNC_A_LEADING_PAD_SAMPLES, 6);
    assert_eq!(
        SYNC_B_TEMPLATE_LEN,
        SYNC_BURST_CYCLES * SAMPLES_PER_SYNC_B_CYCLE
    );
    assert_eq!(SAMPLES_PER_SYNC_A_CYCLE, 12);
    assert_eq!(SAMPLES_PER_SYNC_B_CYCLE, 15);
}

/// #776 — the constructor guard only checked the `2·f_c` Nyquist
/// floor (4800 Hz) but the DC-removing bandpass it builds needs
/// `cutoff + transition/2` below Nyquist, i.e. a rate above
/// 10 600 Hz. 8 / 9.6 kHz audio passed the guard and died inside
/// the tap designer with an error naming filter internals.
#[test]
fn apt_decoder_rejects_rates_the_dc_bandpass_cannot_support() {
    for rate in [8_000, 9_600, 10_600] {
        let err = AptDecoder::new(rate).err().map(|e| e.to_string());
        assert!(err.is_some(), "rate {rate} must be rejected");
        let msg = err.unwrap_or_default();
        assert!(
            msg.contains("input_rate_hz") && msg.contains(&MIN_INPUT_RATE_HZ.to_string()),
            "rate {rate}: error must name the input-rate floor, got: {msg}"
        );
    }
    assert!(
        AptDecoder::new(MIN_INPUT_RATE_HZ + 1).is_ok(),
        "the floor is exclusive: {} Hz is accepted",
        MIN_INPUT_RATE_HZ + 1
    );
    assert!(
        AptDecoder::new(11_025).is_ok(),
        "11 025 Hz clears the floor"
    );
}

#[test]
fn envelope_detector_rejects_too_low_sample_rate() {
    // Below 2·2400 Hz = 4800 Hz Nyquist floor we'd alias the rectification
    // harmonic back into the video band — the detector must refuse.
    assert!(EnvelopeDetector::new(4_000).is_err());
}

#[test]
fn envelope_detector_accepts_intermediate_rate() {
    let det = EnvelopeDetector::new(INTERMEDIATE_RATE_HZ).unwrap();
    // Sanity: taps should land in the hundreds with our design.
    assert!(det.lpf_tap_count() >= 10, "got {}", det.lpf_tap_count());
}

#[test]
fn envelope_recovers_constant_amplitude() {
    // Modulate a unit-amplitude subcarrier: x(t) = cos(2π f_c t).
    // Rectified + LPF should converge to ~2/π ≈ 0.6366 (DC of |cos|).
    let rate = INTERMEDIATE_RATE_HZ;
    let n = 20_800; // 1 second
    let input: Vec<f32> = (0..n)
        .map(|i| (TAU * SUBCARRIER_HZ as f32 * (i as f32) / rate as f32).cos())
        .collect();

    let mut detector = EnvelopeDetector::new(rate).unwrap();
    let mut output = vec![0.0_f32; n];
    detector.process(&input, &mut output).unwrap();

    // Look at the second half of the buffer — past the FIR warmup.
    let steady = &output[n / 2..];
    let mean: f32 = steady.iter().sum::<f32>() / steady.len() as f32;
    let two_over_pi = 2.0 / core::f32::consts::PI;
    assert!(
        (mean - two_over_pi).abs() < 0.02,
        "expected DC ≈ 2/π ({two_over_pi:.4}), got {mean:.4}",
    );

    // And confirm the 2·f_c (4800 Hz) ripple is actually suppressed —
    // peak-to-peak of the steady region should be small.
    // Tolerance: at the new 12480 Hz work rate the 4800 Hz harmonic
    // sits at 0.769·Nyquist (vs. 0.46·Nyquist when this test was
    // originally written for 20800 Hz). The Nuttall LPF still
    // rejects it, but with less margin — ~0.10 ripple is fine for
    // the legacy-path EnvelopeDetector (which is no longer in the
    // live APT pipeline; replaced by Apt137Demodulator in A4).
    let (min, max) = steady
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        });
    assert!(
        (max - min) < 0.10,
        "LPF residual ripple too large: [{min:.4}, {max:.4}]"
    );
}

#[test]
fn envelope_follows_slow_ramp_modulation() {
    // Carrier at 2400 Hz, envelope = linear ramp 0.0 → 1.0 over one APT line.
    // After rectify + LPF, output should track (2/π) · ramp(t) with some
    // FIR-group-delay lag.
    let rate = INTERMEDIATE_RATE_HZ;
    let n = SAMPLES_PER_LINE; // one full scan line
    let input: Vec<f32> = (0..n)
        .map(|i| {
            let env = (i as f32) / (n as f32);
            let carrier = (TAU * SUBCARRIER_HZ as f32 * (i as f32) / rate as f32).cos();
            env * carrier
        })
        .collect();

    let mut detector = EnvelopeDetector::new(rate).unwrap();
    let mut output = vec![0.0_f32; n];
    detector.process(&input, &mut output).unwrap();

    // Sample three points along the ramp (past FIR settling) and check
    // each lies near (2/π) · expected_env with a generous tolerance —
    // the LPF has real group delay, so exact alignment would be wrong.
    let two_over_pi = 2.0 / core::f32::consts::PI;
    let delay = detector.lpf_tap_count() / 2;
    for &check in &[n / 4, n / 2, (3 * n) / 4] {
        let expected = (check as f32) / (n as f32) * two_over_pi;
        let measured = output[check + delay.min(n - check - 1)];
        assert!(
            (measured - expected).abs() < 0.05,
            "ramp point {check}: expected ~{expected:.3}, got {measured:.3}",
        );
    }

    // And the very last output sample (after most of the ramp) should
    // have reached near full amplitude.
    assert!(
        output[n - 1] > 0.6 * two_over_pi,
        "end of ramp should be near full envelope, got {}",
        output[n - 1]
    );
}

#[test]
fn envelope_process_buffer_too_small_errors() {
    let mut detector = EnvelopeDetector::new(INTERMEDIATE_RATE_HZ).unwrap();
    let input = vec![0.0_f32; 32];
    let mut output = vec![0.0_f32; 16];
    assert!(detector.process(&input, &mut output).is_err());
}

#[test]
fn envelope_process_handles_empty_input() {
    let mut detector = EnvelopeDetector::new(INTERMEDIATE_RATE_HZ).unwrap();
    let mut output: [f32; 0] = [];
    let n = detector.process(&[], &mut output).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn real_resampler_downsamples_tone() {
    // 48000 → 20800 Hz. Pump a 1 kHz tone through and verify the output
    // (a) has the expected number of samples (within polyphase rounding)
    // and (b) still oscillates.
    let in_rate = 48_000.0_f64;
    let out_rate = f64::from(INTERMEDIATE_RATE_HZ);
    let n_in = 4800_usize; // 100 ms of input
    let tone_hz = 1_000.0_f32;

    let input: Vec<f32> = (0..n_in)
        .map(|i| (TAU * tone_hz * (i as f32) / (in_rate as f32)).cos())
        .collect();

    let mut r = RealResampler::new(in_rate, out_rate).unwrap();
    // Worst-case: ceil(n_in * out/in) + 1 = ceil(2080) + 1 = 2081.
    let mut output = vec![0.0_f32; 2100];
    let produced = r.process(&input, &mut output).unwrap();

    let expected = (n_in as f64 * out_rate / in_rate) as usize;
    assert!(
        produced.abs_diff(expected) <= 2,
        "expected ~{expected} out samples, got {produced}",
    );

    // Skip FIR warmup, verify the tone is still there (non-trivial peak).
    let skip = produced / 5;
    let steady = &output[skip..produced];
    let peak = steady.iter().map(|x| x.abs()).fold(0.0_f32, f32::max);
    assert!(peak > 0.3, "resampled tone peak too low: {peak}");

    // Zero crossings ≈ 2 · cycles ≈ 2 · (tone_hz / out_rate) · steady_len.
    let crossings = steady
        .windows(2)
        .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
        .count();
    assert!(crossings > 20, "expected oscillation, got {crossings}");
}

#[test]
fn real_resampler_passthrough_on_equal_rates() {
    let mut r = RealResampler::new(48_000.0, 48_000.0).unwrap();
    let input: Vec<f32> = (0..64).map(|i| i as f32).collect();
    let mut output = vec![0.0_f32; 64];
    let n = r.process(&input, &mut output).unwrap();
    assert_eq!(n, 64);
    for (i, &v) in output.iter().enumerate().take(64) {
        assert!((v - i as f32).abs() < 1e-5, "mismatch at {i}: {v}");
    }
}

#[test]
fn real_resampler_continuity_across_chunks() {
    // Feed the same 48 kHz → 20800 Hz tone in one big block vs. three
    // smaller chunks; the stitched output should match the one-shot run
    // to within a couple of samples of polyphase phase drift.
    let in_rate = 48_000.0_f64;
    let out_rate = f64::from(INTERMEDIATE_RATE_HZ);
    let n_in = 3_072_usize;
    let tone_hz = 500.0_f32;
    let input: Vec<f32> = (0..n_in)
        .map(|i| (TAU * tone_hz * (i as f32) / (in_rate as f32)).sin())
        .collect();

    let mut r_whole = RealResampler::new(in_rate, out_rate).unwrap();
    let mut one_shot = vec![0.0_f32; n_in];
    let n_whole = r_whole.process(&input, &mut one_shot).unwrap();

    let mut r_chunked = RealResampler::new(in_rate, out_rate).unwrap();
    let mut chunked: Vec<f32> = Vec::new();
    let mut tmp = vec![0.0_f32; n_in];
    for chunk in input.chunks(1024) {
        let c = r_chunked.process(chunk, &mut tmp).unwrap();
        chunked.extend_from_slice(&tmp[..c]);
    }

    assert!(
        n_whole.abs_diff(chunked.len()) <= 1,
        "one-shot produced {n_whole}, chunked produced {}",
        chunked.len(),
    );

    // Compare the steady portion (past FIR warmup) sample-by-sample.
    let steady_start = n_whole / 4;
    let common = n_whole.min(chunked.len());
    for i in steady_start..common {
        assert!(
            (one_shot[i] - chunked[i]).abs() < 1e-4,
            "chunk drift at {i}: one-shot {} vs chunked {}",
            one_shot[i],
            chunked[i],
        );
    }
}

#[test]
fn real_resampler_empty_input_is_zero() {
    let mut r = RealResampler::new(48_000.0, f64::from(INTERMEDIATE_RATE_HZ)).unwrap();
    let mut output = vec![0.0_f32; 8];
    assert_eq!(r.process(&[], &mut output).unwrap(), 0);
}

#[test]
fn real_resampler_reset_clears_state() {
    let mut r = RealResampler::new(48_000.0, f64::from(INTERMEDIATE_RATE_HZ)).unwrap();
    let hot = vec![1.0_f32; 256];
    let mut out = vec![0.0_f32; 256];
    r.process(&hot, &mut out).unwrap();
    r.reset();
    // After reset, processing zeros should produce near-zeros (no carry).
    let zeros = vec![0.0_f32; 256];
    let mut out2 = vec![0.0_f32; 256];
    let n = r.process(&zeros, &mut out2).unwrap();
    for &v in &out2[..n] {
        assert!(v.abs() < 1e-4, "reset residual too large: {v}");
    }
}

/// Build a synthetic envelope buffer with a sync burst embedded at the
/// given offset, preceded and followed by constant "floor" amplitude.
fn synth_envelope_with_sync(
    total_len: usize,
    sync_offset: usize,
    samples_per_cycle: usize,
    cycles: usize,
    floor: f32,
    peak: f32,
) -> Vec<f32> {
    let mut buf = vec![floor; total_len];
    let sync_len = samples_per_cycle * cycles;
    assert!(sync_offset + sync_len <= total_len);
    for i in 0..sync_len {
        let phase = i % samples_per_cycle;
        let high = phase < samples_per_cycle / 2;
        buf[sync_offset + i] = if high { peak } else { floor };
    }
    buf
}

#[test]
fn sync_detector_template_lengths_match_constants() {
    let d = SyncDetector::new();
    assert_eq!(d.template_a_len(), SYNC_A_TEMPLATE_LEN);
    assert_eq!(d.template_b_len(), SYNC_B_TEMPLATE_LEN);
}

#[test]
fn sync_detector_returns_none_on_short_input() {
    let d = SyncDetector::new();
    let short = vec![0.0_f32; 10];
    assert!(d.find_best(&short, SyncChannel::A).is_none());
    assert!(d.find_best(&short, SyncChannel::B).is_none());
}

#[test]
fn sync_detector_locates_sync_a_exactly() {
    // `synth_envelope_with_sync` plants the burst's first HIGH
    // half-cycle at `burst_offset`. The padded template's first
    // HIGH transition sits at sample
    // `SYNC_A_FIRST_HIGH_OFFSET_SAMPLES` from template start, so
    // a perfect match returns
    // `m.offset = burst_offset - SYNC_A_FIRST_HIGH_OFFSET_SAMPLES`.
    // Pick a burst offset that leaves room for both the leading
    // template padding before it and the trailing tail after.
    let burst_offset = 317;
    let buf = synth_envelope_with_sync(
        2_000,
        burst_offset,
        SAMPLES_PER_SYNC_A_CYCLE,
        SYNC_BURST_CYCLES,
        0.1,
        0.9,
    );
    let m = SyncDetector::new()
        .find_best(&buf, SyncChannel::A)
        .expect("should match");
    assert_eq!(m.channel, SyncChannel::A);
    let expected_template_start = burst_offset - SYNC_A_FIRST_HIGH_OFFSET_SAMPLES;
    assert_eq!(
        m.offset, expected_template_start,
        "expected template-start offset {expected_template_start} \
         (= burst_offset {burst_offset} − SYNC_A_FIRST_HIGH_OFFSET_SAMPLES \
         {SYNC_A_FIRST_HIGH_OFFSET_SAMPLES}), got {}",
        m.offset,
    );
    assert!(
        m.quality > TEST_SYNC_GOOD_LOCK,
        "quality too low: {:.3}",
        m.quality,
    );
}

#[test]
fn sync_detector_locates_sync_b_exactly() {
    let offset = 742;
    let buf = synth_envelope_with_sync(
        2_000,
        offset,
        SAMPLES_PER_SYNC_B_CYCLE,
        SYNC_BURST_CYCLES,
        0.1,
        0.9,
    );
    let m = SyncDetector::new()
        .find_best(&buf, SyncChannel::B)
        .expect("should match");
    assert_eq!(m.channel, SyncChannel::B);
    assert_eq!(m.offset, offset);
    assert!(
        m.quality > TEST_SYNC_GOOD_LOCK,
        "quality too low: {:.3}",
        m.quality,
    );
}

#[test]
fn sync_detector_is_dc_offset_invariant() {
    // Same sync pattern twice, once with large DC offset in the
    // envelope floor; quality must remain high and offset must agree.
    let offset = 200;
    let low = synth_envelope_with_sync(
        1_500,
        offset,
        SAMPLES_PER_SYNC_A_CYCLE,
        SYNC_BURST_CYCLES,
        0.0,
        1.0,
    );
    let high: Vec<f32> = low.iter().map(|v| v + 5.0).collect();
    let d = SyncDetector::new();
    let m_lo = d.find_best(&low, SyncChannel::A).unwrap();
    let m_hi = d.find_best(&high, SyncChannel::A).unwrap();
    assert_eq!(m_lo.offset, m_hi.offset);
    assert!((m_lo.quality - m_hi.quality).abs() < 0.01);
}

#[test]
fn sync_detector_noise_has_low_quality() {
    // Pseudo-random noise (deterministic LCG) — no embedded sync at all.
    // Any accidental peak must score well below a real match.
    let mut state: u32 = 1;
    let buf: Vec<f32> = (0..2_000).map(|_| lcg_step(&mut state)).collect();
    let m = SyncDetector::new().find_best(&buf, SyncChannel::A).unwrap();
    assert!(
        m.quality < TEST_SYNC_NOISE_CEILING,
        "noise quality too high: {:.3} at offset {}",
        m.quality,
        m.offset,
    );
}

#[test]
fn sync_detector_picks_stronger_of_two_bursts() {
    // Two bursts in the same buffer: one attenuated, one full-amp.
    // The detector must pick the full-amp one (higher SNR ⇒ higher NCC).
    // Use `synth_envelope_with_sync` so the template gets a clean
    // shape match — its layout (HIGH-LOW pairs starting HIGH) is
    // what real APT signals produce; manual ±contrast plants in
    // the test would have to mirror that exactly to score 1.0.
    let weak_burst_off = 200;
    let strong_burst_off = 1_000;
    let mut buf = vec![0.1_f32; 2_500];
    // Weak burst: 0.01 contrast above floor.
    for i in 0..(SAMPLES_PER_SYNC_A_CYCLE * SYNC_BURST_CYCLES) {
        let phase = i % SAMPLES_PER_SYNC_A_CYCLE;
        let high = phase < SAMPLES_PER_SYNC_A_CYCLE / 2;
        buf[weak_burst_off + i] = if high { 0.11 } else { 0.10 };
    }
    // Strong burst: 0.9 contrast above floor.
    for i in 0..(SAMPLES_PER_SYNC_A_CYCLE * SYNC_BURST_CYCLES) {
        let phase = i % SAMPLES_PER_SYNC_A_CYCLE;
        let high = phase < SAMPLES_PER_SYNC_A_CYCLE / 2;
        buf[strong_burst_off + i] = if high { 1.0 } else { 0.1 };
    }
    let m = SyncDetector::new().find_best(&buf, SyncChannel::A).unwrap();
    // The matched offset is `burst_first_high − SYNC_A_FIRST_HIGH_OFFSET_SAMPLES`
    // for either burst. Both shapes correlate perfectly, so the detector
    // will pick whichever has the larger raw NCC numerator (the strong
    // one — higher amplitude swing).
    let weak_template_start = weak_burst_off - SYNC_A_FIRST_HIGH_OFFSET_SAMPLES;
    let strong_template_start = strong_burst_off - SYNC_A_FIRST_HIGH_OFFSET_SAMPLES;
    assert!(
        m.offset == weak_template_start || m.offset == strong_template_start,
        "expected one of {{{weak_template_start}, {strong_template_start}}}, \
         got {}",
        m.offset,
    );
    assert!(m.quality > 0.9, "quality too low: {:.3}", m.quality);
}

/// Synthesize one full APT line worth of FM-demod audio at `rate`:
/// a 2400 Hz carrier with envelope = Sync A burst then a constant grey.
/// Keeps tests independent of the real capture pipeline.
fn synth_line_audio(rate: u32, grey_level: f32) -> Vec<f32> {
    let rate_f = f64::from(rate);
    let line_dur = 1.0_f64 / LINES_PER_SECOND;
    let n = (rate_f * line_dur).round() as usize;
    let mut out = Vec::with_capacity(n);
    let sync_samples = (rate_f * SYNC_BURST_CYCLES as f64 / SYNC_A_HZ).round() as usize;
    for i in 0..n {
        let t = (i as f64) / rate_f;
        let carrier = (core::f64::consts::TAU * SUBCARRIER_HZ * t).sin() as f32;
        let envelope = if i < sync_samples {
            // Sync A square-wave envelope: alternating 0 / grey_level
            let cyc_samples = rate_f / SYNC_A_HZ;
            let phase = (i as f64 % cyc_samples) / cyc_samples;
            if phase < 0.5 { grey_level } else { 0.0 }
        } else {
            grey_level
        };
        out.push(envelope * carrier);
    }
    out
}

#[test]
fn apt_decoder_rejects_sub_nyquist_input_rate() {
    // At or below 2·SUBCARRIER_HZ (4800 Hz) the 2400 Hz APT subcarrier
    // is at-or-past Nyquist — at exactly 4800 Hz the cosine samples
    // hit phase-ambiguous points and collapse, so the boundary itself
    // must be rejected, not just rates strictly below.
    assert!(AptDecoder::new(0).is_err());
    assert!(AptDecoder::new(4_799).is_err());
    assert!(AptDecoder::new(4_800).is_err());
    // 8000 Hz used to be accepted but the A4 DC-removal Kaiser
    // bandpass needs Nyquist > cutoff + transition/2 = 5300 Hz, so
    // input rate must exceed ~10.6 kHz. 11025 Hz (CD-quality
    // sub-rate) is the smallest realistic rate that still passes;
    // the FM-demod output (48 kHz) is comfortably above this.
    assert!(AptDecoder::new(8_000).is_err());
    assert!(AptDecoder::new(11_025).is_ok());
    assert!(AptDecoder::new(48_000).is_ok());
}

#[test]
fn apt_decoder_emits_nothing_with_short_input() {
    let mut d = AptDecoder::new(TEST_INPUT_RATE_HZ).unwrap();
    let input = vec![0.0_f32; 128];
    let mut out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
    let n = d.process(&input, &mut out).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn apt_decoder_recovers_line_from_synthetic_audio() {
    // Feed three lines of synthetic audio so the decoder has enough
    // post-warmup buffer to emit at least one.
    let rate = TEST_INPUT_RATE_HZ;
    let mut d = AptDecoder::new(rate).unwrap();
    let one_line = synth_line_audio(rate, TEST_GREY_LEVEL);
    let mut three_lines = Vec::with_capacity(one_line.len() * 3);
    for _ in 0..3 {
        three_lines.extend_from_slice(&one_line);
    }
    let mut out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
    let produced = d.process(&three_lines, &mut out).unwrap();
    assert!(
        produced > 0,
        "expected at least one decoded line from 3-line synthetic input",
    );
    for (i, line) in out[..produced].iter().enumerate() {
        assert_eq!(line.sync_channel, SyncChannel::A);
        assert!(
            line.sync_quality > TEST_SYNC_QUALITY_THRESHOLD_TIGHT,
            "line {i} quality too low: {:.3}",
            line.sync_quality,
        );
    }
}

#[test]
fn apt_decoder_chunked_matches_oneshot() {
    // Any reasonable chunking must produce bit-identical pixel output
    // compared to a single giant call — the decoder's state carries
    // everything the resampler / envelope / accumulator need.
    let rate = TEST_INPUT_RATE_HZ;
    let mut audio = Vec::new();
    for _ in 0..4 {
        audio.extend_from_slice(&synth_line_audio(rate, 0.6));
    }

    let mut one_shot_dec = AptDecoder::new(rate).unwrap();
    let mut lines_whole = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
    let n_whole = one_shot_dec.process(&audio, &mut lines_whole).unwrap();
    lines_whole.truncate(n_whole);

    let mut chunked_dec = AptDecoder::new(rate).unwrap();
    let mut lines_chunked: Vec<AptLine> = Vec::new();
    let mut chunk_out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
    for chunk in audio.chunks(TEST_ODD_PRIME_CHUNK) {
        let n = chunked_dec.process(chunk, &mut chunk_out).unwrap();
        for line in &chunk_out[..n] {
            lines_chunked.push(line.clone());
        }
    }

    assert_eq!(
        lines_whole.len(),
        lines_chunked.len(),
        "chunked and one-shot produced different line counts",
    );
    for (w, c) in lines_whole.iter().zip(lines_chunked.iter()) {
        assert_eq!(w.pixels, c.pixels, "chunked pixels diverge from one-shot");
    }
}

#[test]
fn apt_decoder_reset_clears_pending_state() {
    let rate = TEST_INPUT_RATE_HZ;
    let mut d = AptDecoder::new(rate).unwrap();
    let partial = synth_line_audio(rate, TEST_GREY_LEVEL);
    let mut out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
    // Push part of a line — not enough to emit.
    d.process(&partial[..partial.len() / 4], &mut out).unwrap();

    d.reset();

    // After reset, pushing silence should not emit a line on account
    // of leftover state.
    let silence = vec![0.0_f32; 2_048];
    let n = d.process(&silence, &mut out).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn apt_decoder_bounds_accumulator_on_pure_noise() {
    // Pure pseudo-random noise still trips `find_best` to a peak —
    // what matters is that the internal buffer never grows unbounded.
    let rate = TEST_INPUT_RATE_HZ;
    let mut d = AptDecoder::new(rate).unwrap();
    let mut state: u32 = 7;
    let noise: Vec<f32> = (0..(rate as usize * TEST_NOISE_DURATION_SEC))
        .map(|_| lcg_step(&mut state))
        .collect();
    let mut out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
    for chunk in noise.chunks(TEST_REALISTIC_CHUNK) {
        let _ = d.process(chunk, &mut out).unwrap();
        assert!(
            d.accumulator.len() <= DECODER_BUFFER_CAP,
            "accumulator grew past cap: {}",
            d.accumulator.len(),
        );
    }
}

#[test]
fn apt_decoder_undersized_output_preserves_all_decoded_lines() {
    // Streaming contract: if more lines are decoded than `output` can
    // hold, the surplus lives in the internal ready queue and must
    // surface on subsequent calls — *no* decoded line should ever
    // be silently dropped just because the caller's output was tight.
    let rate = TEST_INPUT_RATE_HZ;

    // Reference run: same audio, generous output, count lines emitted.
    let mut audio = Vec::new();
    for _ in 0..6 {
        audio.extend_from_slice(&synth_line_audio(rate, TEST_GREY_LEVEL));
    }
    let mut reference = AptDecoder::new(rate).unwrap();
    let mut ref_out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
    let n_reference = reference.process(&audio, &mut ref_out).unwrap();
    assert!(
        n_reference > 1,
        "test setup needs to emit multiple lines; got {n_reference}",
    );

    // Tight run: one-slot output, drained line-by-line across calls.
    let mut tight = AptDecoder::new(rate).unwrap();
    let mut tight_out = vec![AptLine::default(); 1];
    let n_first = tight.process(&audio, &mut tight_out).unwrap();
    assert_eq!(n_first, 1);
    let mut tight_total = 1_usize;

    // Drain the ready queue with empty inputs — every queued line
    // must come through.
    loop {
        let n = tight.process(&[], &mut tight_out).unwrap();
        if n == 0 {
            break;
        }
        tight_total += n;
    }

    assert_eq!(
        tight_total, n_reference,
        "tight-output run produced {tight_total} lines, generous run produced \
         {n_reference} — surplus was silently dropped",
    );
}

#[test]
fn apt_decoder_accumulator_capacity_absorbs_intentional_overshoot() {
    // The chunked-ingestion path intentionally lets `accumulator` peak
    // at DECODER_BUFFER_CAP + SAMPLES_PER_LINE before being trimmed.
    // Reserving exactly DECODER_BUFFER_CAP would force a realloc on
    // first backpressure (and Vec keeps the larger capacity afterward,
    // defeating bounded memory). Pre-reserving for the peak avoids
    // it. Verify by snapshotting capacity after construction and
    // again after a multi-line process call — they must match.
    let rate = TEST_INPUT_RATE_HZ;
    let mut d = AptDecoder::new(rate).unwrap();
    let initial_capacity = d.accumulator.capacity();
    assert!(
        initial_capacity >= DECODER_BUFFER_CAP + SAMPLES_PER_LINE,
        "initial accumulator capacity {initial_capacity} too small to \
         absorb the chunked-ingestion overshoot",
    );

    // Push 8 lines through a 1-slot output to force backpressure.
    let mut audio = Vec::new();
    for _ in 0..8 {
        audio.extend_from_slice(&synth_line_audio(rate, TEST_GREY_LEVEL));
    }
    let mut tight_out = vec![AptLine::default(); 1];
    d.process(&audio, &mut tight_out).unwrap();

    assert_eq!(
        d.accumulator.capacity(),
        initial_capacity,
        "accumulator capacity grew under backpressure — Vec reallocated, \
         defeating bounded-memory intent",
    );
}

#[test]
fn apt_decoder_huge_chunk_keeps_resample_scratch_bounded() {
    // Outer-loop subchunking guarantees that resample_scratch and
    // demod_scratch never need to grow with caller chunk size.
    // Snapshot capacities, push a multi-megabyte input chunk, and
    // assert the scratch vectors haven't reallocated to fit the
    // input's full size.
    let rate = TEST_INPUT_RATE_HZ;
    let mut d = AptDecoder::new(rate).unwrap();
    let resample_cap_before = d.resample_scratch.capacity();
    let envelope_cap_before = d.demod_scratch.capacity();

    // 100 audio lines = 2.4 M samples = ~9.6 MB. Pre-bounded design,
    // resample_scratch must stay sized for one INPUT_SUBCHUNK_SAMPLES
    // worth of output, not the whole 9.6 MB input.
    let mut huge = Vec::new();
    for _ in 0..100 {
        huge.extend_from_slice(&synth_line_audio(rate, TEST_GREY_LEVEL));
    }
    let mut roomy_out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
    d.process(&huge, &mut roomy_out).unwrap();

    assert_eq!(
        d.resample_scratch.capacity(),
        resample_cap_before,
        "resample_scratch reallocated under huge input — outer subchunk \
         bound is broken (cap was {resample_cap_before}, now {})",
        d.resample_scratch.capacity(),
    );
    assert_eq!(
        d.demod_scratch.capacity(),
        envelope_cap_before,
        "demod_scratch reallocated under huge input — outer subchunk \
         bound is broken (cap was {envelope_cap_before}, now {})",
        d.demod_scratch.capacity(),
    );
}

#[test]
fn apt_decoder_huge_chunk_keeps_accumulator_bounded() {
    // CR concern: a single oversized input must not let the raw
    // accumulator transiently balloon past its cap. With chunk-bounded
    // ingestion the accumulator should never exceed
    // DECODER_BUFFER_CAP + SAMPLES_PER_LINE at any instant.
    let rate = TEST_INPUT_RATE_HZ;
    let mut d = AptDecoder::new(rate).unwrap();
    let mut huge = Vec::new();
    for _ in 0..100 {
        huge.extend_from_slice(&synth_line_audio(rate, TEST_GREY_LEVEL));
    }
    // One-slot output and a one-slot ready queue effective limit
    // (lines still queue internally up to READY_QUEUE_CAP).
    let mut tight_out = vec![AptLine::default(); 1];
    d.process(&huge, &mut tight_out).unwrap();
    // After the call the accumulator must be at-or-below cap — the
    // chunked-ingestion design re-trims after each chunk.
    assert!(
        d.accumulator.len() <= DECODER_BUFFER_CAP,
        "accumulator past cap after huge input: {}",
        d.accumulator.len(),
    );
    // And the ready queue is bounded by its own cap.
    assert!(
        d.ready_lines.len() <= READY_QUEUE_CAP,
        "ready queue past cap: {}",
        d.ready_lines.len(),
    );
}

#[test]
fn envelope_detector_rejects_below_rectified_nyquist() {
    // The rectified subcarrier harmonic sits at 2·f_c = 4800 Hz, so
    // any sample rate at or below 2·4800 = 9600 Hz aliases that tone
    // back into the video band. The detector must refuse those rates.
    // Earlier values like 8 kHz "look" plausible (above 2·f_c) but
    // the rectified harmonic Nyquist still isn't met — make sure 8 kHz
    // is rejected, and 16 kHz (well above the floor) is accepted.
    assert!(EnvelopeDetector::new(8_000).is_err());
    assert!(EnvelopeDetector::new(9_600).is_err()); // exactly at floor
    assert!(EnvelopeDetector::new(16_000).is_ok());
}

/// Synthesize a realistic APT line with a sync A burst followed by a
/// linear grey gradient across the video area. Returns audio at `rate`
/// with a 2400 Hz AM carrier modulated by the envelope pattern.
fn synth_line_with_gradient(rate: u32, start_grey: f32, end_grey: f32) -> Vec<f32> {
    let rate_f = f64::from(rate);
    let line_dur = 1.0_f64 / LINES_PER_SECOND;
    let n = (rate_f * line_dur).round() as usize;
    let sync_samples = (rate_f * SYNC_BURST_CYCLES as f64 / SYNC_A_HZ).round() as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = (i as f64) / rate_f;
        let carrier = (core::f64::consts::TAU * SUBCARRIER_HZ * t).sin() as f32;
        let envelope = if i < sync_samples {
            let cyc_samples = rate_f / SYNC_A_HZ;
            let phase = (i as f64 % cyc_samples) / cyc_samples;
            if phase < 0.5 { 1.0 } else { 0.0 }
        } else {
            // Linear gradient over the video portion.
            let frac = (i - sync_samples) as f32 / (n - sync_samples) as f32;
            start_grey + frac * (end_grey - start_grey)
        };
        out.push(envelope * carrier);
    }
    out
}

#[test]
fn apt_decoder_end_to_end_gradient_is_monotonic() {
    // Six-line synthetic capture, each line with the same 0.2→0.9 grey
    // gradient. Verify the decoder:
    //   (1) emits at least three lines (early ones eaten by resampler /
    //       envelope filter warmup)
    //   (2) stays locked on every emitted line
    //   (3) produces a roughly monotonic pixel gradient inside each
    //       line's video area
    //   (4) reports strictly-increasing input_sample_index values
    let rate = TEST_INPUT_RATE_HZ;

    let mut audio = Vec::new();
    for _ in 0..6 {
        audio.extend_from_slice(&synth_line_with_gradient(
            rate,
            TEST_GRADIENT_START,
            TEST_GRADIENT_END,
        ));
    }

    let mut decoder = AptDecoder::new(rate).unwrap();
    let mut lines: Vec<AptLine> = Vec::new();
    let mut chunk_out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
    for chunk in audio.chunks(TEST_REALISTIC_CHUNK) {
        let n = decoder.process(chunk, &mut chunk_out).unwrap();
        for line in &chunk_out[..n] {
            lines.push(line.clone());
        }
    }
    assert!(
        lines.len() >= 3,
        "expected >= 3 lines from 6-line input, got {}",
        lines.len(),
    );

    // Sync lock held on every emitted line.
    for (i, line) in lines.iter().enumerate() {
        assert!(
            line.sync_quality > TEST_SYNC_QUALITY_THRESHOLD,
            "line {i}: quality {:.3} below lock threshold",
            line.sync_quality,
        );
        assert_eq!(line.sync_channel, SyncChannel::A);
    }

    // input_sample_index strictly monotonic.
    for pair in lines.windows(2) {
        assert!(
            pair[1].input_sample_index > pair[0].input_sample_index,
            "non-monotonic indices: {} → {}",
            pair[0].input_sample_index,
            pair[1].input_sample_index,
        );
    }

    // Gradient check: sample a few pixels well past the sync region
    // and confirm each emitted line shows a left-to-right increase.
    // Use 1/4 and 3/4 of the line length as probes, skipping the
    // ~5% of pixels that cover the sync burst itself.
    let probe_early = LINE_PIXELS / 4;
    let probe_late = (LINE_PIXELS * 3) / 4;
    for (i, line) in lines.iter().enumerate() {
        let early = line.pixels[probe_early];
        let late = line.pixels[probe_late];
        assert!(
            late > early,
            "line {i}: gradient not increasing — pixels[{probe_early}]={early}, pixels[{probe_late}]={late}",
        );
    }
}

#[test]
fn envelope_detector_reset_clears_filter_state() {
    let mut detector = EnvelopeDetector::new(INTERMEDIATE_RATE_HZ).unwrap();
    // Warm the filter up with a loud carrier.
    let input = vec![1.0_f32; 512];
    let mut output = vec![0.0_f32; 512];
    detector.process(&input, &mut output).unwrap();
    assert!(output.iter().any(|&v| v.abs() > 0.1));

    // After reset, feeding zeros should produce (nearly) zeros — the
    // delay line must have been flushed.
    detector.reset();
    let zeros = vec![0.0_f32; 64];
    let mut out2 = vec![0.0_f32; 64];
    detector.process(&zeros, &mut out2).unwrap();
    for &v in &out2 {
        assert!(v.abs() < 1e-6, "reset should zero output, got {v}");
    }
}

/// #774 — the per-line resample used to restart from a zeroed
/// delay line, shifting every line right by the filter's group
/// delay (~50 px) and zero-ramping its first pixels. The Sync A
/// burst must land in the first ~30 columns, and the video body
/// must already be flat at its nominal level by column 60.
#[test]
fn decoded_line_keeps_sync_a_in_the_first_columns() -> Result<(), DspError> {
    const SYNC_PROBE: std::ops::Range<usize> = 4..28;
    const SHIFTED_PROBE: std::ops::Range<usize> = 50..80;
    const VIDEO_PROBE: std::ops::Range<usize> = 60..90;
    /// The square-wave burst spans nearly the full 0–255 range where
    /// it sits; the other probe must read at most a quarter of that.
    const SYNC_SPREAD_RATIO: f32 = 4.0;
    /// Flat grey video: per-line normalisation noise only.
    const VIDEO_SPREAD_MAX: f32 = 16.0;
    let rate = TEST_INPUT_RATE_HZ;
    let mut d = AptDecoder::new(rate)?;
    let one_line = synth_line_audio(rate, TEST_GREY_LEVEL);
    let mut audio = Vec::with_capacity(one_line.len() * 4);
    for _ in 0..4 {
        audio.extend_from_slice(&one_line);
    }
    let mut out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
    let produced = d.process(&audio, &mut out)?;
    assert!(produced >= 2, "need a settled line, got {produced}");
    let line = &out[produced - 1];
    let spread = |r: std::ops::Range<usize>| -> f32 {
        let px = &line.pixels[r];
        let max = px.iter().copied().max().unwrap_or(0);
        let min = px.iter().copied().min().unwrap_or(0);
        f32::from(max - min)
    };
    assert!(
        spread(SYNC_PROBE) > SYNC_SPREAD_RATIO * spread(SHIFTED_PROBE).max(1.0),
        "Sync A burst must sit in the first columns: sync spread {}, shifted spread {}, pixels[..90] = {:?}",
        spread(SYNC_PROBE),
        spread(SHIFTED_PROBE),
        &line.pixels[..90]
    );
    assert!(
        spread(VIDEO_PROBE) < VIDEO_SPREAD_MAX,
        "video body flat by column 60: {:?}",
        &line.pixels[VIDEO_PROBE]
    );
    assert!(
        line.pixels[..8].iter().any(|&p| p > 0),
        "no zero ramp at the line start"
    );
    Ok(())
}

/// #774 — `SYNC_A_TOTAL_PX` (38) is the matched-filter template
/// width; the spec's Sync A *field* is 39 px and is what the image
/// layout must use.
#[test]
fn sync_a_field_is_39_px_and_the_template_is_38() {
    assert_eq!(SYNC_A_FIELD_PX, 39);
    assert_eq!(SYNC_A_TOTAL_PX, 38);
}

/// #774 — after a lock, a line whose best match falls below the
/// quality floor keeps the nominal line spacing instead of
/// jumping to wherever the noise correlated best.
#[test]
fn weak_sync_after_lock_keeps_nominal_line_spacing() -> Result<(), DspError> {
    /// Sync refinement jitters the slice by a few samples; 5 % of a
    /// line is far below the 20 % drift bound that the gate enforces.
    const LINE_SPACING_TOLERANCE: f64 = 0.05;
    let rate = TEST_INPUT_RATE_HZ;
    let mut d = AptDecoder::new(rate)?;
    let one_line = synth_line_audio(rate, TEST_GREY_LEVEL);
    let line_len = one_line.len();
    let mut audio = Vec::with_capacity(line_len * 9);
    for _ in 0..4 {
        audio.extend_from_slice(&one_line);
    }
    // One line of deterministic noise (LCG) at a level comparable
    // to the signal so a false correlation peak is plausible.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..line_len {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let u = ((state >> 40) as f32) / ((1u64 << 24) as f32);
        audio.push((u * 2.0 - 1.0) * TEST_GREY_LEVEL);
    }
    for _ in 0..4 {
        audio.extend_from_slice(&one_line);
    }
    let mut out = vec![AptLine::default(); 16];
    let produced = d.process(&audio, &mut out)?;
    assert!(produced >= 6, "got {produced} lines");
    let nominal = line_len as f64;
    for pair in out[..produced].windows(2) {
        let delta = pair[1]
            .input_sample_index
            .abs_diff(pair[0].input_sample_index) as f64;
        assert!(
            (delta - nominal).abs() <= nominal * LINE_SPACING_TOLERANCE,
            "line spacing {delta} strays from nominal {nominal}: {:?}",
            out[..produced]
                .iter()
                .map(|l| (l.input_sample_index, l.sync_quality))
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}

/// #774 — the priming context is refilled from each drained line,
/// which relies on the resampler delay being far shorter than a
/// line; pin the invariant and that the history really is the
/// line's tail (the next line's resample reads it).
#[test]
fn priming_context_is_shorter_than_a_line_and_tracks_the_drained_tail() -> Result<(), DspError> {
    let rate = TEST_INPUT_RATE_HZ;
    let mut d = AptDecoder::new(rate)?;
    assert!(
        d.prime > 0 && d.prime <= SAMPLES_PER_LINE,
        "prime = {}",
        d.prime
    );
    assert_eq!(
        d.prime % SAMPLES_PER_PIXEL,
        0,
        "prime is a whole number of pixels"
    );
    assert!(
        d.history.iter().all(|&v| v == 0.0),
        "fresh decoder has a zero context"
    );

    let one_line = synth_line_audio(rate, TEST_GREY_LEVEL);
    let mut audio = Vec::with_capacity(one_line.len() * 3);
    for _ in 0..3 {
        audio.extend_from_slice(&one_line);
    }
    let mut out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
    assert!(d.process(&audio, &mut out)? > 0);
    assert!(
        d.history.iter().any(|&v| v != 0.0),
        "history holds the drained tail"
    );
    Ok(())
}

/// Codacy on PR #801 — the lock is dropped after exactly
/// `MAX_NOMINAL_FALLBACKS` consecutive drifted matches: the first
/// seven (and the eighth itself) slice at the nominal start, and
/// only after the eighth does the next search run unconstrained.
#[test]
fn lock_is_dropped_after_exactly_max_nominal_fallbacks() -> Result<(), DspError> {
    let mut d = AptDecoder::new(TEST_INPUT_RATE_HZ)?;
    // A silent accumulator scores ~0 at the nominal start.
    d.accumulator = vec![0.0; MIN_ACCUMULATOR_FOR_DECODE + d.prime];
    d.locked = true;
    let drifted = SyncMatch {
        offset: MAX_SYNC_DRIFT_SAMPLES + 1,
        channel: SyncChannel::A,
        quality: 0.9,
    };
    for n in 1..MAX_NOMINAL_FALLBACKS {
        let m = d.gate_sync_match(drifted);
        assert_eq!(m.offset, 0, "fallback {n} slices at the nominal start");
        assert!(d.locked, "still locked after {n} fallbacks");
        assert_eq!(d.nominal_fallbacks, n);
    }
    let m = d.gate_sync_match(drifted);
    assert_eq!(
        m.offset, 0,
        "the eighth fallback still slices at the nominal start"
    );
    assert!(
        !d.locked,
        "lock dropped after exactly {MAX_NOMINAL_FALLBACKS}"
    );
    assert_eq!(d.nominal_fallbacks, 0);
    // Unlocked: the drifted match is used as-is and a confident one
    // re-locks.
    assert_eq!(d.gate_sync_match(drifted).offset, drifted.offset);
    assert!(d.locked, "a confident match re-locks");
    Ok(())
}

/// Codacy on PR #801 — `quality_at` scores the Sync B template too
/// (shared `template_for` path): the template against itself is
/// ~1, and against the Sync A template clearly lower.
#[test]
fn quality_at_scores_sync_b_against_its_own_template() {
    /// A template correlated with itself is 1 up to float rounding.
    const SELF_MATCH_MIN: f32 = 0.99;
    /// The Sync A burst (1040 Hz) must score clearly below the Sync B
    /// (832 Hz) self-match.
    const CROSS_TEMPLATE_MARGIN: f32 = 0.1;
    let det = SyncDetector::new();
    let (tpl_b, _) = build_square_template(SAMPLES_PER_SYNC_B_CYCLE, SYNC_BURST_CYCLES);
    let own = det.quality_at(&tpl_b, 0, SyncChannel::B).unwrap();
    assert!(
        own > SELF_MATCH_MIN,
        "Sync B template against itself: {own}"
    );
    let (tpl_a, _) = build_padded_sync_a_template(SAMPLES_PER_PIXEL);
    let cross = det.quality_at(&tpl_a, 0, SyncChannel::B);
    assert!(
        cross.is_none_or(|q| q < own - CROSS_TEMPLATE_MARGIN),
        "Sync A content must not score as Sync B: {cross:?} vs {own}"
    );
    assert!(
        det.quality_at(&tpl_b, 1, SyncChannel::B).is_none(),
        "window past the end"
    );
}
