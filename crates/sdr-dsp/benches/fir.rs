//! Baseline FIR filter throughput — the two shapes real-time DSP
//! hits most: a channel filter on the 2.4 Msps complex IQ stream
//! and an audio LPF on the 48 kHz real output (epic #452 phase 1
//! / #181 phase 4).
//!
//! **Measurement discipline.** Filter + tap buffers allocated
//! once outside the iteration closure. Input buffer is cloned
//! per-iteration via `iter_batched` because `process` reads
//! input + writes output — reusing the same input cross-
//! iteration would be fine correctness-wise but risks the
//! compiler hoisting constants in a way that muddies the
//! comparison with a GPU path that'll see fresh samples every
//! tick.
//!
//! Why these two cases specifically:
//!
//! - **Channel filter**: complex-valued, ~256 taps, buffer
//!   sized to a realistic DSP tick (16384 samples ≈ 6.8 ms at
//!   2.4 Msps). Large enough to be non-trivial, small enough
//!   that the GPU transfer overhead realistically competes.
//! - **Audio LPF**: real-valued f32, ~128 taps, 48 kHz buffer
//!   sized to one render tick (4800 samples = 100 ms). The
//!   shape where small-buffer GPU almost certainly loses —
//!   #181 calls this out.

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use sdr_dsp::filter::{ComplexFirFilter, FirFilter};
use sdr_dsp::taps::low_pass;
use sdr_types::Complex;

/// Channel filter: complex IQ, 2.4 Msps context, ~256 taps
/// (10 kHz transition width at 2.4 Msps via Nuttall window).
const CHANNEL_IQ_BUFFER_SAMPLES: usize = 16_384;
const CHANNEL_SAMPLE_RATE_HZ: f64 = 2_400_000.0;
const CHANNEL_CUTOFF_HZ: f64 = 100_000.0;
const CHANNEL_TRANSITION_HZ: f64 = 20_000.0;

/// Audio LPF: real f32, 48 kHz context, ~128 taps
/// (500 Hz transition at 48 kHz — typical post-demod brick wall).
const AUDIO_BUFFER_SAMPLES: usize = 4_800;
const AUDIO_SAMPLE_RATE_HZ: f64 = 48_000.0;
const AUDIO_CUTOFF_HZ: f64 = 5_000.0;
const AUDIO_TRANSITION_HZ: f64 = 500.0;

/// Radians-per-sample advance for the synthetic IQ fixture. The
/// FIR hot path is data-independent (multiply-add over a fixed
/// tap set), so the exact step only has to be small enough that
/// the generated samples stay in-range — the bench cost is
/// identical either way.
const COMPLEX_INPUT_PHASE_STEP_RAD: f32 = 0.001;
/// Same idea for the real-valued audio fixture, at an order of
/// magnitude faster because 4800 samples is a much shorter span
/// than the 16384-sample IQ buffer.
const REAL_INPUT_PHASE_STEP_RAD: f32 = 0.01;

fn make_complex_input(n: usize) -> Vec<Complex> {
    (0..n)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32;
            Complex {
                re: (t * COMPLEX_INPUT_PHASE_STEP_RAD).sin(),
                im: (t * COMPLEX_INPUT_PHASE_STEP_RAD).cos(),
            }
        })
        .collect()
}

fn make_real_input(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32;
            (t * REAL_INPUT_PHASE_STEP_RAD).sin()
        })
        .collect()
}

fn bench_channel_filter(c: &mut Criterion) {
    let taps = low_pass(
        CHANNEL_CUTOFF_HZ,
        CHANNEL_TRANSITION_HZ,
        CHANNEL_SAMPLE_RATE_HZ,
        /* odd_tap_count */ false,
    )
    .expect("valid lowpass taps");
    let mut filter = ComplexFirFilter::new(taps).expect("non-empty taps");
    let input = make_complex_input(CHANNEL_IQ_BUFFER_SAMPLES);

    let mut group = c.benchmark_group("fir_channel_complex");
    group.throughput(Throughput::Elements(CHANNEL_IQ_BUFFER_SAMPLES as u64));
    group.bench_function(
        format!(
            "taps={}_samples={}",
            filter.tap_count(),
            CHANNEL_IQ_BUFFER_SAMPLES
        ),
        |b| {
            let mut output = vec![Complex::default(); CHANNEL_IQ_BUFFER_SAMPLES];
            b.iter_batched(
                || input.clone(),
                |buf| {
                    filter
                        .process(&buf, &mut output)
                        .expect("output sized to input");
                },
                BatchSize::SmallInput,
            );
        },
    );
    group.finish();
}

fn bench_audio_lpf(c: &mut Criterion) {
    let taps = low_pass(
        AUDIO_CUTOFF_HZ,
        AUDIO_TRANSITION_HZ,
        AUDIO_SAMPLE_RATE_HZ,
        /* odd_tap_count */ false,
    )
    .expect("valid lowpass taps");
    let mut filter = FirFilter::new(taps).expect("non-empty taps");
    let input = make_real_input(AUDIO_BUFFER_SAMPLES);

    let mut group = c.benchmark_group("fir_audio_real");
    group.throughput(Throughput::Elements(AUDIO_BUFFER_SAMPLES as u64));
    group.bench_function(
        format!(
            "taps={}_samples={}",
            filter.tap_count(),
            AUDIO_BUFFER_SAMPLES
        ),
        |b| {
            let mut output = vec![0.0_f32; AUDIO_BUFFER_SAMPLES];
            b.iter_batched(
                || input.clone(),
                |buf| {
                    filter
                        .process_f32(&buf, &mut output)
                        .expect("output sized to input");
                },
                BatchSize::SmallInput,
            );
        },
    );
    group.finish();
}

criterion_group!(benches, bench_channel_filter, bench_audio_lpf);
criterion_main!(benches);
