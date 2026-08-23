use super::*;

/// Compile-time validation that DSP buffer constants are consistent.
const _: () = {
    assert!(DEFAULT_FFT_SIZE > 0);
    assert!(DEFAULT_SAMPLE_RATE > 0.0);
    assert!(DEFAULT_CENTER_FREQ > 0.0);
    assert!(RECV_TIMEOUT_MS > 0);
    assert!(VFO_OUTPUT_PADDING > 0);
};

/// Synthesize a `SstvEvent::VisDetected` for tests. The
/// inner field values don't matter for counter-update tests
/// — we only care that the event variant is `VisDetected`.
fn fake_vis_event() -> slowrx::SstvEvent {
    slowrx::SstvEvent::VisDetected {
        mode: slowrx::SstvMode::Robot36,
        sample_offset: 0,
        hedr_shift_hz: 0.0,
    }
}

/// Synthesize a `SstvEvent::LineDecoded` for tests. Empty
/// pixel buffer is fine — `record_event` only inspects the
/// variant tag, not contents.
fn fake_line_event() -> slowrx::SstvEvent {
    slowrx::SstvEvent::LineDecoded {
        mode: slowrx::SstvMode::Robot36,
        line_index: 0,
        pixels: Vec::new(),
    }
}

/// Synthesize a `SstvEvent::ImageComplete` for tests. Uses
/// `SstvImage::new` (the public constructor — `SstvImage` is
/// `#[non_exhaustive]` so direct struct-literal init is
/// rejected by the compiler) with a minimal 1×1 size. The
/// counter logic doesn't read the image dimensions; the fake
/// just has to be a valid variant. `partial: false` matches
/// the V1 slowrx contract (only the final clean image surface
/// emits this event).
fn fake_image_complete_event() -> slowrx::SstvEvent {
    slowrx::SstvEvent::ImageComplete {
        image: slowrx::SstvImage::new(slowrx::SstvMode::Robot36, 1, 1),
        partial: false,
    }
}

#[test]
fn sstv_pass_stats_default_is_empty() {
    let stats = SstvPassStats::default();
    assert_eq!(stats.vis_count, 0);
    assert_eq!(stats.image_complete_count, 0);
    assert_eq!(stats.lines_decoded, 0);
    assert!(
        !stats.saw_any_event(),
        "default stats must report no events — drives the \
         skip-summary-log decision in reset_imaging_decoders"
    );
}

#[test]
fn sstv_pass_stats_increments_per_event_kind() {
    // Counter dispatch table: each event variant maps to
    // exactly one counter. Per #648.
    let mut stats = SstvPassStats::default();
    stats.record_event(&fake_vis_event());
    assert_eq!(stats.vis_count, 1);
    assert_eq!(stats.image_complete_count, 0);
    assert_eq!(stats.lines_decoded, 0);

    stats.record_event(&fake_line_event());
    stats.record_event(&fake_line_event());
    assert_eq!(stats.lines_decoded, 2);
    assert_eq!(stats.vis_count, 1, "line events must not bump vis_count");

    stats.record_event(&fake_image_complete_event());
    assert_eq!(stats.image_complete_count, 1);
    assert_eq!(stats.vis_count, 1, "image-complete must not bump vis_count");
    assert_eq!(
        stats.lines_decoded, 2,
        "image-complete must not bump lines_decoded"
    );

    assert!(
        stats.saw_any_event(),
        "stats with non-zero counters must report saw_any_event"
    );
}

