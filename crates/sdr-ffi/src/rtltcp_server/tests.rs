use super::*;
use std::ffi::CStr;
use std::time::Duration;

// --------------------------------------------------------
//  Shared test fixtures — per `CodeRabbit` round 5 on
//  PR #360. Repeated literals across the `initial_from_c`
//  and `bind_socket_addr` tests funnel through these.
// --------------------------------------------------------

/// TCP port used in happy-path configs. Matches the
/// `sdr_server_rtltcp::DEFAULT_PORT` convention (1234).
const TEST_PORT: u16 = 1234;

/// Second test port used to prove `bind_socket_addr` honors
/// the caller-supplied value on the all-interfaces path.
const TEST_ALT_PORT: u16 = 9000;

/// Default center frequency in Hz — 100 MHz WFM band.
const TEST_FREQ_HZ: u32 = 100_000_000;

/// Default sample rate — 2.048 Msps (canonical RTL-SDR
/// value that doesn't starve the USB controller).
const TEST_SAMPLE_RATE_HZ: u32 = 2_048_000;

/// Non-zero tuner gain in tenths of dB. 256 = 25.6 dB —
/// well inside the R820T's table so the
/// "auto vs manual" gain-round-trip assertion has a value
/// that's unambiguously "manual."
const TEST_NONZERO_GAIN_TENTHS: i32 = 256;

/// R820T discrete gain-step count, used by the stats
/// fixture when constructing a synthetic
/// `TunerAdvertiseInfo`.
const TEST_TUNER_GAIN_COUNT: u32 = 29;

/// Sentinel that trips the direct-sampling validation.
const TEST_INVALID_DIRECT_SAMPLING: i32 = 3;

/// Device index = 0 matches the first attached dongle.
const TEST_DEVICE_INDEX: u32 = 0;

// ------------------------------------------------------------
// `ClientInfo` fixture constants (`CodeRabbit` round 4 on
// PR #402). Extracted so the multi-client ABI tests stay
// declarative and future per-client assertions inherit the
// same values.
// ------------------------------------------------------------

/// Stable test `ClientId`. Arbitrary non-zero — the registry
/// allocates ids starting at 0, so a mid-range value here
/// proves the FFI path isn't accidentally hard-coding the
/// first-slot assumption.
const TEST_CLIENT_ID: u64 = 42;
/// Peer port for the gain-validity test — high ephemeral
/// range so it doesn't collide with anything real, and
/// disjoint from `TEST_CLIENT_PEER_ADDR_PORT` below.
const TEST_CLIENT_GAIN_PEER_PORT: u16 = 50_100;
/// Peer port + full IP for the NUL-termination test. Uses a
/// private-range IP to match typical LAN-server scenarios so
/// the packed peer string reads like a real deployment.
const TEST_CLIENT_PEER_IP: [u8; 4] = [192, 168, 1, 100];
const TEST_CLIENT_PEER_PORT: u16 = 1234;
/// Synthetic per-client `bytes_sent` used by the gain test
/// so the `client_info_to_c` readback confirms the counter
/// propagated through the projection.
const TEST_CLIENT_BYTES_SENT: u64 = 9_999;
/// Synthetic per-client `buffers_dropped` — non-zero so it
/// can't pass from a zero-initialized struct by accident.
const TEST_CLIENT_BUFFERS_DROPPED: u64 = 1;
/// JSON serialization test: how many seconds back in time
/// to place the second `recent_commands` entry so the
/// `"seconds_ago"` field has a non-trivial value.
const TEST_COMMAND_AGE_SECS: u64 = 3;
/// Peer port for the JSON serialization tests' synthetic
/// `ClientInfo` — arbitrary, just needs to differ from the
/// other fixture ports so a cross-test regression pins to
/// the right test.
const TEST_CLIENT_JSON_PEER_PORT: u16 = 50_200;

