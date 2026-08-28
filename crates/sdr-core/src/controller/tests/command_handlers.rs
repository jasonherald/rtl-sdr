use super::*;

// Coverage for the #816 PR B command handlers CodeRabbit flagged as
// untested: the gain-index bounds fallback (which must not dispatch
// out-of-range indices to remote rtl_tcp servers) and the audio-sink
// swap lifecycle in `start_swapped_sink` (offline latch + status
// events).

/// Fake source for the gain-index bounds check: a configurable local
/// gain table, an optional `rtl_tcp` `Connected.gain_count`, and a
/// `set_gain_by_index` that always errors — so a `"Set gain failed"`
/// event is positive proof the command was dispatched to the source,
/// distinguishable from the `"out of range"` rejection that must
/// short-circuit before dispatch.
struct FakeGainSource {
    gains: Vec<i32>,
    rtl_tcp_gain_count: Option<u32>,
}

impl Source for FakeGainSource {
    fn name(&self) -> &'static str {
        "fake-gain"
    }
    fn start(&mut self) -> Result<(), sdr_types::SourceError> {
        Ok(())
    }
    fn stop(&mut self) -> Result<(), sdr_types::SourceError> {
        Ok(())
    }
    fn tune(&mut self, _frequency_hz: f64) -> Result<(), sdr_types::SourceError> {
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
    fn gains(&self) -> &[i32] {
        &self.gains
    }
    fn set_gain_by_index(&mut self, _index: u32) -> Result<(), sdr_types::SourceError> {
        Err(sdr_types::SourceError::InvalidParameter("fake".to_string()))
    }
    fn rtl_tcp_connection_state(&self) -> Option<RtlTcpConnectionState> {
        self.rtl_tcp_gain_count
            .map(|gain_count| RtlTcpConnectionState::Connected {
                tuner_name: "R820T".to_string(),
                gain_count,
                codec: "None".to_string(),
                granted_role: None,
            })
    }
}

fn state_with_gain_source(
    gains: Vec<i32>,
    rtl_tcp_gain_count: Option<u32>,
) -> (DspState, mpsc::Sender<DspToUi>, mpsc::Receiver<DspToUi>) {
    let (dsp_tx, rx) = mpsc::channel();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.source = Some(Box::new(FakeGainSource {
        gains,
        rtl_tcp_gain_count,
    }));
    (state, dsp_tx, rx)
}

fn error_messages(rx: &mpsc::Receiver<DspToUi>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let DspToUi::Error(msg) = ev {
            out.push(msg);
        }
    }
    out
}

#[test]
fn gain_index_rejected_by_local_table() {
    let (mut state, dsp_tx, rx) = state_with_gain_source(vec![0, 90, 496], None);
    handle_command(&mut state, &dsp_tx, UiToDsp::SetGainByIndex(3));
    let errs = error_messages(&rx);
    assert_eq!(errs.len(), 1, "expected exactly one rejection: {errs:?}");
    assert!(errs[0].contains("out of range"), "{errs:?}");
    // Persisted regardless — replay bounds-checks again at open time.
    assert_eq!(state.tuner_gain_index, Some(3));
}

#[test]
fn gain_index_falls_back_to_rtl_tcp_gain_count_for_rejection() {
    // Empty local table (rtl_tcp sources can't populate it) — the
    // Connected state's gain_count is the only source of truth.
    let (mut state, dsp_tx, rx) = state_with_gain_source(Vec::new(), Some(5));
    handle_command(&mut state, &dsp_tx, UiToDsp::SetGainByIndex(5));
    let errs = error_messages(&rx);
    assert_eq!(errs.len(), 1, "expected exactly one rejection: {errs:?}");
    assert!(
        errs[0].contains("out of range"),
        "index 5 must not be dispatched to a 5-gain server: {errs:?}"
    );
}

#[test]
fn gain_index_within_rtl_tcp_gain_count_is_dispatched() {
    let (mut state, dsp_tx, rx) = state_with_gain_source(Vec::new(), Some(5));
    handle_command(&mut state, &dsp_tx, UiToDsp::SetGainByIndex(4));
    let errs = error_messages(&rx);
    // The fake's set_gain_by_index always errors, so this message is
    // proof the in-range index reached the source.
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].contains("Set gain failed"), "{errs:?}");
}