#[test]
fn sstv_pass_stats_counts_a_realistic_ariss_pass() {
    // Realistic Series 32 pass: 3 VIS bursts (ARISS duty
    // cycle = 36 sec ON / 2 min OFF, typical 7-min pass
    // catches ~3 windows), 2 complete images, 1 partial
    // (~80 lines into the 240-line PD120 image when LOS
    // truncated it). This is the shape we expect post-#648
    // log analysis to reveal. Per #648.
    //
    // Burst-shape constants extracted per CR round 1 — keeps a
    // future test reader from wondering whether 240 / 80 / 3 / 2
    // are arbitrary or load-bearing. Each constant carries the
    // PD120 / Series 32 reference so a rebase against a future
    // mode change updates one place.
    /// VIS bursts captured in the pass — Series 32 duty cycle
    /// fits ~3 windows in a typical 7-minute overpass.
    const ARISS_EXPECTED_VIS_BURSTS: u32 = 3;
    /// Complete images decoded — bursts 1 + 2 finished, burst
    /// 3 was truncated by LOS.
    const ARISS_EXPECTED_COMPLETE_IMAGES: u32 = 2;
    /// PD120 image height in scan lines. Used here to fully
    /// populate bursts 1 + 2 in the synthetic event stream.
    const ARISS_FULL_IMAGE_LINES: usize = 240;
    /// Partial-image scan-line count for burst 3 — mid-frame
    /// when LOS / duty-cycle OFF cut the decode short. ~1/3
    /// of the way into the PD120 image.
    const ARISS_PARTIAL_IMAGE_LINES: usize = 80;

    let mut stats = SstvPassStats::default();

    // Burst 1: VIS → full image lines → ImageComplete
    stats.record_event(&fake_vis_event());
    for _ in 0..ARISS_FULL_IMAGE_LINES {
        stats.record_event(&fake_line_event());
    }
    stats.record_event(&fake_image_complete_event());

    // Burst 2: VIS → full image lines → ImageComplete
    stats.record_event(&fake_vis_event());
    for _ in 0..ARISS_FULL_IMAGE_LINES {
        stats.record_event(&fake_line_event());
    }
    stats.record_event(&fake_image_complete_event());

    // Burst 3: VIS → partial lines → LOS (no ImageComplete)
    stats.record_event(&fake_vis_event());
    for _ in 0..ARISS_PARTIAL_IMAGE_LINES {
        stats.record_event(&fake_line_event());
    }

    assert_eq!(stats.vis_count, ARISS_EXPECTED_VIS_BURSTS);
    assert_eq!(stats.image_complete_count, ARISS_EXPECTED_COMPLETE_IMAGES);
    assert_eq!(
        stats.lines_decoded,
        (ARISS_FULL_IMAGE_LINES * 2 + ARISS_PARTIAL_IMAGE_LINES) as u64,
    );
    // The "partial image" diagnostic: vis_count > image_complete_count
    // AND lines_decoded > 0 means we got imagery but lost it
    // before the final scan-line.
    assert!(stats.vis_count > stats.image_complete_count);
    assert!(stats.lines_decoded > 0);
}

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

/// Drain every pending `DspToUi` event from `rx`.
fn drain(rx: &mpsc::Receiver<DspToUi>) -> Vec<DspToUi> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
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
    std::env::temp_dir().join(format!("sdr-rs-ctl-test-{name}-{}.wav", std::process::id()))
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

/// #699 — the spectrum spans the raw rate but the VFO runs at the
/// post-decimation rate; an offset past ±effective/2 wraps to a
/// different station while the readout claims the clicked one.
#[test]
fn set_vfo_offset_is_clamped_to_half_effective_rate() {
    const OVERSHOOT_FACTOR: f64 = 4.0;
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    rebuild_vfo(&mut state).unwrap();
    let half = state.frontend.effective_sample_rate() / 2.0;
    let _ = drain(&dsp_rx);

    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetVfoOffset(half * OVERSHOOT_FACTOR),
    );
    assert!(
        (state.vfo_offset - half).abs() < f64::EPSILON,
        "offset must clamp to +effective/2, got {}",
        state.vfo_offset
    );
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::VfoOffsetChanged(o) if (o - half).abs() < f64::EPSILON)),
        "the echo must carry the clamped value, got {events:?}"
    );

    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetVfoOffset(-half * OVERSHOOT_FACTOR),
    );
    assert!((state.vfo_offset + half).abs() < f64::EPSILON);
}

