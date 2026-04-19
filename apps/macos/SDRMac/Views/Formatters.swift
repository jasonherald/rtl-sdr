//
// Formatters.swift — small display-formatting helpers shared
// across multiple views. Keep to pure, allocation-light
// functions; anything UI-state-bearing belongs on CoreModel.

import Foundation

/// Human-readable sample-rate / bandwidth string. Picks MHz,
/// kHz, or Hz based on magnitude.
///
/// Used by the Source sidebar panel (sample-rate picker), the
/// status bar (effective sample rate), and `BandwidthEntry`.
func formatRate(_ hz: Double) -> String {
    if hz >= 1_000_000 {
        return String(format: "%.3f MHz", hz / 1_000_000)
    } else if hz >= 1_000 {
        return String(format: "%.1f kHz", hz / 1_000)
    } else {
        return String(format: "%.0f Hz", hz)
    }
}

/// Parse a human-typed frequency / bandwidth string. Accepts
/// plain Hz or `GHz`/`MHz`/`kHz` suffixes and their single-letter
/// variants ("100.7M", "446k", "2.4G"). Case-insensitive, ignores
/// interior whitespace, accepts a leading `+`. Rejects negative
/// inputs — callers treat `nil` as "revert to last value".
///
/// Shared by `FrequencyEntry` (big tuner display) and
/// `BandwidthEntry` (Radio sidebar). Both want the same
/// permissive parse rules.
func parseHzFrequency(_ s: String) -> Double? {
    var trimmed = s.trimmingCharacters(in: .whitespaces).lowercased()
    if trimmed.hasPrefix("-") { return nil }
    if trimmed.hasPrefix("+") { trimmed = String(trimmed.dropFirst()) }
    let multipliers: [(String, Double)] = [
        ("ghz", 1_000_000_000), ("g", 1_000_000_000),
        ("mhz", 1_000_000),     ("m", 1_000_000),
        ("khz", 1_000),         ("k", 1_000),
        ("hz", 1),
    ]
    for (suffix, mult) in multipliers where trimmed.hasSuffix(suffix) {
        let body = trimmed.dropLast(suffix.count).trimmingCharacters(in: .whitespaces)
        if let v = Double(body), v >= 0 { return v * mult }
    }
    guard let v = Double(trimmed), v >= 0 else { return nil }
    return v
}
