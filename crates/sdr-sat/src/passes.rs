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
    EARTH_ROTATION_RAD_PER_SEC, Satellite, SatelliteError, TLE_MAX_AGE, TleFreshness, ecef_to_enu,
    eci_to_ecef, geodetic_to_ecef, gmst_rad,
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

/// Inclusive WGS84 geodetic latitude range (degrees).
pub(crate) const LAT_RANGE_DEG: std::ops::RangeInclusive<f64> = -90.0..=90.0;

/// Inclusive WGS84 geodetic longitude range (degrees).
pub(crate) const LON_RANGE_DEG: std::ops::RangeInclusive<f64> = -180.0..=180.0;

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

    /// Validating constructor: coordinates must be finite, latitude in
    /// [−90, 90], longitude in [−180, 180].
    ///
    /// # Errors
    ///
    /// Returns [`SatelliteError::InvalidStation`] otherwise.
    pub fn try_new(lat_deg: f64, lon_deg: f64, alt_m: f64) -> Result<Self, SatelliteError> {
        let station = Self::new(lat_deg, lon_deg, alt_m);
        station.validate()?;
        Ok(station)
    }

    /// Check the coordinates are finite and within WGS84 ranges.
    ///
    /// # Errors
    ///
    /// Returns [`SatelliteError::InvalidStation`] describing the first bad field.
    pub fn validate(&self) -> Result<(), SatelliteError> {
        let bad = |message: String| SatelliteError::InvalidStation { message };
        if !self.lat_deg.is_finite() || !LAT_RANGE_DEG.contains(&self.lat_deg) {
            return Err(bad(format!(
                "latitude {} out of [{}, {}]",
                self.lat_deg,
                LAT_RANGE_DEG.start(),
                LAT_RANGE_DEG.end()
            )));
        }
        if !self.lon_deg.is_finite() || !LON_RANGE_DEG.contains(&self.lon_deg) {
            return Err(bad(format!(
                "longitude {} out of [{}, {}]",
                self.lon_deg,
                LON_RANGE_DEG.start(),
                LON_RANGE_DEG.end()
            )));
        }
        if !self.alt_m.is_finite() {
            return Err(bad(format!("altitude {} is not finite", self.alt_m)));
        }
        Ok(())
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
    /// Age of the TLE elements at this pass's AOS — how much to trust
    /// the prediction (#718).
    pub tle_age: Duration,
}

impl Pass {
    /// Trust band of the elements this pass was predicted from.
    #[must_use]
    pub fn tle_freshness(&self) -> TleFreshness {
        TleFreshness::for_age(self.tle_age)
    }
}

