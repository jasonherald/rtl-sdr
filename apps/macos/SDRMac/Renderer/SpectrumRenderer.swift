//
// SpectrumRenderer.swift — pure Metal state + encode logic for
// the spectrum + waterfall panes. No view coupling.
//
// ## Why this class exists (and why it's not an MTKView subclass)
//
// The first cut of this renderer was an `MTKView` subclass that
// overrode `draw()`. That pattern works but has three problems on
// macOS 14+:
//
//   1. AppKit can invoke `NSView.draw()` from dirty-rect /
//      invalidation passes that have nothing to do with the
//      Metal display link — especially when the window is
//      focused and SwiftUI is actively layout-invalidating its
//      hosted view. That's the likely root cause of the
//      "focused-window renders broken, unfocused renders clean"
//      symptom we hit: the compositor kept re-entering our
//      draw at unexpected times.
//
//   2. `MTKView` uses `nextDrawable()` under the hood, which
//      blocks when the swap queue is contended. `CAMetalDisplayLink`
//      (macOS 14+) HANDS the drawable to us in the delegate
//      callback — no nextDrawable call, no blocking.
//
//   3. `presentsWithTransaction = true` + `waitUntilScheduled`
//      — the pattern we tried to add — is a known footgun for
//      continuous animation (see Flutter Impeller #131520). It's
//      correct for window resize but blocks the main thread
//      under compositor load, which is exactly when we were
//      seeing partial frames.
//
// The fix is to drive rendering from a `CAMetalDisplayLink` on
// a plain `NSView` with a `CAMetalLayer` backing, and keep the
// Metal state in this view-independent class. That's the shape
// of Apple's WWDC23 "What's new in AppKit" guidance and what
// modern sample code (e.g. the Metal by Example posts) uses.
//
// Testability bonus: with no view coupling, we can unit-test
// the encode pipeline against an offscreen texture if we ever
// want to.

import Metal
import OSLog
import QuartzCore
// SdrCoreKit is not imported here yet — sub-PR 1 uses a
// synthetic FFT source. Sub-PR 3 adds the import + swaps
// `SyntheticFftSource` for `SdrCore.withLatestFftFrame`.

/// Renderer-scoped logger. Visible via
/// `log stream --predicate 'subsystem == "com.sdr.rs" AND category == "renderer"'`.
private let renderLog = Logger(subsystem: "com.sdr.rs", category: "renderer")

/// Uniform block uploaded once per frame. Layout matches
/// `struct Uniforms` in `Shaders.metal` — keep field order +
/// types in sync.
struct RendererUniforms {
    var minDb: Float = -100
    var maxDb: Float = 0
    var binCount: UInt32 = 2048
    var historyRows: UInt32 = 1024
    var writeRow: UInt32 = 0
    var _pad0: UInt32 = 0
}

final class SpectrumRenderer {
    // ----------------------------------------------------------
    //  Public: what the view needs to wire up its CAMetalLayer
    // ----------------------------------------------------------

    /// The Metal device this renderer bound to at construction.
    /// The owning `CAMetalLayer` MUST use this same device.
    let device: MTLDevice

    /// Color format the renderer's pipelines are compiled for.
    /// The owning `CAMetalLayer` MUST use this exact format.
    let colorPixelFormat: MTLPixelFormat = .bgra8Unorm

    // ----------------------------------------------------------
    //  Metal resources (created once at init)
    // ----------------------------------------------------------

    private let commandQueue: MTLCommandQueue
    private let spectrumPipeline: MTLRenderPipelineState
    private let waterfallPipeline: MTLRenderPipelineState

    /// 256×1 rgba8Unorm colormap LUT sampled by the waterfall
    /// fragment shader. Built once at init from `Palettes.turbo`.
    private let paletteTexture: MTLTexture

    /// `fftBins × historyRows` r32Float ring texture. One row
    /// per published FFT frame; `writeRow` advances each draw
    /// and wraps. `.storageMode = .private` so writes flow
    /// through the blit encoder (see `historyStagingBuffer`)
    /// rather than `MTLTexture.replace()`. CPU-side `.replace()`
    /// on a shared-storage texture RACES GPU reads from
    /// previously-committed-but-not-yet-completed render passes
    /// — invisible at 1 fps, obvious as "pieces and artifacts"
    /// at 120 fps on ProMotion. Blit→render in the same command
    /// buffer is sequenced by Metal itself, so no race.
    private var historyTexture: MTLTexture?

    /// Row-sized shared buffer that new FFT rows memcpy into,
    /// then blit-copied to the private history texture each
    /// frame.
    private let historyStagingBuffer: MTLBuffer

    /// Current waterfall width in bins, tracked so we can detect
    /// FFT-size changes and rebuild `historyTexture`.
    private var historyBinCount: Int = 0

