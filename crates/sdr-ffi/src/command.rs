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

use sdr_core::{SourceType, UiToDsp};
use sdr_pipeline::iq_frontend::FftWindow;
use sdr_radio::DeemphasisMode;
use sdr_types::{DemodMode, Protocol};

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
//  Advanced demod — #245 exposure
// ============================================================
//
//  These route straight to the existing `UiToDsp` messages the
//  GTK UI already drives. Mode-gating (e.g. WFM stereo only
//  meaningful in WFM) lives on the host side — the engine
//  accepts the setter in any mode but ignores it when the
//  active demod doesn't care, which matches the GTK UI's
//  pattern of still letting the user set the toggle ahead of a
//  mode switch. Per ABI minor bump 0.7 on PR #347.

/// Minimum accepted value for `sdr_core_set_nb_level`. Values
/// below 1.0 would have the blanker clip every sample (the
/// engine treats the level as a multiplier over the running
/// amplitude), producing silent audio instead of a usable
/// output. Kept as an exclusive-minimum constant so the check
/// in the setter and the boundary tests can't drift apart. Per
/// CodeRabbit round 1 on PR #347.
const NB_LEVEL_MIN: f32 = 1.0;

/// Exclusive lower bound for `sdr_core_set_notch_frequency`.
/// The notch filter coefficients are undefined at 0 Hz (and
/// negative frequencies have no physical meaning here), so the
/// setter rejects `freq_hz <= 0.0`. Per CodeRabbit round 1 on
/// PR #347.
const NOTCH_FREQUENCY_MIN_HZ_EXCLUSIVE: f32 = 0.0;

/// Enable or disable the noise blanker.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_nb_enabled(handle: *mut SdrCore, enabled: bool) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::SetNbEnabled(enabled))) }
}

/// Set the noise-blanker threshold multiplier. Must be finite
/// and `>= 1.0` (the engine treats the level as a multiplier
/// over the running sample amplitude; `< 1.0` would clip every
/// sample). Values exceeding 1.0 loosen the blanking threshold.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_nb_level(handle: *mut SdrCore, level: f32) -> i32 {
    unsafe {
        with_core(handle, |core| {
            require_finite("sdr_core_set_nb_level", f64::from(level))?;
            if level < NB_LEVEL_MIN {
                set_last_error(format!(
                    "sdr_core_set_nb_level: level must be >= {NB_LEVEL_MIN}, got {level}"
                ));
                return Err(SdrCoreError::InvalidArg);
            }
            send(core, UiToDsp::SetNbLevel(level))
        })
    }
}

/// Enable or disable FM IF noise reduction. No-op when the
/// active demod is not an FM mode; host UIs typically hide the
/// toggle outside WFM / NFM.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_fm_if_nr_enabled(handle: *mut SdrCore, enabled: bool) -> i32 {
    unsafe {
        with_core(handle, |core| {
            send(core, UiToDsp::SetFmIfNrEnabled(enabled))
        })
    }
}

/// Enable or disable WFM stereo decode. Only meaningful in WFM
/// mode; the engine ignores the setting in other modes but the
/// host UI should also gate visibility.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_wfm_stereo(handle: *mut SdrCore, enabled: bool) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::SetWfmStereo(enabled))) }
}

/// Enable or disable the audio-stage notch filter.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_notch_enabled(handle: *mut SdrCore, enabled: bool) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::SetNotchEnabled(enabled))) }
}

