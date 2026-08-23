use super::*;
use rustfft::FftPlanner;

/// Minimum energy ratio for NR tone preservation test.
const MIN_ENERGY_RATIO: f32 = 0.05;

// Squelch dB regression constants: amplitude 0.1 → -20 dBFS.
const SQUELCH_REG_AMPLITUDE: f32 = 0.1;
const SQUELCH_REG_BLOCK_LEN: usize = 100;
const SQUELCH_REG_CLOSE_DB: f32 = -15.0;
const SQUELCH_REG_OPEN_DB: f32 = -25.0;

// --- Envelope test constants ---
//
// Canonical AF rate for envelope timing assertions. All block-
// size and convergence-sample constants below are derived from
// this rate paired with `SQUELCH_ATTACK_SECONDS` /
// `SQUELCH_RELEASE_SECONDS` — keeping them centralized makes it
// obvious which value to update if the time constants change.
const ENV_TEST_SAMPLE_RATE_HZ: f32 = 48_000.0;
/// Short block length used for "fresh envelope stays muted" and
/// "tail preserved across close" assertions — ~2 ms at 48 kHz.
const ENV_SHORT_BLOCK_SAMPLES: usize = 100;
/// Long block length used to let the attack envelope converge
/// fully before the convergence-sample assertion — 20 ms at
/// 48 kHz covers ~10 attack time constants.
const ENV_LONG_BLOCK_SAMPLES: usize = 1_000;
/// Index at which the attack envelope is expected to have
/// converged to within ~1% of the target. 500 samples at
/// 48 kHz = 10 ms, which is 5 attack time constants
/// (`SQUELCH_ATTACK_SECONDS = 0.002 s`) → `exp(-5) ≈ 0.0067`
/// residual, well under the 0.99 gain assertion.
const ENV_CONVERGENCE_SAMPLE_IDX: usize = 500;
/// Numerical tolerance for "silenced" assertions. Envelope
/// gain starts at literal 0.0 so output is exactly 0.0 by
/// construction; the epsilon is pro-forma against future
/// float-math drift.
const ENV_SILENCE_EPSILON: f32 = 1e-6;
/// Stereo sample amplitude used in envelope tests — arbitrary
/// unit value so gain multiplication reads directly as the
/// envelope state.
const ENV_STEREO_SAMPLE_AMP: f32 = 1.0;

// --- Power Squelch tests ---

#[test]
fn test_squelch_opens_on_strong_signal() {
    let mut squelch = PowerSquelch::new(-30.0);
    let input = vec![Complex::new(1.0, 0.0); 100];
    let mut output = vec![Complex::default(); 100];
    squelch.process(&input, &mut output).unwrap();
    assert!(squelch.is_open(), "strong signal should open squelch");
    assert!(output[0].re > 0.0, "output should not be zeroed");
}

/// #734 — with IQ muting disabled the gate state is still tracked
/// but the block passes through, so a downstream stage can keep an
/// ungated copy and apply the mute itself.
#[test]
fn power_squelch_can_gate_without_muting() {
    const HIGH_THRESHOLD_DB: f32 = 10.0;
    const WEAK_AMPLITUDE: f32 = 0.001;
    let mut sq = PowerSquelch::new(HIGH_THRESHOLD_DB);
    sq.set_mute_closed(false);
    let input = vec![Complex::new(WEAK_AMPLITUDE, 0.0); 64];
    let mut output = vec![Complex::default(); 64];
    sq.process(&input, &mut output).unwrap();
    assert!(!sq.is_open(), "gate must still close on a weak block");
    assert_eq!(output, input, "closed gate must pass IQ through unmuted");
}

#[test]
fn test_squelch_closes_on_weak_signal() {
    let mut squelch = PowerSquelch::new(10.0); // very high threshold
    let input = vec![Complex::new(0.001, 0.0); 100];
    let mut output = vec![Complex::default(); 100];
    squelch.process(&input, &mut output).unwrap();
    assert!(!squelch.is_open(), "weak signal should close squelch");
    assert!(
        output[0].re.abs() < 1e-10,
        "output should be zeroed when squelch closed"
    );
}

/// Block size used by the #348 regression test. Arbitrary
/// small value — any power of two ≥ 16 produces the same
/// qualitative behavior; 128 is close to typical live DSP
/// audio block sizes.
const STUCK_RECOVERY_BLOCK_LEN: usize = 128;

/// Number of post-settle blocks to drive through a
/// persistent strong carrier to exercise the slow-creep
/// recovery path. Sized so the EMA climbs measurably
/// (~35 dB at `NOISE_FLOOR_STUCK_RECOVERY_ALPHA = 0.002`)
/// while keeping the test fast.
const STUCK_RECOVERY_BLOCKS: usize = 500;

/// Minimum noise-floor climb (dB) that the post-settle
/// creep path must produce over `STUCK_RECOVERY_BLOCKS` of
/// persistent signal. Picked as a conservative floor under
/// the empirical ≈ 35 dB climb so minor tuning of the
/// recovery alpha doesn't rewrite the test.
const STUCK_RECOVERY_MIN_CLIMB_DB: f32 = 10.0;

/// Signal amplitude used as the "persistent carrier" in the
/// stuck-open scenario. 0.1 → -20 dB, comfortably above any
/// reasonable noise floor.
const STUCK_RECOVERY_CARRIER_AMPLITUDE: f32 = 0.1;

/// Signal amplitude used as "silence" in the recovery-then-
/// close assertion. Well below the floor after recovery so
/// the EMA clearly drops and the gate closes.
const STUCK_RECOVERY_SILENCE_AMPLITUDE: f32 = 0.000_01;

/// Number of silence blocks to drive after the creep window
/// to verify the gate closes. Order-of-magnitude matches
/// `NOISE_FLOOR_SETTLE_BLOCKS` so we're not timing-sensitive.
const STUCK_RECOVERY_SILENCE_BLOCKS: usize = 50;

