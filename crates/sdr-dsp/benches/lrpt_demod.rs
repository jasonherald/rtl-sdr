//! LRPT stage-1 demod throughput bench (epic #469 + issue #662).
//!
//! Measures the end-to-end demod chain on 1 second of synthetic
//! 144 ksps input — both the QPSK pipeline (epic #469 baseline)
//! and the OQPSK pipeline (#662, dbdexter port). Establishes the
//! perf floor for regression detection on either path.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sdr_dsp::lrpt::{LrptDemod, LrptMode};
use sdr_types::Complex;

/// 1 second of input at the demod's 144 ksps working sample rate.
const SAMPLES_1S: usize = 144_000;

/// Per-rail amplitude for a unit-power QPSK / OQPSK constellation
/// point: 1/√2 ≈ 0.707, so each corner sits on the unit circle.
const RAIL_AMP: f32 = core::f32::consts::FRAC_1_SQRT_2;

/// Synthetic stream cadence for these benches: 2 input samples
/// per symbol, matching the LRPT demod chain's working rate.
const SAMPLES_PER_SYMBOL: usize = 2;

/// Length of the deterministic constellation pattern fed into the
/// demod (one entry per QPSK quadrant).
const PATTERN_LEN: usize = 4;

fn bench_demod_qpsk(c: &mut Criterion) {
    let symbols = [
        Complex::new(RAIL_AMP, RAIL_AMP),
        Complex::new(-RAIL_AMP, RAIL_AMP),
        Complex::new(RAIL_AMP, -RAIL_AMP),
        Complex::new(-RAIL_AMP, -RAIL_AMP),
    ];
    let buf: Vec<Complex> = (0..SAMPLES_1S)
        .map(|n| {
            if n % SAMPLES_PER_SYMBOL == 0 {
                symbols[(n / SAMPLES_PER_SYMBOL) % PATTERN_LEN]
            } else {
                Complex::new(0.0, 0.0)
            }
        })
        .collect();

    c.bench_function("lrpt_demod_qpsk_1s_144ksps", |b| {
        b.iter(|| {
            // Explicit-mode constructor so a future change to
            // `LrptDemod::new`'s default can't silently flip
            // this bench to the OQPSK pipeline. Per CR round 2
            // on PR #663.
            let mut demod =
                LrptDemod::new_with_mode(LrptMode::Qpsk).expect("LrptDemod::new_with_mode");
            let mut emitted = 0_u32;
            for s in &buf {
                if demod.process(black_box(*s)).is_some() {
                    emitted += 1;
                }
            }
            black_box(emitted);
        });
    });
}

fn bench_demod_oqpsk(c: &mut Criterion) {
    // OQPSK input: I-only sample on even indices, Q-only on odd
    // (the canonical "Q delayed by Tsym/2" representation at
    // 2 sps).
    let i_vals = [RAIL_AMP, -RAIL_AMP, RAIL_AMP, -RAIL_AMP];
    let q_vals = [RAIL_AMP, RAIL_AMP, -RAIL_AMP, -RAIL_AMP];
    let buf: Vec<Complex> = (0..SAMPLES_1S)
        .map(|n| {
            let sym_idx = (n / SAMPLES_PER_SYMBOL) % PATTERN_LEN;
            if n % SAMPLES_PER_SYMBOL == 0 {
                Complex::new(i_vals[sym_idx], 0.0)
            } else {
                Complex::new(0.0, q_vals[sym_idx])
            }
        })
        .collect();

    c.bench_function("lrpt_demod_oqpsk_1s_144ksps", |b| {
        b.iter(|| {
            let mut demod =
                LrptDemod::new_with_mode(LrptMode::Oqpsk).expect("LrptDemod::new_with_mode");
            let mut emitted = 0_u32;
            for s in &buf {
                if demod.process(black_box(*s)).is_some() {
                    emitted += 1;
                }
            }
            black_box(emitted);
        });
    });
}

criterion_group!(benches, bench_demod_qpsk, bench_demod_oqpsk);
criterion_main!(benches);
