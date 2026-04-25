//! Pure SGP4 propagation — TLE parsing, position/velocity propagation,
//! and the geometric helpers needed to go from the propagator's TEME
//! frame to topocentric az/el/range.
//!
//! No I/O, no clock queries, no allocations on the hot path. Wraps the
//! [`sgp4`] crate from crates.io (Vallado's reference implementation;
//! ships the AIAA test vectors and matches the canonical C++ port to
//! sub-metre accuracy).

use chrono::{DateTime, TimeZone, Utc};

// `TimeZone` is used by `Utc.from_utc_datetime` below.

/// Errors from TLE parsing or SGP4 propagation.
#[derive(Debug, thiserror::Error)]
pub enum SatelliteError {
    /// The TLE strings couldn't be parsed by the underlying SGP4 crate.
    #[error("invalid TLE for {name}: {message}")]
    InvalidTle {
        /// Name from the TLE's "0 NAME" line (or whatever the caller
        /// passed) so the message says *which* TLE failed.
        name: String,
        /// Stringified SGP4 parse error.
        message: String,
    },
    /// SGP4 propagation produced a non-physical result. This usually
    /// means the requested time is decades from the TLE's epoch — TLEs
    /// drift, propagation past ~2 weeks is unreliable, past ~1 year is
    /// nonsense.
    #[error("propagation failed for {name} at {when}: {message}")]
    Propagation {
        /// Satellite name for context.
        name: String,
        /// Time we tried to propagate to.
        when: DateTime<Utc>,
        /// Stringified SGP4 propagation error.
        message: String,
    },
}

/// One satellite's parsed TLE plus the SGP4 propagator built from it.
///
/// Construct via [`Satellite::from_tle`]; thereafter call
/// [`Satellite::propagate`] for any UTC time you need a position at.
/// The struct is `Clone` so it can be passed across thread boundaries
/// without lifetime gymnastics — propagating is a few hundred
/// floating-point ops, not worth Arc-wrapping.
#[derive(Debug, Clone)]
pub struct Satellite {
    name: String,
    constants: sgp4::Constants,
    /// UTC moment the TLE's elements describe — used to translate any
    /// future propagation time into "minutes since epoch", which is
    /// what the SGP4 propagator wants.
    epoch: DateTime<Utc>,
}

impl Satellite {
    /// Parse a two-line element set into a propagator.
    ///
    /// `name` is the satellite display name (e.g. `"NOAA 19"`). `line1`
    /// and `line2` are the two TLE lines exactly as they appear in
    /// Celestrak files; trailing newlines are tolerated.
    ///
    /// # Errors
    ///
    /// Returns [`SatelliteError::InvalidTle`] if either line fails the
    /// SGP4 crate's parser (bad checksum, malformed numeric fields,
    /// missing whitespace at the wrong column, etc.).
    pub fn from_tle(name: &str, line1: &str, line2: &str) -> Result<Self, SatelliteError> {
        let elements = sgp4::Elements::from_tle(
            Some(name.to_string()),
            line1.trim_end_matches(['\r', '\n']).as_bytes(),
            line2.trim_end_matches(['\r', '\n']).as_bytes(),
        )
        .map_err(|e| SatelliteError::InvalidTle {
            name: name.to_string(),
            message: format!("{e}"),
        })?;
        let constants =
            sgp4::Constants::from_elements(&elements).map_err(|e| SatelliteError::InvalidTle {
                name: name.to_string(),
                message: format!("{e}"),
            })?;
        // The sgp4 crate parses the TLE epoch into a chrono NaiveDateTime
        // for us — wrap it as UTC.
        let epoch = Utc.from_utc_datetime(&elements.datetime);
        Ok(Self {
            name: name.to_string(),
            constants,
            epoch,
        })
    }

    /// Display name as supplied to [`Satellite::from_tle`].
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// UTC instant the underlying TLE's elements were valid at.
    /// Propagation accuracy degrades the further you get from this —
    /// keep it within ~2 weeks for sub-km accuracy.
    #[must_use]
    pub fn epoch(&self) -> DateTime<Utc> {
        self.epoch
    }