/// Manual squelch threshold the stuck-recovery test uses
/// when constructing `PowerSquelch::new`. The test switches
/// to auto-squelch immediately so the manual value is
/// irrelevant to the behavior under test — kept as a
/// constant to avoid a bare `-60.0` in the body.
const STUCK_RECOVERY_IDLE_LEVEL_DB: f32 = -60.0;

#[test]
fn test_auto_squelch_recovers_from_overshoot_settle() {
    // Regression for issue #348.
    //
    // Scenario: user enables auto-squelch while a strong
    // carrier is on the channel. The 50-block settle window
    // pulls `noise_floor_db` UP to the carrier level, so
    // post-settle the gate correctly reports open. Before
    // the slow-creep recovery fix, `noise_floor_db` then
    // locked at that elevated value because the post-settle
    // update rule only fired when the gate was closed OR
    // the measurement was within the close margin — neither
    // is true for a persistent carrier above the close
    // margin. The gate stayed open forever.
    //
    // This test: enable auto-squelch with strong signal
    // throughout the settle window, then keep the same
    // signal running for enough post-settle blocks to
    // exercise the slow-creep path. Assert `noise_floor_db`
    // visibly creeps toward the signal level — proving the
    // recovery path is active — and that after enough time
    // it's climbed far enough that a subsequent drop below
    // the original floor closes the gate.
    let mut squelch = PowerSquelch::new(STUCK_RECOVERY_IDLE_LEVEL_DB);
    squelch.set_auto_squelch(true);

    let strong =
        vec![Complex::new(STUCK_RECOVERY_CARRIER_AMPLITUDE, 0.0); STUCK_RECOVERY_BLOCK_LEN];
    let mut out = vec![Complex::default(); STUCK_RECOVERY_BLOCK_LEN];

    // Drive the settle window; capture the post-settle floor.
    for _ in 0..NOISE_FLOOR_SETTLE_BLOCKS {
        squelch.process(&strong, &mut out).unwrap();
    }
    let floor_after_settle = squelch.noise_floor_db();
    assert!(
        squelch.is_open(),
        "strong signal must open the gate during / after settle"
    );

    // Now STUCK_RECOVERY_BLOCKS of the same strong signal.
    // With the fix the slow-creep alpha pulls the floor
    // upward toward the signal level; without it the floor
    // stays frozen at the settle value forever.
    for _ in 0..STUCK_RECOVERY_BLOCKS {
        squelch.process(&strong, &mut out).unwrap();
    }
    let floor_after_creep = squelch.noise_floor_db();

    let climb_db = floor_after_creep - floor_after_settle;
    assert!(
        climb_db > STUCK_RECOVERY_MIN_CLIMB_DB,
        "stuck-recovery must pull noise_floor up under persistent signal; \
         observed climb of only {climb_db:.2} dB \
         ({floor_after_settle:.2} dB → {floor_after_creep:.2} dB) (#348)"
    );

    // Drop the signal well below the new floor. With the
    // fix the gate closes promptly because the floor has
    // already climbed (so the close-margin gate of the EMA
    // update fires + fast track-down), returning the squelch
    // to its normal closed-on-silence behavior.
    let silence =
        vec![Complex::new(STUCK_RECOVERY_SILENCE_AMPLITUDE, 0.0); STUCK_RECOVERY_BLOCK_LEN];
    for _ in 0..STUCK_RECOVERY_SILENCE_BLOCKS {
        squelch.process(&silence, &mut out).unwrap();
    }
    assert!(
        !squelch.is_open(),
        "after recovery + silence the gate must close"
    );
}

#[test]
fn test_squelch_empty_input() {
    let mut squelch = PowerSquelch::new(-50.0);
    let input: &[Complex] = &[];
    let mut output: Vec<Complex> = vec![];
    let count = squelch.process(input, &mut output).unwrap();
    assert_eq!(count, 0);
    assert!(!squelch.is_open());
}

#[test]
fn test_squelch_db_scale_regression() {
    // Pin the 20*log10(amplitude) scale: amplitude 0.1 → -20 dBFS.
    // A threshold at -15 dB should close, -25 dB should open.
    let input = vec![Complex::new(SQUELCH_REG_AMPLITUDE, 0.0); SQUELCH_REG_BLOCK_LEN];

    let mut squelch_close = PowerSquelch::new(SQUELCH_REG_CLOSE_DB);
    let mut output = vec![Complex::default(); SQUELCH_REG_BLOCK_LEN];
    squelch_close.process(&input, &mut output).unwrap();
    assert!(
        !squelch_close.is_open(),
        "amplitude 0.1 (-20 dB) should be below -15 dB threshold"
    );

    let mut squelch_open = PowerSquelch::new(SQUELCH_REG_OPEN_DB);
    squelch_open.process(&input, &mut output).unwrap();
    assert!(
        squelch_open.is_open(),
        "amplitude 0.1 (-20 dB) should be above -25 dB threshold"
    );
}

// --- Squelch audio envelope tests (#331) ---

/// Fresh envelope starts muted: gate target 0.0, gain 0.0.
/// A stereo buffer processed without any `set_gate_open` call
/// should stay silent.
#[test]
fn squelch_audio_envelope_starts_muted() {
    use sdr_types::Stereo;
    let mut env = SquelchAudioEnvelope::new(ENV_TEST_SAMPLE_RATE_HZ).unwrap();
    // Use a non-zero input so the "output is 0" assertion is
    // actually exercising the envelope (an all-zero input
    // would pass even if the envelope were broken).
    let mut buf = vec![Stereo { l: 0.5, r: 0.5 }; ENV_SHORT_BLOCK_SAMPLES];
    env.process_stereo(&mut buf);
    for (i, s) in buf.iter().enumerate() {
        assert!(
            s.l.abs() < ENV_SILENCE_EPSILON,
            "sample {i} L not silenced: {}",
            s.l
        );
        assert!(
            s.r.abs() < ENV_SILENCE_EPSILON,
            "sample {i} R not silenced: {}",
            s.r
        );
    }
}

