use super::*;
use core::f32::consts::PI;

// ─── CTCSS threshold test fixtures ──────────────────────────
// Per project convention, test magic numbers (thresholds,
// tolerances, invalid-input lists) are named constants. These
// feed `test_radio_module_ctcss_threshold_*` — if the DSP
// layer's threshold range ever changes, there's one place to
// tune the test data.

/// Float tolerance for CTCSS threshold round-trip equality.
/// `1e-6` comfortably exceeds f32 rounding error for the
/// single-assignment round-trips the tests exercise.
const CTCSS_TEST_EPS: f32 = 1e-6;

/// Non-default value used by the persistence test. Chosen
/// strictly inside the DSP-layer `(0, 1]` range and clearly
/// different from the `CTCSS_DEFAULT_THRESHOLD` (0.1) so a
/// regression that silently reverts to the default fails
/// loudly.
const CTCSS_PERSIST_THRESHOLD: f32 = 0.25;

/// "Last-good" baseline used by the rejection test. Any
/// in-range value would work; 0.2 is distinct from both the
/// DSP default (0.1) and the persistence test's 0.25 so
/// cross-test contamination would be noticeable.
const CTCSS_LAST_GOOD_THRESHOLD: f32 = 0.2;

/// Values that `set_ctcss_threshold` must reject. Covers the
/// boundary cases (0.0, just over 1.0), a sub-zero, and all
/// three non-finite IEEE-754 values. Used by
/// `test_radio_module_ctcss_threshold_rejects_invalid`.
const INVALID_CTCSS_THRESHOLDS: [f32; 6] =
    [0.0, -0.1, 1.001, f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

// ─── Voice-squelch test fixtures ────────────────────────────
// Same "named constants with rationale" pattern as CTCSS.
// These feed `test_radio_module_voice_squelch_*`; a future
// DSP retune of the default thresholds or the accepted range
// should touch these in one place rather than hunting down
// bare literals scattered across the tests.

/// Non-default Syllabic threshold used by the persistence
/// test. Chosen inside the DSP-layer `(0, 1]` range and
/// clearly different from
/// `VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD` (0.15) so a
/// regression that silently reverts to the default fails
/// loudly. Also distinct from
/// `VS_SYLLABIC_TUNED_THRESHOLD` below so the two syllabic
/// tests can't contaminate each other through shared state.
const VS_SYLLABIC_PERSIST_THRESHOLD: f32 = 0.22;

/// Non-default Snr threshold (dB) used by the persistence
/// test's Snr gauntlet. Chosen inside the 0–20 dB UI range
/// and clearly above `VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB`
/// (6.0) so a regression that reverts to the default fails
/// loudly.
const VS_SNR_PERSIST_THRESHOLD_DB: f32 = 9.0;

/// Construction baseline for the threshold-updates-cached-mode
/// test. Equals `VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD`
/// — we start the test at the default so the `set_voice_squelch_mode`
/// call exercises the default-construction path.
const VS_SYLLABIC_BASELINE_THRESHOLD: f32 = 0.15;

/// Tuned Syllabic threshold for the threshold-updates-cached-
/// mode test. Distinct from BOTH
/// `VS_SYLLABIC_BASELINE_THRESHOLD` (so the update is
/// observable) AND `VS_SYLLABIC_PERSIST_THRESHOLD` (so the
/// two syllabic tests are independent).
const VS_SYLLABIC_TUNED_THRESHOLD: f32 = 0.30;

#[test]
fn test_radio_module_default_mode() {
    let radio = RadioModule::with_default_rate().unwrap();
    assert_eq!(radio.current_mode(), DemodMode::Wfm);
}

#[test]
fn test_radio_module_mode_switching() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    let modes = [
        DemodMode::Wfm,
        DemodMode::Nfm,
        DemodMode::Am,
        DemodMode::Usb,
        DemodMode::Lsb,
        DemodMode::Dsb,
        DemodMode::Cw,
        DemodMode::Raw,
    ];
    for mode in modes {
        radio.set_mode(mode).unwrap();
        assert_eq!(radio.current_mode(), mode);
    }
}

