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
fn unique_temp_wav(prefix: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!("{prefix}-{}-{nonce}.wav", std::process::id()))
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

#[test]
fn all_commands_reject_null_handle() {
    // Spot-check: if with_core is broken, every command would
    // dereference null. Picking a representative sample.
    assert_eq!(
        unsafe { sdr_core_start(std::ptr::null_mut()) },
        SdrCoreError::InvalidHandle.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_tune(std::ptr::null_mut(), 100_000_000.0) },
        SdrCoreError::InvalidHandle.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_demod_mode(std::ptr::null_mut(), SDR_DEMOD_NFM) },
        SdrCoreError::InvalidHandle.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_auto_squelch(std::ptr::null_mut(), true) },
        SdrCoreError::InvalidHandle.as_int()
    );
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe { sdr_core_set_audio_device(std::ptr::null_mut(), empty.as_ptr()) },
        SdrCoreError::InvalidHandle.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_start_audio_recording(std::ptr::null_mut(), empty.as_ptr()) },
        SdrCoreError::InvalidHandle.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_stop_audio_recording(std::ptr::null_mut()) },
        SdrCoreError::InvalidHandle.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_start_iq_recording(std::ptr::null_mut(), empty.as_ptr()) },
        SdrCoreError::InvalidHandle.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_stop_iq_recording(std::ptr::null_mut()) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

// ------------------------------------------------------
//  Audio routing + recording (ABI 0.4)
// ------------------------------------------------------

