use super::*;

/// Fixed bandwidth used by the message-variant round-trip
/// tests. 12.5 kHz is NFM's default and the value the VFO-drag
/// feedback loop most commonly emits in practice — hoisting it
/// to a const both removes the magic-number duplication in the
/// construct + match and documents the choice of value.
const TEST_BANDWIDTH_HZ: f64 = 12_500.0;

/// Fixed VFO offset used by the `VfoOffsetChanged` round-trip
/// test. 25 kHz is a representative non-zero offset that
/// click-to-tune / drag flows routinely emit — same hoisting
/// rationale as `TEST_BANDWIDTH_HZ`: avoids a magic-number
/// duplicated between construct and match.
const TEST_VFO_OFFSET_HZ: f64 = 25_000.0;

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

#[test]
fn rtl_tcp_connection_state_message_constructs() {
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

#[test]
#[allow(clippy::too_many_lines)]
fn test_ui_to_dsp_variants() {
    let start = UiToDsp::Start;
    assert!(matches!(start, UiToDsp::Start));

    let stop = UiToDsp::Stop;
    assert!(matches!(stop, UiToDsp::Stop));

    let tune = UiToDsp::Tune(144_000_000.0);
    assert!(matches!(tune, UiToDsp::Tune(f) if (f - 144_000_000.0).abs() < f64::EPSILON));

    let mode = UiToDsp::SetDemodMode(DemodMode::Am);
    assert!(matches!(mode, UiToDsp::SetDemodMode(DemodMode::Am)));

    let bw = UiToDsp::SetBandwidth(12_500.0);
    assert!(matches!(bw, UiToDsp::SetBandwidth(b) if (b - 12_500.0).abs() < f64::EPSILON));

    let sq = UiToDsp::SetSquelch(-50.0);
    assert!(matches!(sq, UiToDsp::SetSquelch(s) if (s - (-50.0)).abs() < f32::EPSILON));

    let sqe = UiToDsp::SetSquelchEnabled(true);
    assert!(matches!(sqe, UiToDsp::SetSquelchEnabled(true)));

    let auto_sq = UiToDsp::SetAutoSquelch(true);
    assert!(matches!(auto_sq, UiToDsp::SetAutoSquelch(true)));

    let vol = UiToDsp::SetVolume(0.75);
    assert!(matches!(vol, UiToDsp::SetVolume(v) if (v - 0.75).abs() < f32::EPSILON));

    let deemp = UiToDsp::SetDeemphasis(DeemphasisMode::Eu50);
    assert!(matches!(
        deemp,
        UiToDsp::SetDeemphasis(DeemphasisMode::Eu50)
    ));

    let sr = UiToDsp::SetSampleRate(2_400_000.0);
    assert!(matches!(sr, UiToDsp::SetSampleRate(r) if (r - 2_400_000.0).abs() < f64::EPSILON));

    let dec = UiToDsp::SetDecimation(4);
    assert!(matches!(dec, UiToDsp::SetDecimation(4)));

    let dc = UiToDsp::SetDcBlocking(true);
    assert!(matches!(dc, UiToDsp::SetDcBlocking(true)));

    let iq = UiToDsp::SetIqInversion(false);
    assert!(matches!(iq, UiToDsp::SetIqInversion(false)));

    let fft = UiToDsp::SetFftSize(2048);
    assert!(matches!(fft, UiToDsp::SetFftSize(2048)));

    let nb = UiToDsp::SetNbEnabled(true);
    assert!(matches!(nb, UiToDsp::SetNbEnabled(true)));

    let nr = UiToDsp::SetFmIfNrEnabled(false);
    assert!(matches!(nr, UiToDsp::SetFmIfNrEnabled(false)));

    let gain = UiToDsp::SetGain(33.8);
    assert!(matches!(gain, UiToDsp::SetGain(g) if (g - 33.8).abs() < f64::EPSILON));

    let agc = UiToDsp::SetAgc(true);
    assert!(matches!(agc, UiToDsp::SetAgc(true)));

    // Software AGC runs alongside hardware AGC — the UI
    // selector (#356 / #357) mutually excludes them, but
    // the engine-side messages are independent.
    let sw_agc_on = UiToDsp::SetSoftwareAgc(true);
    assert!(matches!(sw_agc_on, UiToDsp::SetSoftwareAgc(true)));
    let sw_agc_off = UiToDsp::SetSoftwareAgc(false);
    assert!(matches!(sw_agc_off, UiToDsp::SetSoftwareAgc(false)));

    let iq_corr = UiToDsp::SetIqCorrection(false);
    assert!(matches!(iq_corr, UiToDsp::SetIqCorrection(false)));

    let wf = UiToDsp::SetWindowFunction(sdr_pipeline::iq_frontend::FftWindow::Blackman);
    assert!(matches!(
        wf,
        UiToDsp::SetWindowFunction(sdr_pipeline::iq_frontend::FftWindow::Blackman)
    ));

    let vfo = UiToDsp::SetVfoOffset(25_000.0);
    assert!(matches!(vfo, UiToDsp::SetVfoOffset(o) if (o - 25_000.0).abs() < f64::EPSILON));

    let nb = UiToDsp::SetNbLevel(5.0);
    assert!(matches!(nb, UiToDsp::SetNbLevel(l) if (l - 5.0).abs() < f32::EPSILON));

    let stereo = UiToDsp::SetWfmStereo(true);
    assert!(matches!(stereo, UiToDsp::SetWfmStereo(true)));

    let fft_rate = UiToDsp::SetFftRate(30.0);
    assert!(matches!(fft_rate, UiToDsp::SetFftRate(r) if (r - 30.0).abs() < f64::EPSILON));

    // FFT compute gate (#646 / #647) — both polarities so a
    // future refactor that flips the payload type or renames
    // the variant trips this regression net.
    let fft_on = UiToDsp::SetFftEnabled(true);
    assert!(matches!(fft_on, UiToDsp::SetFftEnabled(true)));
    let fft_off = UiToDsp::SetFftEnabled(false);
    assert!(matches!(fft_off, UiToDsp::SetFftEnabled(false)));

    let hp = UiToDsp::SetHighPass(true);
    assert!(matches!(hp, UiToDsp::SetHighPass(true)));

    let notch_en = UiToDsp::SetNotchEnabled(true);
    assert!(matches!(notch_en, UiToDsp::SetNotchEnabled(true)));

    let notch_freq = UiToDsp::SetNotchFrequency(60.0);
    assert!(matches!(notch_freq, UiToDsp::SetNotchFrequency(f) if (f - 60.0).abs() < f32::EPSILON));

    let ctcss_off = UiToDsp::SetCtcssMode(CtcssMode::Off);
    assert!(matches!(ctcss_off, UiToDsp::SetCtcssMode(CtcssMode::Off)));

    let ctcss_tone = UiToDsp::SetCtcssMode(CtcssMode::Tone(100.0));
    assert!(matches!(
        ctcss_tone,
        UiToDsp::SetCtcssMode(CtcssMode::Tone(hz)) if (hz - 100.0).abs() < f32::EPSILON
    ));

    let ctcss_thresh = UiToDsp::SetCtcssThreshold(0.15);
    assert!(
        matches!(ctcss_thresh, UiToDsp::SetCtcssThreshold(t) if (t - 0.15).abs() < f32::EPSILON)
    );

    let vs_off = UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Off);
    assert!(matches!(
        vs_off,
        UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Off)
    ));

    let vs_syl = UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Syllabic { threshold: 0.15 });
    assert!(matches!(
        vs_syl,
        UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Syllabic { threshold })
            if (threshold - 0.15).abs() < f32::EPSILON
    ));

    let vs_snr = UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Snr { threshold_db: 6.0 });
    assert!(matches!(
        vs_snr,
        UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Snr { threshold_db })
            if (threshold_db - 6.0).abs() < f32::EPSILON
    ));

    let vs_thresh = UiToDsp::SetVoiceSquelchThreshold(0.2);
    assert!(
        matches!(vs_thresh, UiToDsp::SetVoiceSquelchThreshold(t) if (t - 0.2).abs() < f32::EPSILON)
    );

    let device = UiToDsp::SetAudioDevice("default".to_string());
    assert!(matches!(device, UiToDsp::SetAudioDevice(ref s) if s == "default"));

    let src_type = UiToDsp::SetSourceType(SourceType::RtlSdr);
    assert!(matches!(
        src_type,
        UiToDsp::SetSourceType(SourceType::RtlSdr)
    ));

    let src_net = UiToDsp::SetSourceType(SourceType::Network);
    assert!(matches!(
        src_net,
        UiToDsp::SetSourceType(SourceType::Network)
    ));

    let src_file = UiToDsp::SetSourceType(SourceType::File);
    assert!(matches!(src_file, UiToDsp::SetSourceType(SourceType::File)));

    let net_cfg = UiToDsp::SetNetworkConfig {
        hostname: "192.168.1.1".to_string(),
        port: 4321,
        protocol: sdr_types::Protocol::TcpClient,
    };
    assert!(matches!(
        net_cfg,
        UiToDsp::SetNetworkConfig { ref hostname, port: 4321, .. } if hostname == "192.168.1.1"
    ));

    let file_path = UiToDsp::SetFilePath(std::path::PathBuf::from("/tmp/test.wav"));
    assert!(matches!(
        file_path,
        UiToDsp::SetFilePath(ref p) if p == std::path::Path::new("/tmp/test.wav")
    ));

    // Loop-on-EOF toggle — both polarities, since the
    // controller's handler branches on the value and we
    // want a shape regression on either to fail loudly.
    // Per `CodeRabbit` round 1 on PR #371.
    let loop_on = UiToDsp::SetFileLooping(true);
    assert!(matches!(loop_on, UiToDsp::SetFileLooping(true)));
    let loop_off = UiToDsp::SetFileLooping(false);
    assert!(matches!(loop_off, UiToDsp::SetFileLooping(false)));

    let ppm = UiToDsp::SetPpmCorrection(42);
    assert!(matches!(ppm, UiToDsp::SetPpmCorrection(42)));

    let audio_rec = UiToDsp::StartAudioRecording(std::path::PathBuf::from("/tmp/audio.wav"));
    assert!(matches!(audio_rec, UiToDsp::StartAudioRecording(_)));

    let audio_stop = UiToDsp::StopAudioRecording;
    assert!(matches!(audio_stop, UiToDsp::StopAudioRecording));

    let iq_rec = UiToDsp::StartIqRecording(std::path::PathBuf::from("/tmp/iq.wav"));
    assert!(matches!(iq_rec, UiToDsp::StartIqRecording(_)));

    let iq_stop = UiToDsp::StopIqRecording;
    assert!(matches!(iq_stop, UiToDsp::StopIqRecording));

    // LRPT image-handle attach / detach (epic #469 task 7)
    // — pin the variant shape so a future change to the
    // `LrptImage` payload type or a rename of `ClearLrptImage`
    // fails this regression net rather than silently
    // breaking the controller's `lrpt_decode_tap` plumbing.
    // Per CodeRabbit round 1 on PR #543.
    let lrpt_image = sdr_radio::lrpt_image::LrptImage::new();
    let set_lrpt = UiToDsp::SetLrptImage(lrpt_image);
    assert!(matches!(set_lrpt, UiToDsp::SetLrptImage(_)));

    let clear_lrpt = UiToDsp::ClearLrptImage;
    assert!(matches!(clear_lrpt, UiToDsp::ClearLrptImage));

    let (tx, _rx) = std::sync::mpsc::sync_channel::<sdr_transcription::TranscriptionInput>(1);
    let enable = UiToDsp::EnableTranscription(tx);
    assert!(matches!(enable, UiToDsp::EnableTranscription(_)));

    let disable = UiToDsp::DisableTranscription;
    assert!(matches!(disable, UiToDsp::DisableTranscription));

    // Audio tap (issue #314) — constructed here so a future
    // signature tweak to the Vec<f32> payload or the
    // SyncSender<...> type fails this regression net rather
    // than silently going quiet at the FFI handler site. Per
    // CodeRabbit round 1 on PR #349.
    let (tap_tx, _tap_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(1);
    let enable_tap = UiToDsp::EnableAudioTap(tap_tx);
    assert!(matches!(enable_tap, UiToDsp::EnableAudioTap(_)));

    let disable_tap = UiToDsp::DisableAudioTap;
    assert!(matches!(disable_tap, UiToDsp::DisableAudioTap));

    // Network audio sink (issue #247) — constructed here so a
    // future signature tweak to AudioSinkType, the
    // SetNetworkSinkConfig field set, or the Protocol type
    // fails this regression net rather than silently going
    // quiet at the controller's handler. Per CodeRabbit
    // round 1 on PR #351.
    let set_sink_local = UiToDsp::SetAudioSinkType(crate::sink_slot::AudioSinkType::Local);
    assert!(matches!(
        set_sink_local,
        UiToDsp::SetAudioSinkType(crate::sink_slot::AudioSinkType::Local)
    ));
    let set_sink_network = UiToDsp::SetAudioSinkType(crate::sink_slot::AudioSinkType::Network);
    assert!(matches!(
        set_sink_network,
        UiToDsp::SetAudioSinkType(crate::sink_slot::AudioSinkType::Network)
    ));

    let net_cfg = UiToDsp::SetNetworkSinkConfig {
        hostname: "192.0.2.1".to_string(),
        port: 4242,
        protocol: sdr_types::Protocol::Udp,
    };
    assert!(matches!(
        &net_cfg,
        UiToDsp::SetNetworkSinkConfig {
            hostname,
            port: 4242,
            protocol: sdr_types::Protocol::Udp,
        } if hostname == "192.0.2.1"
    ));

    // RTL-TCP connection controls (commit 3 of PR #335) —
    // constructed directly so a future signature change (e.g.
    // adding an instance-selector param) fails this test
    // rather than silently going quiet at the UI-handler site.
    let disc = UiToDsp::DisconnectRtlTcp;
    assert!(matches!(disc, UiToDsp::DisconnectRtlTcp));
    let retry = UiToDsp::RetryRtlTcpNow;
    assert!(matches!(retry, UiToDsp::RetryRtlTcpNow));

    // RTL-TCP role + auth-key config (issue #396). Constructed
    // with a non-default Listen role and a plausible 32-byte
    // key so the shape regression fires on either a field
    // rename / retyping OR the re-export path going stale. The
    // matching `SetRtlTcpClientConfig` handler is load-bearing
    // for the role picker and per-server keyring flows.
    let cfg = UiToDsp::SetRtlTcpClientConfig {
        requested_role: sdr_server_rtltcp::extension::Role::Listen,
        auth_key: Some(vec![0xAB; 32]),
    };
    assert!(matches!(
        cfg,
        UiToDsp::SetRtlTcpClientConfig {
            requested_role: sdr_server_rtltcp::extension::Role::Listen,
            auth_key: Some(ref bytes),
        } if bytes.len() == 32 && bytes.iter().all(|&b| b == 0xAB)
    ));

    // `RetryRtlTcpWithTakeover` is a unit variant today, but
    // the pattern match fails loudly if that changes (e.g.
    // a future refactor adds a scoped-reason payload).
    let takeover = UiToDsp::RetryRtlTcpWithTakeover;
    assert!(matches!(takeover, UiToDsp::RetryRtlTcpWithTakeover));
}

