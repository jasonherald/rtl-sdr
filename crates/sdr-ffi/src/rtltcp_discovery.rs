//! C ABI for the `rtl_tcp` mDNS discovery helpers (issue #325,
//! ABI 0.11). Wraps `sdr-rtltcp-discovery::{Advertiser, Browser}`.
//!
//! Two standalone opaque handle types:
//!
//! - `SdrRtlTcpAdvertiser` — one-shot registration on the LAN.
//!   Drop / stop unregisters.
//! - `SdrRtlTcpBrowser` — background listener with a
//!   host-registered callback fired from a dedicated thread
//!   for every `ServerAnnounced` / `ServerWithdrawn` event.
//!
//! Both live outside `SdrCore` because mDNS has no engine
//! coupling — the same machine can run the engine, advertise a
//! server, and browse for other servers independently.

use std::ffi::{CString, c_char, c_void};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

use sdr_rtltcp_discovery::{
    AdvertiseOptions, Advertiser, Browser, DiscoveredServer, DiscoveryEvent, TxtRecord,
};

use crate::error::{SdrCoreError, clear_last_error, set_last_error};
use crate::lifecycle::panic_message;

// ============================================================
//  Discovery event discriminants
// ============================================================

pub const SDR_RTLTCP_DISCOVERY_ANNOUNCED: i32 = 0;
pub const SDR_RTLTCP_DISCOVERY_WITHDRAWN: i32 = 1;

// ============================================================
//  Advertiser
// ============================================================

/// C-layout advertise options. String pointers must be
/// NUL-terminated UTF-8 (or null for optional fields). The
/// FFI copies strings into owned Rust storage before calling
/// into the `sdr-rtltcp-discovery` crate, so callers may free
/// these immediately after `sdr_rtltcp_advertiser_start`
/// returns.
#[repr(C)]
pub struct SdrRtlTcpAdvertiseOptions {
    pub port: u16,
    /// Required, must be non-null, non-empty.
    pub instance_name: *const c_char,
    /// Optional — null / empty = auto-derive from the local
    /// hostname.
    pub hostname: *const c_char,
    /// TXT: tuner name (e.g. `"R820T"`). Required.
    pub tuner: *const c_char,
    /// TXT: advertiser version string. Required.
    pub version: *const c_char,
    /// TXT: discrete gain-step count.
    pub gains: u32,
    /// TXT: user-editable nickname. Required — caller can pass
    /// `""` to skip, the Rust side treats empty as "no nickname."
    pub nickname: *const c_char,
    /// TXT: whether `txbuf` below is meaningful.
    pub has_txbuf: bool,
    /// TXT: optional buffer-depth hint in bytes.
    pub txbuf: u64,
}

pub struct SdrRtlTcpAdvertiser {
    inner: Mutex<Option<Advertiser>>,
}

impl SdrRtlTcpAdvertiser {
    fn new(advertiser: Advertiser) -> Self {
        Self {
            inner: Mutex::new(Some(advertiser)),
        }
    }
}

