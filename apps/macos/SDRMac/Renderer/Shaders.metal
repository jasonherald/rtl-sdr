//
// Shaders.metal — Metal Shading Language source for the
// spectrum line renderer. Xcode compiles this into
// `default.metallib` at build time; `SpectrumMTKView` loads the
// functions via `device.makeDefaultLibrary()`.
//
// Only the spectrum-line pipeline is in this file for sub-PR 1.
// The waterfall full-screen-quad shader and the VFO overlay
// shader land in follow-up sub-PRs (M4/2 and M4/3), reusing the
// same `Uniforms` struct.

#include <metal_stdlib>
using namespace metal;

// Uniform data shared by all pipelines. Matches the layout of
// `RendererUniforms` in Swift (see `SpectrumMTKView.swift`).
// Keep fields aligned on 4-byte boundaries and ordered by size
// descending so Swift's default struct alignment matches MSL's
// — no explicit `alignas` needed on either side for this size.
struct Uniforms {
    float min_db;         // bottom of visible dB range
    float max_db;         // top of visible dB range
    uint  bin_count;      // current FFT bin count
    uint  history_rows;   // waterfall texture height (sub-PR 2)
    uint  write_row;      // waterfall ring cursor (sub-PR 2)
    uint  _pad0;          // keep 8-byte multiple for Swift interop
};

// --------------------------------------------------------------
//  Spectrum line
// --------------------------------------------------------------

struct SpectrumVertexOut {
    float4 position [[position]];
    float  intensity;  // normalized 0..1 along the dB axis; the
                       // fragment uses it for a subtle vertical
                       // gradient so the strongest signals read
                       // brighter than the noise floor.
};

/// Spectrum vertex shader. One invocation per FFT bin; each
/// bin maps to (x = bin_index/binCount, y = normalized dB).
///
/// Coordinate system: clip space, [-1, 1] × [-1, 1]. The
/// Swift side sets a viewport that limits the spectrum to the
/// top portion of the view (sub-PR 2 splits top/bottom), so
/// the full NDC Y range maps to just the spectrum area.
vertex SpectrumVertexOut spectrum_vert(
    uint                     vid      [[vertex_id]],
    constant float          *mags_db  [[buffer(0)]],
    constant Uniforms&       u        [[buffer(1)]]
) {
    // Guard against degenerate bin_count — shouldn't happen in
    // practice, but returning off-screen is safer than dividing
    // by zero.
    float denom = max(1.0, float(u.bin_count - 1));
    float x = 2.0 * (float(vid) / denom) - 1.0;

    float db = mags_db[vid];
    float t  = saturate((db - u.min_db) / max(0.001, u.max_db - u.min_db));
    float y  = 2.0 * t - 1.0;

    SpectrumVertexOut out;
    out.position  = float4(x, y, 0.0, 1.0);
    out.intensity = t;
    return out;
}

/// Spectrum fragment shader. Solid spectrum color, modulated
/// slightly by intensity so peaks pop visually. Keeping this
/// as a flat color rather than sampling the palette — the
/// palette is the waterfall's domain; the line is its own
/// high-contrast accent against whatever waterfall is underneath.
fragment float4 spectrum_frag(
    SpectrumVertexOut in [[stage_in]]
) {
    // Base: an SDR green-on-dark aesthetic that reads well on
    // both light and dark system appearance, with just enough
    // saturation to pop against the turbo waterfall.
    float3 base   = float3(0.55, 0.95, 0.65);
    float3 accent = float3(1.00, 1.00, 1.00);
    float3 rgb    = mix(base, accent, in.intensity * 0.3);
    return float4(rgb, 1.0);
}
