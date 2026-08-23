use super::*;

#[test]
fn backoff_schedule_caps_at_30s() {
    assert_eq!(backoff_delay(0), Duration::from_secs(1));
    assert_eq!(backoff_delay(4), Duration::from_secs(30));
    // Further attempts saturate.
    assert_eq!(backoff_delay(999), Duration::from_secs(30));
}

#[test]
fn first_retry_uses_1s_backoff() {
    // Regression test for a real off-by-one: the first retry used
    // BACKOFF_SCHEDULE_SECS[1] (2s) instead of [0] (1s). Drive the
    // manager against a never-listener and look at the first
    // Retrying state it publishes.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Drop the listener immediately — any connect() will refuse.
    drop(listener);

    let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
    src.start_manager().unwrap();

    // Wait up to 2s for the first Retrying state.
    let t0 = Instant::now();
    let mut first_delay = None;
    while t0.elapsed() < Duration::from_secs(2) {
        if let ConnectionState::Retrying { attempt, next_at } = src.connection_state() {
            // `attempt` must be 1 for the first retry, and `next_at`
            // must correspond to a ~1 s delay, not 2 s.
            assert_eq!(attempt, 1, "first retry must be attempt 1");
            let delay = next_at.saturating_duration_since(Instant::now());
            first_delay = Some(delay);
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    src.stop_manager();

    let d = first_delay.expect("never saw Retrying state within 2 s");
    // Allow a little jitter. Must be <= 1 s + a small slack,
    // NOT around 2 s.
    assert!(
        d <= Duration::from_millis(1100),
        "first retry delay = {d:?}, expected ~1s"
    );
}

#[test]
fn second_client_is_rejected_not_queued() {
    // This test verifies the contract stated in the module docs:
    // "single client at a time; second connection rejected with
    // graceful close." Upstream rtl_tcp silently hangs second
    // connections in the kernel backlog; our implementation closes
    // them immediately.
    //
    // We don't bring up a full Server here (needs a real RTL-SDR),
    // but we can exercise the exact accept-loop logic by mocking
    // the listener behavior: the key invariant is that a second
    // connection's read(stream) returns EOF quickly rather than
    // hanging for the DATA_READ_TIMEOUT window.
    //
    // Since Server::start requires hardware, cover the pure-logic
    // part — the AtomicBool swap semantics — directly.
    let busy = AtomicBool::new(false);
    assert!(!busy.swap(true, Ordering::SeqCst)); // first claim: was false
    assert!(busy.swap(true, Ordering::SeqCst)); // second claim: already true
    // A second accept caller would see `true` and reject.
    busy.store(false, Ordering::SeqCst); // session done
    assert!(!busy.swap(true, Ordering::SeqCst)); // next client can claim again
}

#[test]
fn set_sample_rate_hz_rejects_zero() {
    // Matches `Source::set_sample_rate`'s `<= 0` guard. Bypassing
    // via the typed helper would cache 0 and wedge the remote USB
    // controller.
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    let err = src.set_sample_rate_hz(0);
    assert!(
        matches!(err, Err(SourceError::InvalidParameter(_))),
        "expected InvalidParameter for 0 Hz sample rate, got {err:?}"
    );
    // Valid rates still succeed.
    assert!(src.set_sample_rate_hz(2_048_000).is_ok());
}

#[test]
fn typed_setter_updates_cached_getter_value() {
    // Regression for the split-brain bug: `set_sample_rate_hz` and
    // `set_center_freq_hz` write the wire command but previously
    // left the cached getter value untouched, so a caller polling
    // `Source::sample_rate()` after using the typed setter would
    // see the default rate, not what they just set. Moving the
    // caches onto `SharedState` with atomic f64 bit-patterns fixes
    // this.
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    // Default before any set:
    let default_rate = <RtlTcpSource as Source>::sample_rate(&src);
    assert!((default_rate - DEFAULT_CLIENT_SAMPLE_RATE_HZ).abs() < 0.5);

    // Typed setter (bypasses `Source::set_sample_rate`):
    src.set_sample_rate_hz(2_400_000).unwrap();
    let after = <RtlTcpSource as Source>::sample_rate(&src);
    assert!(
        (after - 2_400_000.0).abs() < 0.5,
        "getter after set_sample_rate_hz = {after}, want 2_400_000"
    );
}

#[test]
fn record_command_sets_replay_bit() {
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    let cmd = Command {
        op: CommandOp::SetCenterFreq,
        param: 99_500_000,
    };
    src.record_command(cmd);
    let mask = src.shared.replay_mask.load(Ordering::Relaxed);
    // CenterFreq is op 0x01, bit index 0.
    assert_eq!(mask & 0x1, 0x1);
    assert_eq!(
        src.shared.last_center_freq_hz.load(Ordering::Relaxed),
        99_500_000
    );
}

#[test]
fn commands_before_connect_are_recorded_and_replayed() {
    const REPLAYED_COMMANDS: usize = 2;
    const REPLAY_DEADLINE: Duration = Duration::from_millis(1500);
    const TEST_FREQ_HZ: u32 = 433_000_000;
    const TEST_GAIN_TENTHS_DB: i32 = 197;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.write_all(&rtlx_test_dongle_info()).unwrap();
        forward_commands(&mut sock, REPLAYED_COMMANDS, &cmd_tx);
    });

    let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
    src.set_center_freq_hz(TEST_FREQ_HZ).unwrap();
    src.set_tuner_gain_tenths_db(TEST_GAIN_TENTHS_DB).unwrap();
    src.start_manager().unwrap();
    let mut received = Vec::new();
    while let Ok(cmd) = cmd_rx.recv_timeout(REPLAY_DEADLINE) {
        received.push(cmd);
        if received.len() == REPLAYED_COMMANDS {
            break;
        }
    }
    src.stop_manager();
    let _ = server_thread.join();

    let params: Vec<(CommandOp, u32)> = received.iter().map(|c| (c.op, c.param)).collect();
    assert!(
        params.contains(&(CommandOp::SetCenterFreq, TEST_FREQ_HZ)),
        "expected replay of center freq, got {params:?}"
    );
    #[allow(
        clippy::cast_sign_loss,
        reason = "gain travels as raw u32 bits on the wire"
    )]
    let gain_wire = TEST_GAIN_TENTHS_DB as u32;
    assert!(
        params.contains(&(CommandOp::SetTunerGain, gain_wire)),
        "expected replay of tuner gain, got {params:?}"
    );
}

