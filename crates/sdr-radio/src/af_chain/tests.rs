use super::*;
use sdr_dsp::filter::DEEMPHASIS_TAU_US;

#[test]
fn test_af_chain_passthrough_same_rate() {
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    let input = vec![Stereo::new(0.5, -0.5); 100];
    let mut output = vec![Stereo::default(); 100];
    let count = chain.process(&input, &mut output).unwrap();
    assert_eq!(count, 100);
    assert_eq!(output[0].l, 0.5);
    assert_eq!(output[0].r, -0.5);
}

/// WFM AF rate in, audio rate out: 2500 samples → ~480 out
/// (2500 · 48000 / 250000); the bounds allow filter warm-up and
/// block-boundary slack.
const WFM_AF_RATE_HZ: f64 = 250_000.0;
const AUDIO_RATE_HZ: f64 = 48_000.0;
const DOWNSAMPLE_INPUT_SAMPLES: usize = 2500;
const DOWNSAMPLE_MIN_OUTPUT: usize = 350;
const DOWNSAMPLE_MAX_OUTPUT: usize = 600;
/// CW AF rate in: 300 samples → ~4800 out (300 · 48000 / 3000).
const CW_AF_RATE_HZ: f64 = 3_000.0;
const UPSAMPLE_INPUT_SAMPLES: usize = 300;
const UPSAMPLE_OUTPUT_CAPACITY: usize = 6000;
const UPSAMPLE_MIN_OUTPUT: usize = 4000;
const UPSAMPLE_MAX_OUTPUT: usize = 5600;

#[test]
fn test_af_chain_resample_downsample() {
    let mut chain = AfChain::new(WFM_AF_RATE_HZ, AUDIO_RATE_HZ).unwrap();
    let input = vec![Stereo::new(1.0, -1.0); DOWNSAMPLE_INPUT_SAMPLES];
    let mut output = vec![Stereo::default(); DOWNSAMPLE_INPUT_SAMPLES];
    let count = chain.process(&input, &mut output).unwrap();
    assert!(
        (DOWNSAMPLE_MIN_OUTPUT..=DOWNSAMPLE_MAX_OUTPUT).contains(&count),
        "expected ~480 samples, got {count}"
    );
}

#[test]
fn test_af_chain_resample_upsample() {
    let mut chain = AfChain::new(CW_AF_RATE_HZ, AUDIO_RATE_HZ).unwrap();
    let input = vec![Stereo::new(0.5, 0.5); UPSAMPLE_INPUT_SAMPLES];
    let mut output = vec![Stereo::default(); UPSAMPLE_OUTPUT_CAPACITY];
    let count = chain.process(&input, &mut output).unwrap();
    assert!(
        (UPSAMPLE_MIN_OUTPUT..=UPSAMPLE_MAX_OUTPUT).contains(&count),
        "expected ~4800 samples, got {count}"
    );
}

#[test]
fn test_af_chain_deemphasis_attenuates_high_freq() {
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    chain.set_deemp_enabled(true, DEEMPHASIS_TAU_US).unwrap();
    assert!(chain.deemp_enabled());

    // High frequency alternating signal
    let input: Vec<Stereo> = (0..1000)
        .map(|i| {
            let v = if i % 2 == 0 { 1.0 } else { -1.0 };
            Stereo::new(v, v)
        })
        .collect();
    let mut output = vec![Stereo::default(); 1000];
    let count = chain.process(&input, &mut output).unwrap();
    assert_eq!(count, 1000);

    // Peak output should be attenuated compared to input
    let peak = output[500..]
        .iter()
        .map(|s| s.l.abs())
        .fold(0.0_f32, f32::max);
    assert!(
        peak < 0.5,
        "deemphasis should attenuate high freq, peak = {peak}"
    );
}

#[test]
fn test_af_chain_empty_input() {
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    let mut output = vec![Stereo::default(); 10];
    let count = chain.process(&[], &mut output).unwrap();
    assert_eq!(count, 0);
}

#[test]
fn test_af_chain_deemphasis_disabled_passthrough() {
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    chain.set_deemp_enabled(false, 0.0).unwrap();
    assert!(!chain.deemp_enabled());

    let input = vec![Stereo::new(0.5, -0.3); 100];
    let mut output = vec![Stereo::default(); 100];
    let count = chain.process(&input, &mut output).unwrap();
    assert_eq!(count, 100);
    assert_eq!(output[0].l, 0.5);
    assert_eq!(output[0].r, -0.3);
}

