//
// SpectrumMTKView.swift — Metal-backed spectrum (and, in
// sub-PR 2+, waterfall) renderer. Drives a single MTKView on the
// main thread, pulls FFT data each frame, encodes one render
// pass per vsync / setNeedsDisplay tick.
//
// This sub-PR (M4/1) renders the **spectrum line only** from a
// `SyntheticFftSource`. The waterfall full-screen quad pass
// and the viewport split between spectrum and waterfall land
// in the next sub-PR.
//
// Power discipline: `enableSetNeedsDisplay = true` means we
// only draw when explicitly invalidated. The CVDisplayLink
// tick below advances the synthetic source at 20 Hz and
// invalidates — matching the default FFT rate we'll use with
// the real engine. Power-source detection (AC vs battery →
// flip to continuous vsync on plug-in) lands in sub-PR 3.

import MetalKit
import OSLog
// SdrCoreKit is not imported here yet — sub-PR 1 uses a
// synthetic FFT source. Sub-PR 3 adds the import + swaps
// `SyntheticFftSource` for `SdrCore.withLatestFftFrame`.

/// Renderer-scoped logger. Visible via
/// `log stream --predicate 'subsystem == "com.sdr.rs" AND category == "renderer"'`.
private let renderLog = Logger(subsystem: "com.sdr.rs", category: "renderer")

/// Uniform block uploaded once per frame. `#pragma pack`-style
/// layout matches `struct Uniforms` in `Shaders.metal` — keep
/// field order + types in sync.
struct RendererUniforms {
    var minDb: Float = -100
    var maxDb: Float = 0
    var binCount: UInt32 = 2048
    var historyRows: UInt32 = 1024   // used in sub-PR 2+
    var writeRow: UInt32 = 0         // used in sub-PR 2+
    var _pad0: UInt32 = 0            // keep 8-byte multiple
}

/// The MTKView subclass that owns all Metal resources and drives
/// the render loop. `SpectrumWaterfallView` hosts this via
/// `NSViewRepresentable`.
final class SpectrumMTKView: MTKView {
    // ----------------------------------------------------------
    //  Metal resources (created once at init)
    // ----------------------------------------------------------

    private let commandQueue: MTLCommandQueue
    private let spectrumPipeline: MTLRenderPipelineState
    private let waterfallPipeline: MTLRenderPipelineState

    /// 256×1 rgba8Unorm colormap LUT sampled by the waterfall
    /// fragment shader. Built once at init from
    /// `Palettes.turbo`.
    private let paletteTexture: MTLTexture

    /// `fftBins × historyRows` r32Float ring texture. One row
    /// per published FFT frame; `writeRow` advances each draw
    /// and wraps. Recreated lazily if the FFT bin count changes
    /// (rare — user-initiated via the Display panel).
    ///
    /// `.storageMode = .private` so writes flow through the
    /// blit encoder (see `historyStagingBuffer`) rather than
    /// `MTLTexture.replace()`. CPU-side `.replace()` on a
    /// shared-storage texture RACES GPU reads from a
    /// previously-committed but not-yet-completed render pass —
    /// the race is invisible at 1 fps but obvious as "pieces
    /// and artifacts" at 120 fps on ProMotion. Blit→render in
    /// the same command buffer is sequenced by Metal itself,
    /// so no race.
    private var historyTexture: MTLTexture?

    /// Row-sized shared buffer that new FFT rows memcpy'd into,
    /// then blit-copied to the private history texture each
    /// frame. Sized for `maxFftBins`, so one allocation per
    /// renderer lifetime.
    private let historyStagingBuffer: MTLBuffer

    /// Current waterfall width in bins, tracked so we can
    /// detect FFT-size changes and rebuild `historyTexture`.
    private var historyBinCount: Int = 0

    /// Ring cursor. Always in `[0, historyRows)`.
    private var writeRow: UInt32 = 0

    /// Rows of waterfall history. Per the spec — at 20 Hz gives
    /// ~50 s of history, at 60 Hz (sub-PR 3's AC-power mode)
    /// still a comfortable 17 s.
    static let historyRows: UInt32 = 1024

