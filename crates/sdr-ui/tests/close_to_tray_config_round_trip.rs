//! Pin the persisted-config -> `AppState` hydration path. Per #512.
//!
//! These tests use `ConfigManager::in_memory` so they don't touch
//! `~/.config/sdr-rs/config.json`. The two `read_*` helpers in
//! `crate::preferences::general_page` are the canonical reads
//! (also called by `app.rs::connect_activate`); this suite locks
//! their default fallbacks and explicit-override round-trips.

use sdr_config::ConfigManager;
use sdr_ui::preferences::general_page::{read_close_to_tray, read_tray_first_close_seen};

#[test]
fn close_to_tray_default_is_true() {
    let config = ConfigManager::in_memory(&serde_json::json!({}));
    assert!(read_close_to_tray(&config), "default must be true");
}

#[test]
fn close_to_tray_persisted_false_round_trips() {
    let config = ConfigManager::in_memory(&serde_json::json!({
        "close_to_tray": false,
    }));
    assert!(!read_close_to_tray(&config));
}

#[test]
fn tray_first_close_seen_default_is_false() {
    let config = ConfigManager::in_memory(&serde_json::json!({}));
    assert!(!read_tray_first_close_seen(&config));
}

#[test]
fn tray_first_close_seen_persisted_true_round_trips() {
    let config = ConfigManager::in_memory(&serde_json::json!({
        "tray_first_close_seen": true,
    }));
    assert!(read_tray_first_close_seen(&config));
}
