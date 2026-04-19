//! Event delivery from the engine into a host-registered C callback.
//!
//! The FFI dispatcher thread owns the `mpsc::Receiver<DspToUi>`
//! taken from `Engine::subscribe`. It loops on `recv()`, translates
//! each `DspToUi` variant into a C-layout `SdrEvent` struct (tagged
//! union), and invokes the host-registered callback with a borrowed
//! pointer to that event. Borrowed pointers inside the event
//! (device-info strings, gain-list arrays, error strings) are valid
//! only for the duration of the callback call — hosts that want to
//! persist the data must copy it out before returning.
//!
//! ## Threading model (must match `include/sdr_core.h`)
//!
//! - The callback runs on the dispatcher thread, **not** the host's
//!   main thread. Hosts are responsible for marshaling to their
//!   preferred thread (GCD main queue, SwiftUI `MainActor`, GTK
//!   main-context idle, etc.).
//! - `sdr_core_destroy` must **not** be called from inside the
//!   callback. It joins this dispatcher thread, so calling it from
//!   within a dispatched event would deadlock against our own
//!   join.
//! - Other `sdr_core_*` functions (commands, `pull_fft`,
//!   `last_error_message`) are safe to call from inside the
//!   callback.
//!
//! ## Construction order
//!
//! The dispatcher is spawned at `sdr_core_create` time, before the
//! handle is handed back to the host. The callback slot starts
//! `None`; events that arrive before the host registers a callback
//! are silently dropped. The host is expected to register a
//! callback immediately after create and before `sdr_core_start`,
//! otherwise initial DeviceInfo / GainList / DisplayBandwidth
//! events fired during source open will be missed.

use std::ffi::{CString, c_char, c_void};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use sdr_core::DspToUi;

use crate::handle::{EventCallbackGuard, EventCallbackSlot};

// ============================================================
//  Event kind discriminants — must match `SdrEventKind` in
//  `include/sdr_core.h`. Never reorder or renumber.
// ============================================================

pub const SDR_EVT_SOURCE_STOPPED: i32 = 1;
pub const SDR_EVT_SAMPLE_RATE_CHANGED: i32 = 2;
pub const SDR_EVT_SIGNAL_LEVEL: i32 = 3;
pub const SDR_EVT_DEVICE_INFO: i32 = 4;
pub const SDR_EVT_GAIN_LIST: i32 = 5;
pub const SDR_EVT_DISPLAY_BANDWIDTH: i32 = 6;
pub const SDR_EVT_ERROR: i32 = 7;

// ============================================================
//  SdrEvent tagged union — `#[repr(C)]` layout matching the
//  header definition.
// ============================================================

/// Payload for `SDR_EVT_DEVICE_INFO`. Borrowed pointer into
/// dispatcher-owned storage; valid for the callback duration only.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrEventDeviceInfo {
    pub utf8: *const c_char,
}

/// Payload for `SDR_EVT_GAIN_LIST`. Borrowed pointer into
/// dispatcher-owned storage; valid for the callback duration only.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrEventGainList {
    pub values: *const f64,
    pub len: usize,
}

/// Payload for `SDR_EVT_ERROR`. Borrowed pointer into
/// dispatcher-owned storage; valid for the callback duration only.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrEventError {
    pub utf8: *const c_char,
}

/// C-layout tagged union of event payloads. Which field is valid
/// is determined by the `kind` discriminant on the enclosing
/// `SdrEvent`:
///
/// | `kind`                          | Valid field              |
/// |---------------------------------|--------------------------|
/// | `SDR_EVT_SOURCE_STOPPED`        | none                     |
/// | `SDR_EVT_SAMPLE_RATE_CHANGED`   | `sample_rate_hz`         |
/// | `SDR_EVT_SIGNAL_LEVEL`          | `signal_level_db`        |
/// | `SDR_EVT_DEVICE_INFO`           | `device_info.utf8`       |
/// | `SDR_EVT_GAIN_LIST`             | `gain_list.{values,len}` |
/// | `SDR_EVT_DISPLAY_BANDWIDTH`     | `display_bandwidth_hz`   |
/// | `SDR_EVT_ERROR`                 | `error.utf8`             |
///
/// `_placeholder` exists so `SOURCE_STOPPED` events (which carry
/// no payload) can still construct the struct with a meaningful
/// default byte pattern.
#[repr(C)]
#[derive(Clone, Copy)]
pub union SdrEventPayload {
    pub sample_rate_hz: f64,
    pub signal_level_db: f32,
    pub display_bandwidth_hz: f64,
    pub device_info: SdrEventDeviceInfo,
    pub gain_list: SdrEventGainList,
    pub error: SdrEventError,
    /// Placeholder for kinds that carry no payload (e.g.,
    /// `SDR_EVT_SOURCE_STOPPED`). Accessing this field is always
    /// valid as a zero-byte read.
    pub _placeholder: u64,
}

