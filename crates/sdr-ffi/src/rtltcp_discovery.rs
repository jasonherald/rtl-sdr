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
    /// TXT: user-editable nickname. Optional — null or an
    /// empty C string means "no nickname." Host bindings that
    /// marked this as required were enforcing a stricter ABI
    /// than `sdr_rtltcp_advertiser_start` actually implements.
    /// Per `CodeRabbit` round 7 on PR #360.
    pub nickname: *const c_char,
    /// TXT: whether `txbuf` below is meaningful.
    pub has_txbuf: bool,
    /// TXT: optional buffer-depth hint in bytes.
    pub txbuf: u64,
    /// Whether [`Self::codecs`] below is meaningful. `false` (the
    /// zero-init default) publishes the TXT record with NO
    /// `codecs=` key, which every browser then treats as
    /// "legacy-only" — identical to pre-#400 behaviour. Added in
    /// ABI 0.19 per issue #400.
    pub has_codecs: bool,
    /// TXT: compression-mask wire byte. Only used when
    /// [`Self::has_codecs`] is `true`. Value is a [`CodecMask::
    /// to_wire`] byte — bit 0 = `None` codec, bit 1 = `LZ4`, etc.
    /// Typical values: `0x01` = `None`-only (legacy-safe),
    /// `0x03` = `None + LZ4` (signals the server speaks the
    /// extended handshake and accepts compression hellos).
    pub codecs: u8,
    /// Whether [`Self::auth_required`] below is meaningful.
    /// `false` (zero-init default) publishes no `auth_required=`
    /// key — per #395's "omit on false" contract, mDNS browsers
    /// treat absence as "no auth required." Added in ABI 0.19
    /// per issue #400.
    pub has_auth_required: bool,
    /// TXT: whether the server requires a pre-shared key (#394).
    /// Only used when [`Self::has_auth_required`] is `true`. The
    /// FFI advertiser is decoupled from the FFI server (hosts
    /// publish this independently of [`sdr_rtltcp_server_start`]),
    /// so it's the caller's responsibility to keep the flag in
    /// sync with its `ServerConfig.auth_key` state.
    pub auth_required: bool,
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

        // Reject port 0 at the boundary — a zero-initialized
        // `SdrRtlTcpAdvertiseOptions` would otherwise announce
        // on port 0 and succeed, giving browsers a server they
        // can never connect to. Mirrors the guard on
        // `sdr_core_set_network_config` and the network-sink
        // setter. Per `CodeRabbit` round 2 on PR #360.
        if opts.port == 0 {
            set_last_error("sdr_rtltcp_advertiser_start: port must be in 1..=65535, got 0");
            return SdrCoreError::InvalidArg.as_int();
        }

        let instance_name = match unsafe { cstr_to_string("instance_name", opts.instance_name) } {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => {
                set_last_error("sdr_rtltcp_advertiser_start: instance_name is empty");
                return SdrCoreError::InvalidArg.as_int();
            }
            Err(code) => return code.as_int(),
        };
        let hostname = match unsafe { optional_cstr_to_string("hostname", opts.hostname) } {
            Ok(v) => v.unwrap_or_default(),
            Err(code) => return code.as_int(),
        };
        // `tuner` / `version` are documented as required. Reject
        // empty strings alongside null so a caller can't publish
        // a discovery record with blank TXT metadata (browsers
        // would surface the server with "unknown" fields). Per
        // `CodeRabbit` round 5 on PR #360.
        let tuner = match unsafe { cstr_to_string("tuner", opts.tuner) } {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => {
                set_last_error("sdr_rtltcp_advertiser_start: tuner is empty");
                return SdrCoreError::InvalidArg.as_int();
            }
            Err(code) => return code.as_int(),
        };
        let version = match unsafe { cstr_to_string("version", opts.version) } {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => {
                set_last_error("sdr_rtltcp_advertiser_start: version is empty");
                return SdrCoreError::InvalidArg.as_int();
            }
            Err(code) => return code.as_int(),
        };
        let nickname = match unsafe { optional_cstr_to_string("nickname", opts.nickname) } {
            Ok(v) => v.unwrap_or_default(),
            Err(code) => return code.as_int(),
        };

        let txbuf = if opts.has_txbuf {
            // `TxtRecord::txbuf` is `Option<usize>`. `u64 → usize`
            // saturates to `usize::MAX` on 32-bit targets — a
            // buffer-depth hint over 4 GiB is already past any
            // sensible setting, so clamping is fine.
            Some(usize::try_from(opts.txbuf).unwrap_or(usize::MAX))
        } else {
            None
        };

        // `codecs` + `auth_required` project from the
        // `has_*` gates per #400. Zero-init defaults
        // (`has_codecs = has_auth_required = false`) yield the
        // same "omit from TXT" behaviour as pre-ABI-0.19, so
        // hosts that haven't updated their init still publish
        // legacy-compatible records.
        let codecs = if opts.has_codecs {
            Some(opts.codecs)
        } else {
            None
        };
        // `auth_required` MUST be `Some(true)` or `None` only —
        // never `Some(false)`. The #395 mDNS contract is "omit on
        // false" so clients that gate on key presence don't
        // misclassify an explicitly-no-auth server as
        // auth-capable. Per `CodeRabbit` round 1 on PR #418.
        let auth_required = if opts.has_auth_required && opts.auth_required {
            Some(true)
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
                codecs,
                auth_required,
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
    // Recover from mutex poisoning the same way
    // `rtltcp_server::lock_inner` does — `.ok()` would skip the
    // explicit `Advertiser::stop` entirely on a prior caught
    // panic, falling through to `Drop`. Per `CodeRabbit` round
    // 4 on PR #360.
    let taken = boxed
        .inner
        .lock()
        .unwrap_or_else(|poison| {
            tracing::warn!("sdr_rtltcp_advertiser_stop: mutex poisoned, recovering");
            poison.into_inner()
        })
        .take();
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
    /// Whether [`Self::codecs`] below carries meaningful data from
    /// the server's TXT record. `false` = server didn't advertise
    /// a `codecs=` key (legacy rtl_tcp or pre-ABI-0.19 sdr-rs
    /// server). ABI 0.19 per #400 / `CodeRabbit` round 1 on
    /// PR #418.
    pub has_codecs: bool,
    /// TXT: advertised compression-mask wire byte. Typical values:
    /// `0x01` = `None`-only (legacy-safe), `0x03` = `None + LZ4`
    /// (server accepts compression hellos).
    pub codecs: u8,
    /// Whether [`Self::auth_required`] below carries meaningful
    /// data. `false` = absent from TXT.
    pub has_auth_required: bool,
    /// TXT: whether the server requires a pre-shared key for
    /// handshakes (#394). Only meaningful when
    /// [`Self::has_auth_required`] is `true`.
    pub auth_required: bool,
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
///
/// **`user_data` is accessed from the discovery dispatcher
/// thread,** not the host's main thread. Any shared state the
/// callback reaches through `user_data` must be safe for
/// concurrent access (e.g. `Send + Sync`, guarded by a lock,
/// or an atomic) or externally synchronized by the host.
/// Per `CodeRabbit` round 8 on PR #360.
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
/// **Must NOT be called from inside the discovery callback.**
/// The callback runs on the dispatcher thread, and `_stop`
/// joins that thread — calling from the callback asks the
/// thread to join itself, which will panic/abort through this
/// `extern "C"` entrypoint. Hosts that want to stop in
/// response to a discovered server should marshal the call
/// out to another thread (GCD, Swift `Task`, a host-owned
/// channel). Per `CodeRabbit` round 7 on PR #360.
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
    // Poison recovery, same pattern as
    // `sdr_rtltcp_advertiser_stop` and `rtltcp_server`. Per
    // `CodeRabbit` round 4 on PR #360.
    let taken = boxed
        .inner
        .lock()
        .unwrap_or_else(|poison| {
            tracing::warn!("sdr_rtltcp_browser_stop: mutex poisoned, recovering");
            poison.into_inner()
        })
        .take();
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

    // `catch_unwind` here isolates the invocation frame but
    // cannot actually catch a panic from the host callback: a
    // Rust panic crossing an `extern "C"` ABI boundary is
    // defined to abort since Rust 1.81 unless the callback
    // uses `extern "C-unwind"`. In practice hosts are Swift or
    // C so they don't raise Rust panics at all — the guard is
    // defence-in-depth for a future Rust-side caller (tests,
    // in-process dispatch) that might panic inside the
    // callback's translation shim before the call crosses the
    // ABI. On actual cross-ABI panic the process aborts
    // before this `if` runs. Per `CodeRabbit` round 6 on PR
    // #360.
    let event_ptr: *const SdrRtlTcpDiscoveryEvent = &raw const c_event;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: host contract — cb + user_data are valid for
        // the browser's lifetime per the ABI docs.
        unsafe { cb(event_ptr, user_data) };
    }));
    if result.is_err() {
        tracing::warn!(
            "sdr-rtltcp-discovery callback panicked before the ABI crossing \
             (cross-ABI panics would already have aborted the process)"
        );
    }

    // Keep `strings` alive until here so the pointers the
    // callback saw remained valid throughout the dispatch.
    drop(strings);
}

