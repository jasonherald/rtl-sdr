use super::*;
use std::time::Duration;

/// Fixed Unix timestamp used in the favorites round-trip test
/// to pin the `last_seen_unix` field. Value is arbitrary (from
/// November 2023) but deliberately chosen to be well past any
/// clock-skew-fallback sentinel and well before `u32::MAX`
/// seconds so overflow edges aren't in play.
const TEST_LAST_SEEN_UNIX: u64 = 1_700_000_000;
/// Unix timestamp for 2020-01-01T00:00:00Z. Used by the
/// `now_unix_seconds` smoke test as a "modern wall-clock"
/// floor — anything past this is clearly real time and not a
/// clock-skew fallback returning 0.
const MODERN_UNIX_FLOOR: u64 = 1_577_836_800;

/// Compile-time validation that gain constants are consistent.
const _: () = {
    assert!(MIN_GAIN_DB <= MAX_GAIN_DB);
    assert!(GAIN_STEP_DB > 0.0);
};

/// Compile-time validation that port constants are consistent.
const _: () = {
    assert!(MIN_PORT <= MAX_PORT);
    assert!(DEFAULT_PORT >= MIN_PORT);
    assert!(DEFAULT_PORT <= MAX_PORT);
};

/// Compile-time validation that PPM constants are consistent.
const _: () = {
    assert!(MIN_PPM <= MAX_PPM);
    assert!(DEFAULT_PPM >= MIN_PPM);
    assert!(DEFAULT_PPM <= MAX_PPM);
    assert!(PPM_STEP > 0.0);
};

#[test]
fn device_indices_are_distinct() {
    // Full pairwise distinctness — adding a 5th source type
    // without updating this test would let a collision slip
    // through. The device_row -> SourceType match in window.rs
    // depends on these being unique integer indices.
    assert_ne!(DEVICE_RTLSDR, DEVICE_NETWORK);
    assert_ne!(DEVICE_RTLSDR, DEVICE_FILE);
    assert_ne!(DEVICE_RTLSDR, DEVICE_RTLTCP);
    assert_ne!(DEVICE_NETWORK, DEVICE_FILE);
    assert_ne!(DEVICE_NETWORK, DEVICE_RTLTCP);
    assert_ne!(DEVICE_FILE, DEVICE_RTLTCP);
}

/// Part 1 of 3 of the `format_rtl_tcp_state` sweep (`time` … `Connected`).
#[test]
fn format_rtl_tcp_state_covers_every_variant_1() {
    // Disconnected → empty-looking but consistent with the const.

    assert_eq!(
        format_rtl_tcp_state(&RtlTcpConnectionState::Disconnected),
        RTL_TCP_STATUS_DISCONNECTED_SUBTITLE
    );
    // Connecting → ellipsis marker (avoids the reader confusing
    // "Connecting" with "Connected" on a cursory glance).

    assert_eq!(
        format_rtl_tcp_state(&RtlTcpConnectionState::Connecting),
        "Connecting…"
    );
    // Connected with `None` codec keeps the short form — the
    // common path every legacy server hits, and the default for
    // our own client. Adding a "(None)" suffix here would noise
    // up every single connection in exchange for zero signal.

    assert_eq!(
        format_rtl_tcp_state(&RtlTcpConnectionState::Connected {
            tuner_name: "R820T".into(),
            gain_count: 29,
            codec: "None".into(),
            granted_role: Some(true),
        }),
        "Connected — R820T (29 gains)"
    );
}

