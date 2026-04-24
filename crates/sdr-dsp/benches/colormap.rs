//! Baseline waterfall colormap throughput — the CPU path
//! equivalent of "normalize a dB line to 0..1 and look up the
//! palette" that runs once per FFT frame (epic #452 phase 1 /
//! #180 phase 3a).
//!
//! **Scope caveat.** The production colormap lives in
//! `crates/sdr-ui/src/spectrum/colormap.rs` (not in `sdr-dsp`)
//! because it's a render-side concern. Duplicating the 256-entry
//! palette generation + lookup here inline keeps the baseline
//! self-contained without pulling `sdr-ui` into `sdr-dsp`'s dev
//! graph. The GPU phase (#180) compares against a CPU path with
//! the same shape, not against this exact code — the measurement
//! surface is "one f32 → one [u8; 4] per bin".
//!
//! **Measurement discipline.** Palette + input dB vector
//! allocated once outside the closure; only the normalize +
//! lookup + write loop runs inside.

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};

/// Typical waterfall width at high FFT resolution — matches
/// the #180 spec's 65536-bin case.
const BINS: usize = 65_536;

/// Palette resolution — 256 entries is the standard 1-D texture
/// size every colormap picker in the app uses.
const PALETTE_ENTRIES: usize = 256;

/// dB range the picker normalizes into. Matches the
/// `min_db`/`max_db` clamp the spectrum view ships with today
/// (`display_panel.rs` → `DEFAULT_MIN_DB = -70.0`,
/// `DEFAULT_MAX_DB = 0.0`).
const DB_FLOOR: f32 = -70.0;
const DB_CEILING: f32 = 0.0;

/// Max channel byte value (fully-lit red / fully-opaque alpha).
const PALETTE_CHANNEL_MAX: u8 = 255;
/// Divisor applied to the blue channel so the synthetic ramp
/// produces three non-identical colour curves (otherwise the
/// compiler could theoretically fold the `[u8; 4]` build down).
const PALETTE_BLUE_DIVISOR: u8 = 2;

/// Low end of the synthetic dB sweep — below `DB_FLOOR` so the
/// clamp's saturate-low branch actually fires on some bins.
const SWEEP_DB_FLOOR: f32 = -90.0;
/// Span of the sweep. Paired with `SWEEP_DB_FLOOR = -90.0` this
/// takes the ramp up to +5 dB, above `DB_CEILING = 0.0`, so the
/// saturate-high branch also fires.
const SWEEP_DB_RANGE: f32 = 95.0;

/// Build a synthetic 256-entry palette. The real picker uses
/// Turbo / Viridis / Plasma / Inferno; the lookup cost is
/// identical regardless of the palette's colour ramp, so a
/// synthetic gradient keeps the benchmark independent of the
/// `sdr-ui` crate.
fn synthetic_palette() -> Vec<[u8; 4]> {
    (0..PALETTE_ENTRIES)
        .map(|i| {
            #[allow(clippy::cast_possible_truncation)]
            let byte = i as u8;
            [
                byte,
                PALETTE_CHANNEL_MAX - byte,
                byte / PALETTE_BLUE_DIVISOR,
                PALETTE_CHANNEL_MAX,
            ]
        })
        .collect()
}

/// Fill a vector with a plausible FFT power-in-dB line. Real
/// input has more structure (carrier + noise floor); this
/// produces equivalent per-bin work without depending on the
/// FFT path.
fn synthetic_db_line(bins: usize) -> Vec<f32> {
    (0..bins)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let x = i as f32 / bins as f32;
            // Sweep from `SWEEP_DB_FLOOR` to
            // `SWEEP_DB_FLOOR + SWEEP_DB_RANGE` linearly — covers
            // the saturate-low and saturate-high branches of the
            // clamp plus the linear-interior hot path.
            SWEEP_DB_FLOOR + x * SWEEP_DB_RANGE
        })
        .collect()
}

/// The actual hot path: for each bin, clamp into
/// `[DB_FLOOR, DB_CEILING]`, normalize to `[0, 1]`, index the
/// palette, write the RGBA pixel. Mirrors the CPU code in
/// `sdr-ui::spectrum::waterfall::push_line` closely enough that
/// the GPU ticket has a fair comparison surface.
#[inline]
fn apply_colormap(db_line: &[f32], palette: &[[u8; 4]], out: &mut [[u8; 4]]) {
    debug_assert_eq!(db_line.len(), out.len());
    let range = DB_CEILING - DB_FLOOR;
    #[allow(clippy::cast_precision_loss)]
    let scale = (palette.len() - 1) as f32;
    for (src, dst) in db_line.iter().zip(out.iter_mut()) {
        let clamped = src.clamp(DB_FLOOR, DB_CEILING);
        let norm = (clamped - DB_FLOOR) / range;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = (norm * scale).round() as usize;
        *dst = palette[idx.min(palette.len() - 1)];
    }
}

fn bench_colormap(c: &mut Criterion) {
    let palette = synthetic_palette();
    let db_line = synthetic_db_line(BINS);

    let mut group = c.benchmark_group("colormap_lookup_cpu");
    group.throughput(Throughput::Elements(BINS as u64));
    group.bench_function(format!("bins={BINS}"), |b| {
        let mut output = vec![[0_u8; 4]; BINS];
        b.iter_batched(
            || db_line.clone(),
            |line| {
                apply_colormap(&line, &palette, &mut output);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_colormap);
criterion_main!(benches);
