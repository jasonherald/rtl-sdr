//! Command C ABI: the ~20 typed `sdr_core_*` entry points that wrap
//! [`sdr_core::Engine::send_command`] for a single [`UiToDsp`] variant
//! each. Every function has the same shape:
//!
//! 1. Wrap the body in `catch_unwind` so a Rust panic across the FFI
//!    boundary becomes `SDR_CORE_ERR_INTERNAL` instead of UB.
//! 2. Validate the handle pointer (null → `InvalidHandle`).
//! 3. Validate arguments where the engine won't (NaN/infinity floats,
//!    out-of-range enum ints, etc.).
//! 4. Call `engine.send_command(UiToDsp::…)`.
//! 5. Return `SDR_CORE_OK` (and clear the last-error) on success, or
//!    a negative error code (and set the last-error message) on
//!    failure.
//!
//! All of that boilerplate lives in the [`with_core`] helper below —
//! each public function is then just a few lines dispatching to a
//! closure that builds the `UiToDsp` variant. Macros were rejected
//! because CodeRabbit sometimes struggles with macro-generated FFI
//! surfaces; explicit per-function code is easier to review.
//!
//! Mirrors the "Commands" section of `include/sdr_core.h`. Enum
//! values are defined here as `i32` constants with the same names
//! and values the header uses, so changing one forces a change to
//! the other (and the `make ffi-header-check` drift linter in a
//! later checkpoint catches any divergence).

use std::ffi::{CStr, c_char};
use std::path::PathBuf;

use sdr_core::UiToDsp;
use sdr_pipeline::iq_frontend::FftWindow;
use sdr_radio::DeemphasisMode;
use sdr_types::DemodMode;

use crate::error::{SdrCoreError, clear_last_error, set_last_error};
use crate::handle::SdrCore;
use crate::lifecycle::panic_message;

