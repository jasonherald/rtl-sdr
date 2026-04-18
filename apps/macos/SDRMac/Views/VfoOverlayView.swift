//
// VfoOverlayView.swift — SwiftUI overlay drawn atop the Metal
// spectrum/waterfall. Shows the VFO passband as a semi-
// transparent band plus a center-frequency tick. Click-to-tune
// and drag-to-retune write back through `CoreModel.setVfoOffset`.
//
// ## Coordinate system
//
// The Metal renderer paints the full `effectiveSampleRateHz`
// span across the view's width. Offsets are measured from the
// tuner center (bin 0 in FFT terms is to the left). So:
//
//   left edge   ↔ vfoOffsetHz = -sampleRate/2
//   center      ↔ vfoOffsetHz =  0
//   right edge  ↔ vfoOffsetHz = +sampleRate/2
//
// Converting a click x in [0, width] back to Hz:
//
//   offsetHz = (x/width - 0.5) * sampleRate
//
// We clamp to the visible span — dragging past an edge doesn't
// re-center the tuner (that's a follow-up; see SDR++-style
// edge scrolling). For now, the VFO stops at the edges.
//
// ## Visuals
//
// Matching the GTK overlay's blue palette (see
// `crates/sdr-ui/src/spectrum/vfo_overlay.rs`): a light blue
// passband with low alpha, brighter blue center line. Visible
// against the turbo colormap's cold (blue-black) end and hot
// (red-yellow) end alike.

import SwiftUI

struct VfoOverlayView: View {
    /// The source of truth for offset / bandwidth / span. We
    /// read current values per-frame from this model and write
    /// back via `setVfoOffset(_:)` on drag. Taking the whole
    /// model (not individual bindings) keeps the call site
    /// light and avoids a handful of `@Binding` properties
    /// that would each cause a view update.
    let model: CoreModel

    // Match the GTK overlay blue palette so both UIs feel
    // like the same product.
    private static let bandColor = Color(red: 0.2, green: 0.6, blue: 1.0)
    private static let centerColor = Color(red: 0.3, green: 0.7, blue: 1.0)

    var body: some View {
        GeometryReader { geo in
            let width = geo.size.width
            let height = geo.size.height
            // Full frequency span painted by the Metal renderer.
            // The FFT is computed on the RAW (pre-decimation)
            // IQ stream so the spectrum shows the full tuner
            // bandwidth — same convention as the GTK UI (see
            // `crates/sdr-ui/src/spectrum/mod.rs:244` and
            // `crates/sdr-pipeline/src/iq_frontend.rs:156`).
            // Updated from `DisplayBandwidth` events, not
            // `SampleRateChanged` (which carries the post-
            // decimation passband).
            let span = model.displayBandwidthHz

            // Pixel positions derived from current state. We
            // fall through to zero-width when span is 0 so the
            // overlay degrades to invisible rather than dividing
            // by zero.
            let centerPx = span > 0
                ? (model.vfoOffsetHz / span + 0.5) * width
                : width / 2
            let bwPixels = span > 0
                ? model.bandwidthHz / span * width
                : 0
            let leftPx = centerPx - bwPixels / 2

            ZStack(alignment: .topLeading) {
                // Passband fill — wide enough to cover the
                // demodulator's accepted bandwidth. Alpha kept
                // low so waterfall detail underneath stays
                // visible.
                Rectangle()
                    .fill(Self.bandColor.opacity(0.18))
                    .frame(width: max(0, bwPixels), height: height)
                    .offset(x: leftPx)
                    .allowsHitTesting(false)

                // Passband edges — a crisper line on each side
                // so the band reads as a channel, not just a
                // tint. Half-pixel width to stay sharp on non-
                // retina and avoid blur on retina.
                Rectangle()
                    .fill(Self.bandColor.opacity(0.6))
                    .frame(width: 1, height: height)
                    .offset(x: leftPx)
                    .allowsHitTesting(false)
                Rectangle()
                    .fill(Self.bandColor.opacity(0.6))
                    .frame(width: 1, height: height)
                    .offset(x: leftPx + max(0, bwPixels))
                    .allowsHitTesting(false)

                // VFO center tick — the "tuned" frequency. One
                // pixel wide, brighter blue.
                Rectangle()
                    .fill(Self.centerColor)
                    .frame(width: 1, height: height)
                    .offset(x: centerPx)
                    .allowsHitTesting(false)
            }
            // Full-area hit shape so clicks anywhere on the
            // spectrum retune. `minimumDistance: 0` makes the
            // gesture fire on both taps and drags.
            .contentShape(Rectangle())
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { value in
                        applyDrag(at: value.location.x, width: width, span: span)
                    }
            )
        }
    }

    /// Convert the drag's x-pixel to an offset-from-tuner-center
    /// frequency and push it to the engine via CoreModel.
    /// Clamped to the visible span so we don't produce huge
    /// offsets that would be outside anything meaningful. For
    /// now, dragging past the edge just parks the VFO at the
    /// edge; a future pass can add tuner re-centering.
    private func applyDrag(at x: CGFloat, width: CGFloat, span: Double) {
        guard width > 0, span > 0 else { return }
        let frac = max(0, min(1, x / width))
        let rawOffset = (Double(frac) - 0.5) * span
        let halfSpan = span / 2
        let clamped = max(-halfSpan, min(halfSpan, rawOffset))
        model.setVfoOffset(clamped)
    }
}