    /// Propagate the orbit to `when` and return the satellite's
    /// position + velocity in the **TEME** frame (treated as ECI for
    /// our purposes — the ~tens-of-arcseconds difference is negligible
    /// vs SGP4's ~km-level error budget).
    ///
    /// # Errors
    ///
    /// Returns [`SatelliteError::Propagation`] if the SGP4 propagator
    /// fails — usually because `when` is far enough from the epoch
    /// that the orbital elements no longer describe physical motion.
    pub fn propagate(&self, when: DateTime<Utc>) -> Result<EciState, SatelliteError> {
        let dt = when - self.epoch;
        // SGP4 wants minutes since epoch as f64. `num_microseconds`
        // is the most precise integer span chrono will give us; over
        // very long propagations (decades) it can return None — we
        // surface that as a propagation error since SGP4 is unreliable
        // way before then anyway.
        let dt_minutes = match dt.num_microseconds() {
            Some(us) => {
                #[allow(clippy::cast_precision_loss)]
                let us_f = us as f64;
                us_f / 60_000_000.0
            }
            None => {
                return Err(SatelliteError::Propagation {
                    name: self.name.clone(),
                    when,
                    message: "time delta exceeds f64 microsecond range".to_string(),
                });
            }
        };
        let prediction = self
            .constants
            .propagate(sgp4::MinutesSinceEpoch(dt_minutes))
            .map_err(|e| SatelliteError::Propagation {
                name: self.name.clone(),
                when,
                message: format!("{e}"),
            })?;
        Ok(EciState {
            position_km: prediction.position,
            velocity_km_s: prediction.velocity,
            when,
        })
    }
}

/// Position + velocity from a single SGP4 propagation, in the
/// TEME-≈-ECI frame. Position is km, velocity is km/s.
#[derive(Debug, Clone, Copy)]
pub struct EciState {
    /// Position vector \[x, y, z\] in km.
    pub position_km: [f64; 3],
    /// Velocity vector \[vx, vy, vz\] in km/s.
    pub velocity_km_s: [f64; 3],
    /// UTC instant the propagation was evaluated at.
    pub when: DateTime<Utc>,
}

// ─── Earth model + frame conversions ──────────────────────────────────

/// WGS84 semi-major axis in km. The reference ellipsoid every modern
/// geodesy stack uses; matches Celestrak / NORAD / USNO conventions.
pub const EARTH_RADIUS_KM: f64 = 6_378.137;
/// WGS84 flattening, dimensionless.
pub const EARTH_FLATTENING: f64 = 1.0 / 298.257_223_563;
/// Earth's sidereal rotation rate in rad/s (IERS conventions).
pub const EARTH_ROTATION_RAD_PER_SEC: f64 = 7.292_115_146_706_979e-5;

/// Greenwich Mean Sidereal Time at the given UTC instant, in radians,
/// reduced to `[0, 2π)`.
///
/// Uses the standard low-order polynomial (Vallado, "Fundamentals of
/// Astrodynamics and Applications", 4th ed., eq. 3-45). Accuracy is
/// well under 1 arcsec for any time in the 20th–21st centuries — way
/// more than SGP4 itself needs.
#[must_use]
pub fn gmst_rad(when: DateTime<Utc>) -> f64 {
    // Julian Date (UT1, but for our purposes UT1≈UTC to within a second
    // — UT1-UTC is bounded to |0.9 s| by leap-second insertion and
    // SGP4 doesn't care).
    let jd = julian_date_utc(when);
    let t = (jd - 2_451_545.0) / 36_525.0;
    // Vallado eq. 3-45 — GMST in seconds.
    let gmst_seconds =
        67_310.548_41 + (876_600.0 * 3_600.0 + 8_640_184.812_866) * t + 0.093_104 * t * t
            - 6.2e-6 * t.powi(3);
    let gmst_rad = (gmst_seconds % 86_400.0) / 86_400.0 * std::f64::consts::TAU;
    // Reduce to [0, 2π). `rem_euclid` handles negative values that
    // small-time offsets near J2000 can produce.
    gmst_rad.rem_euclid(std::f64::consts::TAU)
}

/// Rotate an ECI vector to ECEF by negating the GMST rotation about Z.
#[must_use]
pub fn eci_to_ecef(eci: [f64; 3], when: DateTime<Utc>) -> [f64; 3] {
    let g = gmst_rad(when);
    let (sin_g, cos_g) = g.sin_cos();
    [
        cos_g * eci[0] + sin_g * eci[1],
        -sin_g * eci[0] + cos_g * eci[1],
        eci[2],
    ]
}

/// Convert geodetic lat/lon/alt (degrees, degrees, metres) to ECEF
/// position (km), using the WGS84 ellipsoid.
#[must_use]
#[allow(clippy::similar_names)] // lat_deg / lon_deg / alt are intentional
pub fn geodetic_to_ecef(lat_deg: f64, lon_deg: f64, altitude_m: f64) -> [f64; 3] {
    let lat = lat_deg.to_radians();
    let lon = lon_deg.to_radians();
    let altitude_km = altitude_m / 1_000.0;
    let e2 = 2.0 * EARTH_FLATTENING - EARTH_FLATTENING * EARTH_FLATTENING;
    let n = EARTH_RADIUS_KM / (1.0 - e2 * lat.sin().powi(2)).sqrt();
    let (sin_lat, cos_lat) = lat.sin_cos();
    let (sin_lon, cos_lon) = lon.sin_cos();
    [
        (n + altitude_km) * cos_lat * cos_lon,
        (n + altitude_km) * cos_lat * sin_lon,
        (n * (1.0 - e2) + altitude_km) * sin_lat,
    ]
}

