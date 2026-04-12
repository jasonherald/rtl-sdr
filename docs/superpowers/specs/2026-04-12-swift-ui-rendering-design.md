---
name: SwiftUI Spectrum + Waterfall Rendering — Design
description: Metal-based renderer for the FFT plot and scrolling waterfall, embedded in SwiftUI via NSViewRepresentable, fed from sdr-core's pull-based FFT buffer
type: spec
---

# SwiftUI Spectrum + Waterfall Rendering — Design

**Status:** Draft
**Date:** 2026-04-12
**Parent epic:** `2026-04-12-swift-ui-macos-epic-design.md`
**Depends on:** `2026-04-12-sdr-ffi-c-abi-design.md` (for `sdr_core_pull_fft`)
**Tracking issues:** TBD

---

## Goal

Render the live FFT plot and the scrolling waterfall at 60 fps on macOS using Metal, embedded inside SwiftUI views via `NSViewRepresentable`. Both views read from the same Rust-owned FFT buffer through `sdr_core_pull_fft`. The renderer is the perf-critical surface of the SwiftUI app — it's the only place where allocation discipline, GPU timing, and threading actually matter.

The result must:

- Sustain 60 fps with FFT size 4096 and a waterfall that holds ~1 minute of history.
- Allocate **zero** Swift heap memory per frame after warmup.
- Forward click and drag interactions to the engine as `setVfoOffset` / `setBandwidth` commands.
- Match the visual quality of the GTK spectrum/waterfall (palette, dB grid, frequency labels, VFO overlay).

## Non-Goals

- **No SwiftUI `Canvas` fallback.** `Canvas` is fine for the FFT line at small sizes but falls over on the waterfall. Two rendering paths is two paths to maintain. Metal handles both.
- **No SceneKit / RealityKit / SpriteKit.** Way too heavy.
- **No CoreImage filter pipeline for the waterfall.** CIImage is convenient but its allocation profile is hostile to a 60 fps audio-driven loop.
- **No 3D waterfall, no perspective tilt.** Flat 2D, like SDR++ and the GTK UI.
- **No OpenGL fallback.** macOS 26 still ships GL but it's deprecated; we don't support 10.x; everything we target has Metal.
- **No GPU compute for the FFT.** The CPU FFT in `sdr-dsp` (rustfft) is fast enough and we want one source of truth. The renderer only consumes magnitude bins.
- **No multi-VFO rendering.** Single VFO overlay, matching the engine.

## Background

### How the GTK side does it

`crates/sdr-ui/src/spectrum/` uses GTK's drawing area with cairo. Per frame: pull the latest FFT bins from `SharedFftBuffer`, draw the line plot, scroll the waterfall image up by one row, draw the new row at the bottom. Custom palette LUT in software. Works fine on Linux because GTK's drawing area double-buffers and cairo is reasonably fast for this scale, but it's CPU-bound and pegs a core at high FFT sizes.

### Why Metal

- The waterfall is fundamentally a texture that scrolls. Metal renders that essentially for free with a UV offset.
- The FFT line is a strip of triangles or a `MTLLineStrip` — also essentially free.
- The palette is a 1D texture lookup in a fragment shader. One sample per pixel, no CPU work.
- `MTKView` manages drawable lifecycle, vsync sync, and resize handling. We don't write any of that.
- Profiling tools (Xcode → Instruments → Metal System Trace) are first-class.

### How the SwiftUI app gets a Metal view

`MTKView` is `NSView`. SwiftUI doesn't host `NSView` natively, but `NSViewRepresentable` does. We wrap `MTKView` once and use it from any SwiftUI view that wants spectrum:

```swift
struct SpectrumWaterfallView: NSViewRepresentable {
    let core: SdrCore
    @Binding var minDb: Float
    @Binding var maxDb: Float
    @Binding var vfoOffsetHz: Double
    @Binding var bandwidthHz: Double

    func makeNSView(context: Context) -> SpectrumMTKView { ... }
    func updateNSView(_ view: SpectrumMTKView, context: Context) { ... }
}
```

`SpectrumMTKView` is the actual `MTKView` subclass that owns the Metal resources. SwiftUI re-runs `updateNSView` whenever a binding changes; the underlying view persists across re-runs.

