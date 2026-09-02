//! Config readers / writers used by the Satellites wiring layer —
//! ground-station coordinates, the auto-record toggles and quality
//! tier, Doppler tracking, watched satellites, notify lead time,
//! and the TLE last-refresh timestamp. Split out of
//! `satellites_panel.rs` per the file-size pass (issue #819).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sdr_config::ConfigManager;

use super::{
    AutoRecordQuality, DEFAULT_STATION_ALT_M, DEFAULT_STATION_LAT_DEG, DEFAULT_STATION_LON_DEG,
    KEY_AUTO_RECORD_APT, KEY_AUTO_RECORD_AUDIO, KEY_AUTO_RECORD_COMPOSITES,
    KEY_AUTO_RECORD_QUALITY, KEY_DOPPLER_TRACKING_ENABLED, KEY_NOTIFY_LEAD_MIN, KEY_STATION_ALT_M,
    KEY_STATION_LAT_DEG, KEY_STATION_LON_DEG, KEY_TLE_LAST_REFRESH, KEY_WATCHED_SATELLITES,
};

// ─── Config readers / writers used by the wiring layer ───────────────

/// Read the persisted ground station latitude, or fall back to
/// [`DEFAULT_STATION_LAT_DEG`].
#[must_use]
pub fn load_station_lat_deg(config: &Arc<ConfigManager>) -> f64 {
    read_f64_or(config, KEY_STATION_LAT_DEG, DEFAULT_STATION_LAT_DEG)
}

/// Read the persisted ground station longitude, or fall back to
/// [`DEFAULT_STATION_LON_DEG`].
#[must_use]
pub fn load_station_lon_deg(config: &Arc<ConfigManager>) -> f64 {
    read_f64_or(config, KEY_STATION_LON_DEG, DEFAULT_STATION_LON_DEG)
}

/// Read the persisted ground station altitude, or fall back to
/// [`DEFAULT_STATION_ALT_M`].
#[must_use]
pub fn load_station_alt_m(config: &Arc<ConfigManager>) -> f64 {
    read_f64_or(config, KEY_STATION_ALT_M, DEFAULT_STATION_ALT_M)
}

/// Read the persisted "auto-record APT passes" toggle state.
/// Defaults to `false` (opt-in).
#[must_use]
pub fn load_auto_record_apt(config: &Arc<ConfigManager>) -> bool {
    read_bool_or(config, KEY_AUTO_RECORD_APT, false)
}

/// Read the persisted "also save audio" toggle. Defaults to
/// `false` (opt-in). Per #533.
#[must_use]
pub fn load_auto_record_audio(config: &Arc<ConfigManager>) -> bool {
    read_bool_or(config, KEY_AUTO_RECORD_AUDIO, false)
}

/// Read the persisted "save false-colour composites" toggle.
/// Defaults to `false` (opt-in — composites add a few MB per
/// pass and not every user wants the extras). Per #547.
#[must_use]
pub fn load_auto_record_composites(config: &Arc<ConfigManager>) -> bool {
    read_bool_or(config, KEY_AUTO_RECORD_COMPOSITES, false)
}

/// Read the persisted auto-record quality threshold. Defaults to
/// [`AutoRecordQuality::DEFAULT`] (`WinnersAndGood`) — matches the
/// previously-hardcoded `AUTO_RECORD_MIN_ELEV_DEG`. Per #511.
#[must_use]
pub fn load_auto_record_quality(config: &Arc<ConfigManager>) -> AutoRecordQuality {
    let idx = config.read(|v| {
        v.get(KEY_AUTO_RECORD_QUALITY)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(AutoRecordQuality::DEFAULT.to_index())
    });
    AutoRecordQuality::from_index(idx)
}

/// Symmetric writer for [`load_auto_record_quality`]. Persists the
/// combo's u32 index — `AutoRecordQuality::from_index` snaps any
/// future-version / out-of-range value back to `DEFAULT` on the
/// next read. Centralizes the enum→u32 mapping with the read helper
/// rather than open-coding it at every call site. Per CR round 1 on
/// PR #574.
pub fn save_auto_record_quality(config: &Arc<ConfigManager>, quality: AutoRecordQuality) {
    config.write(|v| {
        v[KEY_AUTO_RECORD_QUALITY] = serde_json::json!(quality.to_index());
    });
}

/// Load the persisted Doppler-tracking master switch. Defaults
/// to `true` — fresh installs get auto-correction without
/// requiring the user to find and flip the switch. Per #521.
#[must_use]
pub fn load_doppler_tracking_enabled(config: &Arc<ConfigManager>) -> bool {
    config.read(|v| {
        v.get(KEY_DOPPLER_TRACKING_ENABLED)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    })
}

/// Persist `value` under `key`. Single helper for the three
/// lat/lon/alt `SpinRow` change-notify handlers.
pub fn save_f64(config: &Arc<ConfigManager>, key: &str, value: f64) {
    config.write(|v| {
        v[key] = serde_json::json!(value);
    });
}

/// Persist `value` under [`KEY_AUTO_RECORD_APT`].
pub fn save_auto_record_apt(config: &Arc<ConfigManager>, value: bool) {
    config.write(|v| {
        v[KEY_AUTO_RECORD_APT] = serde_json::json!(value);
    });
}

/// Persist `value` under [`KEY_AUTO_RECORD_AUDIO`].
pub fn save_auto_record_audio(config: &Arc<ConfigManager>, value: bool) {
    config.write(|v| {
        v[KEY_AUTO_RECORD_AUDIO] = serde_json::json!(value);
    });
}