#[test]
fn test_af_chain_with_default_rate() {
    let chain = AfChain::with_default_rate(24_000.0).unwrap();
    assert!((chain.audio_sample_rate() - 48_000.0).abs() < 1.0);
    assert!((chain.af_sample_rate() - 24_000.0).abs() < 1.0);
}

// ─────────────────────────────────────────────────────────────
// CTCSS tests (PR 2 of #269)
// ─────────────────────────────────────────────────────────────

use sdr_dsp::tone_detect::{CTCSS_MIN_HITS, CTCSS_WINDOW_SAMPLES};

/// Build a stereo block of a pure CTCSS tone at 48 kHz with
/// amplitude 1.0 on both channels. Enough samples to cover
/// `windows` full detector windows.
fn ctcss_tone_block(freq_hz: f32, windows: usize) -> Vec<Stereo> {
    let n = CTCSS_WINDOW_SAMPLES * windows;
    let mut out = Vec::with_capacity(n);
    let dt = 1.0 / 48_000.0_f32;
    for i in 0..n {
        #[allow(clippy::cast_precision_loss)]
        let t = i as f32 * dt;
        let v = (core::f32::consts::TAU * freq_hz * t).sin();
        out.push(Stereo::new(v, v));
    }
    out
}

#[test]
fn test_ctcss_off_passes_audio_through_unchanged() {
    // Baseline: with CTCSS off the chain behaves exactly like
    // the existing passthrough. No zeroing, no forced HPF,
    // no detector state.
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    assert_eq!(chain.ctcss_mode(), CtcssMode::Off);
    assert!(!chain.effective_high_pass_enabled());

    let input = vec![Stereo::new(0.5, -0.5); 1000];
    let mut output = vec![Stereo::default(); 1000];
    let count = chain.process(&input, &mut output).unwrap();
    assert_eq!(count, 1000);
    assert_eq!(output[0].l, 0.5);
    assert_eq!(output[500].r, -0.5);
}

#[test]
fn test_ctcss_set_mode_rejects_non_table_frequency() {
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    let err = chain.set_ctcss_mode(CtcssMode::Tone(123.456));
    assert!(err.is_err(), "non-CTCSS frequency must be rejected");
    assert_eq!(chain.ctcss_mode(), CtcssMode::Off);
}

#[test]
fn test_ctcss_tone_mode_force_enables_high_pass() {
    // User explicitly has HPF off. CTCSS should still engage
    // the filter to strip the sub-audible tone from the
    // speaker path.
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    chain.set_high_pass_enabled(false);
    assert!(!chain.high_pass_enabled());
    assert!(!chain.effective_high_pass_enabled());

    chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();
    assert!(
        !chain.high_pass_enabled(),
        "user preference must not be silently flipped"
    );
    assert!(
        chain.effective_high_pass_enabled(),
        "CTCSS should force-enable the HPF"
    );
}

#[test]
fn test_ctcss_off_restores_user_high_pass_preference() {
    // User sets HPF off, CTCSS force-engages it, then CTCSS
    // goes back to Off. The user's original preference must
    // re-take effect — no leaked force-on state.
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    chain.set_high_pass_enabled(false);
    chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();
    assert!(chain.effective_high_pass_enabled());

    chain.set_ctcss_mode(CtcssMode::Off).unwrap();
    assert!(!chain.effective_high_pass_enabled());

    // And the opposite: user sets HPF on first, CTCSS toggles
    // around it. User preference survives unchanged.
    let mut chain2 = AfChain::new(48_000.0, 48_000.0).unwrap();
    chain2.set_high_pass_enabled(true);
    chain2.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();
    assert!(chain2.effective_high_pass_enabled());
    chain2.set_ctcss_mode(CtcssMode::Off).unwrap();
    assert!(chain2.effective_high_pass_enabled());
    assert!(chain2.high_pass_enabled());
}

