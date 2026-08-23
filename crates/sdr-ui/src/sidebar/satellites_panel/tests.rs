use super::*;
use chrono::TimeZone;

#[test]
fn defaults_lie_inside_spinrow_bounds() {
    // Belt-and-braces: if someone tweaks DEFAULT_STATION_*
    // without updating the bounds, the SpinRow would clamp
    // the seeded value silently. Pin the invariant.
    assert!((LAT_MIN_DEG..=LAT_MAX_DEG).contains(&DEFAULT_STATION_LAT_DEG));
    assert!((LON_MIN_DEG..=LON_MAX_DEG).contains(&DEFAULT_STATION_LON_DEG));
    assert!((ALT_MIN_M..=ALT_MAX_M).contains(&DEFAULT_STATION_ALT_M));
}

#[test]
fn read_f64_or_returns_default_for_missing_key() {
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({})));
    assert_eq!(read_f64_or(&cfg, "missing_key", 42.5), 42.5);
}

#[test]
fn read_f64_or_returns_default_for_wrong_type() {
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
        "wrong_type": "not a float",
    })));
    assert_eq!(read_f64_or(&cfg, "wrong_type", 42.5), 42.5);
}

#[test]
fn read_f64_or_returns_persisted_value() {
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
        "stored": 12.34,
    })));
    assert_eq!(read_f64_or(&cfg, "stored", 99.0), 12.34);
}

#[test]
fn read_bool_or_returns_persisted_value() {
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
        "auto": true,
    })));
    assert!(read_bool_or(&cfg, "auto", false));
}

#[test]
fn format_last_refresh_says_never_when_unset() {
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({})));
    assert_eq!(format_last_refresh(&cfg), "Never");
}

#[test]
fn format_last_refresh_renders_rfc3339_in_local_time() {
    // 2024-06-15T18:30:00Z is unambiguous; we don't pin the
    // local-formatted string (it depends on the test runner's
    // TZ). Pin two timezone-independent invariants instead:
    //
    //   1. The output isn't the literal "Never" placeholder.
    //   2. The output isn't the raw RFC3339 string — i.e. byte
    //      10 is the date-vs-time separator, and the formatter
    //      uses a space (`%Y-%m-%d %H:%M %Z`) where RFC3339
    //      uses `T`. Checking *that specific position* avoids
    //      false negatives from timezone abbreviations like
    //      `UTC` / `EDT` / `PDT` that happen to contain a `T`.
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
        "sat_tle_last_refresh": "2024-06-15T18:30:00Z",
    })));
    let formatted = format_last_refresh(&cfg);
    assert_ne!(formatted, "Never");
    assert_eq!(
        formatted.chars().nth(10),
        Some(' '),
        "expected a space at position 10 (date/time separator); got: {formatted}",
    );
}

#[test]
fn format_last_refresh_falls_back_to_raw_on_parse_error() {
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
        "sat_tle_last_refresh": "garbage timestamp",
    })));
    assert_eq!(format_last_refresh(&cfg), "garbage timestamp");
}

#[test]
fn watched_satellites_round_trip() {
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({})));
    // Empty initial state.
    assert!(load_watched_satellites(&cfg).is_empty());
    // Save → load round-trip.
    let mut watched = std::collections::HashSet::new();
    watched.insert(33591u32); // NOAA 19
    watched.insert(40069u32); // METEOR-M2 2
    save_watched_satellites(&cfg, &watched);
    let loaded = load_watched_satellites(&cfg);
    assert_eq!(loaded, watched);
}

#[test]
fn watched_satellites_load_skips_invalid_entries() {
    // Mixed-type array — strings / out-of-u32-range numbers
    // get silently dropped; the valid u32s survive.
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
        "sat_watched_norad_ids": [33591, "junk", 9_999_999_999_u64, 40069],
    })));
    let loaded = load_watched_satellites(&cfg);
    let expected: std::collections::HashSet<u32> = [33591u32, 40069u32].into_iter().collect();
    assert_eq!(loaded, expected);
}

#[test]
fn notify_lead_min_default_when_unset() {
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({})));
    assert_eq!(
        load_notify_lead_min(&cfg),
        super::super::satellites_notify::DEFAULT_NOTIFY_LEAD_MIN
    );
}

