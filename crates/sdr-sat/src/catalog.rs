//! The built-in satellite catalog — [`KNOWN_SATELLITES`] plus the
//! NORAD ids, downlink frequencies, bandwidths, ham-band ranges,
//! and expected-APID sets its rows (and their pin tests) are built
//! from. Split out of `lib.rs` per the file-size pass (issue #820);
//! everything here is re-exported from the crate root so external
//! `sdr_sat::X` paths are unchanged.

use crate::types::{ImagingProtocol, KnownSatellite, LrptModulation};

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

/// 2 m amateur band (Hz, inclusive). Same physical allocation as
/// `SSTV_VHF_2M_BAND_HZ` but expressed under the ham-radio name
/// because the catalog tests for AO-7 / PO-101 membership-check
/// against the *amateur allocation*, not the SSTV-format-band
/// (those happen to coincide at 2 m today; the names stay
/// independent so a future schedule shift on either side doesn't
/// silently couple them). Per CR round 2 on PR #656.
pub const HAM_VHF_2M_BAND_HZ: (u64, u64) = (144_000_000, 148_000_000);

/// 70 cm amateur band (Hz, inclusive). Wider than
/// `SSTV_UHF_70CM_BAND_HZ` (430-440 MHz, the SSTV-format
/// allocation slice) — the full ham 70 cm band runs 420-450 MHz
/// in IARU Region 2. SO-50 at 436.795 MHz lives in both, but the
/// catalog test uses the broader ham allocation so a future ham
/// satellite at e.g. 425 MHz wouldn't fail on a too-narrow SSTV
/// range. Per CR round 2 on PR #656.
pub const HAM_UHF_70CM_BAND_HZ: (u64, u64) = (420_000_000, 450_000_000);

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
        // Current-generation METEOR-M2 satellites broadcast
        // differentially-precoded Offset QPSK at 72 ksym/s. Per
        // #662; differential requirement confirmed empirically on
        // M2-4 (#892) and inferred here for M2-3 (identical downlink
        // format) — flip back if an M2-3 live pass ever proves it
        // decodes without differential.
        lrpt_modulation: Some(LrptModulation::Oqpsk),
        lrpt_differential: true,
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
        // Same differentially-precoded OQPSK downlink as M2-3. The
        // differential requirement was confirmed empirically here:
        // the station's first successful M2-4 decode (2026-09-04,
        // #892) produced real AVHRR imagery ONLY with differential
        // precoding — plain OQPSK yielded zero CADUs. The former
        // `lrpt_differential: false` is why every live M2-4 pass
        // silently decoded nothing. Per #662 + #892.
        lrpt_modulation: Some(LrptModulation::Oqpsk),
        lrpt_differential: true,
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
        lrpt_modulation: None,
        lrpt_differential: false,
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
        lrpt_modulation: None,
        lrpt_differential: false,
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
        lrpt_modulation: None,
        lrpt_differential: false,
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
        lrpt_modulation: None,
        lrpt_differential: false,
    },
];

#[cfg(test)]
mod tests;