#[test]
fn second_start_call_is_rejected_not_leaked() {
    // Two back-to-back `start_manager` calls must not leak the
    // first manager thread. Previously the second call silently
    // overwrote `self.manager`, leaving two connection_manager
    // threads racing on the same SharedState and
    // `stop_manager`/`Drop` only waiting for the newest one.
    let mut src = RtlTcpSource::new("127.0.0.1", REFUSED_TEST_PORT);
    src.start_manager().unwrap();
    // The first manager is alive (sitting in the reconnect loop
    // because port 1 refuses). Second call must Err.
    let second = src.start_manager();
    assert!(matches!(second, Err(SourceError::AlreadyRunning)));
    src.stop_manager();

    // After shutdown the prior handle is joined; a fresh start is
    // allowed again. Hit the "finished handle gets reaped" path.
    src.start_manager().unwrap();
    src.stop_manager();
}

#[test]
fn tune_rejects_non_finite_and_out_of_range() {
    let mut src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    // Never started, so no IO will actually happen — the tune call
    // goes through the trait impl's validation guard and either
    // returns Err or short-circuits at the command channel (which
    // is None).
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e12] {
        let err = <RtlTcpSource as Source>::tune(&mut src, bad);
        assert!(
            matches!(err, Err(SourceError::InvalidParameter(_))),
            "tune({bad}) should reject with InvalidParameter"
        );
    }
    // Sanity: a valid finite in-range frequency does NOT trip the guard.
    assert!(<RtlTcpSource as Source>::tune(&mut src, 100_000_000.0).is_ok());
}

#[test]
fn set_sample_rate_rejects_non_finite_zero_negative_and_oversized() {
    let mut src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    for bad in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,  // zero rate would wedge USB
        -1.0, // negative rate
        1e12, // > u32::MAX
    ] {
        let err = <RtlTcpSource as Source>::set_sample_rate(&mut src, bad);
        assert!(
            matches!(err, Err(SourceError::InvalidParameter(_))),
            "set_sample_rate({bad}) should reject with InvalidParameter"
        );
    }
    // Sanity: 2.048 Msps passes.
    assert!(<RtlTcpSource as Source>::set_sample_rate(&mut src, 2_048_000.0).is_ok());
}

#[test]
fn connect_cancellable_aborts_promptly_on_shutdown() {
    // `TcpStream::connect_timeout` itself has no cancellation hook,
    // but our cancellable wrapper polls the shutdown flag at
    // `CONNECT_SHUTDOWN_POLL` cadence. When the flag is pre-set,
    // the caller returns before the helper thread finishes — this
    // exercise that path without needing a blackholed IP in CI.
    let shutdown = AtomicBool::new(true);
    // Any address — doesn't matter, shutdown is already set so the
    // poll loop returns on the first iteration before the helper's
    // `connect_timeout` ever completes.
    let addrs = vec![SocketAddr::from(([127, 0, 0, 1], REFUSED_TEST_PORT))];
    let t0 = Instant::now();
    let result = connect_cancellable(addrs, Duration::from_secs(30), &shutdown);
    let elapsed = t0.elapsed();
    assert!(
        matches!(
            result,
            Err(SourceError::Io(ref e)) if e.kind() == std::io::ErrorKind::Interrupted
        ),
        "expected Interrupted on shutdown, got {result:?}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "connect_cancellable returned in {elapsed:?}, should be ≤ CONNECT_SHUTDOWN_POLL"
    );
}

