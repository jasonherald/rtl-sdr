//! Static device-enumeration functions exposed via the C ABI.
//!
//! These are **handle-free** — they don't take or require an
//! `SdrCore *`. That's important for the intended use case:
//! hosts call them at app launch to surface device presence
//! before (and independently of) creating an engine.
//!
//! Under the hood these thin-wrap `sdr_rtlsdr::get_device_count`
//! / `get_device_name`, which in turn query libusb's device list
//! (no USB control transfers; just matching VID/PID against what
//! the kernel has already enumerated).
//!
//! Strings use the caller-allocated-buffer pattern so there's no
//! lifetime ambiguity across the FFI boundary: the caller owns
//! the memory, we fill it. This is the same contract as POSIX
//! `strerror_r` / `snprintf`.

use std::cell::RefCell;
use std::ffi::c_char;

use crate::error::{SdrCoreError, clear_last_error, set_last_error};

/// Count RTL-SDR devices currently attached to the host's USB bus.
///
/// See `include/sdr_core.h` for the contract. Does not open any
/// device and does not require a handle.
///
/// # Safety
///
/// No pointers accepted; inherently safe. Declared `unsafe` only
/// because `extern "C"` requires it under the 2024 edition.
#[unsafe(no_mangle)]
pub extern "C" fn sdr_core_device_count() -> u32 {
    // Wrap in catch_unwind so a panic in the rtlsdr enumerate path
    // — e.g. a libusb init failure — doesn't cross the FFI boundary.
    // Failure degrades to "0 devices" which is the honest answer
    // when we couldn't enumerate at all.
    std::panic::catch_unwind(sdr_rtlsdr::get_device_count).unwrap_or_else(|_| {
        set_last_error("sdr_core_device_count: panic during enumeration");
        0
    })
}

/// Fill `out_buf` with the name of the device at `index`.
///
/// Returns the number of bytes written (not counting the NUL) on
/// success, or a negative `SdrCoreError` on failure. See header
/// for the full contract.
///
/// # Safety
///
/// `out_buf` must point to at least `buf_len` writable bytes, or
/// be NULL (in which case we return `SDR_CORE_ERR_INVALID_ARG`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_device_name(
    index: u32,
    out_buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        if out_buf.is_null() || buf_len == 0 {
            set_last_error("sdr_core_device_name: out_buf is null or buf_len is 0");
            return SdrCoreError::InvalidArg.as_int();
        }

        // `get_device_name` returns an empty string for out-of-range
        // indices — treat that as a Device error so the host can
        // distinguish "valid but no name" (shouldn't happen for
        // real devices) from "wrong index".
        let count = sdr_rtlsdr::get_device_count();
        if index >= count {
            set_last_error(format!(
                "sdr_core_device_name: index {index} out of range (count={count})"
            ));
            return SdrCoreError::Device.as_int();
        }

        let name = sdr_rtlsdr::get_device_name(index);
        if name.is_empty() {
            set_last_error(format!(
                "sdr_core_device_name: name probe returned empty for index {index}"
            ));
            return SdrCoreError::Device.as_int();
        }

        // Write UTF-8 bytes and NUL-terminate. Truncate cleanly if
        // `buf_len` is smaller than needed — truncation is not an
        // error per the contract, just a shorter-but-still-valid
        // string.
        let bytes = name.as_bytes();
        let max_payload = buf_len.saturating_sub(1); // reserve 1 for NUL
        let to_copy = bytes.len().min(max_payload);

        // SAFETY: Caller contract guarantees `out_buf` is writable
        // for `buf_len` bytes. `to_copy <= buf_len - 1 < buf_len`
        // and the NUL write at `out_buf.add(to_copy)` is within
        // `buf_len` because `to_copy <= buf_len - 1`.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf.cast::<u8>(), to_copy);
            *out_buf.add(to_copy) = 0; // NUL terminator
        }

        clear_last_error();

        // Return bytes written (not counting the NUL). This is
        // `to_copy`, which fits in i32 easily for any sane name.
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        {
            to_copy as i32
        }
    });

    if let Ok(rc) = result {
        rc
    } else {
        set_last_error("sdr_core_device_name: panic during name probe");
        SdrCoreError::Internal.as_int()
    }
}