/// Part 2 of 3 of the `format_rtl_tcp_state` sweep (`Connected` … `from_secs`).
#[test]
fn format_rtl_tcp_state_covers_every_variant_2() {
    // Connected with a non-`None` codec gets an extra suffix so
    // the user can see which codec actually landed. Signals
    // that compression is active without forcing them to hunt
    // through logs.
    assert_eq!(
        format_rtl_tcp_state(&RtlTcpConnectionState::Connected {
            tuner_name: "R820T".into(),
            gain_count: 29,
            codec: "LZ4".into(),
            granted_role: Some(true),
        }),
        "Connected — R820T (29 gains, LZ4)"
    );
    // Retrying ceils fractional seconds so the row never
    // understates the delay: 250 ms remaining → "1 s", not
    // "0 s" (which would read as "the retry just fired").

    assert_eq!(
        format_rtl_tcp_state(&RtlTcpConnectionState::Retrying {
            attempt: 3,
            retry_in: Duration::from_millis(250),
        }),
        "Retrying in 1 s (attempt 3)"
    );
    // Key regression guard for the ceil semantics — 1.9 s must
    // read as "2 s", never "1 s". Flooring on `as_secs` would
    // silently understate the countdown here.

    assert_eq!(
        format_rtl_tcp_state(&RtlTcpConnectionState::Retrying {
            attempt: 4,
            retry_in: Duration::from_millis(1_900),
        }),
        "Retrying in 2 s (attempt 4)"
    );
    // Exact integer seconds must NOT get bumped by the ceil —
    // 12 s stays at "12 s", not "13 s".

    assert_eq!(
        format_rtl_tcp_state(&RtlTcpConnectionState::Retrying {
            attempt: 5,
            retry_in: Duration::from_secs(12),
        }),
        "Retrying in 12 s (attempt 5)"
    );
}

/// Part 3 of 3 of the `format_rtl_tcp_state` sweep (`Failed` … `AuthFailed`).
#[test]
fn format_rtl_tcp_state_covers_every_variant_3() {
    assert_eq!(
        format_rtl_tcp_state(&RtlTcpConnectionState::Failed {
            reason: "bad handshake".into(),
        }),
        "Failed — bad handshake"
    );
    // Role-denial states (#396) get their own short
    // subtitles — no reason string needed because the
    // variant itself IS the reason. Lock in each copy
    // against accidental drift; a typo here would ship
    // to users without CI catching it otherwise.

    assert_eq!(
        format_rtl_tcp_state(&RtlTcpConnectionState::ControllerBusy),
        "Controller slot is occupied",
    );

    assert_eq!(
        format_rtl_tcp_state(&RtlTcpConnectionState::AuthRequired),
        "Server requires a key",
    );

    assert_eq!(
        format_rtl_tcp_state(&RtlTcpConnectionState::AuthFailed),
        "Key rejected",
    );
}

// ---- Client-persistence helpers (favorites + last-connected) ----

fn make_config() -> Arc<ConfigManager> {
    Arc::new(ConfigManager::in_memory(&serde_json::json!({})))
}

#[test]
fn favorites_round_trip_preserves_rich_metadata() {
    let config = make_config();
    // Fresh config → empty list.
    assert!(load_favorites(&config).is_empty());
    let favs = vec![
        FavoriteEntry {
            key: "shack-pi.local.:1234".into(),
            nickname: "Shack Pi".into(),
            tuner_name: Some("R820T".into()),
            gain_count: Some(29),
            last_seen_unix: Some(TEST_LAST_SEEN_UNIX),
            requested_role: Some(FavoriteRole::Listen),
            auth_required: Some(true),
        },
        FavoriteEntry {
            key: "attic-pi.local.:1234".into(),
            nickname: "Attic Pi".into(),
            tuner_name: None,
            gain_count: None,
            last_seen_unix: None,
            requested_role: None,
            auth_required: None,
        },
    ];
    save_favorites(&config, &favs);
    let loaded = load_favorites(&config);
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].key, "shack-pi.local.:1234");
    assert_eq!(loaded[0].nickname, "Shack Pi");
    assert_eq!(loaded[0].tuner_name.as_deref(), Some("R820T"));
    assert_eq!(loaded[0].gain_count, Some(29));
    assert_eq!(loaded[0].last_seen_unix, Some(TEST_LAST_SEEN_UNIX));
    // Role + auth_required round-trip on the opt-in side.
    // Per #396: the JSON surface carries these through the
    // serde `snake_case` rename and skip-if-none attributes.
    assert_eq!(loaded[0].requested_role, Some(FavoriteRole::Listen));
    assert_eq!(loaded[0].auth_required, Some(true));
    // Second entry has every optional field None → must
    // round-trip as None, NOT as missing / default values.
    assert!(loaded[1].tuner_name.is_none());
    assert!(loaded[1].gain_count.is_none());
    assert!(loaded[1].last_seen_unix.is_none());
    assert!(loaded[1].requested_role.is_none());
    assert!(loaded[1].auth_required.is_none());
}

