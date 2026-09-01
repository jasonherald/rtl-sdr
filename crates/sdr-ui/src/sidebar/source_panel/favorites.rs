//! `rtl_tcp` favorites + last-connected persistence for the Source
//! panel (issue #819): the [`FavoriteEntry`] / [`LastConnectedServer`]
//! records, their config load/save round-trips, and the
//! `last_seen_unix` clock helper. Split out of `source_panel.rs`
//! per the file-size pass.

use std::sync::Arc;

use sdr_config::ConfigManager;

use super::{KEY_RTL_TCP_CLIENT_FAVORITES, KEY_RTL_TCP_CLIENT_LAST_CONNECTED};

/// Snapshot of a previously-connected `rtl_tcp` server. Serialized
/// into the `rtl_tcp_client_last_connected` config entry so the
/// next app launch can repopulate the hostname / port / nickname
/// fields without waiting for mDNS to rediscover.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LastConnectedServer {
    /// Hostname or IP literal the Connect button dialed — either
    /// a resolved address (`192.168.1.5`) or an mDNS hostname
    /// (`shack-pi.local.`), whichever the discovery layer yielded.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// User-facing nickname — normally the mDNS TXT nickname, or
    /// the `instance_name` when no nickname was published.
    pub nickname: String,
}

/// Rich favorite-entry record. Persisted in the
/// `rtl_tcp_client_favorites` config array as a JSON object per
/// entry. Keeps the stable `key` (hostname:port — see
/// `window.rs::favorite_key`) alongside display metadata the
/// favorites slide-out shows even when the server is offline: the
/// nickname the user last saw, the tuner type and gain-step count
/// from the last mDNS announcement, and a "last seen" wall-clock
/// stamp.
///
/// Optional fields default to `None` so a freshly-starred server
/// with no cached metadata still round-trips correctly, and so
/// legacy bare-string entries (PR #335 schema) can be read back
/// without drift — see `load_favorites`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FavoriteEntry {
    /// Stable identity: `format!("{}:{}", hostname, port)`. Same
    /// value produced by `window.rs::favorite_key` on the live
    /// `DiscoveredServer`. Load-bearing — two entries with the
    /// same key refer to the same endpoint.
    pub key: String,
    /// User-facing label. Preferred source: the mDNS TXT
    /// `nickname`. Fallback: the DNS-SD `instance_name`. For a
    /// migrated legacy entry this is the same string as `key`
    /// until the server re-announces and the user re-stars (or
    /// next-session metadata refresh lands).
    pub nickname: String,
    /// Tuner model from the last-seen `DiscoveredServer` TXT
    /// record, e.g. `"R820T"`. `None` for offline-only entries
    /// we haven't seen since the schema upgrade.
    pub tuner_name: Option<String>,
    /// Gain-step count from the same TXT record. `None` same as
    /// `tuner_name`.
    pub gain_count: Option<u32>,
    /// Unix timestamp (seconds) of the most recent
    /// `ServerAnnounced` event for this `key`. `None` when we
    /// haven't seen the server this session.
    pub last_seen_unix: Option<u64>,
    /// Last-used role against this server: `"control"` or
    /// `"listen"`. Stored as a string (via
    /// `serde(rename_all = "snake_case")` on the enum) rather
    /// than the raw enum so the JSON is human-readable and a
    /// future enum-variant rename doesn't silently break
    /// deserialization. `None` until the user explicitly picks
    /// a role for this server; the connect path defaults to
    /// Control when `None`. Per issue #396.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_role: Option<FavoriteRole>,
    /// Whether the most recent mDNS TXT for this server
    /// advertised `auth_required=true`. Pre-populated from
    /// discovery events so the UI can reveal the Server key
    /// field BEFORE the user clicks Connect (saves a round
    /// trip through the `AuthRequired` error path). `None`
    /// means "unknown" — either we've never seen a TXT, or the
    /// record didn't carry the field (older server, non-sdr-rs
    /// server). Per issue #396.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_required: Option<bool>,
}

/// Favorite-entry serialized form of a client's preferred role
/// for a given server. `snake_case` so the JSON surface reads
/// as `"control"` / `"listen"` — easier to hand-edit and
/// more forgiving across future enum changes. Per #396.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FavoriteRole {
    Control,
    Listen,
}

impl FavoriteRole {
    /// Translate to the wire-level `sdr_server_rtltcp::extension::Role`
    /// the client hello will carry. Kept as a separate crate
    /// boundary so `FavoriteEntry` doesn't force a dep on
    /// `sdr-server-rtltcp` at every call site that reads a
    /// serialized favorite.
    pub fn as_wire_role(self) -> sdr_server_rtltcp::extension::Role {
        match self {
            Self::Control => sdr_server_rtltcp::extension::Role::Control,
            Self::Listen => sdr_server_rtltcp::extension::Role::Listen,
        }
    }