## High-Level Architecture

```text
SwiftUI body
  └── SpectrumWaterfallView (NSViewRepresentable)
       └── SpectrumMTKView : MTKView
            ├── MTLDevice, MTLCommandQueue                       (created once)
            ├── MTLRenderPipelineState × 3                       (created once)
            │     • spectrumLine.metal      — strip plot
            │     • waterfallScroll.metal   — full-screen quad sampling history texture
            │     • vfoOverlay.metal        — colored rectangle
            ├── MTLTexture: history (Format: r32Float, size W × H_HISTORY)
            ├── MTLTexture: palette (Format: rgba8Unorm, size 256 × 1)
            ├── MTLBuffer:  spectrumVerts (preallocated, size = max FFT bins × stride)
            ├── MTLBuffer:  uniforms (min/max dB, VFO bounds, palette index, scroll offset)
            └── draw(in:)
                 1. core.withLatestFftFrame { bins, sr, freq in
                       fillSpectrumVerts(bins)
                       writeNextHistoryRow(bins)
                       updateUniforms(...)
                    }
                 2. encode passes:
                       a. waterfall full-screen quad
                       b. spectrum line strip
                       c. VFO overlay rectangle
                 3. presentDrawable + commit
```

The display link is `MTKView`'s built-in vsync. We do **not** use `CVDisplayLink` directly — `MTKView` configured with `preferredFramesPerSecond = 60` and `enableSetNeedsDisplay = false` calls `draw(in:)` on the main thread at vsync.

## Texture Strategy: The Waterfall Ring

The waterfall shows the last *N* FFT frames as a heatmap. The naive approach (memmove the texture up by one row each frame) is what cairo does on the GTK side and it scales poorly. Metal makes it trivial to do correctly:

- Allocate **one** `MTLTexture` of size `(fftBins × historyRows)` in `r32Float`. Each texel is one dB value (or one pre-normalized 0..1 value).
- Treat the texture as a **ring buffer along the rows axis.** Maintain a `writeRow` cursor that advances each frame, wrapping at `historyRows`.
- Write the new FFT frame into row `writeRow` via `texture.replace(region:mipmapLevel:withBytes:bytesPerRow:)`. **One row per frame**, ~16 KB at 4096 bins. Negligible.
- Increment `writeRow = (writeRow + 1) % historyRows`. Pass `writeRow` to the shader as a uniform.
- The fragment shader samples `history` at `(uv.x, fract((uv.y * historyRows + writeRow) / historyRows))` so the visible scroll appears continuous even though the texture itself hasn't moved. **Zero memmove. Zero reallocation. One texture write per frame.**

The fragment shader then maps the sampled dB value through the palette LUT:

```metal
fragment float4 waterfall_frag(VertexOut in [[stage_in]],
                               texture2d<float> history [[texture(0)]],
                               texture2d<float> palette [[texture(1)]],
                               constant Uniforms& u    [[buffer(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float wrapped_y = fract((in.uv.y * u.history_rows + u.write_row) / u.history_rows);
    float db = history.sample(s, float2(in.uv.x, wrapped_y)).r;
    float t = saturate((db - u.min_db) / (u.max_db - u.min_db));
    return palette.sample(s, float2(t, 0.5));
}
```

The vertex shader is a standard fullscreen-quad. UV coordinates are passed through; nothing fancy.

### Sizing

- `fftBins` = the current FFT size (1024 / 2048 / 4096 / 8192). When the user changes FFT size, the texture is recreated. This is rare (user-initiated, not per-frame).
- `historyRows` = a fixed 1024. At 20 fps that's ~50 seconds of history; at 60 fps it's ~17 seconds. Both feel right for the use case.

## Spectrum Line Pipeline

Two reasonable approaches:

