// Tiered Cooley-Tukey DIT FFT — one shared-memory sub-FFT per
// workgroup, Stockham ping-pong across passes, parameterized by
// override constants so the same shader serves every sub-FFT
// size we need (32, 64, 128, 256 for epic #452's target N of
// 2048 / 8192 / 65536).
//
// Epic #452 phase 2b / #179. Replaces the per-pass Stockham
// shader from phase 2, which paid ~15 µs of driver scheduling
// overhead per pass × log2(N) passes — dominating compute at
// every SDR-relevant size.
//
// # Algorithm
//
// For a total FFT of size N = P·Q, DIT Cooley-Tukey:
//
//   1. Stage 1: P workgroups, each computes a size-Q sub-FFT
//      reading stride-P from the input buffer. Multiplies by
//      the cross-stage twiddle `ω_N^(p·k_Q)`, then writes to the
//      scratch buffer in column-major layout
//      `Z[p, k_Q] → idx k_Q·P + p`.
//   2. Stage 2: Q workgroups, each computes a size-P sub-FFT
//      reading Z[:, k_Q] (contiguous reads, since Z is column-
//      major) and writes to the output buffer in natural order
//      `y[k_P·Q + k_Q]`.
//
// Both stages use this shader. The stage's access pattern is
// entirely described by the uniform `Params`, so the Rust side
// doesn't need two shaders — just two bind groups and two
// dispatches with different uniform contents.
//
// For single-stage sizes (trivial decomposition N = 1·N), the
// engine just dispatches stage 1 with no twiddle.
//
// # Shared-memory sub-FFT (Stockham ping-pong)
//
// Inside one workgroup:
//
// - Each thread owns `POINTS_PER_THREAD` shared-memory slots and
//   does one butterfly per pass. With POINTS_PER_THREAD = 2 and
//   WORKGROUP_SIZE = SUB_N / 2, that exactly covers SUB_N points
//   and SUB_N/2 butterflies per pass without bounds-checking.
//
// - Two `var<workgroup>` arrays `sm_a`, `sm_b` ping-pong between
//   passes. Pass `s` reads whichever buffer holds the pass-(s-1)
//   output and writes the other. `workgroupBarrier()` between
//   passes — no `storageBarrier()` needed (all traffic is
//   workgroup-local).
//
// - The read/write decision uses a uniform branch on
//   `(s & 1) == 0`, which every thread in the workgroup takes
//   together. No warp divergence.
//
// # Memory access
//
// - Stage 1 input reads are stride-P (threads within a warp hit
//   stride-P global addresses). This is the biggest cost — an
//   explicit transpose pass would fix it at the cost of a third
//   dispatch. Kept simple for now; flagged as a phase 2c knob
//   if the numbers warrant.
//
// - Stage 1 output writes are stride-P (column-major Z), again
//   strided within a warp. Writes are less bandwidth-sensitive
//   than reads on modern GPUs (write-combining buffers).
//
// - Stage 2 input reads are **contiguous** — threads within a
//   warp read consecutive addresses in `Z[:, k_Q]` because Z is
//   column-major and workgroup k_Q reads column k_Q. This is
//   the access pattern we've optimized the Z layout for.
//
// - Stage 2 output writes are stride-Q. Unavoidable if we want
//   natural-order output. One write per point, no re-read cost.

override WORKGROUP_SIZE: u32 = 128u;
override SUB_N: u32 = 256u;
override LOG2_SUB_N: u32 = 8u;
override POINTS_PER_THREAD: u32 = 2u;