    /// Preallocated buffer holding the current frame's
    /// magnitudes in dB. Sized for the MAX supported FFT size
    /// (8192) so we never reallocate when the user changes FFT
    /// size. `.storageModeShared` so CPU writes are visible to
    /// the GPU without a blit.
    private let spectrumVertexBuffer: MTLBuffer

    /// Maximum FFT bin count we pre-size for. Matches the
    /// upper bound the spec picked (8192). `fftBins` on the
    /// frame path stays ≤ this.
    static let maxFftBins = 8192

    /// Fraction of the view's height devoted to the spectrum
    /// line (the rest is waterfall). Matches the GTK UI's
    /// 30/70 split.
    private static let spectrumFraction: CGFloat = 0.30

    // ----------------------------------------------------------
    //  Data source (synthetic for sub-PR 1; real SdrCore in 3)
    // ----------------------------------------------------------

    /// Generates moving spectrum peaks for the renderer to
    /// display while the Metal pipeline is validated in
    /// isolation. Sub-PR 3 removes this and pulls from
    /// `SdrCore.withLatestFftFrame` instead.
    private let syntheticSource: SyntheticFftSource

    /// Uniform block. Mutated each frame before encoding.
    private var uniforms = RendererUniforms()

    /// Instrumentation: timestamps of recent `draw()` entries.
    /// Used by `logFrameRateIfNeeded` to print the measured FPS
    /// every ~1 s so we can diagnose jitter vs. a cleanly
    /// scheduled display-link.
    private var lastFrameRateLog: CFTimeInterval = 0
    private var framesSinceLog: Int = 0


    // ----------------------------------------------------------
    //  Factory
    // ----------------------------------------------------------

    /// Build a configured renderer. Returns `nil` if Metal
    /// setup fails (no device, shader library missing,
    /// pipeline compile error). The callers render a SwiftUI
    /// fallback in that case — see `SpectrumWaterfallView`.
    ///
    /// `MTKView.init(frame:)` isn't failable so we can't
    /// override it as failable. A factory lets us keep the
    /// fail-soft contract without lying about the superclass
    /// initializer.
    static func make() -> SpectrumMTKView? {
        guard let device = MTLCreateSystemDefaultDevice(),
              let queue = device.makeCommandQueue(),
              let library = device.makeDefaultLibrary(),
              let specVert = library.makeFunction(name: "spectrum_vert"),
              let specFrag = library.makeFunction(name: "spectrum_frag"),
              let wfVert = library.makeFunction(name: "waterfall_vert"),
              let wfFrag = library.makeFunction(name: "waterfall_frag")
        else {
            return nil
        }

        let specDesc = MTLRenderPipelineDescriptor()
        specDesc.label = "spectrum_line"
        specDesc.vertexFunction = specVert
        specDesc.fragmentFunction = specFrag
        specDesc.colorAttachments[0].pixelFormat = .bgra8Unorm
        guard let specPipeline = try? device.makeRenderPipelineState(descriptor: specDesc) else {
            return nil
        }

        let wfDesc = MTLRenderPipelineDescriptor()
        wfDesc.label = "waterfall_quad"
        wfDesc.vertexFunction = wfVert
        wfDesc.fragmentFunction = wfFrag
        wfDesc.colorAttachments[0].pixelFormat = .bgra8Unorm
        guard let wfPipeline = try? device.makeRenderPipelineState(descriptor: wfDesc) else {
            return nil
        }

        let bufLen = Self.maxFftBins * MemoryLayout<Float>.stride
        guard let buf = device.makeBuffer(length: bufLen, options: .storageModeShared) else {
            return nil
        }
        buf.label = "spectrum_mags_db"

        // Row-sized staging buffer for history writes. Same
        // max bin count so we never reallocate. `.shared` so
        // CPU `memcpy` is visible to the blit encoder without
        // an intermediate sync.
        guard let staging = device.makeBuffer(length: bufLen, options: .storageModeShared) else {
            return nil
        }
        staging.label = "history_row_staging"

        guard let palette = Palettes.turbo.makeTexture(device: device) else {
            return nil
        }
        palette.label = "turbo_palette_lut"

        return SpectrumMTKView(
            device: device,
            queue: queue,
            spectrumPipeline: specPipeline,
            waterfallPipeline: wfPipeline,
            vertexBuffer: buf,
            stagingBuffer: staging,
            paletteTexture: palette
        )
    }

