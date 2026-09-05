//! Positional Orbcomm spacecraft identification: match a decoded
//! ephemeris sub-satellite position against SGP4-propagated candidate
//! TLEs. Pure — no I/O. The decoded timestamp is GPS-derived and
//! uncorrected (see `GPS_UTC_LEAP_SECONDS`), so propagation is done at
//! `when − leap`.

use chrono::{DateTime, Duration, Utc};

use crate::sgp4_core::{Satellite, eci_to_ecef, geodetic_to_ecef};

/// GPS−UTC offset (leap seconds). GPS time has no leap seconds; the
/// Orbcomm ephemeris timestamp is GPS-derived and uncorrected, so it
/// runs this many seconds ahead of true UTC. 18 s as of 2026-01; update
/// when a new leap second is announced. A wrong value only loosens
/// matches — the distance threshold absorbs small errors.
pub const GPS_UTC_LEAP_SECONDS: i64 = 18;

/// Default max ECEF distance (km) for a confident match.
pub const DEFAULT_MATCH_MAX_DIST_KM: f64 = 50.0;

/// The nearest candidate must be at least this many times closer than
/// the runner-up to be accepted (guards ambiguous overhead cases).
pub const MATCH_AMBIGUITY_MARGIN: f64 = 2.0;

/// A positional identification result.
#[derive(Debug, Clone, PartialEq)]
pub struct SpacecraftMatch {
    /// Matched TLE name line, e.g. `"ORBCOMM FM06"`.
    pub name: String,
    /// ECEF distance (km) between decoded and propagated position.
    pub distance_km: f64,
}

/// Identify a spacecraft from a decoded sub-satellite geodetic position
/// and timestamp. Converts to ECEF and delegates to
/// [`identify_from_ecef`]. `when` is the raw ephemeris timestamp (the
/// leap-second correction is applied internally).
#[must_use]
pub fn identify_spacecraft(
    lat_deg: f64,
    lon_deg: f64,
    alt_m: f64,
    when: DateTime<Utc>,
    candidates: &[(String, Satellite)],
    max_dist_km: f64,
) -> Option<SpacecraftMatch> {
    let target = geodetic_to_ecef(lat_deg, lon_deg, alt_m);
    identify_from_ecef(target, when, candidates, max_dist_km)
}

/// Core matcher over an ECEF target (km). Propagates each candidate to
/// `when − GPS_UTC_LEAP_SECONDS`, keeps the nearest and runner-up, and
/// returns the nearest iff it is within `max_dist_km` AND at least
/// `MATCH_AMBIGUITY_MARGIN`× closer than the runner-up.
fn identify_from_ecef(
    target_km: [f64; 3],
    when: DateTime<Utc>,
    candidates: &[(String, Satellite)],
    max_dist_km: f64,
) -> Option<SpacecraftMatch> {
    let prop_time = when - Duration::seconds(GPS_UTC_LEAP_SECONDS);
    let mut best: Option<(usize, f64)> = None;
    let mut runner_up_km = f64::INFINITY;
    for (i, (_, sat)) in candidates.iter().enumerate() {
        let Ok(eci) = sat.propagate(prop_time) else {
            continue;
        };
        let ecef = eci_to_ecef(eci.position_km, prop_time);
        let d = distance_km(ecef, target_km);
        match best {
            Some((_, bd)) if d >= bd => {
                if d < runner_up_km {
                    runner_up_km = d;
                }
            }
            _ => {
                if let Some((_, bd)) = best {
                    runner_up_km = bd;
                }
                best = Some((i, d));
            }
        }
    }
    let (idx, dist) = best?;
    if dist < max_dist_km && runner_up_km > dist * MATCH_AMBIGUITY_MARGIN {
        Some(SpacecraftMatch {
            name: candidates[idx].0.clone(),
            distance_km: dist,
        })
    } else {
        None
    }
}

fn distance_km(a: [f64; 3], b: [f64; 3]) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