#[test]
fn favorites_loader_upgrades_legacy_string_entries() {
    // Regression guard for the PR #335 → #315 schema
    // migration. Users who starred servers before #315 have
    // `Vec<String>` persisted; the new loader must synthesize
    // `FavoriteEntry` stubs so those favorites still appear
    // in the slide-out (with degraded metadata until the
    // server re-announces).
    let config = make_config();
    config.write(|v| {
        v[KEY_RTL_TCP_CLIENT_FAVORITES] =
            serde_json::json!(["shack-pi.local.:1234", "attic-pi.local.:1235",]);
    });
    let loaded = load_favorites(&config);
    assert_eq!(loaded.len(), 2);
    // `nickname` falls back to the key so the slide-out has
    // something printable.
    assert_eq!(loaded[0].key, "shack-pi.local.:1234");
    assert_eq!(loaded[0].nickname, "shack-pi.local.:1234");
    // Metadata blanks — filled by next re-announce + re-star.
    assert!(loaded[0].tuner_name.is_none());
    assert!(loaded[0].gain_count.is_none());
    assert!(loaded[0].last_seen_unix.is_none());
    // Role + auth-required fields are #396 additions and
    // must also default to `None` for legacy bare-string
    // entries — the connect path treats `None` as
    // "unknown, default to Control / don't pre-reveal the
    // auth row." A regression that silently wrote `Some`
    // defaults here would change the UX for every
    // pre-#396 favorite on the first launch after upgrade.
    assert!(loaded[0].requested_role.is_none());
    assert!(loaded[0].auth_required.is_none());
}

#[test]
fn favorites_loader_tolerates_non_array_entry() {
    // If someone hand-edits the config file and makes the
    // entry a string (not an array), we shouldn't panic or
    // corrupt state — just return empty and let the user
    // re-pin.
    let config = make_config();
    config.write(|v| {
        v[KEY_RTL_TCP_CLIENT_FAVORITES] = serde_json::json!("not an array");
    });
    assert!(load_favorites(&config).is_empty());
}

#[test]
fn favorites_loader_skips_corrupt_object_entries() {
    // Mixed-array case: a well-formed FavoriteEntry object
    // alongside a JSON blob that doesn't match the schema
    // (e.g. missing required fields). The bad entry is
    // dropped; the good one survives — no "one bad apple
    // spoils the list" failure mode.
    let config = make_config();
    config.write(|v| {
        v[KEY_RTL_TCP_CLIENT_FAVORITES] = serde_json::json!([
            { "key": "shack-pi.local.:1234", "nickname": "Shack Pi" },
            { "this": "is not a FavoriteEntry" },
        ]);
    });
    let loaded = load_favorites(&config);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].key, "shack-pi.local.:1234");
}

#[test]
fn now_unix_seconds_is_monotonic_within_call() {
    // Not a real monotonicity test — just a smoke-test that
    // the helper returns a sensible modern value. Anything
    // past `MODERN_UNIX_FLOOR` (2020-01-01T00:00:00Z) is
    // clearly real wall-clock time and not a clock-skew
    // fallback returning 0.
    assert!(now_unix_seconds() > MODERN_UNIX_FLOOR);
}