#[test]
fn test_ctcss_wrong_tone_mutes_output() {
    // Detector targets 100 Hz but the audio is a 131.8 Hz
    // tone. After several windows the sustained gate should
    // still be closed and the output should be muted.
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();

    // Feed 4 windows so the detector has had plenty of time
    // to confirm (or reject) a signal. A bit more than
    // `CTCSS_MIN_HITS` gives a margin for the first window
    // which may partially overlap warmup.
    let windows = CTCSS_MIN_HITS + 1;
    let input = ctcss_tone_block(131.8, windows);
    let mut output = vec![Stereo::default(); input.len()];
    chain.process(&input, &mut output).unwrap();

    assert!(!chain.ctcss_sustained(), "wrong-tone must not sustain");
    // Later samples (past the HPF warmup transient) must all
    // be exactly zero — the muting happens in a single pass
    // at the end of process, so every output sample is 0.0.
    let last_window = &output[CTCSS_WINDOW_SAMPLES * CTCSS_MIN_HITS..];
    for (i, s) in last_window.iter().enumerate() {
        assert_eq!(s.l, 0.0, "sample {i} L should be muted, got {}", s.l);
        assert_eq!(s.r, 0.0, "sample {i} R should be muted, got {}", s.r);
    }
}

#[test]
fn test_ctcss_correct_tone_opens_gate_after_min_hits() {
    // Detector targets 100 Hz and the audio is a 100 Hz tone.
    // After `CTCSS_MIN_HITS` windows the sustained gate must
    // open and the output must contain audible signal (post-
    // HPF, so the 100 Hz tone itself is attenuated, but some
    // residual energy remains).
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();

    let windows = CTCSS_MIN_HITS + 2;
    let input = ctcss_tone_block(100.0, windows);
    let mut output = vec![Stereo::default(); input.len()];
    chain.process(&input, &mut output).unwrap();

    assert!(
        chain.ctcss_sustained(),
        "correct-tone must open the sustained gate"
    );
    // With the gate open the muting step is skipped, so the
    // later windows are NOT all zero. The HPF attenuates 100
    // Hz heavily but doesn't null it — we should still see
    // nonzero samples somewhere in the last window.
    let last_window_start = CTCSS_WINDOW_SAMPLES * (windows - 1);
    let any_nonzero = output[last_window_start..]
        .iter()
        .any(|s| s.l.abs() > 1e-6 || s.r.abs() > 1e-6);
    assert!(
        any_nonzero,
        "open gate must pass some signal through (even after HPF)"
    );
}

