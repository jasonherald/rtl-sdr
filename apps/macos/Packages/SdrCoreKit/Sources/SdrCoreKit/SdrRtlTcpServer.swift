//
// SdrRtlTcpServer.swift — Swift wrapper around the `SdrRtlTcpServer*`
// C handle (issue #325). Lets a macOS host share a locally-connected
// RTL-SDR dongle over the network so other SDR clients can tune it.
//
// Deliberately separate from `SdrCore` — the server has its own
// opaque handle on the FFI side because the engine and the server
// have independent lifecycles. They share the RTL-SDR dongle,
// though, so UI is responsible for mutual exclusivity.

import Foundation
@preconcurrency import sdr_core_c

/// Swift handle to a running rtl_tcp server. Create with
/// `SdrRtlTcpServer(config:)`; the server starts immediately
/// on init (bind + open dongle + spawn accept thread). The
/// `deinit` calls `sdr_rtltcp_server_stop` so hosts don't need
/// an explicit teardown — letting the value go out of scope is
/// enough.
public final class SdrRtlTcpServer: @unchecked Sendable {
    /// Opaque C handle. `nil` after a successful manual `stop()`.
    /// Held under a lock because `stats()` / `recentCommands()`
    /// can be called from any thread — same pattern as the
    /// `SdrCore` handle.
    private let handleBox: HandleBox

    /// Private box so the class itself can be `@unchecked Sendable`
    /// without exposing the inner pointer's lock to callers.
    private final class HandleBox: @unchecked Sendable {
        private let lock = NSLock()
        private var handle: OpaquePointer?

        init(_ handle: OpaquePointer) {
            self.handle = handle
        }

        /// Run `body` with the live handle if it hasn't been
        /// stopped yet; returns `nil` after stop. Holds the lock
        /// for the **entire** duration of `body` so a concurrent
        /// `stop()` can't take the handle out from under an
        /// in-flight FFI call — doing so would pass a freed
        /// pointer into the C layer. Per `CodeRabbit` round 1
        /// on PR #360.
        func withHandle<T>(_ body: (OpaquePointer) throws -> T) rethrows -> T? {
            lock.lock()
            defer { lock.unlock() }
            guard let handle else { return nil }
            return try body(handle)
        }

        /// Take the handle out of the box, leaving `nil` behind.
        /// Returns the old handle (or `nil` if already taken).
        func take() -> OpaquePointer? {
            lock.lock()
            defer { lock.unlock() }
            let h = handle
            handle = nil
            return h
        }
    }

    /// Negotiated stream-codec mask (ABI 0.19, issue #400). Wire
    /// bytes match `CodecMask::to_wire` on the Rust side.
    ///
    /// - `.noneOnly` (0x01) — vanilla rtl_tcp behaviour, every
    ///   client streams uncompressed IQ. The default; LZ4-aware
    ///   clients fall back to this when the server doesn't
    ///   advertise the codecs TXT key.
    /// - `.noneAndLz4` (0x03) — the server is willing to negotiate
    ///   LZ4 compression with clients that ask. Clients that
    ///   don't speak the RTLX extension still get uncompressed
    ///   streams; the mask is a *capability*, not a requirement.
    public enum Compression: UInt8, Sendable, CaseIterable, Codable {
        case noneOnly   = 0x01
        case noneAndLz4 = 0x03

        public var label: String {
            switch self {
            case .noneOnly:   return "None (legacy-compatible)"
            case .noneAndLz4: return "None + LZ4 (compression)"
            }
        }
    }

    /// Configuration passed to `sdr_rtltcp_server_start`. Field
    /// defaults match the C API's "crate default" semantics:
    /// port `0` → 1234, buffer capacity `0` → crate default,
    /// `initialGainTenthsDb: 0` → auto-gain.
    public struct Config: Sendable, Equatable {
        public enum BindAddress: Int32, Sendable, CaseIterable, Codable {
            case loopback       = 0
            case allInterfaces  = 1

            public var label: String {
                switch self {
                case .loopback:      return "Loopback only"
                case .allInterfaces: return "All interfaces"
                }
            }
        }

