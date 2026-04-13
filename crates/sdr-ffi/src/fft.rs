//! FFT frame pull — `sdr_core_pull_fft`.
//!
//! Unlike the per-event callback surface in [`crate::event`], FFT
//! frames are delivered on the host's render tick via a **pull**
//! function: the host calls `sdr_core_pull_fft` from inside its
//! render loop (SwiftUI's `MTKView::draw(in:)` on the Metal path,
//! `glib::timeout_add_local` on the GTK path), and the call
//! synchronously invokes a host-supplied callback with a borrowed
//! slice of the most recent FFT frame — or returns `false` without
//! calling the callback when no new frame has arrived since the
//! previous pull.
//!
//! The borrowed slice is valid only for the duration of the
//! callback. Hosts that want to retain the data must copy it out.
//!
//! Rationale for a pull model instead of pushing FFT frames
//! through the event callback like every other engine message:
//!
//! - Rendering happens at display rate (usually 60 fps); FFT
//!   generation happens at the engine's internal rate (default
//!   20 fps). Pushing every frame through the event callback
//!   would force a full struct-translation + mutex-hold +
//!   allocation (to preserve the borrow through the dispatcher
//!   thread's stack frame) for data that might be dropped before
//!   the renderer even gets to it. Pulling means zero work on
//!   any tick where there's no new frame, and zero cross-thread
//!   traffic on the hot path — the shared-FFT-buffer synchro-
//!   nization already exists in `sdr-core::SharedFftBuffer`.
//!
//! - Rendering on the GPU wants the data *on* the main thread
//!   (where the Metal command encoder lives), not on whatever
//!   arbitrary thread the event dispatcher happens to be running.
//!   A pull function called from the render loop runs on the
//!   right thread by construction.

use std::ffi::c_void;

use crate::error::{clear_last_error, set_last_error};
use crate::handle::SdrCore;
use crate::lifecycle::panic_message;

/// Frame descriptor handed to the host callback.
///
/// `magnitudes_db` points into `sdr-core`'s `SharedFftBuffer`
/// and is valid only for the duration of the callback. `len` is
/// the number of bins (matches the currently-configured FFT size).
/// `sample_rate_hz` and `center_freq_hz` are supplied as metadata
/// so the host doesn't need to separately track them against the
/// event stream — they're the effective (post-decimation) sample
/// rate and the tuner center frequency as the DSP controller saw
/// them at the moment the frame was published.
///
/// v1 sets both metadata fields to `0.0` for now: the engine
/// doesn't thread this context alongside the FFT frame today
/// (`SharedFftBuffer` just holds the magnitude slice). Future
/// work threads the metadata through — until then, hosts that
/// care about the rate/center should correlate with the
/// `SDR_EVT_SAMPLE_RATE_CHANGED` event. The field is exposed in
/// the struct so adding the thread-through later doesn't require
/// an ABI change.
#[repr(C)]
pub struct SdrFftFrame {
    pub magnitudes_db: *const f32,
    pub len: usize,
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
}

/// Host callback signature. Fires synchronously from within
/// `sdr_core_pull_fft` when a new frame is available.
///
/// The `frame` pointer is valid only for the duration of the
/// call; the `magnitudes_db` slice inside it references the
/// FFT ring buffer mutex held for the duration of the callback.
/// `user_data` is the opaque pointer the host passed to
/// `sdr_core_pull_fft` — handed back unchanged.
pub type SdrFftCallback =
    Option<unsafe extern "C" fn(frame: *const SdrFftFrame, user_data: *mut c_void)>;