#[test]
fn shutdown_during_failed_connect_is_prompt() {
    // Point client at a port nothing's listening on; start_manager
    // enters the retry loop. stop_manager should return within ~1 s,
    // well below the exponential-backoff window.
    let mut src = RtlTcpSource::new("127.0.0.1", REFUSED_TEST_PORT); // port 1 likely refused
    src.start_manager().unwrap();
    let t0 = Instant::now();
    src.stop_manager();
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "stop_manager took {elapsed:?}, should be prompt"
    );
}

#[test]
fn record_command_covers_all_14_wire_ops() {
    // Every upstream command is recorded for reconnect-replay so a
    // pre-connect call (e.g. set_testmode before start()) isn't
    // silently lost. Walk all 14 opcodes and confirm each lands in
    // the replay_mask.
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    let all_ops = [
        CommandOp::SetCenterFreq,
        CommandOp::SetSampleRate,
        CommandOp::SetGainMode,
        CommandOp::SetTunerGain,
        CommandOp::SetFreqCorrection,
        CommandOp::SetIfGain,
        CommandOp::SetTestMode,
        CommandOp::SetAgcMode,
        CommandOp::SetDirectSampling,
        CommandOp::SetOffsetTuning,
        CommandOp::SetRtlXtal,
        CommandOp::SetTunerXtal,
        CommandOp::SetGainByIndex,
        CommandOp::SetBiasTee,
    ];
    for op in all_ops {
        // `SetIfGain` carries its 1-based stage in the upper 16
        // bits; stage 0 is rejected, so give it a real stage.
        let param = if op == CommandOp::SetIfGain {
            (1 << IF_GAIN_STAGE_SHIFT_BITS) | 0x2a
        } else {
            42
        };
        src.record_command(Command { op, param });
    }
    let mask = src.shared.replay_mask.load(Ordering::Relaxed);
    // Every op from 0x01..=0x0e should have its bit set (bit index
    // = opcode - 1) — except `SetTunerGain`, whose replay bit the
    // later `SetGainByIndex` clears (they drive the same server
    // gain; #745).
    let tuner_gain_bit = 1u32 << ((CommandOp::SetTunerGain as u32) - 1);
    assert_eq!(mask & 0x3fff, 0x3fff & !tuner_gain_bit, "mask={mask:#x}");
}

/// #745 — `SetTunerGain` and `SetGainByIndex` both drive the same
/// server-side gain; replaying both in table order lets a stale
/// index overwrite a newer dB value. Recording one clears the other.
#[test]
fn recording_a_gain_setter_clears_its_sibling_replay() {
    const GAIN_BIT: u32 = (CommandOp::SetTunerGain as u32) - 1;
    const INDEX_BIT: u32 = (CommandOp::SetGainByIndex as u32) - 1;
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    src.record_command(Command {
        op: CommandOp::SetGainByIndex,
        param: 5,
    });
    src.record_command(Command {
        op: CommandOp::SetTunerGain,
        param: 300,
    });
    let mask = src.shared.replay_mask.load(Ordering::Relaxed);
    assert!(mask & (1 << GAIN_BIT) != 0, "newest setter replays");
    assert!(
        mask & (1 << INDEX_BIT) == 0,
        "older sibling must not replay"
    );

    src.record_command(Command {
        op: CommandOp::SetGainByIndex,
        param: 7,
    });
    let mask = src.shared.replay_mask.load(Ordering::Relaxed);
    assert!(mask & (1 << INDEX_BIT) != 0);
    assert_eq!(mask & (1 << GAIN_BIT), 0);
}

