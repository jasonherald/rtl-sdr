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

// Per-protocol allowed-band lookup lives on `ImagingProtocol::allowed_bands_hz`
// below — APT/LRPT pin to 137-138 MHz; SSTV accepts both VHF 2m (145.8 legacy)
// and UHF 70cm (437.550 current). The previous single-band IMAGING_BAND_MIN_HZ /
// MAX_HZ constants couldn't represent SSTV's two-band reality once ARISS migrated
// to UHF (Series 31+ events on 437.550 MHz, see #638). The constants below are
// the single source of truth — `allowed_bands_hz()`, the catalog's ISS entry,
// and the test that pins the operational frequency all reference them rather
// than re-pasting the literals.

/// 137 MHz weather-satellite VHF slot (Hz, inclusive). NOAA APT and
/// Meteor-M LRPT both downlink in this band.
pub const WEATHER_SAT_137MHZ_BAND_HZ: (u64, u64) = (137_000_000, 138_000_000);

/// 2 m amateur band (Hz, inclusive) used historically for ARISS SSTV
/// at 145.800 MHz before the UHF migration (Series 31+, April 2026).
/// Kept in the SSTV allowed-bands list so the catalog can flip back
/// without code changes if a future ARISS series returns to 2 m.
pub const SSTV_VHF_2M_BAND_HZ: (u64, u64) = (144_000_000, 148_000_000);

/// 70 cm amateur band (Hz, inclusive). Current ARISS SSTV operating
/// band — Series 31 (April 2026) and Series 32 (May 2026) are both on
/// 437.550 MHz within this range.
pub const SSTV_UHF_70CM_BAND_HZ: (u64, u64) = (430_000_000, 440_000_000);

/// Current ARISS SSTV operational downlink (Hz). 437.550 MHz UHF
/// 70 cm. Pinned by `iss_catalog_targets_current_ariss_uhf_frequency`
/// — if a future ARISS series moves the frequency, the test FAILS
/// until this constant + the catalog entry are bumped together.
pub const ISS_SSTV_DOWNLINK_HZ: u64 = 437_550_000;

/// NORAD catalog id for the ISS / ZARYA. Used both by the catalog
/// entry and by tests that look the entry up.
pub const ISS_NORAD_ID: u32 = 25_544;

/// Common downlink for METEOR-M2 series LRPT (Hz). Both M2-3 and
/// M2-4 transmit on this channel. Centralized here so the catalog
/// rows + the CR-noted bandwidth assertion test agree on one value.
pub const METEOR_M2_LRPT_DOWNLINK_HZ: u64 = 137_900_000;

/// LRPT receive bandwidth for the METEOR-M2 series (Hz). Equals the
/// LRPT IF rate (`sdr_dsp::lrpt::SAMPLE_RATE_HZ = 144_000`) so the
/// VFO channel filter is bypassed (`bandwidth >= out_sample_rate`)
/// and the 108-kHz QPSK signal isn't chopped at the ±19 kHz cutoff
/// the previous default would have imposed.
pub const METEOR_M2_LRPT_BANDWIDTH_HZ: u32 = 144_000;

/// Imaging protocol the receiver should use for a given catalog
/// satellite. Drives the auto-record dispatch in
/// `sidebar::satellites_recorder` so APT vs LRPT vs SSTV each get
/// their own decoder + viewer without the recorder itself caring
/// about protocol details.
///
/// `None` on a [`KnownSatellite::imaging_protocol`] means "in the
/// catalog for pass-prediction display purposes, but auto-record
/// is not yet wired for this satellite's protocol." The recorder's
/// eligibility filter excludes those entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagingProtocol {
    /// NOAA Automatic Picture Transmission (analog FM with 2.4 kHz
    /// AM subcarrier on 137 MHz). Decoded by `sdr_dsp::apt::AptDecoder`
    /// + assembled by `sdr_radio::apt_image::AptImage`.
    Apt,
    /// Meteor-M Low-Rate Picture Transmission (QPSK + CCSDS framing
    /// on 137 MHz). Decoded by `sdr_dsp::lrpt::LrptDemod` +
    /// `sdr_lrpt::LrptPipeline`. Shipped in epic #469.
    Lrpt,
    /// Slow-Scan Television (FM audio with PLL pixel decode). Used
    /// during ARISS SSTV events from the ISS. Originally on the 2m
    /// amateur slot at 145.800 MHz; ARISS migrated to UHF 70cm at
    /// 437.550 MHz starting with Series 31 (April 2026). The
    /// catalog tracks the current operational frequency. Decoded by
    /// the `slowrx` crate via `sdr_radio::sstv_image`. Shipped in
    /// epic #472.
    Sstv,
}

