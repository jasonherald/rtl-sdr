use super::*;

#[test]
fn sniff_client_hello_full_hello_parses_correctly() {
    // Happy path: client sends a complete 8-byte hello, sniff
    // returns `Ok(Some)` with the parsed struct. Regression
    // guard against a future refactor breaking the common case.
    use crate::codec::CodecMask;
    use crate::extension::{CLIENT_HELLO_FLAGS_NONE, PROTOCOL_VERSION, Role};
    let hello = ClientHello {
        codec_mask: CodecMask::NONE_AND_LZ4,
        role: Role::Control,
        flags: CLIENT_HELLO_FLAGS_NONE,
        version: PROTOCOL_VERSION,
    };
    let bytes = hello.to_bytes();
    let result = run_sniff_against(move |mut client| {
        client.write_all(&bytes).unwrap();
        // Let the server finish reading before the client
        // stream drops (which would EOF mid-read).
        thread::sleep(Duration::from_millis(50));
    });
    assert_eq!(result.unwrap(), Some(hello));
}

#[test]
fn sniff_client_hello_idle_client_returns_legacy_fallback() {
    // Legacy rtl_tcp client: connects, then idles waiting for
    // the server's `dongle_info_t`. Zero bytes reach the sniff
    // before the timeout fires, so `Ok(None)` is the safe
    // fallback — nothing consumed, no desync risk.
    let result = run_sniff_against(|client| {
        // Hold the socket open well past the sniff timeout.
        thread::sleep(HELLO_SNIFF_TIMEOUT * 3);
        drop(client);
    });
    match result {
        Ok(None) => {}
        other => panic!("expected Ok(None) for idle client, got {other:?}"),
    }
}

#[test]
fn sniff_client_hello_non_magic_prefix_is_legacy_fallback() {
    // Vanilla client sends a `SetCenterFreq` command
    // immediately after connect (opcode 0x01 + 4-byte arg).
    // Peek reads 4 bytes, magic doesn't match, sniff returns
    // `Ok(None)` without consuming — so the command_worker
    // reads the full 5-byte frame cleanly.
    let result = run_sniff_against(|mut client| {
        // 5-byte vanilla SetCenterFreq command: opcode=0x01,
        // freq=100_000_000 Hz big-endian.
        let cmd: [u8; 5] = [0x01, 0x05, 0xF5, 0xE1, 0x00];
        client.write_all(&cmd).unwrap();
        thread::sleep(Duration::from_millis(100));
    });
    match result {
        Ok(None) => {}
        other => panic!("expected Ok(None) for non-RTLX prefix, got {other:?}"),
    }
}

#[test]
fn sniff_client_hello_partial_hello_is_protocol_error() {
    // **Regression test for `CodeRabbit` round 2 on PR #399.**
    // A client that sends the 4-byte `RTLX` magic and then
    // stalls without sending the remaining 4 hello bytes used
    // to fall back to the legacy path — which desynced the
    // command stream by 4 bytes (those magic bytes were
    // already consumed by `read_exact` before it timed out).
    // The fix promotes partial-hello to `Err` so the client
    // gets dropped instead.
    let result = run_sniff_against(|mut client| {
        // Send magic only; hold the connection open past the
        // sniff timeout so `read_exact` observes partial data.
        client.write_all(&EXTENSION_MAGIC).unwrap();
        thread::sleep(HELLO_SNIFF_TIMEOUT * 5);
        drop(client);
    });
    assert!(
        result.is_err(),
        "partial hello (magic only, body stalled) must surface as Err — \
         got {result:?} which would desync the command stream on fallback"
    );
}

#[test]
fn sniff_client_hello_fragmented_magic_completes_successfully() {
    // **Regression test for `CodeRabbit` round 5 on PR #402.**
    // A well-behaved RTLX client whose `ClientHello` bytes
    // span two TCP segments (e.g. `RT` in one, `LX` + body
    // in the next) previously fell back to legacy on the
    // first short peek — corrupting the command stream for
    // the unlucky RTLX client. The fix retries the peek
    // while the observed bytes are a prefix of
    // `EXTENSION_MAGIC`, so a fragmented magic still reaches
    // the full `read_exact` path.
    use crate::codec::CodecMask;
    use crate::extension::{CLIENT_HELLO_FLAGS_NONE, PROTOCOL_VERSION, Role};
    let hello = ClientHello {
        codec_mask: CodecMask::NONE_AND_LZ4,
        role: Role::Control,
        flags: CLIENT_HELLO_FLAGS_NONE,
        version: PROTOCOL_VERSION,
    };
    let bytes = hello.to_bytes();
    let result = run_sniff_against(move |mut client| {
        // Send the first 2 magic bytes, flush, pause briefly
        // to force a short peek on the server side, then
        // send the remaining 6 bytes.
        client.write_all(&bytes[..2]).unwrap();
        client.flush().unwrap();
        thread::sleep(Duration::from_millis(10));
        client.write_all(&bytes[2..]).unwrap();
        // Keep the client alive long enough for the server
        // to finish `read_exact` before we drop and EOF.
        thread::sleep(Duration::from_millis(50));
    });
    assert_eq!(
        result.unwrap(),
        Some(hello),
        "fragmented RTLX hello must not fall back to legacy on the first short peek"
    );
}