        public var bindAddress: BindAddress
        public var port: UInt16
        public var deviceIndex: UInt32
        public var bufferCapacity: UInt32
        public var initialFreqHz: UInt32
        public var initialSampleRateHz: UInt32
        /// 0 means "auto" (tuner-AGC enabled).
        public var initialGainTenthsDb: Int32
        public var initialPpm: Int32
        public var initialBiasTee: Bool
        /// Direct-sampling mode. Shares the same enum as the
        /// client-side `SdrCore.setDirectSampling(_:)` so the
        /// two paths can't drift and a caller can't construct
        /// an invalid raw mode. Per `CodeRabbit` round 1 on
        /// PR #360.
        public var initialDirectSampling: SdrCore.DirectSamplingMode

        /// Stream-codec mask the server is willing to negotiate.
        /// `.noneOnly` is the safe default — equivalent to the
        /// pre-ABI-0.19 behaviour where the server didn't
        /// advertise codecs at all and every client got
        /// uncompressed IQ. Issue #417.
        public var compression: Compression

        public init(
            bindAddress: BindAddress = .loopback,
            port: UInt16 = 1234,
            deviceIndex: UInt32 = 0,
            bufferCapacity: UInt32 = 0,
            initialFreqHz: UInt32 = 100_000_000,
            initialSampleRateHz: UInt32 = 2_048_000,
            initialGainTenthsDb: Int32 = 0,
            initialPpm: Int32 = 0,
            initialBiasTee: Bool = false,
            initialDirectSampling: SdrCore.DirectSamplingMode = .off,
            compression: Compression = .noneOnly
        ) {
            self.bindAddress = bindAddress
            self.port = port
            self.deviceIndex = deviceIndex
            self.bufferCapacity = bufferCapacity
            self.initialFreqHz = initialFreqHz
            self.initialSampleRateHz = initialSampleRateHz
            self.initialGainTenthsDb = initialGainTenthsDb
            self.initialPpm = initialPpm
            self.initialBiasTee = initialBiasTee
            self.initialDirectSampling = initialDirectSampling
            self.compression = compression
        }

        fileprivate func toC() -> SdrRtlTcpServerConfig {
            // ABI 0.16 (#392 / `listener_cap`) and 0.17 (#394 /
            // `auth_key` + `auth_key_len`) both ship in the
            // header but the Mac side hasn't surfaced UI for
            // them yet. Pass documented zero-init defaults that
            // preserve pre-ABI-0.16 behaviour:
            //   - listener_cap = 0     → crate default (10)
            //   - auth_key = NULL,
            //     auth_key_len = 0     → auth disabled (LAN trust)
            //
            // ABI 0.19 (#400) compression IS surfaced as of #417:
            // the picker writes `Config.compression`, which we
            // forward here with `has_compression = true` so the
            // server applies the mask explicitly rather than
            // falling back to the omit-key default.
            SdrRtlTcpServerConfig(
                bind_address: bindAddress.rawValue,
                port: port,
                device_index: deviceIndex,
                buffer_capacity: bufferCapacity,
                initial_freq_hz: initialFreqHz,
                initial_sample_rate_hz: initialSampleRateHz,
                initial_gain_tenths_db: initialGainTenthsDb,
                initial_ppm: initialPpm,
                initial_bias_tee: initialBiasTee,
                initial_direct_sampling: initialDirectSampling.rawValue,
                listener_cap: 0,
                auth_key: nil,
                auth_key_len: 0,
                has_compression: true,
                compression: compression.rawValue
            )
        }
    }

    /// Snapshot of server-wide stats. Maps the post-#391
    /// (multi-client) `SdrRtlTcpServerStats` shape — aggregates
    /// only — plus the tuner-name string output into one value.
    ///
    /// **Per-client state lives in `ClientInfo` rows.** Pre-#391
    /// this struct carried `hasClient` / `connectedClientAddr` /
    /// `currentFreqHz` etc. for the single connected client. The
    /// engine became multi-client in #391; that detail moved
    /// into `SdrRtlTcpClientInfo` rows fetched through
    /// `clientList()` (issue #401, this PR).
    public struct Stats: Sendable, Equatable {
        /// Number of clients connected at the moment of the
        /// snapshot. Membership may change between this read and
        /// a follow-up `clientList()` call.
        public var connectedCount: UInt32
        /// Cumulative bytes fanned out across all clients over
        /// the server's lifetime. Monotonic.
        public var totalBytesSent: UInt64
        /// Cumulative buffer drops across all clients. Monotonic.
        public var totalBuffersDropped: UInt64
        /// Cumulative count of clients accepted over the
        /// server's lifetime. Persists across disconnects.
        public var lifetimeAccepted: UInt64
        /// Tuner family name reported by the dongle. Non-empty
        /// for the entire server lifetime once `start` succeeded.
        public var tunerName: String
        /// Number of discrete gain steps the tuner advertises.
        public var gainCount: UInt32
    }