    /// Designated init used by the factory. Private so callers
    /// must go through `make()` and can't bypass Metal validation.
    private init(
        device: MTLDevice,
        queue: MTLCommandQueue,
        spectrumPipeline: MTLRenderPipelineState,
        waterfallPipeline: MTLRenderPipelineState,
        vertexBuffer: MTLBuffer,
        stagingBuffer: MTLBuffer,
        paletteTexture: MTLTexture
    ) {
        self.commandQueue = queue
        self.spectrumPipeline = spectrumPipeline
        self.waterfallPipeline = waterfallPipeline
        self.spectrumVertexBuffer = vertexBuffer
        self.historyStagingBuffer = stagingBuffer
        self.paletteTexture = paletteTexture
        self.syntheticSource = SyntheticFftSource(binCount: 2048)

        super.init(frame: .zero, device: device)

        // MTKView configuration.
        self.colorPixelFormat = .bgra8Unorm
        self.clearColor = MTLClearColor(red: 0.09, green: 0.09, blue: 0.11, alpha: 1.0)
        self.framebufferOnly = true
        // Drive the drawable size from the view's bounds (the
        // MTKView default) AND tell AppKit to resize the view
        // when its container resizes.
        self.autoResizeDrawable = true
        self.autoresizingMask = [.width, .height]
        // `presentsWithTransaction = true` makes the drawable
        // present synchronize with CoreAnimation's transaction
        // commit. Without this, on a focused SwiftUI-hosted
        // window, Metal presents can race the window server's
        // own compositor updates — the symptom we saw was
        // that the MTKView rendered correctly (Mission Control
        // showed clean frames, off-focus showed smooth motion)
        // but when the app was focused and the compositor was
        // actively handling it, presents got torn/partial.
        //
        // Required pairing (see `draw()` below): instead of
        // `cmd.present(drawable); cmd.commit()`, we do
        // `cmd.commit(); cmd.waitUntilScheduled(); drawable.present()`.
        // Apple's MTKView docs call this out explicitly — the
        // present must happen AFTER the command buffer is
        // scheduled but BEFORE the CA transaction commits,
        // which is exactly what this sequence gives us.
        self.presentsWithTransaction = true
        // Let MTKView's internal display link drive rendering
        // at a fixed 20 fps — CADisplayLink-backed, synchronised
        // with the compositor, robust against main-thread
        // work from the audio dispatcher (~50 Hz SignalLevel
        // events) that would otherwise jitter a free-running
        // Timer.
        //
        // Let MTKView run the display link. `= 60` is a hint,
        // not a cap — on ProMotion (120 Hz) we measured 120 fps
        // actual with this set to 60, so the macOS impl gives
        // us display-native refresh rather than strict
        // throttling. That's fine; we gate DATA updates instead
        // via `lastDataTick`, decoupling motion smoothness
        // (display-link-driven) from data cadence (FFT rate).
        //
        // NB: setting `preferredFramesPerSecond = 0` pauses the
        // display link entirely on macOS — observed as a blank
        // view with `draw()` never firing. Stick with a positive
        // integer.
        self.enableSetNeedsDisplay = false
        self.isPaused = false
        self.preferredFramesPerSecond = 60
    }

    @available(*, unavailable)
    required init(coder: NSCoder) {
        fatalError("SpectrumMTKView does not support NSCoder init")
    }

    // ----------------------------------------------------------
    //  Binding updates from SwiftUI (called from updateNSView)
    // ----------------------------------------------------------

    func applyBindings(minDb: Float, maxDb: Float) {
        // Guard against NaN / degenerate range; renderer does
        // its own saturate but we want uniforms to be sane.
        if minDb.isFinite { uniforms.minDb = minDb }
        if maxDb.isFinite { uniforms.maxDb = max(minDb + 1.0, maxDb) }
    }

    // ----------------------------------------------------------
    //  Draw loop
    // ----------------------------------------------------------

