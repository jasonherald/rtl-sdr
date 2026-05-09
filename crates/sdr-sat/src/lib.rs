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

/// Default channel bandwidth (Hz) for catalog entries that use the
/// standard NFM-style audio path — APT (decoder dormant pending a
/// future Cubesat) and ISS SSTV. Both need ~38 kHz of headroom past
/// the NFM 12.5 kHz default to capture the full subcarrier spectrum
/// without clipping the brighter / darker extremes.
///
/// **Not used by LRPT.** Meteor-M LRPT entries pin
/// `METEOR_M2_LRPT_BANDWIDTH_HZ` (144 kHz) to bypass the VFO
/// channel filter so the 108 kHz QPSK signal is preserved end-to-end
/// — using this default would chop the QPSK content at ±19 kHz and
/// prevent the demod from locking. Hoisted to a module constant so
/// the same number doesn't get pasted into every catalog row.
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

/// NORAD catalog id for METEOR-M2 3. Active LRPT downlink as of 2026.
pub const METEOR_M2_3_NORAD_ID: u32 = 57_166;

/// NORAD catalog id for METEOR-M2 4. Active LRPT downlink as of 2026 —
/// per #645 investigation, currently the easier first-decode target than
/// M2-3 because it transmits the standard channel format (c1/c2/c4)
/// `SatDump`'s presets expect.
pub const METEOR_M2_4_NORAD_ID: u32 = 59_051;

/// NORAD catalog id for METEOR-M 2 (the original; **excluded** from
/// `KNOWN_SATELLITES` due to battery damage from a 2022 micrometeorite
/// collision). Surface as a constant so the absence-pin test and any
/// future audit reference one canonical value.
pub const METEOR_M2_DECOMMISSIONED_NORAD_ID: u32 = 40_069;

/// NORAD catalog id for USA 403 (a classified satellite at 70°
/// inclination that some hobbyist references **incorrectly** quote as
/// the METEOR-M2 4 NORAD id). Surface as a constant so the "do not
/// reintroduce 61024 under a METEOR alias" absence-pin test is
/// self-documenting and can't drift from the original investigation.
pub const USA_403_WRONG_METEOR_NORAD_ID: u32 = 61_024;

/// NORAD catalog id for AMSAT-OSCAR-7 (AO-7). Launched 1974, the
/// oldest still-operational amateur satellite. Battery failed in
/// 1981; resurrected in 2002 when the short cleared, and runs on
/// solar power only — silent during eclipse, audible on the
/// sunlit half of every orbit. Carries a Mode-B linear transponder
/// (70cm uplink → 2m downlink, LSB / CW). Per AMSAT operational
/// status as of May 2026.
pub const AO_7_NORAD_ID: u32 = 7_530;

/// NORAD catalog id for SaudiSat-1C (SO-50). Single-channel FM
/// voice repeater on 70 cm downlink, 2 m uplink. Launched 2002,
/// still active for amateur QSO contacts as of May 2026. Per
/// AMSAT operational satellite list.
pub const SO_50_NORAD_ID: u32 = 27_607;

/// NORAD catalog id for Diwata-2 (PO-101). Filipino microsat
/// carrying an FM voice repeater + store-and-forward digital.
/// **Operational status is intermittent** — historically scheduled
/// in periodic activation windows; a pass with no audio is not
/// necessarily a receive-side failure. Catalog presence still
/// gives the pass-prediction view utility independent of whether
/// the transmitter is keyed during any given pass.
pub const PO_101_NORAD_ID: u32 = 43_678;

/// Standard NFM bandwidth (Hz) for amateur-radio voice satellites
/// with FM repeaters (SO-50, PO-101, …). Matches the 12.5 kHz
/// channel spacing that's standard for narrow-band ham FM and
/// covers the ±3 kHz deviation typical voice traffic uses.
pub const HAM_VOICE_NFM_BANDWIDTH_HZ: u32 = 12_500;

/// AO-7 LSB downlink centre (Hz). Tunes to the middle of the Mode-B
/// linear-transponder downlink passband (145.925-145.975 MHz); the
/// user can drag-tune across to follow individual SSB QSOs.
pub const AO_7_DOWNLINK_HZ: u64 = 145_950_000;

