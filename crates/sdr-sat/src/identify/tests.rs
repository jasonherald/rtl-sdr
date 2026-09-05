use super::*;
use crate::sgp4_core::{Satellite, eci_to_ecef};
use chrono::{TimeZone, Utc};

// Real Orbcomm TLEs (Celestrak, epoch 2026-248) used as fixtures.
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

fn sat(t: (&str, &str, &str)) -> Satellite {
    Satellite::from_tle(t.0, t.1, t.2).expect("valid TLE fixture")
}

/// A satellite's own propagated position (at true UTC `t`) fed back as
/// the decoded ECEF, with `when = t + leap`, matches that satellite at
/// ~0 km — exercising the leap-second correction end to end.
#[test]
fn matches_self_with_leap_correction() {
    let t = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let fm06 = sat(FM06);
    let eci = fm06.propagate(t).unwrap();
    let target = eci_to_ecef(eci.position_km, t);
    let candidates = vec![
        ("ORBCOMM FM06".into(), sat(FM06)),
        ("ORBCOMM FM04".into(), sat(FM04)),
    ];
    let when = t + chrono::Duration::seconds(GPS_UTC_LEAP_SECONDS);
    let m = identify_from_ecef(target, when, &candidates, DEFAULT_MATCH_MAX_DIST_KM)
        .expect("should identify FM06");
    assert_eq!(m.name, "ORBCOMM FM06");
    assert!(
        m.distance_km < 5.0,
        "distance {} km too large",
        m.distance_km
    );
}

/// A point far from every candidate → no match.
#[test]
fn no_match_when_far() {
    let t = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let candidates = vec![("ORBCOMM FM06".into(), sat(FM06))];
    // Origin-ish ECEF (deep inside Earth) is >6000 km from any orbit.
    assert!(
        identify_from_ecef([0.0, 0.0, 0.0], t, &candidates, DEFAULT_MATCH_MAX_DIST_KM).is_none()
    );
}

/// Two candidates near-equidistant from the target → ambiguous → None.
#[test]
fn ambiguous_returns_none() {
    let t = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let fm06 = sat(FM06);
    let eci = fm06.propagate(t).unwrap();
    let target = eci_to_ecef(eci.position_km, t);
    // Same satellite twice under different names: both at distance ~0,
    // runner-up not MARGIN× farther → ambiguous.
    let candidates = vec![("A".into(), sat(FM06)), ("B".into(), sat(FM06))];
    let when = t + chrono::Duration::seconds(GPS_UTC_LEAP_SECONDS);
    assert!(identify_from_ecef(target, when, &candidates, 500.0).is_none());
}
