use super::*;
use crate::sgp4_core::{Satellite, eci_to_ecef};
use chrono::{DateTime, TimeZone, Utc};

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

/// A candidate's propagated ECEF position (km) at `when`.
fn ecef_at(t: (&str, &str, &str), when: DateTime<Utc>) -> [f64; 3] {
    let eci = sat(t).propagate(when).expect("propagates over test window");
    eci_to_ecef(eci.position_km, when)
}

/// Point `f` of the way from `a` to `b` (per-axis lerp), so a synthetic
/// target can be placed at a chosen distance ratio between two real
/// candidate positions.
fn lerp(a: [f64; 3], b: [f64; 3], f: f64) -> [f64; 3] {
    [
        a[0] + (b[0] - a[0]) * f,
        a[1] + (b[1] - a[1]) * f,
        a[2] + (b[2] - a[2]) * f,
    ]
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

/// Runner-up promotion, negative case. When a nearer candidate is found
/// *after* a best is already established, the old best must be promoted
/// into `runner_up_km` — the only path that fills the runner-up from an
/// existing best (`identify.rs` `_ =>` branch). The target sits 40% of
/// the way from FM06 toward FM04, so FM06 is the nearer (0.4·L) and FM04
/// the runner-up (0.6·L), a ratio of 1.5 < `MATCH_AMBIGUITY_MARGIN`.
/// Listing FM04 first forces FM06 to displace it. A correct promotion
/// applies the margin against 0.6·L and returns None; a dropped promotion
/// would leave `runner_up_km` at infinity and wrongly accept the match —
/// so the asserted None specifically guards the promotion value.
#[test]
fn promoted_runner_up_enforces_margin() {
    let when = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let target = lerp(ecef_at(FM06, when), ecef_at(FM04, when), 0.4);
    // FM04 first → becomes best; FM06 (nearer) then promotes it.
    let candidates = vec![
        ("ORBCOMM FM04".into(), sat(FM04)),
        ("ORBCOMM FM06".into(), sat(FM06)),
    ];
    // Large threshold: this exercises the margin/promotion path, not the
    // distance cap.
    assert!(identify_from_ecef(target, when, &candidates, 1.0e7).is_none());
}

/// Runner-up promotion, positive case. Same displacing order (FM04 first,
/// FM06 nearer and found second), but the target sits 30% of the way, so
/// the promoted runner-up (0.7·L) clears `MATCH_AMBIGUITY_MARGIN` against
/// the winner (0.3·L, ratio 2.33) and FM06 is returned. Proves the
/// promotion branch also accepts, not only rejects.
#[test]
fn promotes_previous_best_and_matches() {
    let when = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let target = lerp(ecef_at(FM06, when), ecef_at(FM04, when), 0.3);
    let candidates = vec![
        ("ORBCOMM FM04".into(), sat(FM04)),
        ("ORBCOMM FM06".into(), sat(FM06)),
    ];
    let m = identify_from_ecef(target, when, &candidates, 1.0e7)
        .expect("FM06 clears the ambiguity margin");
    assert_eq!(m.name, "ORBCOMM FM06");
}