/// Shared boilerplate wrapper for every command function.
///
/// Handles: panic catch, handle validation, last-error bookkeeping,
/// and success/failure translation to the `i32` return code.
///
/// The closure receives a borrow of the validated [`SdrCore`] and
/// returns `Result<(), SdrCoreError>`. On `Err` the closure is
/// expected to have already called [`set_last_error`] with a
/// human-readable message; `with_core` just returns the code.
///
/// # Safety
///
/// `handle` must be either null or a pointer previously returned by
/// `sdr_core_create` and not yet destroyed. The closure is passed a
/// valid `&SdrCore`; it must not retain the reference beyond its
/// own scope.
unsafe fn with_core<F>(handle: *mut SdrCore, f: F) -> i32
where
    F: FnOnce(&SdrCore) -> Result<(), SdrCoreError> + std::panic::UnwindSafe,
{
    let result = std::panic::catch_unwind(|| {
        // SAFETY: caller contract mirrors `SdrCore::from_raw`.
        let Some(core) = (unsafe { SdrCore::from_raw(handle) }) else {
            set_last_error("sdr_core: null or invalid handle");
            return SdrCoreError::InvalidHandle.as_int();
        };
        match f(core) {
            Ok(()) => {
                clear_last_error();
                SdrCoreError::Ok.as_int()
            }
            Err(e) => e.as_int(),
        }
    });
    match result {
        Ok(code) => code,
        Err(payload) => {
            set_last_error(format!(
                "sdr_core command: panic: {}",
                panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

/// Helper used by every command that forwards a `UiToDsp` variant
/// to the engine. Translates a failed `send_command` (channel
/// disconnected — engine already torn down) into
/// `SdrCoreError::NotRunning` with a descriptive message.
fn send(core: &SdrCore, cmd: UiToDsp) -> Result<(), SdrCoreError> {
    match core.engine.send_command(cmd) {
        Ok(()) => Ok(()),
        Err(err) => {
            set_last_error(format!("engine.send_command: {err}"));
            Err(SdrCoreError::NotRunning)
        }
    }
}

/// Helper for commands that take a floating-point value that must
/// be finite. Returns `InvalidArg` (with last-error set) if the
/// value is NaN or infinite.
fn require_finite(name: &str, v: f64) -> Result<(), SdrCoreError> {
    if v.is_finite() {
        Ok(())
    } else {
        set_last_error(format!("{name}: value must be finite, got {v}"));
        Err(SdrCoreError::InvalidArg)
    }
}

// ============================================================
//  DemodMode C enum ↔ Rust enum
// ============================================================
//
// Discriminants must match `SdrDemodMode` in `include/sdr_core.h`.

pub const SDR_DEMOD_WFM: i32 = 0;
pub const SDR_DEMOD_NFM: i32 = 1;
pub const SDR_DEMOD_AM: i32 = 2;
pub const SDR_DEMOD_USB: i32 = 3;
pub const SDR_DEMOD_LSB: i32 = 4;
pub const SDR_DEMOD_DSB: i32 = 5;
pub const SDR_DEMOD_CW: i32 = 6;
pub const SDR_DEMOD_RAW: i32 = 7;

fn demod_mode_from_c(v: i32) -> Option<DemodMode> {
    match v {
        SDR_DEMOD_WFM => Some(DemodMode::Wfm),
        SDR_DEMOD_NFM => Some(DemodMode::Nfm),
        SDR_DEMOD_AM => Some(DemodMode::Am),
        SDR_DEMOD_USB => Some(DemodMode::Usb),
        SDR_DEMOD_LSB => Some(DemodMode::Lsb),
        SDR_DEMOD_DSB => Some(DemodMode::Dsb),
        SDR_DEMOD_CW => Some(DemodMode::Cw),
        SDR_DEMOD_RAW => Some(DemodMode::Raw),
        _ => None,
    }
}

// ============================================================
//  Deemphasis C enum ↔ Rust enum
// ============================================================
//
// Note: the variant order mirrors the C header's `SdrDeemphasis`
// enum (None=0, Us75=1, Eu50=2). The Rust `DeemphasisMode` enum in
// `sdr-radio` declares the variants in a different source order but
// that doesn't affect the ABI — these constants are the contract.

pub const SDR_DEEMPH_NONE: i32 = 0;
pub const SDR_DEEMPH_US75: i32 = 1;
pub const SDR_DEEMPH_EU50: i32 = 2;

fn deemphasis_from_c(v: i32) -> Option<DeemphasisMode> {
    match v {
        SDR_DEEMPH_NONE => Some(DeemphasisMode::None),
        SDR_DEEMPH_US75 => Some(DeemphasisMode::Us75),
        SDR_DEEMPH_EU50 => Some(DeemphasisMode::Eu50),
        _ => None,
    }
}

// ============================================================
//  FftWindow C enum ↔ Rust enum
// ============================================================
//
// The `sdr-pipeline::iq_frontend::FftWindow` enum only has three
// variants (Rectangular, Blackman, Nuttall) — no Hann/Hamming
// despite what the FFI spec sketch showed. The C enum matches the
// actual Rust variants. Spec deviation documented in the PR body.

pub const SDR_FFT_WIN_RECT: i32 = 0;
pub const SDR_FFT_WIN_BLACKMAN: i32 = 1;
pub const SDR_FFT_WIN_NUTTALL: i32 = 2;

fn fft_window_from_c(v: i32) -> Option<FftWindow> {
    match v {
        SDR_FFT_WIN_RECT => Some(FftWindow::Rectangular),
        SDR_FFT_WIN_BLACKMAN => Some(FftWindow::Blackman),
        SDR_FFT_WIN_NUTTALL => Some(FftWindow::Nuttall),
        _ => None,
    }
}

// ============================================================
//  Lifecycle (start / stop)
// ============================================================

/// Start the engine's source. See `include/sdr_core.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_start(handle: *mut SdrCore) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::Start)) }
}

/// Stop the engine's source. See `include/sdr_core.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_stop(handle: *mut SdrCore) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::Stop)) }
}

// ============================================================
//  Tuning
// ============================================================

/// Tune to `freq_hz`. Value must be finite.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_tune(handle: *mut SdrCore, freq_hz: f64) -> i32 {
    unsafe {
        with_core(handle, |core| {
            require_finite("sdr_core_tune", freq_hz)?;
            send(core, UiToDsp::Tune(freq_hz))
        })
    }
}

/// Set the VFO offset from the tuner center in Hz. Value must be finite.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_vfo_offset(handle: *mut SdrCore, offset_hz: f64) -> i32 {
    unsafe {
        with_core(handle, |core| {
            require_finite("sdr_core_set_vfo_offset", offset_hz)?;
            send(core, UiToDsp::SetVfoOffset(offset_hz))
        })
    }
}

/// Set the tuner sample rate in Hz. Value must be finite and positive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_sample_rate(handle: *mut SdrCore, rate_hz: f64) -> i32 {
    unsafe {
        with_core(handle, |core| {
            require_finite("sdr_core_set_sample_rate", rate_hz)?;
            if rate_hz <= 0.0 {
                set_last_error(format!(
                    "sdr_core_set_sample_rate: rate must be positive, got {rate_hz}"
                ));
                return Err(SdrCoreError::InvalidArg);
            }
            send(core, UiToDsp::SetSampleRate(rate_hz))
        })
    }
}

