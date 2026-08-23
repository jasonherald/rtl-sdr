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
pub const SDR_EVT_SCANNER_STATE_CHANGED: i32 = 14;
pub const SDR_EVT_SCANNER_ACTIVE_CHANNEL_CHANGED: i32 = 15;
pub const SDR_EVT_SCANNER_EMPTY_ROTATION: i32 = 16;
pub const SDR_EVT_SCANNER_MUTEX_STOPPED: i32 = 17;
pub const SDR_EVT_VFO_OFFSET_CHANGED: i32 = 18;
pub const SDR_EVT_BANDWIDTH_CHANGED: i32 = 19;

// ============================================================
//  Scanner phase discriminants — must match `SdrScannerState`
//  in `include/sdr_core.h`. Numeric values mirror the variant
//  order of `sdr_scanner::ScannerState`. Never reorder or
//  renumber.
// ============================================================

pub const SDR_SCANNER_STATE_IDLE: i32 = 0;
pub const SDR_SCANNER_STATE_RETUNING: i32 = 1;
pub const SDR_SCANNER_STATE_DWELLING: i32 = 2;
pub const SDR_SCANNER_STATE_LISTENING: i32 = 3;
pub const SDR_SCANNER_STATE_HANGING: i32 = 4;

// ============================================================
//  Scanner mutex-stop reasons — must match
//  `SdrScannerMutexReason` in `include/sdr_core.h`. Mirrors
//  the variant order of `sdr_core::messages::ScannerMutexReason`.
//  Never reorder or renumber.
// ============================================================

pub const SDR_SCANNER_MUTEX_RECORDING_STOPPED_FOR_SCANNER: i32 = 0;
/// Reserved ABI slot. Discriminant 1 was previously
/// `TRANSCRIPTION_STOPPED_FOR_SCANNER`; removed when the scanner ↔
/// transcription mutex was deleted (PR #558 / issue #517 — the two
/// are designed to coexist). Kept as a named reserved constant so
/// the C ABI in `include/sdr_core.h` keeps its numeric layout and
/// future discriminants don't accidentally reuse the slot. Per
/// `CodeRabbit` round 1 on PR #558.
pub const SDR_SCANNER_MUTEX_RESERVED_1: i32 = 1;
pub const SDR_SCANNER_MUTEX_SCANNER_STOPPED_FOR_RECORDING: i32 = 2;
/// Reserved ABI slot. Discriminant 3 was previously
/// `SCANNER_STOPPED_FOR_TRANSCRIPTION`; removed alongside slot 1
/// above.
pub const SDR_SCANNER_MUTEX_RESERVED_3: i32 = 3;

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
/// Server has an existing Control client and denied this
/// attempt with `Status::ControllerBusy`. Host UIs should
/// offer the user "Take control" / "Connect as Listener"
/// actions rather than retry silently. No-auto-retry —
/// the client does not attempt another connect while this
/// state is active. ABI 0.18, per #396.
pub const SDR_RTL_TCP_STATE_CONTROLLER_BUSY: i32 = 5;
/// Server requires a pre-shared key (#394) and the client
/// didn't send one. Host UIs should prompt the user for a
/// key and reconnect. No-auto-retry. ABI 0.18, per #396.
pub const SDR_RTL_TCP_STATE_AUTH_REQUIRED: i32 = 6;
/// Server required a key and the client's attempt was
/// rejected (`Status::AuthFailed`). Host UIs should re-
/// prompt for a key. No-auto-retry. ABI 0.18, per #396.
pub const SDR_RTL_TCP_STATE_AUTH_FAILED: i32 = 7;

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

/// Payload for `SDR_EVT_SCANNER_STATE_CHANGED`. `state` is one
/// of the `SDR_SCANNER_STATE_*` discriminants. Per #447 (ABI 0.20).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrEventScannerStateChanged {
    pub state: i32,
}

/// Payload for `SDR_EVT_SCANNER_ACTIVE_CHANNEL_CHANGED`. The
/// scanner emits this on every channel latch (squelch open on
/// the new channel) AND on every release back to Idle. When the
/// scanner is idle (no latched channel), `name_utf8` is NULL and
/// `frequency_hz` is 0 — the host clears its "active channel"
/// readout. When latched, `name_utf8` is the bookmark name the
/// host originally projected via `UpdateScannerChannels` and
/// `frequency_hz` is the matching `ChannelKey::frequency_hz`.
///
/// `name_utf8` is a borrowed pointer into dispatcher-owned
/// storage; valid only for the duration of the callback. Per
/// #447 (ABI 0.20).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrEventScannerActiveChannelChanged {
    pub name_utf8: *const c_char,
    pub frequency_hz: u64,
}