    override func draw() {
        logFrameRateIfNeeded()

        // Advance the synthetic source every draw. Data rate
        // follows render rate here (120 Hz on ProMotion) which
        // makes the waterfall scroll faster than a real 20 Hz
        // engine would — fine for pipeline validation. Sub-PR 3
        // swaps the source for `SdrCore.withLatestFftFrame`,
        // which returns a bool indicating whether a new frame
        // arrived; on `false` we'll skip the memcpy + blit and
        // re-present with the existing texture state.
        syntheticSource.next()

        // 1. Copy the current bins into the spectrum vertex
        //    buffer AND the history-row staging buffer.
        let bins = syntheticSource.magnitudes
        let count = min(bins.count, Self.maxFftBins)
        let rowBytes = count * MemoryLayout<Float>.stride
        bins.withUnsafeBufferPointer { src in
            memcpy(spectrumVertexBuffer.contents(), src.baseAddress!, rowBytes)
            memcpy(historyStagingBuffer.contents(), src.baseAddress!, rowBytes)
        }
        uniforms.binCount = UInt32(count)
        uniforms.historyRows = Self.historyRows

        // 2. Ensure the history ring exists with the right
        //    width.
        guard let history = ensureHistoryTexture(binCount: count) else {
            return
        }

        uniforms.writeRow = writeRow
        let thisWriteRow = writeRow
        writeRow = (writeRow + 1) % Self.historyRows

        // 3. One command buffer with two encoders:
        //    - Blit encoder: copy staging buffer → history
        //      texture at `thisWriteRow`. This is implicitly
        //      ordered before any later render encoder in the
        //      same command buffer, which eliminates the race
        //      that `MTLTexture.replace()` on a shared-storage
        //      texture had at 120 fps.
        //    - Render encoder: waterfall (bottom viewport) +
        //      spectrum line (top viewport).
        //
        //    Single drawable request keeps both halves in sync
        //    and halves the per-frame Metal overhead vs two
        //    MTKViews.
        guard
            let drawable = currentDrawable,
            let passDesc = currentRenderPassDescriptor,
            let cmd = commandQueue.makeCommandBuffer()
        else {
            return
        }
        cmd.label = "sdr_frame"

        // --- Blit: staging → history[writeRow]
        if let blit = cmd.makeBlitCommandEncoder() {
            blit.label = "history_row_blit"
            blit.copy(
                from: historyStagingBuffer,
                sourceOffset: 0,
                sourceBytesPerRow: rowBytes,
                sourceBytesPerImage: rowBytes,
                sourceSize: MTLSizeMake(count, 1, 1),
                to: history,
                destinationSlice: 0,
                destinationLevel: 0,
                destinationOrigin: MTLOriginMake(0, Int(thisWriteRow), 0)
            )
            blit.endEncoding()
        }

        // --- Render encoder for the two visual passes
        guard let enc = cmd.makeRenderCommandEncoder(descriptor: passDesc) else {
            return
        }

        // Drawable size in pixels. `drawableSize` accounts for
        // retina scale already. Convert the 30/70 split to
        // integer viewport coords.
        let w = drawableSize.width
        let h = drawableSize.height
        let spectrumPx = (h * Self.spectrumFraction).rounded()
        let waterfallPx = h - spectrumPx

        // --- Waterfall pass: bottom viewport, textured quad
        enc.label = "waterfall_pass"
        enc.setViewport(MTLViewport(
            originX: 0,
            originY: Double(spectrumPx),
            width: Double(w),
            height: Double(waterfallPx),
            znear: 0,
            zfar: 1
        ))
        enc.setRenderPipelineState(waterfallPipeline)
        enc.setFragmentTexture(history, index: 0)
        enc.setFragmentTexture(paletteTexture, index: 1)
        withUnsafePointer(to: &uniforms) { ptr in
            enc.setFragmentBytes(ptr, length: MemoryLayout<RendererUniforms>.stride, index: 0)
        }
        enc.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)

