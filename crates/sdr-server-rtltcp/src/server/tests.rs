use super::*;

// ============================================================
// Test fixture constants (CodeRabbit round 4 on PR #402).
// Extracted so each test's intent reads at a glance —
// `42_001` on its own is noise, `TEST_CLIENT_A_PORT` plus a
// bounds docstring is self-documenting.
// ============================================================

/// Loopback peer port for the "client A" side of two-client
/// fixtures. Non-privileged, doesn't overlap with anything
/// well-known, and disjoint from `TEST_CLIENT_B_PORT`.
const TEST_CLIENT_A_PORT: u16 = 42_001;
/// Loopback peer port for "client B". Disjoint from
/// `TEST_CLIENT_A_PORT` so snapshot assertions can verify
/// ordering / identity.
const TEST_CLIENT_B_PORT: u16 = 42_002;
/// Small per-client channel depth used by tests that don't
/// exercise the full/drop path — just needs to fit the few
/// chunks a test sends. Anything ≥ the chunk count is fine.
const TEST_CLIENT_CHANNEL_DEPTH: usize = 4;
/// Synthetic `bytes_sent` value for client A's stats —
/// arbitrary small number, just has to differ from B's value
/// so the per-client readback assertions prove the right
/// entry landed in `connected_clients[0]`.
const TEST_CLIENT_A_BYTES: u64 = 100;
/// Synthetic `bytes_sent` value for client B. Differs from
/// A's by an order of magnitude so a cross-over bug stands out.
const TEST_CLIENT_B_BYTES: u64 = 999;
/// 2-meter amateur band frequency (145.5 MHz) stamped into
/// client A's `current_freq_hz` — stand-in for "non-default
/// freq client A commanded".
const TEST_CLIENT_A_FREQ_HZ: u32 = 145_500_000;
/// WFM broadcast band frequency (100 MHz) stamped into
/// client B's `current_freq_hz`. Second distinct sample so
/// cross-client bugs show up as the wrong freq under
/// `connected_clients[1]`.
const TEST_CLIENT_B_FREQ_HZ: u32 = 100_000_000;

/// Test helper: `getsockopt(fd, level, name)` for integer
/// options. Panics on failure with the OS errno so test
/// diagnostics point at the real problem instead of a bare
/// "assertion failed". Linux-only because the only caller is
/// the keepalive readback test below and those constant
/// names are Linux-specific.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn get_sockopt_int(fd: libc::c_int, level: libc::c_int, name: libc::c_int) -> libc::c_int {
    let mut value: libc::c_int = 0;
    let mut len: libc::socklen_t = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `fd` is a valid open socket for the call's
    // duration (test-side caller holds the accepted
    // TcpStream). `value` and `len` live on the stack; their
    // pointers are valid for the `getsockopt` call.
    let ret = unsafe {
        libc::getsockopt(
            fd,
            level,
            name,
            std::ptr::addr_of_mut!(value).cast(),
            std::ptr::addr_of_mut!(len),
        )
    };
    assert_eq!(
        ret,
        0,
        "getsockopt(level={level}, name={name}) failed: {}",
        std::io::Error::last_os_error()
    );
    value
}

#[cfg(target_os = "linux")]
#[test]
fn configure_client_socket_applies_tuned_keepalive_on_linux() {
    // **Regression test for #393.** Verifies that the
    // TCP_KEEPIDLE / TCP_KEEPINTVL / TCP_KEEPCNT constants
    // actually land on the socket — not just that setsockopt
    // returned zero. Calls getsockopt after
    // `configure_client_socket` and asserts the kernel-side
    // values match our per-file constants.
    //
    // Linux-only because the platform constants differ
    // (macOS has only `TCP_KEEPALIVE`, FreeBSD has different
    // names). Other targets are exercised via the Unsupported
    // fallback path in `set_keepalive_tuned`.
    use std::os::unix::io::AsRawFd;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let s = TcpStream::connect(addr).unwrap();
        // Hold the client side alive long enough for the
        // server half to run getsockopt. Drops at thread
        // exit, which cleans up the socket pair.
        thread::sleep(Duration::from_millis(100));
        drop(s);
    });
    let (server_stream, _peer) = listener.accept().unwrap();
    configure_client_socket(&server_stream);

    let fd = server_stream.as_raw_fd();
    assert_ne!(
        get_sockopt_int(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE),
        0,
        "SO_KEEPALIVE should be enabled after configure_client_socket"
    );
    assert_eq!(
        get_sockopt_int(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE) as u32,
        TCP_KEEPALIVE_IDLE_SECS,
        "TCP_KEEPIDLE should match the tuned constant"
    );
    assert_eq!(
        get_sockopt_int(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL) as u32,
        TCP_KEEPALIVE_INTERVAL_SECS,
        "TCP_KEEPINTVL should match the tuned constant"
    );
    assert_eq!(
        get_sockopt_int(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT) as u32,
        TCP_KEEPALIVE_RETRIES,
        "TCP_KEEPCNT should match the tuned constant"
    );

    drop(server_stream);
    let _ = client.join();
}