#[test]
fn test_radio_module_process_nfm() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    // Generate FM-modulated signal
    let input: Vec<Complex> = (0..1000)
        .map(|i| {
            let phase = 2.0 * PI * 1000.0 * (i as f32) / 50_000.0;
            Complex::new(phase.cos(), phase.sin())
        })
        .collect();
    let mut output = vec![Stereo::default(); 2000];
    let count = radio.process(&input, &mut output).unwrap();
    // NFM: 50kHz -> 48kHz, so output count should be ~960
    assert!(count > 0, "should produce output");
    assert!(count <= 2000, "should not overflow");
}

#[test]
fn test_radio_module_process_am() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Am).unwrap();

    // AM signal: carrier with amplitude modulation
    let input: Vec<Complex> = (0..1000)
        .map(|i| {
            let amp = 1.0 + 0.5 * (2.0 * PI * 0.01 * i as f32).sin();
            Complex::new(amp, 0.0)
        })
        .collect();
    let mut output = vec![Stereo::default(); 5000];
    let count = radio.process(&input, &mut output).unwrap();
    // AM: 15kHz -> 48kHz, output should be upsampled
    assert!(count > 0, "should produce output");
}

#[test]
fn test_radio_module_process_raw() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Raw).unwrap();

    let input = vec![Complex::new(0.5, -0.3); 100];
    let mut output = vec![Stereo::default(); 200];
    let count = radio.process(&input, &mut output).unwrap();
    // Raw: 48kHz -> 48kHz, no resampling needed
    assert_eq!(count, 100);
    // Should pass through IQ as stereo (after IF chain which is passthrough when disabled)
    assert!((output[0].l - 0.5).abs() < 1e-4);
    assert!((output[0].r - (-0.3)).abs() < 1e-4);
}

#[test]
fn test_radio_module_process_empty() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    let mut output = vec![Stereo::default(); 100];
    let count = radio.process(&[], &mut output).unwrap();
    assert_eq!(count, 0);
}

#[test]
fn test_radio_module_squelch() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_squelch_enabled(true);
    radio.set_squelch(10.0); // very high threshold

    let input = vec![Complex::new(0.001, 0.0); 500];
    let mut output = vec![Stereo::default(); 1000];
    let count = radio.process(&input, &mut output).unwrap();
    assert!(count > 0);
    // All output should be near zero (squelch closed)
    let peak = output[..count]
        .iter()
        .map(|s| s.l.abs().max(s.r.abs()))
        .fold(0.0_f32, f32::max);
    assert!(peak < 0.01, "squelch should mute output, peak = {peak}");
}

/// FM-modulated audio tone at the NFM IF rate (50 kHz), for the
/// pre-gate tests: the demod output is a clean tone the gates would
/// otherwise hide.
fn fm_tone_iq(count: usize, amplitude: f32) -> Vec<Complex> {
    const IF_RATE_HZ: f32 = 50_000.0;
    const TONE_HZ: f32 = 1_000.0;
    const DEVIATION_HZ: f32 = 3_000.0;
    let mut phase = 0.0_f32;
    (0..count)
        .map(|i| {
            let t = i as f32 / IF_RATE_HZ;
            let inst_freq = DEVIATION_HZ * (2.0 * PI * TONE_HZ * t).sin();
            phase += 2.0 * PI * inst_freq / IF_RATE_HZ;
            Complex::new(amplitude * phase.cos(), amplitude * phase.sin())
        })
        .collect()
}

fn peak(s: &[Stereo]) -> f32 {
    s.iter()
        .map(|v| v.l.abs().max(v.r.abs()))
        .fold(0.0_f32, f32::max)
}

