use super::*;
use std::ffi::CString;

// --------------------------------------------------------
//  Shared test fixtures — per `CodeRabbit` round 5 on
//  PR #360. Hoisted out of per-test literals so future
//  fixture changes don't require spot-patching every case.
// --------------------------------------------------------

/// Non-privileged port used when the value isn't the thing
/// under test.
const TEST_PORT: u16 = 1234;

/// Tuner name advertised in the happy-path fixtures —
/// matches the R820T strings the upstream rtl_tcp servers
/// publish.
const TEST_TUNER: &str = "R820T";

/// Advertiser-version string; deliberately a non-empty
/// placeholder so the "empty required field" tests are a
/// clean contrast.
const TEST_VERSION: &str = "0.1.0";

/// Discrete gain-step count the R820T tuner exposes.
const TEST_GAIN_COUNT: u32 = 29;

/// TXT buffer-depth hint used by the `DiscoveredServer → C`
/// projection tests. 64 KiB is the value the sample server
/// in `sdr-server-rtltcp` reports.
const TEST_TXBUF_BYTES: u64 = 65_536;

/// Short instance name for round-trip checks. Real servers
/// compose hostname + nickname; the tests just need a
/// unique non-empty string.
const TEST_INSTANCE_NAME: &str = "test-instance";

/// Build a happy-path `SdrRtlTcpAdvertiseOptions` backed by
/// a bundle of `CString`s the caller keeps alive for the
/// duration of the FFI call. Tests tweak individual fields
/// (e.g. flip `port` to 0, null out `instance_name`) after
/// construction to target a specific validation branch.
struct AdvertiseFixture {
    // Keep the CStrings alive so the pointers stored on
    // `opts` remain valid. The struct must outlive `opts`.
    _instance: CString,
    _tuner: CString,
    _version: CString,
    opts: SdrRtlTcpAdvertiseOptions,
}

impl AdvertiseFixture {
    fn happy_path() -> Self {
        let instance = CString::new(TEST_INSTANCE_NAME).unwrap();
        let tuner = CString::new(TEST_TUNER).unwrap();
        let version = CString::new(TEST_VERSION).unwrap();
        let opts = SdrRtlTcpAdvertiseOptions {
            port: TEST_PORT,
            instance_name: instance.as_ptr(),
            hostname: std::ptr::null(),
            tuner: tuner.as_ptr(),
            version: version.as_ptr(),
            gains: TEST_GAIN_COUNT,
            nickname: std::ptr::null(),
            has_txbuf: false,
            txbuf: 0,
            // ABI 0.19 defaults — zero-init equivalent so the
            // base fixture matches pre-#400 behaviour. Tests
            // that exercise the new fields mutate these after
            // `happy_path()` returns.
            has_codecs: false,
            codecs: 0,
            has_auth_required: false,
            auth_required: false,
        };
        Self {
            _instance: instance,
            _tuner: tuner,
            _version: version,
            opts,
        }
    }
}

