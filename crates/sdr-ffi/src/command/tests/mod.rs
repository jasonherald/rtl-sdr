use super::*;
use crate::lifecycle::sdr_core_create;
use std::ffi::CString;

/// Small grace period the round-trip recording tests wait
/// after `stop` so the controller thread has time to drop
/// the writer (which finalizes the WAV header on `Drop`)
/// before the test cleans up the file. Sub-second so the
/// test suite stays fast; large enough to comfortably cover
/// the mpsc hop plus file-close syscall on any CI host.
const RECORDING_FLUSH_WAIT_MS: u64 = 50;

/// Loopback host string reused across the network-sink
/// setter tests. Plain IPv4 loopback avoids any resolver
/// step so the tests don't depend on `/etc/hosts` entries
/// or DNS availability on the CI host.
const TEST_NETWORK_HOST_LOOPBACK: &str = "127.0.0.1";

/// Default port the network-sink defaults advertise (see
/// `sdr_core::sink_slot::DEFAULT_NETWORK_SINK_PORT`). Used
/// in the TCP-path happy case. Named so a future default
/// change flows through the tests.
const TEST_NETWORK_PORT_TCP: u16 = 1234;

/// A second distinct port for the UDP-path happy case —
/// keeps the two setter tests exercising different values
/// so a silently-ignored parameter won't pass both by
/// coincidence.
const TEST_NETWORK_PORT_UDP: u16 = 9000;

/// Smallest legal UDP / TCP port (port 0 is reserved at
/// the ABI boundary — see the zero-port rejection test).
/// Named so the "minimum accepted" assertion expresses
/// intent instead of using a bare `1`. Per `CodeRabbit`
/// round 3 on PR #352.
const TEST_NETWORK_MIN_VALID_PORT: u16 = 1;

/// Lowest legal gain index (`gains()` slot 0). Covers the
/// "minimum valid index" boundary for the gain-by-index
/// round-trip test.
const TEST_GAIN_INDEX_BASELINE: u32 = 0;

/// A representative "large but plausible" gain index. The
/// R820T tuner advertises 29 discrete gain steps, so index
/// 28 is the last legal slot — used to exercise a non-zero
/// value without hardcoding a magic literal. Per
/// `CodeRabbit` round 3 on PR #360.
const TEST_GAIN_INDEX_REPRESENTATIVE_HIGH: u32 = 28;

/// Minimum size of a well-formed empty WAV file: the 44-byte
/// header `WavWriter::new` writes before any samples arrive
/// (RIFF/WAVE + fmt chunk + data chunk header). Used by the
/// round-trip recording tests to prove the controller
/// actually opened + wrote the header, not just enqueued
/// the command.
const WAV_HEADER_BYTES: u64 = 44;

/// Build a collision-resistant temp WAV path. PID alone would
/// reuse the same filename across reruns of the same test
/// binary — if a prior run crashed before its cleanup, a
/// stale file could mask a broken `_start_*_recording` by
/// making the `metadata().expect(...)` assertion pass against
/// the old artifact. Adding a nanosecond timestamp gives each
/// test a unique name even when `cargo test` reuses a binary.
/// Per CodeRabbit round 4 on PR #345.
/// A `.wav` path inside a fresh private temp directory; the caller
/// keeps the `TempDir` alive for the test's duration.
fn unique_temp_wav(prefix: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("temp dir");
    let path = dir.path().join("recording.wav");
    (dir, path)
}

/// Helper: make a live engine handle for the duration of a test.
fn make_handle() -> *mut SdrCore {
    let path = CString::new("").unwrap();
    let mut handle: *mut SdrCore = std::ptr::null_mut();
    let rc = unsafe { sdr_core_create(path.as_ptr(), &raw mut handle) };
    assert_eq!(rc, SdrCoreError::Ok.as_int());
    assert!(!handle.is_null());
    handle
}

fn destroy(handle: *mut SdrCore) {
    unsafe { crate::lifecycle::sdr_core_destroy(handle) };
}