/// Set the audio-stage notch filter frequency in Hz. Must be
/// finite and `> 0`. The engine clamps to the audio-rate
/// Nyquist internally; passing a value above Nyquist is not an
/// error here because the clamp is sample-rate dependent and
/// the FFI has no stable reference to that without querying
/// the engine.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_notch_frequency(handle: *mut SdrCore, freq_hz: f32) -> i32 {
    unsafe {
        with_core(handle, |core| {
            require_finite("sdr_core_set_notch_frequency", f64::from(freq_hz))?;
            if freq_hz <= NOTCH_FREQUENCY_MIN_HZ_EXCLUSIVE {
                set_last_error(format!(
                    "sdr_core_set_notch_frequency: frequency must be > {NOTCH_FREQUENCY_MIN_HZ_EXCLUSIVE} Hz, got {freq_hz}"
                ));
                return Err(SdrCoreError::InvalidArg);
            }
            send(core, UiToDsp::SetNotchFrequency(freq_hz))
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

// ============================================================
//  Source selection (#235, #236) — switch the active IQ
//  source and configure the per-source connection details.
//
//  Discriminants mirror the matching `Sdr*` enums in
//  `include/sdr_core.h`. Reordering would silently break ABI.
// ============================================================

pub const SDR_SOURCE_RTLSDR: i32 = 0;
pub const SDR_SOURCE_NETWORK: i32 = 1;
pub const SDR_SOURCE_FILE: i32 = 2;
pub const SDR_SOURCE_RTLTCP: i32 = 3;

fn source_type_from_c(v: i32) -> Option<SourceType> {
    match v {
        SDR_SOURCE_RTLSDR => Some(SourceType::RtlSdr),
        SDR_SOURCE_NETWORK => Some(SourceType::Network),
        SDR_SOURCE_FILE => Some(SourceType::File),
        SDR_SOURCE_RTLTCP => Some(SourceType::RtlTcp),
        _ => None,
    }
}

// The network **source** protocol discriminants live next to the
// source commands rather than piggy-backing on
// `SDR_NETWORK_PROTOCOL_TCP_SERVER` from the audio sink side.
// Why: same underlying `sdr_types::Protocol` enum, but the
// wire direction is opposite. On the sink side `TcpClient`
// means "device listens as TCP server for audio clients" — the
// C ABI name `TCP_SERVER` reflects that. On the source side
// the same `TcpClient` means "device connects outbound as TCP
// client to a remote IQ server". Reusing `TCP_SERVER` here
// would be actively misleading for host authors. Per #235.

pub const SDR_SOURCE_PROTOCOL_TCP: i32 = 0;
pub const SDR_SOURCE_PROTOCOL_UDP: i32 = 1;

fn source_protocol_from_c(v: i32) -> Option<Protocol> {
    match v {
        SDR_SOURCE_PROTOCOL_TCP => Some(Protocol::TcpClient),
        SDR_SOURCE_PROTOCOL_UDP => Some(Protocol::Udp),
        _ => None,
    }
}

/// Switch the active IQ source. `source_type` must be one of
/// `SDR_SOURCE_*`. The engine tears down the current source,
/// rebuilds from the persisted per-type config (network host /
/// port / protocol for Network, file path for File, etc.), and
/// restarts if the engine is currently running. Returns
/// `SDR_CORE_ERR_INVALID_ARG` for an unknown value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_source_type(handle: *mut SdrCore, source_type: i32) -> i32 {
    unsafe {
        with_core(handle, |core| {
            let Some(t) = source_type_from_c(source_type) else {
                set_last_error(format!(
                    "sdr_core_set_source_type: unknown source_type {source_type}"
                ));
                return Err(SdrCoreError::InvalidArg);
            };
            send(core, UiToDsp::SetSourceType(t))
        })
    }
}

/// Configure the network IQ source endpoint. `hostname_utf8`
/// must be a non-null, non-empty NUL-terminated UTF-8 C string.
/// `port` must be in `1..=65535`. `protocol` must be one of
/// `SDR_SOURCE_PROTOCOL_*`. The engine stores the config; a
/// subsequent `sdr_core_set_source_type(SDR_SOURCE_NETWORK)`
/// (or a source restart while Network is already active) uses
/// the stored values to open the connection.
///
/// # Safety
///
/// `hostname_utf8` must be a NUL-terminated UTF-8 C string or
/// null (null returns `SDR_CORE_ERR_INVALID_ARG`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_network_config(
    handle: *mut SdrCore,
    hostname_utf8: *const c_char,
    port: u16,
    protocol: i32,
) -> i32 {
    unsafe {
        with_core(handle, |core| {
            let hostname = cstr_to_string("sdr_core_set_network_config", hostname_utf8)?;
            if hostname.is_empty() {
                set_last_error("sdr_core_set_network_config: hostname is empty");
                return Err(SdrCoreError::InvalidArg);
            }
            // Same zero-port rejection as `sdr_core_set_network_sink_config`
            // — unusable endpoint on either direction.
            if port == 0 {
                set_last_error("sdr_core_set_network_config: port must be in 1..=65535, got 0");
                return Err(SdrCoreError::InvalidArg);
            }
            let Some(proto) = source_protocol_from_c(protocol) else {
                set_last_error(format!(
                    "sdr_core_set_network_config: unknown protocol {protocol}"
                ));
                return Err(SdrCoreError::InvalidArg);
            };
            send(
                core,
                UiToDsp::SetNetworkConfig {
                    hostname,
                    port,
                    protocol: proto,
                },
            )
        })
    }
}

/// Set the filesystem path the file-playback source reads from
/// the next time `SDR_SOURCE_FILE` is activated (or the source
/// is restarted while File is already active). `path_utf8` must
/// be a non-null, non-empty NUL-terminated UTF-8 C string. The
/// engine does not open the file here — only stores the path.
/// Open errors surface as `SDR_EVT_ERROR` / `SDR_EVT_SOURCE_STOPPED`
/// when the source actually starts.
///
/// # Safety
///
/// `path_utf8` must be a NUL-terminated UTF-8 C string or null
/// (null returns `SDR_CORE_ERR_INVALID_ARG`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_file_path(
    handle: *mut SdrCore,
    path_utf8: *const c_char,
) -> i32 {
    unsafe {
        with_core(handle, |core| {
            let path = cstr_to_string("sdr_core_set_file_path", path_utf8)?;
            if path.is_empty() {
                set_last_error("sdr_core_set_file_path: path is empty");
                return Err(SdrCoreError::InvalidArg);
            }
            send(core, UiToDsp::SetFilePath(std::path::PathBuf::from(path)))
        })
    }
}

