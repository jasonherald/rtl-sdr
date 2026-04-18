//
// MetalSpectrumNSView.swift — plain NSView whose backing layer
// is a CAMetalLayer, driven by a CAMetalDisplayLink.
//
// ## Why this exists (instead of an MTKView subclass)
//
// See the comment at the top of `SpectrumRenderer.swift` for the
// full story. Short version: `MTKView` drives rendering via
// `NSView.draw()`, which AppKit can re-enter from dirty-rect
// passes when the window is focused and SwiftUI is actively
// invalidating layout. That's almost certainly what was causing
// "Mission Control thumbnail renders clean, focused window
// renders partial/torn" — the direct compositor path was
// re-entering draw() at unexpected times.
//
// `CAMetalDisplayLink` (macOS 14+) delivers vsync'd callbacks
// independent of AppKit's display mechanism. It also hands us
// the `CAMetalDrawable` directly in the callback, avoiding the
// `nextDrawable()` contention that `MTKView` runs into.
//
// ## The NSView
//
// This view owns a `CAMetalLayer` as its backing layer via
// `makeBackingLayer()`. We set `wantsLayer = true` and AppKit
// does the rest — the layer IS our view's CALayer.
//
// ## Lifecycle
//
// - `viewDidMoveToWindow()` — attach / detach the display link
//   based on whether we have a window. Important: the display
//   link must not outlive the window it's bound to, and we don't
//   want it firing into a teardown.
// - `viewDidChangeBackingProperties()` — retina scale changed
//   (e.g. dragged to a different display). Update
//   `contentsScale` + `drawableSize` accordingly.
// - `layout()` — view bounds changed. Update `drawableSize`
//   to match.
// - `deinit` — belt and suspenders: invalidate the link in case
//   `viewDidMoveToWindow(nil)` wasn't called.

import AppKit
import Metal
import QuartzCore
import SdrCoreKit

final class MetalSpectrumNSView: NSView, CAMetalDisplayLinkDelegate {
    // ----------------------------------------------------------
    //  Dependencies
    // ----------------------------------------------------------

    private let renderer: SpectrumRenderer
    private var displayLink: CAMetalDisplayLink?
    private let powerObserver = PowerModeObserver()

    /// Typed accessor for the backing layer. `makeBackingLayer()`
    /// returns a `CAMetalLayer`, so the force-cast is safe.
    private var metalLayer: CAMetalLayer {
        // swiftlint:disable:next force_cast
        layer as! CAMetalLayer
    }

    // ----------------------------------------------------------
    //  Init
    // ----------------------------------------------------------

    init(renderer: SpectrumRenderer, coreProvider: @escaping () -> SdrCore?) {
        self.renderer = renderer
        super.init(frame: .zero)

        // Wire the renderer to poll the engine handle each
        // frame. The closure is invoked on the display-link
        // thread (which we've attached to the main runloop, so
        // it's the main thread in practice) — safe to read
        // `CoreModel.core` from there given CoreModel is
        // @MainActor.
        renderer.coreProvider = coreProvider

        // Re-apply the display link's rate range on any AC /
        // battery / Low Power Mode transition. `powerObserver`
        // owns the wiring; we just react to the published mode.
        powerObserver.onChange = { [weak self] mode in
            self?.applyPowerMode(mode)
        }

        // Tell AppKit we're layer-backed and that we want to
        // supply our own layer (a CAMetalLayer) rather than
        // using the default CALayer.
        wantsLayer = true
        // Redraw our layer during live resize so the drawable
        // tracks bounds changes smoothly. Metal handles the
        // actual render off the display link — this just tells
        // AppKit not to freeze content during resize.
        layerContentsRedrawPolicy = .duringViewResize

        // The layer was created by `makeBackingLayer()` when
        // `wantsLayer = true` was set. Configure it now that
        // Swift sees it as a CAMetalLayer.
        metalLayer.device = renderer.device
        metalLayer.pixelFormat = renderer.colorPixelFormat
        // YES — our drawables are only used as render targets,
        // never sampled. Lets CoreAnimation pick a display-
        // optimized tiling.
        metalLayer.framebufferOnly = true
        // Fully opaque background: CAMetalLayer defaults to
        // opaque on macOS, but make it explicit. Transparent
        // Metal layers go through a slower alpha-composite
        // path on the window server — we don't need that.
        metalLayer.isOpaque = true
        // Explicit triple-buffering. Matches the in-flight
        // semaphore value in `SpectrumRenderer`. The default
        // is 3, but declaring intent here means a future
        // change to the default doesn't silently alter our
        // frame pacing.
        metalLayer.maximumDrawableCount = 3
        // We present via the display link at
        // `update.targetPresentationTimestamp`; NO need (and
        // indeed harmful) to synchronize with the CA transaction
        // that layout drives. Leaving `presentsWithTransaction`
        // at its default (false) is the whole point of this
        // refactor.
        metalLayer.presentsWithTransaction = false
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("MetalSpectrumNSView does not support NSCoder init")
    }

