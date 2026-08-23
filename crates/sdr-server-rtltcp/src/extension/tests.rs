use super::*;

#[test]
fn client_hello_round_trip() {
    let hello = ClientHello {
        codec_mask: CodecMask::NONE_AND_LZ4,
        role: Role::Control,
        flags: FLAG_REQUEST_TAKEOVER,
        version: PROTOCOL_VERSION,
    };
    let bytes = hello.to_bytes();
    assert_eq!(bytes.len(), CLIENT_HELLO_LEN);
    assert_eq!(&bytes[..4], &EXTENSION_MAGIC);
    assert_eq!(ClientHello::from_bytes(&bytes), Some(hello));
}

#[test]
fn client_hello_rejects_bad_magic() {
    // Legacy commands leak into the hello slot when a client
    // talks to a vanilla server that didn't consume them
    // cleanly. Magic mismatch → None, so the server falls
    // through to its legacy path.
    let mut bytes = [0u8; CLIENT_HELLO_LEN];
    bytes[..4].copy_from_slice(b"RTLY"); // wrong
    assert!(ClientHello::from_bytes(&bytes).is_none());
}

#[test]
fn client_hello_rejects_unknown_role() {
    let mut bytes = [0u8; CLIENT_HELLO_LEN];
    bytes[..4].copy_from_slice(&EXTENSION_MAGIC);
    bytes[4] = CodecMask::NONE_ONLY.to_wire();
    bytes[5] = 99; // unknown role byte
    bytes[7] = PROTOCOL_VERSION;
    assert!(ClientHello::from_bytes(&bytes).is_none());
}

#[test]
fn client_hello_rejects_unknown_version() {
    // Regression test for `CodeRabbit` round 3 on PR #399:
    // a future peer that bumps the wire layout must be
    // rejected so we don't silently mis-negotiate it as v1.
    // Updated for PR #405: supported set is now {v1, v2}, so
    // the first rejected value is v3. Version byte zero is
    // also rejected (guards against uninitialized struct
    // slipping through).
    let mut bytes = [0u8; CLIENT_HELLO_LEN];
    bytes[..4].copy_from_slice(&EXTENSION_MAGIC);
    bytes[4] = CodecMask::NONE_ONLY.to_wire();
    bytes[5] = Role::Control.to_wire();
    bytes[6] = 0;
    // v3 is not in SUPPORTED_VERSIONS → rejected.
    bytes[7] = PROTOCOL_VERSION_V2 + 1;
    assert!(ClientHello::from_bytes(&bytes).is_none());
    // v0 is the uninitialized-struct sentinel → rejected.
    bytes[7] = 0;
    assert!(ClientHello::from_bytes(&bytes).is_none());
}

#[test]
fn client_hello_accepts_both_supported_versions() {
    // **Regression test for `CodeRabbit` round 1 on PR #405.**
    // The version gate widened from strict `== PROTOCOL_VERSION`
    // to `SUPPORTED_VERSIONS.contains(..)` so pre-#394 v1
    // clients can still hand a hello to a post-#394 v2 server
    // without the server rejecting them. Pins both members
    // of the supported set.
    let base = ClientHello {
        codec_mask: CodecMask::NONE_ONLY,
        role: Role::Control,
        flags: CLIENT_HELLO_FLAGS_NONE,
        version: PROTOCOL_VERSION_V1,
    };
    let v1_bytes = base.to_bytes();
    assert_eq!(
        ClientHello::from_bytes(&v1_bytes).map(|h| h.version),
        Some(PROTOCOL_VERSION_V1),
        "v1 hello must be accepted for pre-#394 client back-compat"
    );

    let v2 = ClientHello {
        version: PROTOCOL_VERSION_V2,
        ..base
    };
    let v2_bytes = v2.to_bytes();
    assert_eq!(
        ClientHello::from_bytes(&v2_bytes).map(|h| h.version),
        Some(PROTOCOL_VERSION_V2),
        "v2 hello must be accepted for #394-aware clients"
    );
}