/// Build a happy-path `SdrRtlTcpServerConfig`. Tests tweak
/// a single field to target one validation branch at a
/// time — that way a future schema addition lands in one
/// place instead of N tests.
fn base_test_config() -> SdrRtlTcpServerConfig {
    SdrRtlTcpServerConfig {
        bind_address: SDR_BIND_LOOPBACK,
        port: TEST_PORT,
        device_index: TEST_DEVICE_INDEX,
        buffer_capacity: 0,
        initial_freq_hz: TEST_FREQ_HZ,
        initial_sample_rate_hz: TEST_SAMPLE_RATE_HZ,
        initial_gain_tenths_db: 0,
        initial_ppm: 0,
        initial_bias_tee: false,
        initial_direct_sampling: 0,
        listener_cap: 0, // 0 → use DEFAULT_LISTENER_CAP
        // Auth disabled: NULL pointer + zero length = no
        // auth gate. Matches the "LAN-trust default" from
        // the struct docs.
        auth_key: std::ptr::null(),
        auth_key_len: 0,
        // ABI 0.19 defaults — zero-init equivalent so the base
        // fixture matches pre-#400 behaviour. Tests that
        // exercise the new compression mask mutate these after
        // `base_test_config()` returns.
        has_compression: false,
        compression: 0,
    }
}

#[test]
fn bind_socket_addr_loopback() {
    let addr = bind_socket_addr(SDR_BIND_LOOPBACK, TEST_PORT).unwrap();
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert_eq!(addr.port(), TEST_PORT);
}

#[test]
fn bind_socket_addr_all_interfaces() {
    let addr = bind_socket_addr(SDR_BIND_ALL_INTERFACES, TEST_ALT_PORT).unwrap();
    assert_eq!(addr.ip().to_string(), "0.0.0.0");
    assert_eq!(addr.port(), TEST_ALT_PORT);
}

#[test]
fn bind_socket_addr_rejects_unknown() {
    assert!(bind_socket_addr(99, TEST_PORT).is_err());
}

#[test]
fn bind_socket_addr_zero_port_uses_default() {
    let addr = bind_socket_addr(SDR_BIND_LOOPBACK, 0).unwrap();
    assert_eq!(addr.port(), DEFAULT_PORT);
}

#[test]
fn listener_cap_from_c_zero_uses_default() {
    // Contract: `SdrRtlTcpServerConfig::listener_cap == 0` is
    // the "use the crate default" sentinel. Extracted helper
    // lets this rule be verified without the hardware-backed
    // `sdr_rtltcp_server_start` path. Per `CodeRabbit` round 2
    // on PR #403.
    assert_eq!(
        listener_cap_from_c(0),
        sdr_server_rtltcp::DEFAULT_LISTENER_CAP
    );
}

#[test]
fn listener_cap_from_c_nonzero_is_preserved() {
    // Non-zero values widen from u32 to usize without
    // modification. Picking a mid-range value rules out an
    // off-by-one that would have passed `listener_cap_from_c(1)
    // == 1` trivially.
    assert_eq!(listener_cap_from_c(7), 7);
}

/// Wire byte for `CodecMask::NONE_ONLY` — pinned here so the
/// tests name the protocol value rather than lean on a raw
/// hex literal. Matches `CodecMask::NONE_ONLY.to_wire()` by
/// construction; if that ever drifts, both this constant
/// and the non-test `CodecMask::NONE_ONLY` bits would need
/// to shift in lockstep. Per `CodeRabbit` round 2 on
/// PR #418.
const TEST_CODEC_MASK_NONE_ONLY_WIRE: u8 = 0x01;
/// Wire byte for `CodecMask::NONE_AND_LZ4` — None bit +
/// LZ4 bit. Same pinning rationale as
/// [`TEST_CODEC_MASK_NONE_ONLY_WIRE`].
const TEST_CODEC_MASK_NONE_AND_LZ4_WIRE: u8 = 0x03;