/// Rotate an ECEF vector relative to a station at `(lat, lon)` into
/// the station's topocentric ENU (East / North / Up) frame.
#[must_use]
pub fn ecef_to_enu(ecef: [f64; 3], lat_deg: f64, lon_deg: f64) -> [f64; 3] {
    let lat = lat_deg.to_radians();
    let lon = lon_deg.to_radians();
    let (sin_lat, cos_lat) = lat.sin_cos();
    let (sin_lon, cos_lon) = lon.sin_cos();
    [
        // East
        -sin_lon * ecef[0] + cos_lon * ecef[1],
        // North
        -sin_lat * cos_lon * ecef[0] - sin_lat * sin_lon * ecef[1] + cos_lat * ecef[2],
        // Up
        cos_lat * cos_lon * ecef[0] + cos_lat * sin_lon * ecef[1] + sin_lat * ecef[2],
    ]
}

/// Julian Date for a UTC chrono `DateTime`. Uses the standard
/// Gregorian-calendar formula (Vallado eq. 3-13).
#[allow(clippy::similar_names)]
fn julian_date_utc(when: DateTime<Utc>) -> f64 {
    use chrono::Datelike;
    use chrono::Timelike;
    let mut year_i = when.year();
    let mut month_i = i32::try_from(when.month()).unwrap_or(0);
    let day_i = i32::try_from(when.day()).unwrap_or(0);
    if month_i <= 2 {
        year_i -= 1;
        month_i += 12;
    }
    let year_f = f64::from(year_i);
    let month_f = f64::from(month_i);
    let day_f = f64::from(day_i);
    let century = (year_f / 100.0).floor();
    let leap_correction = 2.0 - century + (century / 4.0).floor();
    let jd_at_midnight = (365.25 * (year_f + 4716.0)).floor()
        + (30.6001 * (month_f + 1.0)).floor()
        + day_f
        + leap_correction
        - 1524.5;
    let day_fraction = (f64::from(when.num_seconds_from_midnight())
        + f64::from(when.nanosecond()) / 1e9)
        / 86_400.0;
    jd_at_midnight + day_fraction
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Vallado's TC0 reference TLE (Vanguard 1, NORAD 5, epoch 2000-06-27)
    /// — the canonical SGP4 verification test case from
    /// "Revisiting Spacetrack Report #3" (AIAA 2006-6753). Pinned here
    /// so the tests don't depend on network state and the line-checksum
    /// digits are guaranteed correct.
    const TEST_TLE_NAME: &str = "VANGUARD 1";
    const TEST_TLE_LINE1: &str =
        "1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753";
    const TEST_TLE_LINE2: &str =
        "2 00005  34.2682 348.7242 1859667 331.7664  19.3264 10.82419157413667";

    #[test]
    fn satellite_from_tle_parses_and_round_trips_name() {
        let sat = Satellite::from_tle(TEST_TLE_NAME, TEST_TLE_LINE1, TEST_TLE_LINE2).unwrap();
        assert_eq!(sat.name(), TEST_TLE_NAME);
    }

    #[test]
    fn satellite_from_tle_rejects_garbage() {
        let result = Satellite::from_tle("BAD", "not a tle line", "either");
        assert!(matches!(result, Err(SatelliteError::InvalidTle { .. })));
    }

    #[test]
    fn satellite_propagate_at_epoch_returns_finite_state() {
        let sat = Satellite::from_tle(TEST_TLE_NAME, TEST_TLE_LINE1, TEST_TLE_LINE2).unwrap();
        let state = sat.propagate(sat.epoch()).unwrap();
        let r = (state.position_km[0].powi(2)
            + state.position_km[1].powi(2)
            + state.position_km[2].powi(2))
        .sqrt();
        // Vanguard 1's orbit varies from ~7000 km perigee to ~10000 km
        // apogee (eccentricity 0.186 around a ~8600 km semi-major axis).
        // Either way the radius is bounded comfortably in this range.
        assert!(
            (5_000.0..15_000.0).contains(&r),
            "satellite radius at epoch out of range: {r:.1} km",
        );
        let v = (state.velocity_km_s[0].powi(2)
            + state.velocity_km_s[1].powi(2)
            + state.velocity_km_s[2].powi(2))
        .sqrt();
        // vis-viva for this orbit gives velocities in the 4–10 km/s range.
        assert!(
            (3.0..12.0).contains(&v),
            "satellite speed at epoch out of range: {v:.3} km/s",
        );
    }

    #[test]
    fn satellite_propagate_an_hour_later_moves() {
        let sat = Satellite::from_tle(TEST_TLE_NAME, TEST_TLE_LINE1, TEST_TLE_LINE2).unwrap();
        let s0 = sat.propagate(sat.epoch()).unwrap();
        let s1 = sat
            .propagate(sat.epoch() + chrono::Duration::hours(1))
            .unwrap();
        // 1 hour into the orbit (~half a revolution at this period) the
        // satellite must have moved a long way from where it started.
        let dx = s1.position_km[0] - s0.position_km[0];
        let dy = s1.position_km[1] - s0.position_km[1];
        let dz = s1.position_km[2] - s0.position_km[2];
        let displacement = (dx * dx + dy * dy + dz * dz).sqrt();
        assert!(
            displacement > 1_000.0,
            "expected > 1000 km displacement in 1 h, got {displacement:.1} km",
        );
    }

    #[test]
    fn gmst_at_j2000_matches_known_value() {
        // GMST at J2000 (2000-01-01 12:00:00 UTC) is 18h 41m 50.5s
        // sidereal time, which is 280.46° ≈ 4.8949612... rad.
        let j2000 = Utc.with_ymd_and_hms(2000, 1, 1, 12, 0, 0).unwrap();
        let g = gmst_rad(j2000);
        let expected = 4.894_961_212_735_793;
        assert!(
            (g - expected).abs() < 1e-3,
            "GMST(J2000) expected ≈ {expected:.6}, got {g:.6}",
        );
    }

    #[test]
    fn gmst_advances_by_one_sidereal_day_per_24h() {
        let t0 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let t1 = t0 + chrono::Duration::hours(24);
        let g0 = gmst_rad(t0);
        let g1 = gmst_rad(t1);
        // After 24 solar hours, sidereal time advances by 24h × (366.25/365.25)
        // ≈ 24h 03m 56s — i.e. ~0.0172 rad past a full revolution.
        let mut diff = g1 - g0;
        diff = diff.rem_euclid(std::f64::consts::TAU);
        // Expected sidereal advance modulo 2π is ≈ 0.01721 rad.
        assert!(
            (0.015..0.020).contains(&diff),
            "24h sidereal advance expected ≈ 0.0172 rad, got {diff:.5}",
        );
    }

    #[test]
    fn geodetic_to_ecef_at_equator_prime_meridian() {
        // (0°, 0°, 0 m) should land at (a, 0, 0).
        let p = geodetic_to_ecef(0.0, 0.0, 0.0);
        assert!((p[0] - EARTH_RADIUS_KM).abs() < 1e-6);
        assert!(p[1].abs() < 1e-6);
        assert!(p[2].abs() < 1e-6);
    }

    #[test]
    fn geodetic_to_ecef_at_north_pole() {
        // North pole at sea level: x=y=0, z = polar radius
        // = a * (1 - f) ≈ 6356.752 km.
        let p = geodetic_to_ecef(90.0, 0.0, 0.0);
        let polar_radius = EARTH_RADIUS_KM * (1.0 - EARTH_FLATTENING);
        assert!(p[0].abs() < 1e-6);
        assert!(p[1].abs() < 1e-6);
        assert!(
            (p[2] - polar_radius).abs() < 1e-3,
            "polar z expected {polar_radius:.6}, got {:.6}",
            p[2],
        );
    }

    #[test]
    fn ecef_to_enu_zenith_at_station() {
        // A point straight up from the station should have ENU
        // = (0, 0, +up). On an oblate ellipsoid, "up" is the surface
        // *normal* — not the geocentric radial — so we have to displace
        // along the geodetic-up unit vector, not along station_ecef.
        let lat: f64 = 40.0;
        let lon: f64 = -75.0;
        let lat_rad = lat.to_radians();
        let lon_rad = lon.to_radians();
        let up_unit = [
            lat_rad.cos() * lon_rad.cos(),
            lat_rad.cos() * lon_rad.sin(),
            lat_rad.sin(),
        ];
        let relative = [100.0 * up_unit[0], 100.0 * up_unit[1], 100.0 * up_unit[2]];
        let enu = ecef_to_enu(relative, lat, lon);
        // East and North components should be tiny, Up ≈ 100 km.
        assert!(enu[0].abs() < 1e-9, "east bias: {}", enu[0]);
        assert!(enu[1].abs() < 1e-9, "north bias: {}", enu[1]);
        assert!(
            (enu[2] - 100.0).abs() < 1e-9,
            "up expected 100 km, got {}",
            enu[2],
        );
    }
}