#[test]
fn required_protocol_version_picks_minimum_viable() {
    // Helper contract: only bump to v2 when the hello
    // carries auth. Plain / compression / takeover hellos
    // stay v1 so pre-#394 servers continue to accept them.
    assert_eq!(
        required_protocol_version(CLIENT_HELLO_FLAGS_NONE),
        PROTOCOL_VERSION_V1
    );
    assert_eq!(
        required_protocol_version(FLAG_REQUEST_TAKEOVER),
        PROTOCOL_VERSION_V1
    );
    assert_eq!(
        required_protocol_version(FLAG_HAS_AUTH),
        PROTOCOL_VERSION_V2
    );
    assert_eq!(
        required_protocol_version(FLAG_HAS_AUTH | FLAG_REQUEST_TAKEOVER),
        PROTOCOL_VERSION_V2,
        "takeover + auth together still needs v2 (any auth bit forces v2)"
    );
}

#[test]
fn server_extension_round_trip() {
    let ext = ServerExtension {
        codec: Codec::Lz4,
        granted_role: Some(Role::Control),
        status: Status::Ok,
        version: PROTOCOL_VERSION,
    };
    let bytes = ext.to_bytes();
    assert_eq!(bytes.len(), SERVER_EXTENSION_LEN);
    assert_eq!(&bytes[..4], &EXTENSION_MAGIC);
    assert_eq!(ServerExtension::from_bytes(&bytes), Some(ext));
}

#[test]
fn server_extension_denied_role_round_trips() {
    // #392 path: the server encodes "denied" as the 0xFF
    // sentinel. Decoder maps it back to `None`.
    let ext = ServerExtension {
        codec: Codec::None,
        granted_role: None,
        status: Status::ControllerBusy,
        version: PROTOCOL_VERSION,
    };
    let bytes = ext.to_bytes();
    assert_eq!(bytes[5], GRANTED_ROLE_DENIED);
    assert_eq!(ServerExtension::from_bytes(&bytes), Some(ext));
}

#[test]
fn server_extension_rejects_bad_magic() {
    // Client peeked random IQ data that happens NOT to match
    // `"RTLX"`. Decoder returns None → client falls back to
    // legacy uncompressed read, and those 4 peeked bytes
    // stay in the TCP read buffer for the next stream read.
    let mut bytes = [0u8; SERVER_EXTENSION_LEN];
    bytes[..4].copy_from_slice(b"\x00\x01\x02\x03"); // unlikely in IQ; arbitrary
    assert!(ServerExtension::from_bytes(&bytes).is_none());
}

#[test]
fn server_extension_rejects_unknown_version() {
    // Same rationale as `client_hello_rejects_unknown_version`
    // — a newer server's response with an unknown schema
    // version must cause a clean protocol error at parse time
    // rather than silently coercing forward-compat fields into
    // v1 semantics. Per CodeRabbit round 3 on PR #399.
    let mut bytes = [0u8; SERVER_EXTENSION_LEN];
    bytes[..4].copy_from_slice(&EXTENSION_MAGIC);
    bytes[4] = Codec::None.to_wire();
    bytes[5] = Role::Control.to_wire();
    bytes[6] = Status::Ok.to_wire();
    // v3 is outside SUPPORTED_VERSIONS → rejected.
    bytes[7] = PROTOCOL_VERSION_V2 + 1;
    assert!(ServerExtension::from_bytes(&bytes).is_none());
}

#[test]
fn server_extension_listener_cap_reached_round_trips() {
    // #392 path: server denies a Listen request because the
    // cap is already filled. Encoded with `granted_role =
    // denied (0xFF)` + `status = ListenerCapReached (4)`.
    // Additive status code — no PROTOCOL_VERSION bump needed
    // because the schema gate already catches peers that
    // read a value they don't know.
    let ext = ServerExtension {
        codec: Codec::None,
        granted_role: None,
        status: Status::ListenerCapReached,
        version: PROTOCOL_VERSION,
    };
    let bytes = ext.to_bytes();
    assert_eq!(bytes[5], GRANTED_ROLE_DENIED);
    assert_eq!(bytes[6], 4);
    assert_eq!(ServerExtension::from_bytes(&bytes), Some(ext));
}

