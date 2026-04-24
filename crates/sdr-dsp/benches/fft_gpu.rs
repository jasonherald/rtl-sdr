//! GPU FFT throughput — `GpuFftEngine` (wgpu + Stockham autosort,
//! radix-2) at the same three power-of-two sizes as the CPU
//! baseline in `benches/fft.rs`. Epic #452 phase 2 / #179.
//!
//! **Comparison surface.** Identical to `benches/fft.rs`:
//!
//! - Same `FftEngine::forward` trait call shape — in-place complex
//!   f32 forward DFT.
//! - Same input (sinusoid via `INPUT_PHASE_STEP_RAD`).
//! - Same `iter_batched` + `SmallInput` + `black_box` discipline
//!   (CPU bench rationale at `benches/fft.rs`).
//! - Same pre-allocation discipline: engine is built once outside
//!   the Criterion closure, only `forward` runs inside. The
//!   engine already owns all its wgpu state (pipeline, ping-pong
//!   buffers, bind groups) so this is where the GPU path earns
//!   back the driver-init cost on every tick.
//!
//! **Adapter selection.** Env var `SDR_GPU_ADAPTER` drives which
//! adapter wgpu picks:
//!
//! - unset / `auto` / `high` → `HighPerformance` (default, picks
//!   discrete GPU on laptops and multi-GPU desktops)
//! - `low` / `igpu`          → `LowPower` (forces iGPU on hybrid
//!   setups)
//! - `fallback` / `software` → forces a software adapter
//!
//! Backend override via `SDR_GPU_BACKEND=vulkan|metal|dx12|gl` —
//! the default is `Backends::all()` which wgpu resolves per-
//! platform. For the multi-GPU matrix on the dev machine (NVIDIA
//! 4080 Super, NVIDIA 3090, AMD iGPU), set both vars to pin the
//! exact adapter per bench run.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use sdr_dsp::fft::FftEngine;
use sdr_dsp::gpu_fft::{GpuFftEngine, GpuFftOptions};
use sdr_types::Complex;

/// FFT sizes under test. Mirrors `benches/fft.rs::SIZES` exactly.
const SIZES: &[usize] = &[2048, 8192, 65_536];

/// Matches `benches/fft.rs::INPUT_PHASE_STEP_RAD`. Data-independent
/// for both engines — same value keeps the two benches directly
/// comparable line-for-line.
const INPUT_PHASE_STEP_RAD: f32 = 0.01;

fn make_input(size: usize) -> Vec<Complex> {
    (0..size)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32;
            Complex {
                re: (t * INPUT_PHASE_STEP_RAD).sin(),
                im: (t * INPUT_PHASE_STEP_RAD).cos(),
            }
        })
        .collect()
}

fn gpu_options_from_env() -> GpuFftOptions {
    let mut opts = GpuFftOptions::default();
    if let Ok(pref) = std::env::var("SDR_GPU_ADAPTER") {
        match pref.to_ascii_lowercase().as_str() {
            "high" | "auto" | "" => {}
            "low" | "igpu" => opts.power_preference = wgpu::PowerPreference::LowPower,
            "fallback" | "software" => opts.force_fallback_adapter = true,
            other => eprintln!("unknown SDR_GPU_ADAPTER={other:?}, using default"),
        }
    }
    if let Ok(backend) = std::env::var("SDR_GPU_BACKEND") {
        match backend.to_ascii_lowercase().as_str() {
            "vulkan" | "vk" => opts.backends = wgpu::Backends::VULKAN,
            "metal" => opts.backends = wgpu::Backends::METAL,
            "dx12" | "d3d12" => opts.backends = wgpu::Backends::DX12,
            "gl" | "opengl" | "gles" => opts.backends = wgpu::Backends::GL,
            other => eprintln!("unknown SDR_GPU_BACKEND={other:?}, using default"),
        }
    }
    opts
}

fn bench_forward(c: &mut Criterion) {
    let opts = gpu_options_from_env();

    let mut group = c.benchmark_group("fft_forward_gpu_stockham");
    for &size in SIZES {
        group.throughput(Throughput::Elements(size as u64));
        let input = make_input(size);

        // Construct the engine once. If the GPU is unavailable on
        // this machine (no adapter / CI runner / driver issue),
        // log and skip rather than panicking — the CPU bench still
        // runs and the comparison just won't include this config.
        let Ok(mut engine) = GpuFftEngine::with_options(size, &opts) else {
            eprintln!("GPU FFT engine unavailable at size={size}, skipping");
            continue;
        };

        // Print the adapter once per size so the bench output
        // makes it obvious which GPU was measured (the dev machine
        // has 3 — 4080 Super / 3090 / AMD 780M iGPU — and the
        // default high-performance pick is not guaranteed to be
        // stable across driver releases).
        let summary = engine.adapter_summary();
        eprintln!(
            "fft_forward_gpu_stockham/size={size}: adapter = {} ({:?} / {:?}) driver = {}",
            summary.name, summary.device_type, summary.backend, summary.driver,
        );

        group.bench_function(format!("size={size}"), |b| {
            b.iter_batched(
                // Same `black_box` shape as the CPU bench so LLVM
                // can't elide the buffer population or hoist
                // constants into the measured call.
                || black_box(input.clone()),
                |mut buf| {
                    engine.forward(&mut buf).expect("GPU forward FFT");
                    black_box(&buf);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_forward);
criterion_main!(benches);