/// Start an rtl_tcp mDNS advertisement. On success writes the
/// handle to `*out_handle` and returns `SDR_CORE_OK`. On
/// failure returns a negative error code and leaves
/// `*out_handle` untouched.
///
/// # Safety
///
/// `opts` and `out_handle` must be non-null. String fields on
/// `opts` must be NUL-terminated UTF-8 C strings (or null where
/// the field's docs allow it).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_advertiser_start(
    opts: *const SdrRtlTcpAdvertiseOptions,
    out_handle: *mut *mut SdrRtlTcpAdvertiser,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        if opts.is_null() || out_handle.is_null() {
            set_last_error("sdr_rtltcp_advertiser_start: null opts or out_handle");
            return SdrCoreError::InvalidArg.as_int();
        }
        // SAFETY: caller contract.
        let opts = unsafe { &*opts };

        let instance_name = match unsafe { cstr_to_string("instance_name", opts.instance_name) } {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => {
                set_last_error("sdr_rtltcp_advertiser_start: instance_name is empty");
                return SdrCoreError::InvalidArg.as_int();
            }
            Err(code) => return code.as_int(),
        };
        let hostname = unsafe { optional_cstr_to_string(opts.hostname) }.unwrap_or_default();
        let tuner = match unsafe { cstr_to_string("tuner", opts.tuner) } {
            Ok(s) => s,
            Err(code) => return code.as_int(),
        };
        let version = match unsafe { cstr_to_string("version", opts.version) } {
            Ok(s) => s,
            Err(code) => return code.as_int(),
        };
        let nickname = unsafe { optional_cstr_to_string(opts.nickname) }.unwrap_or_default();

        let txbuf = if opts.has_txbuf {
            // `TxtRecord::txbuf` is `Option<usize>`. `u64 → usize`
            // saturates to `usize::MAX` on 32-bit targets — a
            // buffer-depth hint over 4 GiB is already past any
            // sensible setting, so clamping is fine.
            Some(usize::try_from(opts.txbuf).unwrap_or(usize::MAX))
        } else {
            None
        };

        let options = AdvertiseOptions {
            port: opts.port,
            instance_name,
            hostname,
            txt: TxtRecord {
                tuner,
                version,
                gains: opts.gains,
                nickname,
                txbuf,
            },
        };

        match Advertiser::announce(options) {
            Ok(adv) => {
                let handle = Box::new(SdrRtlTcpAdvertiser::new(adv));
                // SAFETY: `out_handle` null-checked above.
                unsafe { *out_handle = Box::into_raw(handle) };
                clear_last_error();
                SdrCoreError::Ok.as_int()
            }
            Err(e) => {
                set_last_error(format!("sdr_rtltcp_advertiser_start: {e}"));
                SdrCoreError::Io.as_int()
            }
        }
    });
    match result {
        Ok(code) => code,
        Err(payload) => {
            set_last_error(format!(
                "sdr_rtltcp_advertiser_start: panic: {}",
                panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

/// Unregister and release the advertisement. Passing null is
/// a no-op.
///
/// # Safety
///
/// `handle` must be either null or a pointer previously returned
/// by `sdr_rtltcp_advertiser_start` and not already passed here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_advertiser_stop(handle: *mut SdrRtlTcpAdvertiser) {
    if handle.is_null() {
        return;
    }
    let boxed = unsafe { Box::from_raw(handle) };
    let taken = boxed.inner.lock().ok().and_then(|mut g| g.take());
    if let Some(adv) = taken {
        // The Rust `stop` can return an error; we've committed
        // to releasing regardless. Log on failure so the host
        // sees something in the tracing stream.
        if let Err(e) = adv.stop() {
            tracing::warn!("sdr_rtltcp_advertiser_stop: {e}");
        }
    }
}

// ============================================================
//  Browser
// ============================================================

/// Borrowed-for-callback-duration snapshot of a discovered
/// server. Pointer fields point into CString storage owned by
/// the translation frame — valid only during the call.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrRtlTcpDiscoveredServer {
    pub instance_name: *const c_char,
    pub hostname: *const c_char,
    pub port: u16,
    /// First IPv4 address as a dotted-quad string, or `""` if
    /// none resolved.
    pub address_ipv4: *const c_char,
    /// First IPv6 address (without `%iface` suffix), or `""`
    /// if none resolved.
    pub address_ipv6: *const c_char,
    /// TXT: tuner name.
    pub tuner: *const c_char,
    /// TXT: advertiser version.
    pub version: *const c_char,
    /// TXT: discrete gain-step count.
    pub gains: u32,
    /// TXT: nickname (may be empty).
    pub nickname: *const c_char,
    /// TXT: whether `txbuf` below is meaningful.
    pub has_txbuf: bool,
    /// TXT: buffer-depth hint.
    pub txbuf: u64,
    /// Seconds since the last ServiceResolved for this entry.
    pub last_seen_secs_ago: f64,
}

/// Tagged discovery event the browser dispatches.
/// `announced` is meaningful when `kind == SDR_RTLTCP_DISCOVERY_ANNOUNCED`;
/// `withdrawn_instance_name` is meaningful when
/// `kind == SDR_RTLTCP_DISCOVERY_WITHDRAWN`.
#[repr(C)]
pub struct SdrRtlTcpDiscoveryEvent {
    pub kind: i32,
    pub announced: SdrRtlTcpDiscoveredServer,
    pub withdrawn_instance_name: *const c_char,
}

pub type SdrRtlTcpDiscoveryCallback =
    Option<unsafe extern "C" fn(event: *const SdrRtlTcpDiscoveryEvent, user_data: *mut c_void)>;

pub struct SdrRtlTcpBrowser {
    inner: Mutex<Option<Browser>>,
}

/// Opaque `user_data` wrapper so the closure captured by
/// `Browser::start` satisfies `Send + 'static`. The host
/// contract says `user_data` must outlive the browser, so
/// crossing threads is their responsibility — we assert `Send`
/// here without actually synchronizing anything.
struct BrowserUserData(*mut c_void);
// SAFETY: Host contract in `sdr_rtltcp_browser_start`.
unsafe impl Send for BrowserUserData {}

/// Start a browser that invokes `callback` for every observed
/// server-announce / withdraw event. The callback fires on a
/// dedicated thread (`rtl_tcp-discovery-browser`), NOT the
/// host's main thread — hosts must marshal UI updates across
/// themselves. Same contract as `sdr_core_set_event_callback`.
///
/// Returns `SDR_CORE_OK` + handle on success. On failure
/// returns `SDR_CORE_ERR_INVALID_ARG` (null callback / out
/// handle) or `SDR_CORE_ERR_IO` (mDNS daemon failed to start).
///
/// # Safety
///
/// `out_handle` must be non-null. `user_data` is opaque to the
/// FFI — the caller owns its lifetime and must ensure it
/// remains valid from `_start` through `_stop`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_browser_start(
    callback: SdrRtlTcpDiscoveryCallback,
    user_data: *mut c_void,
    out_handle: *mut *mut SdrRtlTcpBrowser,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        if out_handle.is_null() {
            set_last_error("sdr_rtltcp_browser_start: null out_handle");
            return SdrCoreError::InvalidArg.as_int();
        }
        let Some(cb) = callback else {
            set_last_error("sdr_rtltcp_browser_start: null callback");
            return SdrCoreError::InvalidArg.as_int();
        };

        // Wrap `*mut c_void` in the module-level
        // `BrowserUserData` newtype so the closure captured by
        // `Browser::start` is `Send + 'static`.
        let user_data = BrowserUserData(user_data);

        match Browser::start(move |event| {
            let ud = &user_data;
            dispatch_discovery_event(event, cb, ud.0);
        }) {
            Ok(browser) => {
                let handle = Box::new(SdrRtlTcpBrowser {
                    inner: Mutex::new(Some(browser)),
                });
                // SAFETY: null-checked above.
                unsafe { *out_handle = Box::into_raw(handle) };
                clear_last_error();
                SdrCoreError::Ok.as_int()
            }
            Err(e) => {
                set_last_error(format!("sdr_rtltcp_browser_start: {e}"));
                SdrCoreError::Io.as_int()
            }
        }
    });
    match result {
        Ok(code) => code,
        Err(payload) => {
            set_last_error(format!(
                "sdr_rtltcp_browser_start: panic: {}",
                panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

/// Stop the browser and release the handle. Joins the
/// dispatcher thread before returning, so the host may
/// deterministically free `user_data` on the next line.
/// Passing null is a no-op.
///
/// # Safety
///
/// `handle` must be either null or a pointer previously
/// returned by `sdr_rtltcp_browser_start` and not already
/// passed here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_browser_stop(handle: *mut SdrRtlTcpBrowser) {
    if handle.is_null() {
        return;
    }
    let boxed = unsafe { Box::from_raw(handle) };
    let taken = boxed.inner.lock().ok().and_then(|mut g| g.take());
    if let Some(browser) = taken {
        browser.stop();
    }
}

// ============================================================
//  Translation helpers
// ============================================================

/// Translate a `DiscoveryEvent` into the C-layout struct + own
/// the backing CStrings for the duration of the callback.
fn dispatch_discovery_event(
    event: DiscoveryEvent,
    cb: unsafe extern "C" fn(event: *const SdrRtlTcpDiscoveryEvent, user_data: *mut c_void),
    user_data: *mut c_void,
) {
    // Owned storage outlives the callback call. The Vec here
    // keeps every CString alive until this function returns.
    let mut strings: Vec<CString> = Vec::new();

    let c_event = match event {
        DiscoveryEvent::ServerAnnounced(server) => {
            let announced = discovered_server_to_c(&server, &mut strings);
            SdrRtlTcpDiscoveryEvent {
                kind: SDR_RTLTCP_DISCOVERY_ANNOUNCED,
                announced,
                withdrawn_instance_name: std::ptr::null(),
            }
        }
        DiscoveryEvent::ServerWithdrawn { instance_name } => {
            let sanitized = instance_name.replace('\0', "?");
            let Ok(cstr) = CString::new(sanitized) else {
                return;
            };
            let ptr = cstr.as_ptr();
            strings.push(cstr);
            SdrRtlTcpDiscoveryEvent {
                kind: SDR_RTLTCP_DISCOVERY_WITHDRAWN,
                announced: zeroed_discovered_server(),
                withdrawn_instance_name: ptr,
            }
        }
    };

    // Wrap the host callback in `catch_unwind` — a panic
    // across the FFI boundary is UB, and mDNS events should
    // never tear down the browser.
    let event_ptr: *const SdrRtlTcpDiscoveryEvent = &raw const c_event;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: host contract — cb + user_data are valid for
        // the browser's lifetime per the ABI docs.
        unsafe { cb(event_ptr, user_data) };
    }));
    if result.is_err() {
        tracing::warn!("sdr-rtltcp-discovery callback panicked (payload swallowed)");
    }

    // Keep `strings` alive until here so the pointers the
    // callback saw remained valid throughout the dispatch.
    drop(strings);
}

