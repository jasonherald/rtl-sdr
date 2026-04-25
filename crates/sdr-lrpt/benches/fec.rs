//! FEC stage-2a throughput benches (epic #469).
//!
//! Measures Viterbi (10k bits), sync correlator (1M bits), and
//! derandomizer (1MB) on synthetic fixtures. Sets the per-stage
//! perf floor for regression detection.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sdr_lrpt::fec::{Derandomizer, SyncCorrelator, ViterbiDecoder};

/// Number of encoded bit pairs to drive through Viterbi per
/// iteration. Slightly above one CADU's worth of soft input.
const VITERBI_BIT_PAIRS: usize = 10_000;

/// Number of bits to drive through the sync correlator per
/// iteration. Realistic order-of-magnitude for one Meteor pass
/// at 72 ksym/s × ~12 min (≈ 50 M bits, scaled to a benchable
/// chunk).
const SYNC_BITS: usize = 1_000_000;

/// Number of bytes to drive through the derandomizer per
/// iteration. ~1MB matches the post-RS frame stream produced by
/// a single Meteor pass after CADU framing strips overhead.
const DERAND_BYTES: usize = 1_000_000;

/// Soft-symbol amplitude for the synthetic Viterbi bench input.
/// Slightly under the ±127 saturation point so the bench
/// exercises the typical "clean signal but not slammed against
/// the rails" branch metric path the production demod produces.
const SOFT_POS: i8 = 100;
const SOFT_NEG: i8 = -100;

fn bench_viterbi(c: &mut Criterion) {
    let symbols: Vec<i8> = (0..VITERBI_BIT_PAIRS * 2)
        .map(|n| if n & 1 == 0 { SOFT_POS } else { SOFT_NEG })
        .collect();
    c.bench_function("viterbi_10k_bit_pairs", |b| {
        b.iter(|| {
            let mut dec = ViterbiDecoder::new();
            let mut count = 0_u32;
            for chunk in symbols.chunks_exact(2) {
                if dec.step([chunk[0], chunk[1]]).is_some() {
                    count += 1;
                }
            }
            black_box(count);
        });
    });
}

fn bench_sync(c: &mut Criterion) {
    #[allow(clippy::cast_possible_truncation, reason = "n & 1 always fits in u8")]
    let bits: Vec<u8> = (0..SYNC_BITS).map(|n| (n & 1) as u8).collect();
    c.bench_function("sync_1M_bits", |b| {
        b.iter(|| {
            let mut s = SyncCorrelator::new();
            let mut hits = 0_u32;
            for &bit in &bits {
                if s.push(black_box(bit)).is_some() {
                    hits += 1;
                }
            }
            black_box(hits);
        });
    });
}

fn bench_derand(c: &mut Criterion) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "n & 0xFF always fits in u8"
    )]
    let bytes: Vec<u8> = (0..DERAND_BYTES).map(|n| (n & 0xFF) as u8).collect();
    c.bench_function("derand_1MB", |b| {
        b.iter(|| {
            let mut d = Derandomizer::new();
            let mut sum = 0_u64;
            for &b in &bytes {
                sum = sum.wrapping_add(u64::from(d.process(black_box(b))));
            }
            black_box(sum);
        });
    });
}

criterion_group!(benches, bench_viterbi, bench_sync, bench_derand);
criterion_main!(benches);
