/*
 * sdr_core.h — Hand-written C ABI for the sdr-core SDR engine.
 *
 * This file is the **source of truth** for the C interface between
 * `sdr-ffi` (Rust) and any native host (Swift / C / C++). The Rust
 * side in `crates/sdr-ffi/` MUST match this header byte-for-byte —
 * the `make ffi-header-check` CI lint enforces the match by running
 * `cbindgen` against the Rust source and diffing the result.
 *
 * Spec: docs/superpowers/specs/2026-04-12-sdr-ffi-c-abi-design.md
 *
 * Threading model summary (full contract in the spec):
 *   - Commands can be called from any thread.
 *   - The event callback fires on the FFI dispatcher thread, NOT
 *     the host's main thread. The host is responsible for marshaling
 *     to its UI thread.
 *   - `sdr_core_destroy` should NOT be called from inside the event
 *     callback. The implementation detects self-join and skips the
 *     dispatcher join, but teardown is incomplete in that case —
 *     always destroy from outside the callback.
 *   - Errors go through a thread-local last-error message; call
 *     `sdr_core_last_error_message()` from the same thread that
 *     observed the error code.
 *
 * ABI versioning:
 *   - Minor bump = additive (new function, new event variant, new
 *     error code). Old hosts keep working; they just don't see new
 *     things.
 *   - Major bump = breaking (signature change, struct layout, etc.).
 *     Old hosts must fail to start against a newer library.
 *   - Hosts should call `sdr_core_abi_version()` once at startup and
 *     abort cleanly on a major mismatch.
 */

#ifndef SDR_CORE_H
#define SDR_CORE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ================================================================ */
/*  ABI versioning                                                  */
/* ================================================================ */

#define SDR_CORE_ABI_VERSION_MAJOR 0
#define SDR_CORE_ABI_VERSION_MINOR 9

/*
 * Return the ABI version the library was built with, packed as
 * `(major << 16) | minor`. Hosts call this once at startup and
 * abort (or show a "library mismatch" dialog) on a major mismatch
 * against what they were compiled against.
 */
uint32_t sdr_core_abi_version(void);

/* ================================================================ */
/*  Error model                                                     */
/* ================================================================ */

/*
 * Functions that can fail return an `int32_t` carrying one of these
 * values. `SDR_CORE_OK` (0) is success; negative values are errors.
 *
 * The matching human-readable error message is stashed in a
 * thread-local and can be fetched via `sdr_core_last_error_message()`
 * from the same thread that observed the error code.
 *
 * Never reorder or renumber — these discriminants are part of the
 * ABI. New variants go at the end (and require a minor ABI bump).
 */
typedef enum SdrCoreError {
    SDR_CORE_OK             =  0,
    SDR_CORE_ERR_INTERNAL   = -1, /* Rust panic caught by catch_unwind. */
    SDR_CORE_ERR_INVALID_HANDLE = -2, /* Null or destroyed handle.       */
    SDR_CORE_ERR_INVALID_ARG    = -3, /* Malformed argument.             */
    SDR_CORE_ERR_NOT_RUNNING    = -4, /* Wrong state for this command.  */
    SDR_CORE_ERR_DEVICE         = -5, /* USB / source backend error.    */
    SDR_CORE_ERR_AUDIO          = -6, /* Audio backend error.           */
    SDR_CORE_ERR_IO             = -7, /* File / network I/O error.      */
    SDR_CORE_ERR_CONFIG         = -8, /* Config load/save error.        */
    SDR_CORE_ERR_AUTH           = -9, /* Remote service rejected credentials (RadioReference). */
} SdrCoreError;

/*
 * Return a pointer to the thread-local last-error message set by
 * the most recent `sdr_core_*` call on this thread, or NULL if no
 * error has been recorded on this thread.
 *
 * The returned pointer is owned by thread-local storage. It is
 * valid until the next `sdr_core_*` call on the same thread, which
 * may overwrite or clear the buffer. Callers that want to persist
 * the message should copy it immediately.
 *
 * Safe to call at any time, on any thread, including from inside
 * the event callback. Does not produce its own errors.
 */
const char* sdr_core_last_error_message(void);

/* ================================================================ */
/*  Device enumeration                                              */
/* ================================================================ */

/*
 * Count RTL-SDR devices currently attached to the host's USB bus.
 *
 * Does NOT open any device, does NOT require an `SdrCore` handle,
 * and does NOT issue USB control transfers — under the hood this
 * enumerates libusb's device list and matches by VID/PID.
 *
 * Safe to call at any time, on any thread, as often as the host
 * wants. A host that surfaces device presence in its UI should
 * call this at launch and again on USB hotplug events (on macOS,
 * hotplug comes via `NSWorkspace` / IOKit notifications; this API
 * does not push — it's query-on-demand).
 *
 * Returns the number of devices found (0 if none).
 */
uint32_t sdr_core_device_count(void);

/*
 * Fill `out_buf` with the human-readable name of the RTL-SDR
 * device at `index` (UTF-8, NUL-terminated). `buf_len` is the
 * total capacity of `out_buf` in bytes, INCLUDING the NUL.
 *
 * Returns the number of bytes written (not counting the NUL) on
 * success, or one of:
 *   - `SDR_CORE_ERR_INVALID_ARG`  if `out_buf` is NULL or
 *                                 `buf_len` is 0.
 *   - `SDR_CORE_ERR_DEVICE`       if `index` is out of range or
 *                                 the device name couldn't be
 *                                 probed.
 *
 * A 128-byte buffer is comfortably enough for every RTL-SDR
 * device name known (typically ~30 chars). If `buf_len` is too
 * small the written string is truncated and NUL-terminated at
 * `buf_len - 1`; no error is returned for truncation.
 *
 * Safe to call at any time, on any thread. Does NOT require an
 * `SdrCore` handle.
 */
