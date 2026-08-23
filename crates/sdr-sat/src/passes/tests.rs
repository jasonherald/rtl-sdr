use super::*;
use chrono::TimeZone;

/// Vallado TC0 reference TLE — same as in [`crate::sgp4_core`] tests.
/// Vanguard 1, NORAD 5, epoch 2000-06-27 18:50:19 UTC. Ships in
/// every SGP4 implementation as a sanity-check vector.
const TEST_TLE_NAME: &str = "VANGUARD 1";
const TEST_TLE_LINE1: &str =
    "1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753";
const TEST_TLE_LINE2: &str =
    "2 00005  34.2682 348.7242 1859667 331.7664  19.3264 10.82419157413667";

/// Mid-latitude US station — pinned so pass-count tests are
/// reproducible. (40°N 74°W is roughly Princeton, NJ.)
const TEST_STATION_LAT: f64 = 40.0;
const TEST_STATION_LON: f64 = -74.0;
const TEST_STATION_ALT_M: f64 = 50.0;

/// 5° minimum elevation — the standard "useful pass" cutoff for
/// LEO weather work; below 5° the signal usually has too much
/// horizon attenuation to decode anything.
const TEST_MIN_ELEVATION_DEG: f64 = 5.0;

fn test_satellite() -> Satellite {
    Satellite::from_tle(TEST_TLE_NAME, TEST_TLE_LINE1, TEST_TLE_LINE2).unwrap()
}

fn test_station() -> GroundStation {
    GroundStation::new(TEST_STATION_LAT, TEST_STATION_LON, TEST_STATION_ALT_M)
}

/// Tolerance on the refined boundary elevation: the bisection stops
/// at `REFINE_PRECISION` (1 s), during which a LEO bird moves well
/// under 0.1°, so both edges must sit within that of the threshold.
const BOUNDARY_EL_TOL_DEG: f64 = 0.1;

/// #716 — both AOS *and* LOS must be refined to the threshold crossing.
/// The bisection assumed a rising edge, so LOS collapsed to an end of
/// the 60 s coarse bucket (up to a minute early or late).
#[test]
fn pass_boundaries_sit_at_the_elevation_threshold() {
    let sat = test_satellite();
    let station = test_station();
    let from = sat.epoch();
    let to = from + chrono::Duration::hours(24);
    let passes = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG).unwrap();
    assert!(!passes.is_empty(), "expected at least one pass in 24 h");
    for p in &passes {
        if p.end == to {
            continue; // clamped, not refined
        }
        let el_start = track(&station, &sat, p.start).unwrap().elevation_deg;
        let el_end = track(&station, &sat, p.end).unwrap().elevation_deg;
        assert!(
            (el_start - TEST_MIN_ELEVATION_DEG).abs() < BOUNDARY_EL_TOL_DEG,
            "AOS not at threshold: el = {el_start:.3}"
        );
        assert!(
            (el_end - TEST_MIN_ELEVATION_DEG).abs() < BOUNDARY_EL_TOL_DEG,
            "LOS not at threshold: el = {el_end:.3} (pass {} → {})",
            p.start,
            p.end
        );
    }
}

/// #717 — a non-finite or out-of-range station must be an error, not
/// a fabricated 90°-elevation pass.
#[test]
fn track_rejects_invalid_station() {
    let sat = test_satellite();
    for station in [
        GroundStation::new(f64::NAN, -74.0, 0.0),
        GroundStation::new(40.0, f64::INFINITY, 0.0),
        GroundStation::new(91.0, -74.0, 0.0),
        GroundStation::new(40.0, 181.0, 0.0),
        GroundStation::new(40.0, -74.0, f64::NAN),
    ] {
        assert!(
            matches!(
                track(&station, &sat, sat.epoch()),
                Err(SatelliteError::InvalidStation { .. })
            ),
            "station {station:?} must be rejected"
        );
    }
    assert!(
        matches!(
            upcoming_passes(
                &GroundStation::new(f64::NAN, f64::NAN, 0.0),
                &sat,
                sat.epoch(),
                sat.epoch() + chrono::Duration::hours(24),
                TEST_MIN_ELEVATION_DEG
            ),
            Err(SatelliteError::InvalidStation { .. })
        ),
        "an invalid station must be an error, not an empty pass list"
    );
}

