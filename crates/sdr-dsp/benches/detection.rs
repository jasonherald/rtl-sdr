//! Baseline energy-detection throughput — sweep a full FFT
//! spectrum looking for bins whose power clears the noise-floor
//! threshold (epic #452 phase 1 / #182 phase 3b).
//!
//! **Why this shape.** #182's "automatic signal identification,
//! band activity monitoring, smart squelch" features all start
//! from a fundamental building block: "given N dB bins, return
//! the set of bins whose power exceeds a relative threshold above
//! the estimated noise floor." That's the operation this bench
//! measures. Everything fancier (feature extraction, pattern
//! matching) is downstream of it.
//!
//! **Algorithm.** Two passes in one call:
//!
//! 1. Estimate noise floor as the median-ish of the bottom
//!    quartile (cheap approximation — real impls use more
//!    sophisticated estimators but the per-bin cost is
//!    similar).
//! 2. Scan every bin; count bins exceeding `floor + threshold_db`.
//!
//! The scan's output is a count rather than a `Vec<usize>` of
//! hit indices because allocating a vec inside a tight loop
//! defeats the "measure pure compute" discipline. A real impl
//! would write hits into a pre-allocated output buffer — the
//! GPU path will need one too.
//!
//! **Measurement discipline.** Input vec + output scratch
//! allocated once outside the closure; the hot path is purely
//! compute + one bool-ish write per bin.

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};

const BINS: usize = 65_536;

/// How many dB above the noise floor a bin must sit before it
/// counts as a detection. Picked to match a realistic "busy
/// channel" SNR threshold — this number only affects how many
/// hits the bench reports, not the cost per bin.
const THRESHOLD_DB: f32 = 10.0;

/// Fraction of bins used to estimate the noise floor. Bottom
/// 25 % is a common heuristic — quiet enough to be noise-only
/// on a sparsely-occupied spectrum, big enough to smooth out
/// single-bin glitches.
const NOISE_FLOOR_QUANTILE_BINS: usize = BINS / 4;

fn synthetic_db_line(bins: usize) -> Vec<f32> {
    // Noise floor around -80 dB + a few narrow-band "signals"
    // punched in at known offsets. Gives the bench a
    // realistic mix of detection hits + non-hits so branch
    // predictors see a realistic hit-rate (~1 %).
    let mut v = vec![-80.0_f32; bins];
    for (i, x) in v.iter_mut().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let t = i as f32;
        // Broadband noise variance.
        *x += (t * 0.05).sin() * 3.0;
        // Sparse "signals" every ~100 bins.
        if i % 100 == 0 {
            *x = -40.0;
        }
    }
    v
}

/// Cheap quartile-based noise-floor estimator.
///
/// Sorts a working copy of the bin powers — O(n log n) on the
/// full spectrum. A streaming / running-min-quantile impl would
/// be faster in steady state; this bench measures the fair
/// worst-case starting point so the GPU ticket sees the
/// generous comparison surface.
fn estimate_noise_floor(db_line: &[f32], scratch: &mut Vec<f32>) -> f32 {
    scratch.clear();
    scratch.extend_from_slice(db_line);
    scratch.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    scratch[NOISE_FLOOR_QUANTILE_BINS / 2]
}

/// Count bins exceeding `floor + THRESHOLD_DB`. Output is just
/// the count — see module doc for why.
#[inline]
fn count_detections(db_line: &[f32], floor: f32) -> usize {
    let cutoff = floor + THRESHOLD_DB;
    db_line.iter().filter(|&&x| x > cutoff).count()
}

fn bench_energy_detect(c: &mut Criterion) {
    let db_line = synthetic_db_line(BINS);
    let mut sort_scratch = Vec::with_capacity(BINS);

    let mut group = c.benchmark_group("energy_detect_cpu");
    group.throughput(Throughput::Elements(BINS as u64));
    group.bench_function(format!("bins={BINS}"), |b| {
        b.iter_batched(
            || db_line.clone(),
            |line| {
                let floor = estimate_noise_floor(&line, &mut sort_scratch);
                let _hits = count_detections(&line, floor);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_energy_detect);
criterion_main!(benches);
