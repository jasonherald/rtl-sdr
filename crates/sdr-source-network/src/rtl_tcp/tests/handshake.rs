use super::*;

#[test]
fn new_source_starts_disconnected() {
    let source = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    match source.connection_state() {
        ConnectionState::Disconnected => {}
        other => unreachable!("expected Disconnected, got {other:?}"),
    }
    assert!(source.tuner_info().is_none());
}

#[test]
fn bad_magic_produces_failed_state() {
    // Spin up a toy server that writes junk then closes.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server_thread = thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let _ = s.write_all(b"XXXXjunknoise");
            // Keep open briefly so client reads fail cleanly.
            thread::sleep(Duration::from_millis(200));
        }
    });

    let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
    src.start_manager().unwrap();

    // Wait up to 2s for the manager to transition to Failed.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_failed = false;
    while Instant::now() < deadline {
        if matches!(src.connection_state(), ConnectionState::Failed { .. }) {
            saw_failed = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    src.stop_manager();
    let _ = server_thread.join();
    assert!(saw_failed, "expected Failed state after bad magic");
}

#[test]
fn happy_path_handshake_and_command_roundtrip() {
    // Mock rtl_tcp server: writes a valid RTL0 header then pushes
    // a fixed byte pattern as "samples" while reading tuning
    // commands into a channel we can inspect.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();

    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        // Advertise an R820T with 29 gains.
        let header = DongleInfo {
            tuner: TunerTypeCode::R820t,
            gain_count: 29,
        }
        .to_bytes();
        sock.write_all(&header).unwrap();
        // Stream a few hundred bytes of synthetic I/Q (all 128 = zero).
        sock.write_all(&[128u8; 512]).unwrap();
        // Read one command (5 bytes) from the client and forward it.
        let mut cmd_buf = [0u8; 5];
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        if sock.read_exact(&mut cmd_buf).is_ok() {
            if let Some(cmd) = Command::from_bytes(&cmd_buf) {
                let _ = cmd_tx.send(cmd);
            }
        }
        // Hold connection briefly so the client doesn't see EOF mid-test.
        thread::sleep(Duration::from_millis(200));
    });

    let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
    src.start_manager().unwrap();

    // Wait for Connected state.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut tuner = None;
    while Instant::now() < deadline {
        if let ConnectionState::Connected { tuner: t, .. } = src.connection_state() {
            tuner = Some(t);
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(tuner.is_some(), "client never reached Connected state");
    let t = tuner.unwrap();
    assert_eq!(t.tuner, TunerTypeCode::R820t);
    assert_eq!(t.gain_count, 29);

    // Send a tune command and verify the server received it.
    src.set_center_freq_hz(99_500_000).unwrap();
    let received = cmd_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(received.op, CommandOp::SetCenterFreq);
    assert_eq!(received.param, 99_500_000);

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtlx_handshake_accepted_publishes_codec_and_tuner() {
    // Regression test for CodeRabbit round 5 on PR #399.
    // A server that accepts the extended handshake
    // (`ServerExtension { codec: Lz4, status: Ok }`) must result
    // in the client reaching `Connected { codec: Lz4, .. }` with
    // `tuner_info()` populated. This locks in two ordering rules
    // that earlier rounds introduced:
    //
    //   - The negotiated codec is part of the `Connected` state
    //     (surfaces as the status-row suffix in the UI).
    //   - `shared.tuner` is published atomically with the
    //     `Connected` transition, not earlier during
    //     `dongle_info_t` parsing.
    let (listener, config) = rtlx_test_listener_and_config();
    let addr = listener.local_addr().unwrap();

    let server_thread = thread::spawn(move || {
        let ext = ServerExtension {
            codec: Codec::Lz4,
            granted_role: Some(Role::Control),
            status: Status::Ok,
            version: PROTOCOL_VERSION,
        };
        rtlx_test_serve_one(&listener, ext);
    });

    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    // Before starting the manager, tuner_info must be None —
    // guards against a false positive where the delayed-publish
    // ordering is broken but the test happens to read after an
    // earlier session populated the cache.
    assert!(src.tuner_info().is_none());

    src.start_manager().unwrap();

    let deadline = Instant::now() + RTLX_TEST_STATE_DEADLINE;
    let mut connected_codec: Option<Codec> = None;
    while Instant::now() < deadline {
        if let ConnectionState::Connected { codec, .. } = src.connection_state() {
            connected_codec = Some(codec);
            break;
        }
        thread::sleep(RTLX_TEST_POLL_INTERVAL);
    }
    assert_eq!(connected_codec, Some(Codec::Lz4));
    // `shared.tuner` is published together with the Connected
    // state, so once we observe Connected, `tuner_info()` must
    // return the R820T metadata from the dongle_info_t header.
    let ti = src.tuner_info();
    assert!(ti.is_some(), "tuner_info should be Some after Connected");
    let ti = ti.unwrap();
    assert_eq!(ti.tuner, TunerTypeCode::R820t);
    assert_eq!(ti.gain_count, RTLX_TEST_GAIN_COUNT);

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtlx_handshake_auth_failed_is_terminal_and_leaves_tuner_none() {
    // Regression test for CodeRabbit round 5 + round 7 on
    // PR #399. Originally this test used `ControllerBusy`,
    // but round 7 correctly pointed out that a busy server is
    // transient — the other controller will eventually
    // disconnect and we should retry. `AuthFailed` is the
    // right terminal substitute: a wrong key will not start
    // working on retry, so the manager must transition to a
    // terminal state and stop looping.
    //
    // **Updated for #396:** `AuthFailed` now routes to the
    // dedicated `ConnectionState::AuthFailed` variant (not
    // generic `Failed`) so the UI can surface a specific
    // "Key rejected" toast + re-prompt path instead of an
    // opaque error string. The terminal-ness contract is
    // unchanged — the manager still stops the retry loop on
    // `AuthFailed`.
    //
    // The test also guards the delayed-tuner-publish ordering:
    // `tuner_info()` must stay `None` because the handshake
    // never reached the `set_state(Connected)` path where the
    // cache write lives.
    let (listener, config) = rtlx_test_listener_and_config();
    let addr = listener.local_addr().unwrap();

    let server_thread = thread::spawn(move || {
        let ext = ServerExtension {
            codec: Codec::None,
            granted_role: None,
            status: Status::AuthFailed,
            version: PROTOCOL_VERSION,
        };
        rtlx_test_serve_one(&listener, ext);
    });

    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    assert!(src.tuner_info().is_none());

    src.start_manager().unwrap();

    let deadline = Instant::now() + RTLX_TEST_STATE_DEADLINE;
    let mut reached_terminal = false;
    while Instant::now() < deadline {
        if matches!(src.connection_state(), ConnectionState::AuthFailed) {
            reached_terminal = true;
            break;
        }
        thread::sleep(RTLX_TEST_POLL_INTERVAL);
    }
    assert!(
        reached_terminal,
        "client should transition to AuthFailed on `AuthFailed` ServerExtension status (per #396)"
    );
    assert!(
        src.tuner_info().is_none(),
        "tuner_info must stay None when the extension handshake is rejected"
    );

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtlx_handshake_controller_busy_routes_to_dedicated_state() {
    // Regression test for #396. `ControllerBusy` means
    // another client currently owns the control slot (#392).
    // Pre-#396 this routed to `TemporarilyUnavailable` and
    // the connection manager auto-retried silently via
    // backoff. Per #396 the connection manager now routes
    // it to the dedicated `ConnectionState::ControllerBusy`
    // variant and STOPS the retry loop — the UI needs to
    // surface the busy state to the user so they can pick
    // Take-control or Connect-as-Listener instead of
    // waiting for the other controller to drop.
    //
    // Historical rename: the CR-round-7-on-PR-#399 rule
    // ("ControllerBusy must not route to Failed") still
    // holds — `ControllerBusy` is its OWN terminal state,
    // not `Failed`. The UX contract changed; the naming
    // discipline for generic-error terminal = `Failed` did
    // not.
    //
    // We assert:
    //   1. State reaches `ConnectionState::ControllerBusy`
    //      within the observation window (the new #396
    //      routing).
    //   2. `tuner_info()` stays `None` — the handshake
    //      never reached `Connected`.
    let (listener, config) = rtlx_test_listener_and_config();
    let addr = listener.local_addr().unwrap();

    let server_thread = thread::spawn(move || {
        // Accept a single connection and reject it. Because
        // ControllerBusy is now terminal, the client will
        // NOT retry and we don't need a multi-accept loop
        // here.
        let ext = ServerExtension {
            codec: Codec::None,
            granted_role: None,
            status: Status::ControllerBusy,
            version: PROTOCOL_VERSION,
        };
        rtlx_test_serve_one(&listener, ext);
    });

    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    assert!(src.tuner_info().is_none());

    src.start_manager().unwrap();

    let deadline = Instant::now() + RTLX_TEST_STATE_DEADLINE;
    let mut reached_controller_busy = false;
    while Instant::now() < deadline {
        if matches!(src.connection_state(), ConnectionState::ControllerBusy) {
            reached_controller_busy = true;
            break;
        }
        // The `Failed` path would be a regression — Failed
        // is for generic protocol errors only, not role
        // denials.
        assert!(
            !matches!(src.connection_state(), ConnectionState::Failed { .. }),
            "ControllerBusy must route to its dedicated state, not generic Failed"
        );
        thread::sleep(RTLX_TEST_POLL_INTERVAL);
    }
    assert!(
        reached_controller_busy,
        "client should transition to ControllerBusy on `ControllerBusy` \
         ServerExtension status (per #396)"
    );
    assert!(
        src.tuner_info().is_none(),
        "tuner_info must stay None when the handshake is rejected"
    );

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtlx_handshake_auth_required_routes_to_dedicated_state() {
    // Regression test for #396. `AuthRequired` means the
    // server demands a pre-shared key (#394) and the client
    // didn't include one in the hello. Pre-#396 the
    // connection manager folded this into `Protocol` and
    // landed in `Failed` with a generic reason; per #396 it
    // now routes to the dedicated `ConnectionState::
    // AuthRequired` variant so the UI can reveal + focus
    // the Server key entry row instead of showing an opaque
    // error toast. Terminal — no auto-retry while the UI
    // waits for the user to enter a key.
    //
    // Complements the existing `AuthFailed` /
    // `ControllerBusy` regression tests, which pin the same
    // contract for the other two role-denial statuses. Per
    // `CodeRabbit` round 3 on PR #408.
    let (listener, config) = rtlx_test_listener_and_config();
    let addr = listener.local_addr().unwrap();

    let server_thread = thread::spawn(move || {
        let ext = ServerExtension {
            codec: Codec::None,
            granted_role: None,
            status: Status::AuthRequired,
            version: PROTOCOL_VERSION,
        };
        rtlx_test_serve_one(&listener, ext);
    });

    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    assert!(src.tuner_info().is_none());

    src.start_manager().unwrap();

    let deadline = Instant::now() + RTLX_TEST_STATE_DEADLINE;
    let mut reached_auth_required = false;
    while Instant::now() < deadline {
        if matches!(src.connection_state(), ConnectionState::AuthRequired) {
            reached_auth_required = true;
            break;
        }
        // `Failed` would be a regression back to the pre-#396
        // routing — the dedicated variant is the whole point.
        assert!(
            !matches!(src.connection_state(), ConnectionState::Failed { .. }),
            "AuthRequired must route to its dedicated state, not generic Failed"
        );
        thread::sleep(RTLX_TEST_POLL_INTERVAL);
    }
    assert!(
        reached_auth_required,
        "client should transition to AuthRequired on `AuthRequired` \
         ServerExtension status (per #396)"
    );
    assert!(
        src.tuner_info().is_none(),
        "tuner_info must stay None when the handshake is rejected"
    );

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn partial_header_read_still_completes_handshake() {
    // Server sends the 12-byte dongle_info_t in two chunks with a
    // sleep between, exercising the read_exact_with_context loop.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let header = DongleInfo {
            tuner: TunerTypeCode::E4000,
            gain_count: 14,
        }
        .to_bytes();
        sock.write_all(&header[..5]).unwrap();
        thread::sleep(Duration::from_millis(80));
        sock.write_all(&header[5..]).unwrap();
        // Hold open briefly.
        thread::sleep(Duration::from_millis(200));
    });

    let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
    src.start_manager().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut got = None;
    while Instant::now() < deadline {
        if let ConnectionState::Connected { tuner, .. } = src.connection_state() {
            got = Some(tuner);
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    src.stop_manager();
    let _ = server_thread.join();
    let tuner = got.expect("handshake should succeed across split reads");
    assert_eq!(tuner.tuner, TunerTypeCode::E4000);
    assert_eq!(tuner.gain_count, 14);
}