/// #699 (CR round 1) — an offset that was reachable can become
/// unreachable when decimation shrinks the effective rate; the
/// rebuild must re-clamp it and echo the applied value.
#[test]
fn decimation_change_reclamps_vfo_offset_and_echoes() {
    const DECIM_START: u32 = 1;
    const DECIM_NARROW: u32 = 8;
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetDecimation(DECIM_START));
    let wide_half = state.frontend.effective_sample_rate() / 2.0;
    handle_command(&mut state, &dsp_tx, UiToDsp::SetVfoOffset(wide_half));
    assert!((state.vfo_offset - wide_half).abs() < f64::EPSILON);
    let _ = drain(&dsp_rx);

    handle_command(&mut state, &dsp_tx, UiToDsp::SetDecimation(DECIM_NARROW));
    let narrow_half = state.frontend.effective_sample_rate() / 2.0;
    assert!(
        narrow_half < wide_half,
        "test premise: decimation narrowed the span"
    );
    assert!(
        (state.vfo_offset - narrow_half).abs() < f64::EPSILON,
        "offset must re-clamp to the new ±effective/2, got {}",
        state.vfo_offset
    );
    let events = drain(&dsp_rx);
    assert!(
        events.iter().any(
            |e| matches!(e, DspToUi::VfoOffsetChanged(o) if (o - narrow_half).abs() < f64::EPSILON)
        ),
        "expected VfoOffsetChanged({narrow_half}), got {events:?}"
    );
}

fn test_pre_lock_snapshot() -> crate::acars_airband_lock::PreLockSnapshot {
    crate::acars_airband_lock::PreLockSnapshot {
        source_rate_hz: 2_400_000.0,
        center_freq_hz: 100_000_000.0,
        vfo_offset_hz: 0.0,
        source_type: SourceType::RtlSdr,
        frontend_decim: 8,
    }
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

/// #699 (CR round 3) — `rebuild_vfo` must be transactional: if the
/// new VFO cannot be built, neither the offset nor the old VFO change.
#[test]
fn rebuild_vfo_failure_leaves_state_untouched() {
    const UNREACHABLE_OFFSET_HZ: f64 = 1.0e9;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx).unwrap();
    rebuild_vfo(&mut state).unwrap();
    state.vfo_offset = UNREACHABLE_OFFSET_HZ;
    state.bandwidth = 0.0; // RxVfo::new rejects a zero-width channel
    assert!(
        rebuild_vfo(&mut state).is_err(),
        "test premise: rebuild fails"
    );
    assert!(
        (state.vfo_offset - UNREACHABLE_OFFSET_HZ).abs() < f64::EPSILON,
        "a failed rebuild must not clamp the stored offset"
    );
    assert!(state.vfo.is_some(), "the previous VFO must survive");
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

/// #700 — the shared LRPT canvas must be cleared between passes even
/// when no decoder is alive to do it as a side effect (e.g. after a
/// modulation change dropped it), or pass 2 composites onto pass 1.
/// #725 (Codacy on PR #802) — the harvest holds back the
/// in-progress row group; a modulation change drops the decoder,
/// so the pending group must be flushed to the shared image first.
#[test]
fn lrpt_modulation_change_flushes_the_pending_row_group() {
    use sdr_dsp::lrpt::LrptMode;
    use sdr_radio::lrpt_decoder::LrptDecoder;
    const APID: u16 = 64;
    /// One JPEG MCU is 8 × 8 px; a row group is `MCU_SIDE` lines.
    const MCU_SIDE: usize = 8;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let image = sdr_radio::lrpt_image::LrptImage::new();
    let mut decoder =
        LrptDecoder::new(image.clone(), LrptDownlink::new(LrptMode::Oqpsk, false)).unwrap();
    decoder
        .assembler_mut()
        .place_mcu(APID, 0, 0, &[[200_u8; MCU_SIDE]; MCU_SIDE]);
    state.lrpt_downlink = LrptDownlink::new(LrptMode::Oqpsk, false);
    state.lrpt_image = Some(image.clone());
    state.lrpt_decoder = Some(decoder);
    assert!(
        image.snapshot_channel(APID).is_none(),
        "held back until flushed"
    );

    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetLrptDownlink(LrptDownlink::new(LrptMode::Qpsk, false)),
    );

    assert!(state.lrpt_decoder.is_none(), "decoder dropped for re-init");
    let snap = image.snapshot_channel(APID).expect("pending group flushed");
    assert_eq!(snap.lines, MCU_SIDE);
}

