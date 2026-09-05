use super::*;

// Real Orbcomm TLEs (Celestrak, epoch 2026-248) — same fixtures as
// `sdr_sat::identify::tests`, reused here since `collect_orbcomm_passes`
// needs propagatable candidates rather than the identification path's
// ECEF targets.
const FM06: (&str, &str, &str) = (
    "ORBCOMM FM06",
    "1 25118U 97084G   26248.14598269  .00000608  00000+0  20228-3 0  9991",
    "2 25118  45.0146  27.7326 0001119 213.3371 317.0700 14.47727064505974",
);
const FM04: (&str, &str, &str) = (
    "ORBCOMM FM04",
    "1 25159U 98007C   26248.16811710  .00000396  00000+0  18072-3 0  9991",
    "2 25159 107.9604 334.0349 0041099 188.9146 171.1268 14.35849962488071",
);

fn sat(t: (&str, &str, &str)) -> sdr_sat::Satellite {
    sdr_sat::Satellite::from_tle(t.0, t.1, t.2).expect("valid TLE fixture")
}

/// `collect_orbcomm_passes` loops every candidate through
/// `upcoming_passes`, merges the results, and returns them sorted by
/// AOS regardless of per-candidate order. Mirrors
/// `satellites_panel::passes::enumerate_upcoming_passes` but over an
/// explicit candidate list rather than `KNOWN_SATELLITES`.
#[test]
fn collect_orbcomm_passes_sorted_by_start() {
    use chrono::TimeZone;

    let station = sdr_sat::GroundStation::new(37.1353, -80.4188, 660.0);
    let cands = vec![
        ("ORBCOMM FM06".to_string(), sat(FM06)),
        ("ORBCOMM FM04".to_string(), sat(FM04)),
    ];
    let from = chrono::Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let passes = passes::collect_orbcomm_passes(&station, &cands, from, 8, 10.0);

    assert!(
        !passes.is_empty(),
        "expected at least one pass across two Orbcomm candidates in an 8h window"
    );
    for w in passes.windows(2) {
        assert!(w[0].start <= w[1].start);
    }
}

/// The cap is enforced even when the candidate list would otherwise
/// produce more passes than `MAX_ORBCOMM_PASSES`.
#[test]
fn collect_orbcomm_passes_truncates_to_max() {
    use chrono::TimeZone;

    let station = sdr_sat::GroundStation::new(37.1353, -80.4188, 660.0);
    // Duplicate the same two real candidates several times over — each
    // duplicate propagates identically, so a long-enough window
    // reliably produces more raw passes than the cap.
    let mut cands = Vec::new();
    for i in 0..8 {
        cands.push((format!("ORBCOMM FM06 #{i}"), sat(FM06)));
        cands.push((format!("ORBCOMM FM04 #{i}"), sat(FM04)));
    }
    let from = chrono::Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let passes = passes::collect_orbcomm_passes(&station, &cands, from, 48, 5.0);

    assert!(passes.len() <= passes::MAX_ORBCOMM_PASSES);
}

#[test]
fn log_ring_caps_at_max_entries() {
    let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for i in 0..(MAX_LOG_ENTRIES + 10) {
        push_log_ring(&mut ring, format!("line {i}"));
    }
    assert_eq!(ring.len(), MAX_LOG_ENTRIES);
    assert_eq!(ring.front().unwrap(), &format!("line {}", 10)); // oldest 10 dropped
}

#[test]
fn push_log_ring_returns_evicted_entry() {
    let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for i in 0..MAX_LOG_ENTRIES {
        assert!(push_log_ring(&mut ring, format!("line {i}")).is_empty());
    }
    // At cap: one more push evicts exactly the oldest entry, returned whole.
    let evicted = push_log_ring(&mut ring, "newest".to_string());
    assert_eq!(evicted, vec!["line 0".to_string()]);
    assert_eq!(ring.len(), MAX_LOG_ENTRIES);
}

#[test]
fn evicted_multiline_entry_is_returned_intact_for_full_line_deletion() {
    // A MessageComplete hexdump entry is ONE ring entry but spans several
    // buffer lines. When it reaches the front and is evicted it must be
    // returned whole so append_log_entry deletes all of its buffer lines
    // (not just the first) — this is the buffer/ring-desync regression.
    let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let multiline = "Message complete\n00000000  DE AD BE EF |....|".to_string();
    assert!(push_log_ring(&mut ring, multiline.clone()).is_empty()); // front entry
    for i in 0..(MAX_LOG_ENTRIES - 1) {
        assert!(push_log_ring(&mut ring, format!("line {i}")).is_empty());
    }
    assert_eq!(ring.len(), MAX_LOG_ENTRIES);
    // One more push evicts the multi-line front entry, returned intact.
    let evicted = push_log_ring(&mut ring, "tail".to_string());
    assert_eq!(evicted, vec![multiline.clone()]);
    // It occupies 2 buffer lines — the count append_log_entry deletes.
    assert_eq!(entry_buffer_lines(&evicted[0]), 2);
}

#[test]
fn entry_buffer_lines_counts_all_lines() {
    assert_eq!(entry_buffer_lines("single"), 1);
    assert_eq!(entry_buffer_lines("a\nb\nc"), 3);
}

fn dummy_pass(start: chrono::DateTime<chrono::Utc>) -> sdr_sat::Pass {
    sdr_sat::Pass {
        satellite: "ORBCOMM FM06".to_string(),
        start,
        end: start + chrono::Duration::minutes(10),
        max_elevation_deg: 42.0,
        max_el_time: start + chrono::Duration::minutes(5),
        start_az_deg: 10.0,
        end_az_deg: 200.0,
        tle_age: chrono::Duration::hours(1),
    }
}

/// A pass whose local start date is today gets no weekday prefix.
#[test]
fn format_orbcomm_pass_row_no_weekday_prefix_for_today() {
    let today_local_noon = chrono::Local::now()
        .date_naive()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_local_timezone(chrono::Local)
        .unwrap()
        .with_timezone(&chrono::Utc);
    let pass = dummy_pass(today_local_noon);
    let (_, subtitle) = passes::format_orbcomm_pass_row(&pass, &HashMap::new());
    let start_local = pass.start.with_timezone(&chrono::Local);
    let end_local = pass.end.with_timezone(&chrono::Local);
    let expected = format!(
        "{}–{} (local) · {:.0}°",
        start_local.format("%H:%M"),
        end_local.format("%H:%M"),
        pass.max_elevation_deg,
    );
    assert_eq!(subtitle, expected);
}

/// A pass whose local start date is NOT today is prefixed with a
/// weekday abbreviation so a 24h-lookahead pass spanning a local date
/// boundary isn't ambiguous (`CodeRabbit` review on #903).
#[test]
fn format_orbcomm_pass_row_adds_weekday_prefix_for_other_day() {
    let two_days_out = chrono::Utc::now() + chrono::Duration::days(2);
    let pass = dummy_pass(two_days_out);
    let (_, subtitle) = passes::format_orbcomm_pass_row(&pass, &HashMap::new());
    let start_local = pass.start.with_timezone(&chrono::Local);
    let expected_prefix = format!("{} ", start_local.format("%a"));
    assert!(
        subtitle.starts_with(&expected_prefix),
        "expected subtitle {subtitle:?} to start with weekday prefix {expected_prefix:?}"
    );
}
