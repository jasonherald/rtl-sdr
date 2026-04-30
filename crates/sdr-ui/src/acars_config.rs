//! ACARS config-key holders + read/save helpers.
//!
//! This module deliberately holds no GTK widgets — the
//! Aviation activity panel ships in sub-project 3 of epic
//! #474 as `crates/sdr-ui/src/sidebar/aviation_panel.rs`.
//! Sub-project 2 (pipeline integration) only needs the
//! keys + helpers so app startup persistence works.

use sdr_config::ConfigManager;

/// Persisted ACARS toggle. Default `false`.
pub const KEY_ACARS_ENABLED: &str = "acars_enabled";

/// Channel-set selector. Spec enum has only `"us-6"` in v1.
pub const KEY_ACARS_CHANNEL_SET: &str = "acars_channel_set";

/// Cap on the in-memory `acars_recent` ring buffer. Default
/// 500. Not exposed in the UI in v1; documented here so the
/// constant has one home.
pub const KEY_ACARS_RECENT_KEEP_COUNT: &str = "acars_recent_keep_count";

/// JSONL writer toggle. Default `false`. Issue #578.
pub const KEY_ACARS_JSONL_ENABLED: &str = "acars_jsonl_enabled";

/// JSONL log file path. Empty ⇒ `~/sdr-recordings/acars.jsonl`
/// at writer-open time. Issue #578.
pub const KEY_ACARS_JSONL_PATH: &str = "acars_jsonl_path";

/// UDP feeder toggle. Default `false`. Issue #578.
pub const KEY_ACARS_NETWORK_ENABLED: &str = "acars_network_enabled";

/// UDP feeder host:port. Default `feed.airframes.io:5550`.
/// Issue #578.
pub const KEY_ACARS_NETWORK_ADDR: &str = "acars_network_addr";

/// Operator station identifier embedded in the JSON's
/// `station_id` field. Empty ⇒ field omitted. Issue #578.
pub const KEY_ACARS_STATION_ID: &str = "acars_station_id";

/// Default value used when a key is missing from the config.
const DEFAULT_ACARS_ENABLED: bool = false;
const DEFAULT_ACARS_CHANNEL_SET: &str = "us-6";
const DEFAULT_ACARS_JSONL_ENABLED: bool = false;
const DEFAULT_ACARS_NETWORK_ENABLED: bool = false;
const DEFAULT_ACARS_NETWORK_ADDR: &str = "feed.airframes.io:5550";

/// Read the persisted ACARS-enabled flag, defaulting to
/// `DEFAULT_ACARS_ENABLED` if absent. Mirrors the
/// `read_close_to_tray` callback pattern in
/// `crates/sdr-ui/src/preferences/general_page.rs`.
#[must_use]
pub fn read_acars_enabled(config: &ConfigManager) -> bool {
    config.read(|v| {
        v.get(KEY_ACARS_ENABLED)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(DEFAULT_ACARS_ENABLED)
    })
}

/// Persist the ACARS-enabled flag via `ConfigManager::write`.
pub fn save_acars_enabled(config: &ConfigManager, value: bool) {
    config.write(|v| {
        v[KEY_ACARS_ENABLED] = serde_json::json!(value);
    });
}

/// Read the persisted channel-set string. Returns the default
/// (`"us-6"`) if absent or empty.
#[must_use]
pub fn read_acars_channel_set(config: &ConfigManager) -> String {
    config.read(|v| {
        v.get(KEY_ACARS_CHANNEL_SET)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map_or_else(|| DEFAULT_ACARS_CHANNEL_SET.to_string(), str::to_string)
    })
}

/// Persist the channel-set selector. Uses
/// `AcarsRegion::config_id` as the value so the round-trip
/// stays under the enum's control. Issue #581.
pub fn save_acars_channel_set(config: &ConfigManager, value: &str) {
    config.write(|v| {
        v[KEY_ACARS_CHANNEL_SET] = serde_json::json!(value);
    });
}

#[must_use]
pub fn read_acars_jsonl_enabled(config: &ConfigManager) -> bool {
    config.read(|v| {
        v.get(KEY_ACARS_JSONL_ENABLED)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(DEFAULT_ACARS_JSONL_ENABLED)
    })
}

pub fn save_acars_jsonl_enabled(config: &ConfigManager, value: bool) {
    config.write(|v| {
        v[KEY_ACARS_JSONL_ENABLED] = serde_json::json!(value);
    });
}