#[test]
fn compression_from_c_zero_init_is_none_only() {
    // ABI 0.19 default: `has_compression = false` + any
    // `compression` byte → `NONE_ONLY`. This is the "pre-0.19
    // caller with a zero-init struct stays on the legacy
    // path" contract. Check both 0 and a non-zero raw byte
    // to prove the `compression` field is ignored when the
    // gate is false.
    assert_eq!(compression_from_c(false, 0), CodecMask::NONE_ONLY);
    assert_eq!(
        compression_from_c(false, TEST_CODEC_MASK_NONE_AND_LZ4_WIRE),
        CodecMask::NONE_ONLY
    );
}

#[test]
fn compression_from_c_opt_in_passes_raw_wire_byte() {
    // `has_compression = true` → `CodecMask::from_wire(byte)`.
    // Verify both a `None-only` round-trip and a
    // `None + LZ4` round-trip.
    assert_eq!(
        compression_from_c(true, TEST_CODEC_MASK_NONE_ONLY_WIRE),
        CodecMask::NONE_ONLY
    );
    assert_eq!(
        compression_from_c(true, TEST_CODEC_MASK_NONE_AND_LZ4_WIRE),
        CodecMask::NONE_AND_LZ4
    );
}

#[test]
fn auth_key_from_c_null_pointer_zero_length_is_disabled() {
    // Default C struct (zero-initialized) has NULL + 0 here.
    // Must map to `Disabled`, not `Invalid` — otherwise
    // every default-constructed `SdrRtlTcpServerConfig`
    // would fail to start.
    // SAFETY: NULL pointer with matching zero length is
    // valid input to `auth_key_from_c` per its contract.
    let outcome = unsafe { auth_key_from_c(std::ptr::null(), 0) };
    assert_eq!(outcome, AuthKeyFromC::Disabled);
}

#[test]
fn auth_key_from_c_valid_pointer_and_length_is_enabled() {
    // Normal enable path: caller's buffer + length.
    let buf = [0xAAu8, 0xBB, 0xCC, 0xDD];
    #[allow(
        clippy::cast_possible_truncation,
        reason = "buf.len() is a const 4, fits u32 trivially"
    )]
    let len = buf.len() as u32;
    // SAFETY: `buf.as_ptr()` is valid for `buf.len()` bytes.
    let outcome = unsafe { auth_key_from_c(buf.as_ptr(), len) };
    let AuthKeyFromC::Enabled(bytes) = outcome else {
        unreachable!("expected Enabled variant, got {outcome:?}");
    };
    assert_eq!(bytes, buf.to_vec());
}

#[test]
fn auth_key_from_c_null_with_nonzero_length_is_invalid() {
    // Malformed: caller claimed length but gave NULL. Reject
    // cleanly rather than dereferencing a null pointer.
    // SAFETY: Invalid input path; pointer is not
    // dereferenced.
    let outcome = unsafe { auth_key_from_c(std::ptr::null(), 4) };
    assert_eq!(outcome, AuthKeyFromC::Invalid);
}

#[test]
fn auth_key_from_c_nonnull_with_zero_length_is_invalid() {
    // Malformed: caller gave a pointer but said length is 0.
    // Could be an uninitialized field or a bug where the
    // operator passed a buffer but forgot the length. Reject.
    let buf = [0xAAu8];
    // SAFETY: Invalid input path; pointer is not
    // dereferenced (the length-zero check short-circuits).
    let outcome = unsafe { auth_key_from_c(buf.as_ptr(), 0) };
    assert_eq!(outcome, AuthKeyFromC::Invalid);
}

#[test]
fn auth_key_from_c_over_max_length_is_invalid() {
    // Length > MAX_AUTH_KEY_LEN would fail downstream at
    // AuthKeyMessage serialization. Catch here so the FFI
    // caller sees InvalidArg instead of a runtime handshake
    // failure.
    let buf = vec![0u8; sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN + 1];
    // SAFETY: `buf.as_ptr()` is valid for `buf.len()` bytes,
    // but we expect the length-range check to fail before
    // any deref happens.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "MAX_AUTH_KEY_LEN + 1 = 257 fits u32 trivially"
    )]
    let len = buf.len() as u32;
    let outcome = unsafe { auth_key_from_c(buf.as_ptr(), len) };
    assert_eq!(outcome, AuthKeyFromC::Invalid);
}