#[test]
fn gain_index_unchecked_when_no_table_and_not_connected() {
    // Neither a local table nor a Connected rtl_tcp state: the
    // command goes through unchecked (the source may no-op or
    // surface a wire error later).
    let (mut state, dsp_tx, rx) = state_with_gain_source(Vec::new(), None);
    handle_command(&mut state, &dsp_tx, UiToDsp::SetGainByIndex(999));
    let errs = error_messages(&rx);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].contains("Set gain failed"), "{errs:?}");
}

// ─── start_swapped_sink lifecycle ───

fn network_statuses(rx: &mpsc::Receiver<DspToUi>) -> Vec<NetworkSinkStatus> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let DspToUi::NetworkSinkStatus(s) = ev {
            out.push(s);
        }
    }
    out
}

#[test]
fn sink_swap_while_stopped_emits_inactive_only() {
    let (dsp_tx, rx) = mpsc::channel();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    assert!(!state.running);
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetAudioSinkType(AudioSinkType::Network),
    );
    // Nothing is on the wire yet — the panel must not misreport a
    // not-yet-bound sink as Active.
    assert_eq!(network_statuses(&rx), vec![NetworkSinkStatus::Inactive]);
    assert!(!state.audio_sink_offline);
}

#[test]
fn sink_swap_while_running_success_emits_active_and_clears_latch() {
    let (dsp_tx, rx) = mpsc::channel();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.running = true;
    state.audio_sink_offline = true; // stale latch from a prior failure
    "127.0.0.1".clone_into(&mut state.network_sink_host);
    state.network_sink_port = 0; // ephemeral bind always succeeds
    state.network_sink_protocol = sdr_types::Protocol::TcpClient;
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetAudioSinkType(AudioSinkType::Network),
    );
    let statuses = network_statuses(&rx);
    assert!(
        matches!(&statuses[..], [NetworkSinkStatus::Active { .. }]),
        "{statuses:?}"
    );
    assert!(
        !state.audio_sink_offline,
        "successful start must clear the offline latch"
    );
}

#[test]
fn sink_swap_while_running_failure_latches_offline_and_emits_error() {
    // Occupy a port so the sink's TCP listener bind fails
    // deterministically.
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = blocker.local_addr().unwrap().port();

    let (dsp_tx, rx) = mpsc::channel();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.running = true;
    "127.0.0.1".clone_into(&mut state.network_sink_host);
    state.network_sink_port = port;
    state.network_sink_protocol = sdr_types::Protocol::TcpClient;
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetAudioSinkType(AudioSinkType::Network),
    );
    let statuses = network_statuses(&rx);
    assert!(
        matches!(&statuses[..], [NetworkSinkStatus::Error { .. }]),
        "{statuses:?}"
    );
    assert!(
        state.audio_sink_offline,
        "failed start must latch the write path off"
    );
    drop(blocker);
}

// ── Airspy pre-start / post-start settings mapping (#848 / PR #850) ──

/// Records every gain / mode / bias dispatch so the Airspy settings
/// helpers can be verified without hardware.
#[derive(Default)]
struct RecordingSource {
    gain_modes: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
    gains: std::sync::Arc<std::sync::Mutex<Vec<i32>>>,
    bias: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
    offsets: std::sync::Arc<std::sync::Mutex<Vec<f64>>>,
}

impl Source for RecordingSource {
    fn name(&self) -> &'static str {
        "recording"
    }
    fn start(&mut self) -> Result<(), sdr_types::SourceError> {
        Ok(())
    }
    fn stop(&mut self) -> Result<(), sdr_types::SourceError> {
        Ok(())
    }
    fn tune(&mut self, _frequency_hz: f64) -> Result<(), sdr_types::SourceError> {
        Ok(())
    }
    fn sample_rates(&self) -> &[f64] {
        &[2_500_000.0]
    }
    fn sample_rate(&self) -> f64 {
        2_500_000.0
    }
    fn set_sample_rate(&mut self, _rate: f64) -> Result<(), sdr_types::SourceError> {
        Ok(())
    }
    fn read_samples(&mut self, _output: &mut [Complex]) -> Result<usize, sdr_types::SourceError> {
        Ok(0)
    }
    fn set_gain(&mut self, gain_tenths: i32) -> Result<(), sdr_types::SourceError> {
        self.gains.lock().unwrap().push(gain_tenths);
        Ok(())
    }
    fn set_gain_mode(&mut self, manual: bool) -> Result<(), sdr_types::SourceError> {
        self.gain_modes.lock().unwrap().push(manual);
        Ok(())
    }
    fn set_bias_tee(&mut self, enabled: bool) -> Result<(), sdr_types::SourceError> {
        self.bias.lock().unwrap().push(enabled);
        Ok(())
    }
    fn set_converter_offset(&mut self, offset_hz: f64) -> Result<(), sdr_types::SourceError> {
        self.offsets.lock().unwrap().push(offset_hz);
        Ok(())
    }
}

