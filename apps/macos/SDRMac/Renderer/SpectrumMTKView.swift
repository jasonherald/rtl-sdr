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
// SdrCoreKit is not imported here yet — sub-PR 1 uses a
// synthetic FFT source. Sub-PR 3 adds the import + swaps
// `SyntheticFftSource` for `SdrCore.withLatestFftFrame`.

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

    // ----------------------------------------------------------
    //  Tick driver
    // ----------------------------------------------------------

    /// Invalidate timer. At 20 Hz (50 ms) to match the engine's
    /// default FFT rate. `enableSetNeedsDisplay = true` plus
    /// explicit invalidation from here gives us "draw only when
    /// there's new data" — ~3× GPU power savings vs a free-run
    /// 60 Hz vsync tick.
    private var tickTimer: Timer?

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
              let vertexFn = library.makeFunction(name: "spectrum_vert"),
              let fragmentFn = library.makeFunction(name: "spectrum_frag")
        else {
            return nil
        }

        let pipelineDesc = MTLRenderPipelineDescriptor()
        pipelineDesc.label = "spectrum_line"
        pipelineDesc.vertexFunction = vertexFn
        pipelineDesc.fragmentFunction = fragmentFn
        pipelineDesc.colorAttachments[0].pixelFormat = .bgra8Unorm
        guard let pipeline = try? device.makeRenderPipelineState(descriptor: pipelineDesc) else {
            return nil
        }

        let bufLen = Self.maxFftBins * MemoryLayout<Float>.stride
        guard let buf = device.makeBuffer(length: bufLen, options: .storageModeShared) else {
            return nil
        }
        buf.label = "spectrum_mags_db"

        return SpectrumMTKView(
            device: device,
            queue: queue,
            pipeline: pipeline,
            vertexBuffer: buf
        )
    }

    /// Designated init used by the factory. Private so callers
    /// must go through `make()` and can't bypass Metal validation.
    private init(
        device: MTLDevice,
        queue: MTLCommandQueue,
        pipeline: MTLRenderPipelineState,
        vertexBuffer: MTLBuffer
    ) {
        self.commandQueue = queue
        self.spectrumPipeline = pipeline
        self.spectrumVertexBuffer = vertexBuffer
        self.syntheticSource = SyntheticFftSource(binCount: 2048)

        super.init(frame: .zero, device: device)

        // MTKView configuration.
        self.colorPixelFormat = .bgra8Unorm
        self.clearColor = MTLClearColor(red: 0.09, green: 0.09, blue: 0.11, alpha: 1.0)
        self.framebufferOnly = true
        // Invalidate-on-demand rather than continuous vsync —
        // power-conscious default. See file header for the
        // rationale. Sub-PR 3 flips this to false when the
        // machine is plugged in.
        self.enableSetNeedsDisplay = true
        self.isPaused = true   // no automatic ticks; we invalidate

        // Start the synthetic tick driver. Runs on the main
        // run loop so the `setNeedsDisplay` call below happens
        // on the same thread as `draw(in:)`.
        let timer = Timer(timeInterval: 0.05, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.syntheticSource.next()
            self.needsDisplay = true
        }
        RunLoop.main.add(timer, forMode: .common)
        self.tickTimer = timer
    }

    @available(*, unavailable)
    required init(coder: NSCoder) {
        fatalError("SpectrumMTKView does not support NSCoder init")
    }

    deinit {
        tickTimer?.invalidate()
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

    override func draw(_ dirtyRect: NSRect) {
        // MTKView routes its draws through `draw(in:)` below
        // when `isPaused = true` and `enableSetNeedsDisplay =
        // true`. We DO need to keep this override present so
        // the subclass uses MTKView's plumbing; forwarding to
        // super is what kicks the drawable request.
        super.draw(dirtyRect)
    }

    override func draw() {
        // Copy the current synthetic bins into the vertex
        // buffer. `memcpy` is safe here — the buffer is
        // `storageModeShared` so the GPU will see the bytes
        // after this function returns.
        let bins = syntheticSource.magnitudes
        let count = min(bins.count, Self.maxFftBins)
        _ = bins.withUnsafeBufferPointer { src in
            memcpy(spectrumVertexBuffer.contents(), src.baseAddress!,
                   count * MemoryLayout<Float>.stride)
        }
        uniforms.binCount = UInt32(count)

        guard
            let drawable = currentDrawable,
            let passDesc = currentRenderPassDescriptor,
            let cmd = commandQueue.makeCommandBuffer()
        else {
            return
        }
        cmd.label = "spectrum_frame"

        guard let enc = cmd.makeRenderCommandEncoder(descriptor: passDesc) else {
            return
        }
        enc.label = "spectrum_pass"

        enc.setRenderPipelineState(spectrumPipeline)
        enc.setVertexBuffer(spectrumVertexBuffer, offset: 0, index: 0)
        withUnsafePointer(to: &uniforms) { ptr in
            enc.setVertexBytes(ptr, length: MemoryLayout<RendererUniforms>.stride, index: 1)
        }
        enc.drawPrimitives(type: .lineStrip, vertexStart: 0, vertexCount: count)

        enc.endEncoding()
        cmd.present(drawable)
        cmd.commit()
    }
}
