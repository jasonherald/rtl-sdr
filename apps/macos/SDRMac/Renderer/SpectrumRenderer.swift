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
import SdrCoreKit

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
    private let spectrumFillPipeline: MTLRenderPipelineState
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
    //  Data source
    // ----------------------------------------------------------

    /// Late-bound provider for the engine handle. The view sets
    /// this when it's constructed (or whenever `CoreModel.core`
    /// changes) so the renderer can pull FFT frames without
    /// taking a strong reference to `CoreModel`.
    ///
    /// Read on the display-link thread; written on the main
    /// thread. The display link is on the main runloop, so
    /// reads/writes don't interleave across threads here.
    var coreProvider: (() -> SdrCore?)?

    /// Floor value the spectrum line and waterfall history are
    /// initialised to before any real frame arrives. Picked well
    /// below typical `min_db` UI settings (-100 is the default)
    /// so an empty renderer maps through the palette to the
    /// coldest color, not the hottest.
    private static let floorDb: Float = -120

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
              let fillVert = library.makeFunction(name: "spectrum_fill_vert"),
              let fillFrag = library.makeFunction(name: "spectrum_fill_frag"),
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

        // Fill pipeline shares the vertex buffer with the line
        // but emits 2 vertices per bin (top + baseline). Alpha
        // blending ON so the envelope is translucent — matches
        // the GTK UI's FILL_COLOR alpha of 0.35.
        let fillDesc = MTLRenderPipelineDescriptor()
        fillDesc.label = "spectrum_fill"
        fillDesc.vertexFunction = fillVert
        fillDesc.fragmentFunction = fillFrag
        fillDesc.colorAttachments[0].pixelFormat = .bgra8Unorm
        fillDesc.colorAttachments[0].isBlendingEnabled = true
        fillDesc.colorAttachments[0].rgbBlendOperation = .add
        fillDesc.colorAttachments[0].alphaBlendOperation = .add
        fillDesc.colorAttachments[0].sourceRGBBlendFactor = .sourceAlpha
        fillDesc.colorAttachments[0].sourceAlphaBlendFactor = .sourceAlpha
        fillDesc.colorAttachments[0].destinationRGBBlendFactor = .oneMinusSourceAlpha
        fillDesc.colorAttachments[0].destinationAlphaBlendFactor = .oneMinusSourceAlpha
        guard let fillPipeline = try? device.makeRenderPipelineState(descriptor: fillDesc) else {
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
            spectrumFillPipeline: fillPipeline,
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
        spectrumFillPipeline: MTLRenderPipelineState,
        waterfallPipeline: MTLRenderPipelineState,
        vertexBuffer: MTLBuffer,
        stagingBuffer: MTLBuffer,
        paletteTexture: MTLTexture
    ) {
        self.device = device
        self.commandQueue = queue
        self.spectrumPipeline = spectrumPipeline
        self.spectrumFillPipeline = spectrumFillPipeline
        self.waterfallPipeline = waterfallPipeline
        self.spectrumVertexBuffer = vertexBuffer
        self.historyStagingBuffer = stagingBuffer
        self.paletteTexture = paletteTexture

        // Initialise the spectrum vertex buffer to the floor
        // value so a view with no FFT data yet renders a flat
        // line at the bottom of the plot rather than showing
        // whatever garbage Metal handed us.
        let floorPtr = spectrumVertexBuffer.contents().bindMemory(
            to: Float.self, capacity: Self.maxFftBins)
        for i in 0..<Self.maxFftBins {
            floorPtr[i] = Self.floorDb
        }
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

        // Pull the latest FFT frame from the engine. Returns
        // false on any of: no engine yet, engine idle, no new
        // frame since last pull. On false we DON'T update the
        // history ring — the display link is fixed-cadence but
        // FFT data is engine-cadence, so when a new render
        // tick arrives without new data, we just re-present
        // the existing texture state.
        var newFrameBinCount: Int = 0
        let hasNewFrame = coreProvider?()?.withLatestFftFrame {
            [self] buf, _, _ in
            let count = min(buf.count, Self.maxFftBins)
            newFrameBinCount = count
            let rowBytes = count * MemoryLayout<Float>.stride
            guard let src = buf.baseAddress else { return }
            memcpy(spectrumVertexBuffer.contents(), src, rowBytes)
            memcpy(historyStagingBuffer.contents(), src, rowBytes)
        } ?? false

        // Ensure the history ring exists. Its width tracks the
        // FFT bin count — needs recreation on size change. Use
        // the new frame's count when available, else keep the
        // current size (or skip if we've never seen a frame).
        let historyWidth: Int
        if hasNewFrame {
            historyWidth = newFrameBinCount
        } else if historyBinCount > 0 {
            historyWidth = historyBinCount
        } else {
            // No frame ever, no texture yet — nothing to render
            // but the clear color. Still need to present so the
            // display link doesn't back up.
            renderClearFrame(into: drawable)
            return
        }

        guard let history = ensureHistoryTexture(binCount: historyWidth) else {
            inflightSemaphore.signal()
            return
        }

        uniforms.historyRows = Self.historyRows

        // Advance the ring cursor ONLY when we actually wrote a
        // new row. Otherwise the waterfall would scroll through
        // garbage/stale rows at display-link cadence even with
        // no new data.
        let thisWriteRow = writeRow
        if hasNewFrame {
            uniforms.binCount = UInt32(newFrameBinCount)
            uniforms.writeRow = writeRow
            writeRow = (writeRow + 1) % Self.historyRows
        }

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
        //
        // Only write a new row when we actually have new FFT
        // data this tick. Re-blitting stale data at display-link
        // cadence would still be correct (same bits going to the
        // same row), but there's no need — we already skipped
        // advancing `writeRow` above.
        if hasNewFrame, let blit = cmd.makeBlitCommandEncoder() {
            let rowBytes = newFrameBinCount * MemoryLayout<Float>.stride
            blit.label = "history_row_blit"
            blit.copy(
                from: historyStagingBuffer,
                sourceOffset: 0,
                sourceBytesPerRow: rowBytes,
                sourceBytesPerImage: rowBytes,
                sourceSize: MTLSizeMake(newFrameBinCount, 1, 1),
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
        passDesc.colorAttachments[0].clearColor = MTLClearColor(red: 0.08, green: 0.08, blue: 0.10, alpha: 1.0)

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
        let binCount = Int(uniforms.binCount)
        enc.setVertexBuffer(spectrumVertexBuffer, offset: 0, index: 0)
        withUnsafePointer(to: &uniforms) { ptr in
            enc.setVertexBytes(ptr, length: MemoryLayout<RendererUniforms>.stride, index: 1)
        }
        // Fill first (envelope under the trace), then stroke the
        // line on top — same layering as the GTK UI's
        // `fft_plot.rs` Cairo draw order.
        if binCount >= 2 {
            enc.setRenderPipelineState(spectrumFillPipeline)
            enc.drawPrimitives(
                type: .triangleStrip,
                vertexStart: 0,
                vertexCount: binCount * 2
            )
        }
        enc.setRenderPipelineState(spectrumPipeline)
        enc.drawPrimitives(type: .lineStrip, vertexStart: 0, vertexCount: binCount)
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
        // `.renderTarget` in addition to `.shaderRead` so we can
        // one-shot clear the texture to the floor value via a
        // render-pass `loadAction = .clear`. The render path
        // writes every texel in a single encoder — no per-row
        // blit loop, no staging buffer. See `fillHistoryToFloor`.
        desc.usage = [.shaderRead, .renderTarget]
        guard let tex = device.makeTexture(descriptor: desc) else {
            return nil
        }
        tex.label = "waterfall_history_ring"

        // New `.private` textures contain undefined content.
        // Sampling garbage floats through the palette gives
        // arbitrary colors (sometimes mostly-hot, which looks
        // alarming). Fill the whole ring with the floor value
        // so a pre-data view renders as the coldest palette
        // entry throughout.
        fillHistoryToFloor(tex)

        historyTexture = tex
        historyBinCount = binCount
        writeRow = 0
        return tex
    }

    /// One-shot clear of the whole history texture to the floor
    /// dB value. Issues a single render pass with `loadAction =
    /// .clear` — Metal writes every texel via the GPU's fast
    /// clear hardware path, no extra encoder / staging buffer /
    /// blit-loop required. Called at texture creation; not part
    /// of the per-frame hot path.
    ///
    /// Uses `MTLClearColor.red` as the r32Float write value —
    /// for a single-channel r32Float target, only `.red` is
    /// consumed. The other components are ignored by the
    /// hardware but must be present in the struct.
    private func fillHistoryToFloor(_ texture: MTLTexture) {
        let passDesc = MTLRenderPassDescriptor()
        passDesc.colorAttachments[0].texture = texture
        passDesc.colorAttachments[0].loadAction = .clear
        passDesc.colorAttachments[0].storeAction = .store
        passDesc.colorAttachments[0].clearColor = MTLClearColor(
            red: Double(Self.floorDb),
            green: 0, blue: 0, alpha: 0
        )

        guard let cmd = commandQueue.makeCommandBuffer(),
              let enc = cmd.makeRenderCommandEncoder(descriptor: passDesc) else {
            return
        }
        enc.label = "history_floor_init"
        // Zero draw calls — the clear happens at loadAction time.
        enc.endEncoding()
        cmd.commit()
        // Don't wait — the next render-pass command buffer will
        // naturally order after this via Metal's scheduling
        // guarantees on the same queue.
    }

    /// Render a blank clear-color frame. Used before the first
    /// FFT frame arrives so the display link has something to
    /// present and doesn't queue a backlog waiting for data.
    private func renderClearFrame(into drawable: CAMetalDrawable) {
        let passDesc = MTLRenderPassDescriptor()
        passDesc.colorAttachments[0].texture = drawable.texture
        passDesc.colorAttachments[0].loadAction = .clear
        passDesc.colorAttachments[0].storeAction = .store
        passDesc.colorAttachments[0].clearColor = MTLClearColor(
            red: 0.09, green: 0.09, blue: 0.11, alpha: 1.0)

        guard let cmd = commandQueue.makeCommandBuffer() else {
            inflightSemaphore.signal()
            return
        }
        cmd.label = "sdr_frame_clear"
        cmd.addCompletedHandler { [weak self] _ in
            self?.inflightSemaphore.signal()
        }

        if let enc = cmd.makeRenderCommandEncoder(descriptor: passDesc) {
            enc.label = "clear_only"
            enc.endEncoding()
        }
        cmd.present(drawable)
        cmd.commit()
    }
}
