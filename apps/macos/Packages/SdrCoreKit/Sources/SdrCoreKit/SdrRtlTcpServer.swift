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
            initialDirectSampling: SdrCore.DirectSamplingMode = .off
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
        }

        fileprivate func toC() -> SdrRtlTcpServerConfig {
            // ABI 0.16 (#392 / `listener_cap`), 0.17 (#394 /
            // `auth_key` + `auth_key_len`), and 0.19 (#400 /
            // `has_compression` + `compression`) appended fields
            // at the tail. The Mac side hasn't surfaced auth /
            // listener-cap / compression UI yet, so we pass the
            // documented zero-init defaults that preserve pre-
            // ABI-0.16 behaviour:
            //   - listener_cap = 0     → crate default (10)
            //   - auth_key = NULL,
            //     auth_key_len = 0     → auth disabled (LAN trust)
            //   - has_compression = false → CodecMask::NONE_ONLY
            // When the Mac UI grows controls for any of these,
            // promote the matching field to a typed Swift
            // property and forward it from `Config`.
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
                has_compression: false,
                compression: 0
            )
        }
    }

    /// Snapshot of server-wide stats. Maps the post-#391
    /// (multi-client) `SdrRtlTcpServerStats` shape — aggregates
    /// only — plus the tuner-name string output into one value.
    ///
    /// **Per-client state lives elsewhere now.** Pre-#391 this
    /// struct carried `hasClient` / `connectedClientAddr` /
    /// `currentFreqHz` etc. for the single connected client.
    /// The engine became multi-client in #391; that detail
    /// moved into `SdrRtlTcpClientInfo` rows fetched through
    /// `sdr_rtltcp_server_client_list`. The Mac SwiftUI surface
    /// for that list (per-client rows in the panel + the
    /// `clientList()` Swift wrapper) lands in #496 — until
    /// then the panel renders aggregates only.
    public struct Stats: Sendable, Equatable {
        /// Number of clients connected at the moment of the
        /// snapshot. Membership may change between this read
        /// and a follow-up `clientList()` call (#496).
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
    /// on per-client `SdrRtlTcpClientInfo` rows now (#496).
    private static let statsTunerNameBufferLen = 64

    /// Capture a server-wide stats snapshot. Throws
    /// `SdrCoreError` on a stopped server or other FFI error.
    ///
    /// The full FFI call runs inside the handle-box lock so a
    /// concurrent `stop()` can't free the pointer mid-call —
    /// same pattern as the engine's own handle wrappers.
    ///
    /// Per-client state (peer address, current tuning, recent
    /// commands) is no longer in this snapshot — fetch it via
    /// the upcoming `clientList()` / per-client recent-commands
    /// surface (#496).
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
}
