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
public enum DemodMode: Int32, Sendable, CaseIterable, Codable {
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

    /// Parse the mode label the engine uses as a string (e.g. the
    /// `demodMode` field returned by
    /// `SdrCore.searchRadioReference` — which is already mapped
    /// on the Rust side via `sdr_radioreference::mode_map`).
    /// Returns `nil` for unknown strings.
    public init?(engineLabel: String) {
        switch engineLabel.uppercased() {
        case "WFM": self = .wfm
        case "NFM": self = .nfm
        case "AM":  self = .am
        case "USB": self = .usb
        case "LSB": self = .lsb
        case "DSB": self = .dsb
        case "CW":  self = .cw
        case "RAW": self = .raw
        default: return nil
        }
    }
}

/// FM de-emphasis mode. Matches `SdrDeemphasis` in the C header.
public enum Deemphasis: Int32, Sendable, CaseIterable, Codable {
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

/// Active IQ source. Matches `SdrSourceType` in the C header.
/// `.rtlSdr` drives a locally-connected dongle over USB,
/// `.network` streams IQ from a remote server via TCP or UDP,
/// `.file` replays a WAV file on disk, `.rtlTcp` is a
/// protocol-level rtl_tcp client (separate pipeline from
/// `.network`, see issue #304). Per issues #235, #236.
public enum SourceType: Int32, Sendable, CaseIterable, Codable {
    case rtlSdr  = 0
    case network = 1
    case file    = 2
    case rtlTcp  = 3

    public var label: String {
        switch self {
        case .rtlSdr:  return "RTL-SDR (USB)"
        case .network: return "Network IQ"
        case .file:    return "File playback"
        case .rtlTcp:  return "RTL-TCP"
        }
    }
}

/// Transport for the network IQ source. Matches
/// `SdrSourceProtocol` in the C header.
///
/// Note: this is **not** the same enum as the audio-sink
/// `NetworkProtocol`. Both map to the same underlying Rust
/// `Protocol` variant, but the wire direction is opposite:
/// on the sink side `Protocol::TcpClient` means "device
/// listens as TCP server for audio clients" (hence the sink
/// label `.tcpServer`), while on the source side the same
/// variant means "device connects outbound as TCP client to
/// a remote IQ server". The C ABI uses the neutral names
/// `SDR_SOURCE_PROTOCOL_TCP` / `_UDP` on this side to avoid
/// importing the sink-side confusion. Per issue #235.
public enum NetworkSourceProtocol: Int32, Sendable, CaseIterable, Codable {
    case tcp = 0
    case udp = 1

    public var label: String {
        switch self {
        case .tcp: return "TCP"
        case .udp: return "UDP"
        }
    }
}

/// Active audio sink selection. Matches `SdrAudioSinkType` in the
/// C header. `.local` routes to a `CoreAudio` output device; `.network`
/// streams post-demod audio to the configured host:port over
/// TCP or UDP (see `NetworkProtocol`). Per issue #247.
public enum AudioSinkType: Int32, Sendable, CaseIterable, Codable {
    case local   = 0
    case network = 1

    public var label: String {
        switch self {
        case .local:   return "Local device"
        case .network: return "Network stream"
        }
    }
}

/// Network stream protocol for the network audio sink. Matches
/// `SdrNetworkProtocol` in the C header.
///
/// The `.tcpServer` name reflects the actual wire role: the
/// device listens on the configured port and streams to clients
/// that connect. (The Rust side spells the same thing as
/// `Protocol::TcpClient` for historical SDR++ compatibility;
/// the C ABI uses the clearer name.)
public enum NetworkProtocol: Int32, Sendable, CaseIterable, Codable {
    case tcpServer = 0
    case udp       = 1

    public var label: String {
        switch self {
        case .tcpServer: return "TCP server"
        case .udp:       return "UDP"
        }
    }
}

/// rtl_tcp client connection-state snapshot surfaced via the
/// `rtlTcpConnectionState` engine event. Mirrors
/// `sdr_types::RtlTcpConnectionState` on the Rust side. Per
/// issue #325.
public enum RtlTcpConnectionState: Sendable, Equatable {
    /// No connection attempt has begun yet. Initial state on
    /// source construction, before `start()`.
    case disconnected

    /// First TCP connect is in flight.
    case connecting

    /// Handshake succeeded and data is streaming. Carries tuner
    /// metadata (name, gain-step count) for a status row.
    case connected(tunerName: String, gainCount: UInt32)

    /// Transport-level error; the source is in its reconnect-
    /// with-backoff loop. `attempt` is the monotonically
    /// increasing retry counter; `retryInSecs` is how long
    /// until the next attempt (seconds). Plain `Double` —
    /// avoids dragging `Foundation` into this enum file.
    case retrying(attempt: UInt32, retryInSecs: Double)

    /// Terminal protocol-level failure (non-RTL0 handshake,
    /// etc.). The UI should show "needs user action."
    case failed(reason: String)
}

/// Network sink status surfaced via the `networkSinkStatus`
/// engine event. Mirrors
/// `sdr_core::sink_slot::NetworkSinkStatus` on the Rust side.
///
/// - `.inactive` — the network sink is not currently the
///   active audio output (either never selected, replaced by
///   another sink, or the engine stopped).
/// - `.active(endpoint:protocol:)` — the network sink started
///   successfully. `endpoint` is the host:port the engine is
///   bound to (TCP) or sending to (UDP); hosts typically show
///   it in a Settings status row.
/// - `.error(message:)` — a startup or write failure took the
///   network sink offline. `message` is a human-readable
///   description suitable for a toast or status line.
public enum NetworkSinkStatus: Sendable, Equatable {
    case inactive
    case active(endpoint: String, protocol: NetworkProtocol)
    case error(message: String)
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
