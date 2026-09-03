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
pub use passes::{GroundStation, Pass, Track, is_ascending, track, upcoming_passes};
pub use postal_lookup::{PostalLocation, PostalLookupError, lookup_us_zip};
pub use sgp4_core::{Satellite, SatelliteError, TLE_MAX_AGE, TLE_WARN_AGE, TleFreshness};
pub use tle_cache::{TleCache, TleCacheError, celestrak_gp_url};

mod catalog;
mod types;

pub use catalog::{
    AO_7_DOWNLINK_HZ, AO_7_NORAD_ID, DEFAULT_SATELLITE_BANDWIDTH_HZ, HAM_SSB_BANDWIDTH_HZ,
    HAM_UHF_70CM_BAND_HZ, HAM_VHF_2M_BAND_HZ, HAM_VOICE_NFM_BANDWIDTH_HZ, ISS_NORAD_ID,
    ISS_SSTV_DOWNLINK_HZ, KNOWN_SATELLITES, METEOR_M2_3_EXPECTED_LRPT_APIDS, METEOR_M2_3_NORAD_ID,
    METEOR_M2_4_EXPECTED_LRPT_APIDS, METEOR_M2_4_NORAD_ID, METEOR_M2_DECOMMISSIONED_NORAD_ID,
    METEOR_M2_LRPT_BANDWIDTH_HZ, METEOR_M2_LRPT_DOWNLINK_HZ, PO_101_DOWNLINK_HZ, PO_101_NORAD_ID,
    SO_50_DOWNLINK_HZ, SO_50_NORAD_ID, USA_403_WRONG_METEOR_NORAD_ID,
};
pub use types::{
    ImagingProtocol, KnownSatellite, LrptModulation, SSTV_UHF_70CM_BAND_HZ, SSTV_VHF_2M_BAND_HZ,
    WEATHER_SAT_137MHZ_BAND_HZ,
};

/// Install the pure-Rust `ring` `CryptoProvider` as the process-wide
/// rustls default, idempotently.
///
/// reqwest 0.13's `rustls-no-provider` feature (see the workspace
/// `Cargo.toml` comment) makes `Client::builder().build()` **panic**
/// unless a provider was installed first — it calls
/// `CryptoProvider::get_default()` rather than picking one from crate
/// features. Call this immediately before every client construction;
/// `install_default` returns `Err` when a provider is already set,
/// which is exactly the "someone else got there first" case we want
/// to ignore. Per `CodeRabbit` round 1 on PR #691.
pub(crate) fn ensure_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tls_provider_tests {
    #[test]
    fn blocking_client_builds_after_ensure_tls_provider() {
        // Regression: without an installed provider reqwest panics here.
        super::ensure_tls_provider();
        super::ensure_tls_provider(); // idempotent
        assert!(reqwest::blocking::Client::builder().build().is_ok());
    }
}