#[test]
fn auth_key_from_c_max_length_at_boundary_is_enabled() {
    // Exactly `MAX_AUTH_KEY_LEN = 256` bytes — the upper
    // bound. Pins an off-by-one defense: a check spelled
    // `>` vs `>=` would regress this to `Invalid`.
    let buf = vec![0xEEu8; sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN];
    #[allow(
        clippy::cast_possible_truncation,
        reason = "MAX_AUTH_KEY_LEN = 256 fits u32 trivially"
    )]
    let len = buf.len() as u32;
    // SAFETY: Valid buffer of matching length.
    let outcome = unsafe { auth_key_from_c(buf.as_ptr(), len) };
    let AuthKeyFromC::Enabled(bytes) = outcome else {
        unreachable!("expected Enabled at max length, got {outcome:?}");
    };
    assert_eq!(bytes.len(), sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN);
    assert_eq!(bytes[0], 0xEE);
}

#[test]
fn initial_from_c_rejects_zero_sample_rate() {
    // Pins the guard added in round 2 per `CodeRabbit` —
    // a zero-init `SdrRtlTcpServerConfig` must not slip
    // through and wedge the RTL-SDR USB controller.
    let mut cfg = base_test_config();
    cfg.initial_sample_rate_hz = 0;
    assert!(initial_from_c(&cfg).is_err());
}

#[test]
fn initial_from_c_rejects_out_of_range_direct_sampling() {
    let mut cfg = base_test_config();
    cfg.initial_direct_sampling = TEST_INVALID_DIRECT_SAMPLING;
    assert!(initial_from_c(&cfg).is_err());
}

#[test]
fn initial_from_c_zero_gain_maps_to_auto() {
    let cfg = base_test_config();
    let initial = initial_from_c(&cfg).unwrap();
    assert_eq!(initial.gain_tenths_db, None);
}

#[test]
fn initial_from_c_nonzero_gain_preserved() {
    let mut cfg = base_test_config();
    cfg.initial_gain_tenths_db = TEST_NONZERO_GAIN_TENTHS;
    let initial = initial_from_c(&cfg).unwrap();
    assert_eq!(initial.gain_tenths_db, Some(TEST_NONZERO_GAIN_TENTHS));
}