int32_t sdr_core_device_name(
    uint32_t index,
    char*    out_buf,
    size_t   buf_len
);

/* ================================================================ */
/*  Audio output device enumeration                                 */
/* ================================================================ */

/*
 * These functions snapshot the backend's list of output devices
 * (CoreAudio on macOS, PipeWire on Linux, stub on others) and let
 * the host surface them in a Settings picker. They are handle-free
 * and can be called before `sdr_core_create`.
 *
 * Atomicity (thread-local snapshot):
 *   - `sdr_core_audio_device_count` runs the backend query and
 *     stores the result in a per-thread snapshot.
 *   - `sdr_core_audio_device_name` and `_uid` read from that
 *     snapshot, so for a given thread, `_name(i)` and `_uid(i)`
 *     always refer to the same device entry — even if a device
 *     hot-plugs between the two calls. Callers get coherent
 *     name/UID pairs for every index returned by `_count`.
 *   - Calling `_name(i)` / `_uid(i)` without a prior `_count` on
 *     this thread triggers a lazy refresh. That path doesn't
 *     benefit from cross-index consistency (each call refreshes
 *     if the snapshot was empty), but single-device pickers
 *     still work.
 *   - Each new `_count` call discards the previous snapshot and
 *     takes a fresh one, so the pattern "call count, iterate
 *     indices, call count again" gives the host two independent
 *     views and lets it detect hot-plug by comparing sizes.
 *
 * A v3 hot-plug listener (pushed as a dedicated event variant)
 * is on the roadmap for continuous device-presence tracking.
 *
 * `_name` returns the human-readable label (e.g. "MacBook Pro
 * Speakers"). `_uid` returns the caller-opaque identifier that
 * `sdr_core_set_audio_device` accepts. On macOS the UID is
 * currently the `AudioDeviceID` as a decimal string and is
 * session-scoped (stable within a process lifetime); a later PR
 * migrates to the persistent `kAudioDevicePropertyDeviceUID`
 * string without an ABI change since callers treat it as opaque.
 *
 * Empty string as UID means "system default output" on every
 * backend — index 0 is typically that entry.
 */

uint32_t sdr_core_audio_device_count(void);

int32_t sdr_core_audio_device_name(
    uint32_t index,
    char*    out_buf,
    size_t   buf_len
);

int32_t sdr_core_audio_device_uid(
    uint32_t index,
    char*    out_buf,
    size_t   buf_len
);

/* ================================================================ */
/*  Lifecycle                                                       */
/* ================================================================ */

/*
 * Opaque handle. The Rust definition lives in
 * `crates/sdr-ffi/src/handle.rs` — the host only ever holds a
 * `SdrCore *` and passes it back to FFI functions.
 */
typedef struct SdrCore SdrCore;

/*
 * Log level for `sdr_core_init_logging`. Numerically increasing =
 * more verbose.
 */
typedef enum SdrLogLevel {
    SDR_LOG_ERROR = 0,
    SDR_LOG_WARN  = 1,
    SDR_LOG_INFO  = 2,
    SDR_LOG_DEBUG = 3,
    SDR_LOG_TRACE = 4,
} SdrLogLevel;

/*
 * Initialize Rust `tracing` log routing. Optional — call once
 * before `sdr_core_create` if you want to see the engine's log
 * output. On macOS (eventual v2) this will route to `os_log`; for
 * v1 it routes to stderr via `tracing_subscriber::fmt`.
 *
 * `min_level` is one of the `SDR_LOG_*` constants. It's passed
 * as an `int32_t` rather than the typed `SdrLogLevel` so Swift
 * callers can pass `Int32` directly without a cast through the
 * Clang-imported enum wrapper.
 *
 * Calling this more than once is a no-op after the first
 * successful init (the tracing subscriber is a process-global).
 *
 * Does not return an error: if subscriber setup fails for any
 * reason the function logs a diagnostic to stderr and returns,
 * leaving any previously-installed subscriber intact.
 */
void sdr_core_init_logging(int32_t min_level);

/*
 * Create a new engine instance.
 *
 * `config_path_utf8` is the on-disk config file the engine should
 * eventually load from and persist to. Must be either NULL or a
 * NUL-terminated UTF-8 string. NULL and empty string ("") are
 * equivalent: both run with in-memory defaults and no persistence.
 * v1 engines accept the path and store it for future use but do
 * not yet read or write through it — passing a valid path now
 * means persistence can land in a follow-up without an ABI change.
 *
 * On success: writes the opaque handle to `*out_handle` and
 * returns `SDR_CORE_OK`. The handle must eventually be released
 * via `sdr_core_destroy`.
 *
 * On failure: leaves `*out_handle` untouched (still null if that's
 * how the caller initialized it), returns a negative error code,
 * and stashes a human-readable message retrievable via
 * `sdr_core_last_error_message`.
 *
 * Possible errors:
 *   SDR_CORE_ERR_INVALID_ARG     — `out_handle` is NULL, or
 *                                 `config_path_utf8` is non-NULL
 *                                 but not valid UTF-8.
 *   SDR_CORE_ERR_INTERNAL        — DSP thread spawn failed, or a
 *                                 Rust panic crossed the boundary.
 */
int32_t sdr_core_create(const char* config_path_utf8, SdrCore** out_handle);