// ============================================================
// Auth handshake tests (#394).
//
// The real `spawn_client_workers` needs a live
// `Arc<Mutex<RtlSdrDevice>>` which requires a USB dongle —
// not something CI has. These tests instead exercise
// `sniff_auth_key_message` directly over a loopback TCP
// pair, pinning the wire-read contract that the enforcement
// flow calls into. Full end-to-end (server + client with
// real dongle) lives in the manual smoke test.
// ============================================================

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

/// Scheduling slack on top of `AUTH_REPLY_TIMEOUT` when
/// asserting that a timeout fired inside the budget. Must be
/// tight enough that a regression to per-phase timeouts
/// (where total elapsed could approach `2 * AUTH_REPLY_TIMEOUT`
/// under the header-then-body flow) trips the assertion, but
/// generous enough to absorb realistic OS scheduling jitter
/// on a loaded CI runner. The original 500 ms flaked on
/// shared GitHub-hosted runners at ~508 ms elapsed (8 ms past
/// the budget — pure scheduling noise). Bumped to 1500 ms:
/// still well under the 2× regression threshold of 5 s
/// (a per-phase revert would land elapsed near 10 s, so
/// 5 + 1.5 = 6.5 s is comfortably on the "good" side), while
/// giving enough headroom that one OS scheduling hiccup won't
/// redden CI. Per `CodeRabbit` round 3 on PR #405, bumped
/// per PR #437 CI flake.
const AUTH_TIMEOUT_SLACK: Duration = Duration::from_millis(1500);

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

#[test]
fn start_surfaces_port_conflict_as_typed_error() {
    // Hold a port before calling Server::start — the second bind must
    // surface as ServerError::PortInUse (not a generic IO error), so
    // the UI can fall back without parsing error strings.
    //
    // This test does NOT need a real RTL-SDR dongle present because
    // Server::start binds the listener before touching USB.
    let holder = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let config = ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], port)),
        device_index: 0,
        initial: InitialDeviceState::default(),
        buffer_capacity: 0,
        compression: CodecMask::NONE_ONLY,
        listener_cap: DEFAULT_LISTENER_CAP,
        auth_key: None,
    };
    match Server::start(config) {
        Err(ServerError::PortInUse(ref addr)) => {
            assert!(addr.contains(&format!("{port}")));
        }
        Err(e) => panic!("expected PortInUse, got {e:?}"),
        Ok(_) => panic!("bind should have failed"),
    }
    drop(holder);
}

#[test]
fn start_rejects_empty_auth_key_as_config_error() {
    // **Regression test for `CodeRabbit` round 2 on PR #405.**
    // `ServerConfig` is public, so a library caller can
    // construct `Some(vec![])` and bypass every upstream
    // guard (FFI / CLI / wire format). `Server::start` must
    // catch this BEFORE bind / USB open so the operator sees
    // one clear config error instead of every client
    // failing at handshake.
    //
    // Uses port 0 so bind succeeds even if another test is
    // holding a port — the config validation runs first, so
    // the bind never actually happens. USB is never touched
    // either (same early-exit ordering).
    let config = ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        device_index: 0,
        initial: InitialDeviceState::default(),
        buffer_capacity: 0,
        compression: CodecMask::NONE_ONLY,
        listener_cap: DEFAULT_LISTENER_CAP,
        auth_key: Some(Vec::new()), // empty — invalid per `validate_auth_key`
    };
    match Server::start(config) {
        Err(ServerError::InvalidAuthKeyLength { len, max }) => {
            assert_eq!(len, 0);
            assert_eq!(max, crate::extension::MAX_AUTH_KEY_LEN);
        }
        Err(e) => panic!("expected InvalidAuthKeyLength, got {e:?}"),
        Ok(_) => panic!("empty auth_key must not start a server"),
    }
}

