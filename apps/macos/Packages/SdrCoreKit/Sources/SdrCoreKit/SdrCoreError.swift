//
// SdrCoreError.swift — Swift-side error type mirroring the C ABI.
//
// The C side returns `int32_t` error codes (see `SdrCoreError`
// enum in `include/sdr_core.h`). SdrCoreKit's throwing wrappers
// translate non-OK codes into this Swift error type and drain
// the thread-local last-error message at the same time so callers
// get a single `Error` value carrying both the discriminant and
// the diagnostic.

import Foundation
import sdr_core_c

/// Typed error surfaced from every throwing SdrCoreKit API.
///
/// `code` is the raw C error discriminant; `message` is the
/// thread-local last-error string the FFI recorded at the
/// moment of failure (copied into a Swift `String` so it
/// survives subsequent FFI calls on the same thread, which
/// would otherwise clobber the storage).
public struct SdrCoreError: Error, Equatable, CustomStringConvertible {
    /// Known error discriminants. Numeric values match the
    /// `SdrCoreError` enum in `include/sdr_core.h` byte-for-byte;
    /// unknown values are mapped to `.unknown(Int32)` so a future
    /// FFI that grows the enum doesn't crash an older host.
    public enum Code: Equatable {
        case `internal`
        case invalidHandle
        case invalidArg
        case notRunning
        case device
        case audio
        case io
        case config
        case unknown(Int32)

        init(raw: Int32) {
            switch raw {
            case -1: self = .internal
            case -2: self = .invalidHandle
            case -3: self = .invalidArg
            case -4: self = .notRunning
            case -5: self = .device
            case -6: self = .audio
            case -7: self = .io
            case -8: self = .config
            default: self = .unknown(raw)
            }
        }

        /// Raw discriminant as it appears in the C ABI.
        public var rawValue: Int32 {
            switch self {
            case .internal: return -1
            case .invalidHandle: return -2
            case .invalidArg: return -3
            case .notRunning: return -4
            case .device: return -5
            case .audio: return -6
            case .io: return -7
            case .config: return -8
            case .unknown(let v): return v
            }
        }
    }

    public let code: Code
    public let message: String

    init(code: Code, message: String) {
        self.code = code
        self.message = message
    }

    /// Pull the current thread-local last-error message off
    /// the FFI and wrap it in an `SdrCoreError`. Used by the
    /// throwing wrappers when a C function returns a non-zero
    /// code.
    static func fromCurrentError(rawCode: Int32) -> SdrCoreError {
        let msg: String
        if let cstr = sdr_core_last_error_message() {
            msg = String(cString: cstr)
        } else {
            msg = "(no last-error message available)"
        }
        return SdrCoreError(code: Code(raw: rawCode), message: msg)
    }

    public var description: String {
        "SdrCoreError(code: \(code), message: \(message))"
    }
}

/// Internal helper: call a C function that returns an `Int32`
/// error code. Throws if the code is non-zero.
///
/// Not marked `@inlinable` because it references
/// `SdrCoreError.fromCurrentError(rawCode:)` which is internal
/// — inlining would cross the module boundary. The call site is
/// a single Int32 compare so the missing inlining is not a perf
/// concern at v1 rates.
func checkRc(_ rc: Int32) throws {
    if rc != 0 {
        throw SdrCoreError.fromCurrentError(rawCode: rc)
    }
}