/*
 * Destroy an engine instance.
 *
 * Sends a final `Stop` command, drops the Rust handle (which
 * closes the command channel and lets the detached DSP controller
 * thread exit naturally), and joins the FFI dispatcher thread if
 * one was started. After this call the `handle` pointer is
 * invalid — do not use it again.
 *
 * Safe to pass a null pointer (no-op). Idempotent only in the
 * sense that passing null is OK; passing the same non-null handle
 * twice is use-after-free and will probably crash.
 *
 * Should NOT be called from inside the event callback. The
 * implementation detects a self-join and skips the dispatcher
 * thread join to avoid deadlock, but teardown is incomplete in
 * that case. Always destroy from outside the callback.
 */
void sdr_core_destroy(SdrCore* handle);

/* ================================================================ */
/*  Commands                                                        */
/* ================================================================ */

/*
 * Every function in this section:
 *
 *   - Returns `int32_t`: `SDR_CORE_OK` on success, a negative
 *     `SdrCoreError` on failure. The matching message is on the
 *     thread-local last-error buffer.
 *
 *   - Is safe to call from any thread. Commands are delivered to
 *     the DSP thread via an mpsc channel; this call just enqueues
 *     the command and returns. Actual effect on the running
 *     pipeline is asynchronous — the event callback reports the
 *     new state when it takes effect.
 *
 *   - Is safe to call from within the event callback (reentrant)
 *     with one exception: `sdr_core_destroy`, which would deadlock.
 *
 *   - Returns `SDR_CORE_ERR_INVALID_HANDLE` if `handle` is null or
 *     has been destroyed.
 *
 *   - Returns `SDR_CORE_ERR_INVALID_ARG` for malformed arguments
 *     (non-finite floats, out-of-range enums, zero or non-power-of-
 *     two counts where a power of two is required, etc.).
 *
 *   - Returns `SDR_CORE_ERR_NOT_RUNNING` if the DSP thread's command
 *     channel has already been closed (engine was torn down behind
 *     the host's back, e.g., a panic earlier in the controller).
 */

/* --- Lifecycle ---------------------------------------------------- */

int32_t sdr_core_start(SdrCore* handle);
int32_t sdr_core_stop(SdrCore* handle);

/* --- Tuning ------------------------------------------------------- */

int32_t sdr_core_tune(SdrCore* handle, double freq_hz);
int32_t sdr_core_set_vfo_offset(SdrCore* handle, double offset_hz);
int32_t sdr_core_set_sample_rate(SdrCore* handle, double rate_hz);
int32_t sdr_core_set_decimation(SdrCore* handle, uint32_t factor);
int32_t sdr_core_set_ppm_correction(SdrCore* handle, int32_t ppm);

/* --- Tuner gain --------------------------------------------------- */

int32_t sdr_core_set_gain(SdrCore* handle, double gain_db);
int32_t sdr_core_set_agc(SdrCore* handle, bool enabled);

/* --- Demodulation ------------------------------------------------- */

typedef enum SdrDemodMode {
    SDR_DEMOD_WFM = 0,
    SDR_DEMOD_NFM = 1,
    SDR_DEMOD_AM  = 2,
    SDR_DEMOD_USB = 3,
    SDR_DEMOD_LSB = 4,
    SDR_DEMOD_DSB = 5,
    SDR_DEMOD_CW  = 6,
    SDR_DEMOD_RAW = 7,
} SdrDemodMode;

int32_t sdr_core_set_demod_mode(SdrCore* handle, int32_t mode);
int32_t sdr_core_set_bandwidth(SdrCore* handle, double bw_hz);
int32_t sdr_core_set_squelch_enabled(SdrCore* handle, bool enabled);
int32_t sdr_core_set_squelch_db(SdrCore* handle, float db);

/*
 * Enable or disable auto-squelch (engine-side noise-floor
 * tracking). Complements `sdr_core_set_squelch_enabled` — while
 * auto-squelch is on, the engine continuously adjusts the
 * squelch threshold to sit above the measured noise floor.
 * Manual `sdr_core_set_squelch_db` writes are accepted but the
 * tracker will overwrite them on its next cycle.
 */
int32_t sdr_core_set_auto_squelch(SdrCore* handle, bool enabled);

typedef enum SdrDeemphasis {
    SDR_DEEMPH_NONE = 0,
    SDR_DEEMPH_US75 = 1,
    SDR_DEEMPH_EU50 = 2,
} SdrDeemphasis;

int32_t sdr_core_set_deemphasis(SdrCore* handle, int32_t mode);

/* --- Advanced demod ---------------------------------------------- */
/*
 * Route straight to the matching `UiToDsp` messages. Mode-gating
 * is a host-side concern — e.g. WFM stereo is only meaningful
 * when the active demod is WFM, but the engine still accepts
 * the setter in any mode and no-ops outside of WFM. Host UIs
 * typically hide the controls outside their relevant modes to
 * avoid pointless toggles. Added in ABI minor 0.7 (issue #245).
 */

/* Enable or disable the noise blanker. */
int32_t sdr_core_set_nb_enabled(SdrCore* handle, bool enabled);

/*
 * Noise-blanker threshold multiplier. Must be finite and
 * `>= 1.0` — values below 1 would clip every sample. Higher
 * values loosen the threshold.
 */
int32_t sdr_core_set_nb_level(SdrCore* handle, float level);

/* Enable or disable FM IF noise reduction (WFM / NFM only). */
int32_t sdr_core_set_fm_if_nr_enabled(SdrCore* handle, bool enabled);

/* Enable or disable WFM stereo decode (WFM only). */
int32_t sdr_core_set_wfm_stereo(SdrCore* handle, bool enabled);

/* Enable or disable the audio-stage notch filter. */
int32_t sdr_core_set_notch_enabled(SdrCore* handle, bool enabled);

