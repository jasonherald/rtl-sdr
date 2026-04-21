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
pub const SDR_EVT_AUDIO_RECORDING_STARTED: i32 = 8;
pub const SDR_EVT_AUDIO_RECORDING_STOPPED: i32 = 9;
pub const SDR_EVT_IQ_RECORDING_STARTED: i32 = 10;
pub const SDR_EVT_IQ_RECORDING_STOPPED: i32 = 11;
pub const SDR_EVT_NETWORK_SINK_STATUS: i32 = 12;
pub const SDR_EVT_RTL_TCP_CONNECTION_STATE: i32 = 13;

// ============================================================
//  Network sink status discriminants — must match the
//  matching `SdrNetworkSinkStatusKind` enum in
//  `include/sdr_core.h`. Never reorder or renumber.
// ============================================================

pub const SDR_NETWORK_SINK_STATUS_INACTIVE: i32 = 0;
pub const SDR_NETWORK_SINK_STATUS_ACTIVE: i32 = 1;
pub const SDR_NETWORK_SINK_STATUS_ERROR: i32 = 2;

// ============================================================
//  Network protocol discriminants — must match the matching
//  `SdrNetworkProtocol` enum in `include/sdr_core.h`. Reused
//  by both `sdr_core_set_network_sink_config` (command path)
//  and the network-sink-status payload (event path). Never
//  reorder or renumber.
// ============================================================

pub const SDR_NETWORK_PROTOCOL_TCP_SERVER: i32 = 0;
pub const SDR_NETWORK_PROTOCOL_UDP: i32 = 1;

// ============================================================
//  rtl_tcp connection-state discriminants — must match
//  `SdrRtlTcpConnectionStateKind` in `include/sdr_core.h`.
//  Never reorder or renumber. ABI 0.11.
// ============================================================

pub const SDR_RTL_TCP_STATE_DISCONNECTED: i32 = 0;
pub const SDR_RTL_TCP_STATE_CONNECTING: i32 = 1;
pub const SDR_RTL_TCP_STATE_CONNECTED: i32 = 2;
pub const SDR_RTL_TCP_STATE_RETRYING: i32 = 3;
pub const SDR_RTL_TCP_STATE_FAILED: i32 = 4;

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

/// Payload for `SDR_EVT_AUDIO_RECORDING_STARTED`. Borrowed pointer
/// to the filesystem path the engine opened for writing. Valid only
/// for the duration of the callback.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrEventAudioRecording {
    pub path_utf8: *const c_char,
}

/// Payload for `SDR_EVT_IQ_RECORDING_STARTED`. Same layout as
/// `SdrEventAudioRecording` but declared separately so the union
/// field name stays self-documenting for hosts and so the two
/// feature paths can diverge in the future (e.g. if IQ recording
/// grows a sample-rate field in the payload) without touching the
/// audio path.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrEventIqRecording {
    pub path_utf8: *const c_char,
}

/// Payload for `SDR_EVT_RTL_TCP_CONNECTION_STATE`. Tagged by
/// `kind` (one of `SDR_RTL_TCP_STATE_*`):
///
/// | `kind`                          | `utf8`            | `attempt` | `retry_in_secs` | `gain_count` |
/// |---------------------------------|-------------------|-----------|-----------------|--------------|
/// | `SDR_RTL_TCP_STATE_DISCONNECTED`| NULL              | 0         | 0.0             | 0            |
/// | `SDR_RTL_TCP_STATE_CONNECTING`  | NULL              | 0         | 0.0             | 0            |
/// | `SDR_RTL_TCP_STATE_CONNECTED`   | tuner name        | 0         | 0.0             | gain steps   |
/// | `SDR_RTL_TCP_STATE_RETRYING`    | NULL              | attempt#  | seconds         | 0            |
/// | `SDR_RTL_TCP_STATE_FAILED`      | reason            | 0         | 0.0             | 0            |
///
/// `utf8` is a borrowed pointer into dispatcher-owned storage;
/// valid only for the duration of the callback. Per issue #325.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrEventRtlTcpConnectionState {
    pub kind: i32,
    pub utf8: *const c_char,
    pub attempt: u32,
    pub retry_in_secs: f64,
    pub gain_count: u32,
}