/// Symmetric writer for [`load_auto_record_composites`]. Per #547.
pub fn save_auto_record_composites(config: &Arc<ConfigManager>, value: bool) {
    config.write(|v| {
        v[KEY_AUTO_RECORD_COMPOSITES] = serde_json::json!(value);
    });
}

/// Persist the Doppler-tracking master switch. Per #521.
pub fn save_doppler_tracking_enabled(config: &Arc<ConfigManager>, enabled: bool) {
    config.write(|v| {
        v[KEY_DOPPLER_TRACKING_ENABLED] = serde_json::json!(enabled);
    });
}

/// Load the persisted set of watched-satellite NORAD ids. Per #510.
/// Stored as a JSON array; entries with the wrong type are dropped
/// silently (cheaper than refusing the whole list, and still safe
/// because non-watched ids just don't fire notifications).
#[must_use]
pub fn load_watched_satellites(config: &Arc<ConfigManager>) -> std::collections::HashSet<u32> {
    config.read(|v| {
        v.get(KEY_WATCHED_SATELLITES)
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(serde_json::Value::as_u64)
                    .filter_map(|n| u32::try_from(n).ok())
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Persist the watched-satellite NORAD-id set. Sorted on write so
/// the JSON file diffs cleanly across saves (set iteration order is
/// non-deterministic; sorting makes config commits readable).
///
/// Generic over `BuildHasher` so call sites that build the set with
/// the default hasher don't have to thread a hasher type through —
/// per the `clippy::implicit_hasher` recommendation. Cheap: every
/// path is monomorphized inline.
pub fn save_watched_satellites<S: std::hash::BuildHasher>(
    config: &Arc<ConfigManager>,
    watched: &std::collections::HashSet<u32, S>,
) {
    let mut sorted: Vec<u32> = watched.iter().copied().collect();
    sorted.sort_unstable();
    config.write(|v| {
        v[KEY_WATCHED_SATELLITES] = serde_json::json!(sorted);
    });
}

/// Load the persisted lead-minutes for pre-pass notifications.
/// Defaults to [`crate::sidebar::satellites_notify::DEFAULT_NOTIFY_LEAD_MIN`]
/// and clamps to the `[NOTIFY_LEAD_MIN_LOWER, NOTIFY_LEAD_MIN_UPPER]`
/// range so a hand-edited config can't push us into degenerate
/// states (e.g. `0` would flicker between fire and miss every
/// tick). Per #510.
#[must_use]
pub fn load_notify_lead_min(config: &Arc<ConfigManager>) -> u32 {
    use crate::sidebar::satellites_notify::{
        DEFAULT_NOTIFY_LEAD_MIN, NOTIFY_LEAD_MIN_LOWER, NOTIFY_LEAD_MIN_UPPER,
    };
    let raw = config.read(|v| {
        v.get(KEY_NOTIFY_LEAD_MIN)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(DEFAULT_NOTIFY_LEAD_MIN)
    });
    raw.clamp(NOTIFY_LEAD_MIN_LOWER, NOTIFY_LEAD_MIN_UPPER)
}

/// Persist the pre-pass notify lead time (whole minutes). Clamps
/// to `[NOTIFY_LEAD_MIN_LOWER, NOTIFY_LEAD_MIN_UPPER]` before
/// writing — `load_notify_lead_min` clamps on read too, but
/// clamping at the writer keeps the on-disk contract honest for
/// any caller (tests, CLI tooling, future scripted edits) that
/// hands us a raw value. Per #510 / CR round 1 on PR #568.
pub fn save_notify_lead_min(config: &Arc<ConfigManager>, lead_min: u32) {
    use crate::sidebar::satellites_notify::{NOTIFY_LEAD_MIN_LOWER, NOTIFY_LEAD_MIN_UPPER};
    let lead_min = lead_min.clamp(NOTIFY_LEAD_MIN_LOWER, NOTIFY_LEAD_MIN_UPPER);
    config.write(|v| {
        v[KEY_NOTIFY_LEAD_MIN] = serde_json::json!(lead_min);
    });
}

/// Persist a fresh "last refreshed" RFC3339 timestamp under
/// [`KEY_TLE_LAST_REFRESH`].
pub fn save_tle_last_refresh(config: &Arc<ConfigManager>, when: DateTime<Utc>) {
    config.write(|v| {
        v[KEY_TLE_LAST_REFRESH] = serde_json::json!(when.to_rfc3339());
    });
}

pub(super) fn read_f64_or(config: &Arc<ConfigManager>, key: &str, default: f64) -> f64 {
    config.read(|v| {
        v.get(key)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(default)
    })
}

pub(super) fn read_bool_or(config: &Arc<ConfigManager>, key: &str, default: bool) -> bool {
    config.read(|v| {
        v.get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(default)
    })
}

/// Format the "last refreshed" timestamp for the action row's
/// subtitle. Shows "Never" if no refresh has been recorded; shows
/// the local-timezone wall-clock time otherwise so it reads
/// naturally to the user.
#[must_use]
pub fn format_last_refresh(config: &Arc<ConfigManager>) -> String {
    let raw = config.read(|v| {
        v.get(KEY_TLE_LAST_REFRESH)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    });
    match raw {
        None => "Never".to_string(),
        Some(rfc3339) => match chrono::DateTime::parse_from_rfc3339(&rfc3339) {
            Ok(dt) => dt
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M %Z")
                .to_string(),
            Err(_) => rfc3339, // Show the raw stamp on parse failure
        },
    }
}