    /// Role the server granted a connected client. Mirrors the
    /// Rust `Role` wire byte (`sdr_server_rtltcp::extension`).
    /// Vanilla rtl_tcp clients always land as `.control` — the
    /// server only admits them when the control slot is free
    /// (#392). Listener clients receive the IQ stream but the
    /// server drops any commands they send.
    public enum ClientRole: UInt8, Sendable, CaseIterable {
        case control  = 0
        case listener = 1

        public var label: String {
            switch self {
            case .control:  return "Controller"
            case .listener: return "Listener"
            }
        }
    }

    /// Per-client state snapshot. Returned in oldest-first order
    /// by `clientList()`. The `id` is stable across snapshots
    /// for cross-poll correlation; everything else is a
    /// snapshot-time read.
    public struct ClientInfo: Sendable, Equatable, Identifiable {
        /// Stable, monotonic identifier assigned at accept time.
        /// Never reused.
        public let id: UInt64
        /// Peer socket address as `"ip:port"`. IPv6 literals
        /// arrive in bracketed form (`[fe80::…]:1234`).
        public var peerAddress: String
        /// Seconds since this client's handshake completed.
        public var uptimeSecs: Double
        /// Negotiated stream codec for this session. Pre-#400
        /// servers always report `.noneOnly` (the legacy wire
        /// format).
        public var codec: Compression
        /// Bytes written to this client's TCP socket since
        /// accept. Post-compression — LZ4 sessions report the
        /// compressed-on-wire byte count, NOT the underlying IQ
        /// volume. Per-client only; the server's
        /// `Stats.totalBytesSent` is a separate lifetime
        /// aggregate that includes contributions from
        /// disconnected clients.
        public var bytesSent: UInt64
        /// Buffer drops on this client's broadcaster channel
        /// (server saw `TrySendError::Full`).
        public var buffersDropped: UInt64
        /// Most recent client-issued centre frequency, Hz. `nil`
        /// before the client has sent `SetCenterFreq` for the
        /// first time.
        public var currentFreqHz: UInt32?
        /// Most recent client-issued sample rate, Hz. Same `nil`
        /// semantics as `currentFreqHz`.
        public var currentSampleRateHz: UInt32?
        /// Most recent tuner-gain request in 0.1 dB. `nil` until
        /// the client has issued at least one `SetTunerGain`.
        public var currentGainTenthsDb: Int32?
        /// `true` when the client's last gain-mode request was
        /// auto; `false` when manual; `nil` until they've issued
        /// a `SetGainMode`. Tracked separately from the gain
        /// value because `SetGainMode(auto)` and `SetTunerGain`
        /// can fire independently.
        public var currentGainAuto: Bool?
        /// Number of entries currently in this client's
        /// recent-commands ring (an entry count, not a byte
        /// count). Useful for "N commands since connect" hints.
        public var recentCommandsCount: UInt32
        /// Wire byte of the client's most recently dispatched
        /// command — matches the `rtl_tcp.c` opcodes
        /// (SetCenterFreq=0x01, SetSampleRate=0x02, …,
        /// SetBiasTee=0x0e). `nil` until they've issued any
        /// command this session.
        public var lastCommandOp: UInt8?
        /// Seconds elapsed between the client's most recent
        /// command and this snapshot. Snapshot-time only — NOT
        /// monotonic across polls; a fresh command resets it.
        /// All entries in a single `clientList()` call reference
        /// one snapshot clock so cross-client comparisons within
        /// a snapshot are consistent.
        public var lastCommandAgeSecs: Double?
        /// Role the server granted this client.
        public var role: ClientRole
    }

    // ----------------------------------------------------------
    //  Lifecycle
    // ----------------------------------------------------------