#[test]
fn ground_station_try_new_validates() {
    assert!(GroundStation::try_new(40.0, -74.0, 50.0).is_ok());
    assert!(GroundStation::try_new(f64::NAN, -74.0, 50.0).is_err());
    assert!(GroundStation::try_new(-90.5, 0.0, 0.0).is_err());
    assert!(GroundStation::try_new(0.0, 180.5, 0.0).is_err());
    assert!(GroundStation::try_new(0.0, 0.0, f64::INFINITY).is_err());
}

#[test]
fn track_at_epoch_returns_finite_values() {
    let sat = test_satellite();
    let station = test_station();
    let t = track(&station, &sat, sat.epoch()).unwrap();
    assert!(t.azimuth_deg.is_finite() && (0.0..360.0).contains(&t.azimuth_deg));
    assert!(t.elevation_deg.is_finite() && (-90.0..=90.0).contains(&t.elevation_deg));
    assert!(t.range_km.is_finite() && t.range_km > 0.0);
    assert!(t.range_rate_km_s.is_finite());
}

#[test]
fn doppler_shift_sign_matches_range_rate() {
    // Construct a Track by hand to test the formula in isolation —
    // doesn't depend on SGP4 details.
    let approaching = Track {
        azimuth_deg: 0.0,
        elevation_deg: 30.0,
        range_km: 1_000.0,
        range_rate_km_s: -5.0, // moving toward station
        when: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
    };
    let receding = Track {
        range_rate_km_s: 5.0, // moving away
        ..approaching
    };
    let f = 137.5e6; // APT carrier
    let shift_in = approaching.doppler_shift_hz(f);
    let shift_out = receding.doppler_shift_hz(f);
    assert!(shift_in > 0.0, "approaching = blueshift, got {shift_in}");
    assert!(shift_out < 0.0, "receding = redshift, got {shift_out}");
    // Magnitudes should match.
    assert!((shift_in + shift_out).abs() < 1e-6);
}

#[test]
fn upcoming_passes_finds_passes_in_a_one_day_window() {
    // For a TLE epoch + 1-day window, expect at least a few passes —
    // any LEO satellite has 12–15 orbits/day, of which several are
    // visible from a fixed station.
    let sat = test_satellite();
    let station = test_station();
    let from = sat.epoch();
    let to = from + Duration::days(1);
    let passes = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG).unwrap();
    assert!(!passes.is_empty(), "expected ≥ 1 pass in 24 h");
    // Vanguard 1 has 10.82 orbits/day; ~1/3 of those will be
    // visible above 5° from a single mid-lat station, so 1–6
    // passes is the realistic window.
    assert!(
        passes.len() <= 8,
        "implausibly many passes ({}) — coarse-step bug?",
        passes.len(),
    );
}

#[test]
fn each_pass_is_self_consistent() {
    let sat = test_satellite();
    let station = test_station();
    let from = sat.epoch();
    let to = from + Duration::days(1);
    let passes = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG).unwrap();
    for p in &passes {
        // Time ordering.
        assert!(p.start < p.end, "pass {p:?}: start ≥ end");
        assert!(
            p.start <= p.max_el_time && p.max_el_time <= p.end,
            "pass {p:?}: max_el_time outside [start, end]",
        );
        // Elevation plausibility.
        assert!(
            (TEST_MIN_ELEVATION_DEG..=90.0).contains(&p.max_elevation_deg),
            "pass {p:?}: max_elevation out of [min, 90°]",
        );
        // Azimuths in valid range.
        assert!((0.0..360.0).contains(&p.start_az_deg));
        assert!((0.0..360.0).contains(&p.end_az_deg));
        // Pass duration sanity: at least 30 seconds, less than the
        // satellite's orbital half-period. Round-orbit LEO sats
        // give 5–15 min passes; Vanguard 1's eccentricity (~0.19)
        // produces apogee dwells that can run 30–60 min — both
        // are physically valid. Tightening this would just chase
        // SGP4's eccentric-orbit edge cases. The 90-minute ceiling
        // catches genuine "off-by-orbit-period" bugs without
        // being orbit-shape-specific.
        let duration = p.end - p.start;
        assert!(
            duration > Duration::seconds(30) && duration < Duration::minutes(90),
            "pass {p:?}: implausible duration {duration:?}",
        );
        // Satellite name round-trips.
        assert_eq!(p.satellite, TEST_TLE_NAME);
    }
}