#[test]
fn last_connected_round_trip() {
    let config = make_config();
    assert!(load_last_connected(&config).is_none());
    let server = LastConnectedServer {
        host: "192.168.1.5".to_string(),
        port: 1234,
        nickname: "shack-pi".to_string(),
    };
    save_last_connected(&config, &server);
    let loaded = load_last_connected(&config).expect("loaded");
    assert_eq!(loaded.host, server.host);
    assert_eq!(loaded.port, server.port);
    assert_eq!(loaded.nickname, server.nickname);
}

#[test]
fn last_connected_loader_tolerates_malformed_entry() {
    // Schema drift: an older version persisted a plain string.
    // New loader should return None rather than panic.
    let config = make_config();
    config.write(|v| {
        v[KEY_RTL_TCP_CLIENT_LAST_CONNECTED] = serde_json::json!("shack-pi:1234");
    });
    assert!(load_last_connected(&config).is_none());
}

// --- AGC type persistence tests (#356) ---

/// Fresh config with no AGC key and no legacy key returns
/// the default (Software). Matches the "fresh-install user
/// gets the well-behaved path" contract from the issue.
#[test]
fn load_agc_type_defaults_to_software_on_fresh_config() {
    let config = make_config();
    assert_eq!(load_agc_type(&config), AgcType::Software);
    assert_eq!(AgcType::DEFAULT, AgcType::Software);
}

/// Round-trip: save each variant, load returns it. Pins the
/// serde representation against future rename / enum
/// reordering.
#[test]
fn agc_type_save_load_round_trips_all_variants() {
    for variant in [AgcType::Off, AgcType::Hardware, AgcType::Software] {
        let config = make_config();
        save_agc_type(&config, variant);
        assert_eq!(
            load_agc_type(&config),
            variant,
            "round-trip failed for {variant:?}"
        );
    }
}

/// Legacy migration: a pre-#354 config has only the boolean
/// `rtl_sdr_agc_enabled` key. Loader maps `true → Hardware`,
/// `false → Off`. Preserves the user's upgrade path without
/// a one-shot migration job.
#[test]
fn load_agc_type_migrates_legacy_boolean_on() {
    let config = make_config();
    config.write(|v| {
        v[KEY_LEGACY_AGC_ENABLED] = serde_json::json!(true);
    });
    assert_eq!(load_agc_type(&config), AgcType::Hardware);
}

#[test]
fn load_agc_type_migrates_legacy_boolean_off() {
    let config = make_config();
    config.write(|v| {
        v[KEY_LEGACY_AGC_ENABLED] = serde_json::json!(false);
    });
    assert_eq!(load_agc_type(&config), AgcType::Off);
}

/// When both the new key and the legacy key are present,
/// the new key wins. Guards against a mis-migration that
/// could silently revert a user's post-upgrade selection.
#[test]
fn load_agc_type_new_key_wins_over_legacy_key() {
    let config = make_config();
    config.write(|v| {
        v[KEY_LEGACY_AGC_ENABLED] = serde_json::json!(true);
    });
    save_agc_type(&config, AgcType::Software);
    assert_eq!(load_agc_type(&config), AgcType::Software);
}

/// Corrupt `agc_type` value (e.g. renamed-since enum variant
/// that's no longer recognized) falls back to the legacy
/// key then the default, without panicking.
#[test]
fn load_agc_type_tolerates_corrupt_new_key() {
    let config = make_config();
    config.write(|v| {
        v[KEY_AGC_TYPE] = serde_json::json!("this_is_not_a_valid_variant");
        v[KEY_LEGACY_AGC_ENABLED] = serde_json::json!(true);
    });
    // Corrupt new key skipped → fall through to legacy →
    // Hardware.
    assert_eq!(load_agc_type(&config), AgcType::Hardware);
}