/// The lazy-init path builds the decoder with the profile the
/// controller was told about — modulation and precoding — and a
/// later chunk reuses that decoder (#730).
#[test]
fn lrpt_decode_tap_lazily_builds_the_decoder_with_the_profile() {
    use sdr_dsp::lrpt::LrptMode;
    let image = sdr_radio::lrpt_image::LrptImage::new();
    let mut slot: Option<LrptDecoder> = None;
    let mut init_failed = false;
    let profile = LrptDownlink::new(LrptMode::Oqpsk, true);
    let zeros = vec![Complex::default(); 256];
    lrpt_decode_tap(&mut slot, Some(&image), &zeros, &mut init_failed, profile);
    assert!(!init_failed);
    assert_eq!(slot.as_ref().map(LrptDecoder::downlink), Some(profile));
    // A populated slot is reused, not rebuilt.
    lrpt_decode_tap(&mut slot, Some(&image), &zeros, &mut init_failed, profile);
    assert!(!init_failed);
    assert_eq!(slot.as_ref().map(LrptDecoder::downlink), Some(profile));
    // No image handle → no decoder is built.
    let mut no_image: Option<LrptDecoder> = None;
    lrpt_decode_tap(&mut no_image, None, &zeros, &mut init_failed, profile);
    assert!(no_image.is_none());
}

/// AOS ordering: `SetLrptDownlink` flushes the old decoder's
/// held-back row group into the shared image, then
/// `ClearLrptImageContents` wipes the canvas — in that order on
/// the DSP queue, so the previous pass's tail cannot survive onto
/// the new pass (CR on PR #806).
#[test]
fn lrpt_profile_change_then_clear_leaves_an_empty_canvas() {
    use sdr_dsp::lrpt::LrptMode;
    /// One JPEG MCU is 8 × 8 px (sdr-core has no `sdr_lrpt` dep).
    const MCU_SIDE: usize = 8;
    const APID: u16 = 64;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let image = sdr_radio::lrpt_image::LrptImage::new();
    let mut decoder =
        LrptDecoder::new(image.clone(), LrptDownlink::new(LrptMode::Oqpsk, false)).unwrap();
    decoder
        .assembler_mut()
        .place_mcu(APID, 0, 0, &[[200_u8; MCU_SIDE]; MCU_SIDE]);
    state.lrpt_downlink = LrptDownlink::new(LrptMode::Oqpsk, false);
    state.lrpt_image = Some(image.clone());
    state.lrpt_decoder = Some(decoder);
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetLrptDownlink(LrptDownlink::new(LrptMode::Qpsk, false)),
    );
    assert!(
        image.snapshot_channel(APID).is_some(),
        "flushed tail lands first"
    );
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::ClearLrptImageContents(image.clone()),
    );
    assert!(
        image.snapshot_channel(APID).is_none(),
        "then the canvas is wiped"
    );
}

/// A precoding change alone (same modulation) also drops the
/// decoder so the next init builds the right FEC chain (#730).
#[test]
fn lrpt_precoding_change_drops_the_decoder() {
    use sdr_dsp::lrpt::LrptMode;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let image = sdr_radio::lrpt_image::LrptImage::new();
    state.lrpt_downlink = LrptDownlink::new(LrptMode::Oqpsk, false);
    state.lrpt_decoder = Some(LrptDecoder::new(image.clone(), state.lrpt_downlink).unwrap());
    state.lrpt_image = Some(image);
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetLrptDownlink(LrptDownlink::new(LrptMode::Oqpsk, true)),
    );
    assert!(state.lrpt_decoder.is_none(), "decoder dropped for re-init");
    assert_eq!(
        state.lrpt_downlink,
        LrptDownlink::new(LrptMode::Oqpsk, true)
    );
}

#[test]
fn reset_imaging_decoders_clears_lrpt_image_without_a_decoder() {
    const APID: u16 = 64;
    const LINE_WIDTH: usize = 8;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx).unwrap();
    let image = sdr_radio::lrpt_image::LrptImage::new();
    image.push_line(APID, &[0x80; LINE_WIDTH]);
    assert!(
        !image.channel_apids().is_empty(),
        "test premise: a line landed"
    );
    state.lrpt_image = Some(image);
    state.lrpt_decoder = None;

    reset_imaging_decoders(&mut state);

    let image = state.lrpt_image.as_ref().unwrap();
    assert!(
        image.channel_apids().is_empty(),
        "stale pixels survived the between-pass reset"
    );
}