#[test]
fn set_audio_device_accepts_empty_string_for_default() {
    let h = make_handle();
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe { sdr_core_set_audio_device(h, empty.as_ptr()) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_audio_device_rejects_null_string() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_audio_device(h, std::ptr::null()) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn start_audio_recording_rejects_null_or_empty_path() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_start_audio_recording(h, std::ptr::null()) },
        SdrCoreError::InvalidArg.as_int()
    );
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe { sdr_core_start_audio_recording(h, empty.as_ptr()) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn audio_recording_start_stop_round_trip() {
    // Write to a temp file path so the controller's WavWriter
    // has somewhere it can open. We verify the controller
    // actually created + finalized the WAV header — a
    // controller-side open failure would otherwise pass
    // silently here even though `send_command` returned OK.
    let h = make_handle();
    let tmp = unique_temp_wav("sdr-ffi-test");
    let path = CString::new(tmp.to_string_lossy().into_owned()).unwrap();
    assert_eq!(
        unsafe { sdr_core_start_audio_recording(h, path.as_ptr()) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_stop_audio_recording(h) },
        SdrCoreError::Ok.as_int()
    );
    // Give the controller a moment to process both commands
    // and drop the writer (Drop finalizes the WAV header).
    std::thread::sleep(std::time::Duration::from_millis(RECORDING_FLUSH_WAIT_MS));
    let metadata =
        std::fs::metadata(&tmp).expect("audio recording should create a WAV file before cleanup");
    assert!(
        metadata.len() >= WAV_HEADER_BYTES,
        "audio recording should finalize at least a WAV header"
    );
    std::fs::remove_file(&tmp).unwrap();
    destroy(h);
}

#[test]
fn start_iq_recording_rejects_null_or_empty_path() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_start_iq_recording(h, std::ptr::null()) },
        SdrCoreError::InvalidArg.as_int()
    );
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe { sdr_core_start_iq_recording(h, empty.as_ptr()) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

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

#[test]
fn iq_recording_without_source_is_rejected() {
    // #695 — the IQ WAV header bakes in the source sample rate,
    // which is only authoritative while a source is open. A
    // headless handle has no source, so the controller must
    // refuse to open a writer: the FFI call itself still returns
    // Ok (`send_command` is asynchronous) and the rejection
    // surfaces as an `SDR_EVT_ERROR` event. Previously (PR #345)
    // this round-trip expected a finalized header on disk.
    const ERROR_EVENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const EXPECTED_ERROR: &str = "IQ record failed: press Play before recording IQ";

    let h = make_handle();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let user_data = Box::into_raw(Box::new(tx)).cast::<std::ffi::c_void>();
    assert_eq!(
        unsafe { crate::event::sdr_core_set_event_callback(h, Some(error_capture_cb), user_data) },
        SdrCoreError::Ok.as_int()
    );

    let tmp = unique_temp_wav("sdr-ffi-iq-test");
    let path = CString::new(tmp.to_string_lossy().into_owned()).unwrap();
    assert_eq!(
        unsafe { sdr_core_start_iq_recording(h, path.as_ptr()) },
        SdrCoreError::Ok.as_int()
    );
    let msg = rx
        .recv_timeout(ERROR_EVENT_TIMEOUT)
        .expect("controller must emit SDR_EVT_ERROR for the rejected IQ recording");
    assert_eq!(msg, EXPECTED_ERROR);
    assert_eq!(
        unsafe { sdr_core_stop_iq_recording(h) },
        SdrCoreError::Ok.as_int()
    );
    assert!(
        std::fs::metadata(&tmp).is_err(),
        "IQ recording must not open a WAV file while no source is running"
    );

    // Unregister before freeing the sender: the setter waits for
    // in-flight dispatches, so no callback can observe a dangling
    // `user_data` afterwards.
    assert_eq!(
        unsafe { crate::event::sdr_core_set_event_callback(h, None, std::ptr::null_mut()) },
        SdrCoreError::Ok.as_int()
    );
    // SAFETY: `user_data` came from `Box::into_raw` above and no
    // callback can run after unregistration.
    drop(unsafe { Box::from_raw(user_data.cast::<std::sync::mpsc::Sender<String>>()) });
    destroy(h);
}

// ------------------------------------------------------
//  Squelch — auto-squelch toggle (ABI 0.3)
// ------------------------------------------------------

#[test]
fn set_auto_squelch_round_trip() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_auto_squelch(h, true) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_auto_squelch(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

// ------------------------------------------------------
//  Lifecycle commands
// ------------------------------------------------------

#[test]
fn start_stop_round_trip() {
    let h = make_handle();
    assert_eq!(unsafe { sdr_core_start(h) }, SdrCoreError::Ok.as_int());
    assert_eq!(unsafe { sdr_core_stop(h) }, SdrCoreError::Ok.as_int());
    destroy(h);
}

// ------------------------------------------------------
//  Tuning
// ------------------------------------------------------

#[test]
fn tune_accepts_reasonable_frequency() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_tune(h, 100_700_000.0) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn tune_rejects_nan_and_inf() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_tune(h, f64::NAN) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_tune(h, f64::INFINITY) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_sample_rate_rejects_non_positive() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_sample_rate(h, 0.0) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_sample_rate(h, -1.0) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_decimation_rejects_non_power_of_two() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_decimation(h, 0) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_decimation(h, 3) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_decimation(h, 8) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

// ------------------------------------------------------
//  Enum translation
// ------------------------------------------------------

#[test]
fn demod_mode_c_to_rust_covers_all_variants() {
    assert_eq!(demod_mode_from_c(SDR_DEMOD_WFM), Some(DemodMode::Wfm));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_NFM), Some(DemodMode::Nfm));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_AM), Some(DemodMode::Am));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_USB), Some(DemodMode::Usb));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_LSB), Some(DemodMode::Lsb));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_DSB), Some(DemodMode::Dsb));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_CW), Some(DemodMode::Cw));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_RAW), Some(DemodMode::Raw));
    assert_eq!(demod_mode_from_c(99), None);
    assert_eq!(demod_mode_from_c(-1), None);
}

#[test]
fn deemphasis_c_to_rust_covers_all_variants() {
    assert_eq!(
        deemphasis_from_c(SDR_DEEMPH_NONE),
        Some(DeemphasisMode::None)
    );
    assert_eq!(
        deemphasis_from_c(SDR_DEEMPH_US75),
        Some(DeemphasisMode::Us75)
    );
    assert_eq!(
        deemphasis_from_c(SDR_DEEMPH_EU50),
        Some(DeemphasisMode::Eu50)
    );
    assert_eq!(deemphasis_from_c(99), None);
}