    public init(config: Config) throws {
        var rawHandle: OpaquePointer? = nil
        var cfg = config.toC()
        let rc = withUnsafePointer(to: &cfg) { cfgPtr in
            sdr_rtltcp_server_start(cfgPtr, &rawHandle)
        }
        try checkRc(rc)
        guard let h = rawHandle else {
            throw SdrCoreError(
                code: .internal,
                message: "sdr_rtltcp_server_start returned OK but null handle"
            )
        }
        self.handleBox = HandleBox(h)
    }

    deinit {
        if let h = handleBox.take() {
            sdr_rtltcp_server_stop(h)
        }
    }

    /// Explicitly stop the server before it goes out of scope.
    /// Safe to call more than once — subsequent calls are no-ops.
    /// Returns `true` if this call actually stopped the server,
    /// `false` if it was already stopped.
    @discardableResult
    public func stop() -> Bool {
        guard let h = handleBox.take() else { return false }
        sdr_rtltcp_server_stop(h)
        return true
    }

    /// Returns `true` once the accept thread has exited (either
    /// because `stop()` ran, or because the server hit an
    /// unrecoverable error and cleaned up on its own).
    public var hasStopped: Bool {
        handleBox.withHandle { sdr_rtltcp_server_has_stopped($0) } ?? true
    }

    // ----------------------------------------------------------
    //  Snapshots
    // ----------------------------------------------------------

    /// Capacity for the tuner-name buffer handed to
    /// `sdr_rtltcp_server_stats`. 64 bytes comfortably fits any
    /// RTL-SDR tuner family name. The pre-#391 single-client
    /// peer-address output buffer is gone — peer addresses live
    /// on per-client `SdrRtlTcpClientInfo` rows now (issue #401).
    private static let statsTunerNameBufferLen = 64

    /// Initial capacity for the per-poll client-list buffer. The
    /// server's `listener_cap` defaults to 10 (#392) and we keep
    /// the listener slot count small even at the high end, so 16
    /// entries is plenty for the steady-state read. The retry
    /// path inside `clientList()` doubles when the actual count
    /// exceeds capacity.
    private static let clientListInitialCapacity = 16

    /// Length of the `peer_addr` C-array tail in
    /// `SdrRtlTcpClientInfo`. Mirrors `SDR_RTLTCP_CLIENT_PEER_LEN`
    /// in `include/sdr_core.h`. The C header value is a `#define`
    /// macro and Swift's importer doesn't surface those as
    /// constants — re-stating the literal here is the clean way
    /// to use it. Anchored against the header in
    /// `SdrCoreTests.testClientInfoPeerLengthMatchesHeader`.
    private static let clientPeerAddressLen = 64

    /// Capture a server-wide stats snapshot. Throws
    /// `SdrCoreError` on a stopped server or other FFI error.
    ///
    /// The full FFI call runs inside the handle-box lock so a
    /// concurrent `stop()` can't free the pointer mid-call —
    /// same pattern as the engine's own handle wrappers.
    ///
    /// Per-client state (peer address, current tuning, recent
    /// commands) is fetched via `clientList()` — `connectedCount`
    /// here is the size hint to pass that call.
    public func stats() throws -> Stats {
        let result: Stats? = try handleBox.withHandle { handle -> Stats in
            var cStats = SdrRtlTcpServerStats()
            var tunerBuf = [CChar](repeating: 0, count: Self.statsTunerNameBufferLen)
            let rc = tunerBuf.withUnsafeMutableBufferPointer { tunerPtr in
                sdr_rtltcp_server_stats(
                    handle,
                    &cStats,
                    tunerPtr.baseAddress,
                    tunerPtr.count
                )
            }
            try checkRc(rc)
            return Stats(
                connectedCount: cStats.connected_count,
                totalBytesSent: cStats.total_bytes_sent,
                totalBuffersDropped: cStats.total_buffers_dropped,
                lifetimeAccepted: cStats.lifetime_accepted,
                tunerName: cStringToSwiftString(tunerBuf),
                gainCount: cStats.gain_count
            )
        }
        guard let stats = result else {
            throw SdrCoreError(code: .notRunning, message: "server already stopped")
        }
        return stats
    }

