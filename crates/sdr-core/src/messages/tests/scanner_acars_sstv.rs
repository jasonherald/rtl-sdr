use super::*;

/// Part 1 of 2 of the variant-construction sweep (`ChannelKey` … `Off`).
#[test]
fn test_scanner_dsp_to_ui_variants_1() {
    // Shape regression for the four scanner events added in
    // PR 2 of #317. Catches silent payload changes — if a
    // field gets renamed or the tuple arity changes, the
    // pattern match here fails at compile or runtime.
    let key = sdr_scanner::ChannelKey {
        name: "Test".to_string(),
        frequency_hz: 162_550_000,
    };
    let active = DspToUi::ScannerActiveChannelChanged {
        key: Some(key.clone()),
        freq_hz: 162_550_000,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth: TEST_BANDWIDTH_HZ,
        name: "Test".to_string(),
        ctcss: Some(CtcssMode::Off),
        voice_squelch: None,
    };
    assert!(matches!(
        active,
        DspToUi::ScannerActiveChannelChanged {
            key: Some(_),
            freq_hz: 162_550_000,
            demod_mode: sdr_types::DemodMode::Nfm,
            ctcss: Some(CtcssMode::Off),
            voice_squelch: None,
            ..
        }
    ));
}

/// Part 2 of 2 of the variant-construction sweep (`ScannerActiveChannelChanged` … `ScannerMutexStopped`).
#[test]
fn test_scanner_dsp_to_ui_variants_2() {
    let idle = DspToUi::ScannerActiveChannelChanged {
        key: None,
        freq_hz: 0,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth: 0.0,
        name: String::new(),
        ctcss: None,
        voice_squelch: None,
    };
    assert!(matches!(
        idle,
        DspToUi::ScannerActiveChannelChanged { key: None, .. }
    ));

    let state_changed = DspToUi::ScannerStateChanged(sdr_scanner::ScannerState::Listening);
    assert!(matches!(
        state_changed,
        DspToUi::ScannerStateChanged(sdr_scanner::ScannerState::Listening)
    ));

    let empty = DspToUi::ScannerEmptyRotation;
    assert!(matches!(empty, DspToUi::ScannerEmptyRotation));

    // Pin each mutex-reason variant — the UI toast text is
    // selected by matching these, so a silent rename would
    // misroute toasts rather than fail compilation.
    let mutex_rec = DspToUi::ScannerMutexStopped(ScannerMutexReason::RecordingStoppedForScanner);
    assert!(matches!(
        mutex_rec,
        DspToUi::ScannerMutexStopped(ScannerMutexReason::RecordingStoppedForScanner)
    ));
    let mutex_scan_rec =
        DspToUi::ScannerMutexStopped(ScannerMutexReason::ScannerStoppedForRecording);
    assert!(matches!(
        mutex_scan_rec,
        DspToUi::ScannerMutexStopped(ScannerMutexReason::ScannerStoppedForRecording)
    ));
}

#[test]
fn test_scanner_ui_to_dsp_variants() {
    // Shape regression for the four scanner commands the UI
    // dispatches. Same rationale as the DspToUi test above —
    // enum-shape drift fails here first.
    let key = sdr_scanner::ChannelKey {
        name: "Test".to_string(),
        frequency_hz: 146_520_000,
    };

    let enable = UiToDsp::SetScannerEnabled(true);
    assert!(matches!(enable, UiToDsp::SetScannerEnabled(true)));

    let disable = UiToDsp::SetScannerEnabled(false);
    assert!(matches!(disable, UiToDsp::SetScannerEnabled(false)));

    let update = UiToDsp::UpdateScannerChannels(Vec::new());
    assert!(matches!(
        update,
        UiToDsp::UpdateScannerChannels(ref v) if v.is_empty()
    ));

    let lockout = UiToDsp::LockoutScannerChannel(key.clone());
    assert!(matches!(lockout, UiToDsp::LockoutScannerChannel(_)));

    let unlock = UiToDsp::UnlockScannerChannel(key);
    assert!(matches!(unlock, UiToDsp::UnlockScannerChannel(_)));
}

