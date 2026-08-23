use super::*;

#[test]
fn rtlx_handshake_sends_takeover_flag_when_config_opts_in() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (server_thread, hello_rx) = rtlx_serve_one_with(listener, |_, _| {
        rtlx_ok_extension(Codec::None, Role::Control, PROTOCOL_VERSION)
    });
    let mut config = rtlx_test_config(CodecMask::NONE_ONLY);
    config.request_takeover = true;
    let mut src = rtlx_start(addr, config);

    let hello = recv_hello(&hello_rx);
    assert_eq!(&hello[..EXTENSION_MAGIC.len()], &EXTENSION_MAGIC);
    let flags_byte = hello[HELLO_FLAGS_OFFSET];
    assert_ne!(
        flags_byte & sdr_server_rtltcp::extension::FLAG_REQUEST_TAKEOVER,
        0,
        "request_takeover = true must set FLAG_REQUEST_TAKEOVER bit \
         in the hello (got flags byte 0x{flags_byte:02x})"
    );
    assert_eq!(
        hello[HELLO_CODEC_MASK_OFFSET],
        CodecMask::NONE_ONLY.to_wire()
    );

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtlx_handshake_clears_takeover_flag_when_only_compression_opts_in() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (server_thread, hello_rx) = rtlx_serve_one_with(listener, |_, _| {
        rtlx_ok_extension(Codec::Lz4, Role::Control, PROTOCOL_VERSION)
    });
    let mut src = rtlx_start(addr, rtlx_test_config(CodecMask::NONE_AND_LZ4));

    let hello = recv_hello(&hello_rx);
    assert_eq!(&hello[..EXTENSION_MAGIC.len()], &EXTENSION_MAGIC);
    let flags_byte = hello[HELLO_FLAGS_OFFSET];
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
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let expected_key = b"the-shared-secret-32-bytes-!!".to_vec();
    let (auth_tx, auth_rx) = mpsc::channel::<Vec<u8>>();
    // After the hello the server reads the AuthKey message that the
    // FLAG_HAS_AUTH hello promises, forwarding its body to the test.
    let (server_thread, hello_rx) = rtlx_serve_one_with(listener, move |sock, _| {
        let mut header = [0u8; sdr_server_rtltcp::extension::AUTH_KEY_HEADER_LEN];
        sock.read_exact(&mut header).expect("read auth header");
        let key_len = sdr_server_rtltcp::extension::AuthKeyMessage::parse_header_len(&header)
            .expect("valid header");
        let mut body = vec![0u8; key_len as usize];
        sock.read_exact(&mut body).expect("read auth body");
        let _ = auth_tx.send(body);
        rtlx_ok_extension(Codec::None, Role::Control, PROTOCOL_VERSION)
    });
    let mut config = rtlx_test_config(CodecMask::NONE_ONLY);
    config.auth_key = Some(expected_key.clone());
    let mut src = rtlx_start(addr, config);

    let hello = recv_hello(&hello_rx);
    assert_eq!(&hello[..EXTENSION_MAGIC.len()], &EXTENSION_MAGIC);
    let flags_byte = hello[HELLO_FLAGS_OFFSET];
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
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (probe_tx, probe_rx) = mpsc::channel::<std::io::Result<usize>>();
    // With no auth key the hello must be the client's last word until
    // the server answers: probe for stray bytes, then echo the
    // client's hello version back.
    let (server_thread, hello_rx) = rtlx_serve_one_with(listener, move |sock, hello| {
        let _ = probe_tx.send(probe_for_client_bytes(sock));
        rtlx_ok_extension(Codec::Lz4, Role::Control, hello[HELLO_VERSION_OFFSET])
    });
    let mut src = rtlx_start(addr, rtlx_test_config(CodecMask::NONE_AND_LZ4));

    let hello = recv_hello(&hello_rx);
    assert_eq!(
        hello[HELLO_FLAGS_OFFSET] & sdr_server_rtltcp::extension::FLAG_HAS_AUTH,
        0,
        "auth_key = None must clear FLAG_HAS_AUTH bit in the hello"
    );
    assert_eq!(
        hello[HELLO_VERSION_OFFSET],
        sdr_server_rtltcp::extension::PROTOCOL_VERSION_V1,
        "compression-only hello must emit v1 (compat with pre-#394 servers)"
    );
    let probe_result = probe_rx
        .recv_timeout(RTLX_TEST_STATE_DEADLINE)
        .expect("server probe should resolve within deadline");
    assert_probe_saw_nothing(
        probe_result,
        "client with auth_key = None must NOT emit any follow-up bytes",
    );

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtl_tcp_default_config_sends_no_hello_to_vanilla_server() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (probe_tx, probe_rx) = mpsc::channel::<std::io::Result<usize>>();
    // Vanilla server: probe for a hello that must not come, then send
    // dongle_info_t as rtl_tcp does.
    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let _ = probe_tx.send(probe_for_client_bytes(&mut sock));
        let _ = sock.write_all(&rtlx_test_dongle_info());
        thread::sleep(RTLX_TEST_SERVER_HOLD);
    });
    let mut src = rtlx_start(addr, RtlTcpConfig::default());

    let probe_result = probe_rx
        .recv_timeout(RTLX_TEST_STATE_DEADLINE)
        .expect("server probe should resolve within deadline");
    assert_probe_saw_nothing(
        probe_result,
        "default config must not send a hello before dongle_info_t",
    );

    src.stop_manager();
    let _ = server_thread.join();
}

#[test]
fn rtlx_handshake_sends_listen_role_when_config_opts_in() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (server_thread, hello_rx) = rtlx_serve_one_with(listener, |_, hello| {
        let version = hello[HELLO_VERSION_OFFSET];
        assert_eq!(
            version,
            sdr_server_rtltcp::extension::PROTOCOL_VERSION_V1,
            "role-only hello must stay on v1 for backward compatibility (got 0x{version:02x})",
        );
        rtlx_ok_extension(Codec::None, Role::Listen, version)
    });
    let mut config = rtlx_test_config(CodecMask::NONE_ONLY);
    config.requested_role = Role::Listen;
    let mut src = rtlx_start(addr, config);

    let hello = recv_hello(&hello_rx);
    assert_eq!(&hello[..EXTENSION_MAGIC.len()], &EXTENSION_MAGIC);
    assert_eq!(
        hello[HELLO_ROLE_OFFSET],
        Role::Listen as u8,
        "Listen opt-in must encode Role::Listen at byte offset {HELLO_ROLE_OFFSET} (got 0x{:02x})",
        hello[HELLO_ROLE_OFFSET],
    );
    assert_eq!(hello[HELLO_FLAGS_OFFSET], 0);
    let reached_listen = wait_until(RTLX_TEST_STATE_DEADLINE, || {
        matches!(
            src.connection_state(),
            ConnectionState::Connected {
                granted_role: Some(Role::Listen),
                ..
            }
        )
    });
    assert!(
        reached_listen,
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