// ============================================================
//  Audio sink selection (#247) — switch between local audio
//  device and network stream, configure the network endpoint.
//
//  Discriminants below mirror the matching `Sdr*` enums in
//  `include/sdr_core.h`. Reordering would silently break ABI.
// ============================================================

pub const SDR_AUDIO_SINK_LOCAL: i32 = 0;
pub const SDR_AUDIO_SINK_NETWORK: i32 = 1;

fn audio_sink_type_from_c(v: i32) -> Option<sdr_core::AudioSinkType> {
    match v {
        SDR_AUDIO_SINK_LOCAL => Some(sdr_core::AudioSinkType::Local),
        SDR_AUDIO_SINK_NETWORK => Some(sdr_core::AudioSinkType::Network),
        _ => None,
    }
}

fn protocol_from_c(v: i32) -> Option<sdr_types::Protocol> {
    match v {
        crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER => Some(sdr_types::Protocol::TcpClient),
        crate::event::SDR_NETWORK_PROTOCOL_UDP => Some(sdr_types::Protocol::Udp),
        _ => None,
    }
}

/// Switch between the local audio device sink and the network
/// stream sink. `sink_type` must be one of `SDR_AUDIO_SINK_*`.
/// The engine stops the current sink, builds the replacement
/// from the persisted device / network config, and restarts it
/// if the engine is currently running. Returns `SDR_CORE_OK` on
/// success or `SDR_CORE_ERR_INVALID_ARG` for an unknown
/// `sink_type` value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_audio_sink_type(handle: *mut SdrCore, sink_type: i32) -> i32 {
    unsafe {
        with_core(handle, |core| {
            let Some(t) = audio_sink_type_from_c(sink_type) else {
                set_last_error(format!(
                    "sdr_core_set_audio_sink_type: unknown sink_type {sink_type}"
                ));
                return Err(SdrCoreError::InvalidArg);
            };
            send(core, UiToDsp::SetAudioSinkType(t))
        })
    }
}

/// Configure the network audio sink endpoint. `hostname_utf8`
/// must be a non-null NUL-terminated UTF-8 C string. `protocol`
/// must be one of `SDR_NETWORK_PROTOCOL_*`. The engine stores
/// the config; if the network sink is currently active the
/// underlying `NetworkSink` is rebuilt inline so the new
/// endpoint takes effect immediately.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_set_network_sink_config(
    handle: *mut SdrCore,
    hostname_utf8: *const c_char,
    port: u16,
    protocol: i32,
) -> i32 {
    unsafe {
        with_core(handle, |core| {
            let hostname = cstr_to_string("sdr_core_set_network_sink_config", hostname_utf8)?;
            if hostname.is_empty() {
                set_last_error("sdr_core_set_network_sink_config: hostname is empty");
                return Err(SdrCoreError::InvalidArg);
            }
            // Port 0 has no useful meaning here: UDP would
            // silently drop packets to a bogus destination, and
            // TCP server mode would bind a random ephemeral
            // port the host can't discover. The Swift UI
            // already constrains the picker to 1..=65535, but
            // non-Swift hosts and direct FFI callers go through
            // this path too — reject at the boundary.
            // Per `CodeRabbit` round 2 on PR #352.
            if port == 0 {
                set_last_error(
                    "sdr_core_set_network_sink_config: port must be in 1..=65535, got 0",
                );
                return Err(SdrCoreError::InvalidArg);
            }
            let Some(proto) = protocol_from_c(protocol) else {
                set_last_error(format!(
                    "sdr_core_set_network_sink_config: unknown protocol {protocol}"
                ));
                return Err(SdrCoreError::InvalidArg);
            };
            send(
                core,
                UiToDsp::SetNetworkSinkConfig {
                    hostname,
                    port,
                    protocol: proto,
                },
            )
        })
    }
}

