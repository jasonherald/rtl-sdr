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
// conversions at every arm. Values must stay in lockstep with
// `SdrEventKind` in `include/sdr_core.h`.
private let kSourceStopped: Int32    = 1
private let kSampleRateChanged: Int32 = 2
private let kSignalLevel: Int32      = 3
private let kDeviceInfo: Int32       = 4
private let kGainList: Int32         = 5
private let kDisplayBandwidth: Int32 = 6
private let kError: Int32            = 7

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

        default:
            // Unknown kind — a future FFI may add variants we
            // don't know about. Drop silently; older SwiftUI
            // hosts keep working.
            return nil
        }
    }
}