#[test]
fn start_rejects_oversize_auth_key_as_config_error() {
    // Complement to the empty-key test — keys longer than
    // `MAX_AUTH_KEY_LEN` (256 bytes) can't be serialized by
    // `AuthKeyMessage::to_bytes`, so the server would start
    // but reject every client at handshake. Catch at config
    // time.
    let config = ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        device_index: 0,
        initial: InitialDeviceState::default(),
        buffer_capacity: 0,
        compression: CodecMask::NONE_ONLY,
        listener_cap: DEFAULT_LISTENER_CAP,
        auth_key: Some(vec![0u8; crate::extension::MAX_AUTH_KEY_LEN + 1]),
    };
    match Server::start(config) {
        Err(ServerError::InvalidAuthKeyLength { len, max }) => {
            assert_eq!(len, crate::extension::MAX_AUTH_KEY_LEN + 1);
            assert_eq!(max, crate::extension::MAX_AUTH_KEY_LEN);
        }
        Err(e) => panic!("expected InvalidAuthKeyLength, got {e:?}"),
        Ok(_) => panic!("oversize auth_key must not start a server"),
    }
}

#[test]
fn validate_auth_key_length_accepts_none() {
    // None disables the auth gate entirely — always valid.
    // Per the #395 live-update shared-validation contract.
    validate_auth_key_length(None).unwrap();
}

#[test]
fn validate_auth_key_length_accepts_boundary_sizes() {
    // 1 byte = minimum non-empty key, `MAX_AUTH_KEY_LEN` = 256 =
    // maximum that the wire format (`AuthKeyMessage::to_bytes`)
    // can serialize. Both boundaries must pass — the `1..=max`
    // range is inclusive on both ends.
    validate_auth_key_length(Some(&[0u8; 1])).unwrap();
    validate_auth_key_length(Some(&[0u8; crate::extension::MAX_AUTH_KEY_LEN])).unwrap();
}

#[test]
fn validate_auth_key_length_rejects_empty() {
    // `Some(vec![])` bypasses every upstream guard (FFI, CLI,
    // wire-format) but would silently match an `expected` of
    // the same length at handshake time. `validate_auth_key`
    // already guards the match, but defense-in-depth: catch
    // at construction / set time too. Pinned separately from
    // `Server::start` + `Server::set_auth_key` so the helper's
    // contract is a first-class regression surface independent
    // of the callers.
    match validate_auth_key_length(Some(&[])) {
        Err(ServerError::InvalidAuthKeyLength { len, max }) => {
            assert_eq!(len, 0);
            assert_eq!(max, crate::extension::MAX_AUTH_KEY_LEN);
        }
        other => panic!("expected InvalidAuthKeyLength, got {other:?}"),
    }
}

#[test]
fn validate_auth_key_length_rejects_oversize() {
    // `MAX_AUTH_KEY_LEN + 1` = smallest oversize. Serialization
    // via `AuthKeyMessage::to_bytes` would fail every handshake;
    // catch at config time.
    let oversize = vec![0u8; crate::extension::MAX_AUTH_KEY_LEN + 1];
    match validate_auth_key_length(Some(&oversize)) {
        Err(ServerError::InvalidAuthKeyLength { len, max }) => {
            assert_eq!(len, crate::extension::MAX_AUTH_KEY_LEN + 1);
            assert_eq!(max, crate::extension::MAX_AUTH_KEY_LEN);
        }
        other => panic!("expected InvalidAuthKeyLength, got {other:?}"),
    }
}

#[test]
fn initial_device_state_defaults_match_upstream_rtl_tcp() {
    let d = InitialDeviceState::default();
    // rtl_tcp.c:389-392 — these are the upstream defaults.
    assert_eq!(d.center_freq_hz, 100_000_000);
    assert_eq!(d.sample_rate_hz, 2_048_000);
    assert_eq!(d.ppm, 0);
    assert!(!d.bias_tee);
    assert_eq!(d.direct_sampling, 0);
    assert!(d.gain_tenths_db.is_none());
}

#[test]
fn default_loopback_config_binds_localhost() {
    let cfg = ServerConfig::default_loopback();
    assert_eq!(cfg.bind.ip().to_string(), "127.0.0.1");
    assert_eq!(cfg.bind.port(), crate::protocol::DEFAULT_PORT);
    assert_eq!(cfg.buffer_capacity, DEFAULT_BUFFER_CAPACITY);
}