/// #734 — the speaker stays hard-muted while the power squelch is
/// closed, but `pre_gate_audio()` must still carry the demodulated
/// signal for the APT / SSTV taps.
#[test]
fn pre_gate_audio_survives_a_closed_power_squelch() {
    const WEAK_AMPLITUDE: f32 = 0.001;
    const HIGH_THRESHOLD_DB: f32 = 10.0;
    const BLOCK: usize = 2_000;
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    radio.set_squelch_enabled(true);
    radio.set_squelch(HIGH_THRESHOLD_DB);

    let input = fm_tone_iq(BLOCK, WEAK_AMPLITUDE);
    let mut output = vec![Stereo::default(); radio.max_output_samples(BLOCK)];
    let count = radio.process(&input, &mut output).unwrap();
    assert!(count > 0);
    assert!(
        !radio.if_chain().squelch_open(),
        "test premise: gate closed"
    );
    // `peak` is non-negative, so `<= 0.0` means exactly zero.
    assert!(
        peak(&output[..count]) <= 0.0,
        "speaker output is hard-muted"
    );

    let pre_gate = radio.pre_gate_audio();
    assert_eq!(pre_gate.len(), count);
    assert!(
        peak(pre_gate) > 0.0,
        "pre-gate audio keeps the demodulated tone"
    );
}

/// #734 — same contract for a closed voice squelch (APT / SSTV tones
/// have no speech cadence, so Syllabic / Snr stay closed on them).
#[test]
fn pre_gate_audio_survives_a_closed_voice_squelch() {
    use sdr_dsp::voice_squelch::VoiceSquelchMode;
    // Two seconds at the 50 kHz IF rate: long enough for the
    // Syllabic envelope filter's start-up transient to decay so the
    // steady tone reads as "no cadence".
    const BLOCK: usize = 100_000;
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    // A steady tone has no speech cadence, so Syllabic stays closed.
    radio
        .set_voice_squelch_mode(VoiceSquelchMode::Syllabic {
            threshold: sdr_dsp::voice_squelch::VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        })
        .unwrap();

    let input = fm_tone_iq(BLOCK, 1.0);
    let mut output = vec![Stereo::default(); radio.max_output_samples(BLOCK)];
    let count = radio.process(&input, &mut output).unwrap();
    assert!(count > 0);
    assert!(
        !radio.voice_squelch_open(),
        "test premise: voice gate closed"
    );
    // `peak` is non-negative, so `<= 0.0` means exactly zero.
    assert!(
        peak(&output[..count]) <= 0.0,
        "speaker output is hard-muted"
    );
    assert!(
        peak(radio.pre_gate_audio()) > 0.0,
        "pre-gate audio keeps the tone"
    );
}

/// #734 (CR round 1 on PR #791) — same contract at the module level.
#[test]
fn pre_gate_audio_is_cleared_by_empty_input() {
    const BLOCK: usize = 2_000;
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    let input = fm_tone_iq(BLOCK, 1.0);
    let mut output = vec![Stereo::default(); radio.max_output_samples(BLOCK)];
    let count = radio.process(&input, &mut output).unwrap();
    assert_eq!(
        radio.pre_gate_audio().len(),
        count,
        "test premise: a block is retained"
    );
    assert_eq!(radio.process(&[], &mut output).unwrap(), 0);
    assert!(
        radio.pre_gate_audio().is_empty(),
        "empty input must clear the retained block"
    );
}

#[test]
fn test_radio_module_deemphasis() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Wfm).unwrap();
    // Enable deemphasis
    radio.set_deemp_mode(DeemphasisMode::Eu50).unwrap();
    assert!(radio.demod_config().deemp_allowed);

    // Switch to a mode that doesn't support deemphasis
    radio.set_mode(DemodMode::Am).unwrap();
    assert!(!radio.demod_config().deemp_allowed);
}

#[test]
fn test_radio_module_deemp_mode_tau() {
    assert!((DeemphasisMode::Us75.tau() - 75e-6).abs() < 1e-10);
    assert!((DeemphasisMode::Eu50.tau() - 50e-6).abs() < 1e-10);
    assert!((DeemphasisMode::None.tau() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_radio_module_config_access() {
    let radio = RadioModule::with_default_rate().unwrap();
    let cfg = radio.demod_config();
    assert!(cfg.if_sample_rate > 0.0);
    assert!(cfg.af_sample_rate > 0.0);
}

#[test]
fn test_radio_module_if_chain_access() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.if_chain_mut().set_nb_enabled(true);
    assert!(radio.if_chain().nb_enabled());
}

#[test]
fn test_radio_module_set_bandwidth() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Usb).unwrap();
    // Should not panic or error
    radio.set_bandwidth(3000.0);
}

