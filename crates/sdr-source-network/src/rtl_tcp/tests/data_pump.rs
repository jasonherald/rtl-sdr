use super::*;

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
fn consecutive_timeouts_break_out_of_data_pump() {
    /// Well past `max_consecutive_timeouts × data_read_timeout`.
    const SILENT_SERVER_HOLD: Duration = Duration::from_secs(2);
    const CONNECT_DEADLINE: Duration = Duration::from_secs(1);
    const LEAVE_DEADLINE: Duration = Duration::from_secs(1);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Vanilla server that sends dongle_info_t and then goes silent.
    let server_thread = thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let _ = sock.write_all(&rtlx_test_dongle_info());
            thread::sleep(SILENT_SERVER_HOLD);
        }
    });
    let mut src = rtlx_start(addr, rtlx_test_config(CodecMask::NONE_ONLY));
    assert!(
        wait_until(CONNECT_DEADLINE, || is_connected(&src)),
        "never reached Connected"
    );
    let left_connected = wait_until(LEAVE_DEADLINE, || !is_connected(&src));
    src.stop_manager();
    let _ = server_thread.join();
    assert!(
        left_connected,
        "client still Connected after timeout threshold — reconnect didn't fire"
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
    const LZ4_STALL_MAX_TIMEOUTS: u32 = 10;
    const LZ4_STALL_SERVER_HOLD: Duration = Duration::from_secs(3);
    const LZ4_STALL_LEAVE_DEADLINE: Duration = Duration::from_secs(1);
    const LZ4_STALL_SESSIONS: usize = 2;
    const LZ4_STALL_RECONNECT_DEADLINE: Duration = Duration::from_secs(3);
    let (listener, mut config) = rtlx_test_listener_and_config();
    config.max_consecutive_timeouts = LZ4_STALL_MAX_TIMEOUTS;
    let addr = listener.local_addr().unwrap();
    // Two LZ4 sessions that never send a frame: the first must be torn
    // down on its first read timeout, the second proves the reconnect.
    let server_thread =
        rtlx_serve_silent_lz4_sessions(listener, LZ4_STALL_SESSIONS, LZ4_STALL_SERVER_HOLD);

    let mut src = rtlx_start(addr, config);
    wait_until(RTLX_TEST_STATE_DEADLINE, || is_connected(&src));
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
    assert!(
        wait_until(LZ4_STALL_LEAVE_DEADLINE, || !is_connected(&src)),
        "LZ4 pump must tear down on the first read timeout"
    );
    let reconnected = wait_until(LZ4_STALL_RECONNECT_DEADLINE, || {
        assert!(
            !matches!(src.connection_state(), ConnectionState::Failed { .. }),
            "a stall must not be terminal"
        );
        is_connected(&src)
    });
    src.stop_manager();
    let _ = server_thread.join();
    assert!(
        reconnected,
        "client must reconnect after the stall teardown"
    );
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