fn discovered_server_to_c(
    server: &DiscoveredServer,
    strings: &mut Vec<CString>,
) -> SdrRtlTcpDiscoveredServer {
    let push = |s: String, strings: &mut Vec<CString>| -> *const c_char {
        let sanitized = s.replace('\0', "?");
        match CString::new(sanitized) {
            Ok(cstr) => {
                let ptr = cstr.as_ptr();
                strings.push(cstr);
                ptr
            }
            // Unreachable — replace() stripped NULs. Fall back
            // to an empty string pointer rather than dropping
            // the whole event.
            Err(_) => std::ptr::null(),
        }
    };

    let ipv4 = server
        .addresses
        .iter()
        .find(|a| matches!(a, IpAddr::V4(_)))
        .map(ToString::to_string)
        .unwrap_or_default();
    let ipv6 = server
        .addresses
        .iter()
        .find(|a| matches!(a, IpAddr::V6(_)))
        .map(ToString::to_string)
        .unwrap_or_default();

    let last_seen_secs_ago = Instant::now()
        .saturating_duration_since(server.last_seen)
        .as_secs_f64();

    let (has_txbuf, txbuf) = match server.txt.txbuf {
        Some(v) => (true, v as u64),
        None => (false, 0),
    };

    SdrRtlTcpDiscoveredServer {
        instance_name: push(server.instance_name.clone(), strings),
        hostname: push(server.hostname.clone(), strings),
        port: server.port,
        address_ipv4: push(ipv4, strings),
        address_ipv6: push(ipv6, strings),
        tuner: push(server.txt.tuner.clone(), strings),
        version: push(server.txt.version.clone(), strings),
        gains: server.txt.gains,
        nickname: push(server.txt.nickname.clone(), strings),
        has_txbuf,
        txbuf,
        last_seen_secs_ago,
    }
}