#[test]
fn test_radio_error_display() {
    let err = RadioError::Dsp(DspError::InvalidParameter("test".to_string()));
    let msg = format!("{err}");
    assert!(msg.contains("DSP error"));

    let err = RadioError::ModeSwitchFailed("test".to_string());
    let msg = format!("{err}");
    assert!(msg.contains("mode switch failed"));
}

#[test]
fn test_radio_module_auto_squelch() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_squelch_enabled(true);
    radio.set_auto_squelch_enabled(true);

    // Verify auto-squelch is enabled on the IF chain
    assert!(radio.if_chain().auto_squelch_enabled());

    // Disable and verify
    radio.set_auto_squelch_enabled(false);
    assert!(!radio.if_chain().auto_squelch_enabled());
}

#[test]
fn test_radio_module_mode_switch_preserves_deemp() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Wfm).unwrap();
    radio.set_deemp_mode(DeemphasisMode::Eu50).unwrap();

    // Switch to another FM mode (NFM doesn't support deemp)
    radio.set_mode(DemodMode::Nfm).unwrap();
    // Deemp mode should be preserved in the radio module
    // but disabled in the AF chain since NFM doesn't allow it

    // Switch back to WFM
    radio.set_mode(DemodMode::Wfm).unwrap();
    // The deemp mode is still Eu50 in the radio, and WFM allows it
    assert!(radio.af_chain().deemp_enabled());
}