/// Attack ramp: on gate open, the per-sample gain rises
/// smoothly toward 1.0. After several ms the envelope has
/// converged and the output matches the input.
#[test]
fn squelch_audio_envelope_ramps_up_on_gate_open() {
    use sdr_types::Stereo;
    let mut env = SquelchAudioEnvelope::new(ENV_TEST_SAMPLE_RATE_HZ).unwrap();
    env.set_gate_open(true);
    let mut buf = vec![
        Stereo {
            l: ENV_STEREO_SAMPLE_AMP,
            r: ENV_STEREO_SAMPLE_AMP
        };
        ENV_LONG_BLOCK_SAMPLES
    ];
    env.process_stereo(&mut buf);
    // First sample: partially ramped up. Not the full input
    // (that would be a hard gate), not zero (that would be
    // stuck muted).
    assert!(buf[0].l > 0.0 && buf[0].l < 0.5);
    // By `ENV_CONVERGENCE_SAMPLE_IDX` (5 attack time constants
    // at 48 kHz) the envelope is within ~1% of the target.
    assert!(
        buf[ENV_CONVERGENCE_SAMPLE_IDX].l > 0.99,
        "envelope should converge by sample {ENV_CONVERGENCE_SAMPLE_IDX}, got {}",
        buf[ENV_CONVERGENCE_SAMPLE_IDX].l
    );
}

/// Release ramp on gate close: envelope decays smoothly
/// toward 0 across ~5 ms. Tail of the previous block's audio
/// is NOT clipped on the first sample after close.
#[test]
fn squelch_audio_envelope_preserves_tail_on_close() {
    use sdr_types::Stereo;
    let mut env = SquelchAudioEnvelope::new(ENV_TEST_SAMPLE_RATE_HZ).unwrap();
    // Open and converge.
    env.set_gate_open(true);
    let mut ramp = vec![
        Stereo {
            l: ENV_STEREO_SAMPLE_AMP,
            r: ENV_STEREO_SAMPLE_AMP
        };
        ENV_LONG_BLOCK_SAMPLES
    ];
    env.process_stereo(&mut ramp);

    // Close and feed another block — first sample should
    // be close to full amplitude (tail preserved), not 0.
    env.set_gate_open(false);
    let mut buf = vec![
        Stereo {
            l: ENV_STEREO_SAMPLE_AMP,
            r: ENV_STEREO_SAMPLE_AMP
        };
        ENV_SHORT_BLOCK_SAMPLES
    ];
    env.process_stereo(&mut buf);
    assert!(
        buf[0].l > 0.9,
        "first post-close sample should preserve the tail, got {}",
        buf[0].l
    );
}

/// Reset snaps back to the closed-gate state — used by
/// `RadioModule` on demod-mode change to prevent cross-
/// pipeline gain bleed.
#[test]
fn squelch_audio_envelope_reset_clears_gain() {
    use sdr_types::Stereo;
    let mut env = SquelchAudioEnvelope::new(ENV_TEST_SAMPLE_RATE_HZ).unwrap();
    env.set_gate_open(true);
    let mut buf = vec![
        Stereo {
            l: ENV_STEREO_SAMPLE_AMP,
            r: ENV_STEREO_SAMPLE_AMP
        };
        ENV_LONG_BLOCK_SAMPLES
    ];
    env.process_stereo(&mut buf);

    env.reset();
    // First post-reset block should be silent (gain back to
    // 0, target back to 0).
    let mut buf = vec![
        Stereo {
            l: ENV_STEREO_SAMPLE_AMP,
            r: ENV_STEREO_SAMPLE_AMP
        };
        ENV_SHORT_BLOCK_SAMPLES
    ];
    env.process_stereo(&mut buf);
    for s in &buf {
        assert!(s.l.abs() < ENV_SILENCE_EPSILON);
    }
}

/// `envelope_coefficient` pathological-input guards —
/// non-finite, zero, or negative rate / tau fall back to
/// instant-transition `1.0` so misconfiguration degrades to
/// the hard-gate behavior rather than panicking on divide-
/// by-zero OR poisoning the envelope with NaN / Inf.
#[test]
fn envelope_coefficient_handles_pathological_inputs() {
    let coeff = envelope_coefficient(SQUELCH_ATTACK_SECONDS, ENV_TEST_SAMPLE_RATE_HZ);
    assert!(coeff > 0.0 && coeff < 1.0);
    assert_eq!(envelope_coefficient(SQUELCH_ATTACK_SECONDS, 0.0), 1.0);
    assert_eq!(
        envelope_coefficient(SQUELCH_ATTACK_SECONDS, -ENV_TEST_SAMPLE_RATE_HZ),
        1.0
    );
    assert_eq!(envelope_coefficient(0.0, ENV_TEST_SAMPLE_RATE_HZ), 1.0);
    assert_eq!(
        envelope_coefficient(-SQUELCH_ATTACK_SECONDS, ENV_TEST_SAMPLE_RATE_HZ),
        1.0
    );
    // Non-finite guards: NaN and ±Inf on either input must
    // fall back to 1.0, not propagate into downstream
    // `envelope_gain` math where they'd poison every block.
    assert_eq!(envelope_coefficient(f32::NAN, ENV_TEST_SAMPLE_RATE_HZ), 1.0);
    assert_eq!(envelope_coefficient(SQUELCH_ATTACK_SECONDS, f32::NAN), 1.0);
    assert_eq!(
        envelope_coefficient(f32::INFINITY, ENV_TEST_SAMPLE_RATE_HZ),
        1.0
    );
    assert_eq!(
        envelope_coefficient(SQUELCH_ATTACK_SECONDS, f32::INFINITY),
        1.0
    );
}