#[test]
fn airspy_pre_start_dispatches_mode_before_gain() {
    let (dsp_tx, _rx) = mpsc::channel();
    let mut state = DspState::new(dsp_tx).unwrap();
    state.tuner_agc_auto = false;
    state.tuner_gain_tenths_db = 140;
    let mut source = RecordingSource::default();
    let (modes, gains) = (
        std::sync::Arc::clone(&source.gain_modes),
        std::sync::Arc::clone(&source.gains),
    );
    super::super::source::airspy_pre_start_settings(&state, &mut source);
    // Manual mode (AGC off) then the persisted gain — order matters:
    // the composite gain write assumes AGCs are being disabled, and
    // the value maps to the 0-21 linearity ladder inside the source.
    assert_eq!(*modes.lock().unwrap(), vec![true]);
    assert_eq!(*gains.lock().unwrap(), vec![140]);
}

#[test]
fn airspy_pre_start_respects_agc_auto() {
    let (dsp_tx, _rx) = mpsc::channel();
    let mut state = DspState::new(dsp_tx).unwrap();
    state.tuner_agc_auto = true;
    let mut source = RecordingSource::default();
    let modes = std::sync::Arc::clone(&source.gain_modes);
    super::super::source::airspy_pre_start_settings(&state, &mut source);
    assert_eq!(
        *modes.lock().unwrap(),
        vec![false],
        "agc_auto → manual=false"
    );
}

#[test]
fn airspy_replay_applies_persisted_bias_tee() {
    let (dsp_tx, rx) = mpsc::channel();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.bias_tee_enabled = true;
    let mut source = RecordingSource::default();
    let bias = std::sync::Arc::clone(&source.bias);
    super::super::source::airspy_replay_persisted_settings(&state, &mut source, &dsp_tx);
    assert_eq!(*bias.lock().unwrap(), vec![true]);
    // Success path emits no error event.
    assert!(
        !matches!(rx.try_recv(), Ok(DspToUi::Error(_))),
        "no error toast on successful bias replay"
    );
}

#[test]
fn pre_start_settings_replay_converter_offset_on_both_sources() {
    // #848 phase 4: the persisted upconverter offset must reach the
    // source before start() on both USB source flavors.
    let (dsp_tx, _rx) = mpsc::channel();
    let mut state = DspState::new(dsp_tx).unwrap();
    state.converter_offset_hz = 120_000_000.0;
    let mut source = RecordingSource::default();
    let offsets = std::sync::Arc::clone(&source.offsets);
    super::super::source::airspy_pre_start_settings(&state, &mut source);
    super::super::source::rtl_sdr_pre_start_settings(&state, &mut source);
    assert_eq!(*offsets.lock().unwrap(), vec![120_000_000.0, 120_000_000.0]);
}

#[test]
fn handle_set_converter_offset_persists_and_forwards_live() {
    let (dsp_tx, _rx) = mpsc::channel();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let source = RecordingSource::default();
    let offsets = std::sync::Arc::clone(&source.offsets);
    state.source = Some(Box::new(source));
    super::super::source::handle_set_converter_offset(&mut state, &dsp_tx, 125_000_000.0);
    assert!((state.converter_offset_hz - 125_000_000.0).abs() < f64::EPSILON);
    assert_eq!(*offsets.lock().unwrap(), vec![125_000_000.0]);
}

#[test]
fn handle_command_routes_set_converter_offset() {
    // Codacy round 1 on PR #851: exercise the actual dispatch arm,
    // not just the extracted handler, so a broken match arm can't
    // hide behind direct-call tests.
    let (dsp_tx, _rx) = mpsc::channel();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let source = RecordingSource::default();
    let offsets = std::sync::Arc::clone(&source.offsets);
    state.source = Some(Box::new(source));
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetConverterOffset(120_000_000.0),
    );
    assert!((state.converter_offset_hz - 120_000_000.0).abs() < f64::EPSILON);
    assert_eq!(*offsets.lock().unwrap(), vec![120_000_000.0]);
}