/// #736 — a new VIS means a new image: the in-flight buffer must be
/// reset so a different-geometry mode after an incomplete image is
/// not silently dropped row by row (and the old rows saved as it).
#[test]
fn sstv_vis_detected_resets_the_in_flight_image() {
    const STALE_W: u32 = 320;
    const STALE_H: u32 = 256;
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let image = sdr_radio::sstv_image::SstvImage::new();
    let handle = image.handle();
    handle.write_line(0, STALE_W, STALE_H, &[[1, 2, 3]; STALE_W as usize]);
    assert!(
        handle.snapshot().is_some(),
        "test premise: a stale row exists"
    );
    state.sstv_image = Some(handle.clone());
    let _ = drain(&dsp_rx);

    handle_sstv_event(&mut state, &dsp_tx, fake_vis_event());

    assert!(
        handle.snapshot().is_none(),
        "VIS must reset the in-flight image buffer"
    );
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::SstvVisDetected { .. })),
        "the VIS notification must still reach the UI, got {events:?}"
    );
}

/// #692 — the IQ-correction switch must not share state with DC blocking.
#[test]
fn set_iq_correction_does_not_alias_dc_blocking() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetDcBlocking(true));
    handle_command(&mut state, &dsp_tx, UiToDsp::SetIqCorrection(false));
    assert!(
        state.dc_blocking,
        "IQ correction off must leave DC blocking on"
    );
    assert!(!state.iq_correction);
    assert!(!state.frontend.iq_correction());

    handle_command(&mut state, &dsp_tx, UiToDsp::SetIqCorrection(true));
    handle_command(&mut state, &dsp_tx, UiToDsp::SetDcBlocking(false));
    assert!(
        state.iq_correction,
        "DC blocking off must leave IQ correction on"
    );
    assert!(state.frontend.iq_correction());
    assert!(!state.dc_blocking);
}

/// #692 — a frontend rebuild (sample-rate change) must carry the IQ-correction setting.
#[test]
fn rebuild_frontend_preserves_iq_correction() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetIqCorrection(true));
    rebuild_frontend(&mut state).unwrap();
    assert!(state.frontend.iq_correction());
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

/// #692 (CR round 1) — every path that replaces the frontend must
/// carry the IQ-correction setting, not just `rebuild_frontend`.
#[test]
fn set_fft_size_preserves_iq_correction() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetIqCorrection(true));
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetFftSize(DEFAULT_FFT_SIZE * 2),
    );
    assert_eq!(state.frontend.fft_size(), DEFAULT_FFT_SIZE * 2);
    assert!(state.frontend.iq_correction());
}

#[test]
fn set_window_function_preserves_iq_correction() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetIqCorrection(true));
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetWindowFunction(sdr_pipeline::iq_frontend::FftWindow::Blackman),
    );
    assert!(state.frontend.iq_correction());
}

/// #697 — a mode switch resets the bandwidth to the mode default and
/// must tell the UI so row / overlay / status bar agree with the engine.
#[test]
fn set_demod_mode_emits_bandwidth_changed_with_mode_default() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    rebuild_vfo(&mut state).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetBandwidth(12_000.0));
    let _ = drain(&dsp_rx);

    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetDemodMode(sdr_types::DemodMode::Am),
    );
    let expected = state.radio.demod_config().default_bandwidth;
    assert!((state.bandwidth - expected).abs() < f64::EPSILON);
    let events = drain(&dsp_rx);
    assert!(
        events.iter().any(
            |e| matches!(e, DspToUi::BandwidthChanged(bw) if (bw - expected).abs() < f64::EPSILON)
        ),
        "expected BandwidthChanged({expected}), got {events:?}"
    );
}

/// #764 — tuning to a new centre frequency is a fresh start: the VFO
/// offset must reset to 0 in the engine (the UI overlay already does)
/// and the reset must be echoed so every readout agrees.
#[test]
fn tune_resets_vfo_offset_and_echoes_it() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    rebuild_vfo(&mut state).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetVfoOffset(50_000.0));
    assert!((state.vfo_offset - 50_000.0).abs() < f64::EPSILON);
    let _ = drain(&dsp_rx);

    handle_command(&mut state, &dsp_tx, UiToDsp::Tune(101_000_000.0));
    assert!(
        state.vfo_offset.abs() < f64::EPSILON,
        "Tune must reset the engine VFO offset"
    );
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::VfoOffsetChanged(o) if o.abs() < f64::EPSILON)),
        "expected VfoOffsetChanged(0.0), got {events:?}"
    );
}

#[test]
fn rebuild_vfo_creates_vfo_and_sets_radio_rate() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx).unwrap();
    // Simulate what open_source does: frontend is already built at default rate.
    rebuild_vfo(&mut state).unwrap();
    assert!(state.vfo.is_some());
}

