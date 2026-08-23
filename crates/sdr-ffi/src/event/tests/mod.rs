use super::*;
use crate::error::SdrCoreError;
use crate::handle::SdrCore;
use crate::lifecycle::{sdr_core_create, sdr_core_destroy};
use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};

fn make_handle() -> *mut SdrCore {
    let path = CString::new("").unwrap();
    let mut handle: *mut SdrCore = std::ptr::null_mut();
    let rc = unsafe { sdr_core_create(path.as_ptr(), &raw mut handle) };
    assert_eq!(rc, SdrCoreError::Ok.as_int());
    handle
}

// Top-of-module dummy callbacks. Rust lints (clippy's
// `items_after_statements`) complains when these are defined
// inside a test function body.
unsafe extern "C" fn noop_cb(_event: *const SdrEvent, _user_data: *mut c_void) {}

// Shared atomic for the counting callback test below.
// Each test has its own static to avoid cross-test
// contamination in parallel runs.
static DISPATCH_COUNTER: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn counting_cb(_event: *const SdrEvent, _user_data: *mut c_void) {
    DISPATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
}

// ------------------------------------------------------
//  Stateless construction of the event struct itself.
//  These don't need a real engine.
// ------------------------------------------------------

/// Fixture name for the latched-channel scanner tests. NOAA
/// Weather Radio is a canonical NFM channel with a recognisable
/// string, making debug output easy to read. Per `CodeRabbit`
/// round 2 on PR #497.
const TEST_SCANNER_NAME: &str = "NOAA Weather";
/// Fixture frequency paired with `TEST_SCANNER_NAME` — 162.550
/// MHz is the NOAA Weather Radio canonical channel. Same
/// constant appears in the command-side tests under the same
/// name, but keep them crate-private-per-test-module so a
/// future refactor that splits the modules doesn't collide.
const TEST_SCANNER_FREQ_HZ: u64 = 162_550_000;
/// Fixture bandwidth the active-channel event carries in the
/// flattened `bandwidth` field. NFM default — the FFI layer
/// doesn't read this field, so the exact value is only
/// meaningful as a recognisable placeholder in debug dumps.
const TEST_SCANNER_BANDWIDTH_HZ: f64 = 12_500.0;
/// Interior-NUL fixture for the sanitization test. The
/// dispatcher's `replace('\0', '?')` call should turn this
/// into "Weather?Channel" before it reaches the callback.
const TEST_SCANNER_NAME_WITH_INTERIOR_NUL: &str = "Weather\0Channel";
/// Expected output after the dispatcher sanitizes the
/// interior NUL in `TEST_SCANNER_NAME_WITH_INTERIOR_NUL`.
const TEST_SCANNER_NAME_SANITIZED: &str = "Weather?Channel";

// ------------------------------------------------------
//  translate_event — network sink status (ABI 0.9, #247)
//
//  Direct Rust-side coverage of the three NetworkSinkStatus
//  arms, including NULL vs non-NULL string cases and the
//  `Protocol::TcpClient` → `SDR_NETWORK_PROTOCOL_TCP_SERVER`
//  name-bridge. Locks the contract in before Swift decoding
//  sees it. Per `CodeRabbit` round 1 on PR #352.
// ------------------------------------------------------

mod callbacks;
mod drop_policy;
mod network_sink;
mod rtl_tcp;
mod scanner;