/// `SquelchAudioEnvelope::new` must reject non-finite / non-
/// positive sample rates instead of silently degrading to a
/// hard gate — that's the exact pop behavior the type was
/// added to eliminate, so a misconfigured caller should get
/// a loud error rather than a silent regression.
#[test]
fn squelch_audio_envelope_new_rejects_invalid_sample_rates() {
    assert!(matches!(
        SquelchAudioEnvelope::new(0.0),
        Err(DspError::InvalidParameter(_))
    ));
    assert!(matches!(
        SquelchAudioEnvelope::new(-ENV_TEST_SAMPLE_RATE_HZ),
        Err(DspError::InvalidParameter(_))
    ));
    assert!(matches!(
        SquelchAudioEnvelope::new(f32::NAN),
        Err(DspError::InvalidParameter(_))
    ));
    assert!(matches!(
        SquelchAudioEnvelope::new(f32::INFINITY),
        Err(DspError::InvalidParameter(_))
    ));
    assert!(matches!(
        SquelchAudioEnvelope::new(f32::NEG_INFINITY),
        Err(DspError::InvalidParameter(_))
    ));
    // Positive rate accepted.
    assert!(SquelchAudioEnvelope::new(ENV_TEST_SAMPLE_RATE_HZ).is_ok());
}

/// `set_sample_rate` applies the same validation as `new`.
/// On rejection, the envelope's coefficients AND gain state
/// must be left untouched so a bad rate update doesn't
/// corrupt a working envelope. Warm up the gain to a non-
/// trivial value before the rejection calls so the gain-
/// preservation assertion actually exercises the contract
/// (a fresh envelope's gain is 0.0, which would trivially
/// equal itself after any no-op).
#[test]
fn squelch_audio_envelope_set_sample_rate_rejects_invalid_and_preserves_state() {
    use sdr_types::Stereo;
    let mut env = SquelchAudioEnvelope::new(ENV_TEST_SAMPLE_RATE_HZ).unwrap();
    // Open the gate and run one block so envelope_gain lands
    // somewhere nontrivially > 0.0 but < 1.0 — this is the
    // mid-transition state where a state-clobbering bug in
    // set_sample_rate would be most visible.
    env.set_gate_open(true);
    let mut warmup = vec![
        Stereo {
            l: ENV_STEREO_SAMPLE_AMP,
            r: ENV_STEREO_SAMPLE_AMP,
        };
        ENV_SHORT_BLOCK_SAMPLES
    ];
    env.process_stereo(&mut warmup);
    let gain_before = env.envelope_gain;
    let attack_before = env.attack_coeff;
    let release_before = env.release_coeff;

    // All five invalid cases must produce `InvalidParameter`.
    // ±Inf isn't trivially rejected by the `<= 0.0` clause
    // alone — it's only caught by the `is_finite` guard, so
    // including both directions pins the guard explicitly.
    assert!(matches!(
        env.set_sample_rate(f32::NAN),
        Err(DspError::InvalidParameter(_))
    ));
    assert!(matches!(
        env.set_sample_rate(f32::INFINITY),
        Err(DspError::InvalidParameter(_))
    ));
    assert!(matches!(
        env.set_sample_rate(f32::NEG_INFINITY),
        Err(DspError::InvalidParameter(_))
    ));
    assert!(matches!(
        env.set_sample_rate(0.0),
        Err(DspError::InvalidParameter(_))
    ));
    assert!(matches!(
        env.set_sample_rate(-ENV_TEST_SAMPLE_RATE_HZ),
        Err(DspError::InvalidParameter(_))
    ));

    // Coefficients AND gain must all still match the pre-
    // rejection snapshot — any of these drifting would mean
    // a rejected call silently mutated state.
    assert_eq!(env.attack_coeff, attack_before);
    assert_eq!(env.release_coeff, release_before);
    assert_eq!(env.envelope_gain, gain_before);

    // Valid update applies cleanly.
    assert!(env.set_sample_rate(2.0 * ENV_TEST_SAMPLE_RATE_HZ).is_ok());
}

// --- Noise Blanker tests ---

#[test]
fn test_blanker_new_invalid() {
    assert!(NoiseBlanker::new(0.0, 5.0).is_err());
    assert!(NoiseBlanker::new(1.0, 5.0).is_err());
    assert!(NoiseBlanker::new(0.1, 0.5).is_err()); // level < 1.0
    assert!(NoiseBlanker::new(f32::NAN, 5.0).is_err());
    assert!(NoiseBlanker::new(0.1, f32::NAN).is_err());
}

#[test]
fn test_blanker_passes_normal_signal() {
    let mut nb = NoiseBlanker::new(0.1, 10.0).unwrap();
    let input = vec![Complex::new(1.0, 0.0); 500];
    let mut output = vec![Complex::default(); 500];
    nb.process(&input, &mut output).unwrap();
    // Normal signal should pass through mostly unchanged
    let last = output[499];
    assert!(last.re > 0.5, "normal signal should pass, got {}", last.re);
}

#[test]
fn test_blanker_attenuates_spike() {
    let mut nb = NoiseBlanker::new(0.01, 3.0).unwrap();
    // Settle the amplitude tracker
    let normal = vec![Complex::new(1.0, 0.0); 1000];
    let mut out = vec![Complex::default(); 1000];
    nb.process(&normal, &mut out).unwrap();
    // Now inject a spike
    let mut spike_input = vec![Complex::new(1.0, 0.0); 100];
    spike_input[50] = Complex::new(100.0, 0.0); // huge spike
    let mut spike_out = vec![Complex::default(); 100];
    nb.process(&spike_input, &mut spike_out).unwrap();
    // The spike should be attenuated
    assert!(
        spike_out[50].re < 50.0,
        "spike should be attenuated, got {}",
        spike_out[50].re
    );
}

