use super::*;

#[test]
fn test_if_chain_passthrough_when_disabled() {
    let mut chain = IfChain::new().unwrap();
    let input = vec![Complex::new(1.0, 2.0); 100];
    let mut output = vec![Complex::default(); 100];
    let count = chain.process(&input, &mut output).unwrap();
    assert_eq!(count, 100);
    assert_eq!(output[0].re, 1.0);
    assert_eq!(output[0].im, 2.0);
}

// ---- FM IF NR streaming fixture parameters (#773) ----
/// Matches `FmIfNoiseReduction`'s default block size.
const NR_FFT_SIZE: usize = 256;
/// Two full NR blocks.
const NR_SIGNAL_LEN: usize = 2 * NR_FFT_SIZE;
/// Tone bin (cycles per FFT block) — a clean single peak for the NR.
const NR_TONE_BIN: f32 = 37.0;
/// One block plus a partial remainder, so state is left buffered.
const NR_PARTIAL_CHUNK: usize = NR_FFT_SIZE + 44;
/// Sample-equality tolerance between two chains.
const NR_SAMPLE_TOL: f32 = 1e-4;

fn nr_test_tone(len: usize) -> Vec<Complex> {
    (0..len)
        .map(|i| {
            let theta = 2.0 * core::f32::consts::PI * NR_TONE_BIN * i as f32 / NR_FFT_SIZE as f32;
            Complex::new(theta.cos(), theta.sin())
        })
        .collect()
}

/// #773 (CR) — toggling FM IF NR off and on must not replay samples
/// buffered before the disable: the re-enabled chain behaves like a
/// fresh one.
#[test]
fn fm_if_nr_toggle_resets_buffered_state() {
    let signal = nr_test_tone(NR_SIGNAL_LEN);

    let mut toggled = IfChain::new().unwrap();
    toggled.set_fm_if_nr_enabled(true);
    let mut scratch = vec![Complex::default(); NR_PARTIAL_CHUNK];
    toggled
        .process(&signal[..NR_PARTIAL_CHUNK], &mut scratch)
        .unwrap();
    toggled.set_fm_if_nr_enabled(false);
    toggled.set_fm_if_nr_enabled(true);
    let mut out_toggled = vec![Complex::default(); NR_SIGNAL_LEN];
    let n_toggled = toggled.process(&signal, &mut out_toggled).unwrap();

    let mut fresh = IfChain::new().unwrap();
    fresh.set_fm_if_nr_enabled(true);
    let mut out_fresh = vec![Complex::default(); NR_SIGNAL_LEN];
    let n_fresh = fresh.process(&signal, &mut out_fresh).unwrap();

    assert_eq!(
        n_toggled, n_fresh,
        "stale buffered samples leaked through the toggle"
    );
    for i in 0..n_fresh {
        assert!(
            (out_toggled[i].re - out_fresh[i].re).abs() < NR_SAMPLE_TOL
                && (out_toggled[i].im - out_fresh[i].im).abs() < NR_SAMPLE_TOL,
            "sample {i} differs after toggle"
        );
    }
}

/// #773 (CR) — end of stream: `process` + `flush` must account for
/// every input sample when FM IF NR is enabled, and `flush` is a no-op
/// when it is not.
#[test]
fn fm_if_nr_flush_completes_the_stream() {
    let signal = nr_test_tone(NR_PARTIAL_CHUNK);

    let mut chain = IfChain::new().unwrap();
    chain.set_fm_if_nr_enabled(true);
    let mut out = vec![Complex::default(); NR_PARTIAL_CHUNK];
    let n = chain.process(&signal, &mut out).unwrap();
    assert!(
        n < NR_PARTIAL_CHUNK,
        "a partial block should still be buffered"
    );
    let mut tail = vec![Complex::default(); 2 * NR_FFT_SIZE];
    let flushed = chain.flush(&mut tail).unwrap();
    assert_eq!(n + flushed, NR_PARTIAL_CHUNK);

    let mut plain = IfChain::new().unwrap();
    assert_eq!(
        plain.flush(&mut tail).unwrap(),
        0,
        "flush is a no-op with NR disabled"
    );
}

#[test]
fn test_if_chain_squelch_enabled() {
    let mut chain = IfChain::new().unwrap();
    chain.set_squelch_enabled(true);
    chain.set_squelch_level(10.0); // very high threshold

    let input = vec![Complex::new(0.001, 0.0); 100];
    let mut output = vec![Complex::default(); 100];
    chain.process(&input, &mut output).unwrap();

    // Squelch should close on weak signal
    assert!(!chain.squelch_open());
    // Output should be zeroed
    for s in &output {
        assert!(s.re.abs() < 1e-10);
    }
}

