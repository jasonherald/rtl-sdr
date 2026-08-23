use super::*;

#[test]
fn test_dsp_to_ui_variants() {
    let fft = DspToUi::FftData(vec![1.0, 2.0, 3.0]);
    assert!(matches!(fft, DspToUi::FftData(v) if v.len() == 3));

    let snr = DspToUi::SignalLevel(12.5);
    assert!(matches!(snr, DspToUi::SignalLevel(s) if (s - 12.5).abs() < f32::EPSILON));

    let err = DspToUi::Error("test error".to_string());
    assert!(matches!(err, DspToUi::Error(ref s) if s == "test error"));

    let stopped = DspToUi::SourceStopped;
    assert!(matches!(stopped, DspToUi::SourceStopped));

    let sr = DspToUi::SampleRateChanged(2_400_000.0);
    assert!(matches!(sr, DspToUi::SampleRateChanged(r) if (r - 2_400_000.0).abs() < f64::EPSILON));

    let info = DspToUi::DeviceInfo("RTL2838UHIDIR".to_string());
    assert!(matches!(info, DspToUi::DeviceInfo(ref s) if s == "RTL2838UHIDIR"));

    let audio_rec = DspToUi::AudioRecordingStarted(std::path::PathBuf::from("/tmp/test.wav"));
    assert!(matches!(audio_rec, DspToUi::AudioRecordingStarted(_)));

    let audio_stop = DspToUi::AudioRecordingStopped;
    assert!(matches!(audio_stop, DspToUi::AudioRecordingStopped));

    let iq_rec = DspToUi::IqRecordingStarted(std::path::PathBuf::from("/tmp/iq.wav"));
    assert!(matches!(iq_rec, DspToUi::IqRecordingStarted(_)));

    let iq_stop = DspToUi::IqRecordingStopped;
    assert!(matches!(iq_stop, DspToUi::IqRecordingStopped));
}

#[test]
fn demod_mode_changed_message_constructs() {
    let m = DspToUi::DemodModeChanged(DemodMode::Nfm);
    assert!(matches!(m, DspToUi::DemodModeChanged(DemodMode::Nfm)));
}

#[test]
fn bandwidth_changed_message_constructs() {
    // Pins the variant shape + payload round-trip so future
    // refactors that accidentally change the f64 carrier
    // (e.g. to `u32` Hz or a `Bandwidth` newtype) trip this
    // test.
    let bw = DspToUi::BandwidthChanged(TEST_BANDWIDTH_HZ);
    assert!(
        matches!(bw, DspToUi::BandwidthChanged(v) if (v - TEST_BANDWIDTH_HZ).abs() < f64::EPSILON)
    );
}

#[test]
fn vfo_offset_changed_message_constructs() {
    // Same shape regression as `bandwidth_changed_message_constructs`
    // — future refactors that change the f64 carrier type
    // fail here first.
    let offset = DspToUi::VfoOffsetChanged(TEST_VFO_OFFSET_HZ);
    assert!(matches!(
        offset,
        DspToUi::VfoOffsetChanged(v) if (v - TEST_VFO_OFFSET_HZ).abs() < f64::EPSILON
    ));
}

#[test]
fn ctcss_sustained_changed_message_constructs() {
    let open = DspToUi::CtcssSustainedChanged(true);
    assert!(matches!(open, DspToUi::CtcssSustainedChanged(true)));
    let closed = DspToUi::CtcssSustainedChanged(false);
    assert!(matches!(closed, DspToUi::CtcssSustainedChanged(false)));
}

#[test]
fn voice_squelch_open_changed_message_constructs() {
    let open = DspToUi::VoiceSquelchOpenChanged(true);
    assert!(matches!(open, DspToUi::VoiceSquelchOpenChanged(true)));
    let closed = DspToUi::VoiceSquelchOpenChanged(false);
    assert!(matches!(closed, DspToUi::VoiceSquelchOpenChanged(false)));
}

#[test]
#[allow(clippy::panic)]
fn apt_line_message_round_trips_boxed_payload() {
    // Pins the wire shape: `AptLine` carries a `Box<AptLine>`,
    // not a bare `AptLine`. A future refactor that swaps to an
    // unboxed payload (or adds a tuple field, or splits the
    // line into `(pixels, sync_quality)`, etc.) trips this test
    // before it can silently bloat the enum's stack size.
    let payload = AptLine {
        sync_quality: 0.75,
        input_sample_index: 12_345,
        ..AptLine::default()
    };
    let msg = DspToUi::AptLine(Box::new(payload));
    match msg {
        DspToUi::AptLine(boxed) => {
            assert!((boxed.sync_quality - 0.75).abs() < f32::EPSILON);
            assert_eq!(boxed.input_sample_index, 12_345);
        }
        other => panic!("expected AptLine, got {other:?}"),
    }
}

/// Part 1 of 2 of the variant-construction sweep (`RtlTcpConnectionState` … `RtlTcpConnectionState`).
#[test]
fn rtl_tcp_connection_state_message_constructs_1() {
    // Constructing each variant through the DspToUi wrapper
    // exercises the `#[derive(Debug)]` + message plumbing end
    // to end. Catches the class of bugs where a future refactor
    // changes the `RtlTcpConnectionState` shape without updating
    // the message-side re-export — the build would still pass
    // but the variant wouldn't wrap.
    let disc = DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Disconnected);
    assert!(matches!(
        disc,
        DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Disconnected)
    ));

    let connecting = DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Connecting);
    assert!(matches!(
        connecting,
        DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Connecting)
    ));

    let connected = DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Connected {
        tuner_name: "R820T".into(),
        gain_count: 29,
        codec: "None".into(),
        granted_role: Some(true),
    });
    assert!(matches!(
        connected,
        DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Connected {
            gain_count: 29,
            ref codec,
            ..
        }) if codec == "None"
    ));

    let retrying = DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Retrying {
        attempt: 3,
        retry_in: std::time::Duration::from_secs(5),
    });
    assert!(matches!(
        retrying,
        DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Retrying { attempt: 3, .. })
    ));
}

/// Part 2 of 2 of the variant-construction sweep (`RtlTcpConnectionState` … `NetworkSinkStatus`).
#[test]
fn rtl_tcp_connection_state_message_constructs_2() {
    let failed = DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Failed {
        reason: "bad handshake".into(),
    });
    assert!(matches!(
        failed,
        DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Failed { .. })
    ));

    // Network audio sink status (issue #247) — three
    // variants exercising each shape so a future payload
    // tweak (e.g. adding a bytes_sent counter) trips this
    // regression net rather than silently going quiet at
    // the GTK status-row renderer. Per CodeRabbit round 1
    // on PR #351.
    let net_active = DspToUi::NetworkSinkStatus(NetworkSinkStatus::Active {
        endpoint: "0.0.0.0:1234".to_string(),
        protocol: sdr_types::Protocol::TcpClient,
    });
    assert!(matches!(
        &net_active,
        DspToUi::NetworkSinkStatus(NetworkSinkStatus::Active {
            endpoint,
            protocol: sdr_types::Protocol::TcpClient,
        }) if endpoint == "0.0.0.0:1234"
    ));

    let net_inactive = DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive);
    assert!(matches!(
        net_inactive,
        DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive)
    ));

    let net_err = DspToUi::NetworkSinkStatus(NetworkSinkStatus::Error {
        message: "bind: Address already in use".to_string(),
    });
    assert!(matches!(
        &net_err,
        DspToUi::NetworkSinkStatus(NetworkSinkStatus::Error { message })
            if message == "bind: Address already in use"
    ));
}
