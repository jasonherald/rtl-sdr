//! Audio tap C ABI — streams the engine's post-demod audio to a
//! host-side consumer at 16 kHz mono f32.
//!
//! Primary use case: feeding the macOS `SpeechAnalyzer` /
//! `SpeechTranscriber` frameworks (issue #314) without pulling the
//! `sdr-transcription` Rust backend stack across the FFI.
//!
//! ## Shape
//!
//! Push-style delivery via a C callback. Each time the engine
//! completes an audio block, the DSP thread downsamples to 16 kHz
//! mono (see `sdr_dsp::convert::stereo_48k_to_mono_16k`), hands the
//! chunk to a bounded mpsc channel, and a dedicated FFI dispatcher
//! thread invokes the registered callback with the chunk bytes.
//!
//! Why push instead of pull: Swift hosts asked for push so they
//! don't have to run a render-tick poll loop on their side. The
//! existing event-delivery machinery uses the same shape; this
//! module borrows the pattern with a simpler (no in-flight
//! quiescence) callback lifecycle because the tap doesn't support
//! mid-stream callback swap — callers must `stop` and `start` to
//! change the callback.
//!
//! ## Threading
//!
//! - DSP thread: calls `try_send`. On `TrySendError::Full`, the
//!   chunk is dropped with a debug log (the DSP thread MUST NOT
//!   stall — a dropped SpeechAnalyzer frame is fine; an audio
//!   underrun is not).
//! - Dispatcher thread (`sdr-ffi-audio-tap-dispatcher`): loops on
//!   `rx.recv()`. The callback runs on THIS thread — hosts that
//!   need main-actor UI work must marshal across.
//!
//! ## Lifecycle
//!
//! ```text
//!   start_audio_tap()           stop_audio_tap()
//!         ├─ create mpsc              ├─ send DisableAudioTap
//!         ├─ send EnableAudioTap      ├─ channel disconnects
//!         └─ spawn dispatcher         ├─ dispatcher loop exits
//!                                     └─ join dispatcher thread
//! ```
//!
//! `sdr_core_destroy` calls `stop_audio_tap` as part of teardown so
//! a host that forgets to stop the tap doesn't leave a dangling
//! dispatcher thread holding their `user_data` pointer after the
//! handle is gone.

use std::ffi::c_void;
use std::sync::mpsc;

use sdr_core::UiToDsp;

use crate::error::{SdrCoreError, clear_last_error, set_last_error};
use crate::handle::SdrCore;
use crate::lifecycle::panic_message;

/// Bounded queue depth for the DSP → dispatcher channel.
///
/// Sized so a briefly-backed-up SpeechAnalyzer consumer (say, the
/// first `SpeechAnalyzer.bestAvailable(for:)` call, which can take
/// 100–200 ms on first invocation as the model loads) doesn't
/// immediately trigger frame drops. At ~50 chunks/sec (48 kHz /
/// 1024-sample blocks post-3:1 decimation), 32 slots covers ~640
/// ms of lag.
const AUDIO_TAP_CHANNEL_DEPTH: usize = 32;

/// C callback type registered via `sdr_core_start_audio_tap`.
///
/// `samples` is a borrow into the dispatcher thread's stack frame;
/// valid only for the duration of the call. `sample_count` is the
/// number of f32 elements (not bytes). `user_data` is the opaque
/// pointer the host passed at registration — the FFI side never
/// dereferences it.
///
/// Samples are 16 kHz mono f32 per the module-level contract.
pub type SdrAudioTapCallback =
    Option<unsafe extern "C" fn(samples: *const f32, sample_count: usize, user_data: *mut c_void)>;

/// UserData wrapper so the dispatcher thread's closure captures
/// a `Send` type. Rust 2021 closure-capture rules would
/// otherwise promote the capture of `ud.0` to a capture of the
/// raw `*mut c_void` directly, bypassing the `unsafe impl Send`
/// we attach here; passing `UserDataPtr` through to
/// `dispatcher_loop` as a whole value is what keeps the closure
/// capture of the `Send` wrapper intact.
///
/// The raw `*mut c_void` isn't `Send` by default, but the host
/// contract makes it the caller's responsibility to ensure the
/// pointed-to state is safe to touch from the dispatcher thread
/// — same guarantee the event dispatcher relies on.
struct UserDataPtr(*mut c_void);

