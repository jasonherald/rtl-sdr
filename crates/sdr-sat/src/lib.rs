//! Satellite pass prediction service.
//!
//! Foundation crate for any feature that needs to know "where is
//! satellite X right now" or "when does satellite X next come overhead":
//! NOAA APT (epic #468), Meteor-M LRPT (#469), ISS SSTV (#472), and
//! whatever comes after.
//!
//! The crate is split into three layers:
//!
//! * [`sgp4_core`] — pure SGP4/SDP4 propagation. No I/O, no time-of-day
//!   queries, no allocator surprises in the hot path. Wraps the
//!   well-tested [`sgp4`] crate from crates.io and adds the geometry
//!   helpers we actually need (ECI → ECEF → station-frame az/el/range).
//! * [`passes`] — pass enumeration and real-time tracking. Pure
//!   functions over [`GroundStation`] + [`Satellite`] + time. Doppler
//!   shift is exposed via real-time tracking only — pass enumeration
//!   doesn't need it.
//! * [`tle_cache`] — fetches TLEs from Celestrak once a day and caches
//!   them under `~/.cache/sdr-rs/tle/`. Blocking reqwest call meant to
//!   be invoked from a worker thread; the rest of the crate has zero
//!   network awareness.
//!
//! Hard-coded NORAD IDs for the satellites we ship with are in
//! [`KNOWN_SATELLITES`] so callers don't need to look them up.

pub mod elevation;
pub mod passes;
pub mod postal_lookup;
pub mod sgp4_core;
pub mod tle_cache;

pub use elevation::{ElevationLookupError, lookup_elevation_m};
pub use passes::{GroundStation, Pass, Track, track, upcoming_passes};
pub use postal_lookup::{PostalLocation, PostalLookupError, lookup_us_zip};
pub use sgp4_core::{Satellite, SatelliteError};
pub use tle_cache::{TleCache, TleCacheError, celestrak_gp_url};

/// A satellite the user-facing scheduler ships with by default. The list
/// is intentionally tight — we want passes to "just work" for the most
/// common LEO weather / ham satellites without making the user paste
/// TLEs by hand.
#[derive(Debug, Clone, Copy)]
pub struct KnownSatellite {
    /// Display name, matches the Celestrak TLE name field exactly.
    pub name: &'static str,
    /// NORAD catalog number — the canonical satellite identifier.
    /// [`TleCache`] looks up TLEs by this id directly.
    pub norad_id: u32,
}

/// Built-in catalog. Order is the order the scheduler UI displays.
pub const KNOWN_SATELLITES: &[KnownSatellite] = &[
    // NOAA APT — epic #468
    KnownSatellite {
        name: "NOAA 15",
        norad_id: 25_338,
    },
    KnownSatellite {
        name: "NOAA 18",
        norad_id: 28_654,
    },
    KnownSatellite {
        name: "NOAA 19",
        norad_id: 33_591,
    },
    // Meteor-M LRPT — epic #469
    KnownSatellite {
        name: "METEOR-M 2",
        norad_id: 40_069,
    },
    KnownSatellite {
        name: "METEOR-M2 3",
        norad_id: 57_166,
    },
    KnownSatellite {
        name: "METEOR-M2 4",
        norad_id: 61_024,
    },
    // ISS SSTV — epic #472
    KnownSatellite {
        name: "ISS (ZARYA)",
        norad_id: 25_544,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_satellites_have_unique_norad_ids() {
        let mut ids: Vec<u32> = KNOWN_SATELLITES.iter().map(|s| s.norad_id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(
            ids.len(),
            before,
            "two entries in KNOWN_SATELLITES share a NORAD id",
        );
    }

    #[test]
    fn known_satellites_cover_all_three_epics() {
        let names: Vec<&str> = KNOWN_SATELLITES.iter().map(|s| s.name).collect();
        // NOAA APT
        assert!(names.iter().any(|n| n.contains("NOAA")));
        // Meteor-M LRPT
        assert!(names.iter().any(|n| n.contains("METEOR")));
        // ISS SSTV
        assert!(names.iter().any(|n| n.contains("ISS")));
    }
}
