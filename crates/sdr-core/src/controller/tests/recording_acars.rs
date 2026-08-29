use super::*;

/// Records the order of `Source` setter calls.
#[derive(Default)]
struct RecordingSource {
    calls: Vec<String>,
}

impl Source for RecordingSource {
    fn name(&self) -> &'static str {
        "recording"
    }
    fn start(&mut self) -> Result<(), sdr_types::SourceError> {
        self.calls.push("start".into());
        Ok(())
    }
    fn stop(&mut self) -> Result<(), sdr_types::SourceError> {
        Ok(())
    }
    fn tune(&mut self, _frequency_hz: f64) -> Result<(), sdr_types::SourceError> {
        self.calls.push("tune".into());
        Ok(())
    }
    fn sample_rates(&self) -> &[f64] {
        &[2_400_000.0]
    }
    fn sample_rate(&self) -> f64 {
        2_400_000.0
    }
    fn set_sample_rate(&mut self, _rate: f64) -> Result<(), sdr_types::SourceError> {
        Ok(())
    }
    fn read_samples(&mut self, _output: &mut [Complex]) -> Result<usize, sdr_types::SourceError> {
        Ok(0)
    }
    fn set_direct_sampling(&mut self, mode: i32) -> Result<(), sdr_types::SourceError> {
        self.calls.push(format!("set_direct_sampling({mode})"));
        Ok(())
    }
    fn set_rtl_agc(&mut self, enabled: bool) -> Result<(), sdr_types::SourceError> {
        self.calls.push(format!("set_rtl_agc({enabled})"));
        Ok(())
    }
    fn set_gain_mode(&mut self, manual: bool) -> Result<(), sdr_types::SourceError> {
        self.calls.push(format!("set_gain_mode({manual})"));
        Ok(())
    }
    fn set_gain(&mut self, gain_tenths: i32) -> Result<(), sdr_types::SourceError> {
        self.calls.push(format!("set_gain({gain_tenths})"));
        Ok(())
    }
}

/// #703 — gain mode, gain and RTL AGC must reach the source BEFORE
/// `start()`, so the driver's open-time programming uses the user's
/// persisted values instead of its first-time defaults (29.7 dB manual),
/// which otherwise produces a saturated burst on every Play.
#[test]
fn rtl_sdr_pre_start_settings_dispatch_gain_before_start() {
    const PERSISTED_GAIN_TENTHS_DB: i32 = 0;
    const PERSISTED_DIRECT_SAMPLING: i32 = 2;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx).unwrap();
    state.tuner_agc_auto = false;
    state.tuner_gain_tenths_db = PERSISTED_GAIN_TENTHS_DB;
    state.rtl_agc_enabled = true;
    state.direct_sampling_mode = PERSISTED_DIRECT_SAMPLING;

    let mut source = RecordingSource::default();
    rtl_sdr_pre_start_settings(&state, &mut source);
    source.start().unwrap();

    let start_at = source.calls.iter().position(|c| c == "start").unwrap();
    let before: Vec<&str> = source.calls[..start_at]
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(
        before,
        [
            "set_direct_sampling(2)",
            "set_rtl_agc(true)",
            "set_gain_mode(true)",
            "set_gain(0)",
        ],
        "direct sampling, RTL AGC, gain mode and gain must precede start()"
    );
}

fn temp_wav(name: &str) -> std::path::PathBuf {
    // Randomized, 0700 per-process `tempfile` root; unique file names
    // beneath it instead of fixed names in the system temp dir.
    static TEST_TMP_ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    TEST_TMP_ROOT
        .get_or_init(|| {
            tempfile::Builder::new()
                .prefix("sdr-rs-ctl-tests-")
                .tempdir()
                .expect("test temp root")
        })
        .path()
        .join(format!("{name}.wav"))
}

/// #695 — the IQ WAV header bakes in the sample rate at start, so a
/// rate change mid-recording silently corrupts the file. Reject it.
#[test]
fn set_sample_rate_is_rejected_while_iq_recording() {
    const NEW_RATE_HZ: f64 = 1_024_000.0;
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let path = temp_wav("rate-mutex");
    state.iq_writer = Some(WavWriter::new(&path, 2_400_000, IQ_CHANNELS).unwrap());
    let before = state.configured_sample_rate;
    let _ = drain(&dsp_rx);

    handle_command(&mut state, &dsp_tx, UiToDsp::SetSampleRate(NEW_RATE_HZ));

    assert!((state.configured_sample_rate - before).abs() < f64::EPSILON);
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::Error(m) if m.to_lowercase().contains("recording"))),
        "expected a recording-mutex error, got {events:?}"
    );
    state.iq_writer = None;
    let _ = std::fs::remove_file(&path);
}

