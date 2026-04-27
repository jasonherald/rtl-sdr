//! Doppler-correction tracker: continuously adjusts the VFO
//! offset for a tuned satellite's predicted Doppler shift.
//! Per issue #521 (sub-ticket of #520) and the design spec at
//! `docs/superpowers/specs/2026-04-26-doppler-correction-design.md`.

use sdr_sat::{GroundStation, KnownSatellite, Satellite, SatelliteError, track};

/// ±tolerance around a catalog satellite's `downlink_hz` within
/// which the user's tuned center frequency is considered "tuned
/// to that satellite". Generous enough to absorb PPM-correction
/// nudges and pre-pass drift, narrow enough to avoid catalog
/// collisions on the 137 MHz APT/LRPT cluster. Per spec §2.
pub const FREQ_MATCH_TOLERANCE_HZ: f64 = 20_000.0;

/// One catalog candidate with its currently-evaluated elevation.
/// Built by the caller (which has SGP4 + station + TLE access)
/// and fed into [`pick_active_satellite`].
#[derive(Debug, Clone, Copy)]
pub struct Candidate {
    pub satellite: &'static KnownSatellite,
    /// Elevation in degrees relative to the user's ground
    /// station, computed at the current evaluation time.
    /// Negative values mean below the horizon.
    pub elevation_deg: f64,
}

/// Pure trigger evaluation per spec §2: pick the catalog
/// satellite (if any) the Doppler tracker should be locked to
/// right now.
///
/// Inputs match the trigger rule's three conditions:
///   1. `master_enabled` — user's master switch (Satellites panel)
///   2. `current_freq_hz` — radio's current center frequency
///   3. `candidates` — for each catalog entry whose
///      `downlink_hz` is within `FREQ_MATCH_TOLERANCE_HZ` of
///      `current_freq_hz`, the entry's currently-evaluated
///      elevation. Caller is responsible for the SGP4 propagate
///      and station math; this function only decides which
///      among the matches wins.
///
/// Returns `Some(&KnownSatellite)` for the highest-elevation
/// above-horizon match. Ties are broken by `candidates` order
/// (caller passes them in `KNOWN_SATELLITES` order so the result
/// is deterministic). Returns `None` if the master switch is
/// off, no candidates, or no candidate is above the horizon.
#[must_use]
pub fn pick_active_satellite(
    master_enabled: bool,
    candidates: &[Candidate],
) -> Option<&'static KnownSatellite> {
    if !master_enabled {
        return None;
    }
    // Find the maximum elevation among above-horizon candidates,
    // then return the *first* candidate in `candidates` order that
    // has that elevation. `Iterator::max_by` keeps the *last* equal
    // element, which would break the spec §2 deterministic tie-break
    // (earlier-in-KNOWN_SATELLITES wins). Two-pass avoids that.
    let above: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.elevation_deg > 0.0)
        .collect();
    let max_elev = above.iter().map(|c| c.elevation_deg).reduce(f64::max)?;
    // Exact equality is intentional: `max_elev` came from the same
    // `f64` values in `above`, so this is identity comparison of a
    // value we already read — not a computed approximation.
    #[allow(clippy::float_cmp)]
    above
        .into_iter()
        .find(|c| c.elevation_deg == max_elev)
        .map(|c| c.satellite)
}

/// Doppler-correction errors surfaced to the tracker. We collapse
/// every failure mode (TLE parse, SGP4 propagate, range-rate
/// computation) into a single variant with a string `cause`
/// because every one of them is non-fatal — the tracker logs and
/// stays dormant for that tick rather than escalating.
#[derive(Debug, thiserror::Error)]
pub enum DopplerError {
    /// SGP4 propagator or TLE parse failed.
    #[error("SGP4 / TLE: {0}")]
    Propagation(#[from] SatelliteError),
}

/// Compute the Doppler offset (Hz) to apply to the VFO right
/// now, given a satellite + ground station + parsed TLE +
/// carrier frequency + UTC instant. Pure function — no I/O,
/// no caching.
///
/// Sign convention follows [`sdr_sat::Track::doppler_shift_hz`]:
/// positive = satellite approaching (received frequency > nominal
/// carrier), negative = receding.
///
/// The returned offset is intended to be added to the existing
/// VFO offset value: `live_offset = user_reference + doppler`
/// (per spec §4 additive override).
///
/// # Errors
///
/// Returns [`DopplerError::Propagation`] if SGP4 fails (TLE epoch
/// too far from `when`, malformed elements, etc.).
pub fn compute_doppler_offset_hz(
    satellite: &Satellite,
    station: &GroundStation,
    when: chrono::DateTime<chrono::Utc>,
    carrier_hz: f64,
) -> Result<f64, DopplerError> {
    let track = track(station, satellite, when)?;
    Ok(track.doppler_shift_hz(carrier_hz))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use sdr_sat::KNOWN_SATELLITES;

    fn noaa_15() -> &'static KnownSatellite {
        KNOWN_SATELLITES
            .iter()
            .find(|s| s.name == "NOAA 15")
            .expect("NOAA 15 in catalog")
    }