#[test]
fn upcoming_passes_returns_empty_for_zero_window() {
    let sat = test_satellite();
    let station = test_station();
    let t = sat.epoch();
    assert!(
        upcoming_passes(&station, &sat, t, t, TEST_MIN_ELEVATION_DEG)
            .unwrap()
            .is_empty()
    );
    assert!(
        upcoming_passes(
            &station,
            &sat,
            t,
            t - Duration::seconds(1),
            TEST_MIN_ELEVATION_DEG
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn is_ascending_returns_consistent_bool() {
    // Using Vanguard 1 — a real satellite — over time the sign
    // of the latitude derivative flips between ascending and
    // descending legs. We don't know which leg `epoch` falls
    // on without recomputing, so just verify:
    // (a) is_ascending returns a bool (no error) at epoch
    // (b) the bool flips sign somewhere over a half-orbit
    //     (indicating we're actually computing a meaningful
    //     latitude derivative, not always returning the same
    //     value)
    let sat = test_satellite();
    let epoch = sat.epoch();
    let v_at_epoch = is_ascending(&sat, epoch).unwrap();
    // Half an orbit later (Vanguard 1 has ~133 min period;
    // sampling at +/− 67 min from epoch should land on the
    // opposite leg).
    let half_period_min = 67;
    let v_half_later =
        is_ascending(&sat, epoch + chrono::Duration::minutes(half_period_min)).unwrap();
    assert_ne!(
        v_at_epoch, v_half_later,
        "is_ascending should flip across a half-orbit (epoch={v_at_epoch}, +{half_period_min}min={v_half_later}); \
         constant return suggests the latitude derivative isn't being computed",
    );
}

#[test]
fn is_ascending_handles_propagation_error() {
    // Sample very near `chrono::DateTime::<Utc>::MIN_UTC` so the
    // 30 s offset to `when + Δ` stays representable but SGP4 is
    // forced to extrapolate ~2000 years before any TLE epoch —
    // far enough that all SGP4 implementations either return an
    // error or produce nonsense. Either case must surface as an
    // `Err` from `is_ascending`, NOT a panic and NOT silently a
    // bool (the helper's contract per its own docstring).
    // Per CR round 1 on PR #571.
    let sat = test_satellite();
    let way_too_far = chrono::DateTime::<Utc>::MIN_UTC + chrono::Duration::seconds(60);
    let result = is_ascending(&sat, way_too_far);
    assert!(
        result.is_err(),
        "is_ascending at year ~0 should propagate an SGP4 error, got Ok({result:?})",
    );
}

// --- #718 / #719 / #720 (Aug 2026 deep review) ---

/// #719 — a window the propagator cannot cover is an error, not
/// an empty (quiet-sky) result and not a fabricated setting edge.
#[test]
fn upcoming_passes_reports_propagation_failure_instead_of_guessing() {
    let sat = test_satellite();
    let station = test_station();
    let from = chrono::DateTime::<Utc>::MIN_UTC + Duration::seconds(60);
    let to = from + Duration::hours(2);
    let result = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG);
    assert!(
        matches!(result, Err(SatelliteError::Propagation { .. })),
        "expected a propagation error, got {result:?}"
    );
}

/// #718 — elements older than `TLE_MAX_AGE` at the start of the
/// window are refused rather than propagated into confident
/// nonsense.
#[test]
fn upcoming_passes_refuses_an_expired_tle() {
    let sat = test_satellite();
    let station = test_station();
    let from = sat.epoch() + crate::sgp4_core::TLE_MAX_AGE + Duration::days(1);
    let to = from + Duration::days(1);
    let result = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG);
    assert!(
        matches!(result, Err(SatelliteError::TleExpired { .. })),
        "expected TleExpired, got {result:?}"
    );
}

/// #718 — every pass carries the element age at its own AOS so
/// the UI can flag stale-but-usable predictions.
#[test]
fn passes_carry_the_tle_age_at_window_start() {
    let sat = test_satellite();
    let station = test_station();
    let from = sat.epoch() + crate::sgp4_core::TLE_WARN_AGE + Duration::days(2);
    let to = from + Duration::days(1);
    let passes = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG).unwrap();
    assert!(!passes.is_empty(), "a day of NOAA passes expected");
    for pass in &passes {
        assert_eq!(pass.tle_age, pass.start - sat.epoch());
        assert!(pass.tle_age >= crate::sgp4_core::TLE_WARN_AGE + Duration::days(2));
        assert_eq!(pass.tle_freshness(), TleFreshness::Stale);
    }
}

/// #720 — time arithmetic at the edge of chrono's range must not
/// panic: `is_ascending` samples `when + 30 s`.
#[test]
fn is_ascending_at_max_utc_returns_an_error_instead_of_panicking() {
    let sat = test_satellite();
    let result = is_ascending(&sat, chrono::DateTime::<Utc>::MAX_UTC);
    assert!(result.is_err(), "got {result:?}");
}

