//
// Palettes.swift — colormap look-up tables for the waterfall
// fragment shader.
//
// Each palette is a 256×1 RGBA8 texture. The waterfall fragment
// shader computes `t = saturate((dB - minDb) / (maxDb - minDb))`
// and samples the palette at `(t, 0.5)` with linear filtering.
// One texture sample per pixel, no CPU-side dB-to-color math.
//
// Palette bytes are laid out as RGBA in ascending intensity:
// index 0 is the "cold" end (low dB), index 255 is the "hot"
// end (high dB). Alpha stays 255 throughout.
//
// Only `turbo` ships in v1 per the rendering spec. Additional
// palettes (viridis, magma, classic SDR++) land with the
// Display Settings panel in v2.

import Foundation
import Metal

/// Source of truth for the texture data of a single colormap.
struct ColorPalette {
    /// Human-readable name (for the v2 palette picker).
    let name: String
    /// 256 × RGBA = 1024 bytes of texel data, oldest-first.
    let bytes: [UInt8]

    /// Build an `MTLTexture` from the palette bytes. The texture
    /// is `private` storage on Apple Silicon (GPU-only; the
    /// blit encoder copy below is one-time at setup and doesn't
    /// hit the audio path again), 256 × 1, `rgba8Unorm`. Cheap:
    /// 1 KB texture, allocated once per MTKView lifetime.
    func makeTexture(device: MTLDevice) -> MTLTexture? {
        let desc = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .rgba8Unorm,
            width: 256,
            height: 1,
            mipmapped: false
        )
        desc.storageMode = .shared
        desc.usage = [.shaderRead]
        guard let tex = device.makeTexture(descriptor: desc) else {
            return nil
        }
        bytes.withUnsafeBufferPointer { buf in
            tex.replace(
                region: MTLRegionMake2D(0, 0, 256, 1),
                mipmapLevel: 0,
                withBytes: buf.baseAddress!,
                bytesPerRow: 256 * 4
            )
        }
        return tex
    }
}

// MARK: - Turbo

/// Turbo colormap — porting the GTK UI's 8-stop piecewise-linear
/// implementation (`crates/sdr-ui/src/spectrum/colormap.rs:43`)
/// verbatim so the Linux and macOS waterfalls read identically.
///
/// The original implementation here used the Google Research
/// reference polynomial, which produces a perceptually similar
/// curve but not pixel-identical colors — the noise floor on
/// Linux came out slightly darker and the peaks slightly cooler.
/// Matching the Linux stops is the simplest way to make side-
/// by-side screenshots look like the same product.
enum Palettes {
    static let turbo = ColorPalette(
        name: "Turbo",
        bytes: turboBytes
    )
}

/// 8-stop piecewise-linear turbo. Exact copy of `TURBO_STOPS` in
/// `crates/sdr-ui/src/spectrum/colormap.rs:43-52`.
private let turboStops: [(t: Double, r: UInt8, g: UInt8, b: UInt8)] = [
    (0.00,   0,   0,   0),
    (0.10,  10,  10,  80),
    (0.25,  20,  40, 200),
    (0.40,   0, 180, 220),
    (0.55,  20, 200,  40),
    (0.70, 240, 220,  10),
    (0.85, 240,  40,  10),
    (1.00, 255, 255, 255),
]

private let turboBytes: [UInt8] = {
    var out = [UInt8](repeating: 0, count: 256 * 4)
    for i in 0..<256 {
        let t = Double(i) / 255.0
        // Find the pair of stops `t` falls between. Linear
        // scan is fine — 8 stops, happens 256 times at init.
        var lo = turboStops[0]
        var hi = turboStops[turboStops.count - 1]
        for j in 0..<(turboStops.count - 1) where t >= turboStops[j].t && t <= turboStops[j + 1].t {
            lo = turboStops[j]
            hi = turboStops[j + 1]
            break
        }
        let span = hi.t - lo.t
        let frac = span > 0 ? (t - lo.t) / span : 0
        let r = Double(lo.r) + frac * (Double(hi.r) - Double(lo.r))
        let g = Double(lo.g) + frac * (Double(hi.g) - Double(lo.g))
        let b = Double(lo.b) + frac * (Double(hi.b) - Double(lo.b))
        out[i * 4 + 0] = UInt8(max(0.0, min(255.0, r.rounded())))
        out[i * 4 + 1] = UInt8(max(0.0, min(255.0, g.rounded())))
        out[i * 4 + 2] = UInt8(max(0.0, min(255.0, b.rounded())))
        out[i * 4 + 3] = 255
    }
    return out
}()