/// #695 — with no source there is no IQ flowing and `state.sample_rate`
/// may be stale; a recording started now would get a wrong header.
#[test]
fn start_iq_recording_requires_a_running_source() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    assert!(state.source.is_none());
    let path = temp_wav("needs-source");
    let _ = drain(&dsp_rx);

    handle_command(&mut state, &dsp_tx, UiToDsp::StartIqRecording(path.clone()));

    assert!(state.iq_writer.is_none(), "no writer without a source");
    let events = drain(&dsp_rx);
    assert!(
        events.iter().any(|e| matches!(e, DspToUi::Error(_))),
        "expected an error, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, DspToUi::IqRecordingStarted(_))),
        "must not report a started recording"
    );
    let _ = std::fs::remove_file(&path);
}

/// #695 — ACARS engage forces the airband rate; refuse while recording.
#[test]
fn acars_engage_is_rejected_while_iq_recording() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let path = temp_wav("acars-mutex");
    state.iq_writer = Some(WavWriter::new(&path, 2_400_000, IQ_CHANNELS).unwrap());
    let _ = drain(&dsp_rx);

    let _ = handle_set_acars_enabled(&mut state, true, &dsp_tx);

    assert!(state.acars_pre_lock.is_none(), "must not engage");
    let events = drain(&dsp_rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            DspToUi::AcarsEnabledChanged(Err(
                crate::acars_airband_lock::AcarsEnableError::IqRecordingActive
            ))
        )),
        "expected AcarsEnabledChanged(Err(IqRecordingActive)), got {events:?}"
    );
    state.iq_writer = None;
    let _ = std::fs::remove_file(&path);
}

/// #695 (CR round 3) — a user-initiated ACARS disengage restores the
/// pre-lock source rate, which would desync an open IQ recording.
#[test]
fn acars_disengage_is_rejected_while_iq_recording() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let path = temp_wav("acars-disengage-mutex");
    state.iq_writer = Some(WavWriter::new(&path, 2_400_000, IQ_CHANNELS).unwrap());
    state.acars_pre_lock = Some(test_pre_lock_snapshot());
    let _ = drain(&dsp_rx);

    let _ = handle_set_acars_enabled(&mut state, false, &dsp_tx);

    assert!(state.acars_pre_lock.is_some(), "must stay engaged");
    let events = drain(&dsp_rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            DspToUi::AcarsEnabledChanged(Err(
                crate::acars_airband_lock::AcarsEnableError::IqRecordingActive
            ))
        )),
        "expected AcarsEnabledChanged(Err(IqRecordingActive)), got {events:?}"
    );
    state.iq_writer = None;
    let _ = std::fs::remove_file(&path);
}

/// #695 (CR round 3) — `cleanup()` is the forced teardown path: it
/// finalizes the recording first, so the ACARS disengage inside it
/// must not be blocked by the recording mutex.
#[test]
fn cleanup_disengages_acars_even_while_iq_recording() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let path = temp_wav("acars-cleanup");
    state.iq_writer = Some(WavWriter::new(&path, 2_400_000, IQ_CHANNELS).unwrap());
    state.acars_pre_lock = Some(test_pre_lock_snapshot());
    let _ = drain(&dsp_rx);

    cleanup(&mut state, &dsp_tx);

    assert!(state.iq_writer.is_none(), "cleanup finalizes the recording");
    assert!(state.acars_pre_lock.is_none(), "cleanup disengages ACARS");
    let events = drain(&dsp_rx);
    assert!(
        !events.iter().any(|e| matches!(
            e,
            DspToUi::AcarsEnabledChanged(Err(
                crate::acars_airband_lock::AcarsEnableError::IqRecordingActive
            ))
        )),
        "cleanup must not trip the recording mutex, got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::AcarsEnabledChanged(Ok(false)))),
        "cleanup must ack the disengage, got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::IqRecordingStopped)),
        "cleanup must tell the UI the recording stopped, got {events:?}"
    );
    let _ = std::fs::remove_file(&path);
}