#[test]
fn start_rejects_oversized_buffer_capacity() {
    // Pins the MAX_BUFFER_CAPACITY guard added in round 9
    // per `CodeRabbit`. Construct options that would
    // otherwise pass every earlier check (loopback bind,
    // valid port, valid direct-sampling, non-zero sample
    // rate) and drive `buffer_capacity` past the cap —
    // `_start` must return `InvalidArg` before touching
    // the device layer.
    let opts = SdrRtlTcpServerConfig {
        bind_address: SDR_BIND_LOOPBACK,
        port: TEST_PORT,
        device_index: TEST_DEVICE_INDEX,
        buffer_capacity: SDR_RTLTCP_SERVER_MAX_BUFFER_CAPACITY + 1,
        initial_freq_hz: TEST_FREQ_HZ,
        initial_sample_rate_hz: TEST_SAMPLE_RATE_HZ,
        initial_gain_tenths_db: 0,
        initial_ppm: 0,
        initial_bias_tee: false,
        initial_direct_sampling: 0,
        listener_cap: 0,
        auth_key: std::ptr::null(),
        auth_key_len: 0,
        has_compression: false,
        compression: 0,
    };
    let mut handle: *mut SdrRtlTcpServer = std::ptr::null_mut();
    let rc = unsafe { sdr_rtltcp_server_start(&raw const opts, &raw mut handle) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    assert!(handle.is_null());
}

#[test]
fn start_with_null_pointers_returns_invalid_arg() {
    let rc = unsafe { sdr_rtltcp_server_start(std::ptr::null(), std::ptr::null_mut()) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
}

#[test]
fn stats_with_null_handle_returns_invalid_handle() {
    let mut stats = SdrRtlTcpServerStats::default();
    let rc = unsafe {
        sdr_rtltcp_server_stats(
            std::ptr::null_mut(),
            &raw mut stats,
            std::ptr::null_mut(),
            0,
        )
    };
    assert_eq!(rc, SdrCoreError::InvalidHandle.as_int());
}

#[test]
fn client_list_with_null_handle_returns_invalid_handle() {
    let mut count: usize = 0;
    let rc = unsafe {
        sdr_rtltcp_server_client_list(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &raw mut count,
        )
    };
    assert_eq!(rc, SdrCoreError::InvalidHandle.as_int());
}

#[test]
fn client_list_with_null_out_count_returns_invalid_arg() {
    let rc = unsafe {
        sdr_rtltcp_server_client_list(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
}

#[test]
fn stop_handles_null_gracefully() {
    // No crash, no panic.
    unsafe { sdr_rtltcp_server_stop(std::ptr::null_mut()) };
}

#[test]
fn stats_to_c_packs_aggregate_counters() {
    // Post-#391 shape: `SdrRtlTcpServerStats` carries only
    // aggregate cumulative counters + the tuner gain count.
    // Per-client state belongs in `SdrRtlTcpClientInfo` and
    // ships through `sdr_rtltcp_server_client_list`.
    let tuner = TunerAdvertiseInfo {
        name: "R820T".into(),
        gain_count: TEST_TUNER_GAIN_COUNT,
    };
    let stats = ServerStats {
        connected_clients: Vec::new(),
        total_bytes_sent: 1_234_567,
        total_buffers_dropped: 3,
        lifetime_accepted: 7,
        initial: InitialDeviceState::default(),
    };
    let c = stats_to_c(&stats, &tuner);
    assert_eq!(c.connected_count, 0);
    assert_eq!(c.total_bytes_sent, 1_234_567);
    assert_eq!(c.total_buffers_dropped, 3);
    assert_eq!(c.lifetime_accepted, 7);
    assert_eq!(c.gain_count, TEST_TUNER_GAIN_COUNT);
}

#[test]
fn client_info_to_c_preserves_independent_gain_validity() {
    // Four-state matrix for the two gain Options on
    // `ClientInfo`. Pins the "don't collapse into a single
    // `has_current_gain` bit" behavior that shipped on the
    // pre-#391 server-wide struct (CR round 7 on PR #360),
    // now preserved per-client.
    use sdr_server_rtltcp::codec::Codec;
    let snapshot_at = std::time::Instant::now();
    let mut info = ClientInfo {
        id: TEST_CLIENT_ID,
        peer: SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_GAIN_PEER_PORT)),
        connected_since: std::time::Instant::now(),
        codec: Codec::Lz4,
        role: sdr_server_rtltcp::extension::Role::Control,
        bytes_sent: TEST_CLIENT_BYTES_SENT,
        buffers_dropped: TEST_CLIENT_BUFFERS_DROPPED,
        last_command: None,
        current_freq_hz: None,
        current_sample_rate_hz: None,
        current_gain_tenths_db: None,
        current_gain_auto: None,
        recent_commands: std::collections::VecDeque::new(),
    };

    // (None, None) → neither set
    let c = client_info_to_c(&info, snapshot_at);
    assert!(!c.has_current_gain_value);
    assert!(!c.has_current_gain_mode);
    assert_eq!(c.id, TEST_CLIENT_ID);
    assert_eq!(c.bytes_sent, TEST_CLIENT_BYTES_SENT);
    assert_eq!(c.codec, 1); // LZ4 wire value
    // Role projection: Control → 0 wire byte. Pins the #392
    // FFI contract per `CodeRabbit` round 1 on PR #403 — a
    // regression in `client_info_to_c` that drops the role
    // field would otherwise slip through without detection.
    assert_eq!(
        c.role,
        sdr_server_rtltcp::extension::Role::Control.to_wire()
    );
    // `last_command` fixture is `None` for the whole test;
    // the projection must surface that as `has_last_command
    // == false` with the op / age fields defaulted to zero
    // so FFI hosts never read an undefined opcode or age.
    assert!(!c.has_last_command);
    assert_eq!(c.last_command_op, 0);
    // Exact-zero float comparison is correct here — the
    // `None` branch of the projection assigns the literal
    // `0.0` without any arithmetic, so a non-zero readback
    // would mean the projection wrote the age unconditionally.
    #[allow(
        clippy::float_cmp,
        reason = "projection assigns literal 0.0 in the None branch"
    )]
    let age_is_zero = c.last_command_age_secs == 0.0;
    assert!(age_is_zero);

    // (Some(v), None) → value set, mode unknown
    info.current_gain_tenths_db = Some(TEST_NONZERO_GAIN_TENTHS);
    info.current_gain_auto = None;
    let c = client_info_to_c(&info, snapshot_at);
    assert!(c.has_current_gain_value);
    assert!(!c.has_current_gain_mode);
    assert_eq!(c.current_gain_tenths_db, TEST_NONZERO_GAIN_TENTHS);
    assert!(!c.current_gain_auto);

    // (None, Some(auto)) → mode set, value unknown
    info.current_gain_tenths_db = None;
    info.current_gain_auto = Some(true);
    let c = client_info_to_c(&info, snapshot_at);
    assert!(!c.has_current_gain_value);
    assert!(c.has_current_gain_mode);
    assert!(c.current_gain_auto);

    // (Some(v), Some(manual)) → both set, explicit manual
    info.current_gain_tenths_db = Some(TEST_NONZERO_GAIN_TENTHS);
    info.current_gain_auto = Some(false);
    let c = client_info_to_c(&info, snapshot_at);
    assert!(c.has_current_gain_value);
    assert!(c.has_current_gain_mode);
    assert_eq!(c.current_gain_tenths_db, TEST_NONZERO_GAIN_TENTHS);
    assert!(!c.current_gain_auto);
}