#[test]
fn notify_lead_min_clamps_out_of_range() {
    // 0 is below NOTIFY_LEAD_MIN_LOWER (1) — clamps up.
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
        "sat_notify_lead_min": 0,
    })));
    assert_eq!(
        load_notify_lead_min(&cfg),
        super::super::satellites_notify::NOTIFY_LEAD_MIN_LOWER
    );
    // 999 is above NOTIFY_LEAD_MIN_UPPER — clamps down.
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
        "sat_notify_lead_min": 999,
    })));
    assert_eq!(
        load_notify_lead_min(&cfg),
        super::super::satellites_notify::NOTIFY_LEAD_MIN_UPPER
    );
}

#[test]
fn notify_lead_min_round_trip() {
    let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({})));
    save_notify_lead_min(&cfg, 12);
    assert_eq!(load_notify_lead_min(&cfg), 12);
}

fn synthetic_pass(now: DateTime<Utc>, offset_min: i64) -> Pass {
    // Default to METEOR-M2 3 — historically this helper used
    // "NOAA 19" but NOAA-15/18/19 were decommissioned in August
    // 2025 and removed from `KNOWN_SATELLITES`. METEOR-M2 3 is
    // an active LRPT satellite still in the catalog. The pass
    // geometry below (start/end/elevation/azimuths) is
    // satellite-agnostic — these tests exercise the panel's
    // formatting + state, not orbital mechanics.
    let start = now + ChronoDuration::minutes(offset_min);
    Pass {
        satellite: "METEOR-M2 3".to_string(),
        start,
        end: start + ChronoDuration::minutes(12),
        max_elevation_deg: 56.0,
        max_el_time: start + ChronoDuration::minutes(6),
        start_az_deg: 245.0,
        end_az_deg: 105.0,
        tle_age: chrono::Duration::zero(),
    }
}

#[test]
fn format_pass_subtitle_shows_elevation_and_azimuths() {
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_pass(now, 30);
    let subtitle = format_pass_subtitle(&pass);
    assert!(subtitle.contains("max el 56"));
    assert!(subtitle.contains("AOS 245"));
    assert!(subtitle.contains("LOS 105"));
}

#[test]
fn format_pass_subtitle_includes_quality_tag_and_downlink() {
    // METEOR-M2 3 with 56° peak is a "winner" tier pass;
    // downlink is 137.900 MHz from the catalog.
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_pass(now, 30);
    let subtitle = format_pass_subtitle(&pass);
    assert!(subtitle.contains("winner"), "subtitle: {subtitle}");
    assert!(subtitle.contains("137.900 MHz"), "subtitle: {subtitle}");
}

/// #718 — a pass predicted from stale elements says so, so a
/// normal-looking list after three weeks offline is not mistaken
/// for a trustworthy one.
#[test]
fn format_pass_subtitle_flags_stale_tle_age() {
    let mut pass = synthetic_pass(Utc::now(), 30);
    pass.tle_age = sdr_sat::TLE_WARN_AGE + ChronoDuration::days(2);
    let subtitle = format_pass_subtitle(&pass);
    assert!(
        subtitle.ends_with("TLE 16 d old"),
        "stale elements must be flagged, got: {subtitle}"
    );
    pass.tle_age = ChronoDuration::days(3);
    assert!(
        !format_pass_subtitle(&pass).contains("TLE"),
        "fresh elements are not flagged"
    );
}

#[test]
fn format_pass_subtitle_falls_back_when_satellite_not_in_catalog() {
    // A pass for a satellite the panel doesn't know about (user
    // has manually loaded a TLE, future) — subtitle still works,
    // just without the freq. The fallback is still
    // `quality · geometry`, NOT geometry-only — assert all three
    // invariants so a regression that drops the quality tag in
    // the off-catalog branch trips here rather than reaching the
    // user as an inconsistent row.
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let mut pass = synthetic_pass(now, 30);
    pass.satellite = "FAKESAT-7".to_string();
    let subtitle = format_pass_subtitle(&pass);
    assert!(subtitle.contains("winner"), "subtitle: {subtitle}");
    assert!(subtitle.contains("max el 56"), "subtitle: {subtitle}");
    assert!(!subtitle.contains("MHz"), "subtitle: {subtitle}");
}