struct Params {
    // Total FFT size N (the full transform, not just this stage's
    // sub-FFT). Used by stage 1's twiddle formula ω_N^(p·k_Q).
    total_n: u32,
    // Input access pattern for this workgroup. `global_idx` for
    // shared-memory slot `local_idx` is:
    //   input_sub_offset_base + wg_id * input_sub_offset_mult
    //                         + local_idx * input_stride
    input_sub_offset_base: u32,
    input_sub_offset_mult: u32,
    input_stride: u32,
    // Output access pattern, same shape as input.
    output_sub_offset_base: u32,
    output_sub_offset_mult: u32,
    output_stride: u32,
    // Whether to multiply by the cross-stage twiddle after the
    // sub-FFT and before writing the output. 1 for stage 1 of
    // a tiered transform, 0 for stage 2 and for single-stage
    // transforms. When set, `wg_id * twiddle_p_mult` gives `p`
    // (for the formula `ω_N^(p · k_Q)` where k_Q = local_idx).
    apply_twiddle: u32,
    twiddle_p_mult: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read>       input_buf: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> output_buf: array<vec2<f32>>;

// Ping-pong shared memory. Sized at pipeline compile time via the
// `SUB_N` override constant.
var<workgroup> sm_a: array<vec2<f32>, SUB_N>;
var<workgroup> sm_b: array<vec2<f32>, SUB_N>;

fn complex_mul(a: vec2<f32>, b: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(
        a.x * b.x - a.y * b.y,
        a.x * b.y + a.y * b.x,
    );
}

// Runs one Stockham radix-2 butterfly for the given pass index.
// Reads from `src_select == 0` (sm_a) or 1 (sm_b), writes the
// opposite buffer.
fn stockham_butterfly(bfly: u32, pass_s: u32, src_select: u32) {
    // See `gpu_fft.rs` or the phase-2 shader for the full
    // derivation of this per-butterfly math. Short form:
    //   j = bfly >> pass_s       (group index among 2^s-sized subFFTs)
    //   k = bfly & (2^s - 1)     (position within butterfly)
    //   src_lo = j*m + k, src_hi = src_lo + SUB_N/2
    //   dst_lo = j*l + k, dst_hi = dst_lo + m   where l = 2m
    //   W_L^k = exp(-2πi k / l)
    //   dst_lo = src_lo + W·src_hi
    //   dst_hi = src_lo - W·src_hi
    let m: u32 = 1u << pass_s;
    let l: u32 = m << 1u;
    let mask_m: u32 = m - 1u;
    let j: u32 = bfly >> pass_s;
    let k: u32 = bfly & mask_m;

    let src_lo = j * m + k;
    let src_hi = src_lo + SUB_N / 2u;
    let dst_lo = j * l + k;
    let dst_hi = dst_lo + m;

    let two_pi: f32 = 6.283185307179586;
    let angle: f32 = -two_pi * f32(k) / f32(l);
    let tw = vec2<f32>(cos(angle), sin(angle));

    // Uniform branch — every thread in the workgroup takes the
    // same side on any given pass.
    if (src_select == 0u) {
        let a = sm_a[src_lo];
        let b = sm_a[src_hi];
        let wb = complex_mul(tw, b);
        sm_b[dst_lo] = a + wb;
        sm_b[dst_hi] = a - wb;
    } else {
        let a = sm_b[src_lo];
        let b = sm_b[src_hi];
        let wb = complex_mul(tw, b);
        sm_a[dst_lo] = a + wb;
        sm_a[dst_hi] = a - wb;
    }
}

@compute @workgroup_size(WORKGROUP_SIZE)
fn main(
    @builtin(workgroup_id) wg_id: vec3<u32>,
    @builtin(local_invocation_id) local_id: vec3<u32>,
) {
    let wg = wg_id.x;
    let tid = local_id.x;

    let in_base: u32 = params.input_sub_offset_base + wg * params.input_sub_offset_mult;
    let out_base: u32 = params.output_sub_offset_base + wg * params.output_sub_offset_mult;

    // Load input → sm_a. With POINTS_PER_THREAD = 2 and
    // WORKGROUP_SIZE = SUB_N / 2, each thread loads exactly 2
    // slots and we cover all SUB_N of them.
    for (var i: u32 = 0u; i < POINTS_PER_THREAD; i = i + 1u) {
        let local_idx = tid + i * WORKGROUP_SIZE;
        let global_idx = in_base + local_idx * params.input_stride;
        sm_a[local_idx] = input_buf[global_idx];
    }
    workgroupBarrier();

    // Run LOG2_SUB_N Stockham passes. After pass s, the fresh
    // result lives in sm_b if s is even, sm_a if s is odd.
    // (Pass 0 reads sm_a and writes sm_b.)
    for (var s: u32 = 0u; s < LOG2_SUB_N; s = s + 1u) {
        let src_select: u32 = s & 1u;
        for (var b: u32 = 0u; b < POINTS_PER_THREAD / 2u; b = b + 1u) {
            let bfly = tid + b * WORKGROUP_SIZE;
            stockham_butterfly(bfly, s, src_select);
        }
        workgroupBarrier();
    }

    // Final result is in sm_a if LOG2_SUB_N is even, sm_b if odd.
    let final_is_a: bool = (LOG2_SUB_N & 1u) == 0u;

    // Write output, optionally applying the cross-stage twiddle
    // `ω_N^(p·k_Q)`.
    for (var i: u32 = 0u; i < POINTS_PER_THREAD; i = i + 1u) {
        let local_idx = tid + i * WORKGROUP_SIZE;
        var val: vec2<f32>;
        if (final_is_a) {
            val = sm_a[local_idx];
        } else {
            val = sm_b[local_idx];
        }

        if (params.apply_twiddle == 1u) {
            let p: u32 = wg * params.twiddle_p_mult;
            let k_q: u32 = local_idx;
            let two_pi: f32 = 6.283185307179586;
            let angle: f32 = -two_pi * f32(p * k_q) / f32(params.total_n);
            let tw = vec2<f32>(cos(angle), sin(angle));
            val = complex_mul(val, tw);
        }

        let global_idx = out_base + local_idx * params.output_stride;
        output_buf[global_idx] = val;
    }
}