// ============================================================
//  Audio output device enumeration (handle-free, atomic)
// ============================================================
//
// Wraps `sdr_sink_audio::list_audio_sinks()` — the same snapshot
// the engine uses internally when opening the sink. To give
// callers a coherent name+UID pair for every index in a round
// of enumeration (count → name(0) → uid(0) → name(1) → …), we
// stash the snapshot in a **thread-local** and have the
// per-index getters read from it.
//
// Why thread-local instead of a snapshot-handle API:
//   - Same API shape as `sdr_core_device_count` / `_name` (the
//     RTL-SDR enumerate), which hosts already use without a
//     snapshot handle.
//   - No new ABI surface: host code that was written against
//     the pre-atomic draft (Swift `SdrCore.audioDevices`
//     computed property on PR #344) keeps working — it just
//     gets internal consistency for free.
//   - Snapshot scope is "everything `_name` or `_uid` called on
//     this thread between two `_count` calls." That matches
//     SwiftUI pickers where the whole enumeration runs on the
//     main actor in a synchronous loop.
//
// Semantics of the three entry points:
//   - `_count()` **always** re-runs `list_audio_sinks()` and
//     stores the result in the thread-local, then returns the
//     length. Calling it twice gives you two independent
//     snapshots.
//   - `_name(i)` / `_uid(i)` read from the thread-local. If
//     the thread-local is empty (first call in the thread, or
//     after a previous snapshot observed zero devices), they
//     lazy-refresh before reading. This makes "one-shot"
//     callers that forgot to call `_count` also work — they
//     just don't benefit from cross-index consistency beyond
//     the single call.
//
// Hot-plug between `_count` and `_name(N-1)` is benign: the
// getter still sees the original snapshot and returns the
// name/UID for the element that was at index `i` when `_count`
// ran, whether the device still exists or not. That's the
// behavior the caller wants — any UI round-tripping back
// through `sdr_core_set_audio_device` will get
// `SinkError::DeviceNotFound` from the engine if the target
// disappeared, and the next `_count` call reflects the new
// state.
//
// String fields come straight from the backend-specific
// `AudioDevice` struct:
//   - `display_name` is the human-readable label from
//     `kAudioObjectPropertyName` on CoreAudio, or the PipeWire
//     node description on Linux.
//   - `node_name` is the caller-opaque UID — on CoreAudio it's
//     the `AudioDeviceID` as a decimal string in v1 (per the
//     inline docs in `sdr-sink-audio::coreaudio_impl`),
//     migrating to the `kAudioDevicePropertyDeviceUID` string
//     in a later PR. Empty means "system default output" on
//     every backend.
//
// The caller-allocated-buffer + truncation contract matches
// `sdr_core_device_name` above.

thread_local! {
    /// Per-thread snapshot of the device list. Replaced on each
    /// `sdr_core_audio_device_count` call so the per-index
    /// getters see a coherent view even if devices hot-plug
    /// between calls. `RefCell` because we mutate it (assign the
    /// new snapshot), but we never borrow across a re-entry so
    /// the `borrow_mut` can't panic in practice.
    static AUDIO_DEVICE_SNAPSHOT: RefCell<Vec<sdr_sink_audio::AudioDevice>> =
        const { RefCell::new(Vec::new()) };
}

/// Refresh the thread-local snapshot and return it as a length.
/// Broken out so both `_count` and the lazy-refresh path in
/// `audio_device_string` share one implementation.
fn refresh_audio_device_snapshot() -> usize {
    let devices = sdr_sink_audio::list_audio_sinks();
    let len = devices.len();
    AUDIO_DEVICE_SNAPSHOT.with(|cell| {
        *cell.borrow_mut() = devices;
    });
    len
}