#[test]
fn fft_window_c_to_rust_covers_all_variants() {
    assert_eq!(
        fft_window_from_c(SDR_FFT_WIN_RECT),
        Some(FftWindow::Rectangular)
    );
    assert_eq!(
        fft_window_from_c(SDR_FFT_WIN_BLACKMAN),
        Some(FftWindow::Blackman)
    );
    assert_eq!(
        fft_window_from_c(SDR_FFT_WIN_NUTTALL),
        Some(FftWindow::Nuttall)
    );
    assert_eq!(fft_window_from_c(99), None);
}

#[test]
fn set_demod_mode_rejects_unknown_value() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_demod_mode(h, 99) },
        SdrCoreError::InvalidArg.as_int()
    );
    // And accepts valid ones.
    assert_eq!(
        unsafe { sdr_core_set_demod_mode(h, SDR_DEMOD_WFM) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

// ------------------------------------------------------
//  Volume clamping
// ------------------------------------------------------

#[test]
fn set_volume_clamps_out_of_range() {
    // Clamping is internal — the engine receives the clamped
    // value and accepts it. We can't directly observe the
    // clamped value from the FFI side without hooking the
    // event channel, so just prove the call succeeds for
    // out-of-range inputs.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_volume(h, -1.0) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_volume(h, 2.0) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_volume(h, 0.5) },
        SdrCoreError::Ok.as_int()
    );
    // NaN is rejected (not finite).
    assert_eq!(
        unsafe { sdr_core_set_volume(h, f32::NAN) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

// ------------------------------------------------------
//  FFT controls
// ------------------------------------------------------

#[test]
fn set_fft_size_rejects_non_power_of_two() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, 0) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, 1000) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, 2048) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_fft_size_rejects_values_above_max() {
    // Guards against a host passing usize::MAX (or, on
    // Swift, a sign-cast of a negative Int) and tripping
    // an unbounded allocation in rustfft. The boundary is
    // a power of two so the "not a power of two" check
    // wouldn't catch it.
    let h = make_handle();

    // MAX_FFT_SIZE itself must be accepted.
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, super::MAX_FFT_SIZE) },
        SdrCoreError::Ok.as_int()
    );

    // 2 * MAX_FFT_SIZE is a power of two but over the cap.
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, super::MAX_FFT_SIZE * 2) },
        SdrCoreError::InvalidArg.as_int()
    );

    // usize::MAX isn't a power of two, so it already gets
    // caught by the earlier check — but the upper-bound
    // check is defense in depth. Pick a large power of two
    // that's over the cap to exercise the new arm.
    let large_power_of_two: usize = 1 << 30; // 1 GiB worth of bins
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, large_power_of_two) },
        SdrCoreError::InvalidArg.as_int()
    );

    destroy(h);
}

// ------------------------------------------------------
//  Advanced demod (ABI 0.7) — regression tests for the
//  argument contracts established by the `NB_LEVEL_MIN`
//  and `NOTCH_FREQUENCY_MIN_HZ_EXCLUSIVE` constants. Per
//  CodeRabbit round 1 on PR #347.
// ------------------------------------------------------

#[test]
fn set_nb_level_accepts_at_minimum_and_rejects_below() {
    let h = make_handle();
    // Exactly at the minimum must be accepted — the engine
    // treats `1.0` as "no clipping margin," which is the
    // lower edge of the usable range.
    assert_eq!(
        unsafe { sdr_core_set_nb_level(h, NB_LEVEL_MIN) },
        SdrCoreError::Ok.as_int()
    );
    // Any value below minimum must be rejected.
    assert_eq!(
        unsafe { sdr_core_set_nb_level(h, NB_LEVEL_MIN - 0.0001) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_nb_level(h, 0.0) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_nb_level(h, -1.0) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_nb_level_rejects_nan_and_infinity() {
    let h = make_handle();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            unsafe { sdr_core_set_nb_level(h, bad) },
            SdrCoreError::InvalidArg.as_int(),
            "nb_level must reject {bad}"
        );
    }
    destroy(h);
}

#[test]
fn set_notch_frequency_accepts_positive_rejects_nonpositive() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_notch_frequency(h, 1_000.0) },
        SdrCoreError::Ok.as_int()
    );
    // Exactly at the exclusive lower bound must be rejected.
    assert_eq!(
        unsafe { sdr_core_set_notch_frequency(h, NOTCH_FREQUENCY_MIN_HZ_EXCLUSIVE) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_notch_frequency(h, -50.0) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_notch_frequency_rejects_nan_and_infinity() {
    let h = make_handle();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            unsafe { sdr_core_set_notch_frequency(h, bad) },
            SdrCoreError::InvalidArg.as_int(),
            "notch_frequency must reject {bad}"
        );
    }
    destroy(h);
}

