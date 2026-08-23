use super::*;

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
    let (_tmp_dir, tmp) = unique_temp_wav("sdr-ffi-test");
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
    // `_tmp_dir` removes the file with the directory when it drops.
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

    let (_tmp_dir, tmp) = unique_temp_wav("sdr-ffi-iq-test");
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

#[test]
fn start_stop_round_trip() {
    let h = make_handle();
    assert_eq!(unsafe { sdr_core_start(h) }, SdrCoreError::Ok.as_int());
    assert_eq!(unsafe { sdr_core_stop(h) }, SdrCoreError::Ok.as_int());
    destroy(h);
}
