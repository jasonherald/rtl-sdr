//! Multirate DSP throughput — `PowerDecimator` and
//! `RationalResampler` at realistic SDR sample-rate conversions
//! (epic #452 CPU investigation, track 1).
//!
//! The existing `fir` bench measures `ComplexFirFilter::process`
//! at full input rate, which is NOT how the channel filter runs
//! in the live pipeline (`RxVfo` applies the FIR after resampling
//! down to the output rate, so the filter sees audio-rate samples,
//! not 2.4 Msps). These benches instead measure the two resampler
//! primitives that drive the IQ → audio decimation hot path.
//!
//! Shapes chosen to match the dominant live-pipeline call sites:
//!
//! - **`PowerDecimator` 64:1 on 2.4 Msps** — the `IqFrontend`
//!   decimation from raw tuner samples to roughly 37 kHz
//!   (2.4M / 64). Power-of-two staged polyphase — each stage
//!   has short taps and runs at a progressively lower rate.
//!
//! - **`RationalResampler` 192 kHz → 48 kHz** — a typical audio-
//!   rate conversion after demodulation, used by `WfmDemod` /
//!   `NfmDemod` / `AmDemod` to feed the `SinkManager`.
//!
//! **Measurement discipline** (unchanged from other benches in
//! this crate): pre-allocated input / output / processor
//! outside the Criterion closure; `iter_batched` with
//! `BatchSize::SmallInput` and `black_box` on both sides of the
//! measured call so LLVM can't elide work.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use sdr_dsp::multirate::{PowerDecimator, RationalResampler};
use sdr_types::Complex;

/// Raw tuner rate the `IqFrontend` sees from an RTL-SDR dongle
/// (2.4 Msps is the mainstream wide-FM sample rate).
const TUNER_SAMPLE_RATE_HZ: f64 = 2_400_000.0;

/// Power-of-two decimation ratio typical for "IQ frontend → audio
/// rate" in the live pipeline. 64:1 on 2.4 Msps → 37.5 kHz.
const DECIM_RATIO: u32 = 64;

/// Block size the pipeline processes per tick. 16384 samples at
/// 2.4 Msps is about one TV-tuner USB transfer worth of IQ — a
/// realistic per-tick workload.
const DECIM_INPUT_BLOCK: usize = 16_384;

/// Post-demod sample rate produced by `WfmDemod` (48 kHz stereo
/// composite resampled down to consumer audio). Use 192 kHz
/// here so the resampler bench actually does non-trivial work
/// instead of short-circuiting to the pass-through branch.
const DEMOD_IN_RATE_HZ: f64 = 192_000.0;
/// Consumer audio rate the `SinkManager` needs to feed `PipeWire`.
const AUDIO_OUT_RATE_HZ: f64 = 48_000.0;
/// One audio-tick block at the demod rate (≈ 25 ms).
const RESAMPLER_INPUT_BLOCK: usize = 4_800;

const PHASE_STEP_RAD: f32 = 0.001;

fn make_complex_input(n: usize) -> Vec<Complex> {
    (0..n)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32;
            Complex {
                re: (t * PHASE_STEP_RAD).sin(),
                im: (t * PHASE_STEP_RAD).cos(),
            }
        })
        .collect()
}

fn bench_power_decimator(c: &mut Criterion) {
    let mut decim = PowerDecimator::new(DECIM_RATIO).expect("valid decim ratio");
    let input = make_complex_input(DECIM_INPUT_BLOCK);
    let out_cap = DECIM_INPUT_BLOCK.div_ceil(DECIM_RATIO as usize) + 1;

    let mut group = c.benchmark_group("multirate_power_decimator");
    group.throughput(Throughput::Elements(DECIM_INPUT_BLOCK as u64));
    group.bench_function(
        format!("ratio={DECIM_RATIO}_samples={DECIM_INPUT_BLOCK}_at_{TUNER_SAMPLE_RATE_HZ}Hz"),
        |b| {
            let mut output = vec![Complex::default(); out_cap];
            b.iter_batched(
                || black_box(input.clone()),
                |buf| {
                    decim
                        .process(&buf, &mut output)
                        .expect("output sized to input");
                    black_box(&output);
                },
                BatchSize::SmallInput,
            );
        },
    );
    group.finish();
}

fn bench_rational_resampler(c: &mut Criterion) {
    let mut resamp =
        RationalResampler::new(DEMOD_IN_RATE_HZ, AUDIO_OUT_RATE_HZ).expect("valid resample ratio");
    let input = make_complex_input(RESAMPLER_INPUT_BLOCK);
    // Worst-case output size for any down-sampling resampler is
    // bounded by the input length; allocate generously.
    let out_cap = RESAMPLER_INPUT_BLOCK + 8;

    let mut group = c.benchmark_group("multirate_rational_resampler");
    group.throughput(Throughput::Elements(RESAMPLER_INPUT_BLOCK as u64));
    group.bench_function(
        format!("{DEMOD_IN_RATE_HZ}Hz_to_{AUDIO_OUT_RATE_HZ}Hz_samples={RESAMPLER_INPUT_BLOCK}"),
        |b| {
            let mut output = vec![Complex::default(); out_cap];
            b.iter_batched(
                || black_box(input.clone()),
                |buf| {
                    resamp
                        .process(&buf, &mut output)
                        .expect("output sized to input");
                    black_box(&output);
                },
                BatchSize::SmallInput,
            );
        },
    );
    group.finish();
}

criterion_group!(benches, bench_power_decimator, bench_rational_resampler);
criterion_main!(benches);