    /// Snapshot every connected client's state. Returned in
    /// oldest-first order; identifiers are stable across
    /// snapshots so a UI can correlate rows (e.g. preserve
    /// expand/collapse state per row) without re-keying on
    /// changing peer addresses.
    ///
    /// Throws `SdrCoreError` on a stopped server.
    ///
    /// The FFI surface uses a "size-then-fill" pattern: we
    /// allocate `clientListInitialCapacity` slots, call
    /// `sdr_rtltcp_server_client_list`, and check the returned
    /// `out_count` against capacity. If `out_count > capacity`
    /// the snapshot grew between the call and our buffer sizing
    /// (a new client connected mid-call), so we re-allocate and
    /// retry once. Two iterations is plenty in practice — the
    /// per-poll cadence is 1 Hz; the chance of two consecutive
    /// connects landing inside one snapshot is vanishingly
    /// small. Issue #401.
    public func clientList() throws -> [ClientInfo] {
        let result: [ClientInfo]? = try handleBox.withHandle { handle -> [ClientInfo] in
            var capacity = Self.clientListInitialCapacity
            // Single retry on buffer-too-small. Loop is bounded
            // to avoid pathological "every snapshot adds a
            // client" scenarios — even then capacity doubles
            // each pass, so the bound here is generous.
            for _ in 0..<3 {
                var buf = [SdrRtlTcpClientInfo](
                    repeating: SdrRtlTcpClientInfo(),
                    count: capacity
                )
                var outCount: size_t = 0
                let rc = buf.withUnsafeMutableBufferPointer { ptr in
                    sdr_rtltcp_server_client_list(
                        handle,
                        ptr.baseAddress,
                        size_t(capacity),
                        &outCount
                    )
                }
                try checkRc(rc)
                if Int(outCount) <= capacity {
                    return (0..<Int(outCount)).map { Self.swiftClient(from: buf[$0]) }
                }
                // Outgrew the buffer — the engine wrote
                // `capacity` entries (the first `capacity`
                // clients) and the actual count is in `outCount`.
                // Retry with capacity bumped to fit.
                capacity = Int(outCount)
            }
            // Should never hit this — capacity grows
            // monotonically and the engine returns the actual
            // count on each call. Treat as a transient error so
            // callers can retry on the next poll tick.
            throw SdrCoreError(
                code: .internal,
                message: "clientList capacity grew unboundedly across retries"
            )
        }
        guard let list = result else {
            throw SdrCoreError(code: .notRunning, message: "server already stopped")
        }
        return list
    }

    /// Decode a single C `SdrRtlTcpClientInfo` into the Swift
    /// `ClientInfo` shape. Pulled out as a static helper so the
    /// retry loop in `clientList()` stays readable.
    private static func swiftClient(from c: SdrRtlTcpClientInfo) -> ClientInfo {
        // Peer address comes through as a C array of CChar via
        // the swift bridge. Read as a tuple, build a fixed-size
        // buffer the standard way.
        var peerCopy = c
        let peerString = withUnsafePointer(to: &peerCopy.peer_addr) { tuplePtr -> String in
            tuplePtr.withMemoryRebound(
                to: CChar.self,
                capacity: clientPeerAddressLen
            ) { cstrPtr in
                String(cString: cstrPtr)
            }
        }

        let codec = Compression(rawValue: c.codec) ?? .noneOnly
        let role = ClientRole(rawValue: c.role) ?? .control

        // The `current_*` fields are valid even before the
        // client has issued a command — but they're 0 by the C
        // contract. Map 0-when-never-set to nil so the UI can
        // distinguish "0 Hz" from "no command yet".
        let freq = c.current_freq_hz != 0 ? c.current_freq_hz : nil
        let rate = c.current_sample_rate_hz != 0 ? c.current_sample_rate_hz : nil
        let gainValue = c.has_current_gain_value ? c.current_gain_tenths_db : nil
        let gainAuto  = c.has_current_gain_mode  ? c.current_gain_auto       : nil

        let lastOp  = c.has_last_command ? c.last_command_op       : nil
        let lastAge = c.has_last_command ? c.last_command_age_secs : nil

        return ClientInfo(
            id: c.id,
            peerAddress: peerString,
            uptimeSecs: c.uptime_secs,
            codec: codec,
            bytesSent: c.bytes_sent,
            buffersDropped: c.buffers_dropped,
            currentFreqHz: freq,
            currentSampleRateHz: rate,
            currentGainTenthsDb: gainValue,
            currentGainAuto: gainAuto,
            recentCommandsCount: c.recent_commands_count,
            lastCommandOp: lastOp,
            lastCommandAgeSecs: lastAge,
            role: role
        )
    }
}