#[test]
fn status_from_wire_covers_all_documented_variants() {
    // Pin the 0/1/2/3/4 → enum mapping. A future addition
    // that reshuffles the discriminants would break over-the-
    // wire compat with already-shipped clients; this test is
    // the trip-wire.
    assert_eq!(Status::from_wire(0), Some(Status::Ok));
    assert_eq!(Status::from_wire(1), Some(Status::ControllerBusy));
    assert_eq!(Status::from_wire(2), Some(Status::AuthRequired));
    assert_eq!(Status::from_wire(3), Some(Status::AuthFailed));
    assert_eq!(Status::from_wire(4), Some(Status::ListenerCapReached));
    assert_eq!(Status::from_wire(5), None);
    assert_eq!(Status::from_wire(255), None);
}

#[test]
fn client_hello_takeover_flag_helper() {
    let with_flag = ClientHello {
        codec_mask: CodecMask::NONE_ONLY,
        role: Role::Control,
        flags: FLAG_REQUEST_TAKEOVER,
        version: PROTOCOL_VERSION,
    };
    assert!(with_flag.request_takeover());

    let without_flag = ClientHello {
        flags: 0,
        ..with_flag
    };
    assert!(!without_flag.request_takeover());
}

#[test]
fn magic_first_byte_not_a_legacy_opcode() {
    // Defense-in-depth: if a sdr-rs client accidentally
    // sends a hello to a vanilla rtl_tcp server, the first
    // byte `'R' = 0x52` must NOT collide with a real
    // command opcode, or the server would try to execute it.
    // Documented opcodes are 0x01..=0x0E (per rtl_tcp.c); our
    // magic's first byte sits well above that range.
    assert!(EXTENSION_MAGIC[0] > 0x0E);
}

// ============================================================
// AuthKeyMessage (#394) wire-format tests.
// ============================================================

#[test]
fn auth_key_message_round_trip() {
    // Minimum viable auth key — a single byte. Exercises the
    // length-field encoding + the header + key concatenation.
    let msg = AuthKeyMessage { key: vec![0x42] };
    let bytes = msg.to_bytes().expect("single-byte key serializes");
    assert_eq!(bytes.len(), AUTH_KEY_HEADER_LEN + 1);
    assert_eq!(&bytes[..4], &AUTH_KEY_MAGIC);
    assert_eq!(&bytes[4..6], &1u16.to_be_bytes());
    assert_eq!(bytes[6], 0x42);
    assert_eq!(AuthKeyMessage::from_bytes(&bytes), Some(msg));
}

#[test]
fn auth_key_message_round_trip_full_length() {
    // 256-byte key (the max) — pins that the u16 length field
    // encodes correctly at the upper bound and `from_bytes`
    // accepts it. Regression guard against an off-by-one that
    // rejects exactly-MAX keys.
    let key: Vec<u8> = (0..MAX_AUTH_KEY_LEN).map(|i| i as u8).collect();
    let msg = AuthKeyMessage { key: key.clone() };
    let bytes = msg.to_bytes().expect("max-length key serializes");
    assert_eq!(bytes.len(), AUTH_KEY_HEADER_LEN + MAX_AUTH_KEY_LEN);
    let len_field = u16::from_be_bytes([bytes[4], bytes[5]]);
    assert_eq!(len_field as usize, MAX_AUTH_KEY_LEN);
    assert_eq!(AuthKeyMessage::from_bytes(&bytes), Some(msg));
}

#[test]
fn auth_key_message_empty_key_rejected_on_encode() {
    // Zero-length key would be trivially matched by an empty
    // expected key on the server side — defeats the auth gate.
    // Reject at serialize time.
    let msg = AuthKeyMessage { key: vec![] };
    assert!(msg.to_bytes().is_none());
}

#[test]
fn auth_key_message_over_max_rejected_on_encode() {
    // Anything > MAX_AUTH_KEY_LEN can't be expressed in the
    // u16 length field's valid range (we cap below u16::MAX so
    // a malicious server can't allocate ~64 KiB per handshake).
    let msg = AuthKeyMessage {
        key: vec![0u8; MAX_AUTH_KEY_LEN + 1],
    };
    assert!(msg.to_bytes().is_none());
}

