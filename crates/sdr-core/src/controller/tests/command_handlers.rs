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
