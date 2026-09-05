use super::*;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn sat_names_round_trip_through_config() {
    let cfg = Arc::new(sdr_config::ConfigManager::in_memory(&serde_json::json!({})));
    let mut table = HashMap::new();
    table.insert(0x2C_u8, "ORBCOMM FM06".to_string());
    table.insert(0x05_u8, "ORBCOMM FM04".to_string());
    save_orbcomm_sat_names(&cfg, &table);
    let loaded = load_orbcomm_sat_names(&cfg);
    assert_eq!(loaded, table);
}

#[test]
fn load_missing_key_is_empty() {
    let cfg = Arc::new(sdr_config::ConfigManager::in_memory(&serde_json::json!({})));
    assert!(load_orbcomm_sat_names(&cfg).is_empty());
}
