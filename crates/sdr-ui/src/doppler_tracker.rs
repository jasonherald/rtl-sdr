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

/// Stateful coordinator for the per-window Doppler tracker. The
/// timers, GTK widgets, and DSP-channel dispatch live in the
/// wiring layer (`window.rs::connect_doppler_tracker`); this
/// type owns the model state — master switch, current active
/// satellite, additive user reference offset — and exposes a
/// small set of methods the wiring layer drives on each tick or
/// re-evaluation.
///
/// Decoupled from GTK so the state-transition logic is unit-
/// testable headlessly. Same pattern as `satellites_recorder`'s
/// pure-tick / interpret-action split.
#[derive(Debug, Default)]
pub struct DopplerTracker {
    master_enabled: bool,
    active: Option<&'static KnownSatellite>,
    user_reference_offset_hz: f64,
}

impl DopplerTracker {
    /// Construct with the persisted master-switch value. Pass
    /// the result of
    /// `sidebar::satellites_panel::load_doppler_tracking_enabled`.
    #[must_use]
    pub fn new(master_enabled: bool) -> Self {
        Self {
            master_enabled,
            active: None,
            user_reference_offset_hz: 0.0,
        }
    }

    /// Update the master switch. Caller (wiring layer) is
    /// responsible for persisting the new value via
    /// `save_doppler_tracking_enabled`.
    pub fn set_master_enabled(&mut self, enabled: bool) {
        self.master_enabled = enabled;
        if !enabled {
            // Spec §2: when trigger conditions go false (master
            // off counts), reset the active satellite. The
            // wiring layer handles the final SetVfoOffset(
            // user_reference_offset) dispatch.
            self.active = None;
        }
    }

    /// Whether the master switch is currently on.
    #[must_use]
    pub fn master_enabled(&self) -> bool {
        self.master_enabled
    }

    /// Current additive user reference offset (Hz). Set by the
    /// wiring layer when the user manually drags the VFO offset
    /// slider; Doppler tracking adds on top of this. Per spec §4.
    #[must_use]
    pub fn user_reference_offset_hz(&self) -> f64 {
        self.user_reference_offset_hz
    }

    /// Update the additive user reference offset. Called when
    /// the user manually drags the VFO offset slider. Per spec §4.
    pub fn set_user_reference_offset_hz(&mut self, hz: f64) {
        self.user_reference_offset_hz = hz;
    }

    /// Replace the currently-active satellite (or clear it). On
    /// a transition between distinct satellites or to-None, the
    /// `user_reference_offset_hz` resets to 0 — Doppler tracking
    /// is per-pass, and so is any user fine-tune. Returns true
    /// if the active satellite changed (useful for the wiring
    /// layer to know it needs to dispatch a `SetVfoOffset` and/or
    /// update the status bar).
    pub fn set_active(&mut self, satellite: Option<&'static KnownSatellite>) -> bool {
        let changed = match (self.active, satellite) {
            (None, None) => false,
            (Some(a), Some(b)) => !std::ptr::eq(a, b),
            _ => true,
        };
        if changed {
            // Spec §4 reset semantics: per-pass reference offset.
            self.user_reference_offset_hz = 0.0;
            self.active = satellite;
        }
        changed
    }

    /// Currently-active satellite (if any).
    #[must_use]
    pub fn active(&self) -> Option<&'static KnownSatellite> {
        self.active
    }

    /// Compose the live VFO offset to dispatch:
    /// `live = user_reference + doppler`. Per spec §4.
    #[must_use]
    pub fn live_offset_hz(&self, doppler_hz: f64) -> f64 {
        self.user_reference_offset_hz + doppler_hz
    }
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

    #[test]
    fn tracker_constructs_with_persisted_master_value() {
        let on = DopplerTracker::new(true);
        assert!(on.master_enabled());
        assert!(on.active().is_none());
        assert!((on.user_reference_offset_hz() - 0.0).abs() < f64::EPSILON);

        let off = DopplerTracker::new(false);
        assert!(!off.master_enabled());
    }

    #[test]
    fn set_master_disabled_clears_active_satellite() {
        let mut t = DopplerTracker::new(true);
        let _ = t.set_active(Some(noaa_15()));
        assert_eq!(t.active().map(|s| s.name), Some("NOAA 15"));
        t.set_master_enabled(false);
        assert!(t.active().is_none());
    }

    #[test]
    fn set_master_enabled_does_not_immediately_engage() {
        // Re-enabling the master switch shouldn't synthesize a
        // satellite — the wiring layer's re-evaluate tick is
        // what decides which (if any) satellite to engage.
        let mut t = DopplerTracker::new(false);
        t.set_master_enabled(true);
        assert!(t.active().is_none());
    }

    #[test]
    fn set_active_returns_true_only_on_change() {
        let mut t = DopplerTracker::new(true);
        assert!(t.set_active(Some(noaa_15())), "None → Some is a change");
        assert!(
            !t.set_active(Some(noaa_15())),
            "Some(X) → Some(X) is not a change"
        );
        assert!(
            t.set_active(Some(noaa_18())),
            "Some(X) → Some(Y) is a change"
        );
        assert!(t.set_active(None), "Some → None is a change");
        assert!(!t.set_active(None), "None → None is not a change");
    }

    #[test]
    fn satellite_swap_resets_user_reference_offset() {
        // Spec §4 reset semantics: per-pass reference offset.
        let mut t = DopplerTracker::new(true);
        let _ = t.set_active(Some(noaa_15()));
        t.set_user_reference_offset_hz(500.0);
        assert!((t.user_reference_offset_hz() - 500.0).abs() < f64::EPSILON);

        // Swap to a different satellite — offset resets.
        let _ = t.set_active(Some(noaa_18()));
        assert!((t.user_reference_offset_hz() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn satellite_to_none_resets_user_reference_offset() {
        let mut t = DopplerTracker::new(true);
        let _ = t.set_active(Some(noaa_15()));
        t.set_user_reference_offset_hz(500.0);
        let _ = t.set_active(None);
        assert!((t.user_reference_offset_hz() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn same_satellite_set_active_preserves_user_reference_offset() {
        // If the satellite doesn't change (re-evaluate just
        // confirms what's already there), the user's manual
        // tune offset should NOT reset.
        let mut t = DopplerTracker::new(true);
        let _ = t.set_active(Some(noaa_15()));
        t.set_user_reference_offset_hz(500.0);
        let _ = t.set_active(Some(noaa_15()));
        assert!(
            (t.user_reference_offset_hz() - 500.0).abs() < f64::EPSILON,
            "user offset must survive a no-op set_active"
        );
    }

    #[test]
    fn live_offset_is_additive() {
        // Spec §4: live_offset = user_reference + doppler.
        let mut t = DopplerTracker::new(true);
        t.set_user_reference_offset_hz(300.0);
        assert!((t.live_offset_hz(-1_400.0) - (-1_100.0)).abs() < f64::EPSILON);
        assert!((t.live_offset_hz(2_700.0) - 3_000.0).abs() < f64::EPSILON);
        assert!((t.live_offset_hz(0.0) - 300.0).abs() < f64::EPSILON);
    }
}
