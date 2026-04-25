//! Satellite pass enumeration + real-time tracking.
//!
//! Built on top of [`crate::sgp4_core`]: given a [`GroundStation`] and a
//! parsed [`Satellite`], produce
//!
//! * [`Track`] — current az/el/range/range-rate at one specific UTC
//!   instant. The Doppler shift comes out of `track().doppler_shift_hz`
//!   for whatever carrier frequency the caller cares about (137 MHz
//!   for APT, 145.8 for ISS-SSTV, 137.1 for Meteor LRPT, etc.).
//! * [`Pass`] — start/end/max-elevation summary of one overhead
//!   transit. [`upcoming_passes`] enumerates all passes in a time
//!   window above a caller-specified minimum elevation.
//!
//! The pass enumerator is deliberately simple: coarse 1-minute
//! elevation scan to find horizon crossings, bisection to refine each
//! crossing to ~1-second precision, fine scan inside the pass to
//! locate maximum elevation. This is plenty accurate for an APT pass
//! scheduler — pass timings drift by tens of seconds on a fresh TLE
//! anyway, and SGP4 itself is only good to a few km in position.

use chrono::{DateTime, Duration, Utc};

use crate::sgp4_core::{
    EARTH_ROTATION_RAD_PER_SEC, Satellite, SatelliteError, ecef_to_enu, eci_to_ecef,
    geodetic_to_ecef, gmst_rad,
};

/// Speed of light, km/s. Defined exactly by the SI metre.
const SPEED_OF_LIGHT_KM_S: f64 = 299_792.458;

/// Coarse step for the initial pass scan. One minute is comfortably
/// finer than the shortest LEO horizon-to-horizon transit (a low-pass
/// of ISS at high latitude can be ~6 minutes; APT at NOAA altitude is
/// 12–16 minutes), so we won't miss a pass entirely. Refinement uses
/// bisection to pin the start/end timestamps down to seconds.
const COARSE_STEP: Duration = Duration::seconds(60);

/// Step inside a detected pass for locating the elevation peak.
const FINE_STEP: Duration = Duration::seconds(10);

/// Bisection precision for horizon-crossing refinement.
const REFINE_PRECISION: Duration = Duration::seconds(1);

/// Maximum bisection iterations — bounded so a degenerate input (e.g.
/// satellite near horizon almost the whole window) can't loop forever.
const MAX_REFINE_ITERATIONS: usize = 20;

/// A receiver site on the ground — what the satellite is overhead of.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundStation {
    /// Latitude in degrees, positive north (`-90.0..=90.0`).
    pub lat_deg: f64,
    /// Longitude in degrees, positive east (`-180.0..=180.0`).
    pub lon_deg: f64,
    /// Altitude above the WGS84 ellipsoid, in metres. Sea level is `0.0`.
    pub alt_m: f64,
}

impl GroundStation {
    /// Convenience constructor.
    #[must_use]
    pub const fn new(lat_deg: f64, lon_deg: f64, alt_m: f64) -> Self {
        Self {
            lat_deg,
            lon_deg,
            alt_m,
        }
    }

    /// Station position in ECEF (km).
    #[must_use]
    pub fn ecef_km(&self) -> [f64; 3] {
        geodetic_to_ecef(self.lat_deg, self.lon_deg, self.alt_m)
    }
}

/// Snapshot of where the satellite is *right now* relative to the
/// station — what a tracker would feed to a rotor or an antenna-pattern
/// hint. Doppler shift is exposed as a method so the caller can ask
/// for it at the actual carrier frequency for whatever downlink they
/// care about.
#[derive(Debug, Clone, Copy)]
pub struct Track {
    /// Compass bearing, degrees clockwise from true north
    /// (`0.0..360.0`).
    pub azimuth_deg: f64,
    /// Elevation above local horizontal, degrees (`-90.0..=90.0`).
    /// Negative means below the horizon — won't be in a [`Pass`] but
    /// `track()` will report it for satellites that aren't overhead.
    pub elevation_deg: f64,
    /// Slant range from station to satellite, km.
    pub range_km: f64,
    /// Range rate in km/s. Positive = moving away from station,
    /// negative = approaching. Multiply through `doppler_shift_hz`
    /// for the carrier-frequency shift seen at the station.
    pub range_rate_km_s: f64,
    /// UTC instant the track was evaluated at.
    pub when: DateTime<Utc>,
}

