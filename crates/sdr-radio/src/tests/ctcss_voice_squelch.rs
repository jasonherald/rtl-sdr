use super::*;

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
}

/// SNR mode follows the same cache / live split, and `Off` clears both.
#[test]
fn test_radio_module_voice_squelch_snr_persists_and_off_clears() {
    use sdr_dsp::voice_squelch::VoiceSquelchMode;

    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
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