/// Set the decimation factor (power of 2, 1 = none).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_decimation(handle: *mut SdrCore, factor: u32) -> i32 {
    unsafe {
        with_core(handle, |core| {
            if factor == 0 || !factor.is_power_of_two() {
                set_last_error(format!(
                    "sdr_core_set_decimation: factor must be a nonzero power of two, got {factor}"
                ));
                return Err(SdrCoreError::InvalidArg);
            }
            send(core, UiToDsp::SetDecimation(factor))
        })
    }
}

/// Set the PPM correction for the tuner crystal offset.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_ppm_correction(handle: *mut SdrCore, ppm: i32) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::SetPpmCorrection(ppm))) }
}

// ============================================================
//  Tuner gain
// ============================================================

/// Set the tuner gain in dB. Value must be finite.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_gain(handle: *mut SdrCore, gain_db: f64) -> i32 {
    unsafe {
        with_core(handle, |core| {
            require_finite("sdr_core_set_gain", gain_db)?;
            send(core, UiToDsp::SetGain(gain_db))
        })
    }
}

/// Enable or disable tuner AGC.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_agc(handle: *mut SdrCore, enabled: bool) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::SetAgc(enabled))) }
}

// ============================================================
//  Demodulation
// ============================================================

/// Set the active demodulation mode. `mode` must be one of the
/// `SDR_DEMOD_*` constants from the header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_demod_mode(handle: *mut SdrCore, mode: i32) -> i32 {
    unsafe {
        with_core(handle, |core| {
            let Some(m) = demod_mode_from_c(mode) else {
                set_last_error(format!("sdr_core_set_demod_mode: unknown value {mode}"));
                return Err(SdrCoreError::InvalidArg);
            };
            send(core, UiToDsp::SetDemodMode(m))
        })
    }
}

/// Set the channel bandwidth in Hz.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_bandwidth(handle: *mut SdrCore, bw_hz: f64) -> i32 {
    unsafe {
        with_core(handle, |core| {
            require_finite("sdr_core_set_bandwidth", bw_hz)?;
            if bw_hz <= 0.0 {
                set_last_error(format!(
                    "sdr_core_set_bandwidth: bandwidth must be positive, got {bw_hz}"
                ));
                return Err(SdrCoreError::InvalidArg);
            }
            send(core, UiToDsp::SetBandwidth(bw_hz))
        })
    }
}

/// Enable or disable squelch.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_squelch_enabled(handle: *mut SdrCore, enabled: bool) -> i32 {
    unsafe {
        with_core(handle, |core| {
            send(core, UiToDsp::SetSquelchEnabled(enabled))
        })
    }
}

/// Enable or disable auto-squelch (noise-floor tracking).
/// The engine self-adjusts the squelch threshold while this is
/// on; manual `sdr_core_set_squelch_db` calls are still accepted
/// but will be overwritten by the tracker on the next update.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_auto_squelch(handle: *mut SdrCore, enabled: bool) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::SetAutoSquelch(enabled))) }
}

/// Set the squelch threshold in dB. Value must be finite.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_squelch_db(handle: *mut SdrCore, db: f32) -> i32 {
    unsafe {
        with_core(handle, |core| {
            require_finite("sdr_core_set_squelch_db", f64::from(db))?;
            send(core, UiToDsp::SetSquelch(db))
        })
    }
}