#[test]
fn rebuild_vfo_after_mode_switch_changes_rates() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx).unwrap();
    // Start with NFM (default) — IF rate 50 kHz
    rebuild_vfo(&mut state).unwrap();

    // Switch to WFM — IF rate 250 kHz
    state.radio.set_mode(sdr_types::DemodMode::Wfm).unwrap();
    rebuild_vfo(&mut state).unwrap();
    assert!(state.vfo.is_some());

    // Switch to NFM — IF rate 50 kHz (different from WFM)
    state.radio.set_mode(sdr_types::DemodMode::Nfm).unwrap();
    rebuild_vfo(&mut state).unwrap();
    assert!(state.vfo.is_some());
}

#[test]
fn vfo_preserves_signal_at_zero_offset() {
    // Create an RxVfo at same in/out rate, full bandwidth, offset 0.
    // The signal at DC should pass through essentially unchanged.
    let rate = 250_000.0;
    let mut vfo = RxVfo::new(rate, rate, rate, 0.0).unwrap();
    let input = vec![Complex::new(1.0, 0.0); 1000];
    let mut output = vec![Complex::default(); 1100];
    let count = vfo.process(&input, &mut output).unwrap();
    assert_eq!(count, 1000);
    // DC signal at zero offset should pass through with ~unity amplitude.
    for (i, s) in output[..count].iter().enumerate() {
        assert!(
            s.amplitude() > 0.9,
            "sample {i}: amplitude {} too low",
            s.amplitude()
        );
    }
}

#[test]
fn vfo_translates_offset_signal_to_baseband() {
    // Generate a tone at +10 kHz offset within a 250 kHz stream.
    // Set VFO offset to +10 kHz so the tone lands at DC after translation.
    let in_rate = 250_000.0;
    let offset_hz = 10_000.0;
    let n = 2500;

    // Generate a pure tone at +offset_hz.
    let input: Vec<Complex> = (0..n)
        .map(|i| {
            let phase = 2.0 * std::f64::consts::PI * offset_hz * (i as f64) / in_rate;
            #[allow(clippy::cast_possible_truncation)]
            Complex::new(phase.cos() as f32, phase.sin() as f32)
        })
        .collect();

    let mut vfo = RxVfo::new(in_rate, in_rate, in_rate, offset_hz).unwrap();
    let mut output = vec![Complex::default(); n + 100];
    let count = vfo.process(&input, &mut output).unwrap();
    assert!(count > 0);

    // After translation by -offset_hz, the signal should be near DC.
    // Skip the first few samples (filter settling) and check that the
    // imaginary part is small (signal is near real-only at DC).
    let settle = count / 4;
    let avg_imag: f32 = output[settle..count]
        .iter()
        .map(|s| s.im.abs())
        .sum::<f32>()
        / (count - settle) as f32;
    assert!(
        avg_imag < 0.15,
        "after translation, signal should be near DC — avg |imag| = {avg_imag}"
    );
}

#[test]
fn vfo_resamples_250k_to_50k() {
    // Simulates WFM frontend (250 kHz) feeding NFM demod (50 kHz).
    let in_rate = 250_000.0;
    let out_rate = 50_000.0;
    let bandwidth = 12_500.0;
    let n = 2500; // 10 ms at 250 kHz

    let mut vfo = RxVfo::new(in_rate, out_rate, bandwidth, 0.0).unwrap();
    let input = vec![Complex::new(1.0, 0.0); n];
    let mut output = vec![Complex::default(); n]; // more than enough
    let count = vfo.process(&input, &mut output).unwrap();

    // Expected ~500 samples (2500 * 50k/250k)
    assert!(
        (400..=600).contains(&count),
        "expected ~500 samples at 50 kHz, got {count}"
    );
}

// ACARS decode-tap unit tests (#474). Inlined here per the
// workspace convention (tests at file bottom in
// `#[cfg(test)] mod tests`); access `acars_decode_tap`
// directly through the module hierarchy. End-to-end
// engage→ack→disengage is covered by the `Engine`-API
// integration test in `tests/acars_pipeline_integration.rs`.

use crate::acars_airband_lock::{ACARS_CENTER_HZ, ACARS_SOURCE_RATE_HZ, US_SIX_CHANNELS_HZ};

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
