//
// Formatters.swift — small display-formatting helpers shared
// across multiple views. Keep to pure, allocation-light
// functions; anything UI-state-bearing belongs on CoreModel.

import Foundation

/// Human-readable sample-rate / bandwidth string. Picks MHz,
/// kHz, or Hz based on magnitude.
///
/// Used by the Source sidebar panel (sample-rate picker) and
/// the status bar (effective sample rate).
func formatRate(_ hz: Double) -> String {
    if hz >= 1_000_000 {
        return String(format: "%.3f MHz", hz / 1_000_000)
    } else if hz >= 1_000 {
        return String(format: "%.1f kHz", hz / 1_000)
    } else {
        return String(format: "%.0f Hz", hz)
    }
}