/// #734 — `RadioModule` moves the exact-zero mute to the AF side so
/// the imaging taps can read ungated audio; the IF chain must be
/// able to keep the gate state without zeroing IQ.
#[test]
fn squelch_can_gate_without_muting_iq() {
    let mut chain = IfChain::new().unwrap();
    chain.set_squelch_enabled(true);
    chain.set_squelch_level(10.0); // very high threshold
    chain.set_squelch_mutes_iq(false);

    let input = vec![Complex::new(0.001, 0.0); 100];
    let mut output = vec![Complex::default(); 100];
    chain.process(&input, &mut output).unwrap();

    assert!(!chain.squelch_open(), "gate state is still tracked");
    assert_eq!(output, input, "IQ passes through unmuted");
}

#[test]
fn test_if_chain_squelch_opens_on_strong_signal() {
    let mut chain = IfChain::new().unwrap();
    chain.set_squelch_enabled(true);
    chain.set_squelch_level(-50.0);

    let input = vec![Complex::new(1.0, 0.0); 100];
    let mut output = vec![Complex::default(); 100];
    chain.process(&input, &mut output).unwrap();

    assert!(chain.squelch_open());
}

#[test]
fn test_if_chain_nb_enabled() {
    let mut chain = IfChain::new().unwrap();
    chain.set_nb_enabled(true);
    assert!(chain.nb_enabled());

    let input = vec![Complex::new(1.0, 0.0); 500];
    let mut output = vec![Complex::default(); 500];
    let count = chain.process(&input, &mut output).unwrap();
    assert_eq!(count, 500);
    // Output should be non-zero (normal signal passes)
    assert!(output[499].re.abs() > 0.1);
}

#[test]
fn test_if_chain_fm_if_nr_enabled() {
    let mut chain = IfChain::new().unwrap();
    chain.set_fm_if_nr_enabled(true);
    assert!(chain.fm_if_nr_enabled());

    // Use a signal large enough for the FFT block size (256 default).
    let input = vec![Complex::new(1.0, 0.0); 512];
    let mut output = vec![Complex::default(); 512];
    let count = chain.process(&input, &mut output).unwrap();
    assert_eq!(count, 512);
    // DC signal should mostly survive (peak bin = 0).
    let energy: f32 = output.iter().map(|s| s.re * s.re + s.im * s.im).sum();
    assert!(energy > 0.0, "FM IF NR should produce output");
}

#[test]
fn test_if_chain_all_enabled() {
    let mut chain = IfChain::new().unwrap();
    chain.set_nb_enabled(true);
    chain.set_squelch_enabled(true);
    chain.set_squelch_level(-50.0);
    chain.set_fm_if_nr_enabled(true);

    let input = vec![Complex::new(1.0, 0.0); 512];
    let mut output = vec![Complex::default(); 512];
    let count = chain.process(&input, &mut output).unwrap();
    assert_eq!(count, 512);
}

#[test]
fn test_if_chain_set_nb_level() {
    let mut chain = IfChain::new().unwrap();
    assert!(chain.set_nb_level(10.0).is_ok());
    assert!(chain.set_nb_level(0.5).is_err()); // below minimum of 1.0
}

#[test]
fn test_if_chain_squelch_reports_open_when_disabled() {
    let chain = IfChain::new().unwrap();
    // When squelch is disabled, squelch_open should return true
    assert!(chain.squelch_open());
}

#[test]
fn test_if_chain_buffer_too_small() {
    let mut chain = IfChain::new().unwrap();
    let input = [Complex::default(); 10];
    let mut output = [Complex::default(); 5];
    assert!(chain.process(&input, &mut output).is_err());
}

// --- Software IF AGC tests (#354) ---
//
// Named test fixtures. Centralized here so a future AGC
// retune touches exactly one block — the literals are
// load-bearing against the shipped `SOFTWARE_AGC_ATTACK`
// and `SOFTWARE_AGC_DECAY` coefficients (1/300 and 1/3000).