/// Top-level event struct handed to the host callback.
#[repr(C)]
pub struct SdrEvent {
    pub kind: i32,
    pub payload: SdrEventPayload,
}

/// C callback type registered via `sdr_core_set_event_callback`.
/// `Option<...>` because C callers pass a nullable function
/// pointer (null unregisters any previously-set callback).
///
/// `event` is a borrow into the dispatcher thread's stack frame;
/// valid only for the duration of the call. `user_data` is the
/// opaque pointer the host passed at registration — the FFI side
/// never dereferences it.
pub type SdrEventCallback =
    Option<unsafe extern "C" fn(event: *const SdrEvent, user_data: *mut c_void)>;

// ============================================================
//  Dispatcher thread
// ============================================================

/// Spawn the FFI event dispatcher thread.
///
/// The thread owns `rx` (the Engine's event receiver) and reads
/// the `callback_slot` under a mutex on every event. When `rx`
/// disconnects (because the Engine has been dropped), the loop
/// exits and the thread terminates.
///
/// Called from `sdr_core_create` immediately after `Engine::new`.
pub(crate) fn spawn_dispatcher(
    rx: mpsc::Receiver<DspToUi>,
    callback_guard: Arc<EventCallbackGuard>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("sdr-ffi-event-dispatcher".into())
        .spawn(move || {
            dispatcher_loop(&rx, &callback_guard);
        })
}

/// Dispatcher thread main loop. Exits when the receiver
/// disconnects (engine dropped).
fn dispatcher_loop(rx: &mpsc::Receiver<DspToUi>, callback_guard: &EventCallbackGuard) {
    while let Ok(msg) = rx.recv() {
        let has_callback = callback_guard
            .state
            .lock()
            .is_ok_and(|guard| guard.slot.is_some());
        if !has_callback {
            continue;
        }

        dispatch_one(&msg, callback_guard);
    }
    tracing::debug!("sdr-ffi event dispatcher exiting (channel disconnected)");
}

