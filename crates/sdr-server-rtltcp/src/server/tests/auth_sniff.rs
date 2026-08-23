use super::*;

#[test]
fn sniff_auth_key_message_reads_and_parses_full_message() {
    // Happy path: client sends a valid AuthKeyMessage, server
    // reads + parses. Pins the two-phase read (header then
    // body) + the wire-format round-trip.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Exactly 32 bytes — matches the canonical
    // `DEFAULT_AUTH_KEY_LEN` used by `generate_random_auth_key`
    // so the fixture shape tracks production size. The ASCII
    // label makes test log output readable while keeping the
    // bytes well within the `1..=MAX_AUTH_KEY_LEN` window.
    let key_bytes = b"test-key-32-bytes-exactly-abcdef".to_vec();
    debug_assert_eq!(key_bytes.len(), 32);
    let msg = AuthKeyMessage {
        key: key_bytes.clone(),
    };
    let wire = msg.to_bytes().unwrap();
    let client_thread = thread::spawn(move || {
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(&wire).unwrap();
        // Hold the socket open past the server's read so the
        // server doesn't see EOF mid-body-read.
        thread::sleep(Duration::from_millis(50));
    });
    let (server_stream, _peer) = listener.accept().unwrap();
    let parsed = sniff_auth_key_message(&server_stream).unwrap();
    assert_eq!(parsed.key, key_bytes);
    drop(server_stream);
    let _ = client_thread.join();
}

#[test]
fn sniff_auth_key_message_rejects_bad_magic() {
    // Client sends a 6-byte header with non-RTKA magic. The
    // header parser catches the bad magic via
    // `parse_header_len → None` and the helper surfaces
    // InvalidData.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client_thread = thread::spawn(move || {
        let mut client = TcpStream::connect(addr).unwrap();
        let mut bad = vec![0x00, 0x01, 0x02, 0x03];
        bad.extend_from_slice(&1u16.to_be_bytes());
        bad.push(0xAA);
        client.write_all(&bad).unwrap();
        thread::sleep(Duration::from_millis(50));
    });
    let (server_stream, _peer) = listener.accept().unwrap();
    let err = sniff_auth_key_message(&server_stream).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    drop(server_stream);
    let _ = client_thread.join();
}

#[test]
fn sniff_auth_key_message_times_out_when_client_silent() {
    // Client connects but never sends. Header read blocks
    // up to AUTH_REPLY_TIMEOUT, then fails. Guards against a
    // silent-client DOS that would otherwise wedge the
    // accept thread.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (keep_tx, keep_rx) = std::sync::mpsc::channel::<()>();
    let client_thread = thread::spawn(move || {
        let _client = TcpStream::connect(addr).unwrap();
        // Hold the socket open, don't send anything. Release
        // after the server's timeout has fired.
        let _ = keep_rx.recv();
    });
    let (server_stream, _peer) = listener.accept().unwrap();
    let start = Instant::now();
    let err = sniff_auth_key_message(&server_stream).unwrap_err();
    let elapsed = start.elapsed();
    // Error kind is WouldBlock / TimedOut depending on
    // platform; either is an acceptable "header read timed
    // out" signal from the OS.
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "expected WouldBlock/TimedOut for silent client, got {err:?}"
    );
    // Tight bound: total elapsed must sit inside ONE
    // `AUTH_REPLY_TIMEOUT` plus scheduling slack, not the
    // earlier 2× allowance. The loose 2× bound would have
    // silently accepted a regression to per-phase timeouts
    // where the header + body phases each reset the budget.
    // Per `CodeRabbit` round 3 on PR #405.
    assert!(
        elapsed <= AUTH_REPLY_TIMEOUT + AUTH_TIMEOUT_SLACK,
        "silent-client read took {elapsed:?}, exceeded AUTH_REPLY_TIMEOUT ({AUTH_REPLY_TIMEOUT:?}) + slack ({AUTH_TIMEOUT_SLACK:?})"
    );
    let _ = keep_tx.send(());
    drop(server_stream);
    let _ = client_thread.join();
}

#[test]
fn sniff_auth_key_message_times_out_when_body_stalls() {
    // Regression guard for the absolute-deadline contract.
    // Client sends a valid header but never follows with the
    // body bytes — this is the path that the old per-phase
    // timeout silently accepted: the header read returns
    // quickly (burns ~0 of the budget), then the body read
    // gets a fresh `AUTH_REPLY_TIMEOUT` window of its own.
    // With the absolute-deadline implementation the body
    // read inherits the remaining budget and trips within
    // `AUTH_REPLY_TIMEOUT` of entry.
    //
    // Paired with the silent-client test: together they pin
    // the "one shared budget across both reads" contract and
    // defeat any future revert to per-phase timeouts.
    // Per `CodeRabbit` round 3 on PR #405.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Valid 6-byte header claiming a 32-byte key body. Server
    // will consume it fast, then block on the absent body.
    let key_len: u16 = 32;
    let mut header = [0u8; AUTH_KEY_HEADER_LEN];
    header[..4].copy_from_slice(&crate::extension::AUTH_KEY_MAGIC);
    header[4..6].copy_from_slice(&key_len.to_be_bytes());

    let (keep_tx, keep_rx) = std::sync::mpsc::channel::<()>();
    let client_thread = thread::spawn(move || {
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(&header).unwrap();
        client.flush().unwrap();
        // Hold the socket open WITHOUT sending the body.
        // Server should trip its absolute deadline and
        // return. Release after the server's timeout fires.
        let _ = keep_rx.recv();
    });

    let (server_stream, _peer) = listener.accept().unwrap();
    let start = Instant::now();
    let err = sniff_auth_key_message(&server_stream).unwrap_err();
    let elapsed = start.elapsed();
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "expected WouldBlock/TimedOut for stalled body, got {err:?}"
    );
    // The decisive assertion: elapsed must be within the
    // single-budget bound. A regression to per-phase
    // timeouts would push this toward 2× AUTH_REPLY_TIMEOUT.
    assert!(
        elapsed <= AUTH_REPLY_TIMEOUT + AUTH_TIMEOUT_SLACK,
        "stalled-body read took {elapsed:?}, exceeded AUTH_REPLY_TIMEOUT ({AUTH_REPLY_TIMEOUT:?}) + slack ({AUTH_TIMEOUT_SLACK:?}) — absolute-deadline contract regressed"
    );
    let _ = keep_tx.send(());
    drop(server_stream);
    let _ = client_thread.join();
}

#[test]
fn sniff_auth_key_message_handles_fragmented_send() {
    // Client sends the header in one write and the body in a
    // separate write (with a short pause). The server's
    // read_exact waits for the rest — no protocol desync.
    // Pins the contract that the helper tolerates realistic
    // TCP segmentation.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let key_bytes: Vec<u8> = (0..64).map(|i| i as u8).collect();
    let msg = AuthKeyMessage {
        key: key_bytes.clone(),
    };
    let wire = msg.to_bytes().unwrap();
    let (header_part, body_part) = wire.split_at(AUTH_KEY_HEADER_LEN);
    let header_vec = header_part.to_vec();
    let body_vec = body_part.to_vec();
    let client_thread = thread::spawn(move || {
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(&header_vec).unwrap();
        client.flush().unwrap();
        thread::sleep(Duration::from_millis(20));
        client.write_all(&body_vec).unwrap();
        thread::sleep(Duration::from_millis(50));
    });
    let (server_stream, _peer) = listener.accept().unwrap();
    let parsed = sniff_auth_key_message(&server_stream).unwrap();
    assert_eq!(parsed.key, key_bytes);
    drop(server_stream);
    let _ = client_thread.join();
}
