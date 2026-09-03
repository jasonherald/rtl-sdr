use super::*;

// Build a test audio chunk corresponding to `ms` of stereo
// interleaved silence at the wire format rate. Uses the same
// constants the production buffer-duration math uses so the two
// stay in sync if the wire format ever changes.
fn samples_for_ms(ms: u32) -> Vec<f32> {
    let frames_per_ms = (TRANSCRIPTION_INPUT_SAMPLE_RATE_HZ / 1000) as usize;
    let frames = frames_per_ms * (ms as usize);
    vec![0.5_f32; frames * TRANSCRIPTION_INPUT_CHANNELS]
}

// Default-threshold machine so the PR 8 test expectations around
// the 100/200/400 ms thresholds still hold after the constants
// moved to per-session fields in issue #272.
fn default_machine() -> AutoBreakMachine {
    AutoBreakMachine::new(super::super::host::AutoBreakThresholds::defaults())
}

// Default-threshold alias so tests can phrase the intent as
// "buffer should be ≥ MIN_OPEN" without reaching for the
// session-level `crate::backend::AUTO_BREAK_*` re-exports each
// time. Matches `AutoBreakThresholds::defaults().min_open_ms`.
const TEST_MIN_OPEN_MS: u32 = crate::backend::AUTO_BREAK_MIN_OPEN_MS_DEFAULT;

#[test]
fn clean_utterance_produces_one_decode() {
    let mut machine = default_machine();
    let mut counts = AutoBreakFlushCounts::default();

    machine.on_squelch_opened();
    machine.on_samples(&samples_for_ms(1_000));
    machine.on_squelch_closed();
    if let Some(decision) = machine.on_tail_timeout() {
        counts.record(decision);
    }

    assert_eq!(counts.decodes_flushed, 1);
    assert_eq!(counts.discarded_short, 0);
    assert_eq!(counts.discarded_phantom, 0);
}

#[test]
fn hysteresis_blip_single_utterance() {
    let mut machine = default_machine();
    let mut counts = AutoBreakFlushCounts::default();

    // Open, record, close, re-open before tail timeout, record more, close, timeout.
    machine.on_squelch_opened();
    machine.on_samples(&samples_for_ms(500));
    machine.on_squelch_closed();
    // Hysteresis blip: squelch re-opens before tail fires.
    machine.on_squelch_opened();
    machine.on_samples(&samples_for_ms(500));
    machine.on_squelch_closed();
    if let Some(decision) = machine.on_tail_timeout() {
        counts.record(decision);
    }

    // One decode, not two — the blip should be absorbed into a single utterance.
    assert_eq!(counts.decodes_flushed, 1);
}

#[test]
fn phantom_open_below_min_open_ms_discarded() {
    let mut machine = default_machine();
    let mut counts = AutoBreakFlushCounts::default();

    machine.on_squelch_opened();
    machine.on_samples(&samples_for_ms(50)); // < MIN_OPEN_MS (100)
    machine.on_squelch_closed();
    if let Some(decision) = machine.on_tail_timeout() {
        counts.record(decision);
    }

    assert_eq!(counts.decodes_flushed, 0);
    assert_eq!(counts.discarded_phantom, 1);
}

#[test]
fn sub_min_segment_discarded() {
    let mut machine = default_machine();
    let mut counts = AutoBreakFlushCounts::default();

    machine.on_squelch_opened();
    machine.on_samples(&samples_for_ms(300)); // > MIN_OPEN (100) but < MIN_SEGMENT (400)
    machine.on_squelch_closed();
    if let Some(decision) = machine.on_tail_timeout() {
        counts.record(decision);
    }

    assert_eq!(counts.decodes_flushed, 0);
    assert_eq!(counts.discarded_short, 1);
}

#[test]
fn max_segment_safety_cap_triggers_flush() {
    let mut machine = default_machine();

    machine.on_squelch_opened();
    machine.on_samples(&samples_for_ms(31_000)); // > MAX_SEGMENT_MS (30_000)

    // At this point the machine should have buffer_duration_ms >= MAX,
    // so the external loop (or a check inside the state machine) should
    // treat it as a force-flush condition. We verify via the public API:
    assert!(machine.buffer_duration_ms() >= AUTO_BREAK_MAX_SEGMENT_MS);
    // The state machine itself doesn't flush on sample receipt — the
    // session loop driver checks the buffer duration after each
    // `on_samples` call and force-flushes. Test the take_buffer path.
    let buf = machine.take_buffer();
    assert!(
        !buf.is_empty(),
        "take_buffer should return the captured samples"
    );
}