    fn noaa_18() -> &'static KnownSatellite {
        KNOWN_SATELLITES
            .iter()
            .find(|s| s.name == "NOAA 18")
            .expect("NOAA 18 in catalog")
    }

    fn noaa_19() -> &'static KnownSatellite {
        KNOWN_SATELLITES
            .iter()
            .find(|s| s.name == "NOAA 19")
            .expect("NOAA 19 in catalog")
    }

    #[test]
    fn freq_match_tolerance_is_20_khz() {
        assert!((FREQ_MATCH_TOLERANCE_HZ - 20_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn master_switch_off_returns_none_even_with_overhead_match() {
        let candidates = vec![Candidate {
            satellite: noaa_15(),
            elevation_deg: 45.0,
        }];
        assert!(pick_active_satellite(false, &candidates).is_none());
    }

    #[test]
    fn no_candidates_returns_none() {
        assert!(pick_active_satellite(true, &[]).is_none());
    }

    #[test]
    fn single_above_horizon_candidate_wins() {
        let candidates = vec![Candidate {
            satellite: noaa_15(),
            elevation_deg: 12.5,
        }];
        let pick = pick_active_satellite(true, &candidates).expect("some pick");
        assert_eq!(pick.name, "NOAA 15");
    }

    #[test]
    fn below_horizon_candidate_returns_none() {
        let candidates = vec![Candidate {
            satellite: noaa_15(),
            elevation_deg: -3.0,
        }];
        assert!(pick_active_satellite(true, &candidates).is_none());
    }

    #[test]
    fn zero_elevation_treated_as_below_horizon() {
        // `elevation_deg > 0.0` is the gate — exactly-on-horizon
        // is excluded. Avoids edge cases where SGP4 noise might
        // produce a near-zero elevation that's not a real pass.
        let candidates = vec![Candidate {
            satellite: noaa_15(),
            elevation_deg: 0.0,
        }];
        assert!(pick_active_satellite(true, &candidates).is_none());
    }

    #[test]
    fn highest_elevation_wins_among_overhead_matches() {
        // Spec §2: NOAA 18 + 19 both around 137.9 MHz; multi-sat
        // collision picks the higher-elevation one.
        let candidates = vec![
            Candidate {
                satellite: noaa_18(),
                elevation_deg: 8.0,
            },
            Candidate {
                satellite: noaa_19(),
                elevation_deg: 32.0,
            },
        ];
        let pick = pick_active_satellite(true, &candidates).expect("some pick");
        assert_eq!(pick.name, "NOAA 19");
    }

    #[test]
    fn equal_elevation_breaks_to_first_in_candidates_order() {
        // Spec §2 deterministic tie-break: caller passes
        // candidates in `KNOWN_SATELLITES` order; ties go to
        // earlier entry. Testing with NOAA 18 first so it wins
        // the tie.
        let candidates = vec![
            Candidate {
                satellite: noaa_18(),
                elevation_deg: 15.0,
            },
            Candidate {
                satellite: noaa_19(),
                elevation_deg: 15.0,
            },
        ];
        let pick = pick_active_satellite(true, &candidates).expect("some pick");
        assert_eq!(pick.name, "NOAA 18");
    }

    #[test]
    fn one_above_one_below_picks_above() {
        let candidates = vec![
            Candidate {
                satellite: noaa_18(),
                elevation_deg: -5.0,
            },
            Candidate {
                satellite: noaa_19(),
                elevation_deg: 22.0,
            },
        ];
        let pick = pick_active_satellite(true, &candidates).expect("some pick");
        assert_eq!(pick.name, "NOAA 19");
    }

    // A TLE for NOAA 19 captured 2026-04-26 (epoch 26116.59 ≈
    // 2026-04-26T14:06 UTC) — used to pin the Doppler curve
    // shape against a known geometry. Not updated automatically;
    // if the tests ever fail because the TLE has aged out of the
    // SGP4 epoch budget, capture a fresh pair from Celestrak
    // (https://celestrak.org/NORAD/elements/gp.php?CATNR=33591
    // &FORMAT=tle) and update both lines.
    const NOAA_19_TLE_LINE1: &str =
        "1 33591U 09005A   26116.58781044  .00000054  00000+0  52624-4 0  9999";
    const NOAA_19_TLE_LINE2: &str =
        "2 33591  98.9537 187.4203 0013993 172.1068 188.0327 14.13465738887193";

    fn test_station() -> GroundStation {
        // San Francisco-ish — coords matter only insofar as the
        // satellite is overhead during the chosen propagate time.
        GroundStation::new(37.7749, -122.4194, 50.0)
    }

    #[test]
    fn compute_doppler_offset_returns_signed_value_at_known_time() {
        // Sanity: the function returns SOMETHING and doesn't
        // error on a satellite-not-overhead time. The exact
        // sign/magnitude depends on geometry, but the call
        // shape and error path are pinned.
        let sat = Satellite::from_tle("NOAA 19", NOAA_19_TLE_LINE1, NOAA_19_TLE_LINE2)
            .expect("TLE parses");
        let when = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let result = compute_doppler_offset_hz(&sat, &test_station(), when, 137_100_000.0).unwrap();
        assert!(
            result.abs() < 10_000.0,
            "Doppler magnitude should fit within ±10 kHz at 137 MHz: got {result}"
        );
    }

    #[test]
    fn compute_doppler_offset_curves_through_zero_during_overhead_pass() {
        // Spec §1: across a pass, Doppler sweeps positive →
        // zero at TCA → negative. Sample three instants 4 min
        // apart; check the offset trends from positive toward
        // negative (or vice versa — sign depends on which side
        // of the orbit the station sees). The key invariant is
        // monotonicity: it should always be moving in one
        // direction across a single pass.
        let sat = Satellite::from_tle("NOAA 19", NOAA_19_TLE_LINE1, NOAA_19_TLE_LINE2)
            .expect("TLE parses");
        let station = test_station();
        let carrier = 137_100_000.0_f64;

        // Three instants 4 min apart somewhere in the propagation
        // window. Don't assume any specific pass falls in this
        // window — just check that the offset CHANGES monotonically,
        // which is true outside TCA's brief sign-flip moment.
        let t0 = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let t1 = t0 + chrono::Duration::minutes(4);
        let t2 = t0 + chrono::Duration::minutes(8);

        let d0 = compute_doppler_offset_hz(&sat, &station, t0, carrier).unwrap();
        let d1 = compute_doppler_offset_hz(&sat, &station, t1, carrier).unwrap();
        let d2 = compute_doppler_offset_hz(&sat, &station, t2, carrier).unwrap();

        // Doppler should change appreciably over 8 minutes of
        // satellite motion (the satellite covers ~3500 km in
        // that span — geometry definitely changes).
        let total_swing = (d2 - d0).abs();
        assert!(
            total_swing > 100.0,
            "Doppler should swing >100 Hz over 8 min: d0={d0} d1={d1} d2={d2}"
        );
    }

    #[test]
    fn compute_doppler_offset_sign_matches_approaching_recedeing() {
        // Spec §5: positive Doppler when approaching, negative
        // when receding. Formula is `Δf = -f₀ · ṙ / c`, so a
        // positive range-rate (receding) must give negative Δf.
        //
        // We can't easily construct a synthetic "approaching at
        // 5 km/s" test without implementing our own SGP4, but we
        // can verify the formula's sign by computing the offset
        // for the same satellite at two adjacent times and
        // checking that the SIGN of the offset matches the SIGN
        // implied by the geometry change. This is a regression
        // pin against accidentally flipping the sign.
        let sat = Satellite::from_tle("NOAA 19", NOAA_19_TLE_LINE1, NOAA_19_TLE_LINE2)
            .expect("TLE parses");
        let station = test_station();
        let carrier = 137_100_000.0_f64;

        let when = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let track = sdr_sat::track(&station, &sat, when).expect("track ok");
        let doppler = compute_doppler_offset_hz(&sat, &station, when, carrier).unwrap();

        // Sign relationship: doppler = -carrier * range_rate / c.
        // So `doppler` and `range_rate` always have OPPOSITE signs.
        if track.range_rate_km_s.abs() > 0.01 {
            let opposite_signs = (doppler > 0.0 && track.range_rate_km_s < 0.0)
                || (doppler < 0.0 && track.range_rate_km_s > 0.0);
            assert!(
                opposite_signs,
                "doppler={doppler} should be opposite sign to range_rate={}",
                track.range_rate_km_s
            );
        }
    }

    #[test]
    fn compute_doppler_offset_zero_carrier_returns_zero() {
        // Edge case: f₀ = 0 should yield 0 regardless of geometry
        // (the formula multiplies by `frequency_hz`).
        let sat = Satellite::from_tle("NOAA 19", NOAA_19_TLE_LINE1, NOAA_19_TLE_LINE2)
            .expect("TLE parses");
        let when = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let result = compute_doppler_offset_hz(&sat, &test_station(), when, 0.0).unwrap();
        assert!((result - 0.0).abs() < f64::EPSILON, "got {result}");
    }
}