/*
 * Audio notch center frequency, in Hz. Must be finite and
 * strictly positive. The engine clamps to the audio Nyquist
 * internally; values above Nyquist are not a hard FFI error
 * because the clamp point depends on the active audio sample
 * rate.
 */
int32_t sdr_core_set_notch_frequency(SdrCore* handle, float freq_hz);

/* --- Audio -------------------------------------------------------- */

/*
 * `volume_0_1` is clamped to [0.0, 1.0] internally; passing a value
 * outside that range is NOT an error. NaN/infinity values are an
 * error (returns `SDR_CORE_ERR_INVALID_ARG`).
 */
int32_t sdr_core_set_volume(SdrCore* handle, float volume_0_1);

/*
 * Select the audio output device by caller-opaque UID. The UID is
 * the value previously obtained from `sdr_core_audio_device_uid`.
 * Empty string ("") routes to the system default output — that is
 * the engine default until the host calls this.
 *
 * `uid_utf8` must be a NUL-terminated UTF-8 C string (null returns
 * `SDR_CORE_ERR_INVALID_ARG`). The device swap is engine-side
 * transactional: on a failed swap the previous device is restored,
 * so a rejected UID never leaves the sink silent.
 */
int32_t sdr_core_set_audio_device(SdrCore* handle, const char* uid_utf8);

/* --- Audio sink selection (issue #247, ABI 0.9) --- */
/*
 * Switch between local audio output and TCP/UDP network
 * streaming, and configure the network endpoint. Stops the
 * current sink, builds the replacement using the persisted
 * device / network config, and restarts it if the engine is
 * running. Status updates are surfaced through the new
 * `SDR_EVT_NETWORK_SINK_STATUS` event below — hosts use it
 * to drive a status row in the audio settings panel.
 */

/*
 * Active audio sink. Mirrors `crate::sink_slot::AudioSinkType`
 * on the Rust side. Stable discriminants — never reorder.
 */
typedef enum SdrAudioSinkType {
    SDR_AUDIO_SINK_LOCAL   = 0,
    SDR_AUDIO_SINK_NETWORK = 1,
} SdrAudioSinkType;

/*
 * Network stream protocol. Reused by the network audio sink
 * config command and the matching status payload below. Stable
 * discriminants — never reorder.
 *
 * Note: `TCP_SERVER` mirrors `sdr_types::Protocol::TcpClient`
 * on the Rust side. The historical "client" naming there comes
 * from SDR++ — the device acts as the TCP **server** accepting
 * client connections. The C ABI uses the clearer name.
 */
typedef enum SdrNetworkProtocol {
    SDR_NETWORK_PROTOCOL_TCP_SERVER = 0,
    SDR_NETWORK_PROTOCOL_UDP        = 1,
} SdrNetworkProtocol;

/*
 * Switch the active audio sink type. `sink_type` must be one
 * of `SDR_AUDIO_SINK_*`. Returns `SDR_CORE_ERR_INVALID_ARG`
 * for unknown values.
 */
int32_t sdr_core_set_audio_sink_type(SdrCore* handle, int32_t sink_type);

/*
 * Configure the network audio sink endpoint. `hostname_utf8`
 * must be non-null, non-empty, NUL-terminated UTF-8.
 * `protocol` must be one of `SDR_NETWORK_PROTOCOL_*`.
 *
 * If the network sink is currently active the engine rebuilds
 * it inline so the new endpoint takes effect immediately;
 * otherwise the values are stored for the next switch.
 */
int32_t sdr_core_set_network_sink_config(
    SdrCore*    handle,
    const char* hostname_utf8,
    uint16_t    port,
    int32_t     protocol
);

/*
 * Start / stop recording the demodulated audio stream to a 16-bit
 * PCM WAV file. The engine opens the file on `start`, writes every
 * decoded frame while recording is active, and finalizes the WAV
 * header when the writer drops on `stop`.
 *
 * `start` emits `SDR_EVT_AUDIO_RECORDING_STARTED` on success or
 * `SDR_EVT_ERROR` if the file couldn't be opened / written.
 * `stop` emits `SDR_EVT_AUDIO_RECORDING_STOPPED` (including when
 * no recording was active — the event is the host's signal to
 * clear its "recording" UI regardless of prior state).
 *
 * `path_utf8` must be a non-empty NUL-terminated UTF-8 path; the
 * host is responsible for picking a writable location. Sample rate
 * and channel count are engine-determined (currently
 * AUDIO_SAMPLE_RATE at AUDIO_CHANNELS — see the engine constants).
 */
int32_t sdr_core_start_audio_recording(SdrCore* handle, const char* path_utf8);
int32_t sdr_core_stop_audio_recording(SdrCore* handle);

/*
 * Start / stop recording the raw IQ sample stream to a WAV file.
 * Unlike audio recording — which writes at a fixed 48 kHz —
 * the IQ WAV is written at the current tuner sample rate with
 * two channels (I / Q), so file size per second varies with
 * the source sample rate selection.
 *
 * `start` emits `SDR_EVT_IQ_RECORDING_STARTED` on success or
 * `SDR_EVT_ERROR` if the file couldn't be opened / written.
 * `stop` emits `SDR_EVT_IQ_RECORDING_STOPPED` (including when
 * no recording was active — the event is the host's signal to
 * clear its "recording" UI regardless of prior state).
 *
 * `path_utf8` must be a non-empty NUL-terminated UTF-8 path;
 * the host is responsible for picking a writable location.
 */
int32_t sdr_core_start_iq_recording(SdrCore* handle, const char* path_utf8);
int32_t sdr_core_stop_iq_recording(SdrCore* handle);