/// Payload for `SDR_EVT_NETWORK_SINK_STATUS`. Tagged by `kind`
/// (one of `SDR_NETWORK_SINK_STATUS_*`):
///
/// | `kind`                                | `utf8`             | `protocol`              |
/// |---------------------------------------|--------------------|-------------------------|
/// | `SDR_NETWORK_SINK_STATUS_INACTIVE`    | NULL               | -1 (unused)             |
/// | `SDR_NETWORK_SINK_STATUS_ACTIVE`      | endpoint host:port | `SDR_NETWORK_PROTOCOL_*`|
/// | `SDR_NETWORK_SINK_STATUS_ERROR`       | error message      | -1 (unused)             |
///
/// `utf8` is a borrowed pointer into dispatcher-owned storage;
/// valid only for the duration of the callback. Per issue #247.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrEventNetworkSinkStatus {
    pub kind: i32,
    pub utf8: *const c_char,
    pub protocol: i32,
}

/// C-layout tagged union of event payloads. Which field is valid
/// is determined by the `kind` discriminant on the enclosing
/// `SdrEvent`:
///
/// | `kind`                            | Valid field                  |
/// |-----------------------------------|------------------------------|
/// | `SDR_EVT_SOURCE_STOPPED`          | none                         |
/// | `SDR_EVT_SAMPLE_RATE_CHANGED`     | `sample_rate_hz`             |
/// | `SDR_EVT_SIGNAL_LEVEL`            | `signal_level_db`            |
/// | `SDR_EVT_DEVICE_INFO`             | `device_info.utf8`           |
/// | `SDR_EVT_GAIN_LIST`               | `gain_list.{values,len}`     |
/// | `SDR_EVT_DISPLAY_BANDWIDTH`       | `display_bandwidth_hz`       |
/// | `SDR_EVT_ERROR`                   | `error.utf8`                 |
/// | `SDR_EVT_AUDIO_RECORDING_STARTED` | `audio_recording.path_utf8`  |
/// | `SDR_EVT_AUDIO_RECORDING_STOPPED` | none                         |
/// | `SDR_EVT_IQ_RECORDING_STARTED`    | `iq_recording.path_utf8`     |
/// | `SDR_EVT_IQ_RECORDING_STOPPED`    | none                         |
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
    pub audio_recording: SdrEventAudioRecording,
    pub iq_recording: SdrEventIqRecording,
    pub network_sink_status: SdrEventNetworkSinkStatus,
    pub rtl_tcp_connection_state: SdrEventRtlTcpConnectionState,
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
///
/// The `#[allow(clippy::too_many_lines)]` here is deliberate: the
/// function is a single `match` on the `DspToUi` enum where each
/// arm is the minimum translation for one variant. Splitting it
/// into per-variant helpers would push the `owned_cstring` /
/// `owned_vec` lifetime plumbing across function boundaries
/// without making the logic easier to read. The length grows
/// linearly with each new event kind — that's inherent to this
/// file's job.
#[allow(clippy::too_many_lines)]
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

        DspToUi::AudioRecordingStarted(path) => {
            // Sanitize interior NULs rather than dropping the event
            // on an unusual path (same policy as DeviceInfo).
            let sanitized = path.to_string_lossy().replace('\0', "?");
            let Ok(cstr) = CString::new(sanitized) else {
                return None;
            };
            let ptr = cstr.as_ptr();
            owned_cstring = Some(cstr);
            SdrEvent {
                kind: SDR_EVT_AUDIO_RECORDING_STARTED,
                payload: SdrEventPayload {
                    audio_recording: SdrEventAudioRecording { path_utf8: ptr },
                },
            }
        }

        DspToUi::AudioRecordingStopped => SdrEvent {
            kind: SDR_EVT_AUDIO_RECORDING_STOPPED,
            payload: SdrEventPayload { _placeholder: 0 },
        },

        DspToUi::IqRecordingStarted(path) => {
            // Same sanitize-then-CString pattern as AudioRecordingStarted.
            let sanitized = path.to_string_lossy().replace('\0', "?");
            let Ok(cstr) = CString::new(sanitized) else {
                return None;
            };
            let ptr = cstr.as_ptr();
            owned_cstring = Some(cstr);
            SdrEvent {
                kind: SDR_EVT_IQ_RECORDING_STARTED,
                payload: SdrEventPayload {
                    iq_recording: SdrEventIqRecording { path_utf8: ptr },
                },
            }
        }

        DspToUi::IqRecordingStopped => SdrEvent {
            kind: SDR_EVT_IQ_RECORDING_STOPPED,
            payload: SdrEventPayload { _placeholder: 0 },
        },

        DspToUi::NetworkSinkStatus(status) => {
            use sdr_core::NetworkSinkStatus;
            // Translate the three status variants into the C
            // tagged-payload shape. Borrowed strings get
            // promoted to `CString` so they outlive the
            // dispatcher's call into the host callback.
            // Per issue #247 PR 2.
            let (kind, message_cstr, protocol_int) = match status {
                NetworkSinkStatus::Inactive => (SDR_NETWORK_SINK_STATUS_INACTIVE, None, -1_i32),
                NetworkSinkStatus::Active { endpoint, protocol } => {
                    let sanitized = endpoint.replace('\0', "?");
                    let Ok(cstr) = CString::new(sanitized) else {
                        // Unreachable: replace stripped NULs.
                        return None;
                    };
                    let proto = match protocol {
                        sdr_types::Protocol::TcpClient => SDR_NETWORK_PROTOCOL_TCP_SERVER,
                        sdr_types::Protocol::Udp => SDR_NETWORK_PROTOCOL_UDP,
                    };
                    (SDR_NETWORK_SINK_STATUS_ACTIVE, Some(cstr), proto)
                }
                NetworkSinkStatus::Error { message } => {
                    let sanitized = message.replace('\0', "?");
                    let Ok(cstr) = CString::new(sanitized) else {
                        return None;
                    };
                    (SDR_NETWORK_SINK_STATUS_ERROR, Some(cstr), -1_i32)
                }
            };
            let utf8 = message_cstr
                .as_ref()
                .map_or(std::ptr::null(), |c| c.as_ptr());
            owned_cstring = message_cstr;
            SdrEvent {
                kind: SDR_EVT_NETWORK_SINK_STATUS,
                payload: SdrEventPayload {
                    network_sink_status: SdrEventNetworkSinkStatus {
                        kind,
                        utf8,
                        protocol: protocol_int,
                    },
                },
            }
        }

        // Variants not yet exposed at the FFI boundary. Silently
        // dropped in v1; a future ABI minor bump grows the surface
        // to cover them as each feature lands in the macOS SwiftUI
        // host.
        //
        // Specifically:
        //   - `FftData` is intentionally never routed through the
        //     event callback — FFT frames go through the dedicated
        //     pull function (`sdr_core_pull_fft`) instead so the
        //     render loop stays on the main thread.
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
        DspToUi::RtlTcpConnectionState(state) => {
            use sdr_types::RtlTcpConnectionState;
            // Translate into the C tagged-payload shape.
            // Variants with a borrowed string promote to
            // `CString` so the pointer stays valid for the
            // duration of the host callback (same ownership
            // pattern as the network sink status event).
            let (kind, message_cstr, attempt, retry_in_secs, gain_count) = match state {
                RtlTcpConnectionState::Disconnected => {
                    (SDR_RTL_TCP_STATE_DISCONNECTED, None, 0_u32, 0.0_f64, 0_u32)
                }
                RtlTcpConnectionState::Connecting => {
                    (SDR_RTL_TCP_STATE_CONNECTING, None, 0, 0.0, 0)
                }
                RtlTcpConnectionState::Connected {
                    tuner_name,
                    gain_count,
                } => {
                    let sanitized = tuner_name.replace('\0', "?");
                    let Ok(cstr) = CString::new(sanitized) else {
                        return None;
                    };
                    (SDR_RTL_TCP_STATE_CONNECTED, Some(cstr), 0, 0.0, *gain_count)
                }
                RtlTcpConnectionState::Retrying { attempt, retry_in } => (
                    SDR_RTL_TCP_STATE_RETRYING,
                    None,
                    *attempt,
                    retry_in.as_secs_f64(),
                    0,
                ),
                RtlTcpConnectionState::Failed { reason } => {
                    let sanitized = reason.replace('\0', "?");
                    let Ok(cstr) = CString::new(sanitized) else {
                        return None;
                    };
                    (SDR_RTL_TCP_STATE_FAILED, Some(cstr), 0, 0.0, 0)
                }
            };
            let utf8 = message_cstr
                .as_ref()
                .map_or(std::ptr::null(), |c| c.as_ptr());
            owned_cstring = message_cstr;
            SdrEvent {
                kind: SDR_EVT_RTL_TCP_CONNECTION_STATE,
                payload: SdrEventPayload {
                    rtl_tcp_connection_state: SdrEventRtlTcpConnectionState {
                        kind,
                        utf8,
                        attempt,
                        retry_in_secs,
                        gain_count,
                    },
                },
            }
        }

        DspToUi::FftData(_)
        | DspToUi::DemodModeChanged(_)
        | DspToUi::BandwidthChanged(_)
        | DspToUi::CtcssSustainedChanged(_)
        | DspToUi::VoiceSquelchOpenChanged(_) => return None,
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
        assert_eq!(SDR_EVT_AUDIO_RECORDING_STARTED, 8);
        assert_eq!(SDR_EVT_AUDIO_RECORDING_STOPPED, 9);
        assert_eq!(SDR_EVT_IQ_RECORDING_STARTED, 10);
        assert_eq!(SDR_EVT_IQ_RECORDING_STOPPED, 11);
        assert_eq!(SDR_EVT_NETWORK_SINK_STATUS, 12);
        assert_eq!(SDR_EVT_RTL_TCP_CONNECTION_STATE, 13);
    }

    #[test]
    fn rtl_tcp_state_discriminants_match_header() {
        assert_eq!(SDR_RTL_TCP_STATE_DISCONNECTED, 0);
        assert_eq!(SDR_RTL_TCP_STATE_CONNECTING, 1);
        assert_eq!(SDR_RTL_TCP_STATE_CONNECTED, 2);
        assert_eq!(SDR_RTL_TCP_STATE_RETRYING, 3);
        assert_eq!(SDR_RTL_TCP_STATE_FAILED, 4);
    }

    #[test]
    fn translate_rtl_tcp_connection_state_disconnected() {
        use sdr_types::RtlTcpConnectionState;
        let (event, owned_cstring, _) = translate_event(&DspToUi::RtlTcpConnectionState(
            RtlTcpConnectionState::Disconnected,
        ))
        .expect("Disconnected event should translate");
        assert_eq!(event.kind, SDR_EVT_RTL_TCP_CONNECTION_STATE);
        let payload = unsafe { event.payload.rtl_tcp_connection_state };
        assert_eq!(payload.kind, SDR_RTL_TCP_STATE_DISCONNECTED);
        assert!(payload.utf8.is_null());
        assert_eq!(payload.attempt, 0);
        // `retry_in_secs` is populated from `Duration::as_secs_f64`
        // only on the Retrying arm; Disconnected leaves it at the
        // struct's zero-init. Exact-zero compare is fine here —
        // we put the 0.0 there deterministically.
        assert!(payload.retry_in_secs.abs() < f64::EPSILON);
        assert_eq!(payload.gain_count, 0);
        assert!(owned_cstring.is_none());
    }

    #[test]
    fn translate_rtl_tcp_connection_state_connected_carries_tuner() {
        use sdr_types::RtlTcpConnectionState;
        let (event, owned_cstring, _) = translate_event(&DspToUi::RtlTcpConnectionState(
            RtlTcpConnectionState::Connected {
                tuner_name: "R820T".to_string(),
                gain_count: 29,
            },
        ))
        .expect("Connected event should translate");
        let payload = unsafe { event.payload.rtl_tcp_connection_state };
        assert_eq!(payload.kind, SDR_RTL_TCP_STATE_CONNECTED);
        assert_eq!(payload.gain_count, 29);
        assert!(!payload.utf8.is_null());
        let cstr = unsafe { std::ffi::CStr::from_ptr(payload.utf8) };
        assert_eq!(cstr.to_str().unwrap(), "R820T");
        assert!(owned_cstring.is_some());
    }

    #[test]
    fn translate_rtl_tcp_connection_state_retrying_carries_attempt_and_seconds() {
        use sdr_types::RtlTcpConnectionState;
        let (event, _, _) = translate_event(&DspToUi::RtlTcpConnectionState(
            RtlTcpConnectionState::Retrying {
                attempt: 7,
                retry_in: std::time::Duration::from_millis(2_500),
            },
        ))
        .expect("Retrying event should translate");
        let payload = unsafe { event.payload.rtl_tcp_connection_state };
        assert_eq!(payload.kind, SDR_RTL_TCP_STATE_RETRYING);
        assert_eq!(payload.attempt, 7);
        assert!((payload.retry_in_secs - 2.5).abs() < 1e-9);
        assert!(payload.utf8.is_null());
    }

    #[test]
    fn translate_rtl_tcp_connection_state_failed_carries_reason() {
        use sdr_types::RtlTcpConnectionState;
        let (event, owned_cstring, _) = translate_event(&DspToUi::RtlTcpConnectionState(
            RtlTcpConnectionState::Failed {
                reason: "handshake rejected: not RTL0".to_string(),
            },
        ))
        .expect("Failed event should translate");
        let payload = unsafe { event.payload.rtl_tcp_connection_state };
        assert_eq!(payload.kind, SDR_RTL_TCP_STATE_FAILED);
        assert!(!payload.utf8.is_null());
        let cstr = unsafe { std::ffi::CStr::from_ptr(payload.utf8) };
        assert_eq!(cstr.to_str().unwrap(), "handshake rejected: not RTL0");
        assert!(owned_cstring.is_some());
    }

    #[test]
    fn network_sink_status_discriminants_match_header() {
        // Same lock-in for the tagged-payload sub-discriminants
        // and the protocol values — these are part of the ABI
        // just like the outer event kinds. Per `CodeRabbit`
        // round 1 on PR #352.
        assert_eq!(SDR_NETWORK_SINK_STATUS_INACTIVE, 0);
        assert_eq!(SDR_NETWORK_SINK_STATUS_ACTIVE, 1);
        assert_eq!(SDR_NETWORK_SINK_STATUS_ERROR, 2);
        assert_eq!(SDR_NETWORK_PROTOCOL_TCP_SERVER, 0);
        assert_eq!(SDR_NETWORK_PROTOCOL_UDP, 1);
    }

    // ------------------------------------------------------
    //  translate_event — network sink status (ABI 0.9, #247)
    //
    //  Direct Rust-side coverage of the three NetworkSinkStatus
    //  arms, including NULL vs non-NULL string cases and the
    //  `Protocol::TcpClient` → `SDR_NETWORK_PROTOCOL_TCP_SERVER`
    //  name-bridge. Locks the contract in before Swift decoding
    //  sees it. Per `CodeRabbit` round 1 on PR #352.
    // ------------------------------------------------------

    #[test]
    fn translate_network_sink_status_inactive_has_null_utf8_and_unused_protocol() {
        use sdr_core::{DspToUi, NetworkSinkStatus};
        let (event, owned_cstring, owned_vec) =
            translate_event(&DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive))
                .expect("inactive event should translate");
        assert_eq!(event.kind, SDR_EVT_NETWORK_SINK_STATUS);
        // SAFETY: kind dispatch above narrows the union field.
        let payload = unsafe { event.payload.network_sink_status };
        assert_eq!(payload.kind, SDR_NETWORK_SINK_STATUS_INACTIVE);
        assert!(payload.utf8.is_null());
        assert_eq!(payload.protocol, -1);
        assert!(owned_cstring.is_none());
        assert!(owned_vec.is_none());
    }

    #[test]
    fn translate_network_sink_status_active_tcp_maps_to_tcp_server() {
        use sdr_core::{DspToUi, NetworkSinkStatus};
        let status = NetworkSinkStatus::Active {
            endpoint: "127.0.0.1:1234".to_string(),
            protocol: sdr_types::Protocol::TcpClient,
        };
        let (event, owned_cstring, _) = translate_event(&DspToUi::NetworkSinkStatus(status))
            .expect("active event should translate");
        assert_eq!(event.kind, SDR_EVT_NETWORK_SINK_STATUS);
        let payload = unsafe { event.payload.network_sink_status };
        assert_eq!(payload.kind, SDR_NETWORK_SINK_STATUS_ACTIVE);
        assert!(!payload.utf8.is_null());
        // Rust-side `TcpClient` bridges to the clearer C name
        // `TCP_SERVER`. This is the contract the Swift side
        // relies on — lock it here.
        assert_eq!(payload.protocol, SDR_NETWORK_PROTOCOL_TCP_SERVER);

        // SAFETY: utf8 points into `owned_cstring` which is kept
        // alive by the `_` binding in the destructure above for
        // the duration of this test.
        let cstr = unsafe { std::ffi::CStr::from_ptr(payload.utf8) };
        assert_eq!(cstr.to_str().unwrap(), "127.0.0.1:1234");
        assert!(owned_cstring.is_some(), "endpoint CString must be owned");
    }

    #[test]
    fn translate_network_sink_status_active_udp_maps_to_udp_constant() {
        use sdr_core::{DspToUi, NetworkSinkStatus};
        let status = NetworkSinkStatus::Active {
            endpoint: "192.168.1.10:9000".to_string(),
            protocol: sdr_types::Protocol::Udp,
        };
        let (event, _owned_cstring, _) = translate_event(&DspToUi::NetworkSinkStatus(status))
            .expect("active event should translate");
        let payload = unsafe { event.payload.network_sink_status };
        assert_eq!(payload.kind, SDR_NETWORK_SINK_STATUS_ACTIVE);
        assert_eq!(payload.protocol, SDR_NETWORK_PROTOCOL_UDP);
    }

    #[test]
    fn translate_network_sink_status_error_carries_message_and_unused_protocol() {
        use sdr_core::{DspToUi, NetworkSinkStatus};
        let status = NetworkSinkStatus::Error {
            message: "bind failed: address already in use".to_string(),
        };
        let (event, owned_cstring, _) = translate_event(&DspToUi::NetworkSinkStatus(status))
            .expect("error event should translate");
        let payload = unsafe { event.payload.network_sink_status };
        assert_eq!(payload.kind, SDR_NETWORK_SINK_STATUS_ERROR);
        assert!(!payload.utf8.is_null());
        // Protocol is unused for the error arm per the ABI doc.
        assert_eq!(payload.protocol, -1);
        let cstr = unsafe { std::ffi::CStr::from_ptr(payload.utf8) };
        assert_eq!(
            cstr.to_str().unwrap(),
            "bind failed: address already in use"
        );
        assert!(
            owned_cstring.is_some(),
            "error message CString must be owned"
        );
    }

    #[test]
    fn translate_network_sink_status_sanitizes_interior_nul_in_endpoint() {
        // Regression guard: a stray NUL in an endpoint string
        // must not drop the event silently. The translate path
        // replaces interior NULs with `?` before `CString::new`,
        // same as the DeviceInfo and Error paths.
        use sdr_core::{DspToUi, NetworkSinkStatus};
        let status = NetworkSinkStatus::Active {
            endpoint: "host\0injected:1234".to_string(),
            protocol: sdr_types::Protocol::TcpClient,
        };
        let (event, _owned, _) = translate_event(&DspToUi::NetworkSinkStatus(status))
            .expect("sanitized active event should translate");
        let payload = unsafe { event.payload.network_sink_status };
        assert!(!payload.utf8.is_null());
        let cstr = unsafe { std::ffi::CStr::from_ptr(payload.utf8) };
        assert_eq!(cstr.to_str().unwrap(), "host?injected:1234");
    }

    #[test]
    fn sdr_event_payload_size_is_reasonable() {
        // Sanity check on the union layout. The largest payload
        // today is `SdrEventRtlTcpConnectionState` (kind i32 +
        // utf8 ptr + attempt u32 + retry_in_secs f64 + gain_count
        // u32) which lands at 40 bytes with natural alignment on
        // 64-bit targets. Budget is 48 so a future connection-
        // state extension (e.g. endpoint string alongside tuner
        // name) has a little headroom before the size check
        // tightens. Past budgets: 32 (pre-ABI-0.11 with only the
        // network sink status payload).
        let size = std::mem::size_of::<SdrEvent>();
        assert!(
            size <= 48,
            "SdrEvent size {size} exceeds 48-byte budget — may indicate an unintended union growth"
        );
    }
}
