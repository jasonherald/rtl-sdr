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

/// Spectrum fragment shader. Solid accent-blue trace, matching
/// the GTK UI's TRACE_COLOR
/// (`crates/sdr-ui/src/spectrum/fft_plot.rs:33`): (0.3, 0.7, 1.0).
/// Intensity modulation brightens peaks slightly so they pop
/// against the turbo waterfall underneath without drifting from
/// the Linux-side look.
fragment float4 spectrum_frag(
    SpectrumVertexOut in [[stage_in]]
) {
    float3 base   = float3(0.30, 0.70, 1.00);
    float3 accent = float3(1.00, 1.00, 1.00);
    float3 rgb    = mix(base, accent, in.intensity * 0.3);
    return float4(rgb, 1.0);
}

// --------------------------------------------------------------
//  Spectrum fill — semi-transparent envelope under the trace
//
//  Invoked with 2N vertices for N bins, rendered as a triangle
//  strip. Even vertex_id = top (at signal dB); odd = bottom (at
//  clip-space Y = -1). Pairs form quads that tile across the
//  spectrum, giving a filled envelope. Matches the GTK UI's
//  FILL_COLOR (`fft_plot.rs:35`).
// --------------------------------------------------------------

vertex SpectrumVertexOut spectrum_fill_vert(
    uint                     vid      [[vertex_id]],
    constant float          *mags_db  [[buffer(0)]],
    constant Uniforms&       u        [[buffer(1)]]
) {
    uint  bin    = vid / 2;
    bool  is_top = (vid & 1u) == 0u;

    float denom = max(1.0, float(u.bin_count - 1));
    float x = 2.0 * (float(bin) / denom) - 1.0;

    float db = mags_db[bin];
    float t  = saturate((db - u.min_db) / max(0.001, u.max_db - u.min_db));
    float y  = is_top ? (2.0 * t - 1.0) : -1.0;

    SpectrumVertexOut out;
    out.position  = float4(x, y, 0.0, 1.0);
    out.intensity = t;
    return out;
}

fragment float4 spectrum_fill_frag(
    SpectrumVertexOut in [[stage_in]]
) {
    // GTK UI FILL_COLOR (`fft_plot.rs:35`): (0.2, 0.4, 0.8, 0.35).
    return float4(0.20, 0.40, 0.80, 0.35);
}

// --------------------------------------------------------------
//  Waterfall
//
//  History is an r32Float texture of size (fftBins × historyRows).
//  One row per FFT frame, in dB. Writing advances `write_row` and
//  wraps at `history_rows`, so the texture itself never moves in
//  memory — the wrap happens in the sampling math below.
//
//  Convention: uv.y = 0 at the TOP of the viewport (newest
//  row, just under the spectrum line) → uv.y = 1 at the
//  BOTTOM (oldest row, scrolling out). Matches SDR++ / GTK
//  UI.
// --------------------------------------------------------------

struct WaterfallVertexOut {
    float4 position [[position]];
    float2 uv;
};

/// Full-screen quad vertex shader. `vertex_id` 0..3 produces
/// a triangle strip covering the full clip-space. The Swift
/// side restricts the viewport to the waterfall region so the
/// "full-screen" quad actually fills only the bottom portion
/// of the MTKView's drawable.
vertex WaterfallVertexOut waterfall_vert(
    uint vid [[vertex_id]]
) {
    // Triangle strip: BL, BR, TL, TR.
    float2 pos[4] = {
        float2(-1.0, -1.0),
        float2( 1.0, -1.0),
        float2(-1.0,  1.0),
        float2( 1.0,  1.0),
    };
    // UVs arranged so uv.y=0 is at the TOP of the viewport
    // (matches the "newest at top" convention).
    float2 uvs[4] = {
        float2(0.0, 1.0),  // BL: bottom-left → oldest on the left
        float2(1.0, 1.0),  // BR: bottom-right → oldest on the right
        float2(0.0, 0.0),  // TL: top-left → newest on the left
        float2(1.0, 0.0),  // TR: top-right → newest on the right
    };

    WaterfallVertexOut out;
    out.position = float4(pos[vid], 0.0, 1.0);
    out.uv = uvs[vid];
    return out;
}

/// Waterfall fragment shader. Samples the history ring, maps
/// dB to a palette entry via the turbo LUT.
///
/// UV-wrap math explanation: `write_row` is the index of the
/// row we JUST wrote this frame, i.e. the NEWEST row. (The
/// Swift side sets `uniforms.writeRow` before incrementing
/// the cursor — see `draw()` in `SpectrumMTKView.swift`.)
/// As `uv.y` grows from 0 (top) to 1 (bottom), we walk
/// backwards through the ring. `fract` wraps the resulting
/// normalized row into [0, 1); the `+ history_rows` bias
/// keeps the argument positive even when `write_row` is
/// small and `age_rows` is large.
fragment float4 waterfall_frag(
    WaterfallVertexOut       in      [[stage_in]],
    texture2d<float>         history [[texture(0)]],
    texture2d<float>         palette [[texture(1)]],
    constant Uniforms&       u       [[buffer(0)]]
) {
    constexpr sampler hist_s(filter::linear, address::clamp_to_edge);
    constexpr sampler pal_s (filter::linear, address::clamp_to_edge);

    float hist_rows_f = float(u.history_rows);
    float age_rows    = in.uv.y * (hist_rows_f - 1.0);
    float newest_row  = float(u.write_row);
    float sample_y    = fract((newest_row + hist_rows_f - age_rows) / hist_rows_f);

    float db  = history.sample(hist_s, float2(in.uv.x, sample_y)).r;
    float t   = saturate((db - u.min_db) / max(0.001, u.max_db - u.min_db));
    return palette.sample(pal_s, float2(t, 0.5));
}