/* ================================================================ */
/*  RadioReference integration                                      */
/* ================================================================ */

/*
 * Credential storage and frequency lookups against RadioReference.com
 * (issue #241). All handle-free — they don't touch the engine or
 * DSP thread.
 *
 * Credentials are kept in the OS keyring (Keychain on macOS,
 * libsecret / KeePassXC on Linux) under the SAME service name +
 * key names the GTK UI uses, so a user running both apps on one
 * machine shares a single login.
 *
 * Search (`_search_zip`) returns a JSON document in a caller-
 * allocated buffer — see the function contract for the schema.
 * Callers SHOULD dispatch these calls on a background thread; the
 * underlying HTTP is synchronous blocking and can take multiple
 * seconds on a slow connection.
 */

/*
 * Store RadioReference credentials in the OS keyring. Both
 * pointers must be non-null, non-empty, NUL-terminated UTF-8
 * strings. Returns `SDR_CORE_OK` on success,
 * `SDR_CORE_ERR_INVALID_ARG` on null pointers or empty fields
 * (the rest of the ABI uses empty-buffer as the "not stored"
 * sentinel, so an empty save would be self-inconsistent),
 * `SDR_CORE_ERR_IO` on keyring backend errors.
 */
int32_t sdr_core_radioreference_save_credentials(
    const char* user_utf8,
    const char* pass_utf8
);

/*
 * Load stored credentials into caller-allocated buffers. Both
 * buffers are NUL-terminated on success; values longer than the
 * buffer are truncated (not an error).
 *
 * Return semantics:
 *   - `SDR_CORE_OK` with both buffers filled → credentials
 *     present and copied out.
 *   - `SDR_CORE_OK` with either buffer containing only the NUL
 *     terminator (empty string) → no credentials stored. This
 *     is a normal state, not an error — callers check for an
 *     empty output buffer to detect "not yet configured."
 *   - `SDR_CORE_ERR_IO` → the keyring backend itself failed
 *     (service unavailable, platform error, …). Distinct from
 *     the empty-output "not stored" case so a broken backend
 *     doesn't masquerade as "no credentials."
 *   - `SDR_CORE_ERR_INVALID_ARG` → null buffers, or either
 *     `_buf_len` < 2. A 1-byte buffer can only hold the NUL,
 *     which would collide with the "empty ⇒ not stored"
 *     sentinel — callers must pass at least two bytes so a
 *     single-character credential can still be distinguished
 *     from "nothing stored."
 */
int32_t sdr_core_radioreference_load_credentials(
    char*  out_user,
    size_t user_buf_len,
    char*  out_pass,
    size_t pass_buf_len
);

/*
 * Delete any stored credentials. Idempotent — returns
 * `SDR_CORE_OK` whether or not credentials were present.
 */
int32_t sdr_core_radioreference_delete_credentials(void);

/*
 * Cheap existence probe — returns `true` if both username and
 * password are stored AND non-empty, `false` otherwise.
 * Does not load the values into caller memory; use this to gate
 * "show RadioReference panel" in the UI without surfacing the
 * password.
 */
bool sdr_core_radioreference_has_credentials(void);

/*
 * Test credentials with a lightweight RadioReference API probe
 * (ZIP 90210). Returns `SDR_CORE_OK` on valid credentials,
 * `SDR_CORE_ERR_AUTH` when RR rejected the login,
 * `SDR_CORE_ERR_IO` on network failure,
 * `SDR_CORE_ERR_INVALID_ARG` on empty or null inputs.
 *
 * Does not touch the keyring — caller supplies the credentials
 * to test (typically from a Settings-pane form before saving).
 */
int32_t sdr_core_radioreference_test_credentials(
    const char* user_utf8,
    const char* pass_utf8
);

/*
 * Search RadioReference for frequencies covering a US ZIP code.
 * Performs `getZipcodeInfo(zip)` to resolve the county, then
 * `getCountyFrequencies(county_id)` to fetch all tagged
 * frequencies.
 *
 * Writes a JSON document to `out_buf` when the buffer is
 * large enough to hold the full payload plus a trailing NUL.
 * If it isn't, the function returns
 * `SDR_CORE_ERR_INVALID_ARG`, writes the required allocation
 * size (NUL-inclusive) to `out_required` when non-null, and
 * leaves `out_buf` untouched — callers should reallocate to
 * `*out_required` bytes and retry. A truncated JSON body is
 * never returned. The schema is:
 *
 *   {
 *     "county_id":   <u32>,
 *     "county_name": "<string>",
 *     "state_id":    <u32>,
 *     "city":        "<string>",
 *     "frequencies": [
 *       {
 *         "id":           "<string>",      // opaque RR frequency ID
 *         "freq_hz":      <u64>,
 *         "rr_mode":      "<string>",      // raw RR mode ("FM", "FMN", …)
 *         "demod_mode":   "<string>",      // engine mode ("NFM", "WFM", …)
 *         "bandwidth_hz": <f64>,           // mapped channel bandwidth
 *         "tone_hz":      <f32 | null>,    // CTCSS / PL tone if present
 *         "description":  "<string>",
 *         "alpha_tag":    "<string>",
 *         "category":     "<string>",      // first tag description
 *         "tags":         ["<string>", …]  // all tag descriptions
 *       },
 *       ...
 *     ]
 *   }
 *
 * If `out_required` is non-null, it is filled with the exact
 * number of bytes the caller must allocate to receive the full
 * payload — **including the trailing NUL** — whether or not the
 * buffer was large enough.
 *
 * Returns `SDR_CORE_OK` on success, `SDR_CORE_ERR_AUTH` on
 * rejected credentials, `SDR_CORE_ERR_IO` on network failure,
 * `SDR_CORE_ERR_INVALID_ARG` on malformed ZIP, null buffers,
 * **or a too-small output buffer** (the caller should read
 * `out_required` and retry with a larger allocation),
 * `SDR_CORE_ERR_INTERNAL` on JSON encoding failure.
 *
 * Blocking: this is synchronous HTTP against RadioReference.com.
 * Callers MUST dispatch it on a background thread so the UI /
 * event loop stays responsive.
 */