/// SO-50 FM voice repeater downlink (Hz). Per AMSAT.
pub const SO_50_DOWNLINK_HZ: u64 = 436_795_000;

/// PO-101 FM voice repeater downlink (Hz). Per AMSAT — operator-
/// scheduled, intermittent activation.
pub const PO_101_DOWNLINK_HZ: u64 = 145_900_000;

/// Standard SSB bandwidth (Hz) for amateur-radio satellites with
/// linear transponders (AO-7 …). Single-sideband voice traffic
/// occupies ~3 kHz; the catalog enrolls the wider ham-band
/// frequency range so the user can tune across the transponder
/// passband by drag/click in the spectrum.
pub const HAM_SSB_BANDWIDTH_HZ: u32 = 3_000;

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

/// AVHRR APIDs we expect METEOR-M2 3 to transmit during a clean
/// pass. As of May 2026 Roscosmos has M2-3 broadcasting its
/// **summer mode** — three visual channels c1/c2/c3 (APIDs 64/65/66),
/// no IR. The "Natural colour (123)" composite recipe in
/// `sdr_ui::lrpt_viewer::COMPOSITE_CATALOG` covers this set; the
/// IR-based composites (False-colour IR, Thermal IR) are
/// unavailable on these passes by design — Roscosmos schedules them
/// out for the warm half of the year.
///
/// At LOS the wiring layer compares this set against the actually-
/// received APIDs and warns if any expected APID is missing AND we
/// got at least one APID otherwise (silent passes don't trigger —
/// they're indistinguishable from "satellite was off"). Per #645.
pub const METEOR_M2_3_EXPECTED_LRPT_APIDS: &[u16] = &[64, 65, 66];

/// AVHRR APIDs we expect METEOR-M2 4 to transmit during a clean
/// pass. M2-4 broadcasts the **standard** three-channel set —
/// c1/c2/c4 (APIDs 64/65/68) — visible + visible + thermal IR.
/// All three composite recipes
/// (`sdr_ui::lrpt_viewer::COMPOSITE_CATALOG`) have full coverage on
/// these passes. Per #645 — M2-4 is currently the easier first-decode
/// target than M2-3 for exactly this reason.
pub const METEOR_M2_4_EXPECTED_LRPT_APIDS: &[u16] = &[64, 65, 68];

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
    /// satellite. APT and SSTV use ~38 kHz of headroom past the NFM
    /// default 12.5 kHz (`DEFAULT_SATELLITE_BANDWIDTH_HZ`) to capture
    /// the full subcarrier spectrum without clipping the brighter /
    /// darker extremes. LRPT entries instead pin
    /// `METEOR_M2_LRPT_BANDWIDTH_HZ` (144 kHz, matching
    /// `sdr_dsp::lrpt::SAMPLE_RATE_HZ`) so the VFO channel filter is
    /// bypassed and the 108 kHz QPSK signal is preserved end-to-end.
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
    /// Per-pass expected AVHRR APIDs for LRPT satellites. `None` for
    /// non-LRPT satellites (ISS / future Cubesats), `Some(set)` for
    /// Meteor-M family entries — the value reflects the current
    /// Roscosmos broadcast schedule (M2-3 summer mode = 64/65/66,
    /// M2-4 standard mode = 64/65/68 as of May 2026).
    ///
    /// Used by the wiring layer at LOS: if the satellite delivered
    /// some APIDs but not all of these, we emit a warning so
    /// schedule changes (e.g. Roscosmos flipping M2-3 back to
    /// winter mode) surface as a single log line instead of silent
    /// "missing composite" failures. Per #645.
    ///
    /// NOT a requirement / NOT used as a filter — every received APID
    /// produces a per-channel PNG regardless of whether it's in this
    /// set. The set drives diagnostics only.
    pub expected_lrpt_apids: Option<&'static [u16]>,
}