#[test]
fn pass_quality_label_pins_boundary_values() {
    // Boundary table: the threshold value itself takes the
    // higher tier (`>=`), one tick below drops to the next.
    assert_eq!(pass_quality_label(60.0), "winner");
    assert_eq!(pass_quality_label(40.0), "winner");
    assert_eq!(pass_quality_label(39.9), "good");
    assert_eq!(pass_quality_label(25.0), "good");
    assert_eq!(pass_quality_label(24.9), "marginal");
    assert_eq!(pass_quality_label(15.0), "marginal");
    assert_eq!(pass_quality_label(14.9), "barely");
    assert_eq!(pass_quality_label(5.0), "barely");
}

#[test]
fn format_downlink_mhz_renders_three_decimals_minimum() {
    // 137.100 MHz reads as "137.100", not "137.1" — the panel
    // wants every entry to line up visually.
    assert_eq!(format_downlink_mhz(137_100_000), "137.100 MHz");
    assert_eq!(format_downlink_mhz(145_800_000), "145.800 MHz");
}

#[test]
fn format_downlink_mhz_preserves_extra_precision_when_needed() {
    // NOAA 18 is on 137.9125 MHz exactly — the formatter must
    // not round to 3 decimals and lose the off-channel offset.
    assert_eq!(format_downlink_mhz(137_912_500), "137.9125 MHz");
}

#[test]
fn downlink_hz_for_pass_finds_catalog_entry() {
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_pass(now, 30); // satellite = "METEOR-M2 3"
    assert_eq!(downlink_hz_for_pass(&pass), Some(137_900_000));
}

#[test]
fn downlink_hz_for_pass_returns_none_for_unknown_satellite() {
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let mut pass = synthetic_pass(now, 30);
    pass.satellite = "MYSTERY-SAT".to_string();
    assert_eq!(downlink_hz_for_pass(&pass), None);
}

#[test]
fn tune_target_for_pass_returns_full_tuning_quintuple() {
    // Pin the (downlink_hz, demod_mode, bandwidth_hz,
    // imaging_protocol, norad_id) quintuple for a known catalog
    // entry. A future refactor that splits or reorders the
    // tuple — or that drifts the catalog values — fails here
    // before reaching the play-button wiring layer or the
    // recorder. NORAD id is threaded out of the catalog so the
    // recorder's `Action::StartAutoRecord` can carry it without
    // a name → catalog re-lookup at the wiring layer (per CR
    // round 3 on PR #571).
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_pass(now, 30); // satellite = "METEOR-M2 3"
    let target = tune_target_for_pass(&pass).expect("METEOR-M2 3 is in catalog");
    assert_eq!(target.0, 137_900_000);
    assert_eq!(target.1, sdr_types::DemodMode::Lrpt);
    assert_eq!(target.2, 144_000); // LRPT — matches IF rate so VFO bypasses channel filter
    assert_eq!(target.3, Some(sdr_sat::ImagingProtocol::Lrpt));
    assert_eq!(target.4, 57_166); // METEOR-M2 3 NORAD ID
}

#[test]
fn tune_target_for_pass_returns_lrpt_protocol_for_meteor() {
    // Per epic #469 task 7, METEOR-M 2 / METEOR-M2 3 are now
    // flagged `Some(ImagingProtocol::Lrpt)` in the catalog so
    // the recorder enrolls them in the auto-record flow. The
    // play button still uses the same `tune_target_for_pass`
    // path; the protocol field is only consumed by the
    // recorder, so this test pins the catalog→LRPT routing.
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let mut pass = synthetic_pass(now, 30);
    pass.satellite = "METEOR-M2 3".to_string();
    let target = tune_target_for_pass(&pass).expect("METEOR-M 2 is in catalog");
    assert_eq!(
        target.3,
        Some(sdr_sat::ImagingProtocol::Lrpt),
        "Meteor protocol must be Lrpt after epic #469 task 7",
    );
}

#[test]
fn tune_target_for_pass_returns_none_for_unknown_satellite() {
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let mut pass = synthetic_pass(now, 30);
    pass.satellite = "MYSTERY-SAT".to_string();
    assert!(tune_target_for_pass(&pass).is_none());
}

#[test]
fn format_pass_title_uses_h_m_for_far_passes() {
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_pass(now, 75); // 1 h 15 min away
    let title = format_pass_title(&pass, now);
    assert_eq!(title, "METEOR-M2 3 — in 1h 15m");
}

