use super::*;

#[test]
fn translate_apt_line_is_dropped_at_ffi_boundary() {
    // `DspToUi::AptLine` is intentionally dropped by the FFI
    // translation layer — the macOS frontend's APT viewer is a
    // separate ticket, and forwarding 2 KB-per-line image data
    // through a C ABI without a host consumer would be wasted
    // work. Pin that policy with a regression test: a future
    // change that exposes APT lines through the FFI (or
    // accidentally lets the variant fall through to the
    // catch-all panic arm) trips this assert before it can
    // reach the Mac side. Per CodeRabbit on PR #503.
    let line = sdr_core::messages::AptLine::default();
    let msg = DspToUi::AptLine(Box::new(line));
    assert!(
        translate_event(&msg).is_none(),
        "AptLine must not translate to a wire event yet",
    );
}

/// Build a minimal `AcarsMessage` for the FFI drop-policy
/// tests. `AcarsMessage` doesn't derive `Default` because
/// `SystemTime` has no canonical default; pin a fixture
/// here so the policy tests don't repeat the boilerplate.
fn dummy_acars_message() -> sdr_acars::AcarsMessage {
    sdr_acars::AcarsMessage {
        timestamp: std::time::SystemTime::UNIX_EPOCH,
        channel_idx: 0,
        freq_hz: 131_550_000.0,
        level_db: -42.0,
        error_count: 0,
        mode: b'2',
        label: *b"H1",
        block_id: b'A',
        ack: b'!',
        aircraft: arrayvec::ArrayString::from(".TEST123").unwrap_or_default(),
        flight_id: None,
        message_no: None,
        text: String::new(),
        end_of_message: true,
        reassembled_block_count: 1,
        parsed: None,
    }
}

#[test]
fn translate_acars_message_is_dropped_at_ffi_boundary() {
    // ACARS variants (epic #474) follow the same drop-here
    // policy as `AptLine` until the macOS frontend ships its
    // own Aviation viewer. Pin the contract: any future
    // change that surfaces ACARS through the C ABI without
    // also wiring the host consumer trips this assert.
    let msg = DspToUi::AcarsMessage(Box::new(dummy_acars_message()));
    assert!(
        translate_event(&msg).is_none(),
        "AcarsMessage must not translate to a wire event yet",
    );
}

#[test]
fn translate_acars_channel_stats_is_dropped_at_ffi_boundary() {
    // Cover three widths to exercise the variable-length
    // payload (post-#592 the variant is `Box<[ChannelStats]>`,
    // not the fixed-width `[ChannelStats; 6]`):
    //   - 1 channel (Custom region with a single freq)
    //   - ACARS_CHANNEL_COUNT (predefined US-6 / Europe)
    //   - ACARS_CHANNEL_COUNT + 1 (Custom region wider than
    //     the predefined count, up to MAX_CUSTOM_CHANNELS=8)
    // CR round 1 on PR #598.
    for n in [
        1,
        sdr_core::acars_airband_lock::ACARS_CHANNEL_COUNT,
        sdr_core::acars_airband_lock::ACARS_CHANNEL_COUNT + 1,
    ] {
        let msg = DspToUi::AcarsChannelStats(
            vec![sdr_acars::ChannelStats::default(); n].into_boxed_slice(),
        );
        assert!(
            translate_event(&msg).is_none(),
            "AcarsChannelStats(width={n}) must not translate to a wire event yet",
        );
    }
}

#[test]
fn translate_acars_enabled_changed_is_dropped_at_ffi_boundary() {
    let msg = DspToUi::AcarsEnabledChanged(Ok(true));
    assert!(
        translate_event(&msg).is_none(),
        "AcarsEnabledChanged(Ok) must not translate to a wire event yet",
    );
    let msg = DspToUi::AcarsEnabledChanged(Err(
        sdr_core::acars_airband_lock::AcarsEnableError::UnsupportedSourceType(
            sdr_core::messages::SourceType::File,
        ),
    ));
    assert!(
        translate_event(&msg).is_none(),
        "AcarsEnabledChanged(Err) must not translate to a wire event yet",
    );
}

#[test]
fn translate_acars_output_error_is_dropped_at_ffi_boundary() {
    let msg = DspToUi::AcarsOutputError {
        kind: "udp",
        message: "could not resolve host".to_string(),
    };
    assert!(
        translate_event(&msg).is_none(),
        "AcarsOutputError must not translate to a wire event yet",
    );
}

#[test]
fn translate_sstv_vis_detected_is_dropped_at_ffi_boundary() {
    // Same drop-at-boundary contract as `SstvLineDecoded` /
    // `SstvImageComplete` — the mode-display follow-up keeps
    // SSTV strictly Linux-only at the FFI boundary until the
    // macOS viewer ticket lands. Per epic #472 mode-display
    // follow-up.
    let msg = DspToUi::SstvVisDetected {
        mode_label: "PD120",
    };
    assert!(
        translate_event(&msg).is_none(),
        "SstvVisDetected must not translate to a wire event yet",
    );
}

#[test]
fn translate_sstv_line_decoded_is_dropped_at_ffi_boundary() {
    // SSTV variants (epic #472) are Linux-only for V1; the
    // macOS FFI layer will get its own ticket when the FFI
    // layer gains an SSTV viewer. Pin the drop-at-boundary
    // contract so a future change that accidentally surfaces
    // SSTV through the C ABI trips this assert first.
    // Per CodeRabbit round 1 on PR #599.
    let msg = DspToUi::SstvLineDecoded(0);
    assert!(
        translate_event(&msg).is_none(),
        "SstvLineDecoded must not translate to a wire event yet",
    );
}

#[test]
fn translate_sstv_image_complete_is_dropped_at_ffi_boundary() {
    // Same drop-at-boundary contract as `SstvLineDecoded`
    // above — the pixel Vec should never flow through the
    // C ABI until the macOS viewer ticket ships. Per
    // CodeRabbit round 1 on PR #599.
    let msg = DspToUi::SstvImageComplete {
        width: 1,
        height: 1,
        pixels: vec![[0_u8, 0, 0]],
    };
    assert!(
        translate_event(&msg).is_none(),
        "SstvImageComplete must not translate to a wire event yet",
    );
}
