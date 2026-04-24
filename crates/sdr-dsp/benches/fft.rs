//! Baseline FFT throughput — `RustFftEngine` at the three
//! power-of-two sizes the GPU path is expected to compare against
//! (epic #452 phase 1 / #179 phase 2).
//!
//! **Measurement discipline.** The engine + its planner-allocated
//! scratch buffers are constructed once outside the `iter_batched`
//! closure; the closure itself only runs the in-place forward FFT.
//! This mirrors the pre-allocation discipline the GPU harness
//! MUST follow (wgpu device, pipeline, bind groups, staging
//! buffers all amortized at construction) so the two sides can be
//! compared without one of them paying a setup tax on every tick.
//!
//! The input buffer is deliberately re-seeded from a fresh copy
//! every iteration — `RustFftEngine::forward` is in-place, so if
//! we didn't reset the input the second iteration onward would be
//! running a transform on the previous iteration's frequency-
//! domain data. Criterion's `iter_batched` with `SmallInput` is
//! designed for exactly this pattern: the setup closure clones
//! the input, the routine closure consumes it.

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use sdr_dsp::fft::{FftEngine, RustFftEngine};
use sdr_types::Complex;

/// FFT sizes under test. 2048 and 8192 are typical waterfall-rate
/// sizes; 65536 is the top-end wide-spectrum size where GPU
/// parallelism has the best shot at beating `rustfft`'s scalar
/// SIMD.
const SIZES: &[usize] = &[2048, 8192, 65536];

fn make_input(size: usize) -> Vec<Complex> {
    // Deterministic, non-zero samples — a real FFT would see
    // bursty energy from a live IQ stream, but the cost of
    // `rustfft` is independent of the data values (it's all
    // multiply-adds on the butterfly structure). Using a simple
    // sinusoid keeps the numbers reproducible across machines.
    (0..size)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32;
            Complex {
                re: (t * 0.01).sin(),
                im: (t * 0.01).cos(),
            }
        })
        .collect()
}

fn bench_forward(c: &mut Criterion) {
    let mut group = c.benchmark_group("fft_forward_rustfft");
    for &size in SIZES {
        // Measure throughput in complex samples per second so the
        // output reads as "complex samples/sec" across sizes —
        // easy to eyeball scaling and to compare against the GPU
        // harness later.
        group.throughput(Throughput::Elements(size as u64));
        let input = make_input(size);
        let mut engine = RustFftEngine::new(size).expect("valid FFT size");
        group.bench_function(format!("size={size}"), |b| {
            b.iter_batched(
                || input.clone(),
                |mut buf| {
                    engine.forward(&mut buf).expect("forward FFT in-place");
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_forward);
criterion_main!(benches);
