//
// SdrCoreEvent.swift — Swift-side tagged enum that mirrors the C
// `SdrEvent` tagged union, with borrowed-pointer payloads copied
// into owned Swift types so the event can outlive the callback
// call that produced it.
//
// The C callback fires on the FFI dispatcher thread with a
// `const SdrEvent*` borrow that's only valid for the callback's
// duration. Here we translate that into a Swift `enum` variant
// and — crucially — copy every borrowed String / Array into
// owned Swift storage before returning. The resulting value is
// `Sendable` and can be `yield`ed into an `AsyncStream` for the
// main-thread consumer to read whenever it gets around to it.

import Foundation
import sdr_core_c

// The C header declares `SdrEventKind` as a `typedef enum` whose
// Clang-imported Swift representation has a `UInt32` raw value
// (Clang maps unsigned C enums to UInt32 by default). The
// `SdrEvent.kind` field is declared as `int32_t` for portability,
// so `Int32` and `UInt32` don't match directly in a switch.
//
// These `Int32` mirrors of the kind discriminants let the switch
// below match the `kind` field without per-case `Int32(...)`
// conversions at every arm.
//
// **The values are derived from the Clang-imported `SdrEventKind`
// enum rather than hard-coded literals** so any future renumber
// in `include/sdr_core.h` flows through automatically — the Swift
// side can't drift from the C header silently. CodeRabbit caught
// the hard-coded form on PR #256 round 1.
private let kSourceStopped         = Int32(SDR_EVT_SOURCE_STOPPED.rawValue)
private let kSampleRateChanged     = Int32(SDR_EVT_SAMPLE_RATE_CHANGED.rawValue)
private let kSignalLevel           = Int32(SDR_EVT_SIGNAL_LEVEL.rawValue)
private let kDeviceInfo            = Int32(SDR_EVT_DEVICE_INFO.rawValue)
private let kGainList              = Int32(SDR_EVT_GAIN_LIST.rawValue)
private let kDisplayBandwidth      = Int32(SDR_EVT_DISPLAY_BANDWIDTH.rawValue)
private let kError                 = Int32(SDR_EVT_ERROR.rawValue)
private let kAudioRecordingStarted = Int32(SDR_EVT_AUDIO_RECORDING_STARTED.rawValue)
private let kAudioRecordingStopped = Int32(SDR_EVT_AUDIO_RECORDING_STOPPED.rawValue)
private let kIqRecordingStarted    = Int32(SDR_EVT_IQ_RECORDING_STARTED.rawValue)
private let kIqRecordingStopped    = Int32(SDR_EVT_IQ_RECORDING_STOPPED.rawValue)
private let kNetworkSinkStatus     = Int32(SDR_EVT_NETWORK_SINK_STATUS.rawValue)

/// High-level event from the engine.
///
/// Every variant that carries borrowed C data is translated here
/// into an owned Swift type (`String`, `[Double]`), so the value
/// is safe to pass across actor boundaries, hand to `AsyncStream`,
/// or retain for later inspection.
public enum SdrCoreEvent: Sendable, Equatable {
    /// The engine's active source stopped (device unplugged,
    /// end of file, remote stream closed, …).
    case sourceStopped

    /// The effective (post-decimation) sample rate changed.
    /// Fires after `setSampleRate` / `setDecimation` take effect.
    case sampleRateChanged(sampleRateHz: Double)

    /// Updated signal-level (SNR) measurement in dB.
    case signalLevel(db: Float)

    /// Device identification string (e.g., tuner name, USB
    /// descriptor). Fires once when the source is first opened.
    case deviceInfo(String)

    /// Tuner-reported list of discrete gain values in dB. Fires
    /// once when the source is first opened.
    case gainList([Double])

    /// Raw (pre-decimation) sample rate, used by the spectrum
    /// display to label the frequency axis. Fires when the
    /// source rate changes.
    case displayBandwidth(sampleRateHz: Double)

    /// A non-fatal error occurred in the pipeline.
    case error(String)

    /// Audio recording to WAV has started. The associated path is
    /// the file the engine opened for writing.
    case audioRecordingStarted(path: String)

    /// Audio recording to WAV has stopped. Also fires on engine
    /// shutdown while a recording is active, so hosts can clear
    /// their "recording" UI without tracking teardown separately.
    case audioRecordingStopped

    /// IQ recording to WAV has started. The associated path is
    /// the file the engine opened for writing. Unlike audio
    /// recording, the WAV is written at the current tuner sample
    /// rate (two-channel I/Q), not a fixed 48 kHz, so file size
    /// scales with the selected source rate.
    case iqRecordingStarted(path: String)