int32_t sdr_core_radioreference_search_zip(
    const char* user_utf8,
    const char* pass_utf8,
    const char* zip_utf8,
    char*       out_buf,
    size_t      out_buf_len,
    size_t*     out_required
);

/* --- IQ frontend -------------------------------------------------- */

int32_t sdr_core_set_dc_blocking(SdrCore* handle, bool enabled);
int32_t sdr_core_set_iq_inversion(SdrCore* handle, bool enabled);
int32_t sdr_core_set_iq_correction(SdrCore* handle, bool enabled);

/* --- Spectrum display --------------------------------------------- */

/*
 * The `sdr-pipeline::iq_frontend::FftWindow` enum currently has
 * three variants. Hann/Hamming are not exposed because the Rust
 * engine doesn't implement them; they can be added later with a
 * minor ABI bump if we grow the upstream enum.
 */
typedef enum SdrFftWindow {
    SDR_FFT_WIN_RECT     = 0,
    SDR_FFT_WIN_BLACKMAN = 1,
    SDR_FFT_WIN_NUTTALL  = 2,
} SdrFftWindow;

/* `n` must be a nonzero power of two, at most 65536. */
int32_t sdr_core_set_fft_size(SdrCore* handle, size_t n);
int32_t sdr_core_set_fft_window(SdrCore* handle, int32_t window);
int32_t sdr_core_set_fft_rate(SdrCore* handle, double fps);

/* ================================================================ */
/*  Events                                                          */
/* ================================================================ */

/*
 * Event delivery model:
 *
 *   - The FFI starts a dedicated "event dispatcher" thread at
 *     `sdr_core_create` time. That thread owns the engine's event
 *     receiver and loops reading from it.
 *
 *   - Hosts register a callback via `sdr_core_set_event_callback`.
 *     The callback fires on the dispatcher thread — NOT on the
 *     host's main thread — so hosts are responsible for marshaling
 *     to whatever thread they want to do UI work on.
 *
 *   - Events that arrive before a callback is registered are
 *     silently dropped. Hosts should register a callback
 *     immediately after `sdr_core_create` and before
 *     `sdr_core_start` to avoid missing the initial DeviceInfo /
 *     GainList / DisplayBandwidth events the pipeline fires when
 *     the source opens.
 *
 *   - Borrowed pointers inside the event (`device_info.utf8`,
 *     `gain_list.values`, `error.utf8`) are valid only for the
 *     duration of the callback call. Hosts that want to persist
 *     the data must copy it out before returning.
 *
 *   - The callback is safe to be reentrant with other
 *     `sdr_core_*` calls except for `sdr_core_destroy` and
 *     `sdr_core_set_event_callback`. Destroy from inside the
 *     callback is unsupported (self-join detected and skipped,
 *     teardown incomplete). Set-event-callback from inside the
 *     callback is rejected with `SDR_CORE_ERR_INVALID_ARG`
 *     (the quiescence wait would deadlock against the in-flight
 *     dispatch).
 */

typedef enum SdrEventKind {
    SDR_EVT_SOURCE_STOPPED          = 1,
    SDR_EVT_SAMPLE_RATE_CHANGED     = 2,
    SDR_EVT_SIGNAL_LEVEL            = 3,
    SDR_EVT_DEVICE_INFO             = 4,
    SDR_EVT_GAIN_LIST               = 5,
    SDR_EVT_DISPLAY_BANDWIDTH       = 6,
    SDR_EVT_ERROR                   = 7,
    SDR_EVT_AUDIO_RECORDING_STARTED = 8,
    SDR_EVT_AUDIO_RECORDING_STOPPED = 9,
    SDR_EVT_IQ_RECORDING_STARTED    = 10,
    SDR_EVT_IQ_RECORDING_STOPPED    = 11,
    SDR_EVT_NETWORK_SINK_STATUS     = 12, /* ABI 0.9 — issue #247 */
} SdrEventKind;

/* Discriminants for the `kind` field of `SdrEventNetworkSinkStatus`
 * below. Stable — never reorder.
 */
typedef enum SdrNetworkSinkStatusKind {
    SDR_NETWORK_SINK_STATUS_INACTIVE = 0,
    SDR_NETWORK_SINK_STATUS_ACTIVE   = 1,
    SDR_NETWORK_SINK_STATUS_ERROR    = 2,
} SdrNetworkSinkStatusKind;

/*
 * Payload for SDR_EVT_DEVICE_INFO. `utf8` is a NUL-terminated
 * UTF-8 string borrowed from dispatcher-owned storage; valid only
 * for the duration of the callback.
 */
typedef struct SdrEventDeviceInfo {
    const char* utf8;
} SdrEventDeviceInfo;

/*
 * Payload for SDR_EVT_GAIN_LIST. `values` is a borrowed pointer
 * to `len` contiguous `double` gain values in dB, ordered as the
 * tuner reports them. Valid only for the duration of the callback.
 */
typedef struct SdrEventGainList {
    const double* values;
    size_t len;
} SdrEventGainList;

/*
 * Payload for SDR_EVT_ERROR. `utf8` is a NUL-terminated UTF-8
 * error message borrowed from dispatcher-owned storage.
 */