impl ImagingProtocol {
    /// Frequency bands (Hz) permitted for this protocol's downlink.
    ///
    /// Returns one or more `(low, high)` inclusive ranges. Used by
    /// the catalog assertion to reject typoed or wrong-band
    /// frequencies (e.g. forgot to convert MHz → Hz, pasted a
    /// different satellite's value, used a band the protocol can't
    /// be transmitted on).
    ///
    /// - **APT** (NOAA): 137-138 MHz weather-sat slot only.
    /// - **LRPT** (Meteor-M): 137-138 MHz, same band as APT.
    /// - **SSTV** (ARISS): both the legacy 2m amateur slot
    ///   (144-148 MHz, historically 145.800) and the current UHF 70cm
    ///   amateur slot (430-440 MHz, currently 437.550). Both are
    ///   valid; the active frequency is determined by the ARISS
    ///   event series.
    #[must_use]
    pub const fn allowed_bands_hz(&self) -> &'static [(u64, u64)] {
        match self {
            Self::Apt | Self::Lrpt => &[WEATHER_SAT_137MHZ_BAND_HZ],
            Self::Sstv => &[SSTV_VHF_2M_BAND_HZ, SSTV_UHF_70CM_BAND_HZ],
        }
    }
}

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
    /// Meteor-M, SSTV (currently 437.550 MHz UHF) for ISS during ARISS
    /// transmission events.
    /// Consumed by the Satellites panel's pass-row display and (in
    /// the upcoming #482b work) by the "tune to this satellite" play
    /// button.
    pub downlink_hz: u64,
    /// Demod mode the receiver should be in for this satellite.
    /// NFM for NOAA APT and ISS (wide-FM-style audio channels);
    /// LRPT for Meteor-M (the controller's `lrpt_decode_tap`
    /// drives the QPSK demod + FEC chain off the post-VFO IQ
    /// at 144 ksps, and the LRPT mode's silent-passthrough demod
    /// makes that the IF rate). Tracked as a field rather than
    /// hardcoded so a future amateur-band catalog addition
    /// (e.g. AO-92 with FM voice vs PSK telemetry) can choose
    /// differently without a special case in the wiring layer.
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
    /// Imaging protocol for auto-record dispatch. `None` means the
    /// satellite is in the catalog for pass-prediction display
    /// (so the user sees upcoming passes and can manually tune)
    /// but the auto-record path doesn't have a decoder + viewer
    /// for it yet. NOAA APT shipped in epic #468; Meteor LRPT
    /// shipped in Task 7 of epic #469; ISS SSTV shipped in epic
    /// #472 with `Some(Sstv)`.
    pub imaging_protocol: Option<ImagingProtocol>,
}

