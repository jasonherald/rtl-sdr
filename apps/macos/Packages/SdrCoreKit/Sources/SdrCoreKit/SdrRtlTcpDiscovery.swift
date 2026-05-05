//
// SdrRtlTcpDiscovery.swift — Swift wrappers for the mDNS
// advertiser and browser surfaced by `sdr_rtltcp_advertiser_*`
// and `sdr_rtltcp_browser_*` (issue #325, ABI 0.11).
//
// Both classes follow the `SdrRtlTcpServer` pattern:
// `@unchecked Sendable` with a locked handle slot that `deinit`
// releases automatically so hosts don't need explicit teardown.
// The browser additionally owns a retained `CallbackBox` that
// the C trampoline dereferences via `takeUnretainedValue`.

import Foundation
@preconcurrency import sdr_core_c

/// Publishes a single `_rtl_tcp._tcp.local.` mDNS
/// advertisement. Construct with `SdrRtlTcpAdvertiser(options:)`;
/// the registration is live on the LAN within seconds. Let the
/// value drop (or call `stop()`) to unregister.
public final class SdrRtlTcpAdvertiser: @unchecked Sendable {
    /// Options mirrored from `SdrRtlTcpAdvertiseOptions`.
    /// Required fields default to empty; `SdrCoreError` fires
    /// if the caller leaves `instanceName` / `tuner` / `version`
    /// blank.
    public struct Options: Sendable, Equatable {
        public var port: UInt16
        public var instanceName: String
        /// Empty = auto-derive from the local system hostname.
        public var hostname: String
        public var tuner: String
        public var version: String
        public var gains: UInt32
        public var nickname: String
        public var txbufBytes: UInt64?

        /// ABI 0.19 (#400) — codec mask the server is willing to
        /// negotiate. `nil` omits the TXT key entirely (pre-0.19
        /// behaviour); LZ4-aware clients that don't see the key
        /// fall back to the legacy uncompressed format.
        public var compression: SdrRtlTcpServer.Compression?

        /// ABI 0.19 (#400) — whether the server requires
        /// pre-shared-key auth. `nil` omits the TXT key
        /// (servers without auth advertise nothing); `true`
        /// publishes the bit so clients can stage a credential
        /// prompt before connecting. Issue #417.
        public var authRequired: Bool?

        public init(
            port: UInt16,
            instanceName: String,
            hostname: String = "",
            tuner: String,
            version: String,
            gains: UInt32,
            nickname: String = "",
            txbufBytes: UInt64? = nil,
            compression: SdrRtlTcpServer.Compression? = nil,
            authRequired: Bool? = nil
        ) {
            self.port = port
            self.instanceName = instanceName
            self.hostname = hostname
            self.tuner = tuner
            self.version = version
            self.gains = gains
            self.nickname = nickname
            self.txbufBytes = txbufBytes
            self.compression = compression
            self.authRequired = authRequired
        }
    }

    private let handleBox: HandleBox

    private final class HandleBox: @unchecked Sendable {
        private let lock = NSLock()
        private var handle: OpaquePointer?

        init(_ handle: OpaquePointer) {
            self.handle = handle
        }

        func take() -> OpaquePointer? {
            lock.lock()
            defer { lock.unlock() }
            let h = handle
            handle = nil
            return h
        }
    }

    public init(options: Options) throws {
        // Build the C struct with CString-backed pointers that
        // live for the duration of the `sdr_rtltcp_advertiser_start`
        // call — the Rust side copies everything out before
        // returning.
        var rawHandle: OpaquePointer? = nil
        let rc = options.instanceName.withCString { instancePtr in
            options.hostname.withCString { hostPtr in
                options.tuner.withCString { tunerPtr in
                    options.version.withCString { versionPtr in
                        options.nickname.withCString { nicknamePtr -> Int32 in
                            // ABI 0.19 (#400) opt-in TXT fields:
                            //   - `has_codecs` / `codecs` —
                            //     server's negotiable codec mask
                            //   - `has_auth_required` /
                            //     `auth_required` — whether the
                            //     server demands a pre-shared
                            //     key (#394).
                            // Both default to "key omitted from
                            // the TXT record" (pre-0.19 wire
                            // form) when the matching Swift
                            // option is `nil`. Issue #417.
                            var opts = SdrRtlTcpAdvertiseOptions(
                                port: options.port,
                                instance_name: instancePtr,
                                hostname: hostPtr,
                                tuner: tunerPtr,
                                version: versionPtr,
                                gains: options.gains,
                                nickname: nicknamePtr,
                                has_txbuf: options.txbufBytes != nil,
                                txbuf: options.txbufBytes ?? 0,
                                has_codecs: options.compression != nil,
                                codecs: options.compression?.rawValue ?? 0,
                                has_auth_required: options.authRequired != nil,
                                auth_required: options.authRequired ?? false
                            )
                            return withUnsafePointer(to: &opts) { optsPtr in
                                sdr_rtltcp_advertiser_start(optsPtr, &rawHandle)
                            }
                        }
                    }
                }
            }
        }
        try checkRc(rc)
        guard let h = rawHandle else {
            throw SdrCoreError(
                code: .internal,
                message: "sdr_rtltcp_advertiser_start returned OK but null handle"
            )
        }
        self.handleBox = HandleBox(h)
    }

    deinit {
        if let h = handleBox.take() {
            sdr_rtltcp_advertiser_stop(h)
        }
    }

    /// Explicitly stop the advertisement. Safe to call more
    /// than once — subsequent calls are no-ops.
    public func stop() {
        if let h = handleBox.take() {
            sdr_rtltcp_advertiser_stop(h)
        }
    }
}