    /// Inverse: build a `FavoriteRole` from a wire-level
    /// `Role`. Used when persisting a newly-chosen role back to
    /// the favorite entry after a successful connect.
    pub fn from_wire_role(role: sdr_server_rtltcp::extension::Role) -> Self {
        match role {
            sdr_server_rtltcp::extension::Role::Control => Self::Control,
            sdr_server_rtltcp::extension::Role::Listen => Self::Listen,
        }
    }
}

/// Load the persisted favorites list. Returns an empty `Vec` on
/// first launch / absent / corrupt config — safe to call
/// unconditionally.
///
/// **Backward compatibility:** accepts two on-disk shapes:
///
/// 1. **Current (PR #315):** `Vec<FavoriteEntry>` — array of JSON
///    objects, each decoded via `serde_json::from_value`. Objects
///    that fail to deserialize are skipped AND logged at
///    `tracing::warn!` with the offending entry index and the
///    serde error, so schema drift is diagnosable in bug reports
///    instead of silently eating favorites.
/// 2. **Legacy (PR #335):** `Vec<String>` — array of bare
///    `hostname:port` keys. Upgraded in-place by constructing a
///    `FavoriteEntry` with `nickname = key` and every optional
///    metadata field set to `None`. Those blanks fill in on the
///    next re-announce + re-star, so no user-visible data is lost
///    — just a one-session degraded display until the server is
///    seen again.
pub fn load_favorites(config: &Arc<ConfigManager>) -> Vec<FavoriteEntry> {
    config.read(|v| {
        let Some(arr) = v
            .get(KEY_RTL_TCP_CLIENT_FAVORITES)
            .and_then(serde_json::Value::as_array)
        else {
            return Vec::new();
        };
        arr.iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                if let Some(s) = entry.as_str() {
                    // Legacy bare-string entry. Build a stub
                    // FavoriteEntry so the slide-out still has
                    // something to render while the user waits
                    // for the server to re-announce. Role and
                    // auth-required default to `None` — the
                    // connect path treats both as "unknown"
                    // (role defaults to Control, auth_required
                    // is decided by the server on first
                    // connect).
                    Some(FavoriteEntry {
                        key: s.to_string(),
                        nickname: s.to_string(),
                        tuner_name: None,
                        gain_count: None,
                        last_seen_unix: None,
                        requested_role: None,
                        auth_required: None,
                    })
                } else {
                    // Corrupt object entry — hand-edited JSON or a
                    // shape we don't recognize. Skip the entry so
                    // the rest of the list still loads, but log so
                    // a "my favorite disappeared" bug report
                    // surfaces the parse failure.
                    match serde_json::from_value::<FavoriteEntry>(entry.clone()) {
                        Ok(fav) => Some(fav),
                        Err(err) => {
                            tracing::warn!(
                                entry_index = idx,
                                error = %err,
                                "skipping corrupt rtl_tcp favorite entry",
                            );
                            None
                        }
                    }
                }
            })
            .collect()
    })
}

/// Persist the full favorites list as a JSON array of
/// `FavoriteEntry` objects. Overwrites the config entry — callers
/// pass the current UI state of pinned entries.
pub fn save_favorites(config: &Arc<ConfigManager>, favorites: &[FavoriteEntry]) {
    config.write(|v| {
        v[KEY_RTL_TCP_CLIENT_FAVORITES] =
            serde_json::to_value(favorites).unwrap_or(serde_json::Value::Null);
    });
}

/// Current wall-clock time as Unix seconds. Helper for building
/// `FavoriteEntry::last_seen_unix` on star-toggle / re-announce.
/// Saturating-zero on clock skew (pre-epoch system time) so
/// we never return a garbage very-large value from a
/// `Duration::as_secs` on an error path.
pub fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Load the last-connected server snapshot, if any was recorded.
/// Returns `None` on first launch or when the stored blob fails
/// to deserialize (schema drift, hand-edited config, etc.).
pub fn load_last_connected(config: &Arc<ConfigManager>) -> Option<LastConnectedServer> {
    config.read(|v| {
        v.get(KEY_RTL_TCP_CLIENT_LAST_CONNECTED)
            .and_then(|entry| serde_json::from_value(entry.clone()).ok())
    })
}

/// Persist a `LastConnectedServer` snapshot. Called from the
/// discovery-row Connect handler and from any manual-server
/// connect path once that UI exists.
pub fn save_last_connected(config: &Arc<ConfigManager>, server: &LastConnectedServer) {
    config.write(|v| {
        // Serialize via serde_json::to_value so we don't re-embed
        // JSON-encoded text inside a JSON string (the common
        // round-trip mistake here).
        v[KEY_RTL_TCP_CLIENT_LAST_CONNECTED] =
            serde_json::to_value(server).unwrap_or(serde_json::Value::Null);
    });
}
