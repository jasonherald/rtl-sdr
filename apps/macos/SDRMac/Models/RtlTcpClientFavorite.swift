//
// RtlTcpClientFavorite.swift — persisted rtl_tcp client records.
//
// Two types: `RtlTcpClientFavorite` is the user-pinned favorite
// entry (starred servers that survive across sessions even when
// offline), and `RtlTcpClientLastConnected` is the most-recent
// successful connection snapshot used to repopulate the manual-
// entry fields on next launch.
//
// Both are `Codable` so `UserDefaults` can store them as JSON
// strings — matches the Linux GTK `source_panel.rs` schema so a
// user who runs both frontends sees the same favorites. The
// persistence lives in `CoreModel`; these types are pure data.
//
// Mirrors `FavoriteEntry` + `LastConnectedServer` in
// `crates/sdr-ui/src/sidebar/source_panel.rs`. Field names use
// Swift camelCase; JSON `CodingKeys` map back to the Linux
// snake_case on-disk form so both frontends round-trip.

import Foundation
import SdrCoreKit

/// Pinned favorite record. Key identity is `hostname:port`;
/// display metadata (`nickname`, `tunerName`, `gainCount`,
/// `lastSeenUnix`) is optional so a freshly-starred server with
/// no cached announce still round-trips cleanly.
struct RtlTcpClientFavorite: Codable, Sendable, Equatable, Identifiable {
    /// Stable identity — `"\(host):\(port)"`. Two entries with
    /// the same key refer to the same endpoint; the favorites
    /// list is deduped on this field.
    let key: String

    /// User-facing label. Preferred source: mDNS TXT `nickname`.
    /// Fallback: the DNS-SD `instance_name`. For a legacy entry
    /// that only has a bare `key` persisted, this is the same
    /// string as `key` until the server re-announces.
    var nickname: String

    /// Tuner model from the last-seen `DiscoveredServer` TXT
    /// record, e.g. `"R820T"`. `nil` for offline-only entries
    /// that haven't been seen since they were pinned.
    var tunerName: String?

    /// Gain-step count from the same TXT record. `nil` same as
    /// `tunerName`.
    var gainCount: UInt32?

    /// Unix timestamp (seconds since epoch) of the most recent
    /// `.announced` event for this `key`. `nil` when we haven't
    /// seen the server this session.
    var lastSeenUnix: UInt64?

    /// `Identifiable` — `key` is already unique by definition.
    var id: String { key }

    /// Parse `host:port` out of the `key` for use in a connect
    /// call. Returns `nil` if the key is malformed (missing colon,
    /// non-numeric port, empty host).
    var parsedEndpoint: (host: String, port: UInt16)? {
        guard let colon = key.lastIndex(of: ":") else { return nil }
        let host = String(key[..<colon])
        let portStr = key[key.index(after: colon)...]
        guard !host.isEmpty, let port = UInt16(portStr), port > 0 else {
            return nil
        }
        return (host, port)
    }

    /// Build a fresh favorite from a live mDNS discovery. The
    /// `key` is derived from `hostname:port`; display metadata
    /// comes straight from the announce record.
    init(from server: SdrRtlTcpBrowser.DiscoveredServer) {
        self.key = "\(server.hostname):\(server.port)"
        self.nickname = server.nickname.isEmpty ? server.instanceName : server.nickname
        self.tunerName = server.tuner.isEmpty ? nil : server.tuner
        self.gainCount = server.gains == 0 ? nil : server.gains
        self.lastSeenUnix = UInt64(Date().timeIntervalSince1970)
    }

    /// Direct init for manual-entry / programmatic paths.
    init(
        key: String,
        nickname: String,
        tunerName: String? = nil,
        gainCount: UInt32? = nil,
        lastSeenUnix: UInt64? = nil
    ) {
        self.key = key
        self.nickname = nickname
        self.tunerName = tunerName
        self.gainCount = gainCount
        self.lastSeenUnix = lastSeenUnix
    }

    /// JSON key mapping to the Linux GTK on-disk schema
    /// (`snake_case`), so both frontends share the same file
    /// format when a user runs them on the same machine.
    enum CodingKeys: String, CodingKey {
        case key
        case nickname
        case tunerName = "tuner_name"
        case gainCount = "gain_count"
        case lastSeenUnix = "last_seen_unix"
    }
}

/// Most-recent successfully-connected server. Persisted so next
/// launch can repopulate the manual-entry host/port/nickname
/// fields without waiting for mDNS rediscovery.
struct RtlTcpClientLastConnected: Codable, Sendable, Equatable {
    /// Hostname or IP literal the Connect button dialed. Either
    /// a resolved address or an mDNS hostname (`shack-pi.local.`),
    /// whichever the discovery layer yielded.
    var host: String
    /// TCP port.
    var port: UInt16
    /// User-facing nickname — normally the mDNS TXT nickname, or
    /// the `instance_name` when no nickname was published.
    var nickname: String
}