#[test]
fn format_pass_title_uses_minutes_for_near_passes() {
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_pass(now, 12); // 12 min away
    assert_eq!(format_pass_title(&pass, now), "METEOR-M2 3 — in 12 min");
}

#[test]
fn format_pass_title_says_starting_now_inside_one_minute() {
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    // Pass starts in 30 seconds.
    let pass = Pass {
        satellite: "METEOR-M2 3".to_string(),
        start: now + ChronoDuration::seconds(30),
        end: now + ChronoDuration::minutes(12),
        max_elevation_deg: 50.0,
        max_el_time: now + ChronoDuration::minutes(5),
        start_az_deg: 0.0,
        end_az_deg: 0.0,
        tle_age: chrono::Duration::zero(),
    };
    assert_eq!(format_pass_title(&pass, now), "METEOR-M2 3 — starting now");
}

#[test]
fn format_pass_title_says_in_progress_for_active_passes() {
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    // Started 3 minutes ago, ends in 9.
    let pass = synthetic_pass(now, -3);
    let title = format_pass_title(&pass, now);
    assert!(
        title.contains("in progress"),
        "expected 'in progress', got {title:?}"
    );
    assert!(
        title.contains("3 min in"),
        "expected '3 min in', got {title:?}"
    );
}

#[test]
fn format_pass_title_says_ended_after_los() {
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    // Pass ended 30 minutes ago.
    let pass = synthetic_pass(now, -42);
    assert_eq!(format_pass_title(&pass, now), "METEOR-M2 3 — ended");
}

#[test]
fn format_pass_title_at_exact_one_hour_uses_h_m_format() {
    // Boundary: a pass starting in exactly 60 min should read
    // "in 1h 00m", not "in 60 min". The strict `>` version of
    // this code surfaced the latter — fixed via `>=`.
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_pass(now, 60);
    assert_eq!(format_pass_title(&pass, now), "METEOR-M2 3 — in 1h 00m");
}

#[test]
fn format_pass_title_at_exact_one_minute_says_one_min() {
    // Boundary: a pass starting in exactly 60 s should read
    // "in 1 min", not "starting now". `>=` fixes this.
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_pass(now, 1);
    assert_eq!(format_pass_title(&pass, now), "METEOR-M2 3 — in 1 min");
}

#[test]
fn format_pass_title_clamps_in_progress_min_to_at_least_one() {
    // First 60 seconds of an active pass: floor-div would say
    // "0 min in", which reads like the pass hasn't started.
    // Clamp to a minimum of 1 so the user always sees a real
    // count.
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    // Pass started 30 seconds ago, ends in 12 minutes.
    let pass = Pass {
        satellite: "METEOR-M2 3".to_string(),
        start: now - ChronoDuration::seconds(30),
        end: now + ChronoDuration::minutes(12),
        max_elevation_deg: 45.0,
        max_el_time: now + ChronoDuration::minutes(5),
        start_az_deg: 0.0,
        end_az_deg: 0.0,
        tle_age: chrono::Duration::zero(),
    };
    assert_eq!(
        format_pass_title(&pass, now),
        "METEOR-M2 3 — in progress (1 min in)"
    );
}

fn make_config() -> Arc<ConfigManager> {
    Arc::new(ConfigManager::in_memory(&serde_json::json!({})))
}

#[test]
fn load_doppler_tracking_enabled_defaults_to_on() {
    let config = make_config();
    // Spec §7.1: default ON so fresh installs get auto-
    // correction without user discovery.
    assert!(load_doppler_tracking_enabled(&config));
}

#[test]
fn save_and_load_doppler_tracking_enabled_round_trip() {
    let config = make_config();
    save_doppler_tracking_enabled(&config, false);
    assert!(!load_doppler_tracking_enabled(&config));
    save_doppler_tracking_enabled(&config, true);
    assert!(load_doppler_tracking_enabled(&config));
}

#[test]
fn load_doppler_tracking_enabled_tolerates_non_bool() {
    let config = make_config();
    config.write(|v| {
        v[KEY_DOPPLER_TRACKING_ENABLED] = serde_json::json!("not a bool");
    });
    // Falls back to the default (true), not a panic.
    assert!(load_doppler_tracking_enabled(&config));
}

// ─── AutoRecordQuality (#511) ──────────────────────────────────