#[test]
fn test_source_type_variants() {
    // Equality + discrimination across all four variants. RtlTcp
    // is the rtl_tcp-protocol network client added alongside the
    // existing raw Network variant; keep them distinct at the
    // type level.
    assert_eq!(SourceType::RtlSdr, SourceType::RtlSdr);
    assert_ne!(SourceType::RtlSdr, SourceType::Network);
    assert_ne!(SourceType::Network, SourceType::File);
    assert_ne!(SourceType::Network, SourceType::RtlTcp);
    assert_ne!(SourceType::RtlTcp, SourceType::RtlSdr);

    let types = [
        SourceType::RtlSdr,
        SourceType::Network,
        SourceType::File,
        SourceType::RtlTcp,
    ];
    assert_eq!(types.len(), 4);
}

#[test]
fn test_set_source_type_rtl_tcp_message() {
    // Regression coverage for the new variant — make sure the
    // message wraps it and pattern-matches cleanly, same shape as
    // the existing RtlSdr / Network / File branches elsewhere in
    // this test suite.
    let msg = UiToDsp::SetSourceType(SourceType::RtlTcp);
    assert!(matches!(msg, UiToDsp::SetSourceType(SourceType::RtlTcp)));
}

#[test]
fn test_scanner_dsp_to_ui_variants() {
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

/// Shape regression for `UiToDsp::ResetImagingDecoders`. The
/// auto-record state machine's `Recording → Finalizing`
/// transition emits this — a rename / removal would fail the
/// recorder's wiring at runtime, so pin the variant here. Per
/// issue #544.
#[test]
fn test_reset_imaging_decoders_variant() {
    let msg = UiToDsp::ResetImagingDecoders;
    assert!(matches!(msg, UiToDsp::ResetImagingDecoders));
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
