use super::*;

fn make_test_state() -> Rc<AppState> {
    let (tx, _rx) = mpsc::channel();
    AppState::new_shared(tx)
}

#[test]
fn test_default_state() {
    let state = make_test_state();
    assert!(!state.is_running.get());
    assert!((state.center_frequency.get() - DEFAULT_CENTER_FREQUENCY_HZ).abs() < f64::EPSILON);
    assert_eq!(state.demod_mode.get(), DemodMode::Wfm);
}

#[test]
fn acars_defaults_pin_initializer_contract() {
    // Pin the ACARS field defaults so a future regression
    // (e.g. changing the keep-count config helper, swapping
    // the ChannelStats default, or accidentally pre-loading
    // a snapshot) fails this test instead of silently
    // shipping a UI that mis-states ACARS state. Per
    // CodeRabbit round 3 on PR #584.
    let state = make_test_state();
    assert!(!state.acars_enabled.get(), "ACARS toggle defaults off");
    assert_eq!(state.acars_total_count.get(), 0, "no decoded messages yet");
    let recent = state.acars_recent.borrow();
    assert!(recent.is_empty(), "ring is empty on init");
    // `VecDeque::with_capacity(n)` guarantees AT LEAST n —
    // the allocator may round up. Pin the lower bound rather
    // than exact equality so allocator-growth differences
    // across toolchains don't false-fail this test. Per CR
    // round 4 on PR #584.
    assert!(
        recent.capacity() >= crate::acars_config::default_recent_keep() as usize,
        "ring capacity sourced from acars_config::default_recent_keep (>= {}, got {})",
        crate::acars_config::default_recent_keep(),
        recent.capacity(),
    );
    drop(recent);
    assert!(
        state.acars_channel_stats.borrow().is_empty(),
        "channel-stats Vec starts empty; populated by AcarsChannelStats arrivals"
    );
    assert!(
        state.acars_pre_lock_state.borrow().is_none(),
        "no snapshot until first engage"
    );
    assert!(
        state.acars_viewer_window.borrow().is_none(),
        "no viewer window until first open"
    );
    assert!(
        state.acars_saved_tune.get().is_none(),
        "no saved pre-engage tune (center, offset) until first engage"
    );
    assert!(
        !state.acars_was_engaged_pre_pass.get(),
        "auto-record pre-pass ACARS flag defaults false"
    );
    assert!(
        !state.acars_pending.get(),
        "no SetAcarsEnabled command in flight at construction"
    );
    assert!(
        state.acars_saved_volume.get().is_none(),
        "no saved pre-engage volume until first engage"
    );
    assert!(
        !state.suppress_volume_notify.get(),
        "volume notify suppression off at construction"
    );
    assert!(
        state.pending_aos_actions.borrow().is_none(),
        "no pending AOS action batch stashed at construction"
    );
    assert!(
        state.recorder_action_interpreter.borrow().is_none(),
        "no recorder action interpreter wired until satellites panel connects"
    );
    assert!(
        state.acars_viewer_handles.borrow().is_none(),
        "no viewer handles until first open"
    );
}

#[test]
fn last_dispatched_vfo_offset_hz_defaults_to_zero() {
    // Pin the Doppler dispatch baseline default. Per CR
    // round 8 on PR #554 — without this regression test, a
    // future change to the seeded value would silently break
    // the rate-limit gate's "compare against actual current
    // DSP state" invariant. The first 4 Hz Doppler tick
    // computes `live - baseline`; if `baseline` starts at
    // anything but 0, that comparison is wrong until the
    // first echo from a real `SetVfoOffset` lands.
    let state = make_test_state();
    assert!(
        (state.last_dispatched_vfo_offset_hz.get() - 0.0).abs() < f64::EPSILON,
        "got {}",
        state.last_dispatched_vfo_offset_hz.get()
    );
}

#[test]
fn test_state_mutation() {
    let state = make_test_state();
    state.is_running.set(true);
    state.center_frequency.set(144_000_000.0);
    state.demod_mode.set(DemodMode::Nfm);

    assert!(state.is_running.get());
    assert!((state.center_frequency.get() - 144_000_000.0).abs() < f64::EPSILON);
    assert_eq!(state.demod_mode.get(), DemodMode::Nfm);
}

#[test]
fn test_send_dsp_with_dropped_receiver() {
    let (tx, rx) = mpsc::channel();
    let state = AppState::new_shared(tx);
    drop(rx);
    // Should not panic — just logs a warning.
    state.send_dsp(UiToDsp::Stop);
}

#[test]
fn defaults_are_safe_for_close_to_tray() {
    let s = make_test_state();
    assert!(s.close_to_tray.get(), "default close_to_tray must be true");
    assert!(!s.tray_first_close_seen.get());
    assert!(s.tray_available.get());
    assert!(!s.audio_recording_active.get());
    assert!(!s.iq_recording_active.get());
    assert!(s.lrpt_recording_pass.borrow().is_none());
    assert!(
        s.sstv_recording_pass.borrow().is_none(),
        "no SSTV pass in flight at construction"
    );
}

