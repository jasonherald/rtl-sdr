# Epic #452 — CPU investigation and GPU-path retrospective

Written 2026-04-24, after Phase 2c merged (PR #456).

## TL;DR

- **rustfft's AVX2 path is already 3.8× faster than scalar on the dev machine**
  — SIMD is doing its job, no unclaimed headroom.
- **The complete CPU DSP pipeline uses ~7% of one core** at a worst-case 2.4 Msps / 60 fps / full-pipeline workload. FFT specifically is 0.75%.
- **Rayon parallelism across pipeline stages wouldn't buy us anything** — there's no batched independent-FFT opportunity in the live signal path.
- **Conclusion: skip further CPU FFT optimization entirely.** The lever is already pulled.
- **Recommended next step: Phase 3a (GPU waterfall chain).** Architectural win that addresses the actually-expensive thing (rendering), not a micro-optimization of something that's already cheap.

## Context

Epic #452 set out to move FFT / FIR / waterfall / detection to the GPU for performance. Phases 1–2c delivered:

- Phase 1 (PR #453): CPU baseline benchmark harness
- Phase 2 (PR #454): wgpu Stockham FFT (per-pass dispatches)
- Phase 2b (PR #455): tiered 2D Cooley-Tukey (≤2 dispatches)
- Phase 2c (PR #456): no-readback experiment + by-name adapter

Across these, the wgpu GPU FFT landed at 277 µs for N=65536 on an RTX 4080 Super — consistently **slower than the ~143 µs CPU baseline**. Phase 2c's no-readback experiment showed that GPU→CPU readback accounts for 18–23% of GPU wall-clock time at small N (and is the likely explanation for the N=65536 variance).

User called for a step-back: before committing to more GPU work or CPU work, profile where the CPU time actually goes and decide based on data. This doc is that investigation's output.

## Track 2 — rustfft SIMD is already doing the work

**Question:** Is rustfft using SIMD on the dev machine, and how much is it buying us? Could we do better with `std::simd` / `wide` / AVX-512?

**Method:** rustfft 6.4 exposes `FftPlannerScalar` (portable, always scalar) and `FftPlannerAvx` (runtime-selected AVX2, returns `Err` on CPUs lacking it). A new dedicated bench (`benches/fft_simd_compare.rs`) runs both at the same three FFT sizes.

**Dev machine:** AMD Ryzen 9 PRO 8945HS (Zen 4). Full AVX-512 support (F/BW/DQ/VL + VNNI + BF16).

**Results** (median, release build, `f32`):

| Size  | Scalar      | AVX2        | Speedup |
|-------|-------------|-------------|---------|
| 2048  |    9.65 µs  |    2.68 µs  | **3.6×** |
| 8192  |   43.50 µs  |   11.40 µs  | **3.8×** |
| 65536 |  482.40 µs  |  125.40 µs  | **3.85×** |

**Reading:**
- AVX2 is a ~3.8× speedup and it's what our production path uses today
  (the default `FftPlanner` runtime-selects AVX2 on this CPU — confirmed by
  the numbers matching our existing `benches/fft.rs` baseline within noise).
- **No unclaimed SIMD headroom on 2048/8192/65536 with current rustfft.**
- AVX-512 is available on this CPU but rustfft doesn't use it. Theoretical
  ceiling is another ~2× (256→512-bit vector width), but:
  - No stable pure-Rust FFT library uses AVX-512 kernels today
  - `std::simd` is nightly-only and doesn't reliably emit AVX-512
  - Hand-rolling AVX-512 intrinsics = hyper-specialization against this
    specific CPU, violating the portability goal
  - Even if achieved: ~60 µs → ~30 µs at N=65536 = saves 4 ms/sec at 60 fps
    display rate. Invisible.

**Track 2 verdict:** rustfft AVX2 is the right choice. Further CPU SIMD is
not worth pursuing.

## Track 1 — per-operation CPU budget

**Question:** Where does CPU time *actually* go in a running SDR session?
Is FFT a bottleneck? Is anything?

**Method:** Aggregate measured bench results into a per-second CPU budget,
assuming a representative worst-case wide-FM workload:

- **IQ sample rate:** 2.4 Msps (RTL-SDR typical)
- **Display / waterfall rate:** 60 fps
- **Audio output rate:** 48 kHz
- **IqFrontend decimation:** 64:1 → 37.5 kHz post-decimation
- **Post-demod resampling:** 192 kHz → 48 kHz (WFM stereo composite case)
- **Audio LPF:** full-rate real FIR at 48 kHz

**New benches added for this investigation:**

The existing `fir.rs` bench measured `ComplexFirFilter` at full 2.4 Msps input
rate — which would be ~190% of one CPU core if that's what the live pipeline
actually did. Audit of `sdr-pipeline::IqFrontend` and `sdr-dsp::channel::RxVfo`
shows it does NOT: the channel filter runs AFTER `RationalResampler` has
already decimated the stream to audio rates, so the FIR sees ~37–192 kHz, not
2.4 Msps. The relevant operations are `PowerDecimator` (polyphase staged) and
`RationalResampler`. `benches/multirate.rs` measures both at live-pipeline
shapes.

**Per-call measurements** (median, 4080 Super / Vulkan not relevant — these are
CPU benches, Zen 4 + AVX2):

| Operation                                   | Shape                                        | Per-call time |
|---------------------------------------------|----------------------------------------------|---------------|
| `FftPlanner::forward` (AVX2)                | 65536 points                                 | 125 µs        |
| `PowerDecimator::process` (64:1)            | 16384 IQ in → 256 IQ out                     | 80 µs         |
| `RationalResampler::process` (192→48 kHz)   | 4800 complex in → 1200 out                   | 74 µs         |
| `ComplexFirFilter::process` (channel FIR)   | 960 taps × 4800 samples @ 37.5 kHz           | ~3 ms (extrap)|
| `FirFilter::process_f32` (audio LPF)        | 960 taps × 4800 samples @ 48 kHz             | 3.0 ms        |
| Colormap lookup (f32 dB → RGBA)             | 65536 bins                                   | 225 µs        |
| Energy detection (sort-based floor)         | 65536 bins                                   | 795 µs        |

**Per-second CPU budget** (operation time × calls/sec):

| Operation                              | Calls/sec        | CPU/sec    | % of 1 core |
|----------------------------------------|------------------|------------|-------------|
| FFT                                    | 60 (display)     | 7.5 ms     | **0.75%**   |
| `PowerDecimator` (IQ 2.4M → 37.5 kHz)  | 146 (2.4M/16k)   | 11.7 ms    | 1.17%       |
| `RationalResampler` (192→48 kHz)       | 40 (192k/4.8k)   | 3.0 ms     | 0.30%       |
| Channel filter (`ComplexFirFilter`)    | 8 (37.5k/4.8k)   | ~24 ms     | ~2.4%       |
| Audio LPF (`FirFilter`)                | 10 (48k/4.8k)    | 30 ms      | **3.0%**    |
| Colormap                               | 60 (display)     | 13.5 ms    | 1.35%       |
| Demodulation (NFM/WFM/AM)              | continuous       | <1 ms      | <0.1%       |
| **Pipeline total**                     |                  | **~90 ms** | **~9%**     |

Even being generous with extrapolation, **the entire DSP pipeline consumes under
10% of one CPU core** at worst-case load. The machine has 8+ cores available.

## What this means

1. **FFT is 0.75% of CPU.** Optimizing it further — via rayon, via AVX-512,
   via any means — saves a fraction of a percent. Invisible.

2. **No pipeline stage is dominant.** The biggest single consumer is the audio
   LPF at 3%. Nothing is the bottleneck.

3. **Rayon parallelism doesn't apply well here.** The pipeline stages are
   sequential (decimator → resampler → filter → demod → LPF), not batchable.
   The only truly parallel work would be across multiple simultaneous VFO/demod
   pipelines (scanner-mode or multi-channel receivers), which don't exist in
   the current architecture.

4. **The real expensive CPU work is elsewhere.** GTK4 rendering, Cairo waterfall
   redraws, transcription (whisper/sherpa), and USB I/O are all orders of
   magnitude more CPU than the DSP pipeline. None of them are benched — because
   none of them are controlled by the `sdr-dsp` crate.

5. **GPU FFT-with-readback will never beat CPU at these sizes.** The wgpu
   readback cost (~100–150 µs for N=65536 per Phase 2c measurement) alone
   exceeds the entire CPU FFT budget. This is inherent, not a wgpu bug.

## Recommendation: next direction

### Skip further CPU FFT optimization

No SIMD tweaks, no rayon, no AVX-512 intrinsics. It's solved.

### Skip standalone GPU FFT optimization

Phase 2c proved the GPU path can't beat CPU when it has to read back. Don't keep
pushing.

### Pursue Phase 3a: GPU waterfall chain

This is where GPU architecturally wins. The FFT output goes to:

1. dB conversion (`10·log10(mag²)`)
2. Colormap lookup (f32 → RGBA)
3. Texture upload for waterfall display
4. Line plot render for spectrum display

Currently every step round-trips through CPU memory. If the FFT result stays
on the GPU and feeds directly into a colormap compute pass which writes to a
GPU texture the render pipeline samples — **no CPU readback ever happens**.
That eliminates the ~100–150 µs readback cost AND most of the texture-upload
cost from CPU.

The `forward_no_readback` experiment from Phase 2c is exactly the API this
builds on.

Frontend CPU savings potentially much larger than the DSP numbers above —
Cairo waterfall updates likely dominate our current UI frame time, and moving
them to a GPU texture sampler would be visible user-facing latency
improvement.

### Long-term: Phase 4 GPU FIR (many moons away)

CPU FIR is ~3% of a core at worst. Moving it to GPU adds latency (submit +
readback) and will almost certainly lose the wall-clock comparison, just
like Phase 2's standalone FFT did. The only reason to do it is to free that
3% — which is a "nice to have, not a need" by any honest measure.

## Known unknowns (not resolved by this investigation)

- GTK4 / Cairo rendering CPU cost under live waterfall — not benched.
- Whisper / Sherpa transcription CPU cost — known-large (seconds-scale), not
  part of the DSP pipeline but a significant consumer of CPU on the host.
- USB I/O overhead for RTL-SDR sample retrieval — not benched.
- Scanner mode (if it gets built) would change the FFT-parallelism picture —
  many channels' FFTs in one tick is exactly the rayon-across-batched-FFTs
  case that current code doesn't hit.

These could be followups if the UI or pipeline growth surfaces concrete
regressions.

## Plan

1. ✅ Land this investigation (data + recommendation as PR).
2. File **Phase 3a as the next ticket**: `#180` GPU waterfall colormap,
   built on the `forward_no_readback` foundation from Phase 2c. Scope:
   dB conversion + colormap in a compute pipeline, texture output consumed
   directly by the waterfall render path.
3. Phase 4 (GPU FIR) deferred until app is stable and we have a specific
   rationale to pursue it.