/// #720 — the coarse scan's `t + COARSE_STEP` must not panic when
/// `to` is `MAX_UTC` (the TLE is expired there, so the expected
/// outcome is an error, never a panic).
#[test]
fn upcoming_passes_at_max_utc_does_not_panic() {
    let sat = test_satellite();
    let station = test_station();
    let to = chrono::DateTime::<Utc>::MAX_UTC;
    let from = to - Duration::hours(1);
    let result = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG);
    assert!(result.is_err(), "got {result:?}");
}

/// CR round 1 on PR #799 — freshness applies to the whole window:
/// a window that crosses `TLE_MAX_AGE` is clamped at the expiry
/// horizon, so no pass starting after it is ever returned.
#[test]
fn window_crossing_the_expiry_horizon_is_clamped() {
    let sat = test_satellite();
    let station = test_station();
    let expiry = sat.epoch() + crate::sgp4_core::TLE_MAX_AGE;
    let from = expiry - Duration::days(1);
    let to = expiry + Duration::days(1);
    let passes = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG).unwrap();
    assert!(
        !passes.is_empty(),
        "a day of Vanguard passes expected before expiry"
    );
    for pass in &passes {
        assert!(
            pass.start < expiry,
            "pass at {} starts after expiry",
            pass.start
        );
        assert!(
            pass.end <= expiry,
            "pass ending {} runs past expiry",
            pass.end
        );
        assert_eq!(
            pass.tle_age,
            pass.start - sat.epoch(),
            "age is at the pass start"
        );
    }
}

/// CR round 1 on PR #799 — each pass reports the element age at
/// its own start, so a window straddling `TLE_WARN_AGE` yields
/// fresh passes before the threshold and stale ones after it.
#[test]
fn passes_straddling_the_warn_threshold_report_their_own_age() {
    let sat = test_satellite();
    let station = test_station();
    let warn = sat.epoch() + crate::sgp4_core::TLE_WARN_AGE;
    let from = warn - Duration::days(1);
    let to = warn + Duration::days(1);
    let passes = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG).unwrap();
    let fresh = passes
        .iter()
        .filter(|p| p.tle_freshness() == TleFreshness::Fresh)
        .count();
    let stale = passes
        .iter()
        .filter(|p| p.tle_freshness() == TleFreshness::Stale)
        .count();
    assert!(
        fresh > 0 && stale > 0,
        "expected both bands, got {passes:?}"
    );
    for pass in &passes {
        assert_eq!(
            pass.tle_freshness() == TleFreshness::Stale,
            pass.start >= warn
        );
    }
}

/// Codacy on PR #799 — a pass still in progress at `to` is returned
/// with `end == to` (LOS is outside the window).
#[test]
fn pass_in_progress_at_window_end_is_clamped_to_it() {
    let sat = test_satellite();
    let station = test_station();
    let day = upcoming_passes(
        &station,
        &sat,
        sat.epoch(),
        sat.epoch() + Duration::days(1),
        TEST_MIN_ELEVATION_DEG,
    )
    .unwrap();
    let reference = day.first().expect("a pass within a day");
    let to = reference.max_el_time;
    let from = reference.start - Duration::minutes(5);
    let passes = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG).unwrap();
    assert_eq!(passes.len(), 1, "{passes:?}");
    assert_eq!(passes[0].end, to, "LOS must be clamped to the window end");
    assert!((passes[0].start - reference.start).num_seconds().abs() <= 2);
}

/// CR round 2 on PR #799 — a rise that lands exactly on the window
/// end (the clamped expiry horizon is the real-world case) must
/// not yield a zero-duration `Pass { start: to, end: to }`.
#[test]
fn rise_at_the_window_end_does_not_emit_a_zero_duration_pass() {
    let sat = test_satellite();
    let station = test_station();
    let day = upcoming_passes(
        &station,
        &sat,
        sat.epoch(),
        sat.epoch() + Duration::days(1),
        TEST_MIN_ELEVATION_DEG,
    )
    .unwrap();
    let reference = day.first().expect("a pass within a day");
    let from = reference.start - Duration::hours(1);
    let passes = upcoming_passes(
        &station,
        &sat,
        from,
        reference.start,
        TEST_MIN_ELEVATION_DEG,
    )
    .unwrap();
    for pass in &passes {
        assert!(pass.end > pass.start, "zero-duration pass: {pass:?}");
    }
}
