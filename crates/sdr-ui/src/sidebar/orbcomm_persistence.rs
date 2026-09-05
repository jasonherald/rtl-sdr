//! Persistence for the learned Orbcomm `sat_id → name` table and the
//! parsed Orbcomm TLE candidate list. Keyed off a single JSON object
//! under `orbcomm_sat_names` (precedent: watched-satellites set).

use std::collections::HashMap;
use std::sync::Arc;

use sdr_config::ConfigManager;
use sdr_sat::{Satellite, TleCache};

const KEY_ORBCOMM_SAT_NAMES: &str = "orbcomm_sat_names";

/// Load the learned `sat_id → name` table (empty if absent/malformed).
#[must_use]
pub fn load_orbcomm_sat_names(config: &Arc<ConfigManager>) -> HashMap<u8, String> {
    config.read(|v| {
        v.get(KEY_ORBCOMM_SAT_NAMES)
            .and_then(serde_json::Value::as_object)
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, val)| {
                        let id = k.parse::<u8>().ok()?;
                        let name = val.as_str()?.to_string();
                        Some((id, name))
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Persist the learned table as a JSON object keyed by decimal `sat_id`.
///
/// Generic over `BuildHasher` so call sites that build the map with
/// the default hasher don't have to thread a hasher type through —
/// mirrors `save_watched_satellites`' `clippy::implicit_hasher` fix.
pub fn save_orbcomm_sat_names<S: std::hash::BuildHasher>(
    config: &Arc<ConfigManager>,
    table: &HashMap<u8, String, S>,
) {
    let obj: serde_json::Map<String, serde_json::Value> = table
        .iter()
        .map(|(id, name)| (id.to_string(), serde_json::Value::String(name.clone())))
        .collect();
    config.write(|v| {
        v[KEY_ORBCOMM_SAT_NAMES] = serde_json::Value::Object(obj);
    });
}

/// Parse the cached Orbcomm group TLEs into propagatable `Satellite`s.
/// Skips entries whose elements fail to parse. Empty if the group cache
/// is absent (never fetches — callers refresh off-thread).
#[must_use]
pub fn orbcomm_tles_from_cache(cache: &TleCache) -> Vec<(String, Satellite)> {
    let Ok(triples) = cache.cached_group_tles(sdr_sat::ORBCOMM_TLE_GROUP) else {
        return Vec::new();
    };
    triples
        .into_iter()
        .filter_map(|(name, l1, l2)| Satellite::from_tle(&name, &l1, &l2).ok().map(|s| (name, s)))
        .collect()
}

#[cfg(test)]
mod tests;