/// Set the FM de-emphasis mode. `mode` must be one of the
/// `SDR_DEEMPH_*` constants.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_deemphasis(handle: *mut SdrCore, mode: i32) -> i32 {
    unsafe {
        with_core(handle, |core| {
            let Some(m) = deemphasis_from_c(mode) else {
                set_last_error(format!("sdr_core_set_deemphasis: unknown value {mode}"));
                return Err(SdrCoreError::InvalidArg);
            };
            send(core, UiToDsp::SetDeemphasis(m))
        })
    }
}

// ============================================================
//  Audio
// ============================================================

/// Set the audio output volume, clamped to `[0.0, 1.0]`.
/// Value must be finite.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_volume(handle: *mut SdrCore, volume_0_1: f32) -> i32 {
    unsafe {
        with_core(handle, |core| {
            require_finite("sdr_core_set_volume", f64::from(volume_0_1))?;
            let clamped = volume_0_1.clamp(0.0, 1.0);
            send(core, UiToDsp::SetVolume(clamped))
        })
    }
}

/// Shared helper for commands that take a NUL-terminated UTF-8
/// path / identifier string. Returns an owned `String` on success
/// or an `InvalidArg` after setting the last-error.
///
/// # Safety
///
/// `ptr` must be either null or a pointer to a NUL-terminated UTF-8
/// C string.
unsafe fn cstr_to_string(fn_name: &str, ptr: *const c_char) -> Result<String, SdrCoreError> {
    if ptr.is_null() {
        set_last_error(format!("{fn_name}: string pointer is null"));
        return Err(SdrCoreError::InvalidArg);
    }
    // SAFETY: caller contract.
    let cstr = unsafe { CStr::from_ptr(ptr) };
    if let Ok(s) = cstr.to_str() {
        Ok(s.to_string())
    } else {
        set_last_error(format!("{fn_name}: string is not valid UTF-8"));
        Err(SdrCoreError::InvalidArg)
    }
}

/// Select the audio output device by caller-opaque UID. Empty
/// string routes to the system default output. The UID is the
/// value previously obtained from `sdr_core_audio_device_uid`.
///
/// # Safety
///
/// `uid_utf8` must be a NUL-terminated UTF-8 C string (or empty).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_audio_device(
    handle: *mut SdrCore,
    uid_utf8: *const c_char,
) -> i32 {
    unsafe {
        with_core(handle, |core| {
            let uid = cstr_to_string("sdr_core_set_audio_device", uid_utf8)?;
            send(core, UiToDsp::SetAudioDevice(uid))
        })
    }
}

/// Start writing the demodulated audio stream to a 16-bit PCM WAV
/// file at `path_utf8`. If recording was already active the engine
/// logs a warning and overwrites — callers should stop first.
///
/// The engine confirms start via `SDR_EVT_AUDIO_RECORDING_STARTED`
/// or emits `SDR_EVT_ERROR` on failure (open error, disk full, etc.).
///
/// # Safety
///
/// `path_utf8` must be a NUL-terminated UTF-8 C string naming a
/// writable filesystem path (the engine creates the file). Does
/// not accept null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_start_audio_recording(
    handle: *mut SdrCore,
    path_utf8: *const c_char,
) -> i32 {
    unsafe {
        with_core(handle, |core| {
            let path = cstr_to_string("sdr_core_start_audio_recording", path_utf8)?;
            if path.is_empty() {
                set_last_error("sdr_core_start_audio_recording: path is empty");
                return Err(SdrCoreError::InvalidArg);
            }
            send(core, UiToDsp::StartAudioRecording(PathBuf::from(path)))
        })
    }
}

/// Stop audio recording. The engine finalizes the WAV header on
/// writer drop and confirms via `SDR_EVT_AUDIO_RECORDING_STOPPED`.
/// Safe to call when no recording is active (no-op + stop event).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_stop_audio_recording(handle: *mut SdrCore) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::StopAudioRecording)) }
}

// ============================================================
//  IQ frontend
// ============================================================

/// Enable or disable DC blocking on the IQ frontend.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_dc_blocking(handle: *mut SdrCore, enabled: bool) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::SetDcBlocking(enabled))) }
}