#[test]
fn sniff_client_hello_stalled_magic_prefix_is_protocol_error() {
    // **Regression test for `CodeRabbit` round 5 on PR #402**
    // (initial promotion to `Err`) **+ round 6** (pinning
    // the error kind to `InvalidData`, not `TimedOut`).
    //
    // A client that starts sending the magic (e.g. `RT`)
    // and then stalls without completing the remaining 2
    // bytes must NOT fall back to legacy: the prefix bytes
    // are still queued in the receive buffer, and handing
    // them to `command_worker` would start the legacy
    // command stream at `R` (0x52) which isn't any valid
    // opcode — poisoning every subsequent command. Promote
    // to `Err` with `ErrorKind::InvalidData` so it lands in
    // the same classification bucket as `read_exact`
    // timeout mid-hello and a malformed body — all three
    // are protocol-desync errors from the host's POV.
    let result = run_sniff_against(|mut client| {
        client.write_all(&EXTENSION_MAGIC[..2]).unwrap();
        thread::sleep(HELLO_SNIFF_TIMEOUT * 5);
        drop(client);
    });
    let err = result.expect_err(
        "stalled magic prefix must surface as Err (dropping the client) — legacy \
         fallback would desync the command stream",
    );
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::InvalidData,
        "stalled RTLX prefix classifies as InvalidData (protocol desync), not \
         TimedOut (idle-socket semantics) — got {err:?}"
    );
}

#[test]
fn sniff_client_hello_short_non_magic_prefix_is_legacy_fallback() {
    // Legacy client whose first TCP segment carries only 1
    // byte that already disagrees with magic (e.g. 0x01 =
    // `SetCenterFreq` opcode). The sniff loop must recognize
    // this as a definite non-match from the short peek and
    // fall back to legacy immediately — no reason to wait
    // out the full `HELLO_SNIFF_TIMEOUT` when the first byte
    // already rules out RTLX.
    //
    // Times just the sniff call (not the client-thread join)
    // to verify the short-circuit. Inlined instead of using
    // `run_sniff_against` so the client keeps the socket
    // open past the sniff return — otherwise we can't prove
    // the sniff exited on the definite-non-match path vs.
    // on an EOF from the client dropping.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let keep_alive = Arc::new(AtomicBool::new(true));
    let keep_alive_clone = Arc::clone(&keep_alive);
    let client_thread = thread::spawn(move || {
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(&[0x01]).unwrap();
        client.flush().unwrap();
        // Hold the socket open so the sniff must decide on
        // the 1-byte prefix alone, not on EOF.
        while keep_alive_clone.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(2));
        }
        drop(client);
    });
    let (server_stream, _peer) = listener.accept().unwrap();
    let sniff_start = std::time::Instant::now();
    let result = sniff_client_hello(&server_stream);
    let elapsed = sniff_start.elapsed();
    keep_alive.store(false, Ordering::Relaxed);
    drop(server_stream);
    let _ = client_thread.join();

    match result {
        Ok(None) => {}
        other => panic!("expected Ok(None) for short non-magic prefix, got {other:?}"),
    }
    assert!(
        elapsed < HELLO_SNIFF_TIMEOUT,
        "short non-magic prefix should short-circuit within HELLO_SNIFF_TIMEOUT, \
         but sniff took {elapsed:?} (HELLO_SNIFF_TIMEOUT = {HELLO_SNIFF_TIMEOUT:?})"
    );
}

#[test]
fn sniff_client_hello_malformed_body_is_protocol_error() {
    // Client sends a full 8 bytes starting with `RTLX` but with
    // an unknown role byte (0x99). Body parses as `None` →
    // protocol error. Previously returned `Ok(None)` (legacy
    // fallback on a shifted stream — desync risk).
    let mut garbled = [0u8; CLIENT_HELLO_LEN];
    garbled[..EXTENSION_MAGIC.len()].copy_from_slice(&EXTENSION_MAGIC);
    garbled[4] = 0x03; // codec mask (NONE+LZ4)
    garbled[5] = 0x99; // invalid role — from_bytes returns None
    garbled[6] = 0x00; // flags
    garbled[7] = crate::extension::PROTOCOL_VERSION;
    let result = run_sniff_against(move |mut client| {
        client.write_all(&garbled).unwrap();
        thread::sleep(Duration::from_millis(50));
    });
    assert!(
        result.is_err(),
        "malformed hello body (magic matched, unknown role) must surface as Err — \
         got {result:?}"
    );
}