#[test]
fn test_blanker_skips_ema_on_zero_samples() {
    let mut nb = NoiseBlanker::new(0.5, 3.0).unwrap();
    // Settle the EMA with a known amplitude.
    let normal = vec![Complex::new(1.0, 0.0); 100];
    let mut out = vec![Complex::default(); 100];
    nb.process(&normal, &mut out).unwrap();
    let amp_before = nb.amp;

    // Feed zero samples — EMA should NOT decay.
    let zeros = vec![Complex::default(); 100];
    nb.process(&zeros, &mut out).unwrap();
    assert!(
        (nb.amp - amp_before).abs() < 1e-6,
        "EMA should not change on zero samples, was {amp_before}, now {}",
        nb.amp
    );
}

#[test]
fn test_blanker_reset() {
    let mut nb = NoiseBlanker::new(0.1, 5.0).unwrap();
    let input = vec![Complex::new(10.0, 0.0); 100];
    let mut output = vec![Complex::default(); 100];
    nb.process(&input, &mut output).unwrap();
    nb.reset();
    assert!(
        (nb.amp - 1.0).abs() < 1e-6,
        "after reset, amp should be 1.0"
    );
}

// --- FM IF NR tests ---

#[test]
fn test_fm_if_nr_preserves_tone() {
    use core::f32::consts::PI;

    // Generate a pure tone at bin 8 of a 256-point FFT — it should survive NR.
    let fft_size = 256;
    let mut nr = FmIfNoiseReduction::with_fft_size(fft_size).unwrap();
    let tone_bin = 8;
    let input: Vec<Complex> = (0..fft_size)
        .map(|i| {
            let phase = 2.0 * PI * (tone_bin as f32) * (i as f32) / (fft_size as f32);
            Complex::new(phase.cos(), phase.sin())
        })
        .collect();
    let mut output = vec![Complex::default(); fft_size];
    let count = nr.process(&input, &mut output).unwrap();
    assert_eq!(count, fft_size);

    // Verify the dominant output bin matches the input tone bin.
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);
    let mut spectrum: Vec<RustFftComplex<f32>> = output
        .iter()
        .map(|s| RustFftComplex::new(s.re, s.im))
        .collect();
    fft.process(&mut spectrum);
    let dominant_bin = spectrum
        .iter()
        .enumerate()
        .max_by(|a, b| {
            let ma = a.1.re * a.1.re + a.1.im * a.1.im;
            let mb = b.1.re * b.1.re + b.1.im * b.1.im;
            ma.partial_cmp(&mb).unwrap()
        })
        .map_or(0, |(i, _)| i);
    assert_eq!(
        dominant_bin, tone_bin,
        "recovered dominant bin should match tone_bin"
    );

    // Energy should be above a minimum floor (Nuttall window + single-bin
    // selection reduces passthrough to ~10-15%).
    let energy: f32 = output.iter().map(|s| s.re * s.re + s.im * s.im).sum();
    let input_energy: f32 = input.iter().map(|s| s.re * s.re + s.im * s.im).sum();
    assert!(
        energy > input_energy * MIN_ENERGY_RATIO,
        "tone energy ratio {} below MIN_ENERGY_RATIO {MIN_ENERGY_RATIO}",
        energy / input_energy
    );
}

#[test]
fn test_fm_if_nr_reduces_noise() {
    // A tone + broadband noise: output energy should be less than input energy
    // because NR zeroes the noise bins.
    let fft_size = 256;
    let mut nr = FmIfNoiseReduction::with_fft_size(fft_size).unwrap();

    // Deterministic "noise": many tones across the spectrum.
    let input: Vec<Complex> = (0..fft_size)
        .map(|i| {
            let tone = (2.0 * core::f32::consts::PI * 8.0 * (i as f32) / fft_size as f32).cos();
            // Add energy at many other bins (pseudo-noise).
            let noise = (0.3 * (i as f32 * 1.7).sin())
                + (0.2 * (i as f32 * 3.1).cos())
                + (0.15 * (i as f32 * 7.3).sin());
            Complex::new(tone + noise, 0.0)
        })
        .collect();
    let mut output = vec![Complex::default(); fft_size];
    nr.process(&input, &mut output).unwrap();

    let input_energy: f32 = input.iter().map(|s| s.re * s.re + s.im * s.im).sum();
    let output_energy: f32 = output.iter().map(|s| s.re * s.re + s.im * s.im).sum();
    assert!(
        output_energy < input_energy * 0.9,
        "NR should reduce broadband energy, ratio = {}",
        output_energy / input_energy
    );
}

#[test]
fn test_fm_if_nr_invalid_size() {
    assert!(FmIfNoiseReduction::with_fft_size(0).is_err());
}

#[test]
fn test_fm_if_nr_buffer_too_small() {
    let mut nr = FmIfNoiseReduction::new().unwrap();
    let input = [Complex::default(); 300];
    let mut output = [Complex::default(); 100];
    assert!(nr.process(&input, &mut output).is_err());
}

#[test]
fn test_buffer_too_small() {
    let mut squelch = PowerSquelch::new(-50.0);
    let input = [Complex::default(); 10];
    let mut output = [Complex::default(); 5];
    assert!(squelch.process(&input, &mut output).is_err());
}

// --- Auto-squelch tests ---

// Auto-squelch test constants.
const AUTO_SETTLE_ITERS: usize = 200;
const TEST_BLOCK_LEN: usize = 100;
const NOISE_AMP: f32 = 0.001;
const STRONG_AMP: f32 = 1.0;
const BORDERLINE_AMP: f32 = 0.003;