/// #695 (CR round 5) — switching source type while IQ-recording with
/// ACARS engaged must not trip the recording mutex: `cleanup()` stops
/// the recording first and then performs the forced ACARS teardown.
#[test]
fn source_type_switch_while_iq_recording_defers_acars_teardown_to_cleanup() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let path = temp_wav("acars-source-switch");
    state.iq_writer = Some(WavWriter::new(&path, 2_400_000, IQ_CHANNELS).unwrap());
    state.acars_pre_lock = Some(test_pre_lock_snapshot());
    state.running = true;
    let _ = drain(&dsp_rx);

    // File source with an empty path: the restart after cleanup fails
    // fast and without hardware.
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetSourceType(SourceType::File),
    );

    let events = drain(&dsp_rx);
    assert!(
        !events.iter().any(|e| matches!(
            e,
            DspToUi::AcarsEnabledChanged(Err(
                crate::acars_airband_lock::AcarsEnableError::IqRecordingActive
            ))
        )),
        "source switch must not report a recording-mutex failure, got {events:?}"
    );
    assert!(state.acars_pre_lock.is_none(), "ACARS torn down by cleanup");
    assert!(state.iq_writer.is_none(), "recording stopped by cleanup");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::IqRecordingStopped)),
        "expected IqRecordingStopped, got {events:?}"
    );
    let _ = std::fs::remove_file(&path);
}

/// #694 (CR round 5) — only the WAV structural limit gets the
/// "4 GiB limit" wording; a full filesystem is a plain write failure.
#[test]
fn recording_write_error_message_distinguishes_wav_limit_from_disk_full() {
    let wav_limit = crate::wav_writer::wav_limit_error();
    assert!(recording_write_error_message("IQ", &wav_limit).contains("4 GiB"));
    let disk_full = std::io::Error::from(std::io::ErrorKind::StorageFull);
    assert!(!recording_write_error_message("IQ", &disk_full).contains("4 GiB"));
    assert!(recording_write_error_message("IQ", &disk_full).contains("write failed"));
}

#[test]
fn acars_tap_lazy_inits_bank_on_first_call_and_stays_silent_for_zero_iq() {
    let mut bank: Option<sdr_acars::ChannelBank> = None;
    let mut init_failed = false;
    let (tx, rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 1024];
    let (acars_dsp_tx, _acars_dsp_rx) = mpsc::channel::<DspToUi>();
    let outputs = super::AcarsOutputs::new(acars_dsp_tx).unwrap();

    super::acars_decode_tap(
        &mut bank,
        &mut init_failed,
        ACARS_SOURCE_RATE_HZ,
        ACARS_CENTER_HZ,
        &US_SIX_CHANNELS_HZ,
        &iq,
        &tx,
        &outputs,
    );
    assert!(bank.is_some(), "first call should initialize the bank");
    assert!(!init_failed);
    // Silent IQ produces no messages.
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
}

#[test]
fn acars_tap_skips_processing_after_init_failure() {
    let mut bank: Option<sdr_acars::ChannelBank> = None;
    let mut init_failed = true; // Simulate prior failure.
    let (tx, _rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 1024];
    let (acars_dsp_tx, _acars_dsp_rx) = mpsc::channel::<DspToUi>();
    let outputs = super::AcarsOutputs::new(acars_dsp_tx).unwrap();

    super::acars_decode_tap(
        &mut bank,
        &mut init_failed,
        ACARS_SOURCE_RATE_HZ,
        ACARS_CENTER_HZ,
        &US_SIX_CHANNELS_HZ,
        &iq,
        &tx,
        &outputs,
    );
    assert!(bank.is_none(), "init_failed=true must short-circuit");
    assert!(init_failed);
}

#[test]
fn acars_tap_records_init_failure_on_invalid_channel_list() {
    let mut bank: Option<sdr_acars::ChannelBank> = None;
    let mut init_failed = false;
    let (tx, _rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 1024];
    let bad_channels: [f64; 6] = [0.0; 6]; // outside source bandwidth
    let (acars_dsp_tx, _acars_dsp_rx) = mpsc::channel::<DspToUi>();
    let outputs = super::AcarsOutputs::new(acars_dsp_tx).unwrap();

    super::acars_decode_tap(
        &mut bank,
        &mut init_failed,
        ACARS_SOURCE_RATE_HZ,
        ACARS_CENTER_HZ,
        &bad_channels,
        &iq,
        &tx,
        &outputs,
    );
    assert!(bank.is_none());
    assert!(init_failed, "bad channels should set init_failed");
}