/// Translate one `DspToUi` into a C-layout `SdrEvent` plus the
/// owned storage that must outlive the callback (the raw pointers
/// inside the event reference these locals). Returns `None` for
/// variants not yet exposed at the FFI boundary.
///
/// Allocation cost: the v1 event rate is dominated by SignalLevel
/// updates which don't allocate at all. The per-event allocation
/// cost only matters for the rare DeviceInfo / GainList / Error
/// paths. If profiling ever shows contention here, we can reuse
/// per-dispatcher scratch buffers like the CoreAudio render
/// callback does.
fn translate_event(msg: &DspToUi) -> Option<(SdrEvent, Option<CString>, Option<Vec<f64>>)> {
    let mut owned_cstring: Option<CString> = None;
    let mut owned_vec: Option<Vec<f64>> = None;

    let event = match msg {
        DspToUi::SourceStopped => SdrEvent {
            kind: SDR_EVT_SOURCE_STOPPED,
            payload: SdrEventPayload { _placeholder: 0 },
        },

        DspToUi::SampleRateChanged(rate) => SdrEvent {
            kind: SDR_EVT_SAMPLE_RATE_CHANGED,
            payload: SdrEventPayload {
                sample_rate_hz: *rate,
            },
        },

        DspToUi::SignalLevel(db) => SdrEvent {
            kind: SDR_EVT_SIGNAL_LEVEL,
            payload: SdrEventPayload {
                signal_level_db: *db,
            },
        },

        DspToUi::DisplayBandwidth(hz) => SdrEvent {
            kind: SDR_EVT_DISPLAY_BANDWIDTH,
            payload: SdrEventPayload {
                display_bandwidth_hz: *hz,
            },
        },

        DspToUi::DeviceInfo(name) => {
            // Replace interior NULs defensively rather than drop
            // the event on an unusual device name.
            let sanitized = name.replace('\0', "?");
            let Ok(cstr) = CString::new(sanitized) else {
                // Unreachable: replace('\0', "?") removed all interior NULs.
                return None;
            };
            let ptr = cstr.as_ptr();
            owned_cstring = Some(cstr);
            SdrEvent {
                kind: SDR_EVT_DEVICE_INFO,
                payload: SdrEventPayload {
                    device_info: SdrEventDeviceInfo { utf8: ptr },
                },
            }
        }

        DspToUi::GainList(gains) => {
            let vec = gains.clone();
            let ptr = vec.as_ptr();
            let len = vec.len();
            owned_vec = Some(vec);
            SdrEvent {
                kind: SDR_EVT_GAIN_LIST,
                payload: SdrEventPayload {
                    gain_list: SdrEventGainList { values: ptr, len },
                },
            }
        }

        DspToUi::Error(msg) => {
            let sanitized = msg.replace('\0', "?");
            let Ok(cstr) = CString::new(sanitized) else {
                // Unreachable: replace('\0', "?") removed all interior NULs.
                return None;
            };
            let ptr = cstr.as_ptr();
            owned_cstring = Some(cstr);
            SdrEvent {
                kind: SDR_EVT_ERROR,
                payload: SdrEventPayload {
                    error: SdrEventError { utf8: ptr },
                },
            }
        }

        // Variants not yet exposed at the FFI boundary. Silently
        // dropped in v1; a v2 ABI minor bump grows the surface to
        // cover them as each feature lands in the macOS SwiftUI
        // host.
        //
        // Specifically:
        //   - `FftData` is intentionally never routed through the
        //     event callback — FFT frames go through the dedicated
        //     pull function (`sdr_core_pull_fft`) instead so the
        //     render loop stays on the main thread.
        //   - `AudioRecordingStarted/Stopped` + `IqRecordingStarted/
        //     Stopped` light up when recording controls land in
        //     the macOS UI (v2 backlog issues #238 / #239).
        //   - `DemodModeChanged` is the transcription-session
        //     boundary event. macOS transcription IS on the
        //     roadmap — it's currently blocked on a Metal
        //     inference backend for sherpa-onnx (parallel work,
        //     planned `metal.rs` port). When that lands, this
        //     variant becomes the session-reset trigger for the
        //     SwiftUI transcript panel too, exactly like it does
        //     for the GTK transcript panel today.
        //   - `CtcssSustainedChanged` and `VoiceSquelchOpenChanged`
        //     drive status indicators in the Linux UI. They'll
        //     light up in the macOS UI whenever the CTCSS / voice-
        //     squelch panels get ported (no specific backlog issue
        //     yet — part of the full-parity backlog under #228).
        //
        // Adding any of these to the ABI is additive (new
        // `SDR_EVT_*` discriminant + new payload struct or reuse
        // of existing ones), so a future minor bump won't break
        // older hosts that don't know about them.
        DspToUi::FftData(_)
        | DspToUi::AudioRecordingStarted(_)
        | DspToUi::AudioRecordingStopped
        | DspToUi::IqRecordingStarted(_)
        | DspToUi::IqRecordingStopped
        | DspToUi::DemodModeChanged(_)
        | DspToUi::CtcssSustainedChanged(_)
        | DspToUi::VoiceSquelchOpenChanged(_)
        | DspToUi::RtlTcpConnectionState(_) => return None,
    };

    Some((event, owned_cstring, owned_vec))
}

