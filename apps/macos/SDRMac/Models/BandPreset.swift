//
// BandPreset.swift — common-band quick-tune presets surfaced
// in the General activity panel.
//
// Mirror of the Linux `BAND_PRESETS` slice in
// `crates/sdr-ui/src/sidebar/navigation_panel.rs`. Frequencies
// + demod + bandwidth values copied verbatim so a press of the
// same preset on either frontend lands the same tuning state.
// Reordering or renaming entries here would split the
// frontends — keep the slice in lockstep with the Rust side.
//
// Per epic #441 / sub-ticket #443.

import Foundation
import SdrCoreKit

struct BandPreset: Identifiable, Hashable {
    /// Stable identity for SwiftUI list diffing — uses the
    /// label which is unique across the canonical slice.
    var id: String { name }

    let name: String
    let centerFrequencyHz: Double
    let demodMode: DemodMode
    /// Channel bandwidth in Hz — applied via
    /// `CoreModel.setBandwidth` alongside the tune so the
    /// audio path doesn't carry over WFM's 150 kHz when the
    /// user jumps to NOAA's 12.5 kHz NFM.
    let bandwidthHz: Double
}

/// Canonical preset slice. North America / ITU Region 2
/// frequencies; matches `BAND_PRESETS` in
/// `crates/sdr-ui/src/sidebar/navigation_panel.rs`.
let bandPresets: [BandPreset] = [
    BandPreset(
        name: "FM Broadcast",
        centerFrequencyHz: 98_100_000,
        demodMode: .wfm,
        bandwidthHz: 150_000
    ),
    BandPreset(
        name: "NOAA Weather",
        centerFrequencyHz: 162_550_000,
        demodMode: .nfm,
        bandwidthHz: 12_500
    ),
    BandPreset(
        name: "Aviation (Guard)",
        centerFrequencyHz: 121_500_000,
        demodMode: .am,
        bandwidthHz: 8_333
    ),
    BandPreset(
        name: "2m Calling",
        centerFrequencyHz: 146_520_000,
        demodMode: .nfm,
        bandwidthHz: 12_500
    ),
    BandPreset(
        name: "70cm Calling",
        centerFrequencyHz: 446_000_000,
        demodMode: .nfm,
        bandwidthHz: 12_500
    ),
    BandPreset(
        name: "Marine Ch 16",
        centerFrequencyHz: 156_800_000,
        demodMode: .nfm,
        bandwidthHz: 25_000
    ),
    BandPreset(
        name: "FRS Ch 1",
        centerFrequencyHz: 462_562_500,
        demodMode: .nfm,
        bandwidthHz: 12_500
    ),
    BandPreset(
        name: "MURS Ch 1",
        centerFrequencyHz: 151_820_000,
        demodMode: .nfm,
        bandwidthHz: 11_250
    ),
    BandPreset(
        name: "CB Ch 19",
        centerFrequencyHz: 27_185_000,
        demodMode: .am,
        bandwidthHz: 10_000
    ),
    BandPreset(
        name: "10m Calling",
        centerFrequencyHz: 28_400_000,
        demodMode: .usb,
        bandwidthHz: 2_700
    ),
]
