// Stockham autosort radix-2 forward FFT, one pass per dispatch.
//
// Epic #452 / #179. See `gpu_fft.rs` for the host-side scaffolding
// and algorithm notes. This shader is intentionally minimal:
// every per-pass detail (which pass index, total size) comes in
// via the `Params` uniform so the host can reuse the same pipeline
// for all `log2(N)` passes.
//
// Why Stockham (not in-place Cooley-Tukey): the in-place variant
// needs an explicit bit-reversal permutation, which is ugly to
// parallelize on GPU. Stockham ping-pongs between two buffers
// and writes the combined subFFT contiguously on each pass, so
// the output of pass `s` is already in "natural" ordering for
// pass `s+1` to consume. No permutation pass, no scatter/gather.
//
// Pass `s` (0 ≤ s < log2(N)) takes `src` in "stride-N/2" layout
// and writes `dst` in "contiguous subFFT of size 2^(s+1)" layout:
//
//   L = 2^(s+1)            // size of output subFFTs after this pass
//   m = L / 2 = 2^s        // size of input subFFTs before this pass
//   j = bfly >> s          // which group of paired subFFTs
//   k = bfly & (m - 1)     // position within one butterfly group
//
//   a = src[j*m + k]
//   b = src[j*m + k + N/2]
//   W = exp(-2πi * k / L)                 (forward DFT twiddle)
//   dst[j*L + k]     = a + W*b
//   dst[j*L + k + m] = a - W*b
//
// Total butterflies per pass = N/2 (one per workitem).

struct Params {
    // Which pass of the `log2(N)` sequence this dispatch runs.
    pass_s: u32,
    // Total number of complex points (power of two).
    size: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read>       src: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> dst: array<vec2<f32>>;

fn complex_mul(a: vec2<f32>, b: vec2<f32>) -> vec2<f32> {
    // (a.re + i*a.im) * (b.re + i*b.im) expanded.
    return vec2<f32>(
        a.x * b.x - a.y * b.y,
        a.x * b.y + a.y * b.x,
    );
}

// 64 threads/workgroup — the common sweet-spot for compute
// shaders that have to run well on NVIDIA (warp = 32, 2 warps),
// AMD RDNA (wavefront = 32), AMD GCN (wavefront = 64), and Intel
// iGPUs. No dependence on subgroup size.
@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let bfly = gid.x;
    let half_n = params.size >> 1u;
    if (bfly >= half_n) {
        return;
    }

    let m: u32 = 1u << params.pass_s;     // == 2^s
    let l: u32 = m << 1u;                 // == 2^(s+1)
    let mask_m: u32 = m - 1u;             // == 2^s - 1 (zero when m == 1)

    // Split the flat butterfly index into (group, within-group).
    let j: u32 = bfly >> params.pass_s;
    let k: u32 = bfly & mask_m;

    // Stockham invariant: src reads always stride by N/2,
    // regardless of pass. Only dst writes change their stride
    // (by `m`, which doubles each pass).
    let src_lo = j * m + k;
    let src_hi = src_lo + half_n;
    let a = src[src_lo];
    let b = src[src_hi];

    // Forward-DFT twiddle: W_L^k = exp(-2πi * k / L).
    // Using `radians = -2π·k / L` keeps the angle math in f32;
    // the cos/sin lookup is a single pair of hardware fast-math
    // calls on every GPU architecture we care about.
    let two_pi: f32 = 6.283185307179586;
    let angle: f32 = -two_pi * f32(k) / f32(l);
    let tw = vec2<f32>(cos(angle), sin(angle));
    let wb = complex_mul(tw, b);

    let dst_lo = j * l + k;
    let dst_hi = dst_lo + m;
    dst[dst_lo] = a + wb;
    dst[dst_hi] = a - wb;
}