#[test]
fn tail_extension_does_not_inflate_discard_decision() {
    // Regression: a 300 ms transmission + 200 ms of tail-capture
    // samples (simulating the ~AUTO_BREAK_TAIL_MS of audio the
    // session loop buffers between SquelchClosed and the tail
    // timer expiration) pushes the raw buffer duration to 500 ms,
    // which would cross the 400 ms MIN_SEGMENT threshold. The
    // decision MUST still be DiscardShort because the actual
    // transmission was only 300 ms.
    let mut machine = default_machine();
    let mut counts = AutoBreakFlushCounts::default();

    machine.on_squelch_opened();
    machine.on_samples(&samples_for_ms(300)); // 300 ms of open transmission
    machine.on_squelch_closed(); // Snapshot should fire here
    machine.on_samples(&samples_for_ms(200)); // 200 ms tail during HoldingOff

    // Raw buffer is now 500 ms, BUT transmission_duration_ms
    // should be the pre-close snapshot of 300 ms.
    assert_eq!(
        machine.buffer_duration_ms(),
        500,
        "raw buffer includes the tail-capture audio"
    );
    assert_eq!(
        machine.transmission_duration_ms(),
        300,
        "transmission duration reflects only the pre-close samples"
    );

    if let Some(decision) = machine.on_tail_timeout() {
        counts.record(decision);
    }

    assert_eq!(
        counts.decodes_flushed, 0,
        "sub-min transmission must NOT be decoded even though tail-extended buffer crossed the threshold"
    );
    assert_eq!(counts.discarded_short, 1);
}

#[test]
fn phantom_open_with_tail_does_not_cross_min_open_threshold() {
    // Matching regression for the phantom-open lower bound. A 50 ms
    // open + 200 ms tail = 250 ms raw buffer, which would cross
    // MIN_OPEN_MS (100). The decision MUST still be DiscardPhantom.
    let mut machine = default_machine();
    let mut counts = AutoBreakFlushCounts::default();

    machine.on_squelch_opened();
    machine.on_samples(&samples_for_ms(50)); // 50 ms open (phantom)
    machine.on_squelch_closed();
    machine.on_samples(&samples_for_ms(200)); // 200 ms tail

    assert_eq!(machine.transmission_duration_ms(), 50);
    assert!(machine.buffer_duration_ms() >= TEST_MIN_OPEN_MS);

    if let Some(decision) = machine.on_tail_timeout() {
        counts.record(decision);
    }

    assert_eq!(counts.decodes_flushed, 0);
    assert_eq!(
        counts.discarded_phantom, 1,
        "phantom open must still be discarded even with tail-inflated buffer"
    );
}

#[test]
#[allow(clippy::panic)]
fn emit_stop_notification_drops_queued_requests_and_emits_text() {
    // Mock queue with three pending segments. emit_stop_notification
    // should drain all three and emit exactly one Text event whose
    // body names the discard count.
    let (decode_tx, decode_rx) = mpsc::channel::<DecodeRequest>();
    let (event_tx, event_rx) = mpsc::channel::<TranscriptionEvent>();

    for _ in 0..3 {
        decode_tx
            .send(DecodeRequest {
                mono: vec![0.0_f32; 1600],
            })
            .expect("send to live channel");
    }
    drop(decode_tx); // Simulate I/O thread exiting.

    emit_stop_notification(&decode_rx, &event_tx);

    // decode_rx should be empty.
    assert!(decode_rx.try_recv().is_err());

    // event_rx should have exactly one Text event mentioning the count.
    match event_rx.try_recv() {
        Ok(TranscriptionEvent::Text { text, timestamp }) => {
            assert!(
                text.contains("3 pending"),
                "stop notification text should mention the discard count, got: {text}"
            );
            assert_eq!(timestamp.len(), 8, "timestamp should be HH:MM:SS");
        }
        other => panic!("expected a Text stop notification, got: {other:?}"),
    }
    assert!(
        event_rx.try_recv().is_err(),
        "only one event should be emitted"
    );
}

#[test]
#[allow(clippy::panic)]
fn emit_stop_notification_empty_queue_still_emits_single_text() {
    // Zero pending segments — we still emit a stop marker so the
    // transcript shows when the user stopped, just without a count.
    let (_decode_tx, decode_rx) = mpsc::channel::<DecodeRequest>();
    let (event_tx, event_rx) = mpsc::channel::<TranscriptionEvent>();

    emit_stop_notification(&decode_rx, &event_tx);

    match event_rx.try_recv() {
        Ok(TranscriptionEvent::Text { text, .. }) => {
            assert_eq!(text, "[transcription stopped]");
        }
        other => panic!("expected a Text stop notification, got: {other:?}"),
    }
}

/// Spawn `session_io_loop_auto_break` with default thresholds on its
/// own thread; returns the audio input, the decode-request output, the
/// event receiver (keep it alive — the loop exits when it drops) and
/// the join handle.
fn spawn_auto_break_loop(
    cancel: &Arc<AtomicBool>,
) -> (
    mpsc::Sender<TranscriptionInput>,
    mpsc::Receiver<DecodeRequest>,
    mpsc::Receiver<TranscriptionEvent>,
    std::thread::JoinHandle<()>,
) {
    let (audio_tx, audio_rx) = mpsc::channel::<TranscriptionInput>();
    let (decode_tx, decode_rx) = mpsc::channel::<DecodeRequest>();
    let (event_tx, event_rx) = mpsc::channel::<TranscriptionEvent>();
    let cancel = Arc::clone(cancel);
    let handle = std::thread::spawn(move || {
        session_io_loop_auto_break(SessionIoAutoBreakParams {
            cancel,
            audio_rx,
            event_tx,
            decode_tx,
            noise_gate_ratio: 1.0,
            auto_break_thresholds: super::super::host::AutoBreakThresholds::defaults(),
            audio_enhancement: denoise::AudioEnhancement::default(),
        });
    });
    (audio_tx, decode_rx, event_rx, handle)
}