/// Compute the satellite's current az/el/range/Doppler relative to the
/// ground station.
///
/// # Errors
///
/// Propagates [`SatelliteError`] from the underlying SGP4 propagator, and
/// returns [`SatelliteError::InvalidStation`] for non-finite / out-of-range
/// station coordinates (which would otherwise surface as a bogus 90°
/// elevation, #717).
pub fn track(
    station: &GroundStation,
    satellite: &Satellite,
    when: DateTime<Utc>,
) -> Result<Track, SatelliteError> {
    station.validate()?;
    let sat_eci = satellite.propagate(when)?;
    let sat_ecef = eci_to_ecef(sat_eci.position_km, when);
    let station_ecef = station.ecef_km();
    let relative_ecef = sub(sat_ecef, station_ecef);
    let enu = ecef_to_enu(relative_ecef, station.lat_deg, station.lon_deg);

    let range_km = norm(enu);
    if !range_km.is_finite() {
        return Err(SatelliteError::Propagation {
            name: satellite.name().to_string(),
            when,
            message: "non-finite range from SGP4 state".to_string(),
        });
    }
    // Azimuth: 0 = North, 90 = East, range [0, 360).
    let az_rad = enu[0].atan2(enu[1]);
    let azimuth_deg = az_rad.to_degrees().rem_euclid(360.0);
    // Elevation: positive above horizon. Zero range (satellite exactly
    // at the station) is the only way to reach the 90° branch now that
    // non-finite ranges are rejected above.
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

/// Whether `satellite` is on the ascending leg (heading north) at
/// `when`.
///
/// Computed by propagating to `when` and `when + Δ` (a few seconds),
/// converting both ECI positions to geocentric latitude, and comparing.
/// `Δ` is small enough that the satellite barely moves — gives a clean
/// derivative-via-finite-difference of latitude without needing the
/// orbital velocity vector.
///
/// **Why this exists:** NOAA APT lines transmit in time order. For a
/// descending pass (heading south) the first received line is the
/// northernmost, so the assembled image is naturally "right-side up"
/// (north at top). For an ascending pass the first line is the
/// southernmost — the image is upside-down AND mirrored east/west.
/// Rotating each video channel 180° (per noaa-apt's
/// `processing::rotate`) fixes both.
///
/// # Errors
///
/// Returns [`SatelliteError`] if either propagation fails.
pub fn is_ascending(satellite: &Satellite, when: DateTime<Utc>) -> Result<bool, SatelliteError> {
    /// Sampling delta — 30 s gives a few hundred km of ground-track
    /// movement (enough for a reliable lat-rate-of-change sign), well
    /// inside any realistic SGP4 epoch budget.
    const DELTA: chrono::Duration = chrono::Duration::seconds(30);

    let later = when
        .checked_add_signed(DELTA)
        .ok_or_else(|| time_out_of_range(satellite, when))?;
    let s0 = satellite.propagate(when)?;
    let s1 = satellite.propagate(later)?;

    // Geocentric latitude (in radians) from ECI position. The exact
    // ECEF/geodetic latitude differs by a fraction of a degree near
    // the equator due to Earth's oblateness — we only care about the
    // SIGN of the change, which is preserved under either convention.
    let lat = |p: [f64; 3]| -> f64 {
        let r = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
        if r > 0.0 { (p[2] / r).asin() } else { 0.0 }
    };
    Ok(lat(s1.position_km) > lat(s0.position_km))
}

/// Enumerate all overhead passes of `satellite` from `from` to `to`
/// (inclusive of `from`, exclusive of `to`) with peak elevation at
/// or above `min_elevation_deg`.
///
/// Passes that are already in progress at `from` are still returned —
/// their `start` is whatever moment the satellite first cleared
/// `min_elevation_deg` *within* the window. Symmetrically, passes that
/// haven't fully ended by `to` are returned with `end == to`.
///
/// The window is clamped at the elements' expiry horizon
/// (`epoch + TLE_MAX_AGE`): nothing is predicted past it, and each
/// returned pass carries the element age at its own AOS (#718).
///
/// # Errors
///
/// * [`SatelliteError::TleExpired`] — the elements are already older
///   than [`crate::sgp4_core::TLE_MAX_AGE`] at `from` (#718).
/// * [`SatelliteError::Propagation`] — SGP4 failed or returned a
///   non-finite state somewhere in the window. Earlier versions mapped
///   that to "below the horizon", which fabricated a setting edge plus
///   a phantom second pass when a decayed TLE failed mid-window, and
///   made a window that failed entirely look like a quiet sky (#719).
/// * [`SatelliteError::InvalidStation`] — `station` is out of range.
pub fn upcoming_passes(
    station: &GroundStation,
    satellite: &Satellite,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    min_elevation_deg: f64,
) -> Result<Vec<Pass>, SatelliteError> {
    if to <= from {
        return Ok(Vec::new());
    }
    satellite.check_not_expired(from)?;
    let to = satellite
        .epoch()
        .checked_add_signed(TLE_MAX_AGE)
        .map_or(to, |horizon| to.min(horizon));
    if to <= from {
        return Ok(Vec::new());
    }
    scan_window(station, satellite, from, to, min_elevation_deg)
}

/// The coarse scan + refinement behind [`upcoming_passes`], on a
/// window already validated and clamped to the elements' horizon.
fn scan_window(
    station: &GroundStation,
    satellite: &Satellite,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    min_elevation_deg: f64,
) -> Result<Vec<Pass>, SatelliteError> {
    let mut passes = Vec::new();
    let mut t = from;
    let mut prev_el = elevation_at(station, satellite, t)?;
    let mut pass_open: Option<DateTime<Utc>> = if prev_el >= min_elevation_deg {
        // Window starts mid-pass — clamp the start to the window edge.
        Some(from)
    } else {
        None
    };

    while t < to {
        // Saturate at `to` rather than panic at the edge of chrono's
        // range (#720).
        let next_t = t.checked_add_signed(COARSE_STEP).map_or(to, |n| n.min(to));
        let next_el = elevation_at(station, satellite, next_t)?;

        match (
            pass_open,
            prev_el >= min_elevation_deg,
            next_el >= min_elevation_deg,
        ) {
            // Rising edge: refine the boundary.
            (None, false, true) => {
                let start =
                    refine_crossing(station, satellite, t, next_t, min_elevation_deg, true)?;
                pass_open = Some(start);
            }
            // Setting edge: refine, build the Pass, push.
            (Some(open_at), true, false) => {
                let end = refine_crossing(station, satellite, t, next_t, min_elevation_deg, false)?;
                passes.push(build_pass(station, satellite, open_at, end)?);
                pass_open = None;
            }
            _ => {}
        }

        prev_el = next_el;
        t = next_t;
    }

    // Pass still open at `to` — emit it with end clamped. A rise that
    // refined to `to` itself (the clamped expiry horizon, typically)
    // would be a zero-duration pass: skip it.
    if let Some(open_at) = pass_open
        && open_at < to
    {
        passes.push(build_pass(station, satellite, open_at, to)?);
    }

    Ok(passes)
}

// ─── Internals ────────────────────────────────────────────────────────

fn time_out_of_range(satellite: &Satellite, when: DateTime<Utc>) -> SatelliteError {
    SatelliteError::Propagation {
        name: satellite.name().to_string(),
        when,
        message: "time is at the edge of the representable range".to_string(),
    }
}

fn elevation_at(
    station: &GroundStation,
    satellite: &Satellite,
    when: DateTime<Utc>,
) -> Result<f64, SatelliteError> {
    track(station, satellite, when).map(|t| t.elevation_deg)
}

/// Bisect between `lo` and `hi` (one coarse step apart) to find the
/// moment elevation crosses `threshold_deg`, to within
/// [`REFINE_PRECISION`]. `rising` says which side is above the threshold:
/// for an AOS edge `lo` is below and `hi` above; for an LOS edge it is the
/// reverse. The search keeps the crossing bracketed either way — using
/// the rising-edge rule on a setting edge collapsed every LOS to an
/// endpoint of the 60 s bucket (#716). Returns `hi` if bisection bottoms
/// out before the precision target.
fn refine_crossing(
    station: &GroundStation,
    satellite: &Satellite,
    lo: DateTime<Utc>,
    hi: DateTime<Utc>,
    threshold_deg: f64,
    rising: bool,
) -> Result<DateTime<Utc>, SatelliteError> {
    let mut lo = lo;
    let mut hi = hi;
    for _ in 0..MAX_REFINE_ITERATIONS {
        if hi - lo <= REFINE_PRECISION {
            return Ok(hi);
        }
        let mid = lo + (hi - lo) / 2;
        let above = elevation_at(station, satellite, mid)? >= threshold_deg;
        // Rising: the crossing is before `mid` when `mid` is already above.
        // Setting: the crossing is after `mid` while `mid` is still above.
        if above == rising {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Ok(hi)
}

/// Walk a fine grid between `start` and `end`, find the maximum
/// elevation, and pull the AOS/LOS azimuths. Any propagation failure
/// is the caller's error — there is no "graceful" value for a pass
/// whose peak could not be computed (#719).
fn build_pass(
    station: &GroundStation,
    satellite: &Satellite,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Pass, SatelliteError> {
    let aos = track(station, satellite, start)?;
    let los = track(station, satellite, end)?;

    let mut max_el = aos.elevation_deg.max(los.elevation_deg);
    let mut max_t = if aos.elevation_deg >= los.elevation_deg {
        start
    } else {
        end
    };
    let mut t = start.checked_add_signed(FINE_STEP).unwrap_or(end);
    while t < end {
        let el = elevation_at(station, satellite, t)?;
        if el > max_el {
            max_el = el;
            max_t = t;
        }
        t = t.checked_add_signed(FINE_STEP).unwrap_or(end);
    }

    Ok(Pass {
        satellite: satellite.name().to_string(),
        start,
        end,
        max_elevation_deg: max_el,
        max_el_time: max_t,
        start_az_deg: aos.azimuth_deg,
        end_az_deg: los.azimuth_deg,
        tle_age: satellite.age_at(start),
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
mod tests;