#[test]
fn server_stats_default_is_empty() {
    let stats = ServerStats::default();
    assert!(stats.connected_clients.is_empty());
    assert_eq!(stats.total_bytes_sent, 0);
    assert_eq!(stats.total_buffers_dropped, 0);
    assert_eq!(stats.lifetime_accepted, 0);
    // Default initial state matches the upstream rtl_tcp defaults.
    assert_eq!(stats.initial.center_freq_hz, DEFAULT_CENTER_FREQ_HZ);
    assert_eq!(stats.initial.sample_rate_hz, DEFAULT_SAMPLE_RATE_HZ);
}

#[test]
fn recent_commands_capacity_matches_documented_bound() {
    // Sanity check on the published const. If the UI side starts
    // depending on a specific size for pagination, changing the
    // constant becomes a contract break this test catches.
    assert_eq!(RECENT_COMMANDS_CAPACITY, 50);
}

#[test]
fn has_stopped_is_false_before_accept_thread_exits() {
    // We can't stand up a real Server without hardware, but we CAN
    // sanity-check the `stopped` flag contract: `has_stopped()`
    // reads the AtomicBool directly. Default state is false.
    let stopped = Arc::new(AtomicBool::new(false));
    assert!(!stopped.load(Ordering::Relaxed));
    // Accept thread setting the flag → has_stopped() observes true.
    stopped.store(true, Ordering::SeqCst);
    assert!(stopped.load(Ordering::Relaxed));
}

#[test]
fn buffer_capacity_zero_uses_default() {
    // ServerConfig exposes `buffer_capacity: 0` as "use default". This
    // is checked during Server::start, but we can sanity-check the
    // DEFAULT_BUFFER_CAPACITY matches upstream's llbuf_num = 500
    // (rtl_tcp.c:61).
    assert_eq!(DEFAULT_BUFFER_CAPACITY, 500);
}

#[test]
fn server_stats_exposes_all_connected_clients() {
    // Multi-client shape: `connected_clients` carries one
    // `ClientInfo` per registered slot. Different from the
    // pre-#391 single-client projection which only exposed the
    // first client's session fields. This test pins the
    // contract that every registered slot is visible to the
    // UI / FFI — critical for the per-client rendering that
    // follows in PR B.
    use crate::broadcaster::ClientSlot;
    let registry = Arc::new(ClientRegistry::new());

    let (slot_a, _rx_a) = ClientSlot::new(
        registry.allocate_id(),
        SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_A_PORT)),
        Codec::None,
        Role::Control,
        TEST_CLIENT_CHANNEL_DEPTH,
    );
    if let Ok(mut s) = slot_a.stats.lock() {
        s.bytes_sent = TEST_CLIENT_A_BYTES;
        s.current_freq_hz = Some(TEST_CLIENT_A_FREQ_HZ);
    }
    registry.register(slot_a);

    let (slot_b, _rx_b) = ClientSlot::new(
        registry.allocate_id(),
        SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_B_PORT)),
        Codec::Lz4,
        Role::Control,
        TEST_CLIENT_CHANNEL_DEPTH,
    );
    if let Ok(mut s) = slot_b.stats.lock() {
        s.bytes_sent = TEST_CLIENT_B_BYTES;
        s.current_freq_hz = Some(TEST_CLIENT_B_FREQ_HZ);
    }
    registry.register(slot_b);

    // Snapshot via the registry directly since we don't have a
    // real Server here — the same code path `Server::stats`
    // uses to build its `ServerStats`.
    let stats = ServerStats {
        connected_clients: registry.snapshot(),
        total_bytes_sent: registry.total_bytes_sent(),
        total_buffers_dropped: registry.total_buffers_dropped(),
        lifetime_accepted: registry.lifetime_accepted(),
        initial: InitialDeviceState::default(),
    };

    assert_eq!(stats.connected_clients.len(), 2);
    assert_eq!(
        stats.connected_clients[0].peer,
        SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_A_PORT))
    );
    assert_eq!(stats.connected_clients[0].bytes_sent, TEST_CLIENT_A_BYTES);
    assert_eq!(
        stats.connected_clients[1].peer,
        SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_B_PORT))
    );
    assert_eq!(stats.connected_clients[1].codec, Codec::Lz4);
    assert_eq!(stats.lifetime_accepted, 2);
}