fn zeroed_discovered_server() -> SdrRtlTcpDiscoveredServer {
    SdrRtlTcpDiscoveredServer {
        instance_name: std::ptr::null(),
        hostname: std::ptr::null(),
        port: 0,
        address_ipv4: std::ptr::null(),
        address_ipv6: std::ptr::null(),
        tuner: std::ptr::null(),
        version: std::ptr::null(),
        gains: 0,
        nickname: std::ptr::null(),
        has_txbuf: false,
        txbuf: 0,
        last_seen_secs_ago: 0.0,
    }
}

/// Required-string helper: null → `InvalidArg`, non-UTF-8 →
/// `InvalidArg`, otherwise owned `String`.
///
/// # Safety
///
/// `ptr` must be null or a NUL-terminated UTF-8 C string.
unsafe fn cstr_to_string(field: &str, ptr: *const c_char) -> Result<String, SdrCoreError> {
    if ptr.is_null() {
        set_last_error(format!(
            "sdr_rtltcp_advertiser_start: required field `{field}` is null"
        ));
        return Err(SdrCoreError::InvalidArg);
    }
    // SAFETY: caller contract.
    let cstr = unsafe { std::ffi::CStr::from_ptr(ptr) };
    cstr.to_str().map(str::to_string).map_err(|_| {
        set_last_error(format!(
            "sdr_rtltcp_advertiser_start: field `{field}` is not valid UTF-8"
        ));
        SdrCoreError::InvalidArg
    })
}