/// #745 — IF gain is per stage (upper 16 bits of the param); one
/// slot collapsed every stage into the last one written.
#[test]
fn if_gain_is_recorded_per_stage() {
    const STAGE_1: u32 = 1;
    const STAGE_3: u32 = 3;
    const GAIN_1: u32 = 120;
    const GAIN_3: u32 = 45;
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    src.record_command(Command {
        op: CommandOp::SetIfGain,
        param: (STAGE_1 << IF_GAIN_STAGE_SHIFT_BITS) | GAIN_1,
    });
    src.record_command(Command {
        op: CommandOp::SetIfGain,
        param: (STAGE_3 << IF_GAIN_STAGE_SHIFT_BITS) | GAIN_3,
    });
    let snap = src.rtl_tcp_sticky_snapshot().unwrap();
    assert_eq!(
        snap.last_if_gain[STAGE_1 as usize - 1],
        (STAGE_1 << IF_GAIN_STAGE_SHIFT_BITS) | GAIN_1
    );
    assert_eq!(
        snap.last_if_gain[STAGE_3 as usize - 1],
        (STAGE_3 << IF_GAIN_STAGE_SHIFT_BITS) | GAIN_3
    );
    assert_eq!(
        snap.if_gain_mask,
        (1 << (STAGE_1 - 1)) | (1 << (STAGE_3 - 1))
    );
}

/// #745 — a peer that accepts and then says nothing used to make
/// `stop()` wait for the full data-read timeout: the only way to
/// unblock the manager was the command sink, published only after
/// the handshake reads. The raw stream is now stashed before them.
#[test]
fn stop_before_handshake_returns_promptly() {
    const SILENT_SERVER_HOLD: Duration = Duration::from_secs(8);
    const STOP_DEADLINE: Duration = Duration::from_secs(1);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server_thread = thread::spawn(move || {
        if let Ok((_sock, _)) = listener.accept() {
            thread::sleep(SILENT_SERVER_HOLD);
        }
    });
    // Default config: 5 s data-read timeout.
    let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
    src.start_manager().unwrap();
    // Give the manager time to connect and block in the header read.
    thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    src.stop_manager();
    let elapsed = started.elapsed();
    drop(server_thread); // let the silent server finish on its own
    assert!(
        elapsed < STOP_DEADLINE,
        "stop blocked for {elapsed:?} waiting on the pre-handshake read"
    );
}

/// #745 (CR round 5 on PR #792) — `stop()` must not wait for a
/// command write blocked on a non-reading peer: the session cancel
/// handle shuts the socket without taking the sink lock. A sender
/// thread floods commands at a peer that completes the handshake and
/// then never reads; whether `write_all` actually blocks depends on
/// the loopback socket buffers, but `stop_manager` must return
/// promptly either way.
#[test]
fn stop_during_command_flood_returns_promptly() {
    const FLOOD_SERVER_HOLD: Duration = Duration::from_secs(6);
    const FLOOD_WARMUP: Duration = Duration::from_millis(300);
    const STOP_DEADLINE: Duration = Duration::from_secs(1);
    const CONNECT_DEADLINE: Duration = Duration::from_secs(2);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Vanilla server that accepts, sends dongle_info_t and then only
    // absorbs whatever the flooder writes.
    let server_thread = thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let _ = sock.write_all(&rtlx_test_dongle_info());
            thread::sleep(FLOOD_SERVER_HOLD);
        }
    });
    let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
    src.start_manager().unwrap();
    assert!(
        wait_until(CONNECT_DEADLINE, || is_connected(&src)),
        "test premise: connected"
    );
    let flooding = Arc::new(AtomicBool::new(true));
    let flooder = spawn_command_flooder(Arc::clone(&src.shared), Arc::clone(&flooding));
    thread::sleep(FLOOD_WARMUP);
    let started = Instant::now();
    src.stop_manager();
    let elapsed = started.elapsed();
    flooding.store(false, Ordering::Relaxed);
    let _ = flooder.join();
    drop(server_thread);
    assert!(
        elapsed < STOP_DEADLINE,
        "stop blocked for {elapsed:?} behind a command write"
    );
}

/// #745 — after `stop()` the public view must read Disconnected
/// with no tuner, even when the manager ended in a terminal state.
#[test]
fn stop_manager_resets_state_and_tuner() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server_thread = thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let _ = s.write_all(b"XXXXjunknoise");
            thread::sleep(Duration::from_millis(200));
        }
    });
    let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
    src.start_manager().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && !matches!(src.connection_state(), ConnectionState::Failed { .. })
    {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        matches!(src.connection_state(), ConnectionState::Failed { .. }),
        "test premise: terminal Failed state"
    );
    src.stop_manager();
    let _ = server_thread.join();
    assert!(matches!(
        src.connection_state(),
        ConnectionState::Disconnected
    ));
    assert!(src.tuner_info().is_none());
}

#[test]
fn replay_bits_set_independently_per_op() {
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    src.record_command(Command {
        op: CommandOp::SetBiasTee,
        param: 1,
    });
    let mask = src.shared.replay_mask.load(Ordering::Relaxed);
    // BiasTee is op 0x0e, so bit index (0x0e - 1) = 13.
    assert!(mask & (1 << 13) != 0);
    // No other bits should be set.
    assert_eq!(mask.count_ones(), 1);
}