/// Built-in catalog. Order is the order the scheduler UI displays.
///
/// **Decommissioned / disabled satellites we deliberately omit:**
///
/// - **NOAA-15 / NOAA-18 / NOAA-19** (the legacy POES birds that historically
///   transmitted APT on 137 MHz) were decommissioned by NOAA in 2025:
///   NOAA-18 on 2025-06-06, NOAA-19 on 2025-08-13, NOAA-15 on 2025-08-19.
///   Their transmitters are powered off; the satellites remain in orbit
///   in a safe electrical state but transmit nothing. APT mode is no
///   longer broadcast by any operational satellite. Per
///   <https://www.ospo.noaa.gov/data/messages/2025/08/MSG_20250820_1410.html>.
///
/// - **METEOR-M 2 (NORAD 40069)** suffered a micrometeorite collision in
///   late 2022 and lost battery capacity. Per <https://usradioguy.com/meteor-satellite/>:
///   *"there is insufficient battery power to enable the LRPT stream.
///   HRPT transmissions ceased in July 2024."* The satellite is still
///   in orbit and tracked but cannot downlink imaging data — every pass
///   would queue an empty recording session.
///
/// We intentionally keep the APT decoder code (`sdr_dsp::apt`,
/// `sdr_radio::apt_image`, controller's `apt_decode_tap`) in place so
/// that any future Cubesat or amateur satellite that resurrects the
/// 137 MHz APT format can be added to the catalog without re-porting
/// the decoder. The LRPT decoder + Meteor catalog stay live for the
/// active M2-3 / M2-4 birds.
pub const KNOWN_SATELLITES: &[KnownSatellite] = &[
    // Meteor-M LRPT — epic #469. Both M2-3 and M2-4 transmit on
    // 137.900 MHz with 72 ksym/s QPSK and APIDs 64/65/67. They're in
    // different orbital planes so they don't conflict simultaneously.
    // `imaging_protocol: Some(Lrpt)` enrolls these in the
    // auto-record flow per epic #469 task 7. The recorder
    // constructor's `supported_protocols` set now includes
    // `Lrpt`, the wiring layer's `interpret_action` opens the
    // LRPT viewer + signals the DSP to attach the decoder, and
    // the LOS save walks every decoded APID into a per-pass
    // directory.
    //
    // METEOR-M 2 (40069) is intentionally absent — battery damage from
    // a 2022 micrometeorite collision means it can't power the LRPT
    // downlink. See doc comment on `KNOWN_SATELLITES` above.
    KnownSatellite {
        name: "METEOR-M2 3",
        norad_id: 57_166,
        downlink_hz: METEOR_M2_LRPT_DOWNLINK_HZ,
        demod_mode: sdr_types::DemodMode::Lrpt,
        // See `METEOR_M2_LRPT_BANDWIDTH_HZ` for the bypass-the-VFO
        // rationale; both M2-3 and M2-4 share the channel.
        bandwidth_hz: METEOR_M2_LRPT_BANDWIDTH_HZ,
        imaging_protocol: Some(ImagingProtocol::Lrpt),
    },
    KnownSatellite {
        // METEOR-M2 4 launched in 2024 and is actively transmitting
        // LRPT — same downlink as M2-3 (137.900 MHz, 72 kbaud, APIDs
        // 64/65/67), different orbital plane so the two never contend
        // for the same pass.
        //
        // **NORAD id is 59051**, NOT 61024. The original #506
        // exclusion and some hobbyist references quote 61024, which
        // is actually USA 403 — an unrelated classified satellite at
        // 70° inclination. Real METEOR-M2 4 sits at 98.7° polar
        // sun-sync (COSPAR 2024-039A) and lives in Celestrak's
        // weather group. Source:
        // <https://celestrak.org/NORAD/elements/gp.php?GROUP=weather&FORMAT=tle>
        // and operational status per
        // <https://usradioguy.com/meteor-satellite/>.
        name: "METEOR-M2 4",
        norad_id: 59_051,
        downlink_hz: METEOR_M2_LRPT_DOWNLINK_HZ,
        demod_mode: sdr_types::DemodMode::Lrpt,
        bandwidth_hz: METEOR_M2_LRPT_BANDWIDTH_HZ,
        imaging_protocol: Some(ImagingProtocol::Lrpt),
    },
    // ISS SSTV — epic #472. Currently 437.550 MHz UHF (ARISS Series
    // 31+, April 2026 onward, see #638); the catalog tracks the live
    // operational frequency via `ISS_SSTV_DOWNLINK_HZ`. ISS rides
    // wide-FM so the standard NFM demod path captures it cleanly.
    // `imaging_protocol: Some(Sstv)` enrolls ISS in the auto-record
    // flow: at AOS the recorder opens the SSTV viewer and signals the
    // DSP to attach the `SstvDecoder`; at LOS the per-pass directory
    // is written via `Action::SaveSstvPass`. Audio recording is NOT
    // suppressed for SSTV — the user-toggle applies as usual (SSTV is
    // audible unlike LRPT's silent QPSK). Shipped in epic #472.
    KnownSatellite {
        name: "ISS (ZARYA)",
        norad_id: ISS_NORAD_ID,
        // ARISS migrated SSTV from the legacy 2m slot (145.800 MHz)
        // to UHF 70cm starting with Series 31 (April 2026), and Series
        // 32 (May 8-12, 2026) is also on 437.550. See #638.
        // Note: voice contacts and packet APRS still use 145.800/145.825;
        // this catalog entry is specifically for SSTV auto-record.
        downlink_hz: ISS_SSTV_DOWNLINK_HZ,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth_hz: DEFAULT_SATELLITE_BANDWIDTH_HZ,
        imaging_protocol: Some(ImagingProtocol::Sstv),
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
    fn known_satellites_cover_live_imaging_protocols() {
        // After the August 2025 NOAA POES decommissioning, the live
        // imaging protocols our catalog still ships are LRPT (Meteor-M
        // family) and SSTV (ISS / ARISS events). APT is preserved as
        // a decoder + protocol enum variant for any future Cubesat
        // resurrection, but no satellite currently transmits APT, so
        // the catalog has no APT entries.
        let names: Vec<&str> = KNOWN_SATELLITES.iter().map(|s| s.name).collect();
        assert!(
            names.iter().any(|n| n.contains("METEOR")),
            "catalog should carry at least one METEOR (LRPT) entry",
        );
        assert!(
            names.iter().any(|n| n.contains("ISS")),
            "catalog should carry the ISS (SSTV) entry",
        );
    }

    #[test]
    fn decommissioned_noaa_poes_are_absent() {
        // NOAA-15, NOAA-18, NOAA-19 (the legacy POES birds that
        // historically transmitted 137 MHz APT) were decommissioned
        // in mid-2025. No live transmitters remain; the satellites
        // sit dark in orbit. Their entries are intentionally absent
        // so the auto-record path never fires daily empty WAV
        // recordings on dead birds.
        for &(norad_id, name) in &[
            (25_338_u32, "NOAA-15"),
            (28_654, "NOAA-18"),
            (33_591, "NOAA-19"),
        ] {
            assert!(
                !KNOWN_SATELLITES.iter().any(|s| s.norad_id == norad_id),
                "decommissioned {name} (NORAD {norad_id}) should not be in KNOWN_SATELLITES",
            );
        }
    }

    #[test]
    fn meteor_m2_4_is_present_and_lrpt() {
        // METEOR-M2 4 is NORAD 59051 (NOT 61024 — that's USA 403, an
        // unrelated classified satellite at 70° inclination). The
        // real M2-4 is in Celestrak's weather group at 98.7°
        // sun-sync, COSPAR 2024-039A, and is actively transmitting
        // LRPT on 137.900 MHz with the same APID set as M2-3. The
        // catalog ships it as `Some(Lrpt)` so the auto-record flow
        // fires on its passes.
        let m2_4 = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == 59_051)
            .expect("METEOR-M2 4 (NORAD 59051) should be in KNOWN_SATELLITES");
        assert_eq!(m2_4.downlink_hz, METEOR_M2_LRPT_DOWNLINK_HZ);
        assert_eq!(m2_4.demod_mode, sdr_types::DemodMode::Lrpt);
        // Pin the LRPT receive bandwidth so a regression to
        // `DEFAULT_SATELLITE_BANDWIDTH_HZ` (which the silent-fail
        // debug session showed chops the 108-kHz QPSK signal at
        // ±19 kHz) fails fast. Per CR round 1.
        assert_eq!(m2_4.bandwidth_hz, METEOR_M2_LRPT_BANDWIDTH_HZ);
        assert_eq!(m2_4.imaging_protocol, Some(ImagingProtocol::Lrpt));
        // Pin the wrong-id absence so a future copy-paste from a
        // stale source can't reintroduce 61024 (USA 403) under a
        // METEOR alias.
        assert!(
            !KNOWN_SATELLITES.iter().any(|s| s.norad_id == 61_024),
            "NORAD 61024 is USA 403, NOT METEOR-M2 4 — must not be in KNOWN_SATELLITES",
        );
    }

    #[test]
    fn meteor_m_2_is_excluded_due_to_battery_damage() {
        // METEOR-M 2 (NORAD 40069) suffered a 2022 micrometeorite
        // collision that depleted its batteries — per
        // <https://usradioguy.com/meteor-satellite/>: "there is
        // insufficient battery power to enable the LRPT stream".
        // HRPT also ceased July 2024. Excluded from KNOWN_SATELLITES
        // so the recorder never queues empty pass sessions on it.
        assert!(
            !KNOWN_SATELLITES.iter().any(|s| s.norad_id == 40_069),
            "METEOR-M 2 (NORAD 40069) should not be in KNOWN_SATELLITES — battery dead",
        );
    }

    #[test]
    fn known_satellites_have_protocol_compatible_downlinks() {
        // Catalog entries with an `imaging_protocol` must downlink
        // in one of that protocol's allowed bands. Catches typos
        // (forgot MHz → Hz), pasted-from-another-satellite values,
        // or accidentally putting an APT satellite on a UHF amateur
        // frequency. Entries with `imaging_protocol: None` are
        // skipped — the band rule only applies once a protocol is
        // committed to.
        for s in KNOWN_SATELLITES {
            let Some(proto) = s.imaging_protocol else {
                continue;
            };
            let in_band = proto
                .allowed_bands_hz()
                .iter()
                .any(|&(lo, hi)| (lo..=hi).contains(&s.downlink_hz));
            assert!(
                in_band,
                "{} ({:?}) downlink {} Hz is outside any band allowed for that protocol: {:?}",
                s.name,
                proto,
                s.downlink_hz,
                proto.allowed_bands_hz(),
            );
        }
    }

    #[test]
    fn imaging_protocol_allowed_bands_are_well_formed() {
        // Pin the per-protocol allowed-band semantics so a future
        // edit of the lookup table can't silently break the band
        // assertion above. Each band must have low <= high, and
        // the union must be non-empty for every variant.
        for proto in [
            ImagingProtocol::Apt,
            ImagingProtocol::Lrpt,
            ImagingProtocol::Sstv,
        ] {
            let bands = proto.allowed_bands_hz();
            assert!(!bands.is_empty(), "{proto:?} has empty allowed-band list");
            for &(lo, hi) in bands {
                assert!(lo <= hi, "{proto:?} has malformed band ({lo} > {hi})");
            }
        }
    }

    #[test]
    fn iss_catalog_targets_current_ariss_uhf_frequency() {
        // Pin the ISS catalog entry to the active ARISS SSTV
        // frequency. ARISS migrated from VHF 145.800 to UHF 437.550
        // starting Series 31 (April 2026); Series 32 (May 8-12,
        // 2026) is also on 437.550. If a future series moves the
        // frequency again, this test FAILS until the catalog is
        // bumped — which is the desired behavior, since stale
        // catalog entries record dead air during the event.
        // Lookup is by NORAD id (25544) rather than name-substring
        // so a future catalog rename of the ISS display name (e.g.
        // dropping "ZARYA") doesn't silently make this assertion
        // skip the entry.
        let iss = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == ISS_NORAD_ID)
            .expect("ISS catalog entry (NORAD 25544)");
        assert_eq!(
            iss.downlink_hz, ISS_SSTV_DOWNLINK_HZ,
            "ISS catalog entry should be 437.550 MHz (ARISS Series 31+ UHF)",
        );
    }

    #[test]
    fn known_satellites_have_expected_protocol_assignments() {
        // Pin the catalog's protocol assignments so a future
        // catalog edit can't silently change the auto-record
        // dispatch. The recorder's eligibility filter keys on
        // `imaging_protocol.is_some()`, so flipping a satellite
        // from None → Some (or vice versa) IS a behavior change
        // that should fail this test.
        //
        // NOAA legacy POES (15/18/19) were decommissioned in 2025
        // and are absent from the catalog — `decommissioned_noaa_poes_are_absent`
        // pins that. Any future Cubesat resurrecting APT would re-add
        // an entry with `Some(ImagingProtocol::Apt)`; the per-protocol
        // band check would gate the downlink frequency.

        // METEOR satellites → Lrpt (epic #469 task 7). Both
        // METEOR-M 2 and METEOR-M2 3 ship with Some(Lrpt) once
        // the live LRPT viewer + decoder driver are wired.
        let meteors: Vec<&KnownSatellite> = KNOWN_SATELLITES
            .iter()
            .filter(|s| s.name.starts_with("METEOR"))
            .collect();
        assert!(
            !meteors.is_empty(),
            "catalog regression: no METEOR entries found",
        );
        for s in meteors {
            assert_eq!(
                s.imaging_protocol,
                Some(ImagingProtocol::Lrpt),
                "{} should be Lrpt (Meteor LRPT shipped in epic #469 task 7)",
                s.name,
            );
        }
        // ISS → Some(Sstv). Shipped in epic #472. The `slowrx`-backed
        // SSTV decoder + viewer + per-pass directory save are all wired
        // end-to-end, so the catalog entry flips from None to Some(Sstv).
        let iss = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == ISS_NORAD_ID)
            .expect("ISS catalog entry (NORAD 25544)");
        assert_eq!(
            iss.imaging_protocol,
            Some(ImagingProtocol::Sstv),
            "ISS should be Some(Sstv) after epic #472"
        );
    }
}