/// Optional-string helper: null → None, empty → None,
/// otherwise `Some(String)`. Invalid UTF-8 also degrades to None
/// rather than failing the whole call — these fields are
/// non-critical (hostname auto-derives; nickname falls back to
/// empty).
///
/// # Safety
///
/// `ptr` must be null or a NUL-terminated UTF-8 C string.
unsafe fn optional_cstr_to_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller contract.
    let cstr = unsafe { std::ffi::CStr::from_ptr(ptr) };
    let s = cstr.to_str().ok()?.to_string();
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn advertiser_start_null_options_returns_invalid_arg() {
        let mut handle: *mut SdrRtlTcpAdvertiser = std::ptr::null_mut();
        let rc = unsafe { sdr_rtltcp_advertiser_start(std::ptr::null(), &raw mut handle) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }

    #[test]
    fn advertiser_start_null_out_handle_returns_invalid_arg() {
        let instance = CString::new("x").unwrap();
        let tuner = CString::new("R820T").unwrap();
        let version = CString::new("0.1.0").unwrap();
        let opts = SdrRtlTcpAdvertiseOptions {
            port: 1234,
            instance_name: instance.as_ptr(),
            hostname: std::ptr::null(),
            tuner: tuner.as_ptr(),
            version: version.as_ptr(),
            gains: 29,
            nickname: std::ptr::null(),
            has_txbuf: false,
            txbuf: 0,
        };
        let rc = unsafe { sdr_rtltcp_advertiser_start(&raw const opts, std::ptr::null_mut()) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }

    #[test]
    fn advertiser_start_empty_instance_name_rejected() {
        let empty = CString::new("").unwrap();
        let tuner = CString::new("R820T").unwrap();
        let version = CString::new("0.1.0").unwrap();
        let opts = SdrRtlTcpAdvertiseOptions {
            port: 1234,
            instance_name: empty.as_ptr(),
            hostname: std::ptr::null(),
            tuner: tuner.as_ptr(),
            version: version.as_ptr(),
            gains: 0,
            nickname: std::ptr::null(),
            has_txbuf: false,
            txbuf: 0,
        };
        let mut handle: *mut SdrRtlTcpAdvertiser = std::ptr::null_mut();
        let rc = unsafe { sdr_rtltcp_advertiser_start(&raw const opts, &raw mut handle) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
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
        let rc = unsafe {
            sdr_rtltcp_browser_start(Some(cb), std::ptr::null_mut(), std::ptr::null_mut())
        };
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
    }

    #[test]
    fn discovered_server_to_c_picks_ipv4_first() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        let server = DiscoveredServer {
            instance_name: "test._rtl_tcp._tcp.local.".into(),
            hostname: "test.local.".into(),
            port: 1234,
            addresses: vec![
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 43)),
            ],
            txt: TxtRecord {
                tuner: "R820T".into(),
                version: "0.1.0".into(),
                gains: 29,
                nickname: "dev".into(),
                txbuf: Some(65536),
            },
            last_seen: Instant::now(),
        };
        let mut strings = Vec::new();
        let c = discovered_server_to_c(&server, &mut strings);
        assert!(!c.address_ipv4.is_null());
        let ipv4 = unsafe { std::ffi::CStr::from_ptr(c.address_ipv4) }
            .to_str()
            .unwrap();
        assert_eq!(ipv4, "192.168.1.42");
        assert!(!c.address_ipv6.is_null());
        assert!(c.has_txbuf);
        assert_eq!(c.txbuf, 65536);
    }

    #[test]
    fn discovered_server_to_c_empty_ipv4_when_only_ipv6() {
        use std::net::Ipv6Addr;
        let server = DiscoveredServer {
            instance_name: "test._rtl_tcp._tcp.local.".into(),
            hostname: "test.local.".into(),
            port: 1234,
            addresses: vec![IpAddr::V6(Ipv6Addr::LOCALHOST)],
            txt: TxtRecord {
                tuner: "R820T".into(),
                version: "0.1.0".into(),
                gains: 29,
                nickname: String::new(),
                txbuf: None,
            },
            last_seen: Instant::now(),
        };
        let mut strings = Vec::new();
        let c = discovered_server_to_c(&server, &mut strings);
        let ipv4 = unsafe { std::ffi::CStr::from_ptr(c.address_ipv4) }
            .to_str()
            .unwrap();
        assert_eq!(ipv4, "");
        assert!(!c.has_txbuf);
    }
}