/// `agc_type_from_selected` round-trips each legal index
/// and returns `None` on unknown indices so callers can
/// reject transient GTK teardown values instead of
/// silently dispatching a fallback as a real user choice.
#[test]
fn agc_type_selected_index_helpers_round_trip() {
    for variant in [AgcType::Off, AgcType::Hardware, AgcType::Software] {
        let idx = selected_from_agc_type(variant);
        assert_eq!(agc_type_from_selected(idx), Some(variant));
    }
    // Unknown index → `None`. Notify handler early-returns
    // on this to avoid corrupting persisted config.
    assert_eq!(agc_type_from_selected(99), None);
    // `u32::MAX` is the `gtk4::INVALID_LIST_POSITION`
    // sentinel; make sure we don't panic or coerce.
    assert_eq!(agc_type_from_selected(u32::MAX), None);
}

// --- Bias-T persistence tests (#537) ---

/// Fresh config (no key present) defaults to `false`.
/// "Safe by default" — a user without a powered antenna
/// shouldn't get 5 V on the coax on first launch.
#[test]
fn load_source_rtl_bias_tee_defaults_to_off() {
    let config = make_config();
    assert!(!load_source_rtl_bias_tee(&config));
}

/// Round-trip: write `true`, read back `true`.
#[test]
fn save_and_load_source_rtl_bias_tee_round_trip() {
    let config = make_config();
    save_source_rtl_bias_tee(&config, true);
    assert!(load_source_rtl_bias_tee(&config));
    save_source_rtl_bias_tee(&config, false);
    assert!(!load_source_rtl_bias_tee(&config));
}

/// Corrupt value (wrong JSON type) falls back to default
/// rather than panicking. Mirrors the
/// `load_agc_type_tolerates_corrupt_new_key` resilience
/// pattern.
#[test]
fn load_source_rtl_bias_tee_tolerates_non_bool() {
    let config = make_config();
    config.write(|v| {
        v[KEY_SOURCE_RTL_BIAS_TEE] = serde_json::json!("not a bool");
    });
    assert!(!load_source_rtl_bias_tee(&config));
}

/// #538 persistence: direct-sampling combo index.
#[test]
fn source_rtl_direct_sampling_round_trip_and_default() {
    let config = make_config();
    assert_eq!(
        load_source_rtl_direct_sampling_mode(&config),
        DIRECT_SAMPLING_DISABLED_IDX
    );
    save_source_rtl_direct_sampling_mode(&config, DIRECT_SAMPLING_Q_BRANCH_IDX);
    assert_eq!(
        load_source_rtl_direct_sampling_mode(&config),
        DIRECT_SAMPLING_Q_BRANCH_IDX
    );
    save_source_rtl_direct_sampling_mode(&config, DIRECT_SAMPLING_I_BRANCH_IDX);
    assert_eq!(
        load_source_rtl_direct_sampling_mode(&config),
        DIRECT_SAMPLING_I_BRANCH_IDX
    );
    // Corrupt value falls back to Disabled.
    config.write(|v| v[KEY_SOURCE_RTL_DIRECT_SAMPLING_MODE] = serde_json::json!("not a u64"));
    assert_eq!(
        load_source_rtl_direct_sampling_mode(&config),
        DIRECT_SAMPLING_DISABLED_IDX
    );
    // Out-of-range numeric falls back to Disabled — covers
    // a stale config from a future build that added more
    // direct-sampling modes than this build understands.
    config.write(|v| v[KEY_SOURCE_RTL_DIRECT_SAMPLING_MODE] = serde_json::json!(99));
    assert_eq!(
        load_source_rtl_direct_sampling_mode(&config),
        DIRECT_SAMPLING_DISABLED_IDX
    );
}

/// #539 persistence: offset-tuning toggle.
#[test]
fn source_rtl_offset_tuning_round_trip_and_default() {
    let config = make_config();
    assert!(!load_source_rtl_offset_tuning(&config));
    save_source_rtl_offset_tuning(&config, true);
    assert!(load_source_rtl_offset_tuning(&config));
    save_source_rtl_offset_tuning(&config, false);
    assert!(!load_source_rtl_offset_tuning(&config));
    // Corrupt value falls back to false.
    config.write(|v| v[KEY_SOURCE_RTL_OFFSET_TUNING] = serde_json::json!("not a bool"));
    assert!(!load_source_rtl_offset_tuning(&config));
}

