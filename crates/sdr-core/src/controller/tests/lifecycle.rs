use super::*;

#[test]
fn dsp_state_creates_successfully() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let state = DspState::new(dsp_tx).unwrap();
    assert!(!state.running);
    assert!(state.source.is_none());
    assert_eq!(state.iq_buf.len(), IQ_PAIRS_PER_READ);
    assert_eq!(state.fft_buf.len(), DEFAULT_FFT_SIZE);
    // VFO starts as None (created on device open).
    assert!(state.vfo.is_none());
    assert!((state.vfo_offset - 0.0).abs() < f64::EPSILON);
}

#[test]
fn dsp_state_rtl_sdr_persist_fields_default_correctly() {
    // Pins the persist-and-reapply defaults for issue #551.
    // Each of these fields is replayed unconditionally to a
    // freshly-opened RTL-SDR source; defaults must match the
    // dongle's power-on state so first launch (no persisted
    // dispatch yet) doesn't change hardware behavior.
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let state = DspState::new(dsp_tx).unwrap();
    assert!(!state.bias_tee_enabled);
    assert_eq!(state.direct_sampling_mode, 0);
    assert!(!state.offset_tuning_enabled);
    assert!(!state.rtl_agc_enabled);
    assert!(!state.tuner_agc_auto);
    assert_eq!(state.tuner_gain_tenths_db, 0);
    assert!(state.tuner_gain_index.is_none());
    assert_eq!(state.ppm_correction, 0);
}

/// #693 — `DisconnectRtlTcp` must tear the session down through
/// `cleanup()` like every other stop path, so the audio sink is
/// stopped (otherwise the next Play hits `AlreadyRunning` and audio
/// is latched offline for the rest of the session).
#[test]
fn disconnect_rtl_tcp_runs_cleanup() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.source_type = SourceType::RtlTcp;
    state.audio_sink_type = AudioSinkType::Network;
    let _ = drain(&dsp_rx);
    handle_command(&mut state, &dsp_tx, UiToDsp::DisconnectRtlTcp);
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive))),
        "cleanup() emits NetworkSinkStatus::Inactive; got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, DspToUi::SourceStopped)),
        "expected SourceStopped; got {events:?}"
    );
    assert!(!state.running);
}

/// #693 — the no-source guard in `process_iq_block` (reachable after a
/// failed `rtl_tcp` rebuild) must also go through `cleanup()`.
#[test]
fn process_iq_block_without_source_runs_cleanup() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.audio_sink_type = AudioSinkType::Network;
    state.running = true;
    state.source = None;
    let fft_shared = SharedFftBuffer::new(state.frontend.fft_size());
    let _ = drain(&dsp_rx);
    process_iq_block(&mut state, &dsp_tx, &fft_shared);
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive))),
        "cleanup() emits NetworkSinkStatus::Inactive; got {events:?}"
    );
    assert!(!state.running);
}

/// #755 — a squelch that is not gating (manual off, auto off) reports
/// "open" permanently; that must never be presented to the scanner as
/// a carrier, or it latches in Listening on the first channel.
#[test]
fn scanner_carrier_requires_a_gating_squelch() {
    assert!(!scanner_carrier_present(false, true));
    assert!(!scanner_carrier_present(false, false));
    assert!(scanner_carrier_present(true, true));
    assert!(!scanner_carrier_present(true, false));
}

/// #755 — enabling the scanner without any squelch gating is a
/// configuration the user needs to hear about.
#[test]
fn enabling_scanner_without_squelch_warns() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetSquelchEnabled(false));
    handle_command(&mut state, &dsp_tx, UiToDsp::SetAutoSquelch(false));
    let _ = drain(&dsp_rx);
    handle_command(&mut state, &dsp_tx, UiToDsp::SetScannerEnabled(true));
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::Error(m) if m.to_lowercase().contains("squelch"))),
        "expected a squelch warning, got {events:?}"
    );

    // With squelch gating there is no warning.
    handle_command(&mut state, &dsp_tx, UiToDsp::SetScannerEnabled(false));
    handle_command(&mut state, &dsp_tx, UiToDsp::SetSquelchEnabled(true));
    let _ = drain(&dsp_rx);
    handle_command(&mut state, &dsp_tx, UiToDsp::SetScannerEnabled(true));
    let events = drain(&dsp_rx);
    assert!(
        !events.iter().any(|e| matches!(e, DspToUi::Error(_))),
        "unexpected error: {events:?}"
    );
}