#[test]
fn auth_key_message_rejects_bad_magic() {
    let mut bytes = vec![0x00, 0x01, 0x02, 0x03]; // not RTKA
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes.push(0x42);
    assert!(AuthKeyMessage::from_bytes(&bytes).is_none());
}

#[test]
fn auth_key_message_rejects_zero_length() {
    let mut bytes = AUTH_KEY_MAGIC.to_vec();
    bytes.extend_from_slice(&0u16.to_be_bytes());
    // No trailing bytes — slice length matches header but the
    // length field is 0.
    assert!(AuthKeyMessage::from_bytes(&bytes).is_none());
}

#[test]
fn auth_key_message_rejects_length_mismatch() {
    // Header says 4 bytes but only 2 follow — truncated on
    // the wire. `from_bytes` requires the full message to
    // have been read; servers that decode incrementally must
    // `read_exact(header.key_len)` after parsing the header.
    let mut bytes = AUTH_KEY_MAGIC.to_vec();
    bytes.extend_from_slice(&4u16.to_be_bytes());
    bytes.extend_from_slice(&[0x01, 0x02]);
    assert!(AuthKeyMessage::from_bytes(&bytes).is_none());
}

#[test]
fn auth_key_message_parse_header_len_decodes_valid_range() {
    let mut header = [0u8; AUTH_KEY_HEADER_LEN];
    header[..4].copy_from_slice(&AUTH_KEY_MAGIC);
    header[4..6].copy_from_slice(&42u16.to_be_bytes());
    assert_eq!(AuthKeyMessage::parse_header_len(&header), Some(42));
}

#[test]
fn auth_key_message_parse_header_len_rejects_zero_and_overlong() {
    let mut header = [0u8; AUTH_KEY_HEADER_LEN];
    header[..4].copy_from_slice(&AUTH_KEY_MAGIC);
    // Zero length.
    header[4..6].copy_from_slice(&0u16.to_be_bytes());
    assert!(AuthKeyMessage::parse_header_len(&header).is_none());
    // Length exceeds MAX.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "test-only overflow synthesis, caller check is the point"
    )]
    let overlong = (MAX_AUTH_KEY_LEN + 1) as u16;
    header[4..6].copy_from_slice(&overlong.to_be_bytes());
    assert!(AuthKeyMessage::parse_header_len(&header).is_none());
}

#[test]
fn client_hello_has_auth_flag_helper() {
    let with_auth = ClientHello {
        codec_mask: CodecMask::NONE_ONLY,
        role: Role::Control,
        flags: FLAG_HAS_AUTH,
        version: PROTOCOL_VERSION,
    };
    assert!(with_auth.has_auth());
    assert!(!with_auth.request_takeover());

    // Flags are additive — takeover + auth together.
    let both = ClientHello {
        flags: FLAG_HAS_AUTH | FLAG_REQUEST_TAKEOVER,
        ..with_auth
    };
    assert!(both.has_auth());
    assert!(both.request_takeover());

    let without_auth = ClientHello {
        flags: 0,
        ..with_auth
    };
    assert!(!without_auth.has_auth());
}

#[test]
fn flag_bits_are_distinct() {
    // Defense-in-depth: if someone ever adds a third flag bit
    // and accidentally collides with an existing one, this
    // test trips. Each bit must be uniquely assigned.
    assert_eq!(FLAG_REQUEST_TAKEOVER & FLAG_HAS_AUTH, 0);
    assert_ne!(FLAG_REQUEST_TAKEOVER, 0);
    assert_ne!(FLAG_HAS_AUTH, 0);
}

#[test]
fn auth_key_magic_first_byte_distinct_from_legacy_opcodes() {
    // `RTKA`'s first byte is 'R' (0x52), same as
    // EXTENSION_MAGIC. That's fine here because an
    // AuthKeyMessage is only ever read AFTER a hello, never
    // as the first bytes on a fresh connection, so there's
    // no legacy-opcode collision path. Documenting this as
    // a test so a future refactor that re-reads AUTH_KEY_MAGIC
    // at connection-start would trip the >0x0E assertion
    // and the reviewer knows to re-examine the flow.
    assert_eq!(AUTH_KEY_MAGIC[0], b'R');
    assert!(AUTH_KEY_MAGIC[0] > 0x0E);
}
