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

    /// Pixel radius for "grabbing" a bandwidth edge. Matches the
    /// GTK `BW_HANDLE_THRESHOLD_PX = 8.0` in `vfo_overlay.rs`.
    /// Within this distance of a passband edge, a drag resizes
    /// the bandwidth instead of retuning the tuner.
    private static let edgeThresholdPx: CGFloat = 8

    /// Bandwidth clamp — matches GTK's MIN_BANDWIDTH_HZ (500) /
    /// MAX_BANDWIDTH_HZ (250 kHz) in `vfo_overlay.rs`. Prevents
    /// the user from dragging an edge past the VFO center
    /// (negative bandwidth) or past the visible span.
    private static let minBandwidthHz: Double = 500
    private static let maxBandwidthHz: Double = 250_000

    /// Classification of the active drag gesture. Captured on
    /// the first `.onChanged` event of each drag and honored for
    /// the rest so a drag that *starts* near an edge keeps
    /// resizing even when the mouse wanders toward the center.
    private enum DragKind {
        case idle
        case retune
        case resizeLeft
        case resizeRight
    }
    @State private var dragKind: DragKind = .idle

    /// Snapshot of the VFO state at drag start, so edge-resize
    /// math can pin the opposite edge in Hz space regardless of
    /// how far the drag moves.
    @State private var dragStartVfoOffsetHz: Double = 0
    @State private var dragStartBandwidthHz: Double = 0

    var body: some View {
        GeometryReader { geo in
            let width = geo.size.width
            let height = geo.size.height
            // Use the displayed-viewport span (not the full FFT
            // span) — so the overlay zooms with the waterfall.
            // When there's no zoom this equals
            // `displayBandwidthHz` and the math collapses to the
            // unzoomed case.
            let span = model.effectiveDisplayedSpanHz
            // Viewport center in offset-from-tuner-center Hz. 0
            // when unzoomed.
            let viewCenter = model.displayedCenterOffsetHz

            // Pixel positions derived from current state. We
            // fall through to zero-width when span is 0 so the
            // overlay degrades to invisible rather than dividing
            // by zero.
            let centerPx = span > 0
                ? ((model.vfoOffsetHz - viewCenter) / span + 0.5) * width
                : width / 2
            let bwPixels = span > 0
                ? model.bandwidthHz / span * width
                : 0
            let leftPx = centerPx - bwPixels / 2

            ZStack(alignment: .topLeading) {
                // Clear backdrop forces the ZStack to fill the
                // full geo size. Without this, the ZStack sizes
                // to its largest child (the band), and
                // `.contentShape` only covers that strip — which
                // is why click-to-tune outside the band didn't
                // work.
                Color.clear
                    .frame(width: width, height: height)

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
                        handleDrag(
                            currentX: value.location.x,
                            startX: value.startLocation.x,
                            width: width,
                            span: span,
                            centerPx: centerPx,
                            bwPixels: bwPixels
                        )
                    }
                    .onEnded { _ in
                        dragKind = .idle
                    }
            )
        }
    }

    /// Top-level drag handler. Classifies the drag on its first
    /// frame (retune vs edge-resize) and dispatches to the
    /// appropriate helper. The classification sticks for the
    /// duration of the drag so the gesture doesn't flip modes
    /// when the mouse crosses another edge mid-drag.
    private func handleDrag(
        currentX: CGFloat,
        startX: CGFloat,
        width: CGFloat,
        span: Double,
        centerPx: CGFloat,
        bwPixels: CGFloat
    ) {
        guard width > 0, span > 0 else { return }

        if dragKind == .idle {
            dragKind = classify(
                startX: startX,
                centerPx: centerPx,
                bwPixels: bwPixels
            )
            dragStartVfoOffsetHz = model.vfoOffsetHz
            dragStartBandwidthHz = model.bandwidthHz
        }

        switch dragKind {
        case .idle, .retune:
            retuneAt(x: currentX, width: width, span: span)
        case .resizeLeft:
            resize(edge: .left, currentX: currentX, width: width, span: span)
        case .resizeRight:
            resize(edge: .right, currentX: currentX, width: width, span: span)
        }
    }

    /// Hit-test the drag's starting x against the passband edges.
    /// Within `edgeThresholdPx` of either edge = resize; anywhere
    /// else = retune.
    private func classify(
        startX: CGFloat,
        centerPx: CGFloat,
        bwPixels: CGFloat
    ) -> DragKind {
        let leftPx = centerPx - bwPixels / 2
        let rightPx = centerPx + bwPixels / 2
        if abs(startX - leftPx) <= Self.edgeThresholdPx {
            return .resizeLeft
        }
        if abs(startX - rightPx) <= Self.edgeThresholdPx {
            return .resizeRight
        }
        return .retune
    }

    /// Convert the drag's x-pixel to an absolute frequency and
    /// RETUNE the hardware tuner to center on it, parking the
    /// VFO offset at 0.
    ///
    /// Matches the GTK UI's click-to-tune behaviour
    /// (`crates/sdr-ui/src/window.rs:270` — the VFO-offset-change
    /// callback pulls `center + offset` and calls `Tune(...)`).
    /// Setting `vfoOffset` directly doesn't work for clicks
    /// outside ±effective_sample_rate/2 because the demod only
    /// processes the post-decimation passband; retuning the
    /// tuner to the clicked frequency puts the signal back at
    /// DC where the demod can lock onto it.
    private func retuneAt(x: CGFloat, width: CGFloat, span: Double) {
        let frac = max(0, min(1, x / width))
        // Absolute Hz = tuner center + viewport-center offset +
        // position-within-viewport. `span` is the DISPLAYED
        // span (may be narrower than the full FFT when zoomed);
        // `viewCenter` is the zoom viewport's center offset (0
        // when unzoomed). Unzoomed, this collapses to the old
        // `center + (frac - 0.5) * fullSpan` formula.
        let viewCenter = model.displayedCenterOffsetHz
        let absoluteHz =
            model.centerFrequencyHz + viewCenter + (Double(frac) - 0.5) * span
        guard absoluteHz > 0 else { return }
        model.setCenter(absoluteHz)
        if model.vfoOffsetHz != 0 {
            model.setVfoOffset(0)
        }
    }

    private enum ResizeEdge { case left, right }

    /// Resize the passband by dragging one edge. The OPPOSITE
    /// edge stays pinned in Hz space (snapshot at drag start),
    /// so the visual mental model — "I'm dragging THIS edge" —
    /// holds regardless of how far the drag goes. New
    /// bandwidth is clamped to [minBandwidthHz, maxBandwidthHz]
    /// so the user can't produce negative / nonsensical sizes.
    private func resize(edge: ResizeEdge, currentX: CGFloat, width: CGFloat, span: Double) {
        // Drag position in offset-from-tuner-center Hz (same
        // frame as `vfoOffsetHz` / `bandwidthHz`). `span` is the
        // displayed-viewport span; the viewport may be offset
        // from tuner center when zoomed, so add that in.
        let frac = max(0, min(1, currentX / width))
        let viewCenter = model.displayedCenterOffsetHz
        let dragOffsetHz = viewCenter + (Double(frac) - 0.5) * span

        // Pinned opposite edge in offset-Hz.
        let leftAtStart = dragStartVfoOffsetHz - dragStartBandwidthHz / 2
        let rightAtStart = dragStartVfoOffsetHz + dragStartBandwidthHz / 2

        let newLeft: Double
        let newRight: Double
        switch edge {
        case .left:
            newLeft = min(dragOffsetHz, rightAtStart - Self.minBandwidthHz)
            newRight = rightAtStart
        case .right:
            newRight = max(dragOffsetHz, leftAtStart + Self.minBandwidthHz)
            newLeft = leftAtStart
        }

        let newBandwidth = max(Self.minBandwidthHz, min(Self.maxBandwidthHz, newRight - newLeft))
        let newCenter = (newLeft + newRight) / 2

        // Only push to the engine when the values actually
        // changed — avoids a flood of identical commands on
        // tiny pixel movements.
        if abs(newBandwidth - model.bandwidthHz) > 0.5 {
            model.setBandwidth(newBandwidth)
        }
        if abs(newCenter - model.vfoOffsetHz) > 0.5 {
            model.setVfoOffset(newCenter)
        }
    }
}