    /// Ring cursor. Always in `[0, historyRows)`.
    private var writeRow: UInt32 = 0

    /// Rows of waterfall history. At 20 Hz → ~50 s; at 60 Hz →
    /// ~17 s. Matches the spec.
    static let historyRows: UInt32 = 1024

    /// Preallocated buffer holding the current frame's
    /// magnitudes in dB. Sized for the MAX supported FFT size
    /// (8192) so we never reallocate.
    private let spectrumVertexBuffer: MTLBuffer

    static let maxFftBins = 8192

    /// Fraction of the view's height devoted to the spectrum
    /// line; the rest is waterfall. Matches the GTK UI's 30/70
    /// split.
    private static let spectrumFraction: CGFloat = 0.30

    // ----------------------------------------------------------
    //  Frame pacing
    // ----------------------------------------------------------

    /// Triple-buffer semaphore. Keeps at most 3 frames in flight
    /// on the GPU so the CPU never races ahead of display-link
    /// cadence and clobbers a buffer the GPU is still reading.
    /// Matches CAMetalLayer's default `maximumDrawableCount = 3`.
    /// Signalled from the command buffer's completion handler.
    private let inflightSemaphore = DispatchSemaphore(value: 3)

    // ----------------------------------------------------------
    //  Data source (synthetic for sub-PR 1/2; real in 3)
    // ----------------------------------------------------------

    private let syntheticSource: SyntheticFftSource

    /// Uniform block. Mutated each frame before encoding.
    private var uniforms = RendererUniforms()

    // ----------------------------------------------------------
    //  Instrumentation
    // ----------------------------------------------------------

    private var lastFrameRateLog: CFTimeInterval = 0
    private var framesSinceLog: Int = 0
    /// Captured when the FPS logger runs so we can surface it in
    /// the window title without needing a reference to the view.
    private(set) var measuredFps: Double = 0

    // ----------------------------------------------------------
    //  Factory
    // ----------------------------------------------------------

    /// Build a configured renderer. Returns `nil` if Metal setup
    /// fails (no device, shader library missing, pipeline
    /// compile error). Callers render a SwiftUI fallback in that
    /// case — see `SpectrumWaterfallView`.
    static func make() -> SpectrumRenderer? {
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

        let bufLen = maxFftBins * MemoryLayout<Float>.stride
        guard let buf = device.makeBuffer(length: bufLen, options: .storageModeShared) else {
            return nil
        }
        buf.label = "spectrum_mags_db"

        guard let staging = device.makeBuffer(length: bufLen, options: .storageModeShared) else {
            return nil
        }
        staging.label = "history_row_staging"

        guard let palette = Palettes.turbo.makeTexture(device: device) else {
            return nil
        }
        palette.label = "turbo_palette_lut"

        return SpectrumRenderer(
            device: device,
            queue: queue,
            spectrumPipeline: specPipeline,
            waterfallPipeline: wfPipeline,
            vertexBuffer: buf,
            stagingBuffer: staging,
            paletteTexture: palette
        )
    }

    private init(
        device: MTLDevice,
        queue: MTLCommandQueue,
        spectrumPipeline: MTLRenderPipelineState,
        waterfallPipeline: MTLRenderPipelineState,
        vertexBuffer: MTLBuffer,
        stagingBuffer: MTLBuffer,
        paletteTexture: MTLTexture
    ) {
        self.device = device
        self.commandQueue = queue
        self.spectrumPipeline = spectrumPipeline
        self.waterfallPipeline = waterfallPipeline
        self.spectrumVertexBuffer = vertexBuffer
        self.historyStagingBuffer = stagingBuffer
        self.paletteTexture = paletteTexture
        self.syntheticSource = SyntheticFftSource(binCount: 2048)
    }

    // ----------------------------------------------------------
    //  Binding updates from SwiftUI
    // ----------------------------------------------------------

    func applyBindings(minDb: Float, maxDb: Float) {
        if minDb.isFinite { uniforms.minDb = minDb }
        if maxDb.isFinite { uniforms.maxDb = max(minDb + 1.0, maxDb) }
    }

    // ----------------------------------------------------------
    //  Encode one frame
    // ----------------------------------------------------------