#[test]
fn advertiser_start_null_options_returns_invalid_arg() {
    let mut handle: *mut SdrRtlTcpAdvertiser = std::ptr::null_mut();
    let rc = unsafe { sdr_rtltcp_advertiser_start(std::ptr::null(), &raw mut handle) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
}

#[test]
fn advertiser_start_null_out_handle_returns_invalid_arg() {
    let fixture = AdvertiseFixture::happy_path();
    let rc = unsafe { sdr_rtltcp_advertiser_start(&raw const fixture.opts, std::ptr::null_mut()) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
}

#[test]
fn advertiser_start_port_zero_rejected() {
    // Pins the port-0 guard added in round 2 per
    // `CodeRabbit` — a zero-init `SdrRtlTcpAdvertiseOptions`
    // must not slip through and announce on port 0.
    let mut fixture = AdvertiseFixture::happy_path();
    fixture.opts.port = 0;
    let mut handle: *mut SdrRtlTcpAdvertiser = std::ptr::null_mut();
    let rc = unsafe { sdr_rtltcp_advertiser_start(&raw const fixture.opts, &raw mut handle) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    assert!(handle.is_null());
}

#[test]
fn advertiser_start_empty_instance_name_rejected() {
    let empty = CString::new("").unwrap();
    let mut fixture = AdvertiseFixture::happy_path();
    fixture.opts.instance_name = empty.as_ptr();
    let mut handle: *mut SdrRtlTcpAdvertiser = std::ptr::null_mut();
    let rc = unsafe { sdr_rtltcp_advertiser_start(&raw const fixture.opts, &raw mut handle) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
}

#[test]
fn advertiser_start_empty_tuner_rejected() {
    // Per `CodeRabbit` round 5 — `tuner` is documented as
    // required; empty-string must be rejected so the
    // discovery record never publishes blank TXT metadata.
    let empty = CString::new("").unwrap();
    let mut fixture = AdvertiseFixture::happy_path();
    fixture.opts.tuner = empty.as_ptr();
    let mut handle: *mut SdrRtlTcpAdvertiser = std::ptr::null_mut();
    let rc = unsafe { sdr_rtltcp_advertiser_start(&raw const fixture.opts, &raw mut handle) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
}

#[test]
fn advertiser_start_empty_version_rejected() {
    let empty = CString::new("").unwrap();
    let mut fixture = AdvertiseFixture::happy_path();
    fixture.opts.version = empty.as_ptr();
    let mut handle: *mut SdrRtlTcpAdvertiser = std::ptr::null_mut();
    let rc = unsafe { sdr_rtltcp_advertiser_start(&raw const fixture.opts, &raw mut handle) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
}

#[test]
fn advertiser_start_invalid_utf8_hostname_rejected() {
    // Pins `optional_cstr_to_string`'s "propagate UTF-8
    // errors" behavior added in round 2. A non-UTF-8
    // optional field must fail with `InvalidArg` rather
    // than be silently dropped. Per `CodeRabbit` round 8
    // on PR #360.
    //
    // Build a lone 0xFF byte + NUL via `from_vec_with_nul`
    // so the CStr underlying pointer has length 1 of
    // invalid UTF-8.
    let bad = CString::from_vec_with_nul(vec![0xFF, 0]).unwrap();
    let mut fixture = AdvertiseFixture::happy_path();
    fixture.opts.hostname = bad.as_ptr();
    let mut handle: *mut SdrRtlTcpAdvertiser = std::ptr::null_mut();
    let rc = unsafe { sdr_rtltcp_advertiser_start(&raw const fixture.opts, &raw mut handle) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    assert!(handle.is_null());
}

#[test]
fn advertiser_start_invalid_utf8_nickname_rejected() {
    // Same shape as the hostname case — the second optional
    // field also propagates UTF-8 errors.
    let bad = CString::from_vec_with_nul(vec![0xFE, 0]).unwrap();
    let mut fixture = AdvertiseFixture::happy_path();
    fixture.opts.nickname = bad.as_ptr();
    let mut handle: *mut SdrRtlTcpAdvertiser = std::ptr::null_mut();
    let rc = unsafe { sdr_rtltcp_advertiser_start(&raw const fixture.opts, &raw mut handle) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    assert!(handle.is_null());
}

#[test]
fn advertiser_stop_handles_null() {
    unsafe { sdr_rtltcp_advertiser_stop(std::ptr::null_mut()) };
}

#[test]
fn browser_start_null_callback_returns_invalid_arg() {
    let mut handle: *mut SdrRtlTcpBrowser = std::ptr::null_mut();
    let rc = unsafe { sdr_rtltcp_browser_start(None, std::ptr::null_mut(), &raw mut handle) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
}

#[test]
fn browser_start_null_out_handle_returns_invalid_arg() {
    unsafe extern "C" fn cb(_e: *const SdrRtlTcpDiscoveryEvent, _u: *mut c_void) {}
    let rc =
        unsafe { sdr_rtltcp_browser_start(Some(cb), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
}

#[test]
fn browser_stop_handles_null() {
    unsafe { sdr_rtltcp_browser_stop(std::ptr::null_mut()) };
}

#[test]
fn zeroed_discovered_server_has_null_pointers() {
    let z = zeroed_discovered_server();
    assert!(z.instance_name.is_null());
    assert!(z.hostname.is_null());
    assert!(z.address_ipv4.is_null());
    assert!(z.address_ipv6.is_null());
    assert!(z.nickname.is_null());
    assert_eq!(z.port, 0);
    assert_eq!(z.gains, 0);
    assert!(!z.has_txbuf);
    // ABI 0.19 (#400) — the new capability-field gates must
    // default to `false` too so "absent from TXT" is the
    // zero-init semantic.
    assert!(!z.has_codecs);
    assert!(!z.has_auth_required);
}

#[test]
fn discovered_server_to_c_projects_codecs_and_auth_required() {
    // ABI 0.19 contract: `Some(v)` on the Rust side projects
    // to `(true, v)`; `None` projects to `(false, 0 /
    // false)`. Pin both directions so a future parser change
    // that silently drops TXT capability bits fails here.
    //
    // `TEST_CODEC_MASK_NONE_AND_LZ4` names the `0x03` wire
    // byte (None + LZ4 bits) per the project's
    // "no-magic-numbers" rule. Per `CodeRabbit` round 2 on
    // PR #418.
    const TEST_CODEC_MASK_NONE_AND_LZ4: u8 = 0x03;

    let mut strings: Vec<CString> = Vec::new();
    // Present: None+LZ4 codecs + auth required.
    let present = DiscoveredServer {
        instance_name: format!("{TEST_INSTANCE_NAME}._rtl_tcp._tcp.local."),
        hostname: "test.local.".into(),
        port: TEST_PORT,
        addresses: vec![],
        txt: TxtRecord {
            tuner: TEST_TUNER.into(),
            version: TEST_VERSION.into(),
            gains: TEST_GAIN_COUNT,
            nickname: String::new(),
            txbuf: None,
            codecs: Some(TEST_CODEC_MASK_NONE_AND_LZ4),
            auth_required: Some(true),
        },
        last_seen: Instant::now(),
    };
    let projected = discovered_server_to_c(&present, &mut strings);
    assert!(projected.has_codecs);
    assert_eq!(projected.codecs, TEST_CODEC_MASK_NONE_AND_LZ4);
    assert!(projected.has_auth_required);
    assert!(projected.auth_required);

    // Absent: neither TXT key present.
    let absent = DiscoveredServer {
        instance_name: format!("{TEST_INSTANCE_NAME}._rtl_tcp._tcp.local."),
        hostname: "test.local.".into(),
        port: TEST_PORT,
        addresses: vec![],
        txt: TxtRecord {
            tuner: TEST_TUNER.into(),
            version: TEST_VERSION.into(),
            gains: TEST_GAIN_COUNT,
            nickname: String::new(),
            txbuf: None,
            codecs: None,
            auth_required: None,
        },
        last_seen: Instant::now(),
    };
    let projected = discovered_server_to_c(&absent, &mut strings);
    assert!(!projected.has_codecs);
    assert_eq!(projected.codecs, 0);
    assert!(!projected.has_auth_required);
    assert!(!projected.auth_required);
}

/// Build a `DiscoveredServer` with the TXT fields wired to
/// the shared `TEST_*` constants, caller-supplied addresses,
/// and `last_seen = now`. Keeps the test cases focused on
/// the piece they actually exercise (address preference,
/// txbuf presence, etc.).
fn sample_discovered_server(addresses: Vec<IpAddr>, txbuf: Option<usize>) -> DiscoveredServer {
    DiscoveredServer {
        instance_name: format!("{TEST_INSTANCE_NAME}._rtl_tcp._tcp.local."),
        hostname: "test.local.".into(),
        port: TEST_PORT,
        addresses,
        txt: TxtRecord {
            tuner: TEST_TUNER.into(),
            version: TEST_VERSION.into(),
            gains: TEST_GAIN_COUNT,
            nickname: if txbuf.is_some() {
                "dev".into()
            } else {
                String::new()
            },
            txbuf,
            codecs: None,
            auth_required: None,
        },
        last_seen: Instant::now(),
    }
}

#[test]
fn discovered_server_to_c_picks_ipv4_first() {
    use std::net::{Ipv4Addr, Ipv6Addr};
    let server = sample_discovered_server(
        vec![
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 43)),
        ],
        // `u64 → usize` would truncate on 32-bit targets;
        // use the same saturating conversion as the FFI
        // translation path.
        Some(usize::try_from(TEST_TXBUF_BYTES).unwrap_or(usize::MAX)),
    );
    let mut strings = Vec::new();
    let c = discovered_server_to_c(&server, &mut strings);
    assert!(!c.address_ipv4.is_null());
    let ipv4 = unsafe { std::ffi::CStr::from_ptr(c.address_ipv4) }
        .to_str()
        .unwrap();
    assert_eq!(ipv4, "192.168.1.42");
    assert!(!c.address_ipv6.is_null());
    assert!(c.has_txbuf);
    assert_eq!(c.txbuf, TEST_TXBUF_BYTES);
}

#[test]
fn discovered_server_to_c_empty_ipv4_when_only_ipv6() {
    use std::net::Ipv6Addr;
    let server = sample_discovered_server(vec![IpAddr::V6(Ipv6Addr::LOCALHOST)], None);
    let mut strings = Vec::new();
    let c = discovered_server_to_c(&server, &mut strings);
    let ipv4 = unsafe { std::ffi::CStr::from_ptr(c.address_ipv4) }
        .to_str()
        .unwrap();
    assert_eq!(ipv4, "");
    assert!(!c.has_txbuf);
}