// Manual squelch thresholds used by the rearm tests. The two
// constants are NOT interchangeable — they're deliberately
// picked at different points on the dB scale.
//
/// "Effectively open" manual threshold versus the expected
/// noise amplitude — used by the enabled-rearm test so the
/// process loop always exercises the auto-squelch logic
/// rather than short-circuiting on the manual gate.
const REARM_MANUAL_FLOOR_DB: f32 = -100.0;
/// Manual threshold used specifically by the
/// rearm-while-disabled test. Intentionally sits between
/// `NOISE_AMP` and `NOISE_FLOOR_INITIAL_DB` so
/// "disabled squelch" is unambiguously distinct from
/// "enabled and settled at the floor" — protects the test
/// against accidentally passing if a future edit makes the
/// disabled path mirror the enabled-at-sentinel state.
const REARM_DISABLED_MANUAL_DB: f32 = -50.0;
/// Minimum rise above `NOISE_FLOOR_INITIAL_DB` (dB) that counts
/// as "the EMA converged off the sentinel" — used to confirm
/// the settle loop actually moved the floor before we re-arm.
const REARM_SETTLED_MARGIN_DB: f32 = 1.0;

#[test]
fn test_auto_squelch_tracks_noise_floor() {
    let mut squelch = PowerSquelch::new(-100.0);
    squelch.set_auto_squelch(true);
    assert!(squelch.auto_squelch_enabled());

    // Feed many blocks of low-level noise to settle the noise floor estimate.
    let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
    let mut output = vec![Complex::default(); 100];
    for _ in 0..AUTO_SETTLE_ITERS {
        squelch.process(&noise, &mut output).unwrap();
    }

    // Noise floor should have settled near the noise level (-60 dBFS for 0.001).
    let floor = squelch.noise_floor_db();
    assert!(
        floor > -70.0 && floor < -50.0,
        "noise floor should be near -60 dB, got {floor}"
    );
}

/// #775 — a non-finite sample must not poison the noise-floor
/// tracker: `+inf` pinned `noise_floor_db` at `+inf` (absorbing,
/// gate closed on every frequency until restart) and NaN dragged
/// it to ≈ −757 dB during settling. Such a block closes the gate
/// and is otherwise ignored.
#[test]
fn test_auto_squelch_ignores_non_finite_blocks() {
    let mut squelch = PowerSquelch::new(-100.0);
    squelch.set_auto_squelch(true);
    let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
    let mut output = vec![Complex::default(); TEST_BLOCK_LEN];
    for _ in 0..AUTO_SETTLE_ITERS {
        squelch.process(&noise, &mut output).unwrap();
    }
    let settled = squelch.noise_floor_db();

    for bad in [f32::INFINITY, f32::NAN] {
        let mut block = noise.clone();
        block[TEST_BLOCK_LEN / 2] = Complex::new(bad, 0.0);
        squelch.process(&block, &mut output).unwrap();
        assert!(!squelch.is_open(), "a non-finite block closes the gate");
        assert_eq!(
            squelch.noise_floor_db(),
            settled,
            "noise floor must ignore a block containing {bad}"
        );
        assert!(output.iter().all(|s| s.re == 0.0 && s.im == 0.0));
    }

    // The tracker keeps working afterwards: a strong signal opens.
    let signal = vec![Complex::new(STRONG_AMP, 0.0); TEST_BLOCK_LEN];
    squelch.process(&signal, &mut output).unwrap();
    assert!(
        squelch.is_open(),
        "should open on strong signal after a bad block"
    );
}

#[test]
fn test_auto_squelch_opens_on_signal() {
    let mut squelch = PowerSquelch::new(-100.0);
    squelch.set_auto_squelch(true);

    // Settle noise floor with weak signal.
    let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
    let mut output = vec![Complex::default(); 100];
    for _ in 0..AUTO_SETTLE_ITERS {
        squelch.process(&noise, &mut output).unwrap();
    }
    assert!(!squelch.is_open(), "should be closed on noise-only");

    // Inject a strong signal — should open.
    let signal = vec![Complex::new(STRONG_AMP, 0.0); TEST_BLOCK_LEN];
    squelch.process(&signal, &mut output).unwrap();
    assert!(squelch.is_open(), "should open on strong signal");
}

#[test]
fn test_auto_squelch_hysteresis() {
    let mut squelch = PowerSquelch::new(-100.0);
    squelch.set_auto_squelch(true);

    // Settle noise floor.
    let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
    let mut output = vec![Complex::default(); 100];
    for _ in 0..AUTO_SETTLE_ITERS {
        squelch.process(&noise, &mut output).unwrap();
    }

    // Open squelch with a strong signal.
    let strong = vec![Complex::new(STRONG_AMP, 0.0); TEST_BLOCK_LEN];
    squelch.process(&strong, &mut output).unwrap();
    assert!(squelch.is_open());

    // A borderline signal just above the close margin should stay open
    // (hysteresis: close margin is lower than open margin).
    // Noise floor is ~-60 dB, close margin is +6 dB = -54 dB.
    // Amplitude of 0.003 ≈ -50.5 dB, which is above -54 dB.
    let borderline = vec![Complex::new(BORDERLINE_AMP, 0.0); TEST_BLOCK_LEN];
    squelch.process(&borderline, &mut output).unwrap();
    assert!(
        squelch.is_open(),
        "borderline signal should keep squelch open due to hysteresis"
    );
}

#[test]
fn test_auto_squelch_ignores_manual_level() {
    let mut squelch = PowerSquelch::new(100.0); // impossibly high manual threshold
    squelch.set_auto_squelch(true);

    // Settle noise floor.
    let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
    let mut output = vec![Complex::default(); 100];
    for _ in 0..AUTO_SETTLE_ITERS {
        squelch.process(&noise, &mut output).unwrap();
    }

    // Strong signal should still open despite manual level of 100 dB.
    let strong = vec![Complex::new(STRONG_AMP, 0.0); TEST_BLOCK_LEN];
    squelch.process(&strong, &mut output).unwrap();
    assert!(squelch.is_open(), "auto-squelch should ignore manual level");
}

