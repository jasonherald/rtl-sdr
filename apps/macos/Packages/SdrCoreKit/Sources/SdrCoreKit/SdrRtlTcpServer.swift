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
                initial_direct_sampling: initialDirectSampling.rawValue
            )
        }
    }

    /// Snapshot of server stats. Maps `SdrRtlTcpServerStats` +
    /// the two string outputs into one value for Swift callers.
    public struct Stats: Sendable, Equatable {
        public var hasClient: Bool
        public var connectedClientAddr: String
        public var uptimeSecs: Double
        public var bytesSent: UInt64
        public var buffersDropped: UInt64
        public var currentFreqHz: UInt32
        public var currentSampleRateHz: UInt32
        public var currentGainTenthsDb: Int32
        public var currentGainAuto: Bool
        public var hasCurrentGain: Bool
        public var tunerName: String
        public var gainCount: UInt32
        public var recentCommandsCount: UInt32
    }

    /// One row of the recent-commands ring — the JSON schema
    /// the FFI produces.
    public struct RecentCommand: Sendable, Equatable, Decodable {
        public let op: String
        public let secondsAgo: Double

        enum CodingKeys: String, CodingKey {
            case op
            case secondsAgo = "seconds_ago"
        }
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

    /// Capacity for the client-addr / tuner-name buffers
    /// handed to `sdr_rtltcp_server_stats`. 64 bytes comfortably
    /// fits any IPv6 + port string and any RTL-SDR tuner name.
    private static let statsStringBufferLen = 64

    /// Capture a stats snapshot. Throws `SdrCoreError` on a
    /// stopped server or other FFI error.
    ///
    /// The full FFI call runs inside the handle-box lock so a
    /// concurrent `stop()` can't free the pointer mid-call. Per
    /// `CodeRabbit` round 1 on PR #360.
    public func stats() throws -> Stats {
        let result: Stats? = try handleBox.withHandle { handle -> Stats in
            var cStats = SdrRtlTcpServerStats()
            var clientBuf = [CChar](repeating: 0, count: Self.statsStringBufferLen)
            var tunerBuf = [CChar](repeating: 0, count: Self.statsStringBufferLen)
            let rc = clientBuf.withUnsafeMutableBufferPointer { clientPtr in
                tunerBuf.withUnsafeMutableBufferPointer { tunerPtr in
                    sdr_rtltcp_server_stats(
                        handle,
                        &cStats,
                        clientPtr.baseAddress,
                        clientPtr.count,
                        tunerPtr.baseAddress,
                        tunerPtr.count
                    )
                }
            }
            try checkRc(rc)
            return Stats(
                hasClient: cStats.has_client,
                connectedClientAddr: cStringToSwiftString(clientBuf),
                uptimeSecs: cStats.uptime_secs,
                bytesSent: cStats.bytes_sent,
                buffersDropped: cStats.buffers_dropped,
                currentFreqHz: cStats.current_freq_hz,
                currentSampleRateHz: cStats.current_sample_rate_hz,
                currentGainTenthsDb: cStats.current_gain_tenths_db,
                currentGainAuto: cStats.current_gain_auto,
                hasCurrentGain: cStats.has_current_gain,
                tunerName: cStringToSwiftString(tunerBuf),
                gainCount: cStats.gain_count,
                recentCommandsCount: cStats.recent_commands_count
            )
        }
        guard let stats = result else {
            throw SdrCoreError(code: .notRunning, message: "server already stopped")
        }
        return stats
    }

    /// Fetch the recent-commands ring as decoded rows. Calls
    /// through `sdr_rtltcp_server_recent_commands_json` with a
    /// start-4 KiB buffer and retries if the server reports it
    /// needs more. Decode failures (bad UTF-8, malformed JSON)
    /// propagate as `SdrCoreError` — an empty array is a valid
    /// "no commands" result and shouldn't mask a real
    /// serialization bug. Per `CodeRabbit` round 1 on PR #360.
    public func recentCommands() throws -> [RecentCommand] {
        let result: [RecentCommand]? = try handleBox.withHandle { handle -> [RecentCommand] in
            var capacity = 4096
            while true {
                var buf = [CChar](repeating: 0, count: capacity)
                var required: Int = 0
                let rc = buf.withUnsafeMutableBufferPointer { ptr in
                    sdr_rtltcp_server_recent_commands_json(
                        handle, ptr.baseAddress, ptr.count, &required
                    )
                }
                // OK (0): buffer was big enough — parse + return.
                if rc == 0 {
                    let json = cStringToSwiftString(buf)
                    guard let data = json.data(using: .utf8) else {
                        throw SdrCoreError(
                            code: .internal,
                            message: "recent-commands JSON is not valid UTF-8"
                        )
                    }
                    do {
                        return try JSONDecoder().decode([RecentCommand].self, from: data)
                    } catch {
                        throw SdrCoreError(
                            code: .internal,
                            message: "recent-commands JSON decode failed: \(error)"
                        )
                    }
                }
                // Too-small-buffer contract: `InvalidArg` with
                // `required > buf_len`. Any other combination is
                // a real failure and propagates.
                if rc == SdrCoreError.Code.invalidArg.rawValue && required > capacity {
                    // Retry with the server's reported required
                    // size + a little slack so a race that
                    // appends a command between calls doesn't
                    // loop twice.
                    capacity = required + 128
                    continue
                }
                try checkRc(rc)
            }
        }
        guard let commands = result else {
            throw SdrCoreError(code: .notRunning, message: "server already stopped")
        }
        return commands
    }
}