// SAFETY: see the module-level comment and the event dispatcher's
// matching `EventCallbackSlot` impl. Host owns aliasing rules.
unsafe impl Send for UserDataPtr {}

/// Dispatcher thread main loop — owns the receiver and the
/// (callback, user_data) pair for this tap session. Exits when
/// the channel disconnects (the DSP thread dropped the sender,
/// either because `DisableAudioTap` fired or because the engine
/// is being torn down).
// Pass-by-value is intentional: the closure that spawns this
// function must capture a `Send` value, and `UserDataPtr` is
// only `Send` as an owned wrapper (the inner `*mut c_void` is
// not `Send`). Taking `&UserDataPtr` would change the closure's
// capture to a borrow, which doesn't survive the `move` onto
// the worker thread.
#[allow(clippy::needless_pass_by_value)]
fn dispatcher_loop(
    rx: &mpsc::Receiver<Vec<f32>>,
    callback: unsafe extern "C" fn(*const f32, usize, *mut c_void),
    user_data: UserDataPtr,
) {
    // Destructure (consuming `user_data`) so the wrapper exists
    // only long enough to satisfy the closure-capture Send
    // analysis in `sdr_core_start_audio_tap`; the loop itself
    // just needs the raw pointer.
    let UserDataPtr(ud_ptr) = user_data;
    while let Ok(chunk) = rx.recv() {
        // SAFETY: the host contractually guarantees the callback
        // and user_data remain valid between the start_audio_tap
        // call that registered them and the stop_audio_tap call
        // that unregisters them. The dispatcher thread is joined
        // inside stop_audio_tap before the function returns, so
        // the host can drop user_data immediately after stop.
        unsafe {
            callback(chunk.as_ptr(), chunk.len(), ud_ptr);
        }
    }
    tracing::debug!("sdr-ffi audio tap dispatcher exiting (channel disconnected)");
}