#[test]
fn test_auto_squelch_disable_reverts_to_manual() {
    let mut squelch = PowerSquelch::new(100.0); // impossibly high manual threshold
    squelch.set_auto_squelch(true);

    // Settle noise floor and open with signal.
    let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
    let mut output = vec![Complex::default(); 100];
    for _ in 0..AUTO_SETTLE_ITERS {
        squelch.process(&noise, &mut output).unwrap();
    }
    let strong = vec![Complex::new(STRONG_AMP, 0.0); TEST_BLOCK_LEN];
    squelch.process(&strong, &mut output).unwrap();
    assert!(squelch.is_open());

    // Disable auto-squelch — should revert to manual 100 dB threshold.
    squelch.set_auto_squelch(false);
    squelch.process(&strong, &mut output).unwrap();
    assert!(
        !squelch.is_open(),
        "with auto-squelch off, manual 100 dB threshold should close squelch"
    );
}

#[test]
fn test_rearm_auto_squelch_resets_floor_without_toggling_enabled() {
    // Repros the core condition of issue #374: auto-squelch
    // settles on band A's floor, we retune to band B, and
    // without a re-arm the stale floor drives wrong open/close
    // decisions. `rearm_auto_squelch` should reset the floor
    // back to the initial sentinel so the next block starts
    // re-converging from scratch — while leaving
    // `auto_squelch_enabled` untouched.
    let mut squelch = PowerSquelch::new(REARM_MANUAL_FLOOR_DB);
    squelch.set_auto_squelch(true);

    // Settle band A's noise floor via repeated blocks.
    let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
    let mut output = vec![Complex::default(); TEST_BLOCK_LEN];
    for _ in 0..AUTO_SETTLE_ITERS {
        squelch.process(&noise, &mut output).unwrap();
    }
    let settled_floor = squelch.noise_floor_db();
    assert!(
        settled_floor > NOISE_FLOOR_INITIAL_DB + REARM_SETTLED_MARGIN_DB,
        "sanity: noise floor should have converged above the sentinel (got {settled_floor})",
    );
    assert!(squelch.auto_squelch_enabled());

    squelch.rearm_auto_squelch();

    // Floor is back to the sentinel; auto-squelch stays on.
    assert!(
        (squelch.noise_floor_db() - NOISE_FLOOR_INITIAL_DB).abs() < f32::EPSILON,
        "rearm should reset noise_floor_db to NOISE_FLOOR_INITIAL_DB (got {})",
        squelch.noise_floor_db()
    );
    assert!(
        squelch.auto_squelch_enabled(),
        "rearm must not flip the enabled state"
    );
}

#[test]
fn test_rearm_auto_squelch_is_no_op_when_disabled() {
    // When auto-squelch is off, the noise_floor_db field is
    // unused (manual threshold drives decisions). Re-arming
    // should not touch the floor or flip any other state
    // either — cheap guard against a future edit making
    // rearm quietly enable auto-squelch behind the user's
    // back.
    //
    // We deliberately drive the floor *off* the sentinel
    // before disabling — if we started the disabled test
    // with the floor still at `NOISE_FLOOR_INITIAL_DB`, a
    // buggy implementation that unconditionally resets to
    // sentinel would false-pass (sentinel → sentinel). The
    // pre-rearm assertion below enforces this precondition.
    let mut squelch = PowerSquelch::new(REARM_DISABLED_MANUAL_DB);
    squelch.set_auto_squelch(true);
    let noise = vec![Complex::new(NOISE_AMP, 0.0); TEST_BLOCK_LEN];
    let mut output = vec![Complex::default(); TEST_BLOCK_LEN];
    for _ in 0..AUTO_SETTLE_ITERS {
        squelch.process(&noise, &mut output).unwrap();
    }
    // `set_auto_squelch(false)` only flips the flag — it does
    // not touch the settled floor estimate, which is exactly
    // the state we want to snapshot for the "rearm is a no-op
    // when disabled" check.
    squelch.set_auto_squelch(false);
    assert!(!squelch.auto_squelch_enabled());
    let floor_before = squelch.noise_floor_db();
    assert!(
        (floor_before - NOISE_FLOOR_INITIAL_DB).abs() > REARM_SETTLED_MARGIN_DB,
        "precondition: floor should differ from the sentinel before the \
         disabled rearm call (got {floor_before}) — without this, a buggy \
         unconditional-reset would false-pass the final assertion"
    );

    squelch.rearm_auto_squelch();

    assert!(
        !squelch.auto_squelch_enabled(),
        "rearm must not enable auto-squelch"
    );
    assert!(
        (squelch.noise_floor_db() - floor_before).abs() < f32::EPSILON,
        "rearm should leave noise_floor_db untouched when disabled"
    );
}

// ---- FM-IF-NR streaming fixture parameters (#773) ----
/// Default block size of `FmIfNoiseReduction::new()`.
const FMIF_FFT: usize = FM_IF_NR_FFT_SIZE;
/// Tone bin (cycles per block) and noise amplitude of the fixture.
const FMIF_TONE_BIN: f32 = 37.0;
const FMIF_NOISE_AMPLITUDE: f32 = 0.3;
/// Whole-stream length for the chunking / small-chunk tests, in blocks.
const FMIF_STREAM_BLOCKS: usize = 12;
/// A chunk size that is not a multiple of the block (exposes the old
/// double-emission) and one smaller than a block (exposes the old
/// passthrough latch).
const FMIF_ODD_CHUNK: usize = 300;
const FMIF_SMALL_CHUNK: usize = 100;
/// Partial remainder appended for the flush test.
const FMIF_FLUSH_TAIL: usize = 100;
/// `flush` emits at most two blocks.
const FMIF_FLUSH_CAPACITY: usize = 2 * FMIF_FFT;
/// Sample-equality tolerance between two runs.
const FMIF_SAMPLE_TOL: f32 = 1e-4;

fn fmif_close(a: Complex, b: Complex) -> bool {
    (a.re - b.re).abs() < FMIF_SAMPLE_TOL && (a.im - b.im).abs() < FMIF_SAMPLE_TOL
}