#[test]
fn test_radio_module_ctcss_threshold_persists_across_set_mode() {
    // RadioModule caches ctcss_threshold and reapplies it to
    // the new AF chain on mode switch. Without the persistence,
    // a mode change would snap the threshold back to the
    // DSP-layer default and silently un-tune the user's setting.
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_ctcss_threshold(CTCSS_PERSIST_THRESHOLD).unwrap();
    assert!((radio.ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS);
    assert!((radio.af_chain().ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS);

    // Mode switch rebuilds the AF chain from scratch. The
    // cached threshold must survive AND be reapplied to the
    // new chain, not just stored on the RadioModule.
    radio.set_mode(DemodMode::Nfm).unwrap();
    assert!((radio.ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS);
    assert!((radio.af_chain().ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS);

    radio.set_mode(DemodMode::Am).unwrap();
    assert!((radio.ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS);
    assert!((radio.af_chain().ctcss_threshold() - CTCSS_PERSIST_THRESHOLD).abs() < CTCSS_TEST_EPS);
}

#[test]
fn test_radio_module_ctcss_threshold_rejects_invalid() {
    // Invalid values must fail fast at the RadioModule boundary
    // (not deep in the DSP layer) and must NOT corrupt either
    // the cached value OR the live AF-chain detector state.
    // The RadioModule cache advances only after the AF chain
    // accepts the new value, so a correctly-ordered setter
    // leaves both in sync on rejection. Checking both levels
    // pins that invariant — a regression that mutated one
    // without the other (e.g. af_chain storing the bad value
    // before the range check, or cache advancing before
    // validation) would slip past a cache-only assertion.
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio
        .set_ctcss_threshold(CTCSS_LAST_GOOD_THRESHOLD)
        .unwrap();

    // Match on the exact error variant (not just `is_err`) so
    // a future refactor can't mask the failure with a wrong
    // error type (e.g. accidentally promoting to
    // `RadioError::ModeSwitchFailed`).
    for v in INVALID_CTCSS_THRESHOLDS {
        assert!(
            matches!(
                radio.set_ctcss_threshold(v),
                Err(RadioError::Dsp(DspError::InvalidParameter(_)))
            ),
            "threshold {v} should produce Err(RadioError::Dsp(DspError::InvalidParameter(_)))"
        );
        // After every single rejection, BOTH the cached value
        // and the AF chain's effective value must still be
        // the last-good baseline. Re-asserting inside the loop
        // (not just after) catches a hypothetical bug where
        // the first rejected value corrupts one layer and
        // subsequent rejected values corrupt the other —
        // a post-loop assertion on the final state would
        // miss that.
        assert!(
            (radio.ctcss_threshold() - CTCSS_LAST_GOOD_THRESHOLD).abs() < CTCSS_TEST_EPS,
            "RadioModule cache drifted after rejected value {v}"
        );
        assert!(
            (radio.af_chain().ctcss_threshold() - CTCSS_LAST_GOOD_THRESHOLD).abs() < CTCSS_TEST_EPS,
            "AF chain effective threshold drifted after rejected value {v}"
        );
    }
}

// ─── Voice squelch persistence regression tests ─────────
//
// Mirror the CTCSS dual-level assertion pattern: after each
// mode switch, assert both the RadioModule cache AND the
// live AfChain value so a broken reapply path (cache
// updated but af_chain not) can't hide behind the cached
// field alone. Tests three transitions (Off → Syllabic,
// Syllabic → Snr, mode switch) to cover the
// reconstruct-the-AF-chain-on-set_mode code path.

#[test]
fn test_radio_module_voice_squelch_persists_across_set_mode() {
    use sdr_dsp::voice_squelch::VoiceSquelchMode;

    let mut radio = RadioModule::with_default_rate().unwrap();
    // Baseline: default mode is Off at both levels. Radio
    // starts in WFM mode (RadioModule default).
    assert_eq!(radio.voice_squelch_mode(), VoiceSquelchMode::Off);
    assert_eq!(radio.af_chain().voice_squelch_mode(), VoiceSquelchMode::Off);
    assert_eq!(radio.current_mode(), DemodMode::Wfm);

    // Set a non-default Syllabic mode via the direct setter
    // while on WFM. The setter caches the user's choice but
    // applies it LIVE only on NFM — the same invariant
    // `set_mode` enforces. Bookmark recall sends
    // `SetDemodMode(Wfm)` then `SetVoiceSquelchMode(Syllabic)`;
    // applying it live muted broadcast audio with the control
    // hidden (#737).
    let syl = VoiceSquelchMode::Syllabic {
        threshold: VS_SYLLABIC_PERSIST_THRESHOLD,
    };
    radio.set_voice_squelch_mode(syl).unwrap();
    assert_eq!(radio.voice_squelch_mode(), syl);
    assert_eq!(
        radio.af_chain().voice_squelch_mode(),
        VoiceSquelchMode::Off,
        "direct setter must not arm voice squelch live on WFM (#737)"
    );

    // Mode switch to NFM: the AF chain is rebuilt from
    // scratch. The NFM gate passes, so the cached Syllabic
    // mode applies live on the new chain.
    radio.set_mode(DemodMode::Nfm).unwrap();
    assert_eq!(radio.voice_squelch_mode(), syl);
    assert_eq!(radio.af_chain().voice_squelch_mode(), syl);

    // Switch AWAY from NFM to AM. The cache must preserve
    // the user's Syllabic setting, but the live AF chain
    // must be forced to Off — voice squelch is calibrated
    // for speech and doesn't apply to AM.
    radio.set_mode(DemodMode::Am).unwrap();
    assert_eq!(
        radio.voice_squelch_mode(),
        syl,
        "cache must preserve user's setting across non-NFM transitions"
    );
    assert_eq!(
        radio.af_chain().voice_squelch_mode(),
        VoiceSquelchMode::Off,
        "live AF chain must NOT run voice squelch on AM"
    );

    // Back to NFM — the cached Syllabic must re-apply live
    // without user intervention. This is the core reason
    // we preserve the cache across non-NFM modes: the user
    // doesn't have to re-pick voice squelch every time they
    // visit a non-NFM band and come back.
    radio.set_mode(DemodMode::Nfm).unwrap();
    assert_eq!(radio.voice_squelch_mode(), syl);
    assert_eq!(
        radio.af_chain().voice_squelch_mode(),
        syl,
        "cached setting must re-arm on NFM re-entry"
    );

    // Flip to Snr and run a NFM → WFM → NFM gauntlet.
    let snr = VoiceSquelchMode::Snr {
        threshold_db: VS_SNR_PERSIST_THRESHOLD_DB,
    };
    radio.set_voice_squelch_mode(snr).unwrap();
    radio.set_mode(DemodMode::Wfm).unwrap();
    assert_eq!(radio.voice_squelch_mode(), snr);
    assert_eq!(
        radio.af_chain().voice_squelch_mode(),
        VoiceSquelchMode::Off,
        "WFM must not run voice squelch live"
    );
    radio.set_mode(DemodMode::Nfm).unwrap();
    assert_eq!(radio.voice_squelch_mode(), snr);
    assert_eq!(radio.af_chain().voice_squelch_mode(), snr);

    // Explicitly set Off — must stay Off through any mode.
    radio.set_voice_squelch_mode(VoiceSquelchMode::Off).unwrap();
    radio.set_mode(DemodMode::Wfm).unwrap();
    assert_eq!(radio.voice_squelch_mode(), VoiceSquelchMode::Off);
    assert_eq!(radio.af_chain().voice_squelch_mode(), VoiceSquelchMode::Off);
}

/// #737 (CR round 1 on PR #790) — on non-NFM the setter forces the
/// live chain to Off, which must not let an invalid threshold skip
/// validation, enter the cache, and then fail on NFM re-entry.
#[test]
fn test_voice_squelch_setter_validates_before_forcing_off_on_non_nfm() {
    use sdr_dsp::voice_squelch::VoiceSquelchMode;
    /// Non-positive Syllabic boundary: the detector requires a
    /// finite, strictly positive envelope ratio, so `-1.0` is the
    /// simplest value that must be rejected.
    const INVALID_THRESHOLD: f32 = -1.0;

    let mut radio = RadioModule::with_default_rate().unwrap();
    assert_eq!(radio.current_mode(), DemodMode::Wfm);
    let bad = VoiceSquelchMode::Syllabic {
        threshold: INVALID_THRESHOLD,
    };
    assert!(
        radio.set_voice_squelch_mode(bad).is_err(),
        "an invalid threshold must be rejected even while non-NFM"
    );
    assert_eq!(
        radio.voice_squelch_mode(),
        VoiceSquelchMode::Off,
        "a rejected mode must not be cached"
    );
    // NFM re-entry replays the cache — which must still be valid.
    radio.set_mode(DemodMode::Nfm).unwrap();
    assert_eq!(radio.af_chain().voice_squelch_mode(), VoiceSquelchMode::Off);
}

/// #737 (CR round 2 on PR #790) — while the live chain is `Off`
/// (non-NFM) the AF-chain threshold setter is a no-op, so the
/// cached mode must be validated here or a bad value is replayed
/// on NFM re-entry.
#[test]
fn test_voice_squelch_threshold_validates_cached_mode_on_non_nfm() {
    use sdr_dsp::voice_squelch::VoiceSquelchMode;
    /// Non-positive Syllabic boundary: the detector requires a
    /// finite, strictly positive envelope ratio.
    const INVALID_THRESHOLD: f32 = -1.0;

    let mut radio = RadioModule::with_default_rate().unwrap();
    assert_eq!(radio.current_mode(), DemodMode::Wfm);
    let syl = VoiceSquelchMode::Syllabic {
        threshold: VS_SYLLABIC_BASELINE_THRESHOLD,
    };
    radio.set_voice_squelch_mode(syl).unwrap();
    assert!(
        radio
            .set_voice_squelch_threshold(INVALID_THRESHOLD)
            .is_err(),
        "an invalid threshold must be rejected while the live chain is Off"
    );
    assert_eq!(radio.voice_squelch_mode(), syl, "cache must be unchanged");
    radio.set_mode(DemodMode::Nfm).unwrap();
    assert_eq!(radio.af_chain().voice_squelch_mode(), syl);
}

#[test]
fn test_radio_module_voice_squelch_threshold_updates_cached_mode() {
    // `set_voice_squelch_threshold` has to mirror the new
    // value into the cached `voice_squelch_mode` variant
    // so that `set_mode`'s replay carries the tuned value
    // forward. Regression test: tune a threshold, switch
    // modes, confirm the tuned value is what gets reapplied.
    use sdr_dsp::voice_squelch::VoiceSquelchMode;

    let mut radio = RadioModule::with_default_rate().unwrap();
    // Start on NFM: the direct setter only arms the detector
    // live on NFM (#737), and this test is about the live
    // threshold being mirrored into the cache.
    radio.set_mode(DemodMode::Nfm).unwrap();
    radio
        .set_voice_squelch_mode(VoiceSquelchMode::Syllabic {
            threshold: VS_SYLLABIC_BASELINE_THRESHOLD,
        })
        .unwrap();
    radio
        .set_voice_squelch_threshold(VS_SYLLABIC_TUNED_THRESHOLD)
        .unwrap();

    // Cached mode should now carry the tuned threshold, not
    // the construction-time default.
    assert_eq!(
        radio.voice_squelch_mode(),
        VoiceSquelchMode::Syllabic {
            threshold: VS_SYLLABIC_TUNED_THRESHOLD
        }
    );
    assert_eq!(
        radio.af_chain().voice_squelch_mode(),
        VoiceSquelchMode::Syllabic {
            threshold: VS_SYLLABIC_TUNED_THRESHOLD
        }
    );

    // Mode switch rebuilds the AF chain; the tuned value
    // must survive through the replay.
    radio.set_mode(DemodMode::Nfm).unwrap();
    assert_eq!(
        radio.voice_squelch_mode(),
        VoiceSquelchMode::Syllabic {
            threshold: VS_SYLLABIC_TUNED_THRESHOLD
        }
    );
    assert_eq!(
        radio.af_chain().voice_squelch_mode(),
        VoiceSquelchMode::Syllabic {
            threshold: VS_SYLLABIC_TUNED_THRESHOLD
        }
    );
}

/// #738 — the squelch close edge is ramped by the AF envelope on
/// real audio instead of being hard-zeroed first: the first closed
/// block still carries a decaying tail, and the speaker reaches
/// exact silence once the release has settled.
#[test]
fn squelch_close_edge_is_ramped_then_exactly_silent() {
    /// IQ amplitude that sits well above the squelch threshold
    /// (−6 dBFS vs −30 dB) so the gate opens on the first block.
    const STRONG_AMPLITUDE: f32 = 0.5;
    /// IQ amplitude 30 dB below the threshold so the gate closes
    /// on the first weak block (the FM tone is still present, so
    /// the demod keeps producing audio for the ramp to act on).
    const WEAK_AMPLITUDE: f32 = 0.001;
    /// Manual threshold between the two amplitudes; the production
    /// default is −100 dB (effectively open), which would never
    /// close here.
    const THRESHOLD_DB: f32 = -30.0;
    /// ~42 ms of IQ at the 48 kHz NFM IF rate — a typical DSP block,
    /// short enough that the release is still audibly ramping when
    /// the block ends.
    const BLOCK: usize = 2_000;
    /// Upper bound on closed blocks to reach exact silence: 50 ×
    /// 42 ms ≈ 2 s, far beyond the release time constant, so a
    /// settle that never happens fails the test rather than hides.
    const SETTLE_BLOCKS: usize = 50;
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    radio.set_squelch_enabled(true);
    radio.set_squelch(THRESHOLD_DB);
    let mut output = vec![Stereo::default(); radio.max_output_samples(BLOCK)];

    let strong = fm_tone_iq(BLOCK, STRONG_AMPLITUDE);
    for _ in 0..5 {
        radio.process(&strong, &mut output).unwrap();
    }
    assert!(radio.if_chain().squelch_open(), "test premise: gate open");

    let weak = fm_tone_iq(BLOCK, WEAK_AMPLITUDE);
    let count = radio.process(&weak, &mut output).unwrap();
    assert!(
        !radio.if_chain().squelch_open(),
        "test premise: gate closed"
    );
    assert!(
        peak(&output[..count]) > 0.0,
        "first closed block must carry the release ramp, not a hard step"
    );
    assert!(
        output[0].l.abs() >= output[count - 1].l.abs(),
        "the ramp decays across the block"
    );

    let mut silent = false;
    for _ in 0..SETTLE_BLOCKS {
        let count = radio.process(&weak, &mut output).unwrap();
        if peak(&output[..count]) <= 0.0 {
            silent = true;
            break;
        }
    }
    assert!(
        silent,
        "speaker must reach exact silence once the release settles"
    );
}

/// #738 — the software IF AGC only runs in modes whose config
/// allows it; Raw / LRPT / CW keep their amplitude. The user's
/// preference is remembered and re-applied on a mode that allows it.
#[test]
fn software_if_agc_is_gated_by_the_demod_config() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    radio.set_software_agc_enabled(true);
    assert!(radio.if_chain().software_agc_enabled());

    radio.set_mode(DemodMode::Raw).unwrap();
    assert!(!radio.demod_config().if_agc_allowed);
    assert!(
        !radio.if_chain().software_agc_enabled(),
        "Raw passes IQ through untouched"
    );
    radio.set_software_agc_enabled(true);
    assert!(
        !radio.if_chain().software_agc_enabled(),
        "cannot be enabled live on Raw"
    );

    radio.set_mode(DemodMode::Nfm).unwrap();
    assert!(
        radio.if_chain().software_agc_enabled(),
        "preference re-applied on NFM"
    );
    radio.set_software_agc_enabled(false);
    radio.set_mode(DemodMode::Wfm).unwrap();
    assert!(!radio.if_chain().software_agc_enabled());
}

/// Codacy on PR #800 — the block in which the release crosses the
/// settle threshold must keep its ramp: the last audible block ends
/// below −60 dB of the open level, so exact silence never starts
/// with a step.
#[test]
fn squelch_release_reaches_silence_without_a_step() {
    /// Opens the gate on the first block (see the sibling test).
    const STRONG_AMPLITUDE: f32 = 0.5;
    /// Closes the gate while the demod still produces audio.
    const WEAK_AMPLITUDE: f32 = 0.001;
    /// Manual threshold between the two amplitudes.
    const THRESHOLD_DB: f32 = -30.0;
    /// ~0.4 s of IQ — a block long enough for the release to settle
    /// *inside* it, so zeroing the whole block (the bug) would step
    /// from full level to silence.
    const BLOCK: usize = 20_000;
    /// Upper bound on closed blocks: 50 × 0.4 s ≫ the release time
    /// constant, so a release that never settles fails loudly.
    const SETTLE_BLOCKS: usize = 50;
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    radio.set_squelch_enabled(true);
    radio.set_squelch(THRESHOLD_DB);
    let mut output = vec![Stereo::default(); radio.max_output_samples(BLOCK)];

    let strong = fm_tone_iq(BLOCK, STRONG_AMPLITUDE);
    let mut open_peak = 0.0_f32;
    for _ in 0..5 {
        let count = radio.process(&strong, &mut output).unwrap();
        open_peak = peak(&output[..count]);
    }
    assert!(radio.if_chain().squelch_open() && open_peak > 0.0);

    let weak = fm_tone_iq(BLOCK, WEAK_AMPLITUDE);
    let mut last_audible_tail = f32::NAN;
    let mut silent = false;
    for _ in 0..SETTLE_BLOCKS {
        let count = radio.process(&weak, &mut output).unwrap();
        if peak(&output[..count]) <= 0.0 {
            silent = true;
            break;
        }
        last_audible_tail = output[count - 1].l.abs().max(output[count - 1].r.abs());
    }
    assert!(silent, "must reach exact silence");
    assert!(
        last_audible_tail < open_peak * 1e-3,
        "last audible block must end below -60 dB before silence, got {last_audible_tail} (open {open_peak})"
    );
}