    deinit {
        displayLink?.invalidate()
    }

    // ----------------------------------------------------------
    //  Backing layer
    // ----------------------------------------------------------

    /// AppKit hook: when `wantsLayer = true`, AppKit asks the
    /// view to provide its backing layer here. Returning a
    /// CAMetalLayer is the documented way to make a Metal-
    /// backed NSView.
    override func makeBackingLayer() -> CALayer {
        CAMetalLayer()
    }

    // ----------------------------------------------------------
    //  Window / layout / backing scale
    // ----------------------------------------------------------

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if window != nil {
            updateDrawableSize()
            startDisplayLink()
        } else {
            stopDisplayLink()
        }
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        // Backing scale (retina factor) changed — most commonly
        // because the window was dragged to a different display.
        updateDrawableSize()
    }

    override func layout() {
        super.layout()
        updateDrawableSize()
    }

    /// Recompute the drawable size in pixels from the view's
    /// points-bounds and the window's backing scale factor, and
    /// push it to the metal layer. If either is zero we skip —
    /// the metal layer doesn't allow 0-sized drawables.
    private func updateDrawableSize() {
        let scale = window?.backingScaleFactor ?? (layer?.contentsScale ?? 2.0)
        metalLayer.contentsScale = scale

        let pixelWidth = bounds.width * scale
        let pixelHeight = bounds.height * scale
        guard pixelWidth > 0, pixelHeight > 0 else { return }

        let newSize = CGSize(width: pixelWidth, height: pixelHeight)
        // Skip the assignment if nothing changed — setting
        // drawableSize drops the layer's drawable pool, so
        // assigning-equal would cause a transient flash.
        if metalLayer.drawableSize != newSize {
            metalLayer.drawableSize = newSize
        }
    }

    // ----------------------------------------------------------
    //  Display link lifecycle
    // ----------------------------------------------------------

    private func startDisplayLink() {
        guard displayLink == nil else { return }
        let link = CAMetalDisplayLink(metalLayer: metalLayer)
        link.delegate = self
        link.preferredFrameRateRange = Self.rateRange(for: powerObserver.mode)
        link.add(to: .main, forMode: .common)
        displayLink = link
    }

    /// Recompute and push the preferred frame rate range to the
    /// display link when power posture changes. `link` may be
    /// nil if the view is offscreen (window == nil); in that
    /// case we noop — `startDisplayLink` will re-read the mode
    /// next time the view attaches.
    private func applyPowerMode(_ mode: PowerMode) {
        displayLink?.preferredFrameRateRange = Self.rateRange(for: mode)
    }

    /// Display-link rate range per power mode.
    ///
    /// - `.acFull`: ProMotion-friendly — let the display link
    ///   match display-native refresh (120 Hz on M-series
    ///   laptops). `preferred: 60` is a hint that balances
    ///   visual smoothness against unnecessary GPU work; the
    ///   system picks an actual rate in the window.
    /// - `.conserve`: clamped to 10–30 fps with a 20 fps
    ///   preferred rate. That roughly matches typical FFT engine
    ///   cadence, so the waterfall still moves at its natural
    ///   pace without the render loop burning extra cycles on
    ///   no-new-data ticks.
    private static func rateRange(for mode: PowerMode) -> CAFrameRateRange {
        switch mode {
        case .acFull:
            return CAFrameRateRange(minimum: 30, maximum: 120, preferred: 60)
        case .conserve:
            return CAFrameRateRange(minimum: 10, maximum: 30, preferred: 20)
        }
    }

    private func stopDisplayLink() {
        displayLink?.invalidate()
        displayLink = nil
    }

    // ----------------------------------------------------------
    //  CAMetalDisplayLinkDelegate
    // ----------------------------------------------------------

    func metalDisplayLink(_ link: CAMetalDisplayLink, needsUpdate update: CAMetalDisplayLink.Update) {
        // The display link hands us a drawable that's already
        // allocated and bound to this vsync. No `nextDrawable()`
        // call, no contention.
        let size = metalLayer.drawableSize
        renderer.encode(into: update.drawable, drawableSize: size)

        // Surface the live FPS in the window title so the user
        // can read it without pulling logs. One string-replace
        // per second is cheap.
        if let window = self.window {
            let fps = renderer.measuredFps
            if fps > 0 {
                window.title = String(format: "sdr-rs — %.0f fps", fps)
            }
        }
    }

    // ----------------------------------------------------------
    //  Bindings forwarding
    // ----------------------------------------------------------

    func applyBindings(minDb: Float, maxDb: Float) {
        renderer.applyBindings(minDb: minDb, maxDb: maxDb)
    }
}
