//! Catalog-facing type definitions — the imaging-protocol and
//! LRPT-modulation enums, the [`KnownSatellite`] row type, and the
//! per-protocol allowed-band constants that
//! [`ImagingProtocol::allowed_bands_hz`] is defined over. Split out
//! of `lib.rs` per the file-size pass (issue #820); everything here
//! is re-exported from the crate root so external `sdr_sat::X`
//! paths are unchanged.

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

/// Imaging protocol the receiver should use for a given catalog
/// satellite. Drives the auto-record dispatch in
/// `sidebar::satellites_recorder` so APT vs LRPT vs SSTV each get
/// their own decoder + viewer without the recorder itself caring
/// about protocol details.
///
/// LRPT modulation variant used by a Meteor-M satellite. Per #662,
/// the original METEOR-M N2 (NORAD 40069, decommissioned) used
/// standard QPSK; current generation METEOR-M2 satellites
/// (M2-2, M2-3, M2-4) all use **Offset QPSK** (OQPSK) at 72 ksym/s.
///
/// The two variants share the same constellation (4 corner points)
/// but differ in symbol timing: OQPSK delays Q by half a symbol
/// period relative to I. A pure QPSK demod CAN lock on OQPSK but
/// produces sub-quality soft symbols because the Gardner timing
/// recovery error metric assumes I and Q are co-timed. That's the
/// root cause of the silent-Meteor-pass debug session that
/// surfaced this issue.
///
/// Carried on each [`KnownSatellite`] with `imaging_protocol =
/// Some(Lrpt)` so the demod chain can dispatch correctly per
/// satellite. `None` for non-LRPT satellites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LrptModulation {
    /// Standard QPSK at 72 ksym/s. Used by the original METEOR-M
    /// N2 (now decommissioned). Kept as a variant so a future
    /// satellite reverting to standard QPSK can be enrolled
    /// without code changes.
    Qpsk,
    /// Offset QPSK at 72 ksym/s. Used by all current operational
    /// METEOR-M2 satellites (M2-3, M2-4). Q is delayed by half a
    /// symbol period; demodulator must use dual-tick timing
    /// recovery and separate I/Q PLL mixes per
    /// `original/meteor_demod/dsp/pll.c`.
    Oqpsk,
}

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
    /// LRPT modulation variant for this satellite. `None` for
    /// non-LRPT entries. `Some(LrptModulation::Oqpsk)` for current-
    /// generation Meteor-M2 satellites (M2-3, M2-4) which transmit
    /// Offset QPSK; `Some(Qpsk)` reserved for any future revival
    /// of the original METEOR-M N2 (which used standard QPSK).
    /// Drives the dispatch in `sdr_dsp::lrpt::LrptDemod` so the
    /// timing recovery + PLL paths match the actual signal. Per
    /// #662 — investigation showed our hardcoded-QPSK pipeline
    /// can't cleanly demodulate OQPSK signals.
    pub lrpt_modulation: Option<LrptModulation>,
    /// Whether this satellite's LRPT downlink is differentially
    /// precoded (the FEC chain runs dbdexter's `diff_decode` on the
    /// soft symbols before Viterbi). The legacy METEOR-M N2 used
    /// differential precoding; the current METEOR-M2 3 / M2-4 use
    /// plain concatenated coding. Part of the downlink profile so
    /// the live decoder and the replay CLI build the same chain
    /// (#730). Meaningless (`false`) for non-LRPT entries.
    pub lrpt_differential: bool,
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