fn discovered_server_to_c(
    server: &DiscoveredServer,
    strings: &mut Vec<CString>,
) -> SdrRtlTcpDiscoveredServer {
    // Borrow-taking shape + valid-empty-string fallback:
    //
    // - `&str` instead of `String` avoids a clone at every
    //   callsite (tuner / version / nickname / hostname /
    //   instance_name).
    // - On the CString-construction-failure path (unreachable
    //   after the NUL replace but still handled) we push an
    //   empty CString into the storage vec and return its
    //   pointer. Returning a raw null here would violate the
    //   "non-null C string" expectation every host's callback
    //   decodes with. Per `CodeRabbit` round 6 on PR #360.
    let push = |s: &str, strings: &mut Vec<CString>| -> *const c_char {
        let sanitized = s.replace('\0', "?");
        let cstr = CString::new(sanitized).unwrap_or_default();
        let ptr = cstr.as_ptr();
        strings.push(cstr);
        ptr
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

    // ABI 0.19 (#400): project the same `Option<...>` → `has_*` +
    // value pair shape as `txbuf` above so callers who check
    // `has_codecs=false` / `has_auth_required=false` see a clean
    // "absent from TXT" signal regardless of whatever stale bytes
    // happen to live in the value field. Per `CodeRabbit` round 1
    // on PR #418.
    let (has_codecs, codecs) = match server.txt.codecs {
        Some(v) => (true, v),
        None => (false, 0),
    };
    let (has_auth_required, auth_required) = match server.txt.auth_required {
        Some(v) => (true, v),
        None => (false, false),
    };

    SdrRtlTcpDiscoveredServer {
        instance_name: push(&server.instance_name, strings),
        hostname: push(&server.hostname, strings),
        port: server.port,
        address_ipv4: push(&ipv4, strings),
        address_ipv6: push(&ipv6, strings),
        tuner: push(&server.txt.tuner, strings),
        version: push(&server.txt.version, strings),
        gains: server.txt.gains,
        nickname: push(&server.txt.nickname, strings),
        has_txbuf,
        txbuf,
        last_seen_secs_ago,
        has_codecs,
        codecs,
        has_auth_required,
        auth_required,
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
        // ABI 0.19 defaults — zero-init to match the "absent from
        // TXT" signal.
        has_codecs: false,
        codecs: 0,
        has_auth_required: false,
        auth_required: false,
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

/// Optional-string helper: null → `Ok(None)`, empty → `Ok(None)`,
/// otherwise `Ok(Some(String))`. Invalid UTF-8 propagates as
/// `Err(SdrCoreError::InvalidArg)` — the earlier revision
/// silently dropped malformed input, which masked host bugs and
/// contradicted the documented UTF-8 contract on
/// `SdrRtlTcpAdvertiseOptions`. Per `CodeRabbit` round 2 on
/// PR #360.
///
/// Callers pass `field` so the last-error message can point at
/// the specific field that failed (`hostname` vs `nickname`
/// etc.).
///
/// # Safety
///
/// `ptr` must be null or a NUL-terminated UTF-8 C string.
unsafe fn optional_cstr_to_string(
    field: &str,
    ptr: *const c_char,
) -> Result<Option<String>, SdrCoreError> {
    if ptr.is_null() {
        return Ok(None);
    }
    // SAFETY: caller contract.
    let cstr = unsafe { std::ffi::CStr::from_ptr(ptr) };
    let s = cstr.to_str().map_err(|_| {
        set_last_error(format!(
            "sdr_rtltcp_advertiser_start: optional field `{field}` is not valid UTF-8"
        ));
        SdrCoreError::InvalidArg
    })?;
    if s.is_empty() {
        Ok(None)
    } else {
        Ok(Some(s.to_string()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
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
        let rc =
            unsafe { sdr_rtltcp_advertiser_start(&raw const fixture.opts, std::ptr::null_mut()) };
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
        let mut strings: Vec<CString> = Vec::new();
        // Present: codecs = 0x03 (None+LZ4), auth_required = true.
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
                codecs: Some(0x03),
                auth_required: Some(true),
            },
            last_seen: Instant::now(),
        };
        let projected = discovered_server_to_c(&present, &mut strings);
        assert!(projected.has_codecs);
        assert_eq!(projected.codecs, 0x03);
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
}