impl KnownSatellite {
    /// Compute the APIDs in `expected_lrpt_apids` that are not present
    /// in the `received` slice. Returns an empty `Vec` if:
    /// - This satellite has no expected-APID set (`expected_lrpt_apids` is `None`),
    /// - The satellite delivered no APIDs at all (silent pass — we don't
    ///   want to false-alarm "missing APIDs" when the receiver got
    ///   nothing), or
    /// - All expected APIDs were received.
    ///
    /// Returns the missing APIDs in catalog order (the order they appear
    /// in `expected_lrpt_apids`) otherwise. Pure function — used by the
    /// wiring layer at LOS to drive a single diagnostic warning per
    /// Roscosmos schedule mismatch, no allocations on the empty path.
    /// Per #645.
    #[must_use]
    pub fn missing_lrpt_apids(&self, received: &[u16]) -> Vec<u16> {
        let Some(expected) = self.expected_lrpt_apids else {
            return Vec::new();
        };
        if received.is_empty() {
            return Vec::new();
        }
        expected
            .iter()
            .copied()
            .filter(|apid| !received.contains(apid))
            .collect()
    }
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
    // 137.900 MHz with 72 ksym/s QPSK and AVHRR APIDs in the
    // 64..=68 range. They're in different orbital planes so they
    // don't conflict simultaneously.
    //
    // `imaging_protocol: Some(Lrpt)` enrolls these in the
    // auto-record flow per epic #469 task 7. The recorder
    // constructor's `supported_protocols` set now includes
    // `Lrpt`, the wiring layer's `interpret_action` opens the
    // LRPT viewer + signals the DSP to attach the decoder, and
    // the LOS save walks every decoded APID into a per-pass
    // directory.
    //
    // **Per-satellite APID expectations differ.** Roscosmos schedules
    // each Meteor-M bird's broadcast set independently:
    // M2-3 is currently in summer mode (3 visual channels), M2-4 in
    // standard mode (2 visual + 1 IR). See
    // `METEOR_M2_3_EXPECTED_LRPT_APIDS` /
    // `METEOR_M2_4_EXPECTED_LRPT_APIDS` for the live values; the
    // wiring layer warns at LOS if expected APIDs are missing
    // (vs. silently shipping incomplete composites). Per #645.
    //
    // METEOR-M 2 (40069) is intentionally absent — battery damage from
    // a 2022 micrometeorite collision means it can't power the LRPT
    // downlink. See doc comment on `KNOWN_SATELLITES` above.
    KnownSatellite {
        name: "METEOR-M2 3",
        norad_id: METEOR_M2_3_NORAD_ID,
        downlink_hz: METEOR_M2_LRPT_DOWNLINK_HZ,
        demod_mode: sdr_types::DemodMode::Lrpt,
        // See `METEOR_M2_LRPT_BANDWIDTH_HZ` for the bypass-the-VFO
        // rationale; both M2-3 and M2-4 share the channel.
        bandwidth_hz: METEOR_M2_LRPT_BANDWIDTH_HZ,
        imaging_protocol: Some(ImagingProtocol::Lrpt),
        // Summer mode: c1/c2/c3 (visual triplet). The Natural
        // colour composite covers this set; the IR-based
        // composites are unavailable until Roscosmos schedules
        // M2-3 back to standard mode. Per #645.
        expected_lrpt_apids: Some(METEOR_M2_3_EXPECTED_LRPT_APIDS),
    },
    KnownSatellite {
        // METEOR-M2 4 launched in 2024 and is actively transmitting
        // LRPT — same downlink as M2-3 (137.900 MHz, 72 kbaud,
        // different orbital plane so the two never contend for the
        // same pass.
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
        norad_id: METEOR_M2_4_NORAD_ID,
        downlink_hz: METEOR_M2_LRPT_DOWNLINK_HZ,
        demod_mode: sdr_types::DemodMode::Lrpt,
        bandwidth_hz: METEOR_M2_LRPT_BANDWIDTH_HZ,
        imaging_protocol: Some(ImagingProtocol::Lrpt),
        // Standard mode: c1/c2/c4 (visible/visible/thermal IR). All
        // three composite recipes have full coverage on these
        // passes — currently the easier first-decode target than
        // M2-3 for exactly that reason. Per #645.
        expected_lrpt_apids: Some(METEOR_M2_4_EXPECTED_LRPT_APIDS),
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
        // ISS SSTV is a single FM audio channel, not a multi-APID
        // LRPT broadcast — the per-pass expected-APID set doesn't
        // apply.
        expected_lrpt_apids: None,
    },
    // Amateur-radio voice satellites — #649. Pure pass-prediction
    // entries: the user manually tunes / listens via the existing
    // NFM (or SSB / Lsb) demod path, no auto-record path applies
    // (`imaging_protocol: None`). Adding these keeps the
    // Satellites panel useful between imaging passes and gives ham
    // operators a starting set of birds to chase QSO contacts on.
    //
    // **Operational status verified at catalog merge time (May
    // 2026)** — amateur satellites go in and out of service as
    // batteries fail / power budgets shift / mission lifetimes
    // end. Periodic re-verification against AMSAT's active list
    // is part of the catalog maintenance contract. Decommissioned
    // / deorbited birds we deliberately omit here:
    // - **AO-91** (FOX-1B, NORAD 43017) — battery failed, declared
    //   end-of-mission March 2024 by AMSAT.
    // - **AO-92** (FOX-1D, NORAD 43137) — reentered atmosphere
    //   November 2022.
    // - **LilacSat-2** (NORAD 40908) — decommissioned 2024.
    // Kept here as comments rather than catalog entries so a
    // future re-verifier sees we considered them and chose to
    // exclude vs. forgot to include.
    KnownSatellite {
        // AMSAT-OSCAR-7 (AO-7). 1974-launched Mode-B linear
        // transponder: 70cm uplink (432.125-432.175 MHz LSB) →
        // 2m downlink (145.925-145.975 MHz LSB). Audible on the
        // sunlit half of every orbit (the satellite has no
        // working battery — runs on solar only since 2002). The
        // catalog entry tunes to the centre of the downlink
        // passband; the user can drag-tune around to follow
        // individual SSB QSOs across the transponder.
        name: "AO-7 (OSCAR-7)",
        norad_id: AO_7_NORAD_ID,
        downlink_hz: AO_7_DOWNLINK_HZ,
        demod_mode: sdr_types::DemodMode::Lsb,
        bandwidth_hz: HAM_SSB_BANDWIDTH_HZ,
        imaging_protocol: None,
        expected_lrpt_apids: None,
    },
    KnownSatellite {
        // SaudiSat-1C (SO-50). Single-channel FM voice repeater:
        // 2m uplink (145.850 MHz, CTCSS 67 Hz) → 70cm downlink
        // (436.795 MHz). Popular for contesting and casual QSOs.
        // NFM voice — same demod path the user just validated on
        // the local PD scanner.
        name: "SO-50 (SaudiSat-1C)",
        norad_id: SO_50_NORAD_ID,
        downlink_hz: SO_50_DOWNLINK_HZ,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth_hz: HAM_VOICE_NFM_BANDWIDTH_HZ,
        imaging_protocol: None,
        expected_lrpt_apids: None,
    },
    KnownSatellite {
        // PO-101 (Diwata-2 / Philippines). FM voice repeater +
        // store-and-forward digital. Shipped with intermittent
        // activation windows scheduled by the operator —
        // pass-prediction is reliable, but a silent pass doesn't
        // necessarily indicate a receive-side problem; the
        // transmitter may simply not be keyed. Documented here
        // so a user troubleshooting a silent PO-101 pass doesn't
        // chase a chain bug. Per #649 caveat.
        name: "PO-101 (Diwata-2)",
        norad_id: PO_101_NORAD_ID,
        downlink_hz: PO_101_DOWNLINK_HZ,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth_hz: HAM_VOICE_NFM_BANDWIDTH_HZ,
        imaging_protocol: None,
        expected_lrpt_apids: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Historical NOAA-15 NORAD id. Decommissioned 2025-08-19; pinned
    /// here for the absence test so future copy-paste can't reintroduce
    /// the dark satellite under a Cubesat alias.
    const NOAA_15_DECOMMISSIONED_NORAD_ID: u32 = 25_338;
    /// Historical NOAA-18 NORAD id. Decommissioned 2025-06-06.
    /// See `NOAA_15_DECOMMISSIONED_NORAD_ID`.
    const NOAA_18_DECOMMISSIONED_NORAD_ID: u32 = 28_654;
    /// Historical NOAA-19 NORAD id. Decommissioned 2025-08-13.
    /// See `NOAA_15_DECOMMISSIONED_NORAD_ID`.
    const NOAA_19_DECOMMISSIONED_NORAD_ID: u32 = 33_591;

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
        //
        // Assert directly against `imaging_protocol` rather than name
        // substrings — a regression where someone clears or remaps the
        // protocol on a still-named-METEOR row would slip past a
        // name-only check. Per CR round 2 on PR #650.
        let protocols: Vec<ImagingProtocol> = KNOWN_SATELLITES
            .iter()
            .filter_map(|s| s.imaging_protocol)
            .collect();
        assert!(
            protocols.contains(&ImagingProtocol::Lrpt),
            "catalog should carry at least one satellite with imaging_protocol = Lrpt; \
             got protocols={protocols:?}",
        );
        assert!(
            protocols.contains(&ImagingProtocol::Sstv),
            "catalog should carry at least one satellite with imaging_protocol = Sstv; \
             got protocols={protocols:?}",
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
            (NOAA_15_DECOMMISSIONED_NORAD_ID, "NOAA-15"),
            (NOAA_18_DECOMMISSIONED_NORAD_ID, "NOAA-18"),
            (NOAA_19_DECOMMISSIONED_NORAD_ID, "NOAA-19"),
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
            .find(|s| s.norad_id == METEOR_M2_4_NORAD_ID)
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
            !KNOWN_SATELLITES
                .iter()
                .any(|s| s.norad_id == USA_403_WRONG_METEOR_NORAD_ID),
            "NORAD 61024 is USA 403, NOT METEOR-M2 4 — must not be in KNOWN_SATELLITES",
        );
    }