#[test]
fn auto_record_quality_min_elev_deg_matches_canonical_constants() {
    // Pin the four thresholds against the canonical
    // `QUALITY_*_DEG` constants the per-pass quality tag also
    // reads, so the combo's gate and the row subtitle's tier
    // name (`winner`/`good`/`marginal`/`barely`) can never
    // numerically drift.
    assert!(
        (AutoRecordQuality::WinnersOnly.min_elev_deg() - QUALITY_WINNER_DEG).abs() < f64::EPSILON
    );
    assert!(
        (AutoRecordQuality::WinnersAndGood.min_elev_deg() - QUALITY_GOOD_DEG).abs() < f64::EPSILON
    );
    assert!(
        (AutoRecordQuality::MarginalOrBetter.min_elev_deg() - QUALITY_MARGINAL_DEG).abs()
            < f64::EPSILON
    );
    assert!(
        (AutoRecordQuality::AllPasses.min_elev_deg() - MIN_PASS_ELEVATION_DEG).abs() < f64::EPSILON
    );
}

#[test]
fn auto_record_quality_default_is_winners_and_good() {
    assert_eq!(
        AutoRecordQuality::DEFAULT,
        AutoRecordQuality::WinnersAndGood
    );
    // Critical: the default's threshold must equal the previously-
    // hardcoded `AUTO_RECORD_MIN_ELEV_DEG = 25.0` so existing
    // users get the same behavior on upgrade. Per #511.
    assert!((AutoRecordQuality::DEFAULT.min_elev_deg() - QUALITY_GOOD_DEG).abs() < f64::EPSILON);
}

#[test]
fn auto_record_quality_from_index_round_trips() {
    for variant in AutoRecordQuality::ALL {
        assert_eq!(AutoRecordQuality::from_index(variant.to_index()), variant);
    }
}

#[test]
fn auto_record_quality_from_index_oob_falls_back_to_default() {
    assert_eq!(
        AutoRecordQuality::from_index(99),
        AutoRecordQuality::DEFAULT
    );
    assert_eq!(
        AutoRecordQuality::from_index(u32::MAX),
        AutoRecordQuality::DEFAULT
    );
}

#[test]
fn auto_record_quality_all_indices_are_unique_and_sequential() {
    // Sanity check on the ALL ordering — if a future maintainer
    // accidentally adds a duplicate, the round-trip test would
    // miss it but this catches it directly.
    let indices: Vec<u32> = AutoRecordQuality::ALL
        .iter()
        .map(|v| v.to_index())
        .collect();
    assert_eq!(indices, vec![0, 1, 2, 3]);
}

#[test]
fn auto_record_quality_display_label_numbers_match_min_elev_deg() {
    // `display_label` returns `&'static str` to keep the
    // `gtk4::StringList::new` call cheap and allocation-free,
    // so we can't drive the labels' text directly off the
    // `QUALITY_*_DEG` constants. Instead, this test pins that
    // the integer floor in each label matches the tier's
    // `min_elev_deg`. If anyone bumps a constant without
    // updating the label string this fails. Per CR round 1
    // on PR #574.
    for tier in AutoRecordQuality::ALL {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "min_elev_deg returns positive f64 in the 0..50 range — fits i64 cleanly"
        )]
        let n = tier.min_elev_deg() as i64;
        let needle = format!("≥ {n}°");
        let label = tier.display_label();
        assert!(
            label.contains(&needle),
            "label `{label}` should contain `{needle}` to match min_elev_deg",
        );
    }
}

#[test]
fn auto_record_quality_save_load_round_trips() {
    let config = make_config();
    for quality in AutoRecordQuality::ALL {
        save_auto_record_quality(&config, quality);
        assert_eq!(load_auto_record_quality(&config), quality);
    }
}

// ─── Composites toggle (#547) ──────────────────────────────────

#[test]
fn auto_record_composites_default_is_false() {
    // Opt-in: a fresh install must NOT start writing extra
    // composite PNGs without the user asking. Pin the
    // default so a future read-helper refactor can't flip it
    // accidentally. Per #547.
    let config = make_config();
    assert!(!load_auto_record_composites(&config));
}

#[test]
fn auto_record_composites_save_load_round_trips() {
    let config = make_config();
    save_auto_record_composites(&config, true);
    assert!(load_auto_record_composites(&config));
    save_auto_record_composites(&config, false);
    assert!(!load_auto_record_composites(&config));
}