// ============================================================
// sniff_client_hello regression tests (`CodeRabbit` round 2 on PR #399)
//
// The sniff is the only piece of the per-client handshake that
// can run without a real RTL-SDR dongle, so unit tests live here.
// Each test pairs a server-side accept with a client-side TCP
// connect + controlled write pattern, verifying that
// `sniff_client_hello` classifies the stream correctly.
// ============================================================

/// Accept one TCP client on a loopback listener and hand the
/// accepted socket to `sniff_client_hello`. Factored out so
/// each scenario test stays focused on what bytes the client
/// writes, not the boilerplate of setting up sockets.
fn run_sniff_against<F>(client_behavior: F) -> std::io::Result<Option<ClientHello>>
where
    F: FnOnce(TcpStream) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client_thread = thread::spawn(move || {
        let client = TcpStream::connect(addr).unwrap();
        client_behavior(client);
    });
    let (server_stream, _peer) = listener.accept().unwrap();
    let result = sniff_client_hello(&server_stream);
    // Join best-effort — the client thread may legitimately still
    // be holding the connection open (partial-hello test). Drop
    // the server side first so any pending write on the client
    // side unblocks, then join.
    drop(server_stream);
    let _ = client_thread.join();
    result
}

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

// --- #709 / #711 (Aug 2026 deep review) ---

/// Data-socket stand-in that times out `stalls_remaining` times
/// before accepting bytes, like a peer whose receive window is
/// closed.
struct StallingWriter {
    stalls_remaining: u32,
    written: Vec<u8>,
}

impl Write for StallingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.stalls_remaining > 0 {
            self.stalls_remaining -= 1;
            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
        }
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const STALL_TEST_PORT: u16 = 42_010;

fn test_peer(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}
const STALL_TEST_CHUNK: [u8; 4] = [1, 2, 3, 4];

/// A brief stall drops queued chunks, not the client: the chunk in
/// progress is retried, the chunks that queued up behind it are
/// discarded (counted as drops), and the slot stays live.
#[test]
fn tcp_writer_drops_queued_chunks_before_dropping_a_stalled_client() {
    /// How long the test waits for the writer to drain the stall.
    const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
    let registry = Arc::new(ClientRegistry::new());
    let (slot, rx) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(STALL_TEST_PORT),
        Codec::None,
        Role::Control,
        TEST_CLIENT_CHANNEL_DEPTH,
    );
    for _ in 0..3 {
        slot.tx.send(STALL_TEST_CHUNK.to_vec()).expect("queue");
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    let written = Arc::new(Mutex::new(Vec::new()));
    let writer_thread = {
        let (slot, registry, shutdown, written) = (
            slot.clone(),
            registry.clone(),
            shutdown.clone(),
            written.clone(),
        );
        thread::spawn(move || {
            let mut w = StallingWriter {
                stalls_remaining: 2,
                written: Vec::new(),
            };
            tcp_writer(&mut w, rx, slot, registry, shutdown, true);
            *written.lock().expect("written") = w.written;
        })
    };
    // The slot's own sender keeps the channel open, so the writer
    // idles after the stall; stop it once the drops are visible.
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    while registry.total_buffers_dropped() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    shutdown.store(true, Ordering::SeqCst);
    writer_thread.join().expect("writer thread");
    assert!(
        !slot.is_disconnected(),
        "a brief stall must not kick the client"
    );
    assert_eq!(*written.lock().expect("written"), STALL_TEST_CHUNK.to_vec());
    let dropped = slot.stats.lock().expect("stats").buffers_dropped;
    assert_eq!(
        dropped, 2,
        "the two chunks queued behind the stall were dropped"
    );
    assert_eq!(registry.total_buffers_dropped(), 2);
}

/// A stall that outlasts the budget does drop the client.
#[test]
fn tcp_writer_gives_up_after_the_stall_budget() {
    let registry = Arc::new(ClientRegistry::new());
    let (slot, rx) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(STALL_TEST_PORT),
        Codec::None,
        Role::Control,
        TEST_CLIENT_CHANNEL_DEPTH,
    );
    slot.tx.send(STALL_TEST_CHUNK.to_vec()).expect("queue");
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut w = StallingWriter {
        stalls_remaining: MAX_CONSECUTIVE_WRITE_STALLS + 1,
        written: Vec::new(),
    };
    tcp_writer(&mut w, rx, slot.clone(), registry, shutdown, true);
    assert!(slot.is_disconnected());
    assert!(w.written.is_empty());
}