// ------------------------------------------------------
//  rtl_tcp-specific client commands (#325, ABI 0.11)
// ------------------------------------------------------

#[test]
fn set_bias_tee_round_trips() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_bias_tee(h, true) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_bias_tee(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_direct_sampling_accepts_valid_modes() {
    let h = make_handle();
    for mode in SDR_DIRECT_SAMPLING_MIN..=SDR_DIRECT_SAMPLING_MAX {
        assert_eq!(
            unsafe { sdr_core_set_direct_sampling(h, mode) },
            SdrCoreError::Ok.as_int(),
            "direct-sampling mode {mode} should be accepted"
        );
    }
    destroy(h);
}

#[test]
fn set_direct_sampling_rejects_out_of_range() {
    // Reference the MIN/MAX constants so the test stays in
    // sync with the FFI contract instead of hardcoding
    // boundary literals that could drift. Per `CodeRabbit`
    // round 1 on PR #360.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_direct_sampling(h, SDR_DIRECT_SAMPLING_MAX + 1) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_direct_sampling(h, SDR_DIRECT_SAMPLING_MIN - 1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_offset_tuning_round_trips() {
    // Exercise both polarities so a future regression that
    // silently breaks one branch is caught. Per `CodeRabbit`
    // round 3 on PR #360.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_offset_tuning(h, true) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_offset_tuning(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_rtl_agc_round_trips() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_rtl_agc(h, true) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_rtl_agc(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_gain_by_index_accepts_nonnegative_indices() {
    // Engine bounds-checks against the source's `gains()`
    // length (or the rtl_tcp advertised gain_count); a bad
    // index becomes a `DspToUi::Error` event. The FFI
    // itself accepts any u32 — we're testing the dispatch
    // path here, not the bounds check.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_gain_by_index(h, TEST_GAIN_INDEX_BASELINE) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_gain_by_index(h, TEST_GAIN_INDEX_REPRESENTATIVE_HIGH) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn rtl_tcp_disconnect_dispatches() {
    // The engine drops `DisconnectRtlTcp` with a warn log
    // when the active source isn't rtl_tcp — our test
    // fixture builds a default core which starts on the
    // RTL-SDR source, so we're exercising the FFI dispatch
    // path + engine guard, not the actual socket teardown.
    // That's the right scope for a unit test; the socket
    // path is covered by `sdr-source-network` tests.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_rtl_tcp_disconnect(h) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn rtl_tcp_retry_now_dispatches() {
    // Same coverage as the disconnect test — the engine
    // drops the message with a warn log outside rtl_tcp;
    // we're checking that the FFI return code is `Ok`
    // when the dispatch succeeds. Per issue #326.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_rtl_tcp_retry_now(h) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn rtl_tcp_disconnect_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_rtl_tcp_disconnect(std::ptr::null_mut()) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

#[test]
fn rtl_tcp_retry_now_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_rtl_tcp_retry_now(std::ptr::null_mut()) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

// ------------------------------------------------------
//  Source selection (#235, #236, ABI 0.10)
// ------------------------------------------------------

#[test]
fn source_type_from_c_covers_all_variants() {
    assert_eq!(
        source_type_from_c(SDR_SOURCE_RTLSDR),
        Some(SourceType::RtlSdr)
    );
    assert_eq!(
        source_type_from_c(SDR_SOURCE_NETWORK),
        Some(SourceType::Network)
    );
    assert_eq!(source_type_from_c(SDR_SOURCE_FILE), Some(SourceType::File));
    assert_eq!(
        source_type_from_c(SDR_SOURCE_RTLTCP),
        Some(SourceType::RtlTcp)
    );
    assert_eq!(source_type_from_c(99), None);
    assert_eq!(source_type_from_c(-1), None);
}

#[test]
fn source_protocol_from_c_covers_all_variants() {
    assert_eq!(
        source_protocol_from_c(SDR_SOURCE_PROTOCOL_TCP),
        Some(Protocol::TcpClient)
    );
    assert_eq!(
        source_protocol_from_c(SDR_SOURCE_PROTOCOL_UDP),
        Some(Protocol::Udp)
    );
    assert_eq!(source_protocol_from_c(99), None);
}

#[test]
fn set_source_type_accepts_all_variants() {
    let h = make_handle();
    for t in [
        SDR_SOURCE_RTLSDR,
        SDR_SOURCE_NETWORK,
        SDR_SOURCE_FILE,
        SDR_SOURCE_RTLTCP,
    ] {
        assert_eq!(
            unsafe { sdr_core_set_source_type(h, t) },
            SdrCoreError::Ok.as_int(),
            "source type {t} should be accepted"
        );
    }
    destroy(h);
}

#[test]
fn set_source_type_rejects_out_of_range_value() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_source_type(h, 99) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_source_type(h, -1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_config_accepts_valid_input() {
    let h = make_handle();
    let host = CString::new(TEST_NETWORK_HOST_LOOPBACK).unwrap();
    assert_eq!(
        unsafe {
            sdr_core_set_network_config(
                h,
                host.as_ptr(),
                TEST_NETWORK_PORT_TCP,
                SDR_SOURCE_PROTOCOL_TCP,
            )
        },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe {
            sdr_core_set_network_config(
                h,
                host.as_ptr(),
                TEST_NETWORK_PORT_UDP,
                SDR_SOURCE_PROTOCOL_UDP,
            )
        },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_config_rejects_null_and_empty_hostname() {
    let h = make_handle();
    assert_eq!(
        unsafe {
            sdr_core_set_network_config(
                h,
                std::ptr::null(),
                TEST_NETWORK_PORT_TCP,
                SDR_SOURCE_PROTOCOL_TCP,
            )
        },
        SdrCoreError::InvalidArg.as_int()
    );
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe {
            sdr_core_set_network_config(
                h,
                empty.as_ptr(),
                TEST_NETWORK_PORT_TCP,
                SDR_SOURCE_PROTOCOL_TCP,
            )
        },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_config_rejects_zero_port_and_unknown_protocol() {
    let h = make_handle();
    let host = CString::new(TEST_NETWORK_HOST_LOOPBACK).unwrap();
    assert_eq!(
        unsafe { sdr_core_set_network_config(h, host.as_ptr(), 0, SDR_SOURCE_PROTOCOL_TCP) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_network_config(h, host.as_ptr(), TEST_NETWORK_PORT_TCP, 99) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_network_config(h, host.as_ptr(), TEST_NETWORK_PORT_TCP, -1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_file_path_accepts_valid_path() {
    let h = make_handle();
    let path = CString::new("/tmp/some-iq.wav").unwrap();
    assert_eq!(
        unsafe { sdr_core_set_file_path(h, path.as_ptr()) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_file_path_rejects_null_and_empty() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_file_path(h, std::ptr::null()) },
        SdrCoreError::InvalidArg.as_int()
    );
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe { sdr_core_set_file_path(h, empty.as_ptr()) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_file_looping_round_trips() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_file_looping(h, true) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_file_looping(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_file_looping_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_set_file_looping(std::ptr::null_mut(), true) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

// ------------------------------------------------------
//  AGC type selector (#357, ABI 0.13)
// ------------------------------------------------------

// Legacy bool entry point — pins the tristate-forwarding
// semantics so a future refactor can't silently break
// pre-0.13 hosts. Per `CodeRabbit` round 2 on PR #371.
#[test]
fn set_agc_legacy_bool_round_trips() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_agc(h, true) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_agc(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_agc_legacy_bool_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_set_agc(std::ptr::null_mut(), false) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

#[test]
fn set_agc_type_accepts_valid_variants() {
    let h = make_handle();
    for t in [SDR_AGC_OFF, SDR_AGC_HARDWARE, SDR_AGC_SOFTWARE] {
        assert_eq!(
            unsafe { sdr_core_set_agc_type(h, t) },
            SdrCoreError::Ok.as_int(),
            "AGC type {t} should be accepted"
        );
    }
    destroy(h);
}

#[test]
fn set_agc_type_rejects_out_of_range() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_agc_type(h, SDR_AGC_SOFTWARE + 1) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_agc_type(h, SDR_AGC_OFF - 1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_agc_type_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_set_agc_type(std::ptr::null_mut(), SDR_AGC_OFF) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

// ------------------------------------------------------
//  Audio sink selection (#247, ABI 0.9)
// ------------------------------------------------------

#[test]
fn set_audio_sink_type_accepts_both_variants() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_audio_sink_type(h, SDR_AUDIO_SINK_LOCAL) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_audio_sink_type(h, SDR_AUDIO_SINK_NETWORK) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_audio_sink_type_rejects_out_of_range_value() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_audio_sink_type(h, 99) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_audio_sink_type(h, -1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_sink_config_accepts_valid_input() {
    let h = make_handle();
    let host = CString::new(TEST_NETWORK_HOST_LOOPBACK).unwrap();
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                host.as_ptr(),
                TEST_NETWORK_PORT_TCP,
                crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER,
            )
        },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                host.as_ptr(),
                TEST_NETWORK_PORT_UDP,
                crate::event::SDR_NETWORK_PROTOCOL_UDP,
            )
        },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_sink_config_rejects_null_hostname() {
    let h = make_handle();
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                std::ptr::null(),
                TEST_NETWORK_PORT_TCP,
                crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER,
            )
        },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_sink_config_rejects_empty_hostname() {
    let h = make_handle();
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                empty.as_ptr(),
                TEST_NETWORK_PORT_TCP,
                crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER,
            )
        },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_sink_config_rejects_zero_port() {
    // Port 0 is rejected at the ABI boundary — UDP would
    // silently drop to a bogus destination and TCP server
    // mode would bind an undiscoverable ephemeral port.
    // Per `CodeRabbit` round 2 on PR #352.
    let h = make_handle();
    let host = CString::new(TEST_NETWORK_HOST_LOOPBACK).unwrap();
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                host.as_ptr(),
                0,
                crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER,
            )
        },
        SdrCoreError::InvalidArg.as_int()
    );
    // And accepts the minimum legal port as a boundary check.
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                host.as_ptr(),
                TEST_NETWORK_MIN_VALID_PORT,
                crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER,
            )
        },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_sink_config_rejects_out_of_range_protocol() {
    let h = make_handle();
    let host = CString::new(TEST_NETWORK_HOST_LOOPBACK).unwrap();
    assert_eq!(
        unsafe { sdr_core_set_network_sink_config(h, host.as_ptr(), TEST_NETWORK_PORT_TCP, 99) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_network_sink_config(h, host.as_ptr(), TEST_NETWORK_PORT_TCP, -1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn audio_sink_type_from_c_covers_all_variants() {
    assert_eq!(
        audio_sink_type_from_c(SDR_AUDIO_SINK_LOCAL),
        Some(sdr_core::AudioSinkType::Local)
    );
    assert_eq!(
        audio_sink_type_from_c(SDR_AUDIO_SINK_NETWORK),
        Some(sdr_core::AudioSinkType::Network)
    );
    assert_eq!(audio_sink_type_from_c(99), None);
    assert_eq!(audio_sink_type_from_c(-1), None);
}

#[test]
fn protocol_from_c_covers_all_variants() {
    assert_eq!(
        protocol_from_c(crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER),
        Some(sdr_types::Protocol::TcpClient)
    );
    assert_eq!(
        protocol_from_c(crate::event::SDR_NETWORK_PROTOCOL_UDP),
        Some(sdr_types::Protocol::Udp)
    );
    assert_eq!(protocol_from_c(99), None);
}

#[test]
fn advanced_demod_bool_setters_accept_both_polarities() {
    // The four bool-typed advanced setters have no validation
    // beyond handle + panic catch — this just pins that they
    // don't silently regress to rejecting a valid input.
    let h = make_handle();
    for &on in &[true, false] {
        assert_eq!(
            unsafe { sdr_core_set_nb_enabled(h, on) },
            SdrCoreError::Ok.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_set_fm_if_nr_enabled(h, on) },
            SdrCoreError::Ok.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_set_wfm_stereo(h, on) },
            SdrCoreError::Ok.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_set_notch_enabled(h, on) },
            SdrCoreError::Ok.as_int()
        );
    }
    destroy(h);
}

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

#[test]
fn scanner_commands_reject_null_handle() {
    // Matches the `all_commands_reject_null_handle` pattern
    // above — pins the null-handle guard on all three
    // new entry points so a refactor of `with_core`
    // can't silently drop it for one arm. Per `CodeRabbit`
    // round 1 on PR #497.
    assert_eq!(
        unsafe { sdr_core_set_scanner_enabled(std::ptr::null_mut(), true) },
        SdrCoreError::InvalidHandle.as_int()
    );
    let name = CString::new("Test").unwrap();
    assert_eq!(
        unsafe {
            sdr_core_lockout_scanner_channel(
                std::ptr::null_mut(),
                name.as_ptr(),
                TEST_SCANNER_FREQ_HZ,
            )
        },
        SdrCoreError::InvalidHandle.as_int()
    );
    assert_eq!(
        unsafe {
            sdr_core_unlock_scanner_channel(
                std::ptr::null_mut(),
                name.as_ptr(),
                TEST_SCANNER_FREQ_HZ,
            )
        },
        SdrCoreError::InvalidHandle.as_int()
    );
}

#[test]
fn scanner_lockout_rejects_null_name() {
    // The lockout / unlock commands need a NUL-terminated
    // UTF-8 name to build the `ChannelKey`. Null pointer
    // is a caller bug; surface it as InvalidArg instead
    // of dereferencing.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_lockout_scanner_channel(h, std::ptr::null(), TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_unlock_scanner_channel(h, std::ptr::null(), TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn scanner_lockout_rejects_invalid_utf8_name() {
    // `CStr::to_str` refuses non-UTF-8 bytes; the command
    // returns InvalidArg rather than taking the bytes
    // lossily. A lossy coerce would let a host write a
    // ChannelKey name that would never match the
    // projection-time name the scanner holds.
    let h = make_handle();
    // Lone 0xFF — invalid start byte for any UTF-8
    // codepoint. NUL-terminated so `CStr::from_ptr`
    // finds the end of string.
    let bad: [u8; 2] = [0xFF, 0x00];
    let bad_ptr = bad.as_ptr().cast::<c_char>();
    assert_eq!(
        unsafe { sdr_core_lockout_scanner_channel(h, bad_ptr, TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_unlock_scanner_channel(h, bad_ptr, TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn scanner_commands_happy_path_return_ok() {
    // Engine accepts the scanner command regardless of
    // whether any channels are projected — the state
    // machine just stays in Idle. Confirms dispatch
    // round-trips and the name-copy path works for a
    // normal UTF-8 string.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_scanner_enabled(h, true) },
        SdrCoreError::Ok.as_int()
    );
    let name = CString::new("NOAA Weather").unwrap();
    assert_eq!(
        unsafe { sdr_core_lockout_scanner_channel(h, name.as_ptr(), TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_unlock_scanner_channel(h, name.as_ptr(), TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_scanner_enabled(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

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

#[test]
fn update_scanner_channels_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_update_scanner_channels(std::ptr::null_mut(), std::ptr::null(), 0) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

#[test]
fn update_scanner_channels_empty_clears_rotation() {
    // The empty-list path is how a host clears the
    // scanner's rotation (e.g., user toggled scan_enabled
    // off on every bookmark). Both null+0 callable; the
    // engine accepts the empty Vec and the state machine
    // settles to Idle on the next tick.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_update_scanner_channels(h, std::ptr::null(), 0) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn update_scanner_channels_count_zero_with_nonnull_pointer_is_invalid() {
    // Catch a buggy host that has `count = 0` and a stale
    // pointer — refuse rather than silently accept, so the
    // off-by-one is caught at the boundary.
    let h = make_handle();
    let name = CString::new("Test").unwrap();
    let ch = SdrScannerChannel {
        name_utf8: name.as_ptr(),
        frequency_hz: TEST_SCANNER_FREQ_HZ,
        demod_mode: SDR_DEMOD_NFM,
        bandwidth_hz: TEST_SCANNER_BW_HZ,
        priority: 0,
        dwell_ms: TEST_SCANNER_DWELL_MS,
        hang_ms: TEST_SCANNER_HANG_MS,
    };
    let rc = unsafe { sdr_core_update_scanner_channels(h, &raw const ch, 0) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    destroy(h);
}

#[test]
fn update_scanner_channels_null_pointer_with_count_is_invalid() {
    let h = make_handle();
    let rc = unsafe { sdr_core_update_scanner_channels(h, std::ptr::null(), 1) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    destroy(h);
}

#[test]
fn update_scanner_channels_rejects_null_name_in_entry() {
    let h = make_handle();
    let ch = SdrScannerChannel {
        name_utf8: std::ptr::null(),
        frequency_hz: TEST_SCANNER_FREQ_HZ,
        demod_mode: SDR_DEMOD_NFM,
        bandwidth_hz: TEST_SCANNER_BW_HZ,
        priority: 0,
        dwell_ms: TEST_SCANNER_DWELL_MS,
        hang_ms: TEST_SCANNER_HANG_MS,
    };
    let rc = unsafe { sdr_core_update_scanner_channels(h, &raw const ch, 1) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    destroy(h);
}

#[test]
fn update_scanner_channels_rejects_unknown_demod_mode() {
    let h = make_handle();
    let name = CString::new("Test").unwrap();
    let ch = SdrScannerChannel {
        name_utf8: name.as_ptr(),
        frequency_hz: TEST_SCANNER_FREQ_HZ,
        demod_mode: 99, // out of range — no `SDR_DEMOD_*` matches.
        bandwidth_hz: TEST_SCANNER_BW_HZ,
        priority: 0,
        dwell_ms: TEST_SCANNER_DWELL_MS,
        hang_ms: TEST_SCANNER_HANG_MS,
    };
    let rc = unsafe { sdr_core_update_scanner_channels(h, &raw const ch, 1) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    destroy(h);
}

#[test]
fn update_scanner_channels_rejects_non_positive_bandwidth() {
    let h = make_handle();
    let name = CString::new("Test").unwrap();
    for bad_bw in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let ch = SdrScannerChannel {
            name_utf8: name.as_ptr(),
            frequency_hz: TEST_SCANNER_FREQ_HZ,
            demod_mode: SDR_DEMOD_NFM,
            bandwidth_hz: bad_bw,
            priority: 0,
            dwell_ms: TEST_SCANNER_DWELL_MS,
            hang_ms: TEST_SCANNER_HANG_MS,
        };
        let rc = unsafe { sdr_core_update_scanner_channels(h, &raw const ch, 1) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }
    destroy(h);
}

#[test]
fn update_scanner_channels_happy_path_round_trip() {
    // Two channels — one normal, one priority. Confirms
    // the slice walk + dispatch round-trips for a typical
    // host call.
    let h = make_handle();
    let name_a = CString::new("NOAA Weather").unwrap();
    let name_b = CString::new("FRS Ch 1").unwrap();
    let channels = [
        SdrScannerChannel {
            name_utf8: name_a.as_ptr(),
            frequency_hz: TEST_SCANNER_FREQ_HZ,
            demod_mode: SDR_DEMOD_NFM,
            bandwidth_hz: TEST_SCANNER_BW_HZ,
            priority: 1, // priority tier
            dwell_ms: TEST_SCANNER_DWELL_MS,
            hang_ms: TEST_SCANNER_HANG_MS,
        },
        SdrScannerChannel {
            name_utf8: name_b.as_ptr(),
            frequency_hz: 462_562_500,
            demod_mode: SDR_DEMOD_NFM,
            bandwidth_hz: TEST_SCANNER_BW_HZ,
            priority: 0,
            dwell_ms: TEST_SCANNER_DWELL_MS,
            hang_ms: TEST_SCANNER_HANG_MS,
        },
    ];
    let rc = unsafe { sdr_core_update_scanner_channels(h, channels.as_ptr(), channels.len()) };
    assert_eq!(rc, SdrCoreError::Ok.as_int());
    destroy(h);
}