#[test]
fn is_recording_is_false_when_idle() {
    let s = make_test_state();
    assert!(!s.is_recording());
}

#[test]
fn is_recording_table() {
    // Per repo rule on named constants for magic numbers
    // (`crates/CLAUDE.md`). Per CR round 6 on PR #599.
    const ISS_NORAD_ID: u32 = 25_544;
    const NOAA_19_NORAD_ID: u32 = 33_591;
    const NOAA_LRPT_PLACEHOLDER_ID: u32 = 33_592;
    // Each row: (apt, lrpt, sstv, audio, iq, sstv_pending, expected)
    // The `sstv_pending` column covers in-memory retry batches
    // queued by an LOS save failure (or a late-tail post-success
    // move). Without OR-ing this into `is_recording()` the tray
    // Quit path would treat the app as idle and silently drop the
    // pending imagery. Per CR round 9 #28 on PR #599.
    let cases = [
        (false, false, false, false, false, false, false),
        (true, false, false, false, false, false, true),
        (false, true, false, false, false, false, true),
        (false, false, true, false, false, false, true),
        (false, false, false, true, false, false, true),
        (false, false, false, false, true, false, true),
        (false, false, false, false, false, true, true),
        (true, true, true, true, true, true, true),
        (true, false, false, false, true, false, true),
        (false, true, false, true, false, false, true),
        (false, false, true, false, true, false, true),
        (false, false, false, false, false, true, true),
    ];
    for (apt, lrpt, sstv, audio, iq, sstv_pending, expected) in cases {
        let s = make_test_state();
        if apt {
            *s.apt_recording_pass.borrow_mut() = Some((NOAA_19_NORAD_ID, chrono::Utc::now()));
        }
        if lrpt {
            // NORAD 33_592 = NOAA 19 placeholder; matches the
            // shape `apt_recording_pass` uses above. Per CR
            // round 2 on PR #575.
            *s.lrpt_recording_pass.borrow_mut() =
                Some((NOAA_LRPT_PLACEHOLDER_ID, chrono::Utc::now()));
        }
        if sstv {
            // ISS NORAD 25_544 — the only SSTV entry in the
            // catalog. Per epic #472.
            *s.sstv_recording_pass.borrow_mut() = Some((ISS_NORAD_ID, chrono::Utc::now()));
        }
        s.audio_recording_active.set(audio);
        s.iq_recording_active.set(iq);
        if sstv_pending {
            // Single empty placeholder batch is enough — the
            // guard checks `is_empty()`, not image count.
            s.sstv_pending_export.borrow_mut().push(PendingSstvExport {
                dir: std::path::PathBuf::from("/tmp/test-sstv-pending"),
                start_index: 0,
                images: Vec::new(),
            });
        }
        assert_eq!(
            s.is_recording(),
            expected,
            "row apt={apt} lrpt={lrpt} sstv={sstv} audio={audio} iq={iq} sstv_pending={sstv_pending}",
        );
    }
}

#[test]
fn rtl_tcp_state_discriminant_covers_all_variants() {
    // Lock-in test so a future `RtlTcpConnectionState`
    // variant reorder doesn't silently desync the
    // `RTL_TCP_STATE_DISC_*` u8 constants used by the
    // toast edge-detection path. The constants are
    // `Cell<u8>`-friendly projections of the enum's
    // variant ordering and must match 1:1. Per
    // CodeRabbit round 1 on PR #408.
    use std::time::Duration;
    assert_eq!(
        rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::Disconnected),
        RTL_TCP_STATE_DISC_DISCONNECTED
    );
    assert_eq!(
        rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::Connecting),
        RTL_TCP_STATE_DISC_CONNECTING
    );
    assert_eq!(
        rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::Connected {
            tuner_name: "R820T".into(),
            gain_count: 29,
            codec: "None".into(),
            granted_role: Some(true),
        }),
        RTL_TCP_STATE_DISC_CONNECTED
    );
    assert_eq!(
        rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::Retrying {
            attempt: 1,
            retry_in: Duration::from_secs(1),
        }),
        RTL_TCP_STATE_DISC_RETRYING
    );
    assert_eq!(
        rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::Failed {
            reason: "x".into(),
        }),
        RTL_TCP_STATE_DISC_FAILED
    );
    assert_eq!(
        rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::ControllerBusy),
        RTL_TCP_STATE_DISC_CONTROLLER_BUSY
    );
    assert_eq!(
        rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::AuthRequired),
        RTL_TCP_STATE_DISC_AUTH_REQUIRED
    );
    assert_eq!(
        rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::AuthFailed),
        RTL_TCP_STATE_DISC_AUTH_FAILED
    );
}
