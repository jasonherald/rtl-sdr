//! Doppler-correction tracker: continuously adjusts the VFO
//! offset for a tuned satellite's predicted Doppler shift.
//! Per issue #521 (sub-ticket of #520) and the design spec at
//! `docs/superpowers/specs/2026-04-26-doppler-correction-design.md`.

use sdr_sat::KnownSatellite;

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

#[cfg(test)]
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
}