/// #551 persistence: gain in dB.
#[test]
fn source_rtl_gain_db_round_trip_and_default() {
    let config = make_config();
    assert!((load_source_rtl_gain_db(&config) - 0.0).abs() < f64::EPSILON);
    save_source_rtl_gain_db(&config, 35.5);
    assert!((load_source_rtl_gain_db(&config) - 35.5).abs() < f64::EPSILON);
    config.write(|v| v[KEY_SOURCE_RTL_GAIN_DB] = serde_json::json!("not a number"));
    assert!((load_source_rtl_gain_db(&config) - 0.0).abs() < f64::EPSILON);
}

/// #551 persistence: PPM correction.
#[test]
fn source_rtl_ppm_round_trip_and_default() {
    let config = make_config();
    assert_eq!(load_source_rtl_ppm(&config), 0);
    save_source_rtl_ppm(&config, -25);
    assert_eq!(load_source_rtl_ppm(&config), -25);
    save_source_rtl_ppm(&config, 50);
    assert_eq!(load_source_rtl_ppm(&config), 50);
    config.write(|v| v[KEY_SOURCE_RTL_PPM] = serde_json::json!("not a number"));
    assert_eq!(load_source_rtl_ppm(&config), 0);
}

// ─── #552 persistence round-trips ─────────────────────────

#[test]
fn source_device_index_round_trip_and_default() {
    let config = make_config();
    assert_eq!(load_source_device_index(&config), DEVICE_RTLSDR);
    save_source_device_index(&config, DEVICE_NETWORK);
    assert_eq!(load_source_device_index(&config), DEVICE_NETWORK);
    config.write(|v| v[KEY_SOURCE_DEVICE_INDEX] = serde_json::json!("nope"));
    assert_eq!(load_source_device_index(&config), DEVICE_RTLSDR);
    // Out-of-range numeric: a future build that added more
    // source types and was rolled back must fall back, not
    // pin the combo to a non-existent index.
    config.write(|v| v[KEY_SOURCE_DEVICE_INDEX] = serde_json::json!(999));
    assert_eq!(load_source_device_index(&config), DEVICE_RTLSDR);
}

#[test]
fn source_sample_rate_index_round_trip_and_default() {
    let config = make_config();
    // Missing key falls back to the panel's actual default
    // (DEFAULT_SAMPLE_RATE_INDEX = 7 = 2.4 MHz), not 0
    // (250 kHz). Per CodeRabbit round 1 on PR #558.
    assert_eq!(
        load_source_sample_rate_index(&config),
        DEFAULT_SAMPLE_RATE_INDEX
    );
    save_source_sample_rate_index(&config, 3);
    assert_eq!(load_source_sample_rate_index(&config), 3);
    config.write(|v| v[KEY_SOURCE_SAMPLE_RATE_INDEX] = serde_json::json!("nope"));
    assert_eq!(
        load_source_sample_rate_index(&config),
        DEFAULT_SAMPLE_RATE_INDEX
    );
    // Out-of-range numeric falls back to default.
    config.write(|v| v[KEY_SOURCE_SAMPLE_RATE_INDEX] = serde_json::json!(9999));
    assert_eq!(
        load_source_sample_rate_index(&config),
        DEFAULT_SAMPLE_RATE_INDEX
    );
}

#[test]
fn source_decimation_index_round_trip_and_default() {
    let config = make_config();
    assert_eq!(load_source_decimation_index(&config), 0);
    save_source_decimation_index(&config, 2);
    assert_eq!(load_source_decimation_index(&config), 2);
    config.write(|v| v[KEY_SOURCE_DECIMATION_INDEX] = serde_json::json!("nope"));
    assert_eq!(load_source_decimation_index(&config), 0);
    // Out-of-range numeric falls back to 1× decimation.
    config.write(|v| v[KEY_SOURCE_DECIMATION_INDEX] = serde_json::json!(99));
    assert_eq!(load_source_decimation_index(&config), 0);
}

