//
// CenterView.swift — main spectrum + waterfall area.
//
// Hosts the Metal-backed renderer via `SpectrumWaterfallView`
// (NSViewRepresentable + CAMetalLayer). The renderer consumes
// min/max dB bindings from `CoreModel` — the user adjusts these
// via the Display sidebar section, and the shader saturate()
// maps the dB range to the visible vertical axis. The renderer
// also pulls FFT frames directly from `model.core` on each
// display-link tick.
//
// Two zoom gestures are layered on top:
// - **Scroll wheel** (mouse wheel or two-finger trackpad
//   scroll). `NSEvent.addLocalMonitorForEvents` hooks all
//   scroll events in the window and we filter by view bounds.
//   Cursor-centered zoom — the frequency under the cursor stays
//   under the cursor through the zoom.
// - **Pinch** (trackpad magnify / iPad pinch). SwiftUI
//   `MagnifyGesture` is cross-platform, so this carries over to
//   iPad for free. View-center-centered zoom.
//
// Both drive `CoreModel.zoomView(by:around:)`.

import AppKit
import SwiftUI

struct CenterView: View {
    @Environment(CoreModel.self) private var model

    /// Snapshot of `displayedSpanHz` at pinch start so
    /// `MagnifyGesture.value.magnification` (a cumulative
    /// multiplier) can be converted into an absolute span.
    @State private var pinchStartSpanHz: Double = 0

    var body: some View {
        @Bindable var m = model
        ZStack {
            // 1. Metal spectrum + waterfall (bottom layer)
            SpectrumWaterfallView(
                model: model,
                minDb: $m.minDb,
                maxDb: $m.maxDb
            )
            // 2. Frequency / dB grid + labels. Non-hit-testing
            //    so clicks pass through to the VFO overlay above.
            SpectrumGridView(model: model)
            // 3. VFO band + center tick + click-to-tune. On top
            //    so its DragGesture captures clicks. The grid
            //    underneath renders behind the translucent VFO
            //    band — same layering as SDR++ / the GTK UI.
            VfoOverlayView(model: model)
        }
        .frame(minHeight: 300)
        // Pinch zoom — cross-platform via SwiftUI's gesture.
        // MagnifyGesture.value.magnification is a cumulative
        // multiplier from 1.0 at gesture start, so we snapshot
        // the span at start and apply the ratio each tick.
        .gesture(
            MagnifyGesture()
                .onChanged { value in
                    if pinchStartSpanHz == 0 {
                        pinchStartSpanHz = model.effectiveDisplayedSpanHz
                    }
                    let factor = max(0.01, min(100, value.magnification))
                    let oldSpan = model.effectiveDisplayedSpanHz
                    let targetSpan = pinchStartSpanHz / factor
                    // Convert the target span into a zoom factor
                    // relative to CURRENT span so `zoomView` can
                    // clamp + cursor-center around the viewport
                    // middle. Pinch doesn't expose a focal point
                    // on macOS, so we center on the viewport.
                    let zoomFactor = oldSpan / targetSpan
                    model.zoomView(
                        by: zoomFactor,
                        around: model.displayedCenterOffsetHz
                    )
                }
                .onEnded { _ in
                    pinchStartSpanHz = 0
                }
        )
        // Scroll-wheel zoom. Catches scroll events in the
        // window's event stream and filters by view bounds so
        // clicks, drags, etc. still work on the VFO overlay
        // underneath.
        .overlay(
            ScrollWheelZoomCatcher { deltaY, fracX in
                // GTK uses ZOOM_FACTOR = 1.2 per notch (see
                // `vfo_overlay.rs:40`). Positive deltaY (scroll
                // up / away from user) zooms in; negative zooms
                // out. On trackpads, deltaY arrives as small
                // fractional values — scale so gentle swipes
                // still zoom noticeably.
                let step = 1.0 + 0.2 * min(1.0, abs(Double(deltaY)))
                let factor = deltaY > 0 ? step : 1.0 / step
                // Cursor-centered: compute the Hz under the
                // cursor and zoom around that.
                let span = model.effectiveDisplayedSpanHz
                let focalOffsetHz =
                    model.displayedCenterOffsetHz + (Double(fracX) - 0.5) * span
                model.zoomView(by: factor, around: focalOffsetHz)
            }
            .allowsHitTesting(false)
        )
        // Double-click resets zoom (common convention — matches
        // what most trackpad-first macOS apps do for "fit view").
        .onTapGesture(count: 2) {
            model.resetZoom()
        }
    }
}

// ============================================================
//  ScrollWheelZoomCatcher
// ============================================================
//
//  SwiftUI doesn't expose scroll-wheel events on arbitrary
//  views (only inside `ScrollView`). Register a local event
//  monitor with AppKit and filter by view bounds.
//
//  Using `.overlay(...).allowsHitTesting(false)` so clicks
//  still land on the VFO layer below — the monitor taps into
//  the event stream at the window level, not via hit-testing.

private struct ScrollWheelZoomCatcher: NSViewRepresentable {
    /// Called on each scroll event over this view's bounds.
    /// `deltaY` is `event.scrollingDeltaY` (positive = scroll
    /// up, zoom in). `fracX` is 0…1 cursor position across
    /// the view's width.
    let onScroll: (CGFloat, CGFloat) -> Void

    func makeNSView(context: Context) -> MonitorView {
        MonitorView(onScroll: onScroll)
    }

    func updateNSView(_ nsView: MonitorView, context: Context) {
        nsView.onScroll = onScroll
    }

    final class MonitorView: NSView {
        var onScroll: (CGFloat, CGFloat) -> Void
        private var monitor: Any?

        init(onScroll: @escaping (CGFloat, CGFloat) -> Void) {
            self.onScroll = onScroll
            super.init(frame: .zero)
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) {
            fatalError("ScrollWheelZoomCatcher.MonitorView does not support NSCoder init")
        }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if window != nil {
                startMonitor()
            } else {
                stopMonitor()
            }
        }

        deinit {
            stopMonitor()
        }

        private func startMonitor() {
            guard monitor == nil else { return }
            // Local monitor: fires for events in OUR process's
            // event queue. Return `nil` from the block to consume
            // (so the scroll doesn't also scroll any ancestor
            // ScrollView); return the event to pass it on.
            monitor = NSEvent.addLocalMonitorForEvents(matching: .scrollWheel) { [weak self] event in
                guard let self, let window = self.window else { return event }
                // Only our window, only when cursor is over this
                // view. Don't consume scrolls over the sidebar
                // or header.
                guard event.window === window else { return event }
                let locInView = self.convert(event.locationInWindow, from: nil)
                guard self.bounds.contains(locInView) else { return event }

                // Skip trackpad momentum tails. A flick-scroll
                // delivers dozens of momentum events after the
                // fingers leave the pad; zooming on each one
                // rockets past the user's intent. Keep only
                // discrete wheel clicks and active-touch scrolls
                // — `.momentumPhase == []` covers both.
                if !event.momentumPhase.isEmpty {
                    return event
                }

                let fracX = self.bounds.width > 0
                    ? locInView.x / self.bounds.width
                    : 0.5
                self.onScroll(event.scrollingDeltaY, fracX)
                return nil
            }
        }

        private func stopMonitor() {
            if let m = monitor {
                NSEvent.removeMonitor(m)
                monitor = nil
            }
        }
    }
}