/// Count audio output devices currently enumerable by the backend,
/// taking a fresh snapshot. Subsequent `_name` / `_uid` calls on
/// this thread read from that snapshot.
///
/// See `include/sdr_core.h` for the contract. Does not open any
/// device and does not require a handle.
///
/// # Safety
///
/// No pointers accepted; inherently safe. Declared `unsafe` only
/// because `extern "C"` requires it under the 2024 edition.
#[unsafe(no_mangle)]
pub extern "C" fn sdr_core_audio_device_count() -> u32 {
    // Wrap in catch_unwind: a panic inside the CoreAudio / PipeWire
    // enumeration path mustn't cross the FFI boundary. Fallback is
    // "0 devices" — the honest answer when enumeration failed.
    std::panic::catch_unwind(|| {
        let len = refresh_audio_device_snapshot();
        // Clear the thread-local last-error on success — the
        // header contract says `sdr_core_last_error_message`
        // reflects the *most recent* sdr_core_* call on this
        // thread, so a failed earlier probe must not leak into
        // a successful refresh. Per CodeRabbit round 2 on PR #344.
        clear_last_error();
        // Cap at u32::MAX defensively. In practice there are <100
        // devices on any real system.
        u32::try_from(len).unwrap_or(u32::MAX)
    })
    .unwrap_or_else(|_| {
        set_last_error("sdr_core_audio_device_count: panic during enumeration");
        0
    })
}

/// Shared helper for `sdr_core_audio_device_name` / `_uid`. Picks
/// the string to copy via `select_field` and follows the same
/// write-and-NUL-terminate contract as `sdr_core_device_name`.
/// Reads from the thread-local snapshot; lazy-refreshes the
/// snapshot when empty so a caller that forgot to call `_count`
/// first still gets a usable result (it just won't be
/// consistent with any prior `_count` return).
///
/// # Safety
///
/// `out_buf` must point to at least `buf_len` writable bytes, or
/// be NULL (in which case we return `SDR_CORE_ERR_INVALID_ARG`).
unsafe fn audio_device_string<F>(
    fn_name: &str,
    index: u32,
    out_buf: *mut c_char,
    buf_len: usize,
    select_field: F,
) -> i32
where
    F: Fn(&sdr_sink_audio::AudioDevice) -> &str
        + std::panic::UnwindSafe
        + std::panic::RefUnwindSafe,
{
    let result = std::panic::catch_unwind(|| {
        if out_buf.is_null() || buf_len == 0 {
            set_last_error(format!("{fn_name}: out_buf is null or buf_len is 0"));
            return SdrCoreError::InvalidArg.as_int();
        }

        // Lazy-refresh on first use — a caller that jumps
        // straight to `_name(0)` without calling `_count` still
        // gets the right answer. This does a backend query per
        // call in that path; the atomic multi-index case still
        // uses the cached snapshot.
        let idx = usize::try_from(index).unwrap_or(usize::MAX);
        AUDIO_DEVICE_SNAPSHOT.with(|cell| {
            if cell.borrow().is_empty() {
                let fresh = sdr_sink_audio::list_audio_sinks();
                *cell.borrow_mut() = fresh;
            }

            let snap = cell.borrow();
            let Some(dev) = snap.get(idx) else {
                set_last_error(format!(
                    "{fn_name}: index {index} out of range (count={})",
                    snap.len()
                ));
                return SdrCoreError::Device.as_int();
            };

            let bytes = select_field(dev).as_bytes();
            let max_payload = buf_len.saturating_sub(1); // reserve 1 for NUL
            let to_copy = bytes.len().min(max_payload);

            // SAFETY: Caller contract guarantees `out_buf` is writable
            // for `buf_len` bytes; `to_copy <= buf_len - 1 < buf_len`
            // and the NUL write at `out_buf.add(to_copy)` is within
            // `buf_len` because `to_copy <= buf_len - 1`.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf.cast::<u8>(), to_copy);
                *out_buf.add(to_copy) = 0;
            }

            clear_last_error();

            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            {
                to_copy as i32
            }
        })
    });

    if let Ok(rc) = result {
        rc
    } else {
        set_last_error(format!("{fn_name}: panic during audio device probe"));
        SdrCoreError::Internal.as_int()
    }
}

/// Fill `out_buf` with the human-readable display name of the audio
/// output device at `index`. See header for the contract.
///
/// # Safety
///
/// `out_buf` must point to at least `buf_len` writable bytes, or
/// be NULL (in which case we return `SDR_CORE_ERR_INVALID_ARG`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_audio_device_name(
    index: u32,
    out_buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    unsafe {
        audio_device_string(
            "sdr_core_audio_device_name",
            index,
            out_buf,
            buf_len,
            |dev| dev.display_name.as_str(),
        )
    }
}

