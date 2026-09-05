//! Positional Orbcomm spacecraft identification: match a decoded
//! ephemeris sub-satellite position against SGP4-propagated candidate
//! TLEs. Pure — no I/O. The decoded ephemeris's own timestamp is
//! unreliable (`sdr-orbcomm` issue #900 — it can be off by hours), so
//! the caller passes the *reception time* instead: a live-received
//! ephemeris reflects the satellite's current position, so propagating
//! candidate TLEs to "now" is the more trustworthy reference.

use chrono::{DateTime, Utc};

use crate::sgp4_core::{Satellite, eci_to_ecef, geodetic_to_ecef};

/// Default max ECEF distance (km) for a confident match.
pub const DEFAULT_MATCH_MAX_DIST_KM: f64 = 100.0;

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

/// Identify a spacecraft from a decoded sub-satellite geodetic position.
/// Converts to ECEF and delegates to [`identify_from_ecef`]. `when`
/// should be the reception time (e.g. `Utc::now()`), not the decoded
/// ephemeris timestamp — see the module docs.
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
/// `when`, keeps the nearest and runner-up, and returns the nearest iff
/// it is within `max_dist_km` AND at least `MATCH_AMBIGUITY_MARGIN`×
/// closer than the runner-up.
fn identify_from_ecef(
    target_km: [f64; 3],
    when: DateTime<Utc>,
    candidates: &[(String, Satellite)],
    max_dist_km: f64,
) -> Option<SpacecraftMatch> {
    let mut best: Option<(usize, f64)> = None;
    let mut runner_up_km = f64::INFINITY;
    for (i, (_, sat)) in candidates.iter().enumerate() {
        let Ok(eci) = sat.propagate(when) else {
            continue;
        };
        let ecef = eci_to_ecef(eci.position_km, when);
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
