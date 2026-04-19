//
// AveragingMode.swift — display-side spectrum averaging.
//
// UI-only feature: the engine doesn't know or care about
// averaging. The renderer applies the selected mode to each
// incoming FFT frame before handing the values to the Metal
// shader. Matches the GTK side's `AveragingMode` in
// `crates/sdr-ui/src/spectrum/mod.rs` variant-for-variant so
// the two UIs feel the same.

import Foundation

enum AveragingMode: String, Sendable, CaseIterable {
    /// No averaging — display raw FFT data.
    case none
    /// Hold peak values across frames.
    case peakHold
    /// Exponential moving average with fixed alpha. Smooths
    /// fast transients; signals stay roughly where they are.
    case runningAvg
    /// Hold minimum values across frames. Useful for spotting
    /// noise floor.
    case minHold

    var label: String {
        switch self {
        case .none:       "None"
        case .peakHold:   "Peak Hold"
        case .runningAvg: "Running Avg"
        case .minHold:    "Min Hold"
        }
    }
}

/// Alpha for `.runningAvg` exponential smoothing. Same value
/// as the GTK side's `AVERAGING_ALPHA` so the two UIs show
/// identical smoothing behavior at the same FFT rate.
let averagingAlpha: Float = 0.3