#[test]
fn client_info_to_c_projects_last_command_fields() {
    // **Regression test for `CodeRabbit` round 6 on PR #402**
    // (initial projection) **+ round 7** (deterministic age
    // via injected `snapshot_at` — the function now takes
    // the snapshot clock as a parameter so per-entry drift
    // can't flip the "smallest age wins" ordering).
    //
    // `SdrRtlTcpClientInfo` carries
    // `(has_last_command, last_command_op, last_command_age_secs)`
    // so FFI hosts can replicate the Rust UI's
    // `pick_most_recent_commander` selection without parsing
    // every client's JSON ring. Verify the projection:
    //
    //   `ClientInfo.last_command = None`             → flag=false, op=0, age=0.0
    //   `ClientInfo.last_command = Some((op, at))`   → flag=true, op=op_byte, age=snapshot_at-at
    //
    // The `None` case is already covered by the default path
    // in `client_info_to_c_preserves_independent_gain_validity`;
    // this test pins the `Some` case — opcode byte maps to
    // the wire value, and the age is exactly the delta
    // between the injected `snapshot_at` and the dispatched
    // timestamp (measured in `f64` seconds).
    use sdr_server_rtltcp::codec::Codec;
    use sdr_server_rtltcp::protocol::CommandOp;
    let command_age = Duration::from_secs(TEST_COMMAND_AGE_SECS);
    let base = std::time::Instant::now();
    let dispatched_at = base
        .checked_sub(command_age)
        .expect("Instant::now - TEST_COMMAND_AGE_SECS is representable");
    // `snapshot_at = dispatched_at + command_age` gives an
    // age of *exactly* TEST_COMMAND_AGE_SECS, so the
    // assertion doesn't depend on wall-clock jitter.
    let snapshot_at = dispatched_at + command_age;
    let info = ClientInfo {
        id: TEST_CLIENT_ID,
        peer: SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_GAIN_PEER_PORT)),
        connected_since: std::time::Instant::now(),
        codec: Codec::None,
        role: sdr_server_rtltcp::extension::Role::Control,
        bytes_sent: 0,
        buffers_dropped: 0,
        // `SetBiasTee` (0x0e) chosen because it's the highest
        // documented opcode — a projection bug that truncates
        // to a smaller `u8` range would still surface here.
        last_command: Some((CommandOp::SetBiasTee, dispatched_at)),
        current_freq_hz: None,
        current_sample_rate_hz: None,
        current_gain_tenths_db: None,
        current_gain_auto: None,
        recent_commands: std::collections::VecDeque::new(),
    };
    let c = client_info_to_c(&info, snapshot_at);
    assert!(c.has_last_command);
    assert_eq!(c.last_command_op, CommandOp::SetBiasTee as u8);
    assert_eq!(c.last_command_op, 0x0e, "opcode wire byte");
    // Role projection (round 1 on PR #403): Control → 0.
    assert_eq!(
        c.role,
        sdr_server_rtltcp::extension::Role::Control.to_wire()
    );
    #[allow(
        clippy::cast_precision_loss,
        reason = "seconds count fits in f64 mantissa"
    )]
    let expected_age = TEST_COMMAND_AGE_SECS as f64;
    // Exact-equality float comparison is correct here
    // because `snapshot_at - dispatched_at` is a whole number
    // of seconds converted through `Duration::as_secs_f64` —
    // no accumulated arithmetic, no wall-clock jitter.
    #[allow(
        clippy::float_cmp,
        reason = "deterministic snapshot_at + exact-seconds command_age"
    )]
    let age_matches = c.last_command_age_secs == expected_age;
    assert!(
        age_matches,
        "expected age == {expected_age}s with injected snapshot_at, got {}s",
        c.last_command_age_secs
    );
}