/// #849 (review round 1 on PR #860) — the switch-while-engaged
/// auto-disable is IDENTITY-based: any source-type change while
/// engaged tears the lock down, because the pre-lock snapshot
/// belongs to the old hardware and restoring its rate onto a
/// different source would clamp through the new rate table instead
/// of faithfully restoring the user's state. A same-type dispatch
/// keeps the lock.
#[test]
fn source_switch_while_engaged_tears_down_on_type_change() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.acars_pre_lock = Some(test_pre_lock_snapshot());
    let _ = drain(&dsp_rx);

    // Same-type dispatch (snapshot is RtlSdr): lock survives.
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetSourceType(SourceType::RtlSdr),
    );
    assert!(
        state.acars_pre_lock.is_some(),
        "same-type dispatch must not tear ACARS down"
    );

    // RTL → Airspy: capable hardware, but DIFFERENT hardware — the
    // snapshot can't restore faithfully, so the lock tears down.
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetSourceType(SourceType::Airspy),
    );
    assert!(
        state.acars_pre_lock.is_none(),
        "cross-type switch must tear ACARS down"
    );
}

/// Airspy Mini firmware rate table (IQ, Hz) — the Mini has no
/// 2.5 Msps entry, so the lock's requested rate clamps to 3 Msps.
const AIRSPY_MINI_RATES_HZ: [f64; 2] = [3_000_000.0, 6_000_000.0];

/// #849 (CR round 1 on PR #860) — geometry-only half of the Mini
/// coverage: every Airspy Mini rate is an integer multiple of the
/// 12.5 kHz channel IF rate, so a bank built at a clamped rate is
/// valid. The engagement-level half (the clamp actually flowing
/// through the read-back into the bank) is
/// [`acars_engage_builds_bank_at_the_clamped_rate`].
#[test]
fn channel_bank_builds_at_airspy_mini_clamped_rates() {
    for rate in AIRSPY_MINI_RATES_HZ {
        let bank = sdr_acars::ChannelBank::new(
            rate,
            crate::acars_airband_lock::AcarsRegion::default().center_hz(),
            crate::acars_airband_lock::AcarsRegion::default().channels(),
        );
        assert!(
            bank.is_ok(),
            "bank must build at {rate} Hz: {:?}",
            bank.err()
        );
    }
}

/// Fake source mimicking an Airspy Mini's rate behavior: any
/// requested rate clamps to the nearest entry of
/// [`AIRSPY_MINI_RATES_HZ`], like `AirspySource::set_sample_rate`
/// does against its firmware snapshot.
struct MiniLikeSource {
    rate: f64,
}

impl Source for MiniLikeSource {
    fn name(&self) -> &'static str {
        "mini-like"
    }
    fn sample_rates(&self) -> &[f64] {
        &AIRSPY_MINI_RATES_HZ
    }
    fn start(&mut self) -> Result<(), sdr_types::SourceError> {
        Ok(())
    }
    fn stop(&mut self) -> Result<(), sdr_types::SourceError> {
        Ok(())
    }
    fn read_samples(&mut self, _buf: &mut [Complex]) -> Result<usize, sdr_types::SourceError> {
        Ok(0)
    }
    fn sample_rate(&self) -> f64 {
        self.rate
    }
    fn set_sample_rate(&mut self, rate: f64) -> Result<(), sdr_types::SourceError> {
        self.rate = AIRSPY_MINI_RATES_HZ
            .iter()
            .copied()
            .min_by(|a, b| (a - rate).abs().total_cmp(&(b - rate).abs()))
            .expect("non-empty table");
        Ok(())
    }
    fn tune(&mut self, _frequency_hz: f64) -> Result<(), sdr_types::SourceError> {
        Ok(())
    }
}

/// #849 (CR round 2 on PR #860) — engagement-level half of the Mini
/// coverage: engaging on a source that CLAMPS the 2.5 Msps request
/// must read back the actual hardware rate and build the bank at it,
/// not at the pre-clamp request.
#[test]
fn acars_engage_builds_bank_at_the_clamped_rate() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.source_type = SourceType::Airspy;
    state.source = Some(Box::new(MiniLikeSource { rate: 6_000_000.0 }));
    let _ = drain(&dsp_rx);

    let _ = handle_set_acars_enabled(&mut state, true, &dsp_tx);

    assert!(
        state.acars_pre_lock.is_some(),
        "engaged on the Mini-like source"
    );
    assert!(state.acars_bank.is_some(), "bank pre-built at engage");
    // The 2.5 Msps request clamped to 3 Msps — and the read-back
    // value is what the DSP graph AND the bank were built from. The
    // clamp copies a table entry verbatim, so exact comparison is
    // the correct check.
    assert!((state.sample_rate - 3_000_000.0).abs() < f64::EPSILON);
}
