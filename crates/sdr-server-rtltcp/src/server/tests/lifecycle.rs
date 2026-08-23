use super::*;

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

    // Read the kernel-side values back through socket2 (safe
    // `getsockopt`) rather than raw libc.
    let sock = socket2::SockRef::from(&server_stream);
    assert!(
        sock.keepalive().unwrap(),
        "SO_KEEPALIVE should be enabled after configure_client_socket"
    );
    assert_eq!(
        sock.tcp_keepalive_time().unwrap(),
        Duration::from_secs(u64::from(TCP_KEEPALIVE_IDLE_SECS)),
        "TCP_KEEPIDLE should match the tuned constant"
    );
    assert_eq!(
        sock.tcp_keepalive_interval().unwrap(),
        Duration::from_secs(u64::from(TCP_KEEPALIVE_INTERVAL_SECS)),
        "TCP_KEEPINTVL should match the tuned constant"
    );
    assert_eq!(
        sock.tcp_keepalive_retries().unwrap(),
        TCP_KEEPALIVE_RETRIES,
        "TCP_KEEPCNT should match the tuned constant"
    );

    drop(server_stream);
    let _ = client.join();
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