#[test]
fn test_ctcss_mode_serde_round_trip() {
    // Off → {"kind":"off"}
    let off = CtcssMode::Off;
    let json = serde_json::to_string(&off).unwrap();
    assert_eq!(json, r#"{"kind":"off"}"#);
    let back: CtcssMode = serde_json::from_str(&json).unwrap();
    assert_eq!(back, CtcssMode::Off);

    // Tone(100.0) → {"kind":"tone","hz":100.0}
    let tone = CtcssMode::Tone(100.0);
    let json = serde_json::to_string(&tone).unwrap();
    assert_eq!(json, r#"{"kind":"tone","hz":100.0}"#);
    let back: CtcssMode = serde_json::from_str(&json).unwrap();
    assert_eq!(back, CtcssMode::Tone(100.0));
}

#[test]
fn test_ctcss_threshold_roundtrip_and_validation() {
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    // Default equals the DSP constant.
    assert!((chain.ctcss_threshold() - sdr_dsp::tone_detect::CTCSS_DEFAULT_THRESHOLD).abs() < 1e-6);

    // Valid values are accepted and persist.
    chain.set_ctcss_threshold(0.25).unwrap();
    assert!((chain.ctcss_threshold() - 0.25).abs() < 1e-6);

    // Out-of-range / non-finite values are rejected and the
    // existing value is preserved.
    assert!(chain.set_ctcss_threshold(0.0).is_err());
    assert!(chain.set_ctcss_threshold(-0.1).is_err());
    assert!(chain.set_ctcss_threshold(1.001).is_err());
    assert!(chain.set_ctcss_threshold(f32::NAN).is_err());
    assert!((chain.ctcss_threshold() - 0.25).abs() < 1e-6);
}

#[test]
fn test_ctcss_threshold_persists_across_mode_and_rebuilds_detector() {
    // Set a non-default threshold first, then enable a tone.
    // The built detector must pick up the stored threshold
    // rather than snapping back to CTCSS_DEFAULT_THRESHOLD.
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    chain.set_ctcss_threshold(0.3).unwrap();
    chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();

    // And the reverse order: mode first, then threshold change
    // should rebuild the active detector.
    chain.set_ctcss_threshold(0.4).unwrap();
    assert!((chain.ctcss_threshold() - 0.4).abs() < 1e-6);
    // Sustained state must be reset after the rebuild.
    assert!(!chain.ctcss_sustained());

    // Going Off → Tone preserves the user-tuned threshold.
    chain.set_ctcss_mode(CtcssMode::Off).unwrap();
    chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();
    assert!((chain.ctcss_threshold() - 0.4).abs() < 1e-6);
}

#[test]
fn test_ctcss_mode_change_resets_detector() {
    // Switching target tones rebuilds the detector from
    // scratch; the sustained state should NOT carry over
    // from the previous tone.
    let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
    chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();

    // Open the gate on 100 Hz.
    let input = ctcss_tone_block(100.0, CTCSS_MIN_HITS + 1);
    let mut output = vec![Stereo::default(); input.len()];
    chain.process(&input, &mut output).unwrap();
    assert!(chain.ctcss_sustained());

    // Switch to 131.8 Hz. Sustained state must reset.
    chain.set_ctcss_mode(CtcssMode::Tone(131.8)).unwrap();
    assert!(!chain.ctcss_sustained());
}

// ─────────────────────────────────────────────────────────
// Voice squelch tests
//
// The exhaustive algorithmic tests live in
// `sdr_dsp::voice_squelch`. These are integration tests that
// prove the AF chain wires the voice squelch correctly:
//
// - Off mode leaves audio unchanged
// - Active modes actually gate the stereo output
// - Mode change resets state
// - Threshold update propagates
// - Voice squelch ANDs with CTCSS (both must be open)
// ─────────────────────────────────────────────────────────

use sdr_dsp::voice_squelch::{
    VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB, VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
    VoiceSquelchMode,
};

// Test-only fixture constants. Per CLAUDE.md's "named
// constants for all magic numbers" rule, even test values
// with load-bearing intent get names + rationale so a
// future detector retune can find them in one place.

/// Sample rate for all voice-squelch AF-chain tests. Matches
/// the canonical 48 kHz the DSP layer is calibrated against
/// (see `VOICE_SQUELCH_SAMPLE_RATE_HZ`). Passed to
/// `AfChain::new` for both `af_sample_rate` and
/// `audio_sample_rate` so the tests don't exercise the
/// resampler — voice-squelch behavior is the focus.
const VS_TEST_SAMPLE_RATE: f64 = 48_000.0;

/// Length in samples of a 100 ms short-window test block.
/// Matches `VOICE_SQUELCH_RMS_WINDOW_MS` so silence-rejection
/// tests feed exactly one RMS integration window.
const VS_SHORT_BLOCK_SAMPLES: usize = 4_800;

/// Length in samples of a 2-second long block used by the
/// detector-opens tests. Hang time is 500 ms and the RMS
/// window is 100 ms, so 2 seconds is comfortably long enough
/// for the detector to ring up, the gate to open, and the
/// post-warmup tail to have audible signal.
const VS_LONG_BLOCK_SAMPLES: usize = 96_000;

/// Offset into `VS_LONG_BLOCK_SAMPLES` where we check the
/// output for post-warmup audible signal. 1 second in — past
/// both the HPF warmup transient and the detector ring-up,
/// but still inside the long block.
const VS_LONG_BLOCK_TAIL_OFFSET: usize = 48_000;

/// In-voice-band carrier frequency (Hz) used by all the
/// "strong tone opens the gate" tests. Chosen to land inside
/// both the SNR detector's in-band BPF passband (centered
/// at 1 kHz) and the voice-band weight region used by
/// `enhance_speech` elsewhere.
const VS_TEST_CARRIER_HZ: f32 = 1_000.0;

/// "Strong signal" amplitude — well above the detector's
/// noise floor. 0.8 peak leaves headroom under f32 [-1, 1]
/// so a single-channel stereo dupe doesn't clip.
const VS_STRONG_AMPLITUDE: f32 = 0.8;

/// "Normal signal" amplitude used by the Off-mode passthrough
/// test where we just need to verify audio reaches the
/// output, not open a gate.
const VS_NORMAL_AMPLITUDE: f32 = 0.5;

/// Build a stereo buffer of the given length filled with a
/// pure sine tone at `VS_TEST_CARRIER_HZ` and the given
/// amplitude, duplicated across both channels. Shared by
/// all three "strong tone" tests to avoid the same
/// generator being copy-pasted three times.
fn stereo_tone(n: usize, amplitude: f32) -> Vec<Stereo> {
    #[allow(clippy::cast_possible_truncation)]
    let sample_rate = VS_TEST_SAMPLE_RATE as f32;
    (0..n)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32 / sample_rate;
            let v = amplitude * (core::f32::consts::TAU * VS_TEST_CARRIER_HZ * t).sin();
            Stereo::new(v, v)
        })
        .collect()
}