/// Fire the registered callback for one translated `SdrEvent`.
///
/// No-op if the callback slot became `None` between the check in
/// `dispatcher_loop` and the time we reacquired the lock here (the
/// host can clear the callback at any time from another thread).
///
/// Quiescence protocol: we increment `in_flight` before dropping
/// the lock and decrement after the callback returns. This lets
/// `sdr_core_set_event_callback` wait for in-flight dispatches to
/// drain before returning — preventing use-after-free of the old
/// `user_data` when the host clears or replaces the callback.
///
/// The callback itself is wrapped in `catch_unwind`: if the host's
/// callback panics (unlikely from Swift / C, but possible from a
/// host written in another language bound to this ABI), we don't
/// want the panic to propagate up through our dispatcher and tear
/// down the thread.
fn dispatch_one(msg: &DspToUi, callback_guard: &EventCallbackGuard) {
    let Some((event, owned_cstring, owned_vec)) = translate_event(msg) else {
        return;
    };

    let mut guard = match callback_guard.state.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(slot) = guard.slot.as_ref()
        && let Some(cb) = slot.callback
    {
        let user_data = slot.user_data;
        guard.in_flight += 1;
        // Release the lock before calling the host to avoid
        // deadlock if the callback re-enters the FFI (e.g.,
        // calls a command that eventually needs this lock).
        let event_ptr: *const SdrEvent = &raw const event;
        drop(guard);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: cb is a C callback; user_data ownership
            // is on the host per the contract in
            // `include/sdr_core.h`. event_ptr is valid for
            // the duration of this call because `event`
            // lives on our stack until the end of
            // `dispatch_one`.
            unsafe { cb(event_ptr, user_data) };
        }));
        if result.is_err() {
            tracing::warn!("sdr-ffi event callback panicked (payload swallowed)");
        }

        let mut guard = match callback_guard.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.in_flight -= 1;
        if guard.in_flight == 0 {
            callback_guard.quiesced.notify_all();
        }
    }

    // Explicitly keep the owned storage alive until here so that
    // any pointers the callback received through `event_ptr`
    // remain valid. These go out of scope at end-of-function.
    drop(owned_cstring);
    drop(owned_vec);
}

// ============================================================
//  FFI entry point: set_event_callback
// ============================================================