#[test]
fn client_info_to_c_peer_addr_is_nul_terminated() {
    // Peer address is packed into a fixed-size byte array;
    // the slot past the written bytes must be NUL so C
    // callers see a well-formed string.
    use sdr_server_rtltcp::codec::Codec;
    let info = ClientInfo {
        id: 1,
        peer: SocketAddr::from((TEST_CLIENT_PEER_IP, TEST_CLIENT_PEER_PORT)),
        connected_since: std::time::Instant::now(),
        codec: Codec::None,
        role: sdr_server_rtltcp::extension::Role::Control,
        bytes_sent: 0,
        buffers_dropped: 0,
        last_command: None,
        current_freq_hz: None,
        current_sample_rate_hz: None,
        current_gain_tenths_db: None,
        current_gain_auto: None,
        recent_commands: std::collections::VecDeque::new(),
    };
    let c = client_info_to_c(&info, std::time::Instant::now());
    // Find the NUL byte and decode what's before. `c_char`
    // is `i8` on most platforms; reinterpret-cast the raw
    // bytes as u8 for the UTF-8 decode since ASCII is
    // layout-compatible across the signedness boundary.
    let peer_bytes: Vec<u8> = c.peer_addr.iter().map(|&b| b.to_ne_bytes()[0]).collect();
    let nul_pos = peer_bytes
        .iter()
        .position(|&b| b == 0)
        .expect("NUL terminator");
    let peer_str = std::str::from_utf8(&peer_bytes[..nul_pos]).unwrap();
    assert_eq!(peer_str, "192.168.1.100:1234");
    // Role projection (round 1 on PR #403): Control → 0.
    assert_eq!(
        c.role,
        sdr_server_rtltcp::extension::Role::Control.to_wire()
    );
}

