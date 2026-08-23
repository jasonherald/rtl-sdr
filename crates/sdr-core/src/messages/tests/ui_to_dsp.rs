use super::*;

/// Part 1 of 7 of the variant-construction sweep (`Start` … `SetGain`).
#[test]
fn test_ui_to_dsp_variants_1() {
    let start = UiToDsp::Start;
    assert!(matches!(start, UiToDsp::Start));

    let stop = UiToDsp::Stop;
    assert!(matches!(stop, UiToDsp::Stop));

    let tune = UiToDsp::Tune(144_000_000.0);
    assert!(matches!(tune, UiToDsp::Tune(f) if (f - 144_000_000.0).abs() < f64::EPSILON));

    let mode = UiToDsp::SetDemodMode(DemodMode::Am);
    assert!(matches!(mode, UiToDsp::SetDemodMode(DemodMode::Am)));

    let bw = UiToDsp::SetBandwidth(TEST_BANDWIDTH_HZ);
    assert!(matches!(bw, UiToDsp::SetBandwidth(b) if (b - TEST_BANDWIDTH_HZ).abs() < f64::EPSILON));

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
}

/// Part 2 of 7 of the variant-construction sweep (`SetAgc` … `SetCtcssMode`).
#[test]
fn test_ui_to_dsp_variants_2() {
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

    let vfo = UiToDsp::SetVfoOffset(TEST_VFO_OFFSET_HZ);
    assert!(
        matches!(vfo, UiToDsp::SetVfoOffset(o) if (o - TEST_VFO_OFFSET_HZ).abs() < f64::EPSILON)
    );

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
}

/// Part 3 of 7 of the variant-construction sweep (`SetCtcssMode` … `SetSourceType`).
#[test]
fn test_ui_to_dsp_variants_3() {
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
}

/// Part 4 of 7 of the variant-construction sweep (`SetSourceType` … `StopIqRecording`).
#[test]
fn test_ui_to_dsp_variants_4() {
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
}

/// Part 5 of 7 of the variant-construction sweep (`lrpt_image` … `DisableAudioTap`).
#[test]
fn test_ui_to_dsp_variants_5() {
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
}

/// Part 6 of 7 of the variant-construction sweep (`SetAudioSinkType` … `RetryRtlTcpNow`).
#[test]
fn test_ui_to_dsp_variants_6() {
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
}

/// Part 7 of 7 of the variant-construction sweep (`SetRtlTcpClientConfig` … `RetryRtlTcpWithTakeover`).
#[test]
fn test_ui_to_dsp_variants_7() {
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
