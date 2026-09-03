//! Catalog pin tests — moved verbatim from the former inline
//! `mod tests` in `lib.rs` per the file-size pass (issue #820;
//! same `mod tests;` + `<module>/tests.rs` shape as the #810-#815
//! test extraction). `super` is the `catalog` module, whose own
//! imports supply the type names the old root glob provided.

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
fn lrpt_modulation_pinned_for_active_meteor_satellites() {
    // `lrpt_modulation` is part of the runtime demod-selection
    // contract (sdr-dsp::lrpt::LrptDemod dispatches QPSK vs
    // OQPSK pipelines on it). A future copy/paste that left
    // a Meteor row at `None` would silently fall back to the
    // legacy QPSK path and resurrect the zero-APID pass-failure
    // that motivated #662 in the first place — so pin both
    // active Meteor entries here. Per CR round 1 on PR #663.
    let m2_3 = KNOWN_SATELLITES
        .iter()
        .find(|s| s.norad_id == METEOR_M2_3_NORAD_ID)
        .expect("METEOR-M2 3 should be in KNOWN_SATELLITES");
    assert_eq!(
        m2_3.lrpt_modulation,
        Some(LrptModulation::Oqpsk),
        "METEOR-M2 3 transmits OQPSK; QPSK demod cannot decode it cleanly",
    );
    let m2_4 = KNOWN_SATELLITES
        .iter()
        .find(|s| s.norad_id == METEOR_M2_4_NORAD_ID)
        .expect("METEOR-M2 4 should be in KNOWN_SATELLITES");
    assert_eq!(
        m2_4.lrpt_modulation,
        Some(LrptModulation::Oqpsk),
        "METEOR-M2 4 transmits OQPSK; QPSK demod cannot decode it cleanly",
    );
}

/// Differential precoding is part of the downlink profile (#730):
/// the current Meteor-M2 birds use plain concatenated coding, so
/// the live decoder must not run the differential pre-decoder.
#[test]
fn meteor_downlinks_are_not_differentially_precoded() {
    for norad_id in [METEOR_M2_3_NORAD_ID, METEOR_M2_4_NORAD_ID] {
        let sat = KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == norad_id)
            .expect("Meteor in KNOWN_SATELLITES");
        assert!(
            !sat.lrpt_differential,
            "{}: no differential precoding",
            sat.name
        );
    }
}

#[test]
fn lrpt_modulation_only_set_on_lrpt_entries() {
    // Inverse of the test above: the catalog is small enough
    // that a stray `Some(_)` on a non-LRPT row would slip
    // through review without this guard. Anything not flagged
    // `imaging_protocol = Some(Lrpt)` must have
    // `lrpt_modulation = None` — otherwise the field
    // contradicts the rest of the row's contract. Per CR
    // round 1 on PR #663.
    for sat in KNOWN_SATELLITES {
        let is_lrpt = sat.imaging_protocol == Some(ImagingProtocol::Lrpt);
        if is_lrpt {
            assert!(
                sat.lrpt_modulation.is_some(),
                "{} (NORAD {}) is an LRPT satellite but lrpt_modulation = None — \
                 the demod chain would fall back to legacy QPSK and likely fail to decode",
                sat.name,
                sat.norad_id,
            );
        } else {
            assert!(
                sat.lrpt_modulation.is_none(),
                "{} (NORAD {}) is not LRPT but carries lrpt_modulation = {:?} — \
                 contradicts imaging_protocol = {:?}",
                sat.name,
                sat.norad_id,
                sat.lrpt_modulation,
                sat.imaging_protocol,
            );
        }
    }
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
        (HAM_VHF_2M_BAND_HZ.0..=HAM_VHF_2M_BAND_HZ.1).contains(&ao_7.downlink_hz),
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
        (HAM_UHF_70CM_BAND_HZ.0..=HAM_UHF_70CM_BAND_HZ.1).contains(&so_50.downlink_hz),
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
        (HAM_VHF_2M_BAND_HZ.0..=HAM_VHF_2M_BAND_HZ.1).contains(&po_101.downlink_hz),
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
