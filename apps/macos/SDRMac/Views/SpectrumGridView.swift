//
// SpectrumGridView.swift — SwiftUI Canvas overlay that draws the
// frequency axis and dB grid on top of the Metal renderer.
//
// Two responsibilities:
//
//   1. **dB grid** — horizontal lines + left-edge dB labels,
//      covering just the spectrum region (top 30% of the view).
//      The waterfall has no dB axis.
//
//   2. **Frequency grid** — vertical lines + top-edge Hz labels,
//      spanning BOTH the spectrum and waterfall regions so the
//      user can visually align a signal to a frequency at any
//      vertical position in the view.
//
// Drawn with SwiftUI `Canvas`. Colors match the GTK UI's
// GRID_COLOR / LABEL_COLOR (`crates/sdr-ui/src/spectrum/fft_plot.rs`)
// so side-by-side shots read as the same app.
//
// Tick spacing uses a port of `compute_grid_lines` from
// `crates/sdr-ui/src/spectrum/frequency_axis.rs` — the same
// candidate-step table (1/2/5/10/…) so gridlines land on the
// same "round" numbers on both platforms.
//
// Non-hit-testing: `.allowsHitTesting(false)` on the view so
// mouse clicks pass through to the VFO overlay underneath.

import SwiftUI

struct SpectrumGridView: View {
    let model: CoreModel

    // Layout constants. Match the renderer and the GTK reference.
    private static let spectrumFraction: CGFloat = 0.30
    private static let dbGridLineCount: Int = 8
    private static let freqGridMaxLines: Int = 10
    private static let labelFontSize: CGFloat = 10

    // Colors ported verbatim from
    // `crates/sdr-ui/src/spectrum/fft_plot.rs:37,26`.
    private static let gridColor = Color(red: 0.4, green: 0.4, blue: 0.4).opacity(0.5)
    private static let labelColor = Color(red: 0.6, green: 0.6, blue: 0.6).opacity(0.8)

    var body: some View {
        Canvas { context, size in
            // Use the DISPLAYED span + center so the grid zooms
            // with the waterfall. Unzoomed, these collapse to
            // the full FFT span centered on the tuner, same
            // behaviour as before.
            let span = model.effectiveDisplayedSpanHz
            let center = model.centerFrequencyHz + model.displayedCenterOffsetHz
            // Compute frequency grid lines once per draw and share
            // them between the grid-line and label passes — the
            // positions are identical and the step-size lookup
            // isn't free. Canvas redraws are cheap, but there's no
            // reason to do the same work twice.
            let freqLines = frequencyGridLines(span: span, center: center)
            drawGrid(context: context, size: size, freqLines: freqLines, span: span, center: center)
            drawLabels(context: context, size: size, freqLines: freqLines, span: span, center: center)
        }
        .allowsHitTesting(false)
    }

    // ----------------------------------------------------------
    //  Grid lines (drawn under labels)
    // ----------------------------------------------------------

    private func drawGrid(
        context: GraphicsContext,
        size: CGSize,
        freqLines: [(Double, String)],
        span: Double,
        center: Double
    ) {
        let w = size.width
        let h = size.height
        let spectrumH = (h * Self.spectrumFraction).rounded()

        // --- Horizontal dB grid (spectrum region only)
        var dbPath = Path()
        for i in 0...Self.dbGridLineCount {
            let frac = CGFloat(i) / CGFloat(Self.dbGridLineCount)
            // +0.5 so 1px lines sit on pixel centers (no blur)
            let y = (spectrumH * frac).rounded() + 0.5
            dbPath.move(to: CGPoint(x: 0, y: y))
            dbPath.addLine(to: CGPoint(x: w, y: y))
        }
        context.stroke(dbPath, with: .color(Self.gridColor), lineWidth: 1)

        // --- Vertical frequency grid (spans full height)
        guard !freqLines.isEmpty, span > 0 else { return }
        var freqPath = Path()
        let leftHz = center - span / 2
        for (freq, _) in freqLines {
            let frac = (freq - leftHz) / span
            let x = (w * CGFloat(frac)).rounded() + 0.5
            freqPath.move(to: CGPoint(x: x, y: 0))
            freqPath.addLine(to: CGPoint(x: x, y: h))
        }
        context.stroke(freqPath, with: .color(Self.gridColor), lineWidth: 1)
    }

    // ----------------------------------------------------------
    //  Labels (drawn on top of grid)
    // ----------------------------------------------------------

    private func drawLabels(
        context: GraphicsContext,
        size: CGSize,
        freqLines: [(Double, String)],
        span: Double,
        center: Double
    ) {
        let w = size.width
        let h = size.height
        let spectrumH = (h * Self.spectrumFraction).rounded()

        // --- Frequency labels at top of spectrum
        if span > 0 {
            let leftHz = center - span / 2
            for (freq, label) in freqLines {
                let frac = (freq - leftHz) / span
                let x = w * CGFloat(frac)
                let text = Text(label)
                    .font(.system(size: Self.labelFontSize, design: .monospaced))
                    .foregroundColor(Self.labelColor)
                // Nudge x+2 so labels don't overlap the vertical
                // grid line they're attached to. Matches GTK's
                // `move_to(x + 2.0, FREQ_LABEL_TOP_MARGIN - 2.0)`.
                context.draw(text, at: CGPoint(x: x + 2, y: 2), anchor: .topLeading)
            }
        }

        // --- dB labels on left edge at each horizontal grid line
        let minDb = Double(model.minDb)
        let maxDb = Double(model.maxDb)
        let dbRange = maxDb - minDb
        guard dbRange > 0 else { return }
        for i in 0...Self.dbGridLineCount {
            let frac = CGFloat(i) / CGFloat(Self.dbGridLineCount)
            let y = spectrumH * frac
            // frac 0 = top = maxDb; frac 1 = bottom (of spectrum
            // area) = minDb. Matches the GTK convention.
            let dbVal = maxDb - Double(frac) * dbRange
            let text = Text(String(format: "%.0f dB", dbVal))
                .font(.system(size: Self.labelFontSize, design: .monospaced))
                .foregroundColor(Self.labelColor)
            // Nudge +2 / -2 away from the line crossing so text
            // doesn't clip the line itself. Matches the Cairo
            // reference: `move_to(2.0, y - 2.0)`.
            context.draw(text, at: CGPoint(x: 2, y: y - 2), anchor: .bottomLeading)
        }
    }

    // ----------------------------------------------------------
    //  Frequency grid computation
    // ----------------------------------------------------------
    //
    //  Delegates to `FrequencyAxis` so the pure math is testable
    //  without a live view (see
    //  `SDRMacTests/FrequencyAxisTests.swift`).

    private func frequencyGridLines(span: Double, center: Double)
        -> [(Double, String)]
    {
        guard span > 0 else { return [] }
        let halfSpan = span / 2
        return FrequencyAxis.computeGridLines(
            startHz: center - halfSpan,
            endHz: center + halfSpan,
            maxLines: Self.freqGridMaxLines
        )
    }
}