    /// Encode and submit one frame into `drawable`.
    /// `drawableSize` is the drawable's pixel size — used to
    /// compute the spectrum / waterfall viewport split.
    ///
    /// Called on the main thread by the `CAMetalDisplayLink`
    /// delegate callback. Triple-buffer gated via
    /// `inflightSemaphore`: if the CPU is running ahead of the
    /// GPU, this blocks until a buffer slot frees, which is
    /// exactly the behavior you want for smooth presentation —
    /// it couples the CPU frame rate to the GPU's ability to
    /// drain the work.
    ///
    /// Note on present timing: we use the plain `present(drawable)`
    /// variant, NOT `present(drawable, atTime:)`. The atTime form
    /// schedules a future present that Metal's completion-queue
    /// thread executes via a deferred block. On macOS 14/26 with
    /// a `CAMetalDisplayLink`-provided drawable, that deferred
    /// block can throw `NSInvalidArgumentException` from inside
    /// `-[CAMetalDrawable presentWithOptions:]` — the display
    /// link already has expectations about when/how the drawable
    /// is presented, and the time-scheduled present conflicts
    /// with them. Plain `present(drawable)` presents at the next
    /// vsync that the display link is already aligned to, which
    /// is exactly what we want.
    func encode(
        into drawable: CAMetalDrawable,
        drawableSize: CGSize
    ) {
        // Gate on triple-buffer capacity first. If we're ahead
        // of the GPU, block here instead of queuing work that
        // would clobber in-flight command buffers' resources.
        _ = inflightSemaphore.wait(timeout: .distantFuture)

        logFrameRateIfNeeded()

        // Advance the synthetic source. Sub-PR 3 swaps this for
        // `SdrCore.withLatestFftFrame` which returns a bool
        // indicating whether a new frame arrived; on `false`
        // we'll still re-present but skip the memcpy + blit.
        syntheticSource.next()

        // 1. Copy current bins into the spectrum vertex buffer
        //    AND the history-row staging buffer.
        let bins = syntheticSource.magnitudes
        let count = min(bins.count, Self.maxFftBins)
        let rowBytes = count * MemoryLayout<Float>.stride
        bins.withUnsafeBufferPointer { src in
            memcpy(spectrumVertexBuffer.contents(), src.baseAddress!, rowBytes)
            memcpy(historyStagingBuffer.contents(), src.baseAddress!, rowBytes)
        }
        uniforms.binCount = UInt32(count)
        uniforms.historyRows = Self.historyRows

        // 2. Ensure the history ring exists with the right width.
        guard let history = ensureHistoryTexture(binCount: count) else {
            inflightSemaphore.signal()
            return
        }

        uniforms.writeRow = writeRow
        let thisWriteRow = writeRow
        writeRow = (writeRow + 1) % Self.historyRows

        // 3. One command buffer, two encoders.
        guard let cmd = commandQueue.makeCommandBuffer() else {
            inflightSemaphore.signal()
            return
        }
        cmd.label = "sdr_frame"

        // Signal the semaphore from the completion handler so a
        // new frame can start encoding only after the GPU
        // finishes with this one's resources. Captured weakly
        // to keep this class deallocating cleanly if the view
        // tears down mid-frame.
        cmd.addCompletedHandler { [weak self] _ in
            self?.inflightSemaphore.signal()
        }

        // --- Blit: staging buffer → history[writeRow]
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

        // --- Render encoder: waterfall + spectrum
        let passDesc = MTLRenderPassDescriptor()
        passDesc.colorAttachments[0].texture = drawable.texture
        passDesc.colorAttachments[0].loadAction = .clear
        passDesc.colorAttachments[0].storeAction = .store
        passDesc.colorAttachments[0].clearColor = MTLClearColor(red: 0.09, green: 0.09, blue: 0.11, alpha: 1.0)

        guard let enc = cmd.makeRenderCommandEncoder(descriptor: passDesc) else {
            // Semaphore will be released by the completion
            // handler we added above — no double-signal.
            cmd.commit()
            return
        }

        // Split the drawable into spectrum (top ~30%) and
        // waterfall (bottom ~70%). Use the actual drawable size
        // in pixels — CAMetalLayer's drawableSize, not the
        // view's bounds.
        let w = drawableSize.width
        let h = drawableSize.height
        let spectrumPx = (h * Self.spectrumFraction).rounded()
        let waterfallPx = h - spectrumPx

        // --- Waterfall pass: bottom viewport
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

        // --- Spectrum pass: top viewport
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

        // Present at the next vsync. The display link already
        // gave us a drawable aligned to the upcoming display
        // refresh, so plain `present` is the right call — see
        // the NSException note at the top of `encode(...)`.
        cmd.present(drawable)
        cmd.commit()
    }

    // ----------------------------------------------------------
    //  Instrumentation helpers
    // ----------------------------------------------------------

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
            measuredFps = fps
            // `.notice` level so the line survives the unified
            // log system's default filtering.
            renderLog.notice("fps: \(fps, format: .fixed(precision: 1))")
            framesSinceLog = 0
            lastFrameRateLog = now
        }
    }

    // ----------------------------------------------------------
    //  History texture lifecycle
    // ----------------------------------------------------------

    private func ensureHistoryTexture(binCount: Int) -> MTLTexture? {
        if let existing = historyTexture, historyBinCount == binCount {
            return existing
        }
        let desc = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .r32Float,
            width: binCount,
            height: Int(Self.historyRows),
            mipmapped: false
        )
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