#[test]
fn acars_set_enabled_round_trips_debug() {
    let cmd = UiToDsp::SetAcarsEnabled(true);
    let s = format!("{cmd:?}");
    assert!(s.contains("SetAcarsEnabled"), "got {s}");
    assert!(s.contains("true"), "got {s}");
}

#[test]
fn acars_set_region_constructs() {
    // Wire-contract pin for the variant added by issue #581.
    // CR round 1 on PR #593 flagged the absence of a
    // shape-regression test alongside the other UiToDsp
    // checks; this is the matching `matches!` assertion.
    let cmd = UiToDsp::SetAcarsRegion(crate::acars_airband_lock::AcarsRegion::Europe);
    assert!(matches!(
        cmd,
        UiToDsp::SetAcarsRegion(crate::acars_airband_lock::AcarsRegion::Europe)
    ));
}

#[test]
fn acars_enabled_changed_carries_error() {
    use crate::acars_airband_lock::AcarsEnableError;
    let msg = DspToUi::AcarsEnabledChanged(Err(AcarsEnableError::UnsupportedSourceType(
        crate::messages::SourceType::File,
    )));
    let s = format!("{msg:?}");
    assert!(s.contains("AcarsEnabledChanged"), "got {s}");
    assert!(s.contains("UnsupportedSourceType"), "got {s}");
}

#[test]
fn acars_output_error_variant_constructs() {
    let msg = DspToUi::AcarsOutputError {
        kind: "jsonl",
        message: "Could not open /tmp/acars.jsonl: permission denied".to_string(),
    };
    assert!(matches!(
        msg,
        DspToUi::AcarsOutputError { kind: "jsonl", .. }
    ));
}

#[test]
fn acars_output_ui_to_dsp_variants_construct() {
    // Pin the wire-level shape of the 5 SetAcars* output
    // commands. Catches accidental signature drift.
    let _: UiToDsp = UiToDsp::SetAcarsJsonlEnabled(true);
    let _: UiToDsp = UiToDsp::SetAcarsJsonlPath("x.jsonl".to_string());
    let _: UiToDsp = UiToDsp::SetAcarsNetworkEnabled(true);
    let _: UiToDsp = UiToDsp::SetAcarsNetworkAddr("127.0.0.1:5550".to_string());
    let _: UiToDsp = UiToDsp::SetAcarsStationId("STN1".to_string());
}

#[test]
fn sstv_dsp_to_ui_variants_construct() {
    // Shape regression for the two SSTV events added in
    // epic #472. Catches silent payload changes — if the
    // line_index field is renamed or the struct gains a
    // field, the pattern match here fails at compile or
    // runtime before it can silently break the UI handler.
    // Per CodeRabbit round 1 on PR #599.
    let line_decoded = DspToUi::SstvLineDecoded(42);
    assert!(matches!(
        line_decoded,
        DspToUi::SstvLineDecoded(idx) if idx == 42
    ));

    let image_complete = DspToUi::SstvImageComplete {
        width: 640,
        height: 496,
        pixels: vec![[255_u8, 0, 0]; 640 * 496],
    };
    assert!(matches!(
        image_complete,
        DspToUi::SstvImageComplete {
            width: 640,
            height: 496,
            ref pixels,
        } if pixels.len() == 640 * 496
    ));
}

#[test]
fn sstv_ui_to_dsp_variants_construct() {
    // Shape regression for the two SSTV control commands
    // added in epic #472. A future rename of `SetSstvImage`
    // / `ClearSstvImage` or a change to the
    // `SstvImageHandle` payload type trips this regression
    // net rather than silently breaking the controller's
    // `sstv_decode_tap` plumbing.
    // Per CodeRabbit round 1 on PR #599.
    let img = sdr_radio::sstv_image::SstvImage::new();
    let set_sstv = UiToDsp::SetSstvImage(img.handle());
    assert!(matches!(set_sstv, UiToDsp::SetSstvImage(_)));

    let clear_sstv = UiToDsp::ClearSstvImage;
    assert!(matches!(clear_sstv, UiToDsp::ClearSstvImage));
}