/// Register (or clear) the host's event callback. See
/// `include/sdr_core.h`.
///
/// # Safety
///
/// `handle` must be non-null and valid (see `sdr_core_create`).
/// `callback` is a nullable function pointer; passing null clears
/// any previously-registered callback and silences subsequent
/// events. `user_data` is opaque to the FFI side and is handed
/// back to `callback` on every invocation — the host is
/// responsible for its lifetime and thread-safety.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_event_callback(
    handle: *mut crate::handle::SdrCore,
    callback: SdrEventCallback,
    user_data: *mut c_void,
) -> i32 {
    use crate::error::{SdrCoreError, clear_last_error, set_last_error};
    use crate::handle::SdrCore;

    let result = std::panic::catch_unwind(|| {
        // SAFETY: caller contract.
        let Some(core) = (unsafe { SdrCore::from_raw(handle) }) else {
            set_last_error("sdr_core_set_event_callback: null handle");
            return SdrCoreError::InvalidHandle.as_int();
        };

        // Reject re-entry from the dispatcher thread. If the host
        // calls this from inside the event callback, the quiescence
        // wait below would deadlock (in_flight is non-zero because
        // WE are the in-flight dispatch).
        let is_dispatcher = core
            .dispatcher_handle
            .lock()
            .ok()
            .and_then(|g| {
                g.as_ref()
                    .map(|h| h.thread().id() == std::thread::current().id())
            })
            .unwrap_or(false);
        if is_dispatcher {
            set_last_error(
                "sdr_core_set_event_callback: called from inside the event callback \
                 (re-entry not supported)",
            );
            return SdrCoreError::InvalidArg.as_int();
        }

        let mut guard = match core.event_callback.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Wait for any in-flight dispatch of the old callback to
        // finish before replacing the slot. This guarantees the
        // host can safely free old user_data after this call returns.
        while guard.in_flight > 0 {
            guard = core
                .event_callback
                .quiesced
                .wait(guard)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }

        guard.slot = callback.map(|cb| EventCallbackSlot {
            callback: Some(cb),
            user_data,
        });

        clear_last_error();
        SdrCoreError::Ok.as_int()
    });

    match result {
        Ok(code) => code,
        Err(payload) => {
            set_last_error(format!(
                "sdr_core_set_event_callback: panic: {}",
                crate::lifecycle::panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
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

    #[test]
    fn set_event_callback_null_handle_returns_invalid_handle() {
        let rc = unsafe {
            sdr_core_set_event_callback(std::ptr::null_mut(), None, std::ptr::null_mut())
        };
        assert_eq!(rc, SdrCoreError::InvalidHandle.as_int());
    }

    // Top-of-module dummy callbacks. Rust lints (clippy's
    // `items_after_statements`) complains when these are defined
    // inside a test function body.
    unsafe extern "C" fn noop_cb(_event: *const SdrEvent, _user_data: *mut c_void) {}

    #[test]
    fn set_event_callback_clear_then_set_then_clear() {
        let h = make_handle();
        // Clearing on a fresh engine is a no-op but must succeed.
        assert_eq!(
            unsafe { sdr_core_set_event_callback(h, None, std::ptr::null_mut()) },
            SdrCoreError::Ok.as_int()
        );

        assert_eq!(
            unsafe { sdr_core_set_event_callback(h, Some(noop_cb), std::ptr::null_mut()) },
            SdrCoreError::Ok.as_int()
        );

        // Clear again.
        assert_eq!(
            unsafe { sdr_core_set_event_callback(h, None, std::ptr::null_mut()) },
            SdrCoreError::Ok.as_int()
        );

        unsafe { sdr_core_destroy(h) };
    }

    // Shared atomic for the counting callback test below.
    // Each test has its own static to avoid cross-test
    // contamination in parallel runs.
    static DISPATCH_COUNTER: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counting_cb(_event: *const SdrEvent, _user_data: *mut c_void) {
        DISPATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn dispatcher_exits_cleanly_on_destroy_with_callback_registered() {
        // Whether any events actually fire depends on what the
        // DSP controller happens to emit on startup (without a
        // real source running it may emit zero). What we're
        // really testing is that registering a callback and
        // then destroying the engine doesn't crash, hang, or
        // leave the dispatcher thread alive.
        DISPATCH_COUNTER.store(0, Ordering::Relaxed);

        let h = make_handle();
        assert_eq!(
            unsafe { sdr_core_set_event_callback(h, Some(counting_cb), std::ptr::null_mut()) },
            SdrCoreError::Ok.as_int()
        );

        // Give the dispatcher a tiny moment to process any
        // initial events before destroying.
        std::thread::sleep(std::time::Duration::from_millis(20));

        unsafe { sdr_core_destroy(h) };
        // Counter may be 0 (no events) or >0 (some fired). Both
        // are fine; the contract we're testing is just that
        // destroy returned, which it did.
        let _ = DISPATCH_COUNTER.load(Ordering::Relaxed);
    }

    // ------------------------------------------------------
    //  Stateless construction of the event struct itself.
    //  These don't need a real engine.
    // ------------------------------------------------------

    #[test]
    fn event_kind_discriminants_match_header() {
        // Locks in the values against the header. If these drift,
        // `make ffi-header-check` (next checkpoint) will also
        // catch it, but this runs as a plain unit test.
        assert_eq!(SDR_EVT_SOURCE_STOPPED, 1);
        assert_eq!(SDR_EVT_SAMPLE_RATE_CHANGED, 2);
        assert_eq!(SDR_EVT_SIGNAL_LEVEL, 3);
        assert_eq!(SDR_EVT_DEVICE_INFO, 4);
        assert_eq!(SDR_EVT_GAIN_LIST, 5);
        assert_eq!(SDR_EVT_DISPLAY_BANDWIDTH, 6);
        assert_eq!(SDR_EVT_ERROR, 7);
    }

    #[test]
    fn sdr_event_payload_size_is_reasonable() {
        // Sanity check on the union layout. On 64-bit targets,
        // the largest payload (gain_list with {ptr, usize}) is
        // 16 bytes; the event struct adds i32 kind plus padding.
        // We expect total <= 32 bytes which is the C-side
        // expectation too.
        let size = std::mem::size_of::<SdrEvent>();
        assert!(
            size <= 32,
            "SdrEvent size {size} exceeds 32-byte budget — may indicate an unintended union growth"
        );
    }
}