#[test]
fn client_info_to_c_projects_listen_role() {
    // **Regression test for `CodeRabbit` round 1 on PR #403.**
    // The existing projection tests all use `Role::Control`
    // (the default for vanilla clients), so a bug that hard-
    // coded `role: 0` in `client_info_to_c` would pass
    // every test in this module. Flip a fixture to
    // `Role::Listen` and verify the wire byte flips to 1.
    use sdr_server_rtltcp::codec::Codec;
    let info = ClientInfo {
        id: TEST_CLIENT_ID,
        peer: SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_GAIN_PEER_PORT)),
        connected_since: std::time::Instant::now(),
        codec: Codec::None,
        role: sdr_server_rtltcp::extension::Role::Listen,
        bytes_sent: 0,
        buffers_dropped: 0,
        last_command: None,
        current_freq_hz: None,
        current_sample_rate_hz: None,
        current_gain_tenths_db: None,
        current_gain_auto: None,
        recent_commands: std::collections::VecDeque::new(),
    };
    let c = client_info_to_c(&info, std::time::Instant::now());
    assert_eq!(c.role, sdr_server_rtltcp::extension::Role::Listen.to_wire());
    assert_eq!(c.role, 1, "Listen wire byte");
}

#[test]
fn has_stopped_null_handle_returns_true() {
    assert!(unsafe { sdr_rtltcp_server_has_stopped(std::ptr::null_mut()) });
}

/// Sentinel byte the test buffers fill with — any non-NUL
/// value works; using the same constant avoids the
/// `u8 as c_char` cast-wrap lint.
const FILL_BYTE: c_char = 0x78; // ASCII 'x'

#[test]
fn write_cstr_truncates_without_overflow() {
    let mut buf = [FILL_BYTE; 5];
    // SAFETY: buffer is owned locally.
    unsafe { write_cstr(buf.as_mut_ptr(), buf.len(), "hello world") };
    // Expect "hell\0"
    assert_eq!(buf[4], 0);
    let s = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
    assert_eq!(s, "hell");
}

#[test]
fn write_cstr_null_or_zero_len_is_noop() {
    unsafe { write_cstr(std::ptr::null_mut(), 0, "hi") };
    unsafe { write_cstr(std::ptr::null_mut(), 5, "hi") };
    let mut buf = [FILL_BYTE; 1];
    unsafe { write_cstr(buf.as_mut_ptr(), 0, "hi") };
    assert_eq!(buf[0], FILL_BYTE);
}

/// Build a bare-bones `ClientInfo` fixture for the JSON
/// serialization tests. Callers mutate the `recent_commands`
/// field for their specific assertions.
fn empty_client_info() -> ClientInfo {
    use sdr_server_rtltcp::codec::Codec;
    ClientInfo {
        id: 1,
        peer: SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_JSON_PEER_PORT)),
        connected_since: std::time::Instant::now(),
        codec: Codec::None,
        role: sdr_server_rtltcp::extension::Role::Control,
        bytes_sent: 0,
        buffers_dropped: 0,
        last_command: None,
        current_freq_hz: None,
        current_sample_rate_hz: None,
        current_gain_tenths_db: None,
        current_gain_auto: None,
        recent_commands: std::collections::VecDeque::new(),
    }
}

#[test]
fn recent_commands_json_empty_when_no_commands() {
    let info = empty_client_info();
    let json = recent_commands_to_json(&info).expect("serialize empty ring");
    assert_eq!(json, "[]");
}

#[test]
fn recent_commands_json_entries_shape() {
    let mut info = empty_client_info();
    info.recent_commands
        .push_back((CommandOp::SetCenterFreq, std::time::Instant::now()));
    info.recent_commands.push_back((
        CommandOp::SetBiasTee,
        std::time::Instant::now()
            .checked_sub(Duration::from_secs(TEST_COMMAND_AGE_SECS))
            .expect("Instant::now - 3s is representable"),
    ));
    let json = recent_commands_to_json(&info).expect("serialize populated ring");
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["op"], "SetCenterFreq");
    assert_eq!(arr[1]["op"], "SetBiasTee");
    let seconds_ago = arr[1]["seconds_ago"].as_f64().unwrap();
    #[allow(
        clippy::cast_precision_loss,
        reason = "seconds count fits in f64 mantissa"
    )]
    let min_seconds_ago = TEST_COMMAND_AGE_SECS as f64;
    assert!(
        seconds_ago >= min_seconds_ago,
        "expected >={min_seconds_ago}s, got {seconds_ago}"
    );
}