/// A compressed stream cannot resume mid-block: its first stall
/// closes the client instead of being retried.
#[test]
fn tcp_writer_closes_a_compressed_stream_on_its_first_stall() {
    let registry = Arc::new(ClientRegistry::new());
    let (slot, rx) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(STALL_TEST_PORT),
        Codec::None,
        Role::Control,
        TEST_CLIENT_CHANNEL_DEPTH,
    );
    slot.tx.send(STALL_TEST_CHUNK.to_vec()).expect("queue");
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut w = StallingWriter {
        stalls_remaining: 1,
        written: Vec::new(),
    };
    tcp_writer(&mut w, rx, slot.clone(), registry, shutdown, false);
    assert!(slot.is_disconnected());
    assert!(w.written.is_empty());
}

/// Socket stand-in that alternates one byte of progress with a
/// stall: never two stalls in a row, so the consecutive budget
/// must never trip no matter how long the chunk is.
struct TricklingWriter {
    stall_next: bool,
    written: Vec<u8>,
}

impl Write for TricklingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stall_next = !self.stall_next;
        if !self.stall_next {
            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
        }
        self.written.push(buf[0]);
        Ok(1)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The stall budget counts consecutive stalls: progress resets it.
#[test]
fn stall_budget_resets_on_progress() {
    let registry = ClientRegistry::new();
    let (slot, rx) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(STALL_TEST_PORT),
        Codec::None,
        Role::Control,
        TEST_CLIENT_CHANNEL_DEPTH,
    );
    // Longer than the budget, so an accumulating counter would
    // close the client part-way through.
    let chunk = vec![0xA5_u8; (MAX_CONSECUTIVE_WRITE_STALLS as usize) * 3];
    let mut w = TricklingWriter {
        stall_next: false,
        written: Vec::new(),
    };
    let outcome = write_chunk_shedding_backlog(&mut w, &chunk, &rx, &slot, &registry, true);
    assert_eq!(outcome, ChunkOutcome::Sent);
    assert_eq!(w.written, chunk);
    assert!(!slot.is_disconnected());
}

/// #808: any successful read — even a zero-length one — resets
/// the consecutive-error budget; a timeout leaves it untouched.
#[test]
fn zero_length_usb_read_resets_the_error_budget() {
    let mut errors = 0_u32;
    for _ in 1..MAX_CONSECUTIVE_USB_ERRORS {
        assert_eq!(
            classify_usb_read(Err(rusb::Error::Io), &mut errors),
            UsbReadOutcome::Retry(rusb::Error::Io)
        );
    }
    assert_eq!(
        classify_usb_read(Err(rusb::Error::Timeout), &mut errors),
        UsbReadOutcome::Idle
    );
    assert_eq!(errors, MAX_CONSECUTIVE_USB_ERRORS - 1, "timeout is neutral");
    assert_eq!(classify_usb_read(Ok(0), &mut errors), UsbReadOutcome::Idle);
    assert_eq!(errors, 0, "Ok(0) is a successful read");
    assert_eq!(
        classify_usb_read(Err(rusb::Error::Io), &mut errors),
        UsbReadOutcome::Retry(rusb::Error::Io)
    );
    assert_eq!(
        classify_usb_read(Ok(7), &mut errors),
        UsbReadOutcome::Data(7)
    );
    assert_eq!(errors, 0);
}

/// USB read failures are tolerated up to librtlsdr's consecutive
/// transfer-error budget; a device loss stops immediately.
#[test]
fn usb_read_failures_stop_only_after_the_consecutive_budget() {
    let mut errors = 0_u32;
    for _ in 1..MAX_CONSECUTIVE_USB_ERRORS {
        assert_eq!(
            classify_usb_read(Err(rusb::Error::Overflow), &mut errors),
            UsbReadOutcome::Retry(rusb::Error::Overflow)
        );
    }
    assert_eq!(
        classify_usb_read(Err(rusb::Error::Pipe), &mut errors),
        UsbReadOutcome::Stop(rusb::Error::Pipe)
    );
    let mut fresh = 0_u32;
    assert_eq!(
        classify_usb_read(Err(rusb::Error::NoDevice), &mut fresh),
        UsbReadOutcome::Stop(rusb::Error::NoDevice)
    );
}