    /// IQ recording to WAV has stopped. Also fires on engine
    /// shutdown while a recording is active.
    case iqRecordingStopped

    /// Network audio sink lifecycle / error update. Fires when
    /// the network sink becomes the active audio output
    /// (`.active`), when it is replaced by another sink or the
    /// engine stops (`.inactive`), or when a startup / write
    /// failure takes it offline (`.error`). Per issue #247.
    case networkSinkStatus(NetworkSinkStatus)

    /// Translate a C `SdrEvent` into a Swift value.
    ///
    /// Safety: the caller must ensure `event` is non-null and
    /// points at a valid `SdrEvent` for the duration of this
    /// call. Borrowed pointers inside the event (device info
    /// string, gain list array, error message string) are
    /// immediately copied into owned Swift storage, so the
    /// returned value has no lifetime dependency on `event`.
    static func fromC(_ event: UnsafePointer<SdrEvent>) -> SdrCoreEvent? {
        let kind = event.pointee.kind
        let payload = event.pointee.payload

        switch kind {
        case kSourceStopped:
            return .sourceStopped

        case kSampleRateChanged:
            return .sampleRateChanged(sampleRateHz: payload.sample_rate_hz)

        case kSignalLevel:
            return .signalLevel(db: payload.signal_level_db)

        case kDisplayBandwidth:
            return .displayBandwidth(sampleRateHz: payload.display_bandwidth_hz)

        case kDeviceInfo:
            let cstr = payload.device_info.utf8
            guard let cstr else { return .deviceInfo("") }
            return .deviceInfo(String(cString: cstr))

        case kGainList:
            let list = payload.gain_list
            if list.len == 0 || list.values == nil {
                return .gainList([])
            }
            // Copy the borrowed array into an owned Swift [Double].
            let buf = UnsafeBufferPointer(start: list.values, count: list.len)
            return .gainList(Array(buf))

        case kError:
            let cstr = payload.error.utf8
            guard let cstr else { return .error("") }
            return .error(String(cString: cstr))

        case kAudioRecordingStarted:
            // Drop the event on either a null path pointer OR a
            // zero-length C string — both produce an empty Swift
            // String, which would flip CoreModel's
            // `audioRecordingPath` into a bogus non-nil
            // "recording" state with no file the UI can
            // meaningfully display or reveal. The subsequent
            // `.audioRecordingStopped` will still clear state
            // correctly. Per CodeRabbit rounds 1 + 2 on PR #344.
            guard let cstr = payload.audio_recording.path_utf8 else { return nil }
            let path = String(cString: cstr)
            guard !path.isEmpty else { return nil }
            return .audioRecordingStarted(path: path)

        case kAudioRecordingStopped:
            return .audioRecordingStopped

        case kIqRecordingStarted:
            // Same null/empty guard as audio. An empty path would
            // flip CoreModel's `iqRecordingPath` into a bogus
            // "recording" state with no file to display / reveal.
            guard let cstr = payload.iq_recording.path_utf8 else { return nil }
            let path = String(cString: cstr)
            guard !path.isEmpty else { return nil }
            return .iqRecordingStarted(path: path)

        case kIqRecordingStopped:
            return .iqRecordingStopped

        case kNetworkSinkStatus:
            let status = payload.network_sink_status
            switch status.kind {
            case Int32(SDR_NETWORK_SINK_STATUS_INACTIVE.rawValue):
                return .networkSinkStatus(.inactive)
            case Int32(SDR_NETWORK_SINK_STATUS_ACTIVE.rawValue):
                // `active` always carries a non-null endpoint
                // string from the Rust side. Drop the event if
                // the string is somehow missing rather than
                // fabricate an empty endpoint that the UI would
                // render as "Streaming to :" — clearer to skip.
                guard let cstr = status.utf8 else { return nil }
                let endpoint = String(cString: cstr)
                let proto = NetworkProtocol(rawValue: status.protocol) ?? .tcpServer
                return .networkSinkStatus(.active(endpoint: endpoint, protocol: proto))
            case Int32(SDR_NETWORK_SINK_STATUS_ERROR.rawValue):
                let message: String
                if let cstr = status.utf8 {
                    message = String(cString: cstr)
                } else {
                    message = ""
                }
                return .networkSinkStatus(.error(message: message))
            default:
                // Unknown sub-kind — future ABI extension. Drop
                // silently, same policy as the outer `default`.
                return nil
            }

        default:
            // Unknown kind — a future FFI may add variants we
            // don't know about. Drop silently; older SwiftUI
            // hosts keep working.
            return nil
        }
    }
}