#[must_use]
pub fn read_acars_jsonl_path(config: &ConfigManager) -> String {
    config.read(|v| {
        v.get(KEY_ACARS_JSONL_PATH)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    })
}

pub fn save_acars_jsonl_path(config: &ConfigManager, value: &str) {
    config.write(|v| {
        v[KEY_ACARS_JSONL_PATH] = serde_json::json!(value);
    });
}

#[must_use]
pub fn read_acars_network_enabled(config: &ConfigManager) -> bool {
    config.read(|v| {
        v.get(KEY_ACARS_NETWORK_ENABLED)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(DEFAULT_ACARS_NETWORK_ENABLED)
    })
}

pub fn save_acars_network_enabled(config: &ConfigManager, value: bool) {
    config.write(|v| {
        v[KEY_ACARS_NETWORK_ENABLED] = serde_json::json!(value);
    });
}

#[must_use]
pub fn read_acars_network_addr(config: &ConfigManager) -> String {
    config.read(|v| {
        v.get(KEY_ACARS_NETWORK_ADDR)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map_or_else(|| DEFAULT_ACARS_NETWORK_ADDR.to_string(), str::to_string)
    })
}

pub fn save_acars_network_addr(config: &ConfigManager, value: &str) {
    config.write(|v| {
        v[KEY_ACARS_NETWORK_ADDR] = serde_json::json!(value);
    });
}

#[must_use]
pub fn read_acars_station_id(config: &ConfigManager) -> String {
    config.read(|v| {
        v.get(KEY_ACARS_STATION_ID)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    })
}

pub fn save_acars_station_id(config: &ConfigManager, value: &str) {
    config.write(|v| {
        v[KEY_ACARS_STATION_ID] = serde_json::json!(value);
    });
}

/// Default ring-buffer cap. Returns the spec default
/// (`ACARS_RECENT_DEFAULT_KEEP = 500`); sub-project 3 may
/// extend this to consult `ConfigManager` for an override.
#[must_use]
pub const fn default_recent_keep() -> u32 {
    sdr_core::acars_airband_lock::ACARS_RECENT_DEFAULT_KEEP
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_config() -> ConfigManager {
        // Mirror the in-tree pattern (see
        // `crates/sdr-ui/src/sidebar/activity_bar.rs` tests):
        // `ConfigManager::in_memory(&serde_json::json!({}))`.
        // serde_json is already a workspace dep of sdr-ui.
        ConfigManager::in_memory(&serde_json::json!({}))
    }

    #[test]
    fn defaults_when_unset() {
        let cfg = fresh_config();
        assert!(!read_acars_enabled(&cfg));
        assert_eq!(read_acars_channel_set(&cfg), "us-6");
    }

    #[test]
    fn round_trip_enabled() {
        let cfg = fresh_config();
        save_acars_enabled(&cfg, true);
        assert!(read_acars_enabled(&cfg));
        save_acars_enabled(&cfg, false);
        assert!(!read_acars_enabled(&cfg));
    }

    #[test]
    fn round_trip_channel_set() {
        let cfg = fresh_config();
        save_acars_channel_set(&cfg, "europe");
        assert_eq!(read_acars_channel_set(&cfg), "europe");
        save_acars_channel_set(&cfg, "us-6");
        assert_eq!(read_acars_channel_set(&cfg), "us-6");
    }

    #[test]
    fn output_keys_default_when_unset() {
        let cfg = fresh_config();
        assert!(!read_acars_jsonl_enabled(&cfg));
        assert_eq!(read_acars_jsonl_path(&cfg), "");
        assert!(!read_acars_network_enabled(&cfg));
        assert_eq!(read_acars_network_addr(&cfg), "feed.airframes.io:5550");
        assert_eq!(read_acars_station_id(&cfg), "");
    }

    #[test]
    fn output_keys_round_trip() {
        let cfg = fresh_config();
        save_acars_jsonl_enabled(&cfg, true);
        save_acars_jsonl_path(&cfg, "/tmp/foo.jsonl");
        save_acars_network_enabled(&cfg, true);
        save_acars_network_addr(&cfg, "127.0.0.1:5550");
        save_acars_station_id(&cfg, "TEST1");
        assert!(read_acars_jsonl_enabled(&cfg));
        assert_eq!(read_acars_jsonl_path(&cfg), "/tmp/foo.jsonl");
        assert!(read_acars_network_enabled(&cfg));
        assert_eq!(read_acars_network_addr(&cfg), "127.0.0.1:5550");
        assert_eq!(read_acars_station_id(&cfg), "TEST1");
    }
}
