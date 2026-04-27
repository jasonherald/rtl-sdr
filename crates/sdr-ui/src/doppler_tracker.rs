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
    ///
    /// On a transition to **disabled**, the tracker atomically:
    ///   - clears the active satellite (matching `set_active(None)` semantics)
    ///   - captures the current `user_reference_offset_hz`
    ///   - resets `user_reference_offset_hz` to 0
    ///   - returns `Some(captured)` so the wiring layer can
    ///     dispatch one final `SetVfoOffset(captured)` to flush
    ///     the DSP back to the user-reference baseline (or 0 if
    ///     they hadn't touched the slider).
    ///
    /// Returns `None` on enable transitions (or no-change calls)
    /// — engagement of a satellite is the trigger re-evaluate
    /// tick's job, not this method's. Per CR round 1 on PR #554:
    /// before this change the wiring layer needed a "dispatch
    /// first, then clear later" dance that left
    /// `user_reference_offset_hz` non-zero across master-off,
    /// breaking parity with `set_active(None)`.
    pub fn set_master_enabled(&mut self, enabled: bool) -> Option<f64> {
        self.master_enabled = enabled;
        if !enabled {
            let final_offset_hz = self.user_reference_offset_hz;
            self.active = None;
            self.user_reference_offset_hz = 0.0;
            return Some(final_offset_hz);
        }
        None
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
        // San Francisco-ish — picked because NOAA 19's polar orbit
        // delivers passes everywhere; the actual lat/lon doesn't
        // shape the test outcomes once we use `upcoming_passes` to
        // find a real pass relative to this station.
        GroundStation::new(37.7749, -122.4194, 50.0)
    }

    /// TLE epoch as a UTC instant (decoded from
    /// `NOAA_19_TLE_LINE1` field 4: year 26, day-of-year 116.58781).
    /// Used as the search anchor for finding a real pass — SGP4
    /// is most accurate within a few days of epoch, and CR
    /// round 1 on PR #554 flagged that propagating ~28 months
    /// before epoch (the prior 2024-01-01 anchor) made the
    /// "monotonic across a pass" assertion meaningless.
    fn tle_epoch_utc() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-04-26T14:06:26Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    /// Locate the first NOAA 19 pass over `test_station()` within
    /// 48 hours of the TLE epoch, and return (`AOS`, `TCA`, `LOS`)
    /// timestamps. Panics if no pass is found (would be a TLE-or-
    /// math regression — there are typically ~6 passes per day
    /// from any mid-latitude station).
    fn first_pass_aos_tca_los(
        sat: &Satellite,
        station: &GroundStation,
    ) -> (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) {
        let from = tle_epoch_utc();
        let to = from + chrono::Duration::hours(48);
        // 5° minimum elevation: low enough to admit the typical
        // horizon-to-horizon NOAA pass shape, high enough to
        // exclude the marginal grazes that don't actually have
        // useful Doppler curve geometry.
        let pass = sdr_sat::upcoming_passes(station, sat, from, to, 5.0)
            .into_iter()
            .next()
            .expect("a NOAA 19 pass should fall within 48 h of epoch");
        (pass.start, pass.max_el_time, pass.end)
    }

    #[test]
    fn compute_doppler_offset_returns_signed_value_at_known_time() {
        // Sanity: the function returns a value in the right
        // ballpark when sampled mid-pass. Pinning the call shape
        // and the order-of-magnitude (±10 kHz fits the entire
        // ±5 kHz Doppler envelope at 137 MHz with margin).
        let sat = Satellite::from_tle("NOAA 19", NOAA_19_TLE_LINE1, NOAA_19_TLE_LINE2)
            .expect("TLE parses");
        let station = test_station();
        let (_aos, tca, _los) = first_pass_aos_tca_los(&sat, &station);
        let result = compute_doppler_offset_hz(&sat, &station, tca, 137_100_000.0).unwrap();
        assert!(
            result.abs() < 10_000.0,
            "Doppler magnitude at TCA should fit within ±10 kHz at 137 MHz: got {result}"
        );
    }

    #[test]
    #[allow(
        clippy::similar_names,
        reason = "AOS / TCA / LOS are the canonical satellite-tracking \
                  pass milestones; the d_aos/d_tca/d_los names mirror \
                  them directly and are clearer than synonyms would be"
    )]
    fn compute_doppler_offset_pass_shape_aos_to_los() {
        // Spec §1: across a pass, Doppler sweeps positive at AOS
        // (satellite approaching) → through ~zero at TCA → to
        // negative at LOS (satellite receding). Anchored to a
        // real pass found via `upcoming_passes` so the assertions
        // are meaningful — not just "the value moved 100 Hz".
        let sat = Satellite::from_tle("NOAA 19", NOAA_19_TLE_LINE1, NOAA_19_TLE_LINE2)
            .expect("TLE parses");
        let station = test_station();
        let carrier = 137_100_000.0_f64;
        let (aos, tca, los) = first_pass_aos_tca_los(&sat, &station);

        let d_aos = compute_doppler_offset_hz(&sat, &station, aos, carrier).unwrap();
        let d_tca = compute_doppler_offset_hz(&sat, &station, tca, carrier).unwrap();
        let d_los = compute_doppler_offset_hz(&sat, &station, los, carrier).unwrap();

        // AOS = approaching = positive Doppler.
        assert!(
            d_aos > 0.0,
            "AOS Doppler should be positive (approaching): d_aos={d_aos}"
        );
        // LOS = receding = negative Doppler.
        assert!(
            d_los < 0.0,
            "LOS Doppler should be negative (receding): d_los={d_los}"
        );
        // Monotonic decrease across the pass.
        assert!(
            d_aos > d_tca && d_tca > d_los,
            "Doppler should decrease monotonically AOS→TCA→LOS: \
             d_aos={d_aos}, d_tca={d_tca}, d_los={d_los}"
        );
        // |TCA| < |AOS| AND |TCA| < |LOS|: at TCA the satellite is
        // closest to the station, so the radial velocity component
        // is at its smallest magnitude.
        assert!(
            d_tca.abs() < d_aos.abs() && d_tca.abs() < d_los.abs(),
            "Doppler magnitude should be smallest at TCA: \
             |d_aos|={}, |d_tca|={}, |d_los|={}",
            d_aos.abs(),
            d_tca.abs(),
            d_los.abs()
        );
    }

    #[test]
    fn compute_doppler_offset_sign_matches_approaching_receding() {
        // Spec §5: positive Doppler when approaching, negative
        // when receding. Formula is `Δf = -f₀ · ṙ / c`, so a
        // positive range-rate (receding) must give negative Δf.
        // Anchored to AOS so we know the satellite is genuinely
        // approaching — `range_rate_km_s` will be sharply
        // negative and the sign assertion is meaningful.
        let sat = Satellite::from_tle("NOAA 19", NOAA_19_TLE_LINE1, NOAA_19_TLE_LINE2)
            .expect("TLE parses");
        let station = test_station();
        let carrier = 137_100_000.0_f64;
        let (aos, _tca, _los) = first_pass_aos_tca_los(&sat, &station);

        let track = sdr_sat::track(&station, &sat, aos).expect("track ok");
        let doppler = compute_doppler_offset_hz(&sat, &station, aos, carrier).unwrap();

        // At AOS, range_rate_km_s should be negative (approaching)
        // and doppler should be positive — opposite signs by the
        // formula `Δf = -f₀ · ṙ / c`.
        assert!(
            track.range_rate_km_s < 0.0,
            "AOS range-rate should be negative (approaching): {}",
            track.range_rate_km_s
        );
        assert!(
            doppler > 0.0,
            "AOS Doppler should be positive (approaching): {doppler}"
        );
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
        let final_offset = t.set_master_enabled(false);
        assert!(t.active().is_none());
        // Returns Some on disable transition. Per CR round 1
        // on PR #554.
        assert_eq!(final_offset, Some(0.0));
    }

    #[test]
    fn set_master_disabled_resets_user_reference_offset_and_returns_it() {
        // Spec §4 reset semantics + CR round 1 on PR #554:
        // master-off must reset `user_reference_offset_hz` to
        // match `set_active(None)` semantics, AND return the
        // pre-reset value so the wiring layer can dispatch one
        // final `SetVfoOffset(captured)` to flush DSP.
        let mut t = DopplerTracker::new(true);
        let _ = t.set_active(Some(noaa_15()));
        t.set_user_reference_offset_hz(750.0);

        let final_offset = t.set_master_enabled(false);

        assert_eq!(final_offset, Some(750.0), "must return pre-reset value");
        assert!(t.active().is_none(), "active must be cleared");
        assert!(
            (t.user_reference_offset_hz() - 0.0).abs() < f64::EPSILON,
            "user_reference_offset_hz must reset to 0"
        );
    }

    #[test]
    fn set_master_enabled_does_not_immediately_engage() {
        // Re-enabling the master switch shouldn't synthesize a
        // satellite — the wiring layer's re-evaluate tick is
        // what decides which (if any) satellite to engage.
        let mut t = DopplerTracker::new(false);
        let result = t.set_master_enabled(true);
        assert!(t.active().is_none());
        // Enable transitions return None — only disables return
        // a flush-offset.
        assert_eq!(result, None);
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
    fn set_active_swap_resets_user_reference_offset_at_model_layer() {
        // **Model-layer** behavior: `set_active` unconditionally
        // resets `user_reference_offset_hz` on any change.
        //
        // Production-level behavior (spec §4 reset semantics)
        // differs: the wiring layer in `window.rs` captures
        // `prior_user_ref` BEFORE calling `set_active` and
        // immediately restores it afterward on Some(A) → Some(B)
        // swaps so the user's manual fine-tune offset survives.
        // The model's reset behavior is what the wiring layer
        // compensates against. Per CR round 5 on PR #554.
        let mut t = DopplerTracker::new(true);
        let _ = t.set_active(Some(noaa_15()));
        t.set_user_reference_offset_hz(500.0);
        assert!((t.user_reference_offset_hz() - 500.0).abs() < f64::EPSILON);

        // Swap to a different satellite — offset resets at model
        // layer (wiring-layer restoration is tested via smoke).
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
