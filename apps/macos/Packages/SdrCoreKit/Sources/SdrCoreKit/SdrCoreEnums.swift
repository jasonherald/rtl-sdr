//
// SdrCoreEnums.swift — Swift mirror of the three C ABI enums.
//
// The C side passes `int32_t` discriminants for DemodMode,
// Deemphasis, and FftWindow. Swift callers work with typed
// enums; the bridging functions convert between the two.
// Numeric values are the contract (see `include/sdr_core.h` +
// `crates/sdr-ffi/src/command.rs`).

import sdr_core_c

/// Demodulation mode. Matches `SdrDemodMode` in the C header.
public enum DemodMode: Int32, Sendable, CaseIterable {
    case wfm  = 0
    case nfm  = 1
    case am   = 2
    case usb  = 3
    case lsb  = 4
    case dsb  = 5
    case cw   = 6
    case raw  = 7

    /// Human-readable label for UI pickers.
    public var label: String {
        switch self {
        case .wfm: return "WFM"
        case .nfm: return "NFM"
        case .am:  return "AM"
        case .usb: return "USB"
        case .lsb: return "LSB"
        case .dsb: return "DSB"
        case .cw:  return "CW"
        case .raw: return "RAW"
        }
    }
}

/// FM de-emphasis mode. Matches `SdrDeemphasis` in the C header.
public enum Deemphasis: Int32, Sendable, CaseIterable {
    case none = 0
    case us75 = 1
    case eu50 = 2

    public var label: String {
        switch self {
        case .none: return "None"
        case .us75: return "US 75µs"
        case .eu50: return "EU 50µs"
        }
    }
}

/// FFT window function. Matches `SdrFftWindow` in the C header.
///
/// Only three variants because that's what
/// `sdr-pipeline::iq_frontend::FftWindow` currently supports.
/// Hann/Hamming land in a future ABI minor bump if the upstream
/// enum grows.
public enum FftWindow: Int32, Sendable, CaseIterable {
    case rectangular = 0
    case blackman    = 1
    case nuttall     = 2

    public var label: String {
        switch self {
        case .rectangular: return "Rectangular"
        case .blackman:    return "Blackman"
        case .nuttall:     return "Nuttall"
        }
    }
}