/// Pull the latest FFT frame, if a new one is available.
///
/// Returns `true` and invokes `callback` synchronously when a
/// new frame is available. Returns `false` without calling
/// `callback` when no new frame has arrived since the previous
/// pull.
///
/// Lock-free fast path when no new frame is available; acquires
/// the shared FFT buffer's mutex only for the short `memcpy`
/// window when a frame is being handed to the callback.
///
/// # Safety
///
/// `handle` must be non-null and valid (see `sdr_core_create`).
/// `callback` is a nullable function pointer; passing null is
/// equivalent to "check if a frame is ready but don't do
/// anything with it" — the function returns whether a frame
/// existed but does not invoke any callback. `user_data` is
/// opaque and not dereferenced by the FFI side.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_pull_fft(
    handle: *mut SdrCore,
    callback: SdrFftCallback,
    user_data: *mut c_void,
) -> bool {
    let result = std::panic::catch_unwind(|| {
        // SAFETY: caller contract.
        let Some(core) = (unsafe { SdrCore::from_raw(handle) }) else {
            set_last_error("sdr_core_pull_fft: null handle");
            return false;
        };

        let mut fired = false;

        let was_ready = core.engine.pull_fft(|data| {
            let frame = SdrFftFrame {
                magnitudes_db: data.as_ptr(),
                len: data.len(),
                // v1: metadata not yet threaded through the
                // SharedFftBuffer. See the struct docs.
                sample_rate_hz: 0.0,
                center_freq_hz: 0.0,
            };

            if let Some(cb) = callback {
                // Wrap the host callback in catch_unwind so a
                // panicking host doesn't propagate up through
                // the shared-buffer mutex (which would leave
                // the mutex poisoned for the next pull).
                let frame_ptr: *const SdrFftFrame = &raw const frame;
                let cb_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // SAFETY: cb is a C callback; frame_ptr is
                    // valid for the callback duration because
                    // `frame` lives on this closure's stack
                    // frame, which outlives the call.
                    unsafe { cb(frame_ptr, user_data) };
                }));
                if cb_result.is_err() {
                    tracing::warn!("sdr_core_pull_fft: host callback panicked (swallowed)");
                }
            }
            fired = true;
        });

        // `was_ready` tells us whether `pull_fft` took the lock
        // and called our closure; `fired` tells us whether we
        // successfully handed a frame to the host callback (it
        // matches `was_ready` unless the caller passed null for
        // the callback, in which case we still return true to
        // signal "a frame existed").
        let _ = fired;
        clear_last_error();
        was_ready
    });

    match result {
        Ok(b) => b,
        Err(payload) => {
            set_last_error(format!(
                "sdr_core_pull_fft: panic: {}",
                panic_message(&payload)
            ));
            false
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::lifecycle::{sdr_core_create, sdr_core_destroy};
    use std::ffi::CString;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_handle() -> *mut SdrCore {
        let path = CString::new("").unwrap();
        let mut handle: *mut SdrCore = std::ptr::null_mut();
        let rc = unsafe { sdr_core_create(path.as_ptr(), &raw mut handle) };
        assert_eq!(rc, 0);
        handle
    }

    static PULL_COUNTER: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counting_pull_cb(_frame: *const SdrFftFrame, _user_data: *mut c_void) {
        PULL_COUNTER.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn pull_fft_null_handle_returns_false() {
        let got = unsafe {
            sdr_core_pull_fft(
                std::ptr::null_mut(),
                Some(counting_pull_cb),
                std::ptr::null_mut(),
            )
        };
        assert!(!got);
    }

    #[test]
    fn pull_fft_with_no_frame_returns_false_and_does_not_fire_callback() {
        // Fresh engine has never produced an FFT frame — the
        // SharedFftBuffer is empty, so pull_fft returns false
        // without calling our callback.
        PULL_COUNTER.store(0, Ordering::Relaxed);

        let h = make_handle();
        let got = unsafe { sdr_core_pull_fft(h, Some(counting_pull_cb), std::ptr::null_mut()) };
        assert!(!got, "no FFT frame should be available on a fresh engine");
        assert_eq!(PULL_COUNTER.load(Ordering::Relaxed), 0);

        unsafe { sdr_core_destroy(h) };
    }

    #[test]
    fn pull_fft_with_null_callback_is_allowed() {
        // Null callback is "probe only" — returns whether a
        // frame is available without handing it to anyone. On a
        // fresh engine the answer is `false`.
        let h = make_handle();
        let got = unsafe { sdr_core_pull_fft(h, None, std::ptr::null_mut()) };
        assert!(!got);
        unsafe { sdr_core_destroy(h) };
    }

    #[test]
    fn fft_frame_struct_is_abi_sized() {
        // On 64-bit targets: *const f32 + usize + f64 + f64 =
        // 8 + 8 + 8 + 8 = 32 bytes. Locks in the layout so
        // accidental reordering or field addition is caught at
        // test time, not at first Swift call.
        assert_eq!(std::mem::size_of::<SdrFftFrame>(), 32);
    }
}