    #[test]
    fn meteor_m2_3_carries_summer_mode_expected_apids() {
        // M2-3 currently broadcasts c1/c2/c3 (visual triplet). Pin
        // the expected-APID set so a Roscosmos schedule change back
        // to standard mode (c1/c2/c4) shows up as a CR-able diff
        // here, not a silent failure of the missing-APIDs warning
        // at LOS. Per #645.
        let m2_3 = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == METEOR_M2_3_NORAD_ID)
            .expect("METEOR-M2 3 should be in KNOWN_SATELLITES");
        assert_eq!(
            m2_3.expected_lrpt_apids,
            Some(METEOR_M2_3_EXPECTED_LRPT_APIDS),
        );
        assert_eq!(METEOR_M2_3_EXPECTED_LRPT_APIDS, &[64, 65, 66]);
    }

    #[test]
    fn meteor_m2_4_carries_standard_mode_expected_apids() {
        // M2-4 broadcasts c1/c2/c4 (visual + visual + thermal IR) —
        // the standard set every composite recipe in
        // `sdr_ui::lrpt_viewer::COMPOSITE_CATALOG` covers. Per #645.
        let m2_4 = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == METEOR_M2_4_NORAD_ID)
            .expect("METEOR-M2 4 should be in KNOWN_SATELLITES");
        assert_eq!(
            m2_4.expected_lrpt_apids,
            Some(METEOR_M2_4_EXPECTED_LRPT_APIDS),
        );
        assert_eq!(METEOR_M2_4_EXPECTED_LRPT_APIDS, &[64, 65, 68]);
    }

    #[test]
    fn iss_has_no_expected_lrpt_apids() {
        // ISS is SSTV (single FM audio channel), not LRPT. The
        // per-pass expected-APID set doesn't apply.
        let iss = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == ISS_NORAD_ID)
            .expect("ISS should be in KNOWN_SATELLITES");
        assert_eq!(iss.expected_lrpt_apids, None);
    }

    #[test]
    fn missing_lrpt_apids_returns_empty_when_no_expected_set() {
        // Satellites with `expected_lrpt_apids: None` (ISS,
        // future non-LRPT entries) never emit the warning even if
        // an unrelated `received` slice is passed.
        let iss = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == ISS_NORAD_ID)
            .expect("ISS should be in KNOWN_SATELLITES");
        assert!(iss.missing_lrpt_apids(&[1, 2, 3]).is_empty());
        assert!(iss.missing_lrpt_apids(&[]).is_empty());
    }

    #[test]
    fn missing_lrpt_apids_returns_empty_on_silent_pass() {
        // Silent pass (received is empty) must NOT warn — that's
        // a different failure mode (no signal / weak signal /
        // satellite off) handled by `pass_decoded_nothing` in the
        // wiring layer. Returning empty here keeps the warning
        // scoped to "got SOME imagery but expected MORE."
        let m2_3 = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == METEOR_M2_3_NORAD_ID)
            .expect("METEOR-M2 3 should be in KNOWN_SATELLITES");
        assert!(m2_3.missing_lrpt_apids(&[]).is_empty());
    }

    #[test]
    fn missing_lrpt_apids_reports_summer_mode_partial_pass() {
        // M2-3 expects 64/65/66; if we got 64+65 only, the warning
        // should call out 66 missing.
        let m2_3 = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == METEOR_M2_3_NORAD_ID)
            .expect("METEOR-M2 3 should be in KNOWN_SATELLITES");
        assert_eq!(m2_3.missing_lrpt_apids(&[64, 65]), vec![66]);
    }

    #[test]
    fn missing_lrpt_apids_reports_standard_mode_partial_pass() {
        // M2-4 expects 64/65/68; if we got 64+68 only, the warning
        // should call out 65 missing. Order follows the catalog
        // expected-APIDs order so future readers see the slot
        // gap directly.
        let m2_4 = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == METEOR_M2_4_NORAD_ID)
            .expect("METEOR-M2 4 should be in KNOWN_SATELLITES");
        assert_eq!(m2_4.missing_lrpt_apids(&[64, 68]), vec![65]);
    }

    #[test]
    fn missing_lrpt_apids_returns_empty_when_full_set_received() {
        // Happy path: every expected APID delivered. No warning.
        let m2_3 = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == METEOR_M2_3_NORAD_ID)
            .expect("METEOR-M2 3 should be in KNOWN_SATELLITES");
        assert!(m2_3.missing_lrpt_apids(&[64, 65, 66]).is_empty());
        // Extra received APIDs (e.g., M2-3 unexpectedly transmitting
        // an IR channel) are fine — the function returns the
        // complement of expected over received, ignoring extras.
        assert!(m2_3.missing_lrpt_apids(&[64, 65, 66, 70]).is_empty());
    }

    #[test]
    fn ao_7_is_present_with_lsb_linear_transponder() {
        // AO-7 catalog entry pins the historical Mode-B downlink
        // and LSB demod. If a future maintainer flips it to NFM
        // (wrong) or USB (also wrong — Mode B uses LSB by
        // convention), this test fails. Per #649.
        let ao_7 = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == AO_7_NORAD_ID)
            .expect("AO-7 (NORAD 7530) should be in KNOWN_SATELLITES");
        assert_eq!(ao_7.demod_mode, sdr_types::DemodMode::Lsb);
        assert_eq!(ao_7.bandwidth_hz, HAM_SSB_BANDWIDTH_HZ);
        assert_eq!(ao_7.imaging_protocol, None);
        // Pin the exact downlink so a typo / band-edit can't shift
        // the catalog away from the AMSAT-published frequency
        // without a CR-able diff. Per CR round 1.
        assert_eq!(ao_7.downlink_hz, AO_7_DOWNLINK_HZ);
        // Belt-and-braces: also check the band membership so a
        // future drag-tune helper that recentres entries can't
        // land outside the legal 2 m amateur allocation
        // (144-148 MHz) without a test failure.
        assert!(
            (144_000_000..=148_000_000).contains(&ao_7.downlink_hz),
            "AO-7 downlink {} Hz should be in the 2m amateur band",
            ao_7.downlink_hz,
        );
    }

    #[test]
    fn so_50_is_present_with_nfm_voice_repeater() {
        // SO-50 catalog entry pins the 70cm FM voice downlink.
        // Per #649. Bandwidth equals `HAM_VOICE_NFM_BANDWIDTH_HZ`
        // so a future global change to the NFM-voice default
        // applies here without a per-row edit.
        let so_50 = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == SO_50_NORAD_ID)
            .expect("SO-50 (NORAD 27607) should be in KNOWN_SATELLITES");
        assert_eq!(so_50.demod_mode, sdr_types::DemodMode::Nfm);
        assert_eq!(so_50.bandwidth_hz, HAM_VOICE_NFM_BANDWIDTH_HZ);
        assert_eq!(so_50.imaging_protocol, None);
        // Pin the exact downlink. Per CR round 1.
        assert_eq!(so_50.downlink_hz, SO_50_DOWNLINK_HZ);
        // Belt-and-braces 70 cm amateur band (420-450 MHz) check.
        assert!(
            (420_000_000..=450_000_000).contains(&so_50.downlink_hz),
            "SO-50 downlink {} Hz should be in the 70cm amateur band",
            so_50.downlink_hz,
        );
    }

    #[test]
    fn po_101_is_present_with_nfm_voice_repeater() {
        // PO-101 catalog entry pins the 2m FM voice downlink.
        // Documented as "intermittent" — a silent pass is not
        // necessarily a receive-side bug. Per #649.
        let po_101 = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == PO_101_NORAD_ID)
            .expect("PO-101 (NORAD 43678) should be in KNOWN_SATELLITES");
        assert_eq!(po_101.demod_mode, sdr_types::DemodMode::Nfm);
        assert_eq!(po_101.bandwidth_hz, HAM_VOICE_NFM_BANDWIDTH_HZ);
        assert_eq!(po_101.imaging_protocol, None);
        // Pin the exact downlink. Per CR round 1.
        assert_eq!(po_101.downlink_hz, PO_101_DOWNLINK_HZ);
        // Belt-and-braces 2 m amateur band (144-148 MHz) check.
        assert!(
            (144_000_000..=148_000_000).contains(&po_101.downlink_hz),
            "PO-101 downlink {} Hz should be in the 2m amateur band",
            po_101.downlink_hz,
        );
    }

    #[test]
    fn decommissioned_amateur_satellites_are_absent() {
        // AMSAT amateur satellites that have been formally
        // decommissioned or have reentered the atmosphere as of
        // May 2026. Pinning their absence prevents a future
        // copy-paste from a stale guide reintroducing dead birds
        // that would only ever produce empty pass sessions.
        // Per #649. Sources: AMSAT operational satellite list.
        //
        // NORAD IDs hoisted to named test-only constants per CR
        // round 1 — keeps the assertion loop self-documenting and
        // matches the constant-first style used elsewhere in this
        // module (`NOAA_15_DECOMMISSIONED_NORAD_ID`, etc.).
        const AO_91_DECOMMISSIONED_NORAD_ID: u32 = 43_017;
        const AO_92_DECOMMISSIONED_NORAD_ID: u32 = 43_137;
        const LILACSAT_2_DECOMMISSIONED_NORAD_ID: u32 = 40_908;
        for &(norad_id, name, reason) in &[
            (
                AO_91_DECOMMISSIONED_NORAD_ID,
                "AO-91 (FOX-1B)",
                "battery failed, end-of-mission March 2024",
            ),
            (
                AO_92_DECOMMISSIONED_NORAD_ID,
                "AO-92 (FOX-1D)",
                "reentered atmosphere November 2022",
            ),
            (
                LILACSAT_2_DECOMMISSIONED_NORAD_ID,
                "LilacSat-2",
                "decommissioned 2024",
            ),
        ] {
            assert!(
                !KNOWN_SATELLITES.iter().any(|s| s.norad_id == norad_id),
                "decommissioned {name} (NORAD {norad_id}) should not be in \
                 KNOWN_SATELLITES — {reason}",
            );
        }
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
            !KNOWN_SATELLITES
                .iter()
                .any(|s| s.norad_id == METEOR_M2_DECOMMISSIONED_NORAD_ID),
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

        // METEOR satellites → Lrpt (epic #469 task 7). The live
        // catalog pair is METEOR-M2 3 + METEOR-M2 4 — both ship
        // with `Some(Lrpt)`. METEOR-M 2 (the original) is excluded
        // due to battery damage from a 2022 micrometeorite collision
        // (see `meteor_m_2_is_excluded_due_to_battery_damage` test).
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