#[test]
fn test_voice_squelch_defaults_to_off_and_passes_through() {
    let mut chain = AfChain::new(VS_TEST_SAMPLE_RATE, VS_TEST_SAMPLE_RATE).unwrap();
    assert_eq!(chain.voice_squelch_mode(), VoiceSquelchMode::Off);
    assert!(chain.voice_squelch_open(), "Off mode gate must start open");

    // Feed a pure in-band tone — should pass through unchanged
    // because the gate is permanently open in Off mode.
    let input = stereo_tone(VS_SHORT_BLOCK_SAMPLES, VS_NORMAL_AMPLITUDE);
    let mut output = vec![Stereo::default(); VS_SHORT_BLOCK_SAMPLES];
    chain.process(&input, &mut output).unwrap();
    // Sample 100 should survive untouched (HPF is off by
    // default and any warmup is well before this point).
    assert!(output[100].l.abs() > 0.01);
}

#[test]
fn test_voice_squelch_snr_rejects_silence() {
    // SNR mode with default threshold fed one 100 ms window
    // of silence: gate must stay closed and the output must
    // be zeroed.
    let mut chain = AfChain::new(VS_TEST_SAMPLE_RATE, VS_TEST_SAMPLE_RATE).unwrap();
    chain
        .set_voice_squelch_mode(VoiceSquelchMode::Snr {
            threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
        })
        .unwrap();

    let input = vec![Stereo::new(0.0, 0.0); VS_SHORT_BLOCK_SAMPLES];
    let mut output = vec![Stereo::default(); VS_SHORT_BLOCK_SAMPLES];
    chain.process(&input, &mut output).unwrap();

    assert!(!chain.voice_squelch_open());
    assert!(
        output.iter().all(|s| s.l == 0.0 && s.r == 0.0),
        "silence in → zero out regardless"
    );
}

/// #734 — the chain keeps an ungated copy of the block it just
/// zeroed for a closed CTCSS / voice gate, so imaging decoders
/// (APT / SSTV tones have no speech cadence) still get the audio.
#[test]
fn test_ungated_output_survives_a_closed_voice_gate() {
    let mut chain = AfChain::new(VS_TEST_SAMPLE_RATE, VS_TEST_SAMPLE_RATE).unwrap();
    chain
        .set_voice_squelch_mode(VoiceSquelchMode::Syllabic {
            threshold: sdr_dsp::voice_squelch::VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        })
        .unwrap();
    // A steady tone has no speech cadence: once the envelope
    // filter's start-up transient has decayed (the long block), the
    // Syllabic gate is closed although the input is a clean tone.
    let input = stereo_tone(VS_LONG_BLOCK_SAMPLES, VS_NORMAL_AMPLITUDE);
    let mut output = vec![Stereo::default(); VS_LONG_BLOCK_SAMPLES];
    let n = chain.process(&input, &mut output).unwrap();
    assert!(!chain.voice_squelch_open(), "test premise: gate closed");
    assert!(output[..n].iter().all(|s| s.l == 0.0 && s.r == 0.0));

    let ungated = chain.ungated_output();
    assert_eq!(ungated.len(), n);
    assert!(
        ungated.iter().any(|s| s.l.abs() > 0.0),
        "ungated copy must keep the tone"
    );
}

/// #734 (CR round 1 on PR #791) — an empty-input call returns 0 and
/// must not leave the previous block readable as "current" audio.
#[test]
fn test_ungated_output_is_cleared_by_empty_input() {
    let mut chain = AfChain::new(VS_TEST_SAMPLE_RATE, VS_TEST_SAMPLE_RATE).unwrap();
    let input = stereo_tone(VS_SHORT_BLOCK_SAMPLES, VS_NORMAL_AMPLITUDE);
    let mut output = vec![Stereo::default(); VS_SHORT_BLOCK_SAMPLES];
    let n = chain.process(&input, &mut output).unwrap();
    assert_eq!(
        chain.ungated_output().len(),
        n,
        "test premise: a block is retained"
    );
    assert_eq!(chain.process(&[], &mut output).unwrap(), 0);
    assert!(
        chain.ungated_output().is_empty(),
        "empty input must clear the retained block"
    );
}

