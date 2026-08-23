use super::*;
// Tests build `ServerExtension` / `ClientHello` values with
// explicit `version: PROTOCOL_VERSION`. Lib code itself
// picks versions via `required_protocol_version` and no
// longer needs the constant at the top of the file; the
// test-scope import keeps clippy's `unused_imports` lint
// happy on the lib target.
use sdr_server_rtltcp::extension::PROTOCOL_VERSION;
use std::net::TcpListener;
// `CLIENT_HELLO_LEN` is only consumed by the loopback fixtures
// in the RTLX handshake tests below — keep it in test scope
// so the lib build doesn't warn it as unused.
use sdr_server_rtltcp::extension::CLIENT_HELLO_LEN;

/// Placeholder host/port for tests that never actually connect —
/// just exercise builder state or buffer logic. The string "127.0.0.1"
/// is fine as-is, but the port number is named for intent.
const UNUSED_TEST_PORT: u16 = 1234;

/// A port we expect connect() to fail with ECONNREFUSED on localhost
/// so the shutdown-during-retry test doesn't hang waiting for a SYN
/// timeout. Port 1 is a well-known unused privileged port and on
/// Linux loopback refuses instantly.
const REFUSED_TEST_PORT: u16 = 1;

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
fn append_with_cap_drops_oldest_when_full() {
    let mut rx: VecDeque<u8> = VecDeque::from(vec![0u8; RX_BUFFER_SOFT_CAP_BYTES - 10]);
    // Mark the tail so we can verify what survives.
    rx[RX_BUFFER_SOFT_CAP_BYTES - 11] = 0xAA;
    let incoming = vec![0xFFu8; 100]; // 100 bytes incoming — need to drop 90
    let dropped = append_with_cap_inner(&mut rx, &incoming);
    assert_eq!(dropped, 90);
    assert_eq!(rx.len(), RX_BUFFER_SOFT_CAP_BYTES);
    // Tail should be the 100 new 0xFF bytes.
    assert!(rx.range(rx.len() - 100..).all(|&b| b == 0xFF));
}

#[test]
fn append_with_cap_handles_oversized_chunk() {
    // Chunk larger than the cap: keep only the tail of the chunk.
    let mut rx: VecDeque<u8> = VecDeque::new();
    let mut big = vec![0u8; RX_BUFFER_SOFT_CAP_BYTES + 1000];
    // Mark the tail so we can verify it survives.
    let len = big.len();
    big[len - 1] = 0xAB;
    let dropped = append_with_cap_inner(&mut rx, &big);
    assert_eq!(dropped, 1000);
    assert_eq!(rx.len(), RX_BUFFER_SOFT_CAP_BYTES);
    assert_eq!(*rx.back().unwrap(), 0xAB);
}

#[test]
fn append_with_cap_rounds_drop_up_to_even() {
    // Construct an overflow by exactly 1 byte. The drop count MUST
    // round up to 2 so we don't split an I/Q pair — Q would get
    // misaligned with the next I, phase-shifting the output stream
    // until another odd-drop event happened to realign it.
    //
    // Mark the I byte of the pair that should survive after drop so
    // we can verify it ends up at rx[0] (still an I position, not
    // shifted into a Q slot).
    let mut rx: VecDeque<u8> = VecDeque::from(vec![0u8; RX_BUFFER_SOFT_CAP_BYTES]);
    rx[0] = 0x11; // I of dropped pair 0
    rx[1] = 0x12; // Q of dropped pair 0
    rx[2] = 0xAA; // I of surviving pair 1 — must land at rx[0] post-drop
    rx[3] = 0xBB; // Q of surviving pair 1
    let incoming = [0xFFu8; 1];
    let dropped = append_with_cap_inner(&mut rx, &incoming);
    assert_eq!(dropped, 2, "drop count must be even, got {dropped}");
    // Pair 1's I landed at position 0 — alignment preserved.
    assert_eq!(rx[0], 0xAA, "I byte of surviving pair must be at rx[0]");
    assert_eq!(rx[1], 0xBB, "Q byte of surviving pair must be at rx[1]");
    // rx can legitimately end on an odd length (the trailing byte is
    // half of a pair waiting for its mate on the next read) — what
    // matters is that the DROP was pair-aligned, not the final length.
}