typedef struct SdrEventError {
    const char* utf8;
} SdrEventError;

/*
 * Payload for SDR_EVT_AUDIO_RECORDING_STARTED. `path_utf8` is the
 * NUL-terminated UTF-8 filesystem path the engine opened for
 * writing — borrowed from dispatcher-owned storage; valid only
 * for the duration of the callback.
 *
 * SDR_EVT_AUDIO_RECORDING_STOPPED carries no payload.
 */
typedef struct SdrEventAudioRecording {
    const char* path_utf8;
} SdrEventAudioRecording;

/*
 * Payload for SDR_EVT_IQ_RECORDING_STARTED. Same layout as
 * SdrEventAudioRecording but declared separately so hosts can
 * switch cleanly on `kind` without needing to remember which
 * union field to read, and so the two feature paths can diverge
 * in a later version (e.g. if IQ recording grows a sample-rate
 * field).
 *
 * SDR_EVT_IQ_RECORDING_STOPPED carries no payload.
 */
typedef struct SdrEventIqRecording {
    const char* path_utf8;
} SdrEventIqRecording;

/*
 * Payload for SDR_EVT_NETWORK_SINK_STATUS. Tagged by `kind`
 * (one of `SDR_NETWORK_SINK_STATUS_*`):
 *
 * | kind                              | utf8                  | protocol                |
 * |-----------------------------------|-----------------------|-------------------------|
 * | SDR_NETWORK_SINK_STATUS_INACTIVE  | NULL                  | -1 (unused)             |
 * | SDR_NETWORK_SINK_STATUS_ACTIVE    | endpoint host:port    | SDR_NETWORK_PROTOCOL_*  |
 * | SDR_NETWORK_SINK_STATUS_ERROR     | error message         | -1 (unused)             |
 *
 * `utf8` is borrowed from dispatcher-owned storage; valid only
 * for the duration of the callback. Per issue #247.
 */
typedef struct SdrEventNetworkSinkStatus {
    int32_t     kind;
    const char* utf8;
    int32_t     protocol;
} SdrEventNetworkSinkStatus;

/*
 * Tagged union of all event payloads. Which union field is valid
 * is determined by the `kind` discriminant on the enclosing
 * SdrEvent (see the table below).
 *
 * Kind                              -> Valid field
 * ---------------------------------   ---------------------------
 * SDR_EVT_SOURCE_STOPPED              none (all-zero payload)
 * SDR_EVT_SAMPLE_RATE_CHANGED         sample_rate_hz
 * SDR_EVT_SIGNAL_LEVEL                signal_level_db
 * SDR_EVT_DISPLAY_BANDWIDTH           display_bandwidth_hz
 * SDR_EVT_DEVICE_INFO                 device_info.utf8
 * SDR_EVT_GAIN_LIST                   gain_list.{values,len}
 * SDR_EVT_ERROR                       error.utf8
 * SDR_EVT_AUDIO_RECORDING_STARTED     audio_recording.path_utf8
 * SDR_EVT_AUDIO_RECORDING_STOPPED     none (all-zero payload)
 * SDR_EVT_IQ_RECORDING_STARTED        iq_recording.path_utf8
 * SDR_EVT_IQ_RECORDING_STOPPED        none (all-zero payload)
 * SDR_EVT_NETWORK_SINK_STATUS         network_sink_status.{kind,utf8,protocol}
 */
typedef union SdrEventPayload {
    double sample_rate_hz;
    float  signal_level_db;
    double display_bandwidth_hz;
    SdrEventDeviceInfo        device_info;
    SdrEventGainList          gain_list;
    SdrEventError             error;
    SdrEventAudioRecording    audio_recording;
    SdrEventIqRecording       iq_recording;
    SdrEventNetworkSinkStatus network_sink_status;
    /* Placeholder so kinds with no payload (e.g., SOURCE_STOPPED)
     * have a well-defined zeroed payload representation. */
    uint64_t _placeholder;
} SdrEventPayload;

typedef struct SdrEvent {
    int32_t         kind;
    SdrEventPayload payload;
} SdrEvent;

/*
 * Host-supplied callback signature. `event` is a borrowed pointer
 * valid only for the duration of the call. `user_data` is the
 * same opaque pointer the host passed to
 * `sdr_core_set_event_callback`.
 */
typedef void (*SdrEventCallback)(const SdrEvent* event, void* user_data);

/*
 * Register (or clear) the host's event callback.
 *
 * Passing a non-null `callback` registers it with the given
 * `user_data`; passing a null `callback` clears any previously-
 * registered callback (events that arrive subsequently are
 * silently dropped).
 *
 * Thread-safe. Safe from any thread. Not safe from inside the
 * callback itself (the implementation takes the callback-slot
 * mutex).
 */
int32_t sdr_core_set_event_callback(
    SdrCore*         handle,
    SdrEventCallback callback,
    void*            user_data
);

/* ================================================================ */
/*  Audio tap (ABI 0.8)                                             */
/* ================================================================ */