#[test]
fn test_voice_squelch_snr_opens_on_strong_tone() {
    // Strong in-voice-band tone — the SNR detector should
    // open the gate after ingesting the 2 s block, and the
    // post-warmup output must contain audible signal.
    let mut chain = AfChain::new(VS_TEST_SAMPLE_RATE, VS_TEST_SAMPLE_RATE).unwrap();
    chain
        .set_voice_squelch_mode(VoiceSquelchMode::Snr {
            threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
        })
        .unwrap();

    let input = stereo_tone(VS_LONG_BLOCK_SAMPLES, VS_STRONG_AMPLITUDE);
    let mut output = vec![Stereo::default(); VS_LONG_BLOCK_SAMPLES];
    chain.process(&input, &mut output).unwrap();

    assert!(
        chain.voice_squelch_open(),
        "SNR gate must open on strong in-band tone"
    );
    let late_window = &output[VS_LONG_BLOCK_TAIL_OFFSET..];
    assert!(late_window.iter().any(|s| s.l.abs() > 0.01));
}

#[test]
fn test_voice_squelch_mode_change_resets_gate() {
    let mut chain = AfChain::new(VS_TEST_SAMPLE_RATE, VS_TEST_SAMPLE_RATE).unwrap();
    chain
        .set_voice_squelch_mode(VoiceSquelchMode::Snr {
            threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
        })
        .unwrap();

    // Open on a strong tone.
    let input = stereo_tone(VS_LONG_BLOCK_SAMPLES, VS_STRONG_AMPLITUDE);
    let mut output = vec![Stereo::default(); VS_LONG_BLOCK_SAMPLES];
    chain.process(&input, &mut output).unwrap();
    assert!(chain.voice_squelch_open());

    // Switch to a different mode — gate must reset.
    chain
        .set_voice_squelch_mode(VoiceSquelchMode::Syllabic {
            threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        })
        .unwrap();
    assert!(
        !chain.voice_squelch_open(),
        "mode change must reset gate to closed"
    );
}

#[test]
fn test_voice_squelch_and_gates_with_ctcss() {
    // When BOTH voice squelch and CTCSS are active, audio
    // should only pass if both gates agree. Here CTCSS is on
    // a tone that isn't present → CTCSS closed → output
    // muted regardless of voice squelch state.
    let mut chain = AfChain::new(VS_TEST_SAMPLE_RATE, VS_TEST_SAMPLE_RATE).unwrap();
    chain.set_ctcss_mode(CtcssMode::Tone(100.0)).unwrap();
    chain
        .set_voice_squelch_mode(VoiceSquelchMode::Snr {
            threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
        })
        .unwrap();

    // Strong in-band tone — voice squelch should open (high
    // in-band energy) but CTCSS should NOT open (no 100 Hz
    // sub-audible tone present), so CTCSS wins the AND and
    // the output must be zeroed.
    let input = stereo_tone(VS_LONG_BLOCK_SAMPLES, VS_STRONG_AMPLITUDE);
    let mut output = vec![Stereo::default(); VS_LONG_BLOCK_SAMPLES];
    chain.process(&input, &mut output).unwrap();

    assert!(
        chain.voice_squelch_open(),
        "test premise: the strong in-band tone opens the voice gate"
    );
    assert!(
        !chain.ctcss_sustained(),
        "CTCSS should stay closed without a 100 Hz sub-audible tone"
    );
    let tail = &output[VS_LONG_BLOCK_TAIL_OFFSET..];
    assert!(
        tail.iter().all(|s| s.l == 0.0 && s.r == 0.0),
        "CTCSS closed must mute output regardless of voice squelch state"
    );
}

#[test]
fn test_voice_squelch_threshold_update_validates() {
    let mut chain = AfChain::new(VS_TEST_SAMPLE_RATE, VS_TEST_SAMPLE_RATE).unwrap();
    chain
        .set_voice_squelch_mode(VoiceSquelchMode::Syllabic { threshold: 0.15 })
        .unwrap();

    // Valid update.
    assert!(chain.set_voice_squelch_threshold(0.2).is_ok());
    // Non-finite rejected.
    assert!(chain.set_voice_squelch_threshold(f32::NAN).is_err());
    assert!(chain.set_voice_squelch_threshold(f32::INFINITY).is_err());
    // Non-positive rejected (syllabic mode only).
    assert!(chain.set_voice_squelch_threshold(0.0).is_err());
    assert!(chain.set_voice_squelch_threshold(-0.1).is_err());
}