impl Track {
    /// Doppler frequency shift the station observes at carrier
    /// `frequency_hz`. Positive shift = received frequency higher than
    /// transmitted (satellite approaching); the formula is
    /// `Δf = -f₀ · ṙ / c`.
    #[must_use]
    pub fn doppler_shift_hz(&self, frequency_hz: f64) -> f64 {
        -frequency_hz * self.range_rate_km_s / SPEED_OF_LIGHT_KM_S
    }
}

/// Summary of one overhead pass — what the scheduler UI displays.
#[derive(Debug, Clone)]
pub struct Pass {
    /// Display name copied from the [`Satellite`] used for enumeration,
    /// so the result is self-describing once moved out of the call site.
    pub satellite: String,
    /// AOS (Acquisition Of Signal) — the moment the satellite first
    /// crosses above the requested minimum elevation.
    pub start: DateTime<Utc>,
    /// LOS (Loss Of Signal) — when the satellite drops back below the
    /// minimum elevation.
    pub end: DateTime<Utc>,
    /// Peak elevation reached during the pass (degrees).
    pub max_elevation_deg: f64,
    /// UTC instant the elevation peak occurs at.
    pub max_el_time: DateTime<Utc>,
    /// Azimuth at AOS (degrees clockwise from true north).
    pub start_az_deg: f64,
    /// Azimuth at LOS (degrees clockwise from true north).
    pub end_az_deg: f64,
}

/// Compute the satellite's current az/el/range/Doppler relative to the
/// ground station.
///
/// # Errors
///
/// Propagates [`SatelliteError`] from the underlying SGP4 propagator.
pub fn track(
    station: &GroundStation,
    satellite: &Satellite,
    when: DateTime<Utc>,
) -> Result<Track, SatelliteError> {
    let sat_eci = satellite.propagate(when)?;
    let sat_ecef = eci_to_ecef(sat_eci.position_km, when);
    let station_ecef = station.ecef_km();
    let relative_ecef = sub(sat_ecef, station_ecef);
    let enu = ecef_to_enu(relative_ecef, station.lat_deg, station.lon_deg);

    let range_km = norm(enu);
    // Azimuth: 0 = North, 90 = East, range [0, 360).
    let az_rad = enu[0].atan2(enu[1]);
    let azimuth_deg = az_rad.to_degrees().rem_euclid(360.0);
    // Elevation: positive above horizon.
    let elevation_deg = if range_km > 0.0 {
        (enu[2] / range_km).asin().to_degrees()
    } else {
        90.0
    };

    // Range rate: compute in ECI to avoid the rotating-frame Coriolis
    // bookkeeping. Station velocity in ECI is ω × r_station_eci.
    let g = gmst_rad(when);
    let (sin_g, cos_g) = g.sin_cos();
    // Inverse of eci_to_ecef: rotate ECEF back into ECI.
    let station_eci = [
        cos_g * station_ecef[0] - sin_g * station_ecef[1],
        sin_g * station_ecef[0] + cos_g * station_ecef[1],
        station_ecef[2],
    ];
    let omega = EARTH_ROTATION_RAD_PER_SEC;
    // ω × r where ω = (0, 0, omega): result is (-omega·y, omega·x, 0).
    let station_vel_eci = [-omega * station_eci[1], omega * station_eci[0], 0.0];
    let range_vec_eci = sub(sat_eci.position_km, station_eci);
    let rel_vel_eci = sub(sat_eci.velocity_km_s, station_vel_eci);
    let range_rate_km_s = dot(range_vec_eci, rel_vel_eci) / norm(range_vec_eci);

    Ok(Track {
        azimuth_deg,
        elevation_deg,
        range_km,
        range_rate_km_s,
        when,
    })
}