1. **`MTLLineStrip` primitive.** Each FFT bin = one vertex. Vertex shader maps `(binIndex, db)` to NDC using uniforms (min/max dB, bin count). No fragment work beyond color. ~4 KB of vertex data per frame at 4096 bins. **This is the choice.**
2. **Triangle strip "filled area" plot** (a la SDR++'s shaded spectrum). Optional v2 visual: same vertex data, doubled, with the bottom row at `min_db`. We start with the line and add the fill in v2 if users want it.

```metal
vertex VertexOut spectrum_vert(uint vid [[vertex_id]],
                               constant float* mags_db [[buffer(0)]],
                               constant Uniforms& u    [[buffer(1)]]) {
    float x = 2.0 * (float(vid) / float(u.bin_count - 1)) - 1.0;
    float y = 2.0 * saturate((mags_db[vid] - u.min_db) / (u.max_db - u.min_db)) - 1.0;
    VertexOut out;
    out.position = float4(x, y, 0.0, 1.0);
    return out;
}
```

The buffer holding `mags_db` is **pre-allocated once** at the maximum supported FFT size. Each frame we copy the current bins into the prefix and tell `drawPrimitives` how many vertices to use. No allocation, no resize.

## Frame Path (per frame, on the main thread)

```text
draw(in: MTKView):

  1. did_pull = core.withLatestFftFrame { bins, sample_rate, center_freq in
         memcpy bins → spectrumVerts.contents()
         texture.replace(region: row(writeRow), bytes: bins, ...)
         writeRow = (writeRow + 1) % historyRows
         lastSampleRate = sample_rate
         lastCenterFreq = center_freq
     }
     // If did_pull == false, we're rendering the same data as last frame.
     // The waterfall scroll offset is NOT advanced — we don't fake history.

  2. uniforms.minDb         = bindings.minDb
     uniforms.maxDb         = bindings.maxDb
     uniforms.binCount      = currentFftSize
     uniforms.historyRows   = HISTORY_ROWS
     uniforms.writeRow      = writeRow
     uniforms.vfoCenterNdc  = ... derived from vfoOffsetHz, sampleRate
     uniforms.vfoWidthNdc   = ... derived from bandwidthHz

  3. cmd = commandQueue.makeCommandBuffer()
     enc = cmd.makeRenderCommandEncoder(descriptor: drawable.passDescriptor)

     enc.setRenderPipelineState(waterfallPipeline)
     enc.setFragmentTexture(historyTexture, index: 0)
     enc.setFragmentTexture(paletteTexture, index: 1)
     enc.setFragmentBytes(&uniforms, length: ..., index: 0)
     enc.drawPrimitives(.triangleStrip, vertexStart: 0, vertexCount: 4)

     enc.setRenderPipelineState(spectrumPipeline)
     enc.setVertexBuffer(spectrumVertsBuffer, offset: 0, index: 0)
     enc.setVertexBytes(&uniforms, length: ..., index: 1)
     enc.drawPrimitives(.lineStrip, vertexStart: 0, vertexCount: currentFftSize)

     enc.setRenderPipelineState(vfoOverlayPipeline)
     enc.setFragmentBytes(&uniforms, length: ..., index: 0)
     enc.drawPrimitives(.triangleStrip, vertexStart: 0, vertexCount: 4)

     enc.endEncoding()
     cmd.present(drawable)
     cmd.commit()
```

Allocations on the hot path: **zero**. Buffers, textures, pipeline states, and uniform structs all live for the lifetime of the view.

## Layout: Two Views or One?

The GTK app uses a `GtkPaned` to split spectrum (top) and waterfall (bottom). Two options for SwiftUI:

1. **Two separate `MTKView`s** in a `VStack` with a divider, each rendering its own thing.
2. **One `MTKView` with a viewport split**, drawing spectrum into the top portion and waterfall into the bottom.

**Choice: option 2.** One Metal pipeline, one drawable per frame, one uniforms upload, one FFT pull. Two views would mean two `draw(in:)` callbacks, two pulls (only one of which gets a fresh frame), and a desync hazard between the two halves. The split is a viewport calculation, not separate widgets.

The user-draggable divider is implemented in SwiftUI via a `GeometryReader` + `gesture(DragGesture())` that updates a `@State splitFraction: CGFloat`, which is passed into the renderer as a uniform. The Metal side just uses two viewports per frame.

```swift
// SpectrumWaterfallView.swift
GeometryReader { geo in
    SpectrumWaterfallMetalView(core: core, splitY: $splitFraction, ...)
        .gesture(
            DragGesture()
                .onChanged { drag in
                    splitFraction = clamp((drag.location.y / geo.size.height), 0.1, 0.9)
                }
        )
}
```

## Interaction: Click-to-Tune & VFO Drag

Mouse events arrive as `NSEvent` on the `MTKView`. We override:

- `mouseDown(with:)` — convert click X to a frequency offset relative to center, send `core.setVfoOffset(offset)` (and `core.tune(...)` if click is in the spectrum/waterfall area but Cmd is held → re-center). Same model as the GTK UI's click handler.
- `mouseDragged(with:)` — if the drag started inside the VFO bandwidth band, update bandwidth (`core.setBandwidth(...)`); if outside, drag the VFO center.
- `magnify(with:)` — pinch zoom on the spectrum X-axis (v2 nicety).
- `scrollWheel(with:)` — scroll the waterfall through history (v2). For v1 we ignore.

The hit test for "is this click on the VFO band" uses the same uniforms the shader uses, so they can never disagree.

Coordinates come in `NSView` space. We convert with `convert(_:from:nil)` and divide by `bounds.width` to normalize. Center frequency and effective sample rate are kept on the SwiftUI side (received via `SDR_EVT_SAMPLE_RATE_CHANGED` and tune commands), so we don't have to read them back from the engine.

## Frequency Scale & dB Grid Overlay

These are static-ish text and lines. Two options:

1. **Draw them in Metal too**, using a glyph atlas. Highest perf, most code.
2. **Overlay a SwiftUI `Canvas`** above the `MTKView` in a `ZStack`. SwiftUI handles text rendering (which we don't want to reimplement). The grid lines and labels update only when range/center frequency changes, not per frame.

**Choice: option 2.** Text rendering in Metal is a deep hole and the labels don't change every frame. A SwiftUI overlay is one view and uses macOS-standard typography. Cost: a small GPU compositing step per frame, but SwiftUI's `Canvas` only re-rasterizes when its inputs change, so the steady-state cost is just a layer composite.

```swift
ZStack(alignment: .topLeading) {
    SpectrumWaterfallView(core: core, ...)         // MTKView wrapper
    FrequencyScaleOverlay(centerHz: ..., spanHz: ..., minDb: ..., maxDb: ...)
        .allowsHitTesting(false)                   // mouse falls through to Metal
}
```

## Performance Budget

Target: **60 fps at FFT size 4096**, on Apple Silicon and Intel.

Per-frame budget (16.6 ms):
- FFT pull + memcpy bins (4096 × 4 = 16 KB)            : ~10 µs
- Texture row replace                                  : ~50 µs
- Uniform struct fill                                  : <1 µs
- 3 GPU passes (waterfall quad, spectrum strip, VFO)   : <1 ms on M1, <3 ms on Intel UHD 630
- SwiftUI frequency scale overlay (when changed)       : <2 ms
- **Total CPU**                                        : ~3 ms
- **Total GPU**                                        : ~3-5 ms
- Headroom                                             : 8-10 ms

This budget assumes the dispatcher thread is **not** the renderer's bottleneck. The FFT pull is lock-free when nothing's new and a short-mutex when something is. The renderer is allowed to draw the same frame twice if no new data arrived; we don't block.

When FFT rate < display rate (e.g., 20 fps engine, 60 fps display), the renderer naturally interpolates by drawing the same data 3 times. We don't temporal-interpolate the spectrum or the waterfall — that would lie about the data.

## Resize Handling

`MTKView` calls `mtkView(_:drawableSizeWillChange:)` when the window resizes. We don't recreate any textures or pipelines on resize — only the drawable size changes, and the vertex shader uses NDC, not pixels. The frequency scale overlay re-rasterizes naturally because its frame changes.

## Threading

All Metal calls happen on the main thread inside `draw(in:)`. The FFT pull is from the main thread. The renderer never touches engine state from any other thread. This matches SwiftUI's `MainActor` model and CoreGraphics' threading rules.

The dispatcher thread (owned by `sdr-ffi`) hands events to SwiftUI via the `AsyncStream`; SwiftUI consumers `for await`-loop on `MainActor` and update bindings. Bindings drive the renderer's uniforms on the next frame. There is exactly one mutation point for any rendering input.

## Test Strategy

- **Unit tests** for the math: bin index → frequency, click X → VFO offset, dB → palette index. These run in `SdrCoreKitTests` without any rendering.
- **Snapshot tests** of the renderer using `MTKView`'s `currentDrawable.texture` after one `draw(in:)` call, comparing against a checked-in PNG. Useful for catching shader regressions. macOS-only test target.
- **Performance test** using `XCTMetric.gpu` and `XCTMetric.cpu`: spin the renderer for 5 seconds with a synthetic FFT source (a fixed bin pattern fed via `sdr_core_pull_fft`'s test mode), assert frame-time stays under 16 ms.
- **Manual perf gate**: Instruments → Metal System Trace → no frames over 16 ms, no main-thread allocations after warmup. Captured before merging M4.

## Risks

| Risk | Mitigation |
|------|------------|
| `MTKView` inside `NSViewRepresentable` re-creates on every SwiftUI re-render | `makeNSView` runs once; `updateNSView` only mutates. The renderer's expensive setup is in `init`, called from `makeNSView`. Verified by adding a counter and asserting it stays at 1 per view lifetime. |
| Click-to-tune feels laggy because of FFT-rate / display-rate mismatch | The click handler sends commands directly to `core` — independent of FFT rate. Visual feedback on the spectrum follows on the next FFT frame. Same UX as SDR++. |
| Palette LUT is hardcoded in code, hard to swap | Ship 4 palettes (turbo, viridis, magma, classic) as compile-time `[UInt8]` arrays. Settings UI swaps which one is uploaded. v2 — for v1 just use turbo. |
| Texture row replace is a GPU stall on Intel discrete GPUs | Rare on macOS hardware (almost everything is integrated). If it bites, switch to a tripled-up `MTLBuffer` and write via blit encoder. v2 if needed. |
| User changes FFT size mid-stream — texture must reallocate without flicker | On size change: lazily recreate `historyTexture` on the next `draw(in:)`, fill with `min_db` baseline so the new history fades in. The user perceives one blank frame; acceptable. |
| SwiftUI animation interferes with `MTKView`'s vsync | Set `view.layer?.actions = [:]` and `disableAnimations` on the host view to suppress implicit Core Animation transitions. Standard `MTKView` boilerplate. |

## Open Questions

- **`enableSetNeedsDisplay = false` (continuous vsync) vs. true (manual invalidate):** continuous gives us a steady 60 fps tick which simplifies the "draw same frame twice if no new data" path. Manual would save a few percent of GPU when nothing's happening. **Lean: continuous in v1**, revisit if battery profiling on a MacBook is bad.
- **MetalKit vs. raw Metal:** `MTKView` is in MetalKit; nothing else from MetalKit is needed. Adds one framework link, no runtime cost. **Lean: MTKView**.
- **Should the SwiftUI app expose a "Renderer FPS" debug overlay?** Useful during development, ugly in shipping. **Lean: yes, behind a hidden settings toggle that defaults off.**
- **Should the renderer draw a max-hold trace** (peak hold across all FFT frames)? **No for v1**; this is a Display Panel feature and the engine already supports it via `SetAveragingMode`. v2 surfaces it.

## Implementation Sequencing

This is M4 in the epic. It can start as soon as the FFI surface (M2) is callable from Swift, even before the full SwiftUI app shell exists. M4 produces a standalone test app that just renders a Metal spectrum from a synthetic FFT source, so the renderer is validated in isolation before being dropped into the real app shell in M5.

Sub-PRs:
1. **Renderer skeleton + spectrum line only**, fed by a synthetic source. No waterfall, no overlay.
2. **Waterfall texture ring + scroll shader**, same synthetic source.
3. **VFO overlay + click-to-tune + drag**, wired into a real `SdrCore` instance.
4. **Frequency scale overlay (SwiftUI Canvas)** and dB grid.

## References

- [Apple — `MTKView` reference](https://developer.apple.com/documentation/metalkit/mtkview)
- [Apple — Drawing Geometry with Metal](https://developer.apple.com/documentation/metal/using_metal_to_draw_a_view_s_contents)
- `2026-04-12-sdr-ffi-c-abi-design.md` — `sdr_core_pull_fft` and `SdrFftFrame` it consumes
- `crates/sdr-ui/src/spectrum/` — GTK reference implementation (cairo-based) for visual parity