/// Number of samples used by the weak / strong convergence
/// tests. ~10 decay time constants at the current coefficients,
/// enough for the envelope tracker to converge close to the
/// target set point without the test taking forever.
const AGC_CONVERGENCE_BLOCK_LEN: usize = 30_000;
/// Short block used by the disable-revert and squelch-interop
/// tests where full convergence isn't needed.
const AGC_SHORT_BLOCK_LEN: usize = 1_000;
/// Input amplitude 40 dB below the `SOFTWARE_AGC_SET_POINT = 1.0`
/// target. Used by the weak-signal amplification test.
const AGC_WEAK_INPUT_AMP: f32 = 0.01;
/// Input amplitude 20 dB above the `SOFTWARE_AGC_SET_POINT = 1.0`
/// target. Used by the strong-signal attenuation test.
const AGC_STRONG_INPUT_AMP: f32 = 10.0;
/// Pass-through sample amplitude used by the disable-revert test.
/// Arbitrary non-zero value; the assertion is equality to this.
const AGC_PASSTHROUGH_AMP: f32 = 5.0;
/// Squelch threshold used by the interop test. Set above the
/// weak-input `-40 dBFS` level so the gate closes on the test
/// input, exercising the AGC-after-squelch ordering.
const AGC_SQUELCH_TEST_THRESHOLD_DB: f32 = -20.0;
/// Float tolerance for the passthrough assertion. Tight —
/// scaling by 1.0 should preserve the input exactly in f32.
const AGC_PASSTHROUGH_EPSILON: f32 = 1e-5;
/// Float tolerance for the "gate is zeroed" assertion. Tighter
/// than the passthrough tolerance because the expected value
/// is literal 0.0, not a scaled input.
const AGC_ZERO_EPSILON: f32 = 1e-10;
/// Fraction of the block used as the tail window for
/// convergence assertions. The last quarter captures steady
/// state after the envelope has converged.
const AGC_TAIL_FRACTION: usize = 4;
/// Minimum gain factor for the weak-signal convergence
/// assertion. Well below the theoretical `1.0 / 0.01 = 100×`
/// so the test stays robust against coefficient tweaks.
const AGC_WEAK_MIN_GAIN: f32 = 5.0;
/// Maximum residual factor for the strong-signal attenuation
/// assertion. Well above the theoretical `1.0 / 10.0 = 0.1×`
/// floor so the test stays robust against coefficient tweaks.
const AGC_STRONG_MAX_RESIDUAL: f32 = 0.5;

/// Default state: software AGC is off. A fresh `IfChain` with
/// no other stages active should passthrough IQ unchanged —
/// the AGC flag defaults to `false` and the `any_enabled`
/// short-circuit covers the no-op fast path.
#[test]
fn software_agc_off_by_default() {
    let chain = IfChain::new().unwrap();
    assert!(!chain.software_agc_enabled());
}

/// With software AGC enabled, a weak constant-envelope input
/// should see its effective gain rise over time as the
/// envelope tracker converges toward the `1.0` set point.
/// Pins the core "AGC actually moves gain toward set point"
/// contract — a bypassed or broken AGC would leave output =
/// input, a too-aggressive one would overshoot.
#[test]
fn software_agc_amplifies_weak_signal() {
    let mut chain = IfChain::new().unwrap();
    chain.set_software_agc_enabled(true);

    let n = AGC_CONVERGENCE_BLOCK_LEN;
    let input = vec![Complex::new(AGC_WEAK_INPUT_AMP, 0.0); n];
    let mut output = vec![Complex::default(); n];
    chain.process(&input, &mut output).unwrap();

    let tail = &output[n - n / AGC_TAIL_FRACTION..];
    let mean_out: f32 = tail
        .iter()
        .map(|s| (s.re * s.re + s.im * s.im).sqrt())
        .sum::<f32>()
        / tail.len() as f32;
    assert!(
        mean_out > AGC_WEAK_INPUT_AMP * AGC_WEAK_MIN_GAIN,
        "software AGC should amplify weak signal, input = {AGC_WEAK_INPUT_AMP}, mean output = {mean_out}"
    );
}

/// With software AGC enabled, a high-amplitude input should
/// be attenuated toward the set point. Complements the
/// amplification test.
#[test]
fn software_agc_attenuates_strong_signal() {
    let mut chain = IfChain::new().unwrap();
    chain.set_software_agc_enabled(true);

    let n = AGC_CONVERGENCE_BLOCK_LEN;
    let input = vec![Complex::new(AGC_STRONG_INPUT_AMP, 0.0); n];
    let mut output = vec![Complex::default(); n];
    chain.process(&input, &mut output).unwrap();

    let tail = &output[n - n / AGC_TAIL_FRACTION..];
    let mean_out: f32 = tail
        .iter()
        .map(|s| (s.re * s.re + s.im * s.im).sqrt())
        .sum::<f32>()
        / tail.len() as f32;
    assert!(
        mean_out < AGC_STRONG_INPUT_AMP * AGC_STRONG_MAX_RESIDUAL,
        "software AGC should attenuate strong signal, input = {AGC_STRONG_INPUT_AMP}, mean output = {mean_out}"
    );
}