/// Tone + deterministic noise for FM-IF-NR streaming tests.
fn fmif_test_signal(len: usize) -> Vec<Complex> {
    let mut seed: u32 = 0x1234_5678;
    (0..len)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f32 / 16_777_216.0 - 0.5;
            let theta = 2.0 * core::f32::consts::PI * FMIF_TONE_BIN * i as f32 / FMIF_FFT as f32;
            Complex::new(
                theta.cos() + FMIF_NOISE_AMPLITUDE * noise,
                theta.sin() + FMIF_NOISE_AMPLITUDE * noise,
            )
        })
        .collect()
}

fn run_fmif_chunked(input: &[Complex], chunk: usize) -> Vec<Complex> {
    let mut nr = FmIfNoiseReduction::new().unwrap();
    let mut out = Vec::new();
    for c in input.chunks(chunk) {
        let mut buf = vec![Complex::default(); c.len()];
        let n = nr.process(c, &mut buf).unwrap();
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// #773 — the output stream must be the same sample sequence regardless
/// of how the input is chunked (no duplicated or reordered tails).
#[test]
fn fm_if_nr_output_is_chunking_independent() {
    let input = fmif_test_signal(FMIF_FFT * FMIF_STREAM_BLOCKS);
    let whole = run_fmif_chunked(&input, input.len());
    let odd = run_fmif_chunked(&input, FMIF_ODD_CHUNK);
    let n = whole.len().min(odd.len());
    assert!(
        n >= FMIF_FFT * (FMIF_STREAM_BLOCKS - 2),
        "too little output to compare: {n}"
    );
    for i in 0..n {
        assert!(
            fmif_close(whole[i], odd[i]),
            "sample {i} differs between whole-buffer and {FMIF_ODD_CHUNK}-sample chunking: {:?} vs {:?}",
            whole[i],
            odd[i]
        );
    }
}

/// #773 (CR) — at end of stream, `flush` must emit the buffered partial
/// block so every input sample comes out exactly once, in order.
#[test]
fn fm_if_nr_flush_emits_every_buffered_sample() {
    let input = fmif_test_signal(FMIF_FFT * 4 + FMIF_FLUSH_TAIL);
    let mut nr = FmIfNoiseReduction::new().unwrap();
    let mut out = Vec::new();
    for c in input.chunks(FMIF_ODD_CHUNK) {
        let mut buf = vec![Complex::default(); c.len()];
        let n = nr.process(c, &mut buf).unwrap();
        out.extend_from_slice(&buf[..n]);
    }
    assert!(
        out.len() < input.len(),
        "some samples should still be buffered"
    );
    let mut tail = vec![Complex::default(); FMIF_FLUSH_CAPACITY];
    let n = nr.flush(&mut tail).unwrap();
    out.extend_from_slice(&tail[..n]);
    assert_eq!(
        out.len(),
        input.len(),
        "flush must account for every input sample"
    );
    // Flushing twice emits nothing more.
    assert_eq!(nr.flush(&mut tail).unwrap(), 0);
    // The whole-buffer run (plus flush) must produce the same sequence.
    let mut whole = FmIfNoiseReduction::new().unwrap();
    let mut wbuf = vec![Complex::default(); input.len()];
    let wn = whole.process(&input, &mut wbuf).unwrap();
    let mut wout = wbuf[..wn].to_vec();
    let wf = whole.flush(&mut tail).unwrap();
    wout.extend_from_slice(&tail[..wf]);
    assert_eq!(wout.len(), out.len());
    for i in 0..out.len() {
        assert!(fmif_close(wout[i], out[i]), "sample {i} differs");
    }
}

/// #773 — input chunks smaller than the FFT block must still be
/// processed (not latch into raw passthrough forever).
#[test]
fn fm_if_nr_small_chunks_keep_processing() {
    let input = fmif_test_signal(FMIF_FFT * FMIF_STREAM_BLOCKS);
    let out = run_fmif_chunked(&input, FMIF_SMALL_CHUNK);
    // Allow up to two blocks of latency, but everything else must come out…
    assert!(
        out.len() + FMIF_FLUSH_CAPACITY >= input.len(),
        "emitted only {} of {}",
        out.len(),
        input.len()
    );
    // …and it must be the processed (noise-reduced) stream, not a raw copy.
    let differs = out
        .iter()
        .zip(&input)
        .skip(2 * FMIF_FFT)
        .filter(|(o, i)| !fmif_close(**o, **i))
        .count();
    assert!(
        differs > out.len() / 2,
        "output looks like raw passthrough (only {differs} samples differ)"
    );
}

/// #738 — the release ramp decays exponentially and never reaches
/// zero on its own; once it is inaudibly small the envelope snaps
/// to exactly zero so the listener gets true silence.
#[test]
fn squelch_audio_envelope_settles_to_exact_zero_after_release() {
    const RATE: f32 = 48_000.0;
    const BLOCK: usize = 480;
    let mut env = SquelchAudioEnvelope::new(RATE).unwrap();
    env.reset_to_open();
    env.set_gate_open(false);
    let mut buf = vec![sdr_types::Stereo { l: 1.0, r: 1.0 }; BLOCK];
    env.process_stereo(&mut buf);
    assert!(!env.settle_if_closed(), "still audibly ramping after 10 ms");
    assert!(
        buf[0].l > 0.5 && buf[BLOCK - 1].l < buf[0].l,
        "release ramps down"
    );
    for _ in 0..100 {
        let mut buf = vec![sdr_types::Stereo { l: 1.0, r: 1.0 }; BLOCK];
        env.process_stereo(&mut buf);
        if env.settle_if_closed() {
            break;
        }
    }
    assert!(env.settle_if_closed(), "must settle within 1 s");
    let mut buf = vec![sdr_types::Stereo { l: 1.0, r: 1.0 }; BLOCK];
    env.process_stereo(&mut buf);
    assert!(
        buf.iter().all(|s| s.l == 0.0 && s.r == 0.0),
        "exact zero once settled"
    );
    // Reopening leaves the settled state.
    env.set_gate_open(true);
    assert!(!env.settle_if_closed());
}