        // --- Spectrum pass: top viewport, line strip
        enc.pushDebugGroup("spectrum_pass")
        enc.setViewport(MTLViewport(
            originX: 0,
            originY: 0,
            width: Double(w),
            height: Double(spectrumPx),
            znear: 0,
            zfar: 1
        ))
        enc.setRenderPipelineState(spectrumPipeline)
        enc.setVertexBuffer(spectrumVertexBuffer, offset: 0, index: 0)
        withUnsafePointer(to: &uniforms) { ptr in
            enc.setVertexBytes(ptr, length: MemoryLayout<RendererUniforms>.stride, index: 1)
        }
        enc.drawPrimitives(type: .lineStrip, vertexStart: 0, vertexCount: count)
        enc.popDebugGroup()

        enc.endEncoding()
        // `presentsWithTransaction = true` flow:
        //   1. Commit the command buffer so Metal schedules
        //      its GPU work.
        //   2. Wait until Metal has SCHEDULED (not finished)
        //      the buffer — guarantees the drawable will be
        //      ready by the time we present.
        //   3. Call drawable.present() directly. This puts
        //      the present INSIDE the current CoreAnimation
        //      transaction, which the window server commits
        //      atomically with the rest of the window update.
        //      No more tear between our render and the
        //      compositor's view of the window.
        cmd.commit()
        cmd.waitUntilScheduled()
        drawable.present()
    }

    /// Print the measured frame rate roughly once per second,
    /// so "feels choppy" bugs can be diagnosed as "display link
    /// isn't firing evenly" vs "renderer is slow" vs "feels
    /// slow but is actually fine". Released-build optimization
    /// removes the hot-path cost; `print` only fires every
    /// ~60 frames on 60 Hz, which the OS buffers cheaply.
    private func logFrameRateIfNeeded() {
        let now = CACurrentMediaTime()
        framesSinceLog += 1
        if lastFrameRateLog == 0 {
            lastFrameRateLog = now
            return
        }
        let elapsed = now - lastFrameRateLog
        if elapsed >= 1.0 {
            let fps = Double(framesSinceLog) / elapsed
            // `.notice` level so the line survives the unified
            // log system's default filtering (`.info`/`.debug`
            // get dropped from `log show` without a custom
            // profile).
            renderLog.notice("""
                fps: \(fps, format: .fixed(precision: 1)) \
                drawable: \(self.drawableSize.width, format: .fixed(precision: 0))×\(self.drawableSize.height, format: .fixed(precision: 0)) \
                bounds: \(self.bounds.width, format: .fixed(precision: 0))×\(self.bounds.height, format: .fixed(precision: 0))
                """)
            // Also put the live FPS in the window title so the
            // user can read it even in fullscreen via Mission
            // Control. Cheap: one string replacement per
            // second.
            if let window = self.window {
                window.title = String(format: "sdr-rs — %.0f fps", fps)
            }
            framesSinceLog = 0
            lastFrameRateLog = now
        }
    }

    // ----------------------------------------------------------
    //  History texture lifecycle
    // ----------------------------------------------------------

    /// Return a history texture sized for the current FFT bin
    /// count, creating / recreating it if the bin count has
    /// changed since the last frame. Recreation:
    ///   - clears `writeRow` back to 0 so the new ring fills
    ///     from the top (one blank frame is acceptable; the
    ///     user just changed FFT size, they expect a brief
    ///     transient).
    ///   - leaves the texture initialised to zeros, which
    ///     maps through the palette to the "coldest" color.
    private func ensureHistoryTexture(binCount: Int) -> MTLTexture? {
        if let existing = historyTexture, historyBinCount == binCount {
            return existing
        }
        guard let device else { return nil }

        let desc = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .r32Float,
            width: binCount,
            height: Int(Self.historyRows),
            mipmapped: false
        )
        // `.private` keeps the texture GPU-owned. Writes go
        // through the blit encoder in `draw()` — implicitly
        // ordered with any render pass reads in the same
        // command buffer, eliminating the CPU-vs-GPU race we
        // hit at 120 fps with the old `.shared` +
        // `MTLTexture.replace()` approach.
        desc.storageMode = .private
        desc.usage = [.shaderRead]
        guard let tex = device.makeTexture(descriptor: desc) else {
            return nil
        }
        tex.label = "waterfall_history_ring"

        historyTexture = tex
        historyBinCount = binCount
        writeRow = 0
        return tex
    }
}