/// Shared helper for the two `start_*_recording` commands. Validates
/// `path_utf8` (non-null, UTF-8, non-empty) and dispatches the
/// appropriate `UiToDsp` variant via `build_cmd`. Keeps the path
/// validation in one place so a future rule (e.g., rejecting
/// directory paths, normalizing trailing whitespace) lands in
/// both start paths at once.
///
/// # Safety
///
/// `path_utf8` must be a NUL-terminated UTF-8 C string or null
/// (null returns `SDR_CORE_ERR_INVALID_ARG`). `core` must be a
/// valid engine reference.
unsafe fn start_recording_with_path(
    core: &SdrCore,
    fn_name: &str,
    path_utf8: *const c_char,
    build_cmd: impl FnOnce(PathBuf) -> UiToDsp,
) -> Result<(), SdrCoreError> {
    let path = unsafe { cstr_to_string(fn_name, path_utf8) }?;
    if path.is_empty() {
        set_last_error(format!("{fn_name}: path is empty"));
        return Err(SdrCoreError::InvalidArg);
    }
    send(core, build_cmd(PathBuf::from(path)))
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
            start_recording_with_path(
                core,
                "sdr_core_start_audio_recording",
                path_utf8,
                UiToDsp::StartAudioRecording,
            )
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

/// Start writing the raw IQ sample stream to a WAV file at
/// `path_utf8`. Unlike audio recording, the IQ WAV is written at
/// the current tuner sample rate (not a fixed 48 kHz) with two
/// channels (I / Q), so the file size per second varies with the
/// source sample rate selection.
///
/// The engine confirms start via `SDR_EVT_IQ_RECORDING_STARTED`
/// or emits `SDR_EVT_ERROR` on failure (open error, disk full, etc.).
///
/// # Safety
///
/// `path_utf8` must be a NUL-terminated UTF-8 C string naming a
/// writable filesystem path (the engine creates the file). Does
/// not accept null or empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_start_iq_recording(
    handle: *mut SdrCore,
    path_utf8: *const c_char,
) -> i32 {
    unsafe {
        with_core(handle, |core| {
            start_recording_with_path(
                core,
                "sdr_core_start_iq_recording",
                path_utf8,
                UiToDsp::StartIqRecording,
            )
        })
    }
}

/// Stop IQ recording. The engine finalizes the WAV header on
/// writer drop and confirms via `SDR_EVT_IQ_RECORDING_STOPPED`.
/// Safe to call when no recording is active (no-op + stop event).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_stop_iq_recording(handle: *mut SdrCore) -> i32 {
    unsafe { with_core(handle, |core| send(core, UiToDsp::StopIqRecording)) }
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
        let metadata = std::fs::metadata(&tmp)
            .expect("audio recording should create a WAV file before cleanup");
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

    #[test]
    fn iq_recording_start_stop_round_trip() {
        // Same shape as the audio recording round-trip test —
        // verifies the controller opened + finalized the WAV
        // file, not just that `send_command` returned OK. Per
        // CodeRabbit round 2 on PR #345.
        let h = make_handle();
        let tmp = unique_temp_wav("sdr-ffi-iq-test");
        let path = CString::new(tmp.to_string_lossy().into_owned()).unwrap();
        assert_eq!(
            unsafe { sdr_core_start_iq_recording(h, path.as_ptr()) },
            SdrCoreError::Ok.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_stop_iq_recording(h) },
            SdrCoreError::Ok.as_int()
        );
        std::thread::sleep(std::time::Duration::from_millis(RECORDING_FLUSH_WAIT_MS));
        let metadata =
            std::fs::metadata(&tmp).expect("IQ recording should create a WAV file before cleanup");
        assert!(
            metadata.len() >= WAV_HEADER_BYTES,
            "IQ recording should finalize at least a WAV header"
        );
        std::fs::remove_file(&tmp).unwrap();
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
            unsafe {
                sdr_core_set_network_sink_config(h, host.as_ptr(), TEST_NETWORK_PORT_TCP, 99)
            },
            SdrCoreError::InvalidArg.as_int()
        );
        assert_eq!(
            unsafe {
                sdr_core_set_network_sink_config(h, host.as_ptr(), TEST_NETWORK_PORT_TCP, -1)
            },
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
}