/// Enable or disable IQ inversion (conjugation).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_iq_inversion(handle: *mut SdrCore, enabled: bool) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::SetIqInversion(enabled))) }
}

/// Enable or disable adaptive IQ imbalance correction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_iq_correction(handle: *mut SdrCore, enabled: bool) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::SetIqCorrection(enabled))) }
}

// ============================================================
//  Spectrum display
// ============================================================

/// Maximum accepted FFT size. Matches the GTK display panel's
/// upper bound (`FFT_SIZES` in `sdr-ui::sidebar::display_panel`,
/// which tops out at 65536). Without an upper bound, a host that
/// passes `usize::MAX` — or, on Swift, the interpretation of a
/// signed `Int.max` through the unsigned `usize` parameter —
/// would trigger an unbounded allocation attempt in rustfft
/// and OOM the process before the engine can refuse it. The
/// controller doesn't impose its own upper bound at the moment,
/// so the FFI is the place to clamp.
///
/// 65536 bins at float32 = 256 KB per FFT buffer, well under
/// any reasonable budget. Hosts that need more can raise this
/// in a future ABI minor bump alongside whatever upstream work
/// makes the engine comfortable with it.
const MAX_FFT_SIZE: usize = 65536;

/// Set the FFT size. Must be a nonzero power of two and
/// `<= MAX_FFT_SIZE`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_fft_size(handle: *mut SdrCore, n: usize) -> i32 {
    unsafe {
        with_core(handle, |core| {
            if n == 0 || !n.is_power_of_two() {
                set_last_error(format!(
                    "sdr_core_set_fft_size: size must be a nonzero power of two, got {n}"
                ));
                return Err(SdrCoreError::InvalidArg);
            }
            if n > MAX_FFT_SIZE {
                set_last_error(format!(
                    "sdr_core_set_fft_size: size {n} exceeds maximum {MAX_FFT_SIZE}"
                ));
                return Err(SdrCoreError::InvalidArg);
            }
            send(core, UiToDsp::SetFftSize(n))
        })
    }
}

/// Set the FFT window function. `window` must be one of the
/// `SDR_FFT_WIN_*` constants.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_fft_window(handle: *mut SdrCore, window: i32) -> i32 {
    unsafe {
        with_core(handle, |core| {
            let Some(w) = fft_window_from_c(window) else {
                set_last_error(format!("sdr_core_set_fft_window: unknown value {window}"));
                return Err(SdrCoreError::InvalidArg);
            };
            send(core, UiToDsp::SetWindowFunction(w))
        })
    }
}

/// Set the FFT display frame rate in fps. Must be finite and positive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_fft_rate(handle: *mut SdrCore, fps: f64) -> i32 {
    unsafe {
        with_core(handle, |core| {
            require_finite("sdr_core_set_fft_rate", fps)?;
            if fps <= 0.0 {
                set_last_error(format!(
                    "sdr_core_set_fft_rate: rate must be positive, got {fps}"
                ));
                return Err(SdrCoreError::InvalidArg);
            }
            send(core, UiToDsp::SetFftRate(fps))
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::lifecycle::sdr_core_create;
    use std::ffi::CString;

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
            unsafe {
                sdr_core_start_audio_recording(std::ptr::null_mut(), empty.as_ptr())
            },
            SdrCoreError::InvalidHandle.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_stop_audio_recording(std::ptr::null_mut()) },
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
        // has somewhere it can open. We don't inspect the output —
        // just exercise the command plumbing.
        let h = make_handle();
        let tmp = std::env::temp_dir().join(format!(
            "sdr-ffi-test-{}.wav",
            std::process::id()
        ));
        let path = CString::new(tmp.to_string_lossy().into_owned()).unwrap();
        assert_eq!(
            unsafe { sdr_core_start_audio_recording(h, path.as_ptr()) },
            SdrCoreError::Ok.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_stop_audio_recording(h) },
            SdrCoreError::Ok.as_int()
        );
        // Give the controller a moment to process + drop the writer,
        // then clean up. If the file wasn't created (e.g., the DSP
        // thread hadn't processed the command yet) remove_file errs;
        // that's fine — test doesn't depend on it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = std::fs::remove_file(&tmp);
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
}