/// One squelch-open → `ms` of samples → squelch-close transmission.
fn send_burst(audio_tx: &mpsc::Sender<TranscriptionInput>, ms: u32) {
    for input in [
        TranscriptionInput::SquelchOpened,
        TranscriptionInput::Samples(samples_for_ms(ms)),
        TranscriptionInput::SquelchClosed,
    ] {
        audio_tx.send(input).expect("I/O thread alive");
    }
}

#[test]
fn session_io_loop_auto_break_forwards_back_to_back_segments() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (audio_tx, decode_rx, _event_rx, handle) = spawn_auto_break_loop(&cancel);

    // First transmission, then wait past the tail deadline so the
    // segment flushes before the second one starts.
    send_burst(&audio_tx, 1_000);
    std::thread::sleep(std::time::Duration::from_millis(
        u64::from(crate::backend::AUTO_BREAK_TAIL_MS_DEFAULT) * 2 + 100,
    ));
    send_burst(&audio_tx, 700);

    drop(audio_tx);

    // Wait for the I/O thread to finish so both tail timeouts have
    // fired and both DecodeRequests are queued on decode_rx.
    handle.join().expect("I/O thread should join cleanly");

    // Now drain decode_rx: both requests should be there, in order.
    let req1 = decode_rx
        .try_recv()
        .expect("first DecodeRequest should be queued");
    let req2 = decode_rx
        .try_recv()
        .expect("second DecodeRequest should be queued even though we never read req1");
    assert!(
        !req1.mono.is_empty(),
        "first segment's mono buffer must not be empty"
    );
    assert!(
        !req2.mono.is_empty(),
        "second segment's mono buffer must not be empty"
    );
    assert!(
        decode_rx.try_recv().is_err(),
        "no extra phantom requests expected"
    );
}

#[test]
fn max_segment_safety_flush_resumes_recording_not_idle() {
    // Regression: after the 30 s safety cap fires on a carrier that
    // stays open, the state machine MUST resume in Recording (so
    // subsequent samples continue to buffer and the next cap splits
    // the transmission) rather than Idle (which would silently drop
    // all samples until the next close→open edge that never comes).
    let mut machine = default_machine();
    machine.on_squelch_opened();
    machine.on_samples(&samples_for_ms(31_000)); // trigger safety cap

    // Simulate the session loop's force-flush path.
    let _ = machine.take_buffer();
    machine.reset_after_force_flush(AutoBreakState::Recording);

    assert_eq!(
        machine.state(),
        AutoBreakState::Recording,
        "safety cap must resume in Recording to split a long transmission"
    );
    assert_eq!(
        machine.buffer_duration_ms(),
        0,
        "buffer must be empty after reset"
    );

    // And subsequent samples should still be captured (proving the
    // resume state is effective, not just nominal).
    machine.on_samples(&samples_for_ms(1_000));
    assert_eq!(machine.buffer_duration_ms(), 1_000);
}

#[test]
fn max_segment_cap_during_holdoff_keeps_pending_close_edge() {
    // CodeRabbit round 1 on PR #891: a transmission that closes just
    // under the cap keeps receiving gap-free samples during the tail
    // window; when the cap trips in `HoldingOff`, the forced flush
    // must NOT discard the already-observed close edge (resume in
    // `Recording` + cleared deadline left the machine buffering dead
    // air and dispatching a silence segment every cap interval).
    let mut machine = default_machine();
    let (decode_tx, decode_rx) = mpsc::channel();
    let mut deadline = Some(std::time::Instant::now() + std::time::Duration::from_millis(500));

    machine.on_squelch_opened();
    machine.on_samples(&samples_for_ms(AUTO_BREAK_MAX_SEGMENT_MS - 200));
    machine.on_squelch_closed();

    let flow = handle_samples_arm(
        &mut machine,
        &samples_for_ms(400),
        1.0,
        denoise::AudioEnhancement::Off,
        &decode_tx,
        &mut deadline,
    );
    assert!(matches!(flow, std::ops::ControlFlow::Continue(())));
    // The oversized buffer was force-flushed to the decoder…
    assert!(decode_rx.try_recv().is_ok(), "cap flush must dispatch");
    // …but the close edge survives: still HoldingOff with the tail
    // deadline intact, so `on_tail_timeout` finalizes the
    // transmission instead of the machine sticking in `Recording`.
    assert!(
        matches!(machine.state(), AutoBreakState::HoldingOff),
        "cap during HoldingOff must resume in HoldingOff"
    );
    assert!(
        deadline.is_some(),
        "tail deadline must survive a cap flush in HoldingOff"
    );
}