/// Start streaming post-demod audio to `callback`. The callback
/// fires from a dedicated dispatcher thread with 16 kHz mono f32
/// samples. Only one tap can be active per handle — calling this
/// again without `sdr_core_stop_audio_tap` first returns
/// `SDR_CORE_ERR_INVALID_HANDLE` with a descriptive last-error
/// message (the second tap would leak the first dispatcher).
///
/// `user_data` is opaque to the FFI — it's handed back to the
/// callback verbatim. NULL is allowed; the callback must simply
/// handle receiving NULL there.
///
/// # Safety
///
/// The callback + user_data must remain valid until
/// `sdr_core_stop_audio_tap` returns or the handle is destroyed.
/// The callback may run on any thread (not the caller's).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_start_audio_tap(
    handle: *mut SdrCore,
    callback: SdrAudioTapCallback,
    user_data: *mut c_void,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        // SAFETY: caller contract matches `SdrCore::from_raw_mut`.
        let Some(core) = (unsafe { SdrCore::from_raw_mut(handle) }) else {
            set_last_error("sdr_core_start_audio_tap: null or invalid handle");
            return SdrCoreError::InvalidHandle.as_int();
        };

        let Some(cb) = callback else {
            set_last_error("sdr_core_start_audio_tap: callback is null");
            return SdrCoreError::InvalidArg.as_int();
        };

        // Reject a second start without a stop in between — the
        // dispatcher thread would leak and the engine would hold
        // two senders routing to the same callback on every
        // block.
        {
            let Ok(guard) = core.audio_tap_dispatcher.lock() else {
                set_last_error("sdr_core_start_audio_tap: dispatcher mutex poisoned");
                return SdrCoreError::Internal.as_int();
            };
            if guard.is_some() {
                set_last_error(
                    "sdr_core_start_audio_tap: tap already active; call stop_audio_tap first",
                );
                return SdrCoreError::InvalidHandle.as_int();
            }
        }

        // Create the DSP → dispatcher channel. Sender goes to the
        // engine; receiver stays on the dispatcher thread.
        let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(AUDIO_TAP_CHANNEL_DEPTH);

        // Tell the engine to start tapping.
        if let Err(e) = core.engine.send_command(UiToDsp::EnableAudioTap(tx)) {
            set_last_error(format!("sdr_core_start_audio_tap: send_command: {e}"));
            return SdrCoreError::NotRunning.as_int();
        }

        // Spawn the dispatcher. Captures the callback fn pointer
        // and the user_data wrapper — both guaranteed `Send` by
        // construction (fn pointer) or manual impl (user_data).
        // Pass `ud` to `dispatcher_loop` as a whole value (not
        // `ud.0`) so the closure captures the `Send` wrapper
        // rather than the inner raw pointer — Rust 2021 granular
        // capture would otherwise break the Send analysis.
        let ud = UserDataPtr(user_data);
        let spawn_result = std::thread::Builder::new()
            .name("sdr-ffi-audio-tap-dispatcher".into())
            .spawn(move || {
                dispatcher_loop(&rx, cb, ud);
            });
        let join_handle = match spawn_result {
            Ok(h) => h,
            Err(e) => {
                // Roll back the engine-side tap enable so we don't
                // leave the DSP thread pushing into a receiver no
                // one will ever drain.
                let _ = core.engine.send_command(UiToDsp::DisableAudioTap);
                set_last_error(format!("sdr_core_start_audio_tap: spawn failed: {e}"));
                return SdrCoreError::Internal.as_int();
            }
        };

        if let Ok(mut guard) = core.audio_tap_dispatcher.lock() {
            *guard = Some(join_handle);
        } else {
            set_last_error("sdr_core_start_audio_tap: dispatcher mutex poisoned");
            return SdrCoreError::Internal.as_int();
        }

        clear_last_error();
        SdrCoreError::Ok.as_int()
    });
    match result {
        Ok(code) => code,
        Err(payload) => {
            set_last_error(format!(
                "sdr_core_start_audio_tap: panic: {}",
                panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

/// Stop a tap started by `sdr_core_start_audio_tap`. Safe to call
/// when no tap is active — returns `SDR_CORE_OK` in that case
/// (idempotent teardown). Blocks until the dispatcher thread has
/// joined, so by the time this returns the host can safely free
/// the `user_data` passed at start time.
///
/// # Safety
///
/// Must NOT be called from inside the tap callback (would
/// self-deadlock on the dispatcher join).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_stop_audio_tap(handle: *mut SdrCore) -> i32 {
    let result = std::panic::catch_unwind(|| {
        // SAFETY: caller contract matches `SdrCore::from_raw_mut`.
        let Some(core) = (unsafe { SdrCore::from_raw_mut(handle) }) else {
            set_last_error("sdr_core_stop_audio_tap: null or invalid handle");
            return SdrCoreError::InvalidHandle.as_int();
        };

        stop_audio_tap_internal(core)
    });
    match result {
        Ok(code) => code,
        Err(payload) => {
            set_last_error(format!(
                "sdr_core_stop_audio_tap: panic: {}",
                panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

/// Shared stop path — used by both the FFI `sdr_core_stop_audio_tap`
/// and by `sdr_core_destroy` (which must stop the tap before
/// dropping the engine to avoid the dispatcher thread outliving
/// the host's user_data).
pub(crate) fn stop_audio_tap_internal(core: &mut SdrCore) -> i32 {
    // Disable the engine tap — DSP thread drops its sender, which
    // will cause the dispatcher's `rx.recv()` to return Err once
    // any in-flight chunks are drained.
    //
    // Note: we call this BEFORE taking the JoinHandle below so
    // that even if the engine send fails (engine already torn
    // down), we still join a dispatcher thread whose channel will
    // naturally disconnect through the same path.
    let _ = core.engine.send_command(UiToDsp::DisableAudioTap);

    // Take the JoinHandle and join it. `take()` leaves None in
    // the slot so a second `stop_audio_tap` call is a no-op.
    let handle = if let Ok(mut guard) = core.audio_tap_dispatcher.lock() {
        guard.take()
    } else {
        set_last_error("sdr_core_stop_audio_tap: dispatcher mutex poisoned");
        return SdrCoreError::Internal.as_int();
    };

    if let Some(h) = handle
        && let Err(e) = h.join()
    {
        set_last_error(format!(
            "sdr_core_stop_audio_tap: dispatcher panic: {}",
            panic_message(&e)
        ));
        return SdrCoreError::Internal.as_int();
    }

    clear_last_error();
    SdrCoreError::Ok.as_int()
}