/// Toggling the AGC off reverts to IQ passthrough. Pins
/// that `set_software_agc_enabled(false)` actually takes
/// effect on the NEXT `process` call — a state-leak bug
/// would leave the AGC stage silently active.
#[test]
fn software_agc_disable_reverts_to_passthrough() {
    let mut chain = IfChain::new().unwrap();
    chain.set_software_agc_enabled(true);

    let n = AGC_SHORT_BLOCK_LEN;
    let input = vec![Complex::new(AGC_PASSTHROUGH_AMP, 0.0); n];
    let mut output = vec![Complex::default(); n];
    chain.process(&input, &mut output).unwrap();

    // Disable, run same input again — output should match
    // input verbatim now (no other stages enabled).
    chain.set_software_agc_enabled(false);
    chain.process(&input, &mut output).unwrap();
    for (i, s) in output.iter().enumerate() {
        assert!(
            (s.re - AGC_PASSTHROUGH_AMP).abs() < AGC_PASSTHROUGH_EPSILON
                && s.im.abs() < AGC_PASSTHROUGH_EPSILON,
            "sample {i} should be pure passthrough after disable, got ({}, {})",
            s.re,
            s.im
        );
    }
}

/// Software AGC + squelch must interoperate: the squelch
/// reads pre-AGC amplitude (so it can distinguish signal
/// from noise) and AGC only runs when the gate is open.
/// Pins the processing order documented on `IfChain`.
#[test]
fn software_agc_after_squelch_preserves_gating() {
    let mut chain = IfChain::new().unwrap();
    chain.set_software_agc_enabled(true);
    chain.set_squelch_enabled(true);
    chain.set_squelch_level(AGC_SQUELCH_TEST_THRESHOLD_DB);

    // Quiet input: 0.01 amplitude = -40 dBFS, below the
    // -20 dB threshold. Gate must close.
    let n = AGC_SHORT_BLOCK_LEN;
    let input = vec![Complex::new(AGC_WEAK_INPUT_AMP, 0.0); n];
    let mut output = vec![Complex::default(); n];
    chain.process(&input, &mut output).unwrap();

    assert!(
        !chain.squelch_open(),
        "squelch should still close on quiet pre-AGC signal"
    );
    // When gate is closed, output is IQ-zero regardless of
    // AGC state — AGC skips the block entirely when squelch
    // is muting, and PowerSquelch's zero output propagates
    // through.
    for s in &output {
        assert!(s.re.abs() < AGC_ZERO_EPSILON && s.im.abs() < AGC_ZERO_EPSILON);
    }
}

/// AGC state must survive a squelch close/reopen cycle
/// without winding toward max gain or producing a burst on
/// reopen. Runs one block of loud signal (AGC converges
/// toward attenuation), one block of quiet below-threshold
/// noise (gate closes, AGC skipped), then another loud
/// block. The first post-reopen sample's magnitude must
/// stay bounded — a wind-up bug would push it into the
/// `SOFTWARE_AGC_MAX_OUTPUT = 10.0` look-ahead clipping
/// cap or beyond.
#[test]
fn software_agc_survives_squelch_cycle_without_burst() {
    let mut chain = IfChain::new().unwrap();
    chain.set_software_agc_enabled(true);
    chain.set_squelch_enabled(true);
    chain.set_squelch_level(AGC_SQUELCH_TEST_THRESHOLD_DB);

    let n = AGC_SHORT_BLOCK_LEN;
    let mut output = vec![Complex::default(); n];

    // Block 1: loud signal — gate open, AGC attacks.
    let loud = vec![Complex::new(AGC_STRONG_INPUT_AMP, 0.0); n];
    chain.process(&loud, &mut output).unwrap();
    assert!(chain.squelch_open(), "loud signal should open gate");

    // Block 2: quiet noise — gate closes, AGC should be
    // skipped entirely so its state is frozen at block 1's
    // convergence rather than being fed zeros.
    let quiet = vec![Complex::new(AGC_WEAK_INPUT_AMP, 0.0); n];
    chain.process(&quiet, &mut output).unwrap();
    assert!(!chain.squelch_open(), "quiet signal should close gate");
    for s in &output {
        assert!(
            s.re.abs() < AGC_ZERO_EPSILON && s.im.abs() < AGC_ZERO_EPSILON,
            "gate-closed output should be zero"
        );
    }

    // Block 3: loud signal returns — gate reopens, AGC
    // resumes from block 1's state. First sample amplitude
    // must be bounded by the look-ahead clipping cap; a
    // wind-up bug would push it well above that.
    chain.process(&loud, &mut output).unwrap();
    let first_mag = (output[0].re * output[0].re + output[0].im * output[0].im).sqrt();
    assert!(
        first_mag < AGC_STRONG_INPUT_AMP,
        "first post-reopen sample should not burst above the input level, got mag = {first_mag}"
    );
}