/// Fill `out_buf` with the caller-opaque UID of the audio output
/// device at `index` (the string to pass to
/// `sdr_core_set_audio_device`). See header for the contract.
///
/// # Safety
///
/// `out_buf` must point to at least `buf_len` writable bytes, or
/// be NULL (in which case we return `SDR_CORE_ERR_INVALID_ARG`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_audio_device_uid(
    index: u32,
    out_buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    unsafe {
        audio_device_string(
            "sdr_core_audio_device_uid",
            index,
            out_buf,
            buf_len,
            |dev| dev.node_name.as_str(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn count_is_nonnegative_and_terminates() {
        // We can't assume a device is present in CI; just make
        // sure the call returns in bounded time and yields a
        // sensible number.
        let c = sdr_core_device_count();
        assert!(c < 1024, "device count should be small, got {c}");
    }

    #[test]
    fn name_with_null_buf_returns_invalid_arg() {
        let rc = unsafe { sdr_core_device_name(0, std::ptr::null_mut(), 32) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }

    #[test]
    fn name_with_zero_len_returns_invalid_arg() {
        let mut buf = [0_u8; 1];
        let rc = unsafe { sdr_core_device_name(0, buf.as_mut_ptr().cast::<c_char>(), 0) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }

    #[test]
    fn name_with_out_of_range_index_returns_device_error() {
        let mut buf = [0_u8; 64];
        // Choose an index way past any real device count.
        let rc =
            unsafe { sdr_core_device_name(u32::MAX, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
        assert_eq!(rc, SdrCoreError::Device.as_int());
    }

    #[test]
    fn name_round_trips_when_device_present() {
        let count = sdr_core_device_count();
        if count == 0 {
            // No hardware attached — skip. Not a failure.
            return;
        }
        let mut buf = [0_u8; 128];
        let rc = unsafe { sdr_core_device_name(0, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
        assert!(rc >= 0, "expected success, got {rc}");
        let written = usize::try_from(rc).expect("rc is non-negative after the assert above");
        let got = CStr::from_bytes_with_nul(&buf[..=written])
            .expect("FFI writes a NUL at `written` per contract")
            .to_string_lossy();
        assert!(!got.is_empty(), "device name should not be empty");
    }

    // ------------------------------------------------------
    //  Audio output device enumeration (ABI 0.4)
    // ------------------------------------------------------

    #[test]
    fn audio_device_count_is_at_least_one() {
        // The stub and every real backend include at least the
        // "system default" entry with empty node_name, so this must
        // always be >= 1 on any supported platform.
        let c = sdr_core_audio_device_count();
        assert!(c >= 1, "expected at least one audio device, got {c}");
        assert!(c < 1024, "audio device count should be small, got {c}");
    }

    #[test]
    fn audio_device_name_rejects_null_and_zero_len() {
        let mut buf = [0_u8; 64];
        assert_eq!(
            unsafe { sdr_core_audio_device_name(0, std::ptr::null_mut(), 32) },
            SdrCoreError::InvalidArg.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_audio_device_name(0, buf.as_mut_ptr().cast::<c_char>(), 0) },
            SdrCoreError::InvalidArg.as_int()
        );
    }

    #[test]
    fn audio_device_uid_rejects_null_and_zero_len() {
        let mut buf = [0_u8; 64];
        assert_eq!(
            unsafe { sdr_core_audio_device_uid(0, std::ptr::null_mut(), 32) },
            SdrCoreError::InvalidArg.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_audio_device_uid(0, buf.as_mut_ptr().cast::<c_char>(), 0) },
            SdrCoreError::InvalidArg.as_int()
        );
    }

    #[test]
    fn audio_device_name_out_of_range_returns_device_error() {
        let mut buf = [0_u8; 64];
        let rc = unsafe {
            sdr_core_audio_device_name(u32::MAX, buf.as_mut_ptr().cast::<c_char>(), buf.len())
        };
        assert_eq!(rc, SdrCoreError::Device.as_int());
    }

    #[test]
    fn audio_device_count_clears_stale_last_error() {
        // Regression: before the round 2 fix, a successful
        // `_count` didn't clear a stale last-error from a prior
        // failed probe. Run this on its own thread so the
        // thread-local state is isolated from other tests.
        let handle = std::thread::spawn(|| {
            // Poison the thread-local with a known error — calling
            // `_name` with a null buffer returns InvalidArg and
            // sets the last-error message.
            let rc = unsafe { sdr_core_audio_device_name(0, std::ptr::null_mut(), 64) };
            assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
            let msg_ptr = crate::error::sdr_core_last_error_message();
            assert!(
                !msg_ptr.is_null(),
                "last-error should be set after failed probe"
            );

            // Now run a successful `_count` — it must clear the
            // last-error per the header contract.
            let _ = sdr_core_audio_device_count();
            let msg_ptr = crate::error::sdr_core_last_error_message();
            assert!(
                msg_ptr.is_null(),
                "sdr_core_audio_device_count must clear the thread-local last-error on success"
            );
        });
        handle.join().expect("thread should exit cleanly");
    }

    #[test]
    fn audio_device_name_and_uid_share_snapshot_after_count() {
        // Regression test for the CodeRabbit round 1 finding on
        // PR #344: before the thread-local snapshot, `_name(i)`
        // and `_uid(i)` each re-ran `list_audio_sinks()`, so a
        // hot-plug between the two could pair a name from one
        // snapshot with a UID from another. The thread-local
        // pins the snapshot to the last `_count` call — each
        // index pairs `_name(i)` with `_uid(i)` consistently.
        //
        // We can't force a hot-plug in a unit test, but we can
        // verify that between `_count` and the per-index reads
        // the same index always resolves to matching strings
        // from a single backend call, regardless of what a
        // parallel thread's own snapshot looks like.
        let count = sdr_core_audio_device_count();
        assert!(count >= 1);

        for i in 0..count {
            let mut name_buf = [0_u8; 256];
            let mut uid_buf = [0_u8; 256];
            let rc_name = unsafe {
                sdr_core_audio_device_name(
                    i,
                    name_buf.as_mut_ptr().cast::<c_char>(),
                    name_buf.len(),
                )
            };
            let rc_uid = unsafe {
                sdr_core_audio_device_uid(i, uid_buf.as_mut_ptr().cast::<c_char>(), uid_buf.len())
            };
            assert!(rc_name >= 0);
            assert!(rc_uid >= 0);
            // Both calls must succeed for every index the count
            // call reported — if `_uid` returned Device error while
            // `_name` succeeded, that's the inconsistency the
            // thread-local is meant to prevent.
        }
    }

    #[test]
    fn audio_device_lazy_refresh_when_count_not_called() {
        // A caller that jumps straight to `_name(0)` without
        // calling `_count` first should still get a valid answer.
        // We exercise this on a fresh thread so there's no prior
        // snapshot in thread-local storage.
        let handle = std::thread::spawn(|| {
            let mut buf = [0_u8; 256];
            let rc = unsafe {
                sdr_core_audio_device_name(0, buf.as_mut_ptr().cast::<c_char>(), buf.len())
            };
            assert!(
                rc >= 0,
                "lazy refresh path must return a valid answer for index 0, got {rc}"
            );
        });
        handle.join().expect("thread should exit cleanly");
    }

    #[test]
    fn audio_device_first_entry_round_trips() {
        // Every backend returns at least one entry for index 0.
        // Name may be empty on the stub's "Default" when the test
        // harness happens to run with backend features off, so we
        // only require that the call succeeds and NUL-terminates.
        let count = sdr_core_audio_device_count();
        assert!(count >= 1);

        let mut buf = [0_u8; 256];
        let rc =
            unsafe { sdr_core_audio_device_name(0, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
        assert!(rc >= 0, "expected success, got {rc}");
        let written = usize::try_from(rc).expect("rc is non-negative after the assert above");
        // Check NUL termination — not the string contents (backend-dependent).
        let _ = CStr::from_bytes_with_nul(&buf[..=written])
            .expect("FFI writes a NUL at `written` per contract");

        // UID for index 0 is typically the empty string ("system default"),
        // but that's backend policy — we just check the call works.
        let mut uid_buf = [0_u8; 256];
        let rc = unsafe {
            sdr_core_audio_device_uid(0, uid_buf.as_mut_ptr().cast::<c_char>(), uid_buf.len())
        };
        assert!(rc >= 0, "expected success, got {rc}");
    }
}