/// Watches the LAN for `_rtl_tcp._tcp.local.` advertisements.
/// Construct with `SdrRtlTcpBrowser(onEvent:)`; the callback
/// fires on a dedicated dispatcher thread, NOT the main
/// actor — hosts must marshal across themselves.
public final class SdrRtlTcpBrowser: @unchecked Sendable {
    /// Mirrors `SdrRtlTcpDiscoveredServer` with owned Swift
    /// types so the value can outlive the C callback frame.
    public struct DiscoveredServer: Sendable, Equatable {
        public var instanceName: String
        public var hostname: String
        public var port: UInt16
        public var addressIpv4: String
        public var addressIpv6: String
        public var tuner: String
        public var version: String
        public var gains: UInt32
        public var nickname: String
        public var txbufBytes: UInt64?
        public var lastSeenSecsAgo: Double

        /// ABI 0.19 (#400) — server's advertised codec mask.
        /// `nil` when the server didn't publish the TXT key
        /// (pre-0.19 servers, or servers explicitly omitting
        /// it). LZ4-aware clients use this to gate "negotiate
        /// LZ4" before the handshake. Issue #417.
        public var compression: SdrRtlTcpServer.Compression?

        /// ABI 0.19 (#400) — whether the server requires
        /// pre-shared-key auth. `nil` when the server didn't
        /// publish the TXT key. Clients use this to stage a
        /// credential prompt before connecting. Issue #417.
        public var authRequired: Bool?
    }

    public enum Event: Sendable, Equatable {
        case announced(DiscoveredServer)
        case withdrawn(instanceName: String)
    }

    private let handleBox: HandleBox
    /// Retained box passed as `user_data` to the C callback.
    /// Lives for the lifetime of the browser so the callback
    /// can recover it safely via `takeUnretainedValue`.
    private let callbackBox: CallbackBox

    private final class HandleBox: @unchecked Sendable {
        private let lock = NSLock()
        private var handle: OpaquePointer?

        init(_ handle: OpaquePointer) {
            self.handle = handle
        }

        func take() -> OpaquePointer? {
            lock.lock()
            defer { lock.unlock() }
            let h = handle
            handle = nil
            return h
        }
    }

    private final class CallbackBox {
        let handler: @Sendable (Event) -> Void
        init(handler: @escaping @Sendable (Event) -> Void) {
            self.handler = handler
        }
    }

    public init(onEvent: @escaping @Sendable (Event) -> Void) throws {
        let box = CallbackBox(handler: onEvent)
        self.callbackBox = box
        var rawHandle: OpaquePointer? = nil
        let boxPtr = Unmanaged.passUnretained(box).toOpaque()
        let rc = sdr_rtltcp_browser_start(SdrRtlTcpBrowser.trampoline, boxPtr, &rawHandle)
        if rc != 0 {
            // Capture the error message BEFORE any other FFI
            // call overwrites the thread-local.
            throw SdrCoreError.fromCurrentError(rawCode: rc)
        }
        guard let h = rawHandle else {
            throw SdrCoreError(
                code: .internal,
                message: "sdr_rtltcp_browser_start returned OK but null handle"
            )
        }
        self.handleBox = HandleBox(h)
    }

    deinit {
        if let h = handleBox.take() {
            sdr_rtltcp_browser_stop(h)
        }
    }

    /// Explicitly stop the browser. Joins the dispatcher
    /// thread before returning; safe to call more than once.
    public func stop() {
        if let h = handleBox.take() {
            sdr_rtltcp_browser_stop(h)
        }
    }

    // ----------------------------------------------------------
    //  C trampoline
    // ----------------------------------------------------------

    private static let trampoline:
        @convention(c) (
            UnsafePointer<SdrRtlTcpDiscoveryEvent>?,
            UnsafeMutableRawPointer?
        ) -> Void = { eventPtr, userData in
            guard let eventPtr, let userData else { return }
            let box = Unmanaged<CallbackBox>.fromOpaque(userData).takeUnretainedValue()
            let raw = eventPtr.pointee
            switch raw.kind {
            case Int32(SDR_RTLTCP_DISCOVERY_ANNOUNCED.rawValue):
                let announced = raw.announced
                let ds = DiscoveredServer(
                    instanceName: stringFromPtr(announced.instance_name),
                    hostname: stringFromPtr(announced.hostname),
                    port: announced.port,
                    addressIpv4: stringFromPtr(announced.address_ipv4),
                    addressIpv6: stringFromPtr(announced.address_ipv6),
                    tuner: stringFromPtr(announced.tuner),
                    version: stringFromPtr(announced.version),
                    gains: announced.gains,
                    nickname: stringFromPtr(announced.nickname),
                    txbufBytes: announced.has_txbuf ? announced.txbuf : nil,
                    lastSeenSecsAgo: announced.last_seen_secs_ago,
                    compression: announced.has_codecs
                        ? SdrRtlTcpServer.Compression(rawValue: announced.codecs)
                        : nil,
                    authRequired: announced.has_auth_required ? announced.auth_required : nil
                )
                box.handler(.announced(ds))
            case Int32(SDR_RTLTCP_DISCOVERY_WITHDRAWN.rawValue):
                let name = stringFromPtr(raw.withdrawn_instance_name)
                box.handler(.withdrawn(instanceName: name))
            default:
                // Unknown sub-kind — drop silently like the main
                // event dispatcher does.
                return
            }
        }

    /// Decode a NUL-terminated C string borrowed for the
    /// callback's duration into an owned Swift `String`.
    /// Null pointer degrades to an empty string so the
    /// callbacks don't have to branch on it.
    private static func stringFromPtr(_ ptr: UnsafePointer<CChar>?) -> String {
        guard let ptr else { return "" }
        return String(cString: ptr)
    }
}