// ------------------------------------------------------
//  Handle validation
// ------------------------------------------------------

// ------------------------------------------------------
//  Audio routing + recording (ABI 0.4)
// ------------------------------------------------------

/// Forwards `SDR_EVT_ERROR` messages to the `mpsc::Sender<String>`
/// passed as `user_data`; every other event kind is ignored.
unsafe extern "C" fn error_capture_cb(
    event: *const crate::event::SdrEvent,
    user_data: *mut std::ffi::c_void,
) {
    // SAFETY: `event` is valid for the duration of the callback
    // (dispatcher contract) and `user_data` is the boxed sender
    // this test registered and keeps alive until unregistration.
    unsafe {
        let event = &*event;
        if event.kind != crate::event::SDR_EVT_ERROR {
            return;
        }
        let msg = std::ffi::CStr::from_ptr(event.payload.error.utf8)
            .to_string_lossy()
            .into_owned();
        let tx = &*user_data.cast::<std::sync::mpsc::Sender<String>>();
        let _ = tx.send(msg);
    }
}

// ------------------------------------------------------
//  Squelch — auto-squelch toggle (ABI 0.3)
// ------------------------------------------------------

// ------------------------------------------------------
//  Lifecycle commands
// ------------------------------------------------------

// ------------------------------------------------------
//  Tuning
// ------------------------------------------------------

// ------------------------------------------------------
//  Enum translation
// ------------------------------------------------------

// ------------------------------------------------------
//  Volume clamping
// ------------------------------------------------------

// ------------------------------------------------------
//  FFT controls
// ------------------------------------------------------

// ------------------------------------------------------
//  Advanced demod (ABI 0.7) — regression tests for the
//  argument contracts established by the `NB_LEVEL_MIN`
//  and `NOTCH_FREQUENCY_MIN_HZ_EXCLUSIVE` constants. Per
//  CodeRabbit round 1 on PR #347.
// ------------------------------------------------------

// ------------------------------------------------------
//  rtl_tcp-specific client commands (#325, ABI 0.11)
// ------------------------------------------------------

// ------------------------------------------------------
//  Source selection (#235, #236, ABI 0.10)
// ------------------------------------------------------

// ------------------------------------------------------
//  AGC type selector (#357, ABI 0.13)
// ------------------------------------------------------

// ------------------------------------------------------
//  Audio sink selection (#247, ABI 0.9)
// ------------------------------------------------------

// ------------------------------------------------------
//  Scanner — ABI 0.20, issue #447
// ------------------------------------------------------

/// Fixture frequency reused across all four scanner
/// command tests. 162.550 MHz is the NOAA Weather Radio
/// canonical frequency — a realistic NFM channel rather
/// than a round number, so a naive default-0 in either
/// side of the lockout path would be obvious in logs.
/// Per `CodeRabbit` round 2 on PR #497.
const TEST_SCANNER_FREQ_HZ: u64 = 162_550_000;

// ------------------------------------------------------
//  update_scanner_channels — ABI 0.22, issue #490
// ------------------------------------------------------

/// Fixture bandwidth used by the channel-projection tests.
/// 12.5 kHz is the canonical NFM voice channel width and
/// matches the `bandwidth` field on the actual NOAA-Weather
/// fixture name in use elsewhere in this module.
const TEST_SCANNER_BW_HZ: f64 = 12_500.0;
/// Fixture dwell — ms the host would resolve from the
/// scanner's default-dwell setting once per-bookmark
/// overrides are folded in. Just needs to be a finite,
/// realistic value for the projection-side tests.
const TEST_SCANNER_DWELL_MS: u32 = 100;
/// Fixture hang — same rationale as `TEST_SCANNER_DWELL_MS`.
const TEST_SCANNER_HANG_MS: u32 = 2_000;

mod lifecycle_recording;
mod scanner;
mod sink;
mod source;
mod tuning_demod;
