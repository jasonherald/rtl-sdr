//! LRPT stage-1 demod throughput bench (epic #469).
//!
//! Measures the end-to-end demod chain on 1 second of synthetic
//! 144 ksps QPSK input. Establishes the perf floor for stage-1
//! regression detection.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sdr_dsp::lrpt::LrptDemod;
use sdr_types::Complex;

/// 1 second of input at the demod's 144 ksps working sample rate.
const SAMPLES_1S: usize = 144_000;

fn bench_demod(c: &mut Criterion) {
    let symbols = [
        Complex::new(0.707, 0.707),
        Complex::new(-0.707, 0.707),
        Complex::new(0.707, -0.707),
        Complex::new(-0.707, -0.707),
    ];
    let buf: Vec<Complex> = (0..SAMPLES_1S)
        .map(|n| {
            if n % 2 == 0 {
                symbols[(n / 2) % 4]
            } else {
                Complex::new(0.0, 0.0)
            }
        })
        .collect();

    c.bench_function("lrpt_demod_1s_144ksps", |b| {
        b.iter(|| {
            let mut demod = LrptDemod::new().expect("LrptDemod::new");
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

criterion_group!(benches, bench_demod);
criterion_main!(benches);
