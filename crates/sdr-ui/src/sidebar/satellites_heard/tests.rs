use std::time::{Duration, Instant};

use super::*;

#[test]
fn record_and_rows_round_trip() {
    let mut model = HeardSatellites::new();
    let now = Instant::now();
    model.record(0x2C, Some((51.2, 7.4, 715_000.0)), None, None, now);

    let rows = model.rows(now);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, sdr_orbcomm::sat_names::sat_label(0x2C));
    assert_eq!(rows[0].age_secs, 0);
    assert_eq!(rows[0].position, Some((51.2, 7.4, 715_000.0)));
}

#[test]
fn sync_after_ephemeris_keeps_position_and_refreshes_age() {
    let mut model = HeardSatellites::new();
    let t0 = Instant::now();
    model.record(0x2C, Some((51.2, 7.4, 715_000.0)), None, None, t0);

    // Before the Sync lands, age tracks the Ephemeris timestamp.
    let t_mid = t0 + Duration::from_secs(5);
    let rows_mid = model.rows(t_mid);
    assert_eq!(rows_mid[0].age_secs, 5);
    assert_eq!(rows_mid[0].position, Some((51.2, 7.4, 715_000.0)));

    // A Sync beacon (no position) lands 10 s after the Ephemeris.
    let t_sync = t0 + Duration::from_secs(10);
    model.record(0x2C, None, None, None, t_sync);

    // 2 s after the Sync: age must reflect the Sync (not the
    // original Ephemeris), and the position must be unchanged.
    let t_query = t0 + Duration::from_secs(12);
    let rows = model.rows(t_query);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].age_secs, 2);
    assert_eq!(rows[0].position, Some((51.2, 7.4, 715_000.0)));
}

#[test]
fn expiry_drops_old_entries() {
    let mut model = HeardSatellites::new();
    let t0 = Instant::now();
    model.record(0x05, None, None, None, t0);

    let just_inside = t0 + Duration::from_secs(HEARD_EXPIRY_SECS - 1);
    assert_eq!(model.rows(just_inside).len(), 1);

    let at_boundary = t0 + Duration::from_secs(HEARD_EXPIRY_SECS);
    assert!(model.rows(at_boundary).is_empty());

    let well_past = t0 + Duration::from_secs(HEARD_EXPIRY_SECS + 60);
    assert!(model.rows(well_past).is_empty());
}

#[test]
fn rows_sorted_most_recently_heard_first() {
    let mut model = HeardSatellites::new();
    let t0 = Instant::now();
    model.record(0x01, None, None, None, t0);
    let t1 = t0 + Duration::from_secs(30);
    model.record(0x02, None, None, None, t1);

    let rows = model.rows(t1);

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].label, sdr_orbcomm::sat_names::sat_label(0x02));
    assert_eq!(rows[1].label, sdr_orbcomm::sat_names::sat_label(0x01));
}

#[test]
fn two_satellites_tracked_independently() {
    let mut model = HeardSatellites::new();
    let now = Instant::now();
    model.record(0x01, Some((10.0, 20.0, 700_000.0)), None, None, now);
    model.record(0x02, Some((-5.0, -30.0, 720_000.0)), None, None, now);

    let rows = model.rows(now);

    assert_eq!(rows.len(), 2);
    let sat_01 = rows
        .iter()
        .find(|r| r.label == sdr_orbcomm::sat_names::sat_label(0x01))
        .expect("sat 0x01 row present");
    let sat_02 = rows
        .iter()
        .find(|r| r.label == sdr_orbcomm::sat_names::sat_label(0x02))
        .expect("sat 0x02 row present");
    assert_eq!(sat_01.position, Some((10.0, 20.0, 700_000.0)));
    assert_eq!(sat_02.position, Some((-5.0, -30.0, 720_000.0)));
}

#[test]
fn ephemeris_record_retains_velocity_and_sat_time() {
    let now = Instant::now();
    let mut heard = HeardSatellites::new();
    heard.record(
        0x2C,
        Some((51.2, 7.4, 715_000.0)),
        Some(7450.0),
        Some(1_600_000_000),
        now,
    );
    let rows = heard.rows(now);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].vel_ms, Some(7450.0));
    assert_eq!(rows[0].sat_time_unix, Some(1_600_000_000));
}

#[test]
fn sync_only_record_leaves_velocity_and_time_none() {
    let now = Instant::now();
    let mut heard = HeardSatellites::new();
    heard.record(0x2C, None, None, None, now);
    let rows = heard.rows(now);
    assert_eq!(rows[0].vel_ms, None);
    assert_eq!(rows[0].sat_time_unix, None);
    assert_eq!(rows[0].position, None);
}

#[test]
fn sync_after_ephemeris_preserves_last_velocity_and_time() {
    let now = Instant::now();
    let mut heard = HeardSatellites::new();
    heard.record(
        0x2C,
        Some((1.0, 2.0, 700_000.0)),
        Some(7400.0),
        Some(111),
        now,
    );
    heard.record(0x2C, None, None, None, now); // Sync beacon after a fix
    let rows = heard.rows(now);
    assert_eq!(rows[0].vel_ms, Some(7400.0));
    assert_eq!(rows[0].sat_time_unix, Some(111));
    assert_eq!(rows[0].position, Some((1.0, 2.0, 700_000.0)));
}
