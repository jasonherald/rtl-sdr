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
/// Shared by the bandwidth / frequency text-entry surfaces
/// (currently `BandwidthEntry`; the big tuner now uses a
/// digit grid and parses nothing). These inputs want the same
/// permissive parse rules.
func parseHzFrequency(_ s: String) -> Double? {
    // Sanity cap — 1 THz is well above any known SDR tunable
    // range, so anything larger is almost certainly a typo
    // (trailing digit, missing decimal, etc.). Letting
    // `Double("1e309")` through returns `.infinity`, which then
    // crashes any downstream `UInt64(hz)` / `Int64(hz)` cast.
    // Reject non-finite and out-of-range up front.
    let maxHz: Double = 1e12

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
        guard let v = Double(body), v.isFinite, v >= 0 else { return nil }
        let hz = v * mult
        guard hz.isFinite, hz <= maxHz else { return nil }
        return hz
    }
    guard let v = Double(trimmed),
          v.isFinite, v >= 0, v <= maxHz else { return nil }
    return v
}