/*
 * Stream post-demod audio to a host-side consumer at 16 kHz mono
 * f32. Primary use case: feeding macOS `SpeechAnalyzer` /
 * `SpeechTranscriber` for the transcription panel (issue #314).
 *
 * Shape: push-style via a C callback. Each time the engine
 * finishes an audio block the DSP thread downsamples the stereo
 * 48 kHz buffer to mono 16 kHz and hands the chunk to a bounded
 * queue drained by a dedicated FFI dispatcher thread. The
 * dispatcher invokes the host callback with a pointer into the
 * chunk and the chunk length (sample count, not bytes).
 *
 * Only one tap can be active per handle at a time. Calling
 * `_start_audio_tap` a second time without an intervening
 * `_stop_audio_tap` returns `SDR_CORE_ERR_INVALID_HANDLE` with
 * a descriptive last-error message. Callers are expected to tear
 * down and restart if they want to swap callbacks.
 *
 * Lifetime: the registered callback + `user_data` must remain
 * valid between `_start_audio_tap` and the matching
 * `_stop_audio_tap` (or until `sdr_core_destroy`, which stops
 * the tap as part of teardown). `_stop_audio_tap` joins the
 * dispatcher thread before returning, so the host can
 * deterministically free `user_data` immediately on the next
 * line.
 *
 * Thread: the callback fires on the dispatcher thread (named
 * `sdr-ffi-audio-tap-dispatcher`), NOT the host's main thread.
 * Hosts that need main-actor work (SwiftUI state updates, etc.)
 * must marshal across.
 */

/*
 * Audio-tap callback.
 *
 * `samples` points to an audio chunk buffer owned by the
 * dispatcher (not on its stack — it's a heap Vec borrowed
 * for the duration of the call). Valid only for the duration
 * of this call. `sample_count` is the
 * number of `float` samples (not bytes). Format: 16 kHz mono
 * f32. `user_data` is the opaque pointer the host passed at
 * registration.
 */
typedef void (*SdrAudioTapCallback)(
    const float* samples,
    size_t       sample_count,
    void*        user_data
);

/*
 * Start streaming audio to `callback`. `callback` must be
 * non-null. `user_data` may be null; it's opaque to the FFI.
 *
 * Returns `SDR_CORE_OK` on success,
 * `SDR_CORE_ERR_INVALID_HANDLE` when `handle` is null or a tap
 * is already active, `SDR_CORE_ERR_INVALID_ARG` when `callback`
 * is null, or `SDR_CORE_ERR_NOT_RUNNING` when the engine's
 * command channel is disconnected.
 */
int32_t sdr_core_start_audio_tap(
    SdrCore*            handle,
    SdrAudioTapCallback callback,
    void*               user_data
);

/*
 * Stop an active tap. Idempotent — returns `SDR_CORE_OK` when
 * no tap is active. Blocks until the dispatcher thread has
 * joined so the host can deterministically free `user_data`
 * immediately after the call.
 *
 * Must NOT be called from inside the audio-tap callback — the
 * implementation joins the dispatcher thread, which would
 * self-deadlock against a callback still running on that
 * thread.
 */
int32_t sdr_core_stop_audio_tap(SdrCore* handle);

/* ================================================================ */
/*  FFT frame pull                                                  */
/* ================================================================ */

/*
 * Unlike the per-event callback surface above, FFT frames are
 * delivered on the host's render tick via a **pull** function:
 * the host calls `sdr_core_pull_fft` from inside its render loop
 * (SwiftUI `MTKView::draw(in:)` on the Metal path; GTK's
 * `glib::timeout_add_local` on the Linux path) and the call
 * synchronously hands the most recent FFT frame to a host
 * callback — or returns `false` without calling anything when no
 * new frame has arrived since the previous pull.
 *
 * Rationale: rendering happens at display rate (usually 60 fps)
 * and FFT generation happens at the engine's internal rate
 * (default 20 fps). Pushing every frame through the event
 * callback would force a full struct-translation + mutex-hold +
 * allocation for data that might be discarded before the
 * renderer gets to it. Pulling means zero work on any tick
 * where no new frame is ready, and zero cross-thread traffic on
 * the hot path.
 */

/*
 * FFT frame descriptor. `magnitudes_db` points into the engine's
 * shared FFT buffer and is valid only for the duration of the
 * callback. `len` is the current FFT bin count.
 *
 * `sample_rate_hz` is the effective (post-decimation) sample
 * rate and `center_freq_hz` is the tuner center frequency as
 * observed when the frame was published. In v1 both are set to
 * 0.0 because the engine does not yet thread this context
 * alongside the FFT frame; hosts should correlate with the
 * `SDR_EVT_SAMPLE_RATE_CHANGED` event until the thread-through
 * lands. The fields are exposed in the struct so adding the
 * data later does not require an ABI change.
 */
typedef struct SdrFftFrame {
    const float* magnitudes_db;
    size_t       len;
    double       sample_rate_hz;
    double       center_freq_hz;
} SdrFftFrame;

/*
 * Callback signature. Fires synchronously from within
 * `sdr_core_pull_fft` when a new frame is available. The
 * `frame` pointer (and the `magnitudes_db` slice inside it) are
 * valid only for the duration of this call — copy the data out
 * if you need it later.
 */
typedef void (*SdrFftCallback)(const SdrFftFrame* frame, void* user_data);

/*
 * Pull the latest FFT frame, if a new one is available.
 *
 * Returns `true` and invokes `callback` synchronously when a new
 * frame has been published since the previous pull. Returns
 * `false` without calling `callback` when no new frame is
 * ready — hosts render the previous frame again (or skip
 * rendering) in that case.
 *
 * Fast path when no new frame is available. Acquires the shared
 * FFT buffer's mutex briefly when a frame is being handed to the
 * host.
 *
 * Safe from any thread, but in practice hosts call this from
 * their render loop (which is on the main / display-linked
 * thread). The FFI imposes no threading constraint on the
 * call site itself.
 *
 * Passing a null `callback` is allowed and means "probe
 * only" — returns whether a frame is available without handing
 * it to anyone.
 */
bool sdr_core_pull_fft(
    SdrCore*        handle,
    SdrFftCallback  callback,
    void*           user_data
);

#ifdef __cplusplus
}
#endif

#endif /* SDR_CORE_H */