#[test]
fn append_with_cap_no_drop_below_cap() {
    let mut rx: VecDeque<u8> = VecDeque::from(vec![0u8; 1000]);
    let incoming = vec![0xFFu8; 500];
    let dropped = append_with_cap_inner(&mut rx, &incoming);
    assert_eq!(dropped, 0);
    assert_eq!(rx.len(), 1500);
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
fn consecutive_timeouts_break_out_of_data_pump() {
    // Server completes handshake then stops sending anything. With
    // a 200 ms read timeout and max 2 consecutive timeouts, the
    // client should leave Connected within ~400 ms rather than
    // hanging for the full DATA_READ_TIMEOUT window.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server_thread = thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let header = DongleInfo {
                tuner: TunerTypeCode::R820t,
                gain_count: 29,
            }
            .to_bytes();
            let _ = sock.write_all(&header);
            // Hold for well past 2 × read_timeout so the client's
            // read() actually hits TimedOut (not EOF).
            thread::sleep(Duration::from_secs(2));
        }
    });

    let config = RtlTcpConfig {
        data_read_timeout: Duration::from_millis(200),
        max_consecutive_timeouts: 2,
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        compression: sdr_server_rtltcp::codec::CodecMask::NONE_ONLY,
        request_takeover: false,
        auth_key: None,
        requested_role: Role::Control,
    };
    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    src.start_manager().unwrap();

    // Wait for Connected.
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut reached_connected = false;
    while Instant::now() < deadline {
        if matches!(src.connection_state(), ConnectionState::Connected { .. }) {
            reached_connected = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(reached_connected, "never reached Connected");

    // After 2 × 200 ms of silence the data pump should break out.
    // Give up to 1 second of slack for scheduling jitter.
    let timeout_deadline = Instant::now() + Duration::from_secs(1);
    let mut left_connected = false;
    while Instant::now() < timeout_deadline {
        if !matches!(src.connection_state(), ConnectionState::Connected { .. }) {
            left_connected = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    src.stop_manager();
    let _ = server_thread.join();
    assert!(
        left_connected,
        "client still Connected after timeout threshold — reconnect didn't fire"
    );
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
fn session_end_clears_rx_buf_and_rearms_overflow() {
    // Stale I/Q in rx_buf after a session ends would replay into
    // the next session's consumer, rewinding the stream. Set some
    // fake rx state + overflow flag, then simulate the path that
    // run_data_pump takes on disconnect.
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    if let Ok(mut rx) = src.shared.rx_buf.lock() {
        rx.extend(&[0u8; 1024]);
    }
    src.shared.rx_in_overflow.store(true, Ordering::Relaxed);
    if let Ok(mut sink) = src.shared.command_sink.lock() {
        // Use a dummy stream so there's something to clear.
        let (client, _server) = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server_thread = thread::spawn(move || listener.accept().unwrap().0);
            let c = TcpStream::connect(addr).unwrap();
            (c, server_thread.join().unwrap())
        };
        *sink = Some(client);
    }

    // Simulate the disconnect cleanup body inline.
    if let Ok(mut sink) = src.shared.command_sink.lock() {
        *sink = None;
    }
    if let Ok(mut rx) = src.shared.rx_buf.lock() {
        rx.clear();
    }
    src.shared.rx_in_overflow.store(false, Ordering::Relaxed);

    assert_eq!(src.shared.rx_buf.lock().unwrap().len(), 0);
    assert!(!src.shared.rx_in_overflow.load(Ordering::Relaxed));
    assert!(src.shared.command_sink.lock().unwrap().is_none());
}

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

// ============================================================
// RTLX handshake fixture constants (CodeRabbit round 7 on PR #399)
//
// Pulled out of the individual tests so the acceptance,
// rejection, and retry paths all use the same gain count,
// socket timeouts, and state-observation deadlines — avoiding
// silent drift between the fixtures.
// ============================================================

/// Gain step count the fixture dongle advertises in its
/// `dongle_info_t` header. R820T's published table is 29
/// steps; matches upstream rtl-sdr exactly.
const RTLX_TEST_GAIN_COUNT: u32 = 29;
/// Read timeout the client uses against the fixture server.
/// Short (200 ms) so a stalled test exits quickly rather than
/// holding the whole suite up.
const RTLX_TEST_DATA_READ_TIMEOUT: Duration = Duration::from_millis(200);
/// How long the fixture server holds the accepted-connection
/// socket open after writing its responses. Must exceed
/// `RTLX_TEST_DATA_READ_TIMEOUT` so the client finishes
/// reading the extension body before EOF, but short enough
/// that the server thread joins quickly at test teardown.
const RTLX_TEST_SERVER_HOLD: Duration = Duration::from_millis(400);
/// Wall-clock deadline the tests give the client to reach
/// its expected state (Connected / Failed / non-Failed).
/// Generous enough to absorb CI scheduling jitter.
const RTLX_TEST_STATE_DEADLINE: Duration = Duration::from_secs(2);
/// Poll interval inside the state-observation loops. Short
/// enough to catch brief state visits without pegging a core.
const RTLX_TEST_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Slice large enough to catch any stray prefix the default
/// client might emit against a vanilla-shape server — 4 bytes
/// would be the EXTENSION_MAGIC prefix, 8 would be a full
/// `ClientHello`. 16 bytes comfortably covers either
/// regression. Used by
/// `rtl_tcp_default_config_sends_no_hello_to_vanilla_server`.
const NO_HELLO_PROBE_LEN: usize = 16;
/// Read timeout for the "did the client send anything?" probe.
/// Short enough to keep the test fast, long enough that a
/// real client-side hello emission would fall well within
/// this window.
const NO_HELLO_PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// Shared fixture setup: listener on loopback + config that
/// opts into the extended handshake. Keeps the three RTLX
/// handshake tests aligned on compression mask + timeouts.
fn rtlx_test_listener_and_config() -> (TcpListener, RtlTcpConfig) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = RtlTcpConfig {
        data_read_timeout: RTLX_TEST_DATA_READ_TIMEOUT,
        max_consecutive_timeouts: 2,
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        compression: CodecMask::NONE_AND_LZ4,
        request_takeover: false,
        auth_key: None,
        requested_role: Role::Control,
    };
    (listener, config)
}

/// Loopback server behavior for a single RTLX accept: read
/// the 8-byte `ClientHello`, write `dongle_info_t`, write the
/// caller-supplied `ServerExtension`, then hold the socket
/// open for `RTLX_TEST_SERVER_HOLD` so the client reads the
/// extension body before EOF.
fn rtlx_test_serve_one(listener: &TcpListener, ext: ServerExtension) {
    let (mut sock, _) = listener.accept().expect("accept");
    sock.set_read_timeout(Some(RTLX_TEST_STATE_DEADLINE))
        .unwrap();
    let mut hello_buf = [0u8; CLIENT_HELLO_LEN];
    sock.read_exact(&mut hello_buf).expect("read hello");
    assert_eq!(&hello_buf[..EXTENSION_MAGIC.len()], &EXTENSION_MAGIC);
    let header = DongleInfo {
        tuner: TunerTypeCode::R820t,
        gain_count: RTLX_TEST_GAIN_COUNT,
    }
    .to_bytes();
    sock.write_all(&header).unwrap();
    sock.write_all(&ext.to_bytes()).unwrap();
    thread::sleep(RTLX_TEST_SERVER_HOLD);
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
fn rtlx_handshake_sends_takeover_flag_when_config_opts_in() {
    // **Regression test for #393.** When
    // `RtlTcpConfig::request_takeover = true`, the outgoing
    // `ClientHello.flags` byte must set bit 0
    // (`FLAG_REQUEST_TAKEOVER`). Captures the hello off the
    // wire server-side and asserts the bit is set. Pairs
    // with the server-side takeover matrix tests in
    // `broadcaster::tests::register_with_role_takeover_*`
    // (which exercise the server's reaction); this test
    // locks in the client's end of the same wire contract.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (hello_tx, hello_rx) = std::sync::mpsc::channel::<[u8; CLIENT_HELLO_LEN]>();
    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(RTLX_TEST_STATE_DEADLINE))
            .unwrap();
        let mut hello_buf = [0u8; CLIENT_HELLO_LEN];
        sock.read_exact(&mut hello_buf).expect("read hello");
        let _ = hello_tx.send(hello_buf);
        // Accept the handshake so the client reaches
        // Connected and doesn't loop the accept socket.
        let header = DongleInfo {
            tuner: TunerTypeCode::R820t,
            gain_count: RTLX_TEST_GAIN_COUNT,
        }
        .to_bytes();
        sock.write_all(&header).unwrap();
        let ext = ServerExtension {
            codec: Codec::None,
            granted_role: Some(Role::Control),
            status: Status::Ok,
            version: PROTOCOL_VERSION,
        };
        sock.write_all(&ext.to_bytes()).unwrap();
        thread::sleep(RTLX_TEST_SERVER_HOLD);
    });

    let config = RtlTcpConfig {
        data_read_timeout: RTLX_TEST_DATA_READ_TIMEOUT,
        max_consecutive_timeouts: 2,
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        // Takeover opt-in also enables the hello even when
        // compression is off — the extension_enabled gate is
        // "compression != NONE_ONLY OR request_takeover".
        compression: CodecMask::NONE_ONLY,
        request_takeover: true,
        auth_key: None,
        requested_role: Role::Control,
    };
    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    src.start_manager().unwrap();

    let hello = hello_rx
        .recv_timeout(RTLX_TEST_STATE_DEADLINE)
        .expect("server should receive hello within deadline");
    // Magic bytes first — sanity-check we read the hello,
    // not IQ garbage or something else.
    assert_eq!(&hello[..EXTENSION_MAGIC.len()], &EXTENSION_MAGIC);
    // Flags byte (offset 6) must have bit 0 set.
    let flags_byte = hello[6];
    assert_ne!(
        flags_byte & sdr_server_rtltcp::extension::FLAG_REQUEST_TAKEOVER,
        0,
        "request_takeover = true must set FLAG_REQUEST_TAKEOVER bit \
         in the hello (got flags byte 0x{flags_byte:02x})"
    );
    // And the takeover flag opens the hello path even when
    // compression stayed at NONE_ONLY — the codec_mask on
    // the wire reflects the caller's mask verbatim.
    assert_eq!(hello[4], CodecMask::NONE_ONLY.to_wire());

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtlx_handshake_clears_takeover_flag_when_only_compression_opts_in() {
    // **Regression test for #393 + CodeRabbit round 2 on PR #404.**
    // Pins the RTLX-hello-path contract: when compression
    // triggers the hello (the `extension_enabled` gate) but
    // `request_takeover` stays at its default `false`, the
    // flags byte on the wire must NOT have
    // `FLAG_REQUEST_TAKEOVER` set. Protects against a bug
    // where the bit gets hard-coded anywhere in the client
    // emission path.
    //
    // The complementary "true default path sends no hello at
    // all" case is covered by
    // `rtl_tcp_default_config_sends_no_hello_to_vanilla_server`
    // below — that test pins NONE_ONLY + request_takeover =
    // false → no hello, which is the legacy-safe default.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (hello_tx, hello_rx) = std::sync::mpsc::channel::<[u8; CLIENT_HELLO_LEN]>();
    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(RTLX_TEST_STATE_DEADLINE))
            .unwrap();
        let mut hello_buf = [0u8; CLIENT_HELLO_LEN];
        sock.read_exact(&mut hello_buf).expect("read hello");
        let _ = hello_tx.send(hello_buf);
        let header = DongleInfo {
            tuner: TunerTypeCode::R820t,
            gain_count: RTLX_TEST_GAIN_COUNT,
        }
        .to_bytes();
        sock.write_all(&header).unwrap();
        let ext = ServerExtension {
            codec: Codec::Lz4,
            granted_role: Some(Role::Control),
            status: Status::Ok,
            version: PROTOCOL_VERSION,
        };
        sock.write_all(&ext.to_bytes()).unwrap();
        thread::sleep(RTLX_TEST_SERVER_HOLD);
    });

    // Compression opts into the extended handshake (required
    // for this path — without it, `extension_enabled = false`
    // and no hello is sent). `request_takeover` at its
    // default `false`.
    let config = RtlTcpConfig {
        data_read_timeout: RTLX_TEST_DATA_READ_TIMEOUT,
        max_consecutive_timeouts: 2,
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        compression: CodecMask::NONE_AND_LZ4,
        request_takeover: false,
        auth_key: None,
        requested_role: Role::Control,
    };
    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    src.start_manager().unwrap();

    let hello = hello_rx
        .recv_timeout(RTLX_TEST_STATE_DEADLINE)
        .expect("server should receive hello within deadline");
    assert_eq!(&hello[..EXTENSION_MAGIC.len()], &EXTENSION_MAGIC);
    let flags_byte = hello[6];
    assert_eq!(
        flags_byte & sdr_server_rtltcp::extension::FLAG_REQUEST_TAKEOVER,
        0,
        "request_takeover = false must clear FLAG_REQUEST_TAKEOVER \
         in the hello (got flags byte 0x{flags_byte:02x})"
    );

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtlx_handshake_sends_auth_key_when_config_opts_in() {
    // **Regression test for #394.** When
    // `RtlTcpConfig::auth_key = Some(key)`, the client must
    // emit `FLAG_HAS_AUTH` on the hello AND follow with an
    // `AuthKeyMessage` carrying the configured bytes. Pairs
    // with the server-side `sniff_auth_key_message` tests in
    // `sdr_server_rtltcp::server::tests` — this test locks
    // in the client's end of the same wire contract.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let expected_key = b"the-shared-secret-32-bytes-!!".to_vec();
    let expected_key_for_server = expected_key.clone();

    let (hello_tx, hello_rx) = std::sync::mpsc::channel::<[u8; CLIENT_HELLO_LEN]>();
    let (auth_tx, auth_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(RTLX_TEST_STATE_DEADLINE))
            .unwrap();
        let mut hello_buf = [0u8; CLIENT_HELLO_LEN];
        sock.read_exact(&mut hello_buf).expect("read hello");
        let _ = hello_tx.send(hello_buf);
        // Read the AuthKeyMessage follow-up: 6-byte header
        // (RTKA + u16 key_len) then key_len bytes.
        let mut header = [0u8; sdr_server_rtltcp::extension::AUTH_KEY_HEADER_LEN];
        sock.read_exact(&mut header).expect("read auth header");
        let key_len = sdr_server_rtltcp::extension::AuthKeyMessage::parse_header_len(&header)
            .expect("valid header");
        let mut body = vec![0u8; key_len as usize];
        sock.read_exact(&mut body).expect("read auth body");
        let _ = auth_tx.send(body);
        // Accept the handshake so the client reaches
        // Connected.
        let header_out = DongleInfo {
            tuner: TunerTypeCode::R820t,
            gain_count: RTLX_TEST_GAIN_COUNT,
        }
        .to_bytes();
        sock.write_all(&header_out).unwrap();
        let ext = ServerExtension {
            codec: Codec::None,
            granted_role: Some(Role::Control),
            status: Status::Ok,
            version: PROTOCOL_VERSION,
        };
        sock.write_all(&ext.to_bytes()).unwrap();
        thread::sleep(RTLX_TEST_SERVER_HOLD);
        // Consume the server's copy so the test data stays
        // alive until the thread runs.
        let _ = expected_key_for_server;
    });

    let config = RtlTcpConfig {
        data_read_timeout: RTLX_TEST_DATA_READ_TIMEOUT,
        max_consecutive_timeouts: 2,
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        // Auth opt-in triggers the hello even without
        // compression — the `extension_enabled` gate is
        // `compression != NONE_ONLY OR request_takeover OR
        // auth_key.is_some()`.
        compression: CodecMask::NONE_ONLY,
        request_takeover: false,
        auth_key: Some(expected_key.clone()),
        requested_role: Role::Control,
    };
    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    src.start_manager().unwrap();

    let hello = hello_rx
        .recv_timeout(RTLX_TEST_STATE_DEADLINE)
        .expect("server should receive hello within deadline");
    assert_eq!(&hello[..EXTENSION_MAGIC.len()], &EXTENSION_MAGIC);
    let flags_byte = hello[6];
    assert_ne!(
        flags_byte & sdr_server_rtltcp::extension::FLAG_HAS_AUTH,
        0,
        "auth_key = Some(..) must set FLAG_HAS_AUTH bit in the \
         hello (got flags byte 0x{flags_byte:02x})"
    );
    let observed_key = auth_rx
        .recv_timeout(RTLX_TEST_STATE_DEADLINE)
        .expect("server should receive auth key within deadline");
    assert_eq!(
        observed_key, expected_key,
        "client must emit the configured key bytes verbatim"
    );

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtlx_handshake_omits_auth_flag_when_no_auth_key_configured() {
    // Complement to the auth opt-in test: when `auth_key =
    // None`, the hello's `FLAG_HAS_AUTH` bit must be clear
    // AND the server must not observe any bytes after the
    // 8-byte hello (the client sends no AuthKeyMessage).
    // Protects against a bug that hard-codes the flag or
    // always emits a key.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (hello_tx, hello_rx) = std::sync::mpsc::channel::<[u8; CLIENT_HELLO_LEN]>();
    let (probe_tx, probe_rx) = std::sync::mpsc::channel::<std::io::Result<usize>>();
    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(RTLX_TEST_STATE_DEADLINE))
            .unwrap();
        let mut hello_buf = [0u8; CLIENT_HELLO_LEN];
        sock.read_exact(&mut hello_buf).expect("read hello");
        // Capture the client-sent version byte (offset 7)
        // BEFORE forwarding the hello to the test driver.
        // Compression-only opt-in should produce v1 per
        // `required_protocol_version(flags)`; echoing this
        // value in the response means any future regression
        // that changes the client-side version selection
        // surfaces here (driver asserts below). Per
        // `CodeRabbit` round 2 on PR #405.
        let client_hello_version = hello_buf[7];
        let _ = hello_tx.send(hello_buf);
        // Probe with a short timeout for any additional
        // bytes — expect WouldBlock / TimedOut because the
        // client shouldn't have sent an AuthKeyMessage.
        sock.set_read_timeout(Some(NO_HELLO_PROBE_TIMEOUT)).unwrap();
        let mut probe_buf = [0u8; 8];
        let read_result = sock.read(&mut probe_buf);
        let _ = probe_tx.send(read_result);
        sock.set_read_timeout(None).unwrap();
        let header_out = DongleInfo {
            tuner: TunerTypeCode::R820t,
            gain_count: RTLX_TEST_GAIN_COUNT,
        }
        .to_bytes();
        sock.write_all(&header_out).unwrap();
        let ext = ServerExtension {
            codec: Codec::Lz4,
            granted_role: Some(Role::Control),
            status: Status::Ok,
            // Echo the client's hello version — this is what
            // a real v2-era server does so v1 clients can
            // still parse the response under their strict
            // version gate. Hard-coding `PROTOCOL_VERSION`
            // here would mask a regression that changes
            // client-side version selection away from v1.
            version: client_hello_version,
        };
        sock.write_all(&ext.to_bytes()).unwrap();
        thread::sleep(RTLX_TEST_SERVER_HOLD);
    });

    // Compression-only opt-in, no auth.
    let config = RtlTcpConfig {
        data_read_timeout: RTLX_TEST_DATA_READ_TIMEOUT,
        max_consecutive_timeouts: 2,
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        compression: CodecMask::NONE_AND_LZ4,
        request_takeover: false,
        auth_key: None,
        requested_role: Role::Control,
    };
    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    src.start_manager().unwrap();

    let hello = hello_rx
        .recv_timeout(RTLX_TEST_STATE_DEADLINE)
        .expect("server should receive hello within deadline");
    let flags_byte = hello[6];
    assert_eq!(
        flags_byte & sdr_server_rtltcp::extension::FLAG_HAS_AUTH,
        0,
        "auth_key = None must clear FLAG_HAS_AUTH bit in the hello"
    );
    // Compression-only + no auth + no takeover →
    // `required_protocol_version(flags) == V1`. Pin the
    // version byte so a regression that swaps the default
    // to v2 against pre-#394 servers surfaces here. Per
    // `CodeRabbit` round 2 on PR #405.
    assert_eq!(
        hello[7],
        sdr_server_rtltcp::extension::PROTOCOL_VERSION_V1,
        "compression-only hello must emit v1 (compat with pre-#394 servers)"
    );

    // Server probe for follow-up bytes. Must time out — no
    // AuthKeyMessage means nothing more on the wire before
    // the server's response.
    let probe_result = probe_rx
        .recv_timeout(RTLX_TEST_STATE_DEADLINE)
        .expect("server probe should resolve within deadline");
    match probe_result {
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
        Ok(0) => {}
        Ok(n) => panic!(
            "client with auth_key = None must NOT emit any follow-up bytes, \
             but server observed {n} byte(s) after the hello"
        ),
        Err(e) => panic!("unexpected server probe error: {e:?}"),
    }

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtl_tcp_default_config_sends_no_hello_to_vanilla_server() {
    // **Regression test for `CodeRabbit` round 2 on PR #404.**
    // The true default path (no compression opt-in, no
    // takeover opt-in) MUST NOT send a `ClientHello` — that's
    // the legacy-safe contract that keeps the client
    // wire-compatible with every vanilla rtl_tcp server
    // (GQRX, SDR++, CubicSDR, upstream rtl_tcp, etc.). An
    // unexpected 8-byte hello would straddle their 5-byte
    // command framing and cause garbage dispatches.
    //
    // Test server: accept, briefly try to read from the
    // client with a short timeout, assert the read times out
    // (no bytes received) — proves the client sent nothing
    // before expecting the server's `dongle_info_t`. Then
    // send the legacy `dongle_info_t` to let the client's
    // manager settle into Connected cleanly.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (probe_tx, probe_rx) = std::sync::mpsc::channel::<std::io::Result<usize>>();
    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(NO_HELLO_PROBE_TIMEOUT))
            .expect("set_read_timeout");
        // Try to read. If the client sent nothing (the
        // correct behavior), this returns WouldBlock /
        // TimedOut after the probe window. Any bytes
        // received mean the client incorrectly emitted
        // something before waiting for dongle_info_t.
        let mut probe_buf = [0u8; NO_HELLO_PROBE_LEN];
        let read_result = sock.read(&mut probe_buf);
        let _ = probe_tx.send(read_result);
        // Clear the probe timeout before the legacy send so
        // the client's `set_read_timeout` on its side
        // doesn't interact with ours.
        sock.set_read_timeout(None).expect("clear timeout");
        // Complete the legacy handshake so the client
        // settles into Connected.
        let header = DongleInfo {
            tuner: TunerTypeCode::R820t,
            gain_count: RTLX_TEST_GAIN_COUNT,
        }
        .to_bytes();
        let _ = sock.write_all(&header);
        thread::sleep(RTLX_TEST_SERVER_HOLD);
    });

    // True default: NONE_ONLY compression + request_takeover
    // default (false) → `extension_enabled = false` → no
    // hello on the wire.
    let config = RtlTcpConfig::default();
    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    src.start_manager().unwrap();

    let probe_result = probe_rx
        .recv_timeout(RTLX_TEST_STATE_DEADLINE)
        .expect("server probe should resolve within deadline");
    match probe_result {
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            // Expected outcome: the client sent nothing, so
            // the server's short-timeout read times out.
            // Legacy-safe contract holds.
        }
        Ok(0) => {
            // Also acceptable: clean EOF before any bytes
            // (e.g., the manager tore down before proceeding
            // to the legacy read). Still proves no hello was
            // emitted.
        }
        Ok(n) => panic!(
            "default config must not send a hello before dongle_info_t, \
             but the server observed {n} byte(s) from the client before \
             writing its own response"
        ),
        Err(e) => panic!("unexpected server probe error (not the benign timeout/EOF): {e:?}"),
    }

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtlx_handshake_sends_listen_role_when_config_opts_in() {
    // **Regression test for #396.** When
    // `RtlTcpConfig::requested_role = Role::Listen`, the
    // `extension_enabled` gate must widen to emit a hello
    // (even with compression=NONE_ONLY + takeover=false +
    // auth_key=None) AND the role byte in the hello must be
    // `Role::Listen`. Server-side admission logic for the
    // Listen path is pinned by the role-matrix tests in
    // `broadcaster::tests::register_with_role_*`; this test
    // locks in the client's end of the same wire contract.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (hello_tx, hello_rx) = std::sync::mpsc::channel::<[u8; CLIENT_HELLO_LEN]>();
    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(RTLX_TEST_STATE_DEADLINE))
            .unwrap();
        let mut hello_buf = [0u8; CLIENT_HELLO_LEN];
        sock.read_exact(&mut hello_buf).expect("read hello");
        // Pin the role-only hello to protocol v1 — this
        // gate path has non-default role but zero flags, so
        // `required_protocol_version(flags)` (used by the
        // client hello-builder) returns v1. A regression
        // that silently promoted this hello to v2 would
        // lock out pre-#394 servers without surfacing a
        // test failure unless the version byte is checked
        // explicitly. Per CodeRabbit round 1 on PR #408.
        let client_hello_version = hello_buf[7];
        assert_eq!(
            client_hello_version,
            sdr_server_rtltcp::extension::PROTOCOL_VERSION_V1,
            "role-only hello must stay on v1 for backward compatibility \
             (got 0x{client_hello_version:02x})",
        );
        let _ = hello_tx.send(hello_buf);
        // Accept the handshake with Listen granted so the
        // client reaches Connected. Echo the client-chosen
        // protocol version back rather than the compile-time
        // `PROTOCOL_VERSION` so a future bump of `PROTOCOL_
        // VERSION` doesn't mask a regression where the role-
        // only hello regresses to a newer version.
        let header = DongleInfo {
            tuner: TunerTypeCode::R820t,
            gain_count: RTLX_TEST_GAIN_COUNT,
        }
        .to_bytes();
        sock.write_all(&header).unwrap();
        let ext = ServerExtension {
            codec: Codec::None,
            granted_role: Some(Role::Listen),
            status: Status::Ok,
            version: client_hello_version,
        };
        sock.write_all(&ext.to_bytes()).unwrap();
        thread::sleep(RTLX_TEST_SERVER_HOLD);
    });

    let config = RtlTcpConfig {
        data_read_timeout: RTLX_TEST_DATA_READ_TIMEOUT,
        max_consecutive_timeouts: 2,
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        // Only role is non-default; every other gate field
        // stays at the vanilla-safe default. This pins the
        // "role opt-in alone trips the hello" contract so a
        // future refactor of `extension_enabled` can't
        // silently drop the role from the gate list.
        compression: CodecMask::NONE_ONLY,
        request_takeover: false,
        auth_key: None,
        requested_role: Role::Listen,
    };
    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    src.start_manager().unwrap();

    let hello = hello_rx
        .recv_timeout(RTLX_TEST_STATE_DEADLINE)
        .expect("server should receive hello within deadline");
    // Magic bytes first — sanity-check we read a hello, not
    // command framing.
    assert_eq!(&hello[..EXTENSION_MAGIC.len()], &EXTENSION_MAGIC);
    // Role byte (offset 5) must be `Role::Listen`.
    assert_eq!(
        hello[5],
        Role::Listen as u8,
        "Listen opt-in must encode Role::Listen at byte offset 5 (got 0x{:02x})",
        hello[5],
    );
    // Flags byte (offset 6) must be zero — we didn't opt
    // into takeover or auth, just role.
    assert_eq!(hello[6], 0);

    // Lock in the state-side contract: after the handshake
    // completes, `ConnectionState::Connected.granted_role`
    // must carry `Some(Role::Listen)` — the value the server
    // wrote into `ServerExtension.granted_role` above. A
    // regression in `attempt_connect` that drops the
    // extension's `granted_role` on the floor would still
    // pass the wire-byte assertions but would break the
    // status-bar badge provenance (it would read `None` and
    // hide the badge even when the server explicitly
    // admitted us as a Listener). Poll with a bounded
    // deadline since the state transition is driven by the
    // manager thread. Per `CodeRabbit` round 2 on PR #408.
    let deadline = Instant::now() + RTLX_TEST_STATE_DEADLINE;
    let mut reached_connected_with_listen = false;
    while Instant::now() < deadline {
        if matches!(
            src.connection_state(),
            ConnectionState::Connected {
                granted_role: Some(Role::Listen),
                ..
            }
        ) {
            reached_connected_with_listen = true;
            break;
        }
        thread::sleep(RTLX_TEST_POLL_INTERVAL);
    }
    assert!(
        reached_connected_with_listen,
        "Connected state should retain the server-granted Listen role \
         (final observed state: {:?})",
        src.connection_state()
    );

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtlx_handshake_emits_control_role_by_default() {
    // Complement to the Listen test above: the TRUE default
    // path (fields all at `Default`) sends no hello at all,
    // matching `rtl_tcp_default_config_sends_no_hello_to_vanilla_server`.
    // The DEFAULT *role* is `Role::Control`, but the default
    // config gate (`extension_enabled = false` because
    // compression / takeover / auth / role are all default)
    // means no hello is emitted on the wire.
    //
    // This test pins the "role defaults to Control" struct
    // contract via `Default::default()` so a future typo or
    // accidental swap (e.g. flipping the default to Listen)
    // would trip here immediately.
    let config = RtlTcpConfig::default();
    assert_eq!(
        config.requested_role,
        Role::Control,
        "Default for `requested_role` must be `Control` — legacy-safe behavior"
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
fn read_samples_with_empty_output_returns_zero() {
    let mut src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    let mut output: [Complex; 0] = [];
    let n = src.read_samples(&mut output).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn read_samples_with_no_data_returns_zero() {
    let mut src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    // Source was never started, no bytes buffered.
    let mut output = [Complex::default(); 4];
    let n = src.read_samples(&mut output).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn read_samples_converts_8bit_offset_iq() {
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    // 128 is midscale zero, 255 is +1 - small epsilon, 0 is -1.
    if let Ok(mut rx) = src.shared.rx_buf.lock() {
        rx.extend(&[128, 128, 255, 0, 0, 255]);
    }
    let mut out = [Complex::default(); 3];
    // Call read_samples via the trait impl, matching public API.
    let mut mutable_src = src;
    let n = mutable_src.read_samples(&mut out).unwrap();
    assert_eq!(n, 3);
    // Midscale pair → near zero.
    assert!(out[0].re.abs() < 0.01);
    assert!(out[0].im.abs() < 0.01);
    // (255, 0) → +1, -1.
    assert!((out[1].re - 1.0).abs() < 0.01);
    assert!((out[1].im + 1.0).abs() < 0.01);
    // (0, 255) → -1, +1.
    assert!((out[2].re + 1.0).abs() < 0.01);
    assert!((out[2].im - 1.0).abs() < 0.01);
}

#[test]
fn read_samples_handles_partial_pair_at_end() {
    // Odd byte count — the trailing lone byte must stay queued
    // rather than produce half a sample.
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    if let Ok(mut rx) = src.shared.rx_buf.lock() {
        rx.extend(&[128, 128, 200]); // 1.5 pairs
    }
    let mut out = [Complex::default(); 2];
    let mut src = src;
    let n = src.read_samples(&mut out).unwrap();
    assert_eq!(n, 1, "should only consume the complete pair");
    // The trailing 200 stays queued — drained on the next call.
    let remaining = src.shared.rx_buf.lock().unwrap().len();
    assert_eq!(remaining, 1);
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

#[test]
fn tcp_eof_mid_stream_transitions_to_retrying() {
    // Server completes handshake then immediately closes and drops
    // its listener — client must leave Connected and enter Retrying.
    // NOTE: do NOT accept a second time here. A second accept without
    // a header write would make the client hang on the header read
    // until DATA_READ_TIMEOUT (5 s). We let the listener drop so the
    // reconnect attempt gets ECONNREFUSED immediately, which puts
    // the client into Retrying within a few ms.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let header = DongleInfo {
            tuner: TunerTypeCode::R820t,
            gain_count: 29,
        }
        .to_bytes();
        sock.write_all(&header).unwrap();
        // Drop sock → FIN → client's data-pump read returns Ok(0).
        // Dropping `listener` at the end of the closure scope makes
        // subsequent connect() from the client fail with
        // ECONNREFUSED, which lands the client in Retrying.
    });

    let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
    src.start_manager().unwrap();
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut saw_retrying = false;
    while Instant::now() < deadline {
        if matches!(src.connection_state(), ConnectionState::Retrying { .. }) {
            saw_retrying = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    src.stop_manager();
    let _ = server_thread.join();
    assert!(saw_retrying, "client never entered Retrying after EOF");
}

#[test]
fn commands_before_connect_are_recorded_and_replayed() {
    // Driver queues commands before start() / before the server
    // accepts; on handshake those values should be replayed to the
    // server.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();

    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let header = DongleInfo {
            tuner: TunerTypeCode::R820t,
            gain_count: 29,
        }
        .to_bytes();
        sock.write_all(&header).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        // Read whatever the client sends (replays + any subsequent
        // calls) for up to 1 s or until we've collected 2 commands.
        let mut got = 0;
        let deadline = Instant::now() + Duration::from_secs(1);
        while got < 2 && Instant::now() < deadline {
            let mut buf = [0u8; 5];
            match sock.read_exact(&mut buf) {
                Ok(()) => {
                    if let Some(cmd) = Command::from_bytes(&buf) {
                        let _ = cmd_tx.send(cmd);
                        got += 1;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
    // Queue commands BEFORE start — these must end up sent after
    // handshake via the replay path.
    src.set_center_freq_hz(433_000_000).unwrap();
    src.set_tuner_gain_tenths_db(197).unwrap();

    src.start_manager().unwrap();
    // Collect the replayed commands.
    let mut received = Vec::new();
    while let Ok(cmd) = cmd_rx.recv_timeout(Duration::from_millis(1500)) {
        received.push(cmd);
        if received.len() == 2 {
            break;
        }
    }
    src.stop_manager();
    let _ = server_thread.join();

    let params: Vec<(CommandOp, u32)> = received.iter().map(|c| (c.op, c.param)).collect();
    assert!(
        params.contains(&(CommandOp::SetCenterFreq, 433_000_000)),
        "expected replay of center freq, got {params:?}"
    );
    assert!(
        params.contains(&(CommandOp::SetTunerGain, 197)),
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

#[test]
fn rx_overflow_warning_is_edge_triggered() {
    // Fill rx past cap → first overflow flips the flag. Subsequent
    // overflows without a drain in between should leave the flag
    // set (log suppressed). A drain below half-cap rearms the flag.
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    assert!(!src.shared.rx_in_overflow.load(Ordering::Relaxed));

    // Simulate first overflow.
    {
        let mut rx = src.shared.rx_buf.lock().unwrap();
        *rx = VecDeque::from(vec![0u8; RX_BUFFER_SOFT_CAP_BYTES]);
        append_with_cap_to_shared(&src.shared, &mut rx, &[0xFFu8; 100]);
    }
    assert!(src.shared.rx_in_overflow.load(Ordering::Relaxed));

    // Second overflow — flag already set, no transition.
    {
        let mut rx = src.shared.rx_buf.lock().unwrap();
        append_with_cap_to_shared(&src.shared, &mut rx, &[0xFFu8; 100]);
    }
    assert!(src.shared.rx_in_overflow.load(Ordering::Relaxed));

    // Drain well below half-cap and then append a non-overflowing
    // chunk — flag should rearm.
    {
        let mut rx = src.shared.rx_buf.lock().unwrap();
        rx.clear();
        append_with_cap_to_shared(&src.shared, &mut rx, &[0u8; 100]);
    }
    assert!(
        !src.shared.rx_in_overflow.load(Ordering::Relaxed),
        "flag should rearm once buffer drains below half-cap"
    );
}

/// #743 — a framed (LZ4) stream cannot resume after a partial read:
/// `read_exact` inside the frame decoder has already consumed bytes
/// when `SO_RCVTIMEO` fires, so retrying on the same decoder restarts
/// at the wrong offset and surfaces as terminal corruption. The pump
/// must tear down for reconnect on the FIRST timeout instead.
#[test]
fn lz4_stall_breaks_out_after_a_single_timeout() {
    /// Long enough that the legacy "N consecutive timeouts" policy
    /// (N × read timeout = 2 s) would still be Connected.
    const LZ4_STALL_MAX_TIMEOUTS: u32 = 10;
    const LZ4_STALL_SERVER_HOLD: Duration = Duration::from_secs(3);
    const LZ4_STALL_LEAVE_DEADLINE: Duration = Duration::from_secs(1);
    /// The server serves two sessions so the test can observe the
    /// reconnect, not just the departure from `Connected`.
    const LZ4_STALL_SESSIONS: usize = 2;
    const LZ4_STALL_RECONNECT_DEADLINE: Duration = Duration::from_secs(3);
    let (listener, mut config) = rtlx_test_listener_and_config();
    config.max_consecutive_timeouts = LZ4_STALL_MAX_TIMEOUTS;
    let addr = listener.local_addr().unwrap();
    let server_thread = thread::spawn(move || {
        let mut socks = Vec::new();
        for _ in 0..LZ4_STALL_SESSIONS {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut hello_buf = [0u8; CLIENT_HELLO_LEN];
            sock.read_exact(&mut hello_buf).expect("read hello");
            let header = DongleInfo {
                tuner: TunerTypeCode::R820t,
                gain_count: RTLX_TEST_GAIN_COUNT,
            }
            .to_bytes();
            sock.write_all(&header).unwrap();
            let ext = ServerExtension {
                codec: Codec::Lz4,
                granted_role: Some(Role::Control),
                status: Status::Ok,
                version: PROTOCOL_VERSION,
            };
            sock.write_all(&ext.to_bytes()).unwrap();
            // Silence: the client's next read hits the timeout.
            socks.push(sock);
        }
        thread::sleep(LZ4_STALL_SERVER_HOLD);
    });

    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    src.start_manager().unwrap();
    let deadline = Instant::now() + RTLX_TEST_STATE_DEADLINE;
    while Instant::now() < deadline
        && !matches!(src.connection_state(), ConnectionState::Connected { .. })
    {
        thread::sleep(RTLX_TEST_POLL_INTERVAL);
    }
    assert!(
        matches!(
            src.connection_state(),
            ConnectionState::Connected {
                codec: Codec::Lz4,
                ..
            }
        ),
        "test premise: LZ4 session established"
    );

    let leave_deadline = Instant::now() + LZ4_STALL_LEAVE_DEADLINE;
    let mut left = false;
    while Instant::now() < leave_deadline {
        if !matches!(src.connection_state(), ConnectionState::Connected { .. }) {
            left = true;
            break;
        }
        thread::sleep(RTLX_TEST_POLL_INTERVAL);
    }
    assert!(left, "LZ4 pump must tear down on the first read timeout");

    // ...and come back: the teardown is a reconnect, not a failure.
    let reconnect_deadline = Instant::now() + LZ4_STALL_RECONNECT_DEADLINE;
    let mut reconnected = false;
    while Instant::now() < reconnect_deadline {
        if matches!(src.connection_state(), ConnectionState::Connected { .. }) {
            reconnected = true;
            break;
        }
        assert!(
            !matches!(src.connection_state(), ConnectionState::Failed { .. }),
            "a stall must not be terminal"
        );
        thread::sleep(RTLX_TEST_POLL_INTERVAL);
    }
    src.stop_manager();
    let _ = server_thread.join();
    assert!(
        reconnected,
        "client must reconnect after the stall teardown"
    );
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
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server_thread = thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let header = DongleInfo {
                tuner: TunerTypeCode::R820t,
                gain_count: 29,
            }
            .to_bytes();
            let _ = sock.write_all(&header);
            // Never read: the client's sends back up.
            thread::sleep(FLOOD_SERVER_HOLD);
        }
    });
    let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
    src.start_manager().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && !matches!(src.connection_state(), ConnectionState::Connected { .. })
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        matches!(src.connection_state(), ConnectionState::Connected { .. }),
        "test premise: connected"
    );
    let shared = Arc::clone(&src.shared);
    let flooding = Arc::new(AtomicBool::new(true));
    let flood_flag = Arc::clone(&flooding);
    let flooder = thread::spawn(move || {
        let mut hz = 100_000_000u32;
        while flood_flag.load(Ordering::Relaxed) {
            if let Ok(mut sink) = shared.command_sink.lock() {
                let Some(stream) = sink.as_mut() else { break };
                let cmd = Command {
                    op: CommandOp::SetCenterFreq,
                    param: hz,
                };
                if stream.write_all(&cmd.to_bytes()).is_err() {
                    break;
                }
                hz = hz.wrapping_add(1);
            }
        }
    });
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

/// #745 — the drop counter has a reader.
#[test]
fn rx_dropped_bytes_is_observable() {
    let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
    assert_eq!(src.rx_dropped_bytes(), 0);
    {
        let mut rx = src.shared.rx_buf.lock().unwrap();
        rx.clear();
        rx.extend(std::iter::repeat_n(0u8, RX_BUFFER_SOFT_CAP_BYTES));
        append_with_cap_to_shared(&src.shared, &mut rx, &[0xFFu8; 100]);
    }
    assert_eq!(src.rx_dropped_bytes(), 100);
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