/// Payload for `SDR_EVT_SCANNER_MUTEX_STOPPED`. `reason` is one
/// of the `SDR_SCANNER_MUTEX_*` discriminants — describes which
/// side of the scanner ↔ recording / transcription mutex fired.
/// Per #447 (ABI 0.20).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrEventScannerMutexStopped {
    pub reason: i32,
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
/// | `SDR_EVT_NETWORK_SINK_STATUS`     | `network_sink_status.{kind,utf8,protocol}` |
/// | `SDR_EVT_RTL_TCP_CONNECTION_STATE`| `rtl_tcp_connection_state.{kind,utf8,attempt,retry_in_secs,gain_count}` |
/// | `SDR_EVT_SCANNER_STATE_CHANGED`   | `scanner_state.state`        |
/// | `SDR_EVT_SCANNER_ACTIVE_CHANNEL_CHANGED` | `scanner_active_channel.{name_utf8,frequency_hz}` |
/// | `SDR_EVT_SCANNER_EMPTY_ROTATION`  | none                         |
/// | `SDR_EVT_SCANNER_MUTEX_STOPPED`   | `scanner_mutex_stopped.reason` |
/// | `SDR_EVT_VFO_OFFSET_CHANGED`      | `vfo_offset_hz`              |
/// | `SDR_EVT_BANDWIDTH_CHANGED`       | `bandwidth_hz`               |
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
    pub scanner_state: SdrEventScannerStateChanged,
    pub scanner_active_channel: SdrEventScannerActiveChannelChanged,
    pub scanner_mutex_stopped: SdrEventScannerMutexStopped,
    /// Payload for `SDR_EVT_VFO_OFFSET_CHANGED` (#488 / ABI
    /// 0.23). Engine echoes this whenever the VFO offset
    /// changes — host commands AND engine-internal resets
    /// (e.g., scanner retune to a new channel) — so the
    /// observable model can stay in sync without polling.
    pub vfo_offset_hz: f64,
    /// Payload for `SDR_EVT_BANDWIDTH_CHANGED` (#488 / ABI
    /// 0.24). Engine echoes this whenever the channel
    /// bandwidth changes — host commands AND engine-internal
    /// changes (scanner retune, future per-mode auto-pick).
    /// Symmetric with `vfo_offset_hz` above; same observable-
    /// stays-in-sync rationale.
    pub bandwidth_hz: f64,
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
        // Scanner Phase 1 UI events (`ScannerActiveChannelChanged`,
        // `ScannerStateChanged`, `ScannerEmptyRotation`,
        // `ScannerMutexStopped`) landed at the FFI boundary in ABI
        // 0.20 (#447) — see the dedicated arms above.
        // `VfoOffsetChanged` landed in ABI 0.23 (#488) and
        // `BandwidthChanged` followed in ABI 0.24 (#616 /
        // CodeRabbit) — also dedicated arms above.
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
                    ..
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
                // Role-denial terminal states (#396). Payload
                // shape matches Disconnected/Connecting: no
                // message string, zero counters. The kind
                // discriminant is enough for the host to pick
                // the right toast copy ("Controller busy" /
                // "Server requires a key" / "Key rejected").
                RtlTcpConnectionState::ControllerBusy => {
                    (SDR_RTL_TCP_STATE_CONTROLLER_BUSY, None, 0, 0.0, 0)
                }
                RtlTcpConnectionState::AuthRequired => {
                    (SDR_RTL_TCP_STATE_AUTH_REQUIRED, None, 0, 0.0, 0)
                }
                RtlTcpConnectionState::AuthFailed => {
                    (SDR_RTL_TCP_STATE_AUTH_FAILED, None, 0, 0.0, 0)
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

        DspToUi::ScannerStateChanged(state) => {
            use sdr_scanner::ScannerState;
            let state_int = match state {
                ScannerState::Idle => SDR_SCANNER_STATE_IDLE,
                ScannerState::Retuning => SDR_SCANNER_STATE_RETUNING,
                ScannerState::Dwelling => SDR_SCANNER_STATE_DWELLING,
                ScannerState::Listening => SDR_SCANNER_STATE_LISTENING,
                ScannerState::Hanging => SDR_SCANNER_STATE_HANGING,
            };
            SdrEvent {
                kind: SDR_EVT_SCANNER_STATE_CHANGED,
                payload: SdrEventPayload {
                    scanner_state: SdrEventScannerStateChanged { state: state_int },
                },
            }
        }

        DspToUi::ScannerActiveChannelChanged { key, .. } => {
            // `mode_override` is intentionally NOT exposed at the
            // FFI boundary — the host already chose the demod
            // mode when it projected the bookmark into a
            // `ScannerChannel`, and the scanner's retune already
            // applied it. The UI just needs the channel identity
            // for its "active channel" readout.
            let (name_ptr, frequency_hz, name_cstr) = match key {
                Some(k) => {
                    let sanitized = k.name.replace('\0', "?");
                    let Ok(cstr) = CString::new(sanitized) else {
                        // Unreachable: replace stripped NULs.
                        return None;
                    };
                    let ptr = cstr.as_ptr();
                    (ptr, k.frequency_hz, Some(cstr))
                }
                None => (std::ptr::null(), 0_u64, None),
            };
            owned_cstring = name_cstr;
            SdrEvent {
                kind: SDR_EVT_SCANNER_ACTIVE_CHANNEL_CHANGED,
                payload: SdrEventPayload {
                    scanner_active_channel: SdrEventScannerActiveChannelChanged {
                        name_utf8: name_ptr,
                        frequency_hz,
                    },
                },
            }
        }

        DspToUi::ScannerEmptyRotation => SdrEvent {
            kind: SDR_EVT_SCANNER_EMPTY_ROTATION,
            payload: SdrEventPayload { _placeholder: 0 },
        },

        DspToUi::ScannerMutexStopped(reason) => {
            use sdr_core::messages::ScannerMutexReason;
            let reason_int = match reason {
                ScannerMutexReason::RecordingStoppedForScanner => {
                    SDR_SCANNER_MUTEX_RECORDING_STOPPED_FOR_SCANNER
                }
                ScannerMutexReason::ScannerStoppedForRecording => {
                    SDR_SCANNER_MUTEX_SCANNER_STOPPED_FOR_RECORDING
                }
            };
            SdrEvent {
                kind: SDR_EVT_SCANNER_MUTEX_STOPPED,
                payload: SdrEventPayload {
                    scanner_mutex_stopped: SdrEventScannerMutexStopped { reason: reason_int },
                },
            }
        }

        DspToUi::VfoOffsetChanged(hz) => SdrEvent {
            // Engine-side echo of every VFO-offset change
            // (host commands AND engine-internal resets like
            // scanner retune). Host updates its observable
            // `vfoOffsetHz` from this event so the spectrum
            // overlay stays in sync without polling. Per #488
            // (ABI 0.23).
            kind: SDR_EVT_VFO_OFFSET_CHANGED,
            payload: SdrEventPayload {
                vfo_offset_hz: *hz,
            },
        },

        DspToUi::BandwidthChanged(hz) => SdrEvent {
            // Symmetric with the VFO-offset echo above —
            // host commands AND engine-internal bandwidth
            // changes (scanner retune to a channel with a
            // different bandwidth, future per-mode auto-
            // pick). Host updates its observable
            // `bandwidthHz` so the bandwidth-row reset icon's
            // enabled state and the floating Reset-VFO
            // button's visibility track engine truth without
            // polling. Per `CodeRabbit` round 1 on PR #616
            // (ABI 0.24).
            kind: SDR_EVT_BANDWIDTH_CHANGED,
            payload: SdrEventPayload {
                bandwidth_hz: *hz,
            },
        },

        DspToUi::FftData(_)
        | DspToUi::DemodModeChanged(_)
        | DspToUi::CtcssSustainedChanged(_)
        | DspToUi::VoiceSquelchOpenChanged(_)
        // APT lines (#482) aren't surfaced through the FFI layer
        // yet — the macOS frontend will gain a native APT viewer
        // through its own ticket. Drop them here so the Linux UI
        // side can keep emitting without a Mac-side build break.
        | DspToUi::AptLine(_)
        // ACARS variants (epic #474) aren't surfaced through the FFI
        // layer yet — sub-project 3 is Linux-only; macOS will get
        // its own ticket. Drop here for the same reason as AptLine.
        | DspToUi::AcarsMessage(_)
        | DspToUi::AcarsChannelStats(_)
        | DspToUi::AcarsEnabledChanged(_)
        // Output-error toast (issue #578) — Linux-only writer path;
        // macOS ticket will handle when FFI layer gets ACARS output.
        | DspToUi::AcarsOutputError { .. }
        // SSTV variants (epic #472) — Linux-only for V1; macOS will
        // get its own ticket when the FFI layer gains an SSTV viewer.
        | DspToUi::SstvVisDetected { .. }
        | DspToUi::SstvLineDecoded(_)
        | DspToUi::SstvImageComplete { .. } => return None,
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
mod tests;
