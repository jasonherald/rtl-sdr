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

/// A satellite's own propagated position (at reception time `t`) fed
/// back as the decoded ECEF, with `when = t`, matches that satellite at
/// ~0 km.
#[test]
fn matches_self_at_reception_time() {
    let t = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let fm06 = sat(FM06);
    let eci = fm06.propagate(t).unwrap();
    let target = eci_to_ecef(eci.position_km, t);
    let candidates = vec![
        ("ORBCOMM FM06".into(), sat(FM06)),
        ("ORBCOMM FM04".into(), sat(FM04)),
    ];
    let when = t;
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
    let when = t;
    assert!(identify_from_ecef(target, when, &candidates, 500.0).is_none());
}

/// Real FM108 pass calibration: a live decoded ephemeris position from
/// this bird, matched against candidate TLEs propagated to reception
/// time, identifies FM108 unambiguously (~47 km vs a >3000 km
/// runner-up). Regression guard for the reception-time fix (#900).
#[test]
fn identifies_fm108_from_real_pass() {
    const FM108: (&str, &str, &str) = (
        "ORBCOMM FM108",
        "1 41187U 15081J   26248.48179345  .00000253  00000+0  89670-4 0  9992",
        "2 41187  47.0044 162.5353 0002300 224.2950 135.7752 14.58968400571802",
    );
    let candidates = vec![
        ("ORBCOMM FM108".into(), sat(FM108)),
        ("ORBCOMM FM06".into(), sat(FM06)),
        ("ORBCOMM FM04".into(), sat(FM04)),
    ];
    let when = Utc.with_ymd_and_hms(2026, 9, 5, 20, 1, 39).unwrap();
    let m = identify_spacecraft(
        36.2,
        -81.6,
        700_000.0,
        when,
        &candidates,
        DEFAULT_MATCH_MAX_DIST_KM,
    )
    .expect("should identify FM108");
    assert_eq!(m.name, "ORBCOMM FM108");
    assert!(
        m.distance_km < 100.0,
        "distance {} km too large",
        m.distance_km
    );
}
