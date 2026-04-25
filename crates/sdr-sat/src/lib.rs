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

/// Default channel bandwidth (Hz) for every catalog entry. APT,
/// LRPT, and ISS SSTV all need ~38 kHz of headroom past the NFM
/// 12.5 kHz default to capture the full subcarrier spectrum without
/// clipping the brighter / darker extremes. Hoisted to a module
/// constant so the same number doesn't get pasted into every
/// catalog row — and so a future re-tune of the default applies
/// everywhere consistently.
pub const DEFAULT_SATELLITE_BANDWIDTH_HZ: u32 = 38_000;

/// VHF imaging-band lower bound (Hz). Every catalog satellite's
/// downlink must land in [`IMAGING_BAND_MIN_HZ`,
/// [`IMAGING_BAND_MAX_HZ`]] — checked at compile time-ish via the
/// unit test below. Captures the standard 137 MHz weather-sat band
/// plus the 145.8 MHz ISS SSTV / amateur-satellite slot.
pub const IMAGING_BAND_MIN_HZ: u64 = 137_000_000;
/// VHF imaging-band upper bound (Hz). See [`IMAGING_BAND_MIN_HZ`].
pub const IMAGING_BAND_MAX_HZ: u64 = 148_000_000;

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
    /// Downlink centre frequency, Hz. Targets the satellite's primary
    /// imaging signal — APT (137.x MHz) for NOAA, LRPT (137.x MHz) for
    /// Meteor-M, SSTV (145.8 MHz) for ISS during transmission events.
    /// Consumed by the Satellites panel's pass-row display and (in
    /// the upcoming #482b work) by the "tune to this satellite" play
    /// button.
    pub downlink_hz: u64,
    /// Demod mode the receiver should be in for this satellite. NFM
    /// for everything we ship today: NOAA APT, Meteor LRPT, and ISS
    /// SSTV all ride wide-FM-style channels and our demod chain
    /// handles them through the same NFM front-end with a wider
    /// channel filter. Tracked as a field rather than hardcoded so a
    /// future amateur-band catalog addition (e.g. AO-92 with FM voice
    /// vs PSK telemetry) can choose differently without a special
    /// case in the wiring layer.
    pub demod_mode: sdr_types::DemodMode,
    /// Channel bandwidth (Hz) the receiver should use for this
    /// satellite. APT / LRPT / SSTV all need ~38 kHz of headroom
    /// past the NFM default 12.5 kHz to capture the full subcarrier
    /// spectrum without clipping the brighter / darker extremes.
    /// Same per-satellite philosophy as `demod_mode` — the catalog
    /// entry is the single source of truth so the play button can
    /// dispatch a `SetBandwidth` without re-deriving the value from
    /// signal type.
    pub bandwidth_hz: u32,
}

/// Built-in catalog. Order is the order the scheduler UI displays.
pub const KNOWN_SATELLITES: &[KnownSatellite] = &[
    // NOAA APT — epic #468
    KnownSatellite {
        name: "NOAA 15",
        norad_id: 25_338,
        downlink_hz: 137_620_000,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth_hz: DEFAULT_SATELLITE_BANDWIDTH_HZ,
    },
    KnownSatellite {
        name: "NOAA 18",
        norad_id: 28_654,
        downlink_hz: 137_912_500,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth_hz: DEFAULT_SATELLITE_BANDWIDTH_HZ,
    },
    KnownSatellite {
        name: "NOAA 19",
        norad_id: 33_591,
        downlink_hz: 137_100_000,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth_hz: DEFAULT_SATELLITE_BANDWIDTH_HZ,
    },
    // Meteor-M LRPT — epic #469. Note: METEOR-M 2 and NOAA 19 share
    // the 137.100 MHz channel — they're never on simultaneously by
    // design. The pass scheduler picks whichever is overhead.
    KnownSatellite {
        name: "METEOR-M 2",
        norad_id: 40_069,
        downlink_hz: 137_100_000,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth_hz: DEFAULT_SATELLITE_BANDWIDTH_HZ,
    },
    KnownSatellite {
        name: "METEOR-M2 3",
        norad_id: 57_166,
        downlink_hz: 137_900_000,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth_hz: DEFAULT_SATELLITE_BANDWIDTH_HZ,
    },
    // METEOR-M2 4 (NORAD 61024) deliberately omitted — launched 2024,
    // failed shortly after, no usable TLE on Celestrak (404 from the
    // GP API). Per #506.
    // ISS SSTV — epic #472. 145.800 MHz is the primary downlink for
    // SSTV transmission events and the ARISS voice repeater; both
    // ride wide-FM and use the same tune-and-listen flow.
    KnownSatellite {
        name: "ISS (ZARYA)",
        norad_id: 25_544,
        downlink_hz: 145_800_000,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth_hz: DEFAULT_SATELLITE_BANDWIDTH_HZ,
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

    #[test]
    fn meteor_m2_4_is_dropped_from_catalog() {
        // NORAD 61024 (METEOR-M2 4) launched 2024 and failed shortly
        // after — Celestrak's GP API returns 404 for it. Per #506,
        // the entry is intentionally absent so refreshes don't
        // accumulate per-call warn logs for a satellite that will
        // never produce a pass.
        assert!(
            !KNOWN_SATELLITES.iter().any(|s| s.norad_id == 61_024),
            "METEOR-M2 4 (NORAD 61024) should not be in KNOWN_SATELLITES",
        );
    }

    #[test]
    fn known_satellites_have_imaging_band_downlinks() {
        // All catalog entries today are weather / imaging / SSTV
        // satellites that downlink in the 137-148 MHz VHF window.
        // Pin the range so a future entry with a wildly-wrong freq
        // (e.g. forgot to convert MHz → Hz, or pasted a different
        // satellite's value) trips this test rather than reaching
        // the user as a misconfigured tune button.
        for s in KNOWN_SATELLITES {
            assert!(
                (IMAGING_BAND_MIN_HZ..=IMAGING_BAND_MAX_HZ).contains(&s.downlink_hz),
                "{} downlink {} Hz is outside the {}-{} Hz VHF imaging band",
                s.name,
                s.downlink_hz,
                IMAGING_BAND_MIN_HZ,
                IMAGING_BAND_MAX_HZ,
            );
        }
    }
}