/// Enumerate all overhead passes of `satellite` from `from` to `to`
/// (inclusive of `from`, exclusive of `to`) with peak elevation at
/// or above `min_elevation_deg`.
///
/// Passes that are already in progress at `from` are still returned —
/// their `start` is whatever moment the satellite first cleared
/// `min_elevation_deg` *within* the window. Symmetrically, passes that
/// haven't fully ended by `to` are returned with `end == to`.
pub fn upcoming_passes(
    station: &GroundStation,
    satellite: &Satellite,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    min_elevation_deg: f64,
) -> Vec<Pass> {
    if to <= from {
        return Vec::new();
    }

    let mut passes = Vec::new();
    let mut t = from;
    let mut prev_el = elevation_at(station, satellite, t);
    let mut pass_open: Option<DateTime<Utc>> = if prev_el >= min_elevation_deg {
        // Window starts mid-pass — clamp the start to the window edge.
        Some(from)
    } else {
        None
    };

    while t < to {
        let next_t = (t + COARSE_STEP).min(to);
        let next_el = elevation_at(station, satellite, next_t);

        match (
            pass_open,
            prev_el >= min_elevation_deg,
            next_el >= min_elevation_deg,
        ) {
            // Rising edge: refine the boundary.
            (None, false, true) => {
                let start = refine_crossing(station, satellite, t, next_t, min_elevation_deg);
                pass_open = Some(start);
            }
            // Setting edge: refine, build the Pass, push.
            (Some(open_at), true, false) => {
                let end = refine_crossing(station, satellite, t, next_t, min_elevation_deg);
                if let Some(p) = build_pass(station, satellite, open_at, end) {
                    passes.push(p);
                }
                pass_open = None;
            }
            _ => {}
        }

        prev_el = next_el;
        t = next_t;
    }

    // Pass still open at `to` — emit it with end clamped.
    if let Some(open_at) = pass_open
        && let Some(p) = build_pass(station, satellite, open_at, to)
    {
        passes.push(p);
    }

    passes
}

// ─── Internals ────────────────────────────────────────────────────────

fn elevation_at(station: &GroundStation, satellite: &Satellite, when: DateTime<Utc>) -> f64 {
    track(station, satellite, when).map_or(f64::NEG_INFINITY, |t| t.elevation_deg)
}

/// Bisect between `lo` (below threshold) and `hi` (above threshold) to
/// find the moment elevation crosses `threshold_deg`, to within
/// [`REFINE_PRECISION`]. Returns `hi` if bisection bottoms out before
/// the precision target.
fn refine_crossing(
    station: &GroundStation,
    satellite: &Satellite,
    lo: DateTime<Utc>,
    hi: DateTime<Utc>,
    threshold_deg: f64,
) -> DateTime<Utc> {
    let mut lo = lo;
    let mut hi = hi;
    for _ in 0..MAX_REFINE_ITERATIONS {
        if hi - lo <= REFINE_PRECISION {
            return hi;
        }
        let mid = lo + (hi - lo) / 2;
        if elevation_at(station, satellite, mid) >= threshold_deg {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    hi
}

/// Walk a fine grid between `start` and `end`, find the maximum
/// elevation, and pull the AOS/LOS azimuths. Returns `None` only if
/// SGP4 propagation fails at *both* AOS and LOS — `elevation_at`
/// handles transient propagation failures gracefully so this is rare.
fn build_pass(
    station: &GroundStation,
    satellite: &Satellite,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Option<Pass> {
    let aos = track(station, satellite, start).ok()?;
    let los = track(station, satellite, end).ok()?;

    let mut max_el = aos.elevation_deg.max(los.elevation_deg);
    let mut max_t = if aos.elevation_deg >= los.elevation_deg {
        start
    } else {
        end
    };
    let mut t = start + FINE_STEP;
    while t < end {
        let el = elevation_at(station, satellite, t);
        if el > max_el {
            max_el = el;
            max_t = t;
        }
        t += FINE_STEP;
    }

    Some(Pass {
        satellite: satellite.name().to_string(),
        start,
        end,
        max_elevation_deg: max_el,
        max_el_time: max_t,
        start_az_deg: aos.azimuth_deg,
        end_az_deg: los.azimuth_deg,
    })
}

#[inline]
fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

#[inline]
fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[inline]
fn norm(v: [f64; 3]) -> f64 {
    dot(v, v).sqrt()
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::unwrap_used)]
mod tests {
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
        let passes = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG);
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
        let passes = upcoming_passes(&station, &sat, from, to, TEST_MIN_ELEVATION_DEG);
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
        assert!(upcoming_passes(&station, &sat, t, t, TEST_MIN_ELEVATION_DEG).is_empty());
        assert!(
            upcoming_passes(
                &station,
                &sat,
                t,
                t - Duration::seconds(1),
                TEST_MIN_ELEVATION_DEG
            )
            .is_empty()
        );
    }
}
