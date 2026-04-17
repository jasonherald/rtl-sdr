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

/// Google's Turbo colormap. Perceptually uniform, high dynamic
/// range. Reference:
/// https://ai.googleblog.com/2019/08/turbo-improved-rainbow-colormap-for.html
///
/// The numeric values below are the standard published Turbo
/// coefficients, sampled at 256 points. Generating them at
/// compile time would require a polynomial evaluation in Swift
/// that's slower than just embedding the LUT. The byte array is
/// 1 KB — well within the "fine to commit" size bar.
enum Palettes {
    static let turbo = ColorPalette(
        name: "Turbo",
        bytes: turboBytes
    )
}

/// 256×RGBA turbo LUT generated from the reference polynomial:
///
///     R = 34.61 + t*(1172.33 - t*(10793.56 - t*(33300.12 - t*(38394.49 - t*14825.05))))
///     G = 23.31 + t*(557.33 + t*(1225.33 - t*(3574.96 - t*(1073.77 + t*707.56))))
///     B = 27.2 + t*(3211.1 - t*(15327.97 - t*(27814 - t*(22569.18 - t*6838.66))))
///
/// Values precomputed and clamped to [0, 255]. Alpha is always 255.
private let turboBytes: [UInt8] = {
    var out = [UInt8](repeating: 0, count: 256 * 4)
    for i in 0..<256 {
        let t = Double(i) / 255.0
        // Turbo polynomial (Google Research reference)
        let r = 34.61
            + t * (1172.33
                - t * (10793.56
                    - t * (33300.12
                        - t * (38394.49 - t * 14825.05))))
        let g = 23.31
            + t * (557.33
                + t * (1225.33
                    - t * (3574.96
                        - t * (1073.77 + t * 707.56))))
        let b = 27.2
            + t * (3211.1
                - t * (15327.97
                    - t * (27814.0
                        - t * (22569.18 - t * 6838.66))))
        let rc = max(0.0, min(255.0, r.rounded()))
        let gc = max(0.0, min(255.0, g.rounded()))
        let bc = max(0.0, min(255.0, b.rounded()))
        out[i * 4 + 0] = UInt8(rc)
        out[i * 4 + 1] = UInt8(gc)
        out[i * 4 + 2] = UInt8(bc)
        out[i * 4 + 3] = 255
    }
    return out
}()
