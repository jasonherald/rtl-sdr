//
// FrequencyAxis.swift — pure frequency-label / grid-step helpers
// for the spectrum grid overlay. Broken out from SpectrumGridView
// so it can be unit-tested against a snapshot of Linux's
// `crates/sdr-ui/src/spectrum/frequency_axis.rs` output — see
// `SDRMacTests/FrequencyAxisTests.swift` for the parity fixture.
//
// These are byte-for-byte ports of the Linux implementations:
//   - `STEP_CANDIDATES` ↔ `stepCandidates`
//   - `format_frequency` ↔ `formatFrequency`
//   - `compute_grid_lines` ↔ `computeGridLines`
//
// If the Linux side changes, the test fixture fails on the next
// build and whoever updates Linux also updates the Mac snapshot.
// That's the "drift check" — not automated cross-platform sync,
// but a loud tripwire.

import Foundation

enum FrequencyAxis {
    /// Candidate step sizes in Hz, smallest to largest.
    /// Verbatim copy of `STEP_CANDIDATES` from
    /// `crates/sdr-ui/src/spectrum/frequency_axis.rs:48-77`.
    static let stepCandidates: [Double] = [
        1, 2, 5,
        10, 20, 50,
        100, 200, 500,
        1_000, 2_000, 5_000,
        10_000, 20_000, 50_000,
        100_000, 200_000, 500_000,
        1_000_000, 2_000_000, 5_000_000,
        10_000_000, 20_000_000, 50_000_000,
        100_000_000, 200_000_000, 500_000_000,
        1_000_000_000,
    ]

    /// Human-readable Hz → string. Port of `format_frequency` in
    /// `crates/sdr-ui/src/spectrum/frequency_axis.rs:31-44`.
    /// Same thresholds, same precision per unit.
    static func formatFrequency(_ hz: Double) -> String {
        let abs = Swift.abs(hz)
        let sign = hz < 0 ? "-" : ""
        switch abs {
        case 1_000_000_000...:
            return String(format: "%@%.3f GHz", sign, abs / 1_000_000_000)
        case 1_000_000...:
            return String(format: "%@%.3f MHz", sign, abs / 1_000_000)
        case 1_000...:
            return String(format: "%@%.1f kHz", sign, abs / 1_000)
        default:
            return String(format: "%@%.1f Hz", sign, abs)
        }
    }

    /// Compute grid-line positions + labels for a frequency axis.
    /// Port of `compute_grid_lines` from
    /// `crates/sdr-ui/src/spectrum/frequency_axis.rs:90-117`.
    /// `startHz` / `endHz` are absolute frequencies. Returns
    /// `[(freqHz, label)]` spaced at a round step that yields at
    /// most `maxLines` entries.
    static func computeGridLines(
        startHz: Double,
        endHz: Double,
        maxLines: Int
    ) -> [(Double, String)] {
        guard maxLines > 0, endHz > startHz else { return [] }
        let span = endHz - startHz
        // Find smallest step that yields strictly fewer than
        // `maxLines` entries. Strict `<` matches Linux; the line
        // count is `floor(span/step) + 1` worst-case.
        //
        // Fallback `span * 2`: if no candidate fits (very
        // narrow span with high `maxLines`, or `maxLines == 1`),
        // the step exceeds the span so `first` lands past
        // `endHz` and the loop emits zero lines. That's
        // deliberate — better to show no grid than a crowded
        // one on pathological inputs.
        let step = stepCandidates.first(where: { (span / $0) < Double(maxLines) })
            ?? span * 2
        let first = (startHz / step).rounded(.up) * step

        // Index-based iteration avoids floating-point drift
        // accumulating across many `freq += step` additions.
        // Matters at high line counts + sub-Hz precision; for
        // the ≤20 lines we typically ask for it's overkill but
        // the same cost. Per #320 review.
        let remaining = endHz - first
        guard remaining >= 0, step > 0 else { return [] }
        let count = Int(floor(remaining / step)) + 1
        var lines: [(Double, String)] = []
        lines.reserveCapacity(count)
        for i in 0..<count {
            let freq = first + Double(i) * step
            // Defensive `<= endHz` guard — floor + step math is
            // exact for power-of-ten steps but protect against
            // any weird edge case.
            if freq > endHz { break }
            lines.append((freq, formatFrequency(freq)))
        }
        return lines
    }
}
