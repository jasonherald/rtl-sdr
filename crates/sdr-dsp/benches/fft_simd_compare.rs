//! Compare rustfft's scalar path against its AVX2 path on the
//! same host, to quantify how much SIMD is already buying us
//! today (epic #452 CPU investigation, track 2).
//!
//! rustfft 6.x exposes `FftPlannerScalar` (always scalar code,
//! portable across every CPU) and `FftPlannerAvx` (requires
//! runtime AVX2 detection, returns `Err` on CPUs without it).
//! Both yield the same `Arc<dyn Fft<T>>` handle so the hot path
//! is identical — the only difference is which kernel family
//! the planner picks. That lets us measure the SIMD lift
//! without touching any build flags or features.
//!
//! **Why this bench exists:** before we commit to an
//! "add SIMD / rayon" optimization direction, we need to know
//! what our current baseline already gets. If rustfft's AVX2
//! path is 3-4× scalar, SIMD is doing its job and "more SIMD"
//! is a diminishing-returns lever. If it's 1.2-1.5×, there's
//! real headroom.
//!
//! **Portability note:** this bench only provides useful numbers
//! on `x86_64` CPUs with AVX2 (widely supported since ~2013 on
//! Intel Haswell and AMD Excavator). On CPUs lacking AVX2 the
//! AVX group is skipped and only the scalar numbers land. The
//! scalar group always runs.
//!
//! **Not a claim about our FFT hot path.** Our production code
//! uses `sdr_dsp::fft::RustFftEngine` which wraps the default
//! `FftPlanner`. The planner already runtime-selects AVX2 on
//! this hardware, so our production numbers equal (or very
//! nearly) the AVX numbers here.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use rustfft::num_complex::Complex as RustFftComplex;
use rustfft::{FftPlannerAvx, FftPlannerScalar};

/// Same three sizes as the other FFT benches — `benches/fft.rs`
/// measures the auto-selecting `FftPlanner`, so these tables are
/// directly comparable.
const SIZES: &[usize] = &[2048, 8192, 65_536];

/// Matches the phase-step used by the radix-2 / radix-4 benches
/// so the input shape is identical across the FFT bench family.
const INPUT_PHASE_STEP_RAD: f32 = 0.01;

fn make_input(size: usize) -> Vec<RustFftComplex<f32>> {
    (0..size)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32;
            RustFftComplex::new(
                (t * INPUT_PHASE_STEP_RAD).sin(),
                (t * INPUT_PHASE_STEP_RAD).cos(),
            )
        })
        .collect()
}

fn bench_scalar(c: &mut Criterion) {
    let mut planner = FftPlannerScalar::<f32>::new();

    let mut group = c.benchmark_group("fft_rustfft_scalar");
    for &size in SIZES {
        group.throughput(Throughput::Elements(size as u64));
        let fft = planner.plan_fft_forward(size);
        let scratch_len = fft.get_inplace_scratch_len();
        let mut scratch = vec![RustFftComplex::new(0.0_f32, 0.0); scratch_len];
        let input = make_input(size);

        group.bench_function(format!("size={size}"), |b| {
            b.iter_batched(
                || black_box(input.clone()),
                |mut buf| {
                    fft.process_with_scratch(&mut buf, &mut scratch);
                    black_box(&buf);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_avx(c: &mut Criterion) {
    // `FftPlannerAvx::new` returns Err on CPUs without AVX2 at
    // runtime — log-and-skip so the bench binary still runs on
    // older hardware (CI runners, etc.) rather than panicking.
    let Ok(mut planner) = FftPlannerAvx::<f32>::new() else {
        eprintln!("CPU lacks AVX2, skipping fft_rustfft_avx benches");
        return;
    };

    let mut group = c.benchmark_group("fft_rustfft_avx");
    for &size in SIZES {
        group.throughput(Throughput::Elements(size as u64));
        let fft = planner.plan_fft_forward(size);
        let scratch_len = fft.get_inplace_scratch_len();
        let mut scratch = vec![RustFftComplex::new(0.0_f32, 0.0); scratch_len];
        let input = make_input(size);

        group.bench_function(format!("size={size}"), |b| {
            b.iter_batched(
                || black_box(input.clone()),
                |mut buf| {
                    fft.process_with_scratch(&mut buf, &mut scratch);
                    black_box(&buf);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_scalar, bench_avx);
criterion_main!(benches);