#[test]
fn source_dc_blocking_round_trip_and_default() {
    let config = make_config();
    assert!(load_source_dc_blocking(&config));
    save_source_dc_blocking(&config, false);
    assert!(!load_source_dc_blocking(&config));
    save_source_dc_blocking(&config, true);
    assert!(load_source_dc_blocking(&config));
    config.write(|v| v[KEY_SOURCE_DC_BLOCKING] = serde_json::json!("nope"));
    assert!(load_source_dc_blocking(&config));
}

#[test]
fn source_iq_correction_round_trip_and_default() {
    let config = make_config();
    assert!(!load_source_iq_correction(&config));
    save_source_iq_correction(&config, true);
    assert!(load_source_iq_correction(&config));
    config.write(|v| v[KEY_SOURCE_IQ_CORRECTION] = serde_json::json!("nope"));
    assert!(!load_source_iq_correction(&config));
}

#[test]
fn source_iq_inversion_round_trip_and_default() {
    let config = make_config();
    assert!(!load_source_iq_inversion(&config));
    save_source_iq_inversion(&config, true);
    assert!(load_source_iq_inversion(&config));
    config.write(|v| v[KEY_SOURCE_IQ_INVERSION] = serde_json::json!("nope"));
    assert!(!load_source_iq_inversion(&config));
}

#[test]
fn source_network_hostname_round_trip_and_default() {
    let config = make_config();
    assert_eq!(load_source_network_hostname(&config), "localhost");
    save_source_network_hostname(&config, "shack-pi.local");
    assert_eq!(load_source_network_hostname(&config), "shack-pi.local");
    config.write(|v| v[KEY_SOURCE_NETWORK_HOSTNAME] = serde_json::json!(42));
    assert_eq!(load_source_network_hostname(&config), "localhost");
}

#[test]
fn source_network_port_round_trip_and_default() {
    let config = make_config();
    assert_eq!(load_source_network_port(&config), 1234);
    save_source_network_port(&config, 8888);
    assert_eq!(load_source_network_port(&config), 8888);
    // out-of-range u16 falls back
    config.write(|v| v[KEY_SOURCE_NETWORK_PORT] = serde_json::json!(70_000));
    assert_eq!(load_source_network_port(&config), 1234);
    config.write(|v| v[KEY_SOURCE_NETWORK_PORT] = serde_json::json!("nope"));
    assert_eq!(load_source_network_port(&config), 1234);
}

#[test]
fn source_network_protocol_index_round_trip_and_default() {
    let config = make_config();
    assert_eq!(
        load_source_network_protocol_index(&config),
        NETWORK_PROTOCOL_TCPCLIENT_IDX
    );
    save_source_network_protocol_index(&config, NETWORK_PROTOCOL_UDP_IDX);
    assert_eq!(
        load_source_network_protocol_index(&config),
        NETWORK_PROTOCOL_UDP_IDX
    );
    config.write(|v| v[KEY_SOURCE_NETWORK_PROTOCOL_INDEX] = serde_json::json!("nope"));
    assert_eq!(
        load_source_network_protocol_index(&config),
        NETWORK_PROTOCOL_TCPCLIENT_IDX
    );
    // Out-of-range numeric falls back to TCP-client (idx 0).
    config.write(|v| v[KEY_SOURCE_NETWORK_PROTOCOL_INDEX] = serde_json::json!(42));
    assert_eq!(
        load_source_network_protocol_index(&config),
        NETWORK_PROTOCOL_TCPCLIENT_IDX
    );
}

#[test]
fn source_file_path_round_trip_and_default() {
    let config = make_config();
    assert_eq!(load_source_file_path(&config), "");
    save_source_file_path(&config, "/tmp/iq.wav");
    assert_eq!(load_source_file_path(&config), "/tmp/iq.wav");
    config.write(|v| v[KEY_SOURCE_FILE_PATH] = serde_json::json!(42));
    assert_eq!(load_source_file_path(&config), "");
}
