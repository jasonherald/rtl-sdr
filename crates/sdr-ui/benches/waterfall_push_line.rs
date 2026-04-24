//! CPU cost of `WaterfallRenderer::push_line` at representative
//! display sizes (epic #452 Phase 3a scope investigation).
//!
//! The current waterfall is Cairo/`DrawingArea`-based. Each new
//! FFT frame calls `push_line`, which:
//!
//! 1. Downsamples the FFT to `display_width`.
//! 2. Normalizes dB values to 0..255.
//! 3. **Shifts every existing row down by one** (the whole
//!    `pixel_buf` `copy_within`) — a `display_width · (HISTORY_LINES-1) · 4`-byte
//!    memcpy per frame.
//! 4. Writes the new top row with colormap lookup.
//!
//! Step 3 is the suspected hot spot — at a 1920-wide display with
//! 1024 history rows, that's ~7.5 MB copied per frame, and we call
//! this at the display FFT rate (typically 60 Hz). A GPU
//! texture-ring-buffer architecture would eliminate this memcpy
//! entirely by just advancing a write index.
//!
//! This bench measures the real CPU cost so the decision about
//! porting the waterfall to `GtkGLArea` + wgpu rendering can be
//! made against data, not theory.
//!
//! Shapes under test mirror real user configurations:
//! - **800×1024**  — small window (laptop side-panel width)
//! - **1920×1024** — full-HD primary display
//! - **4096×1024** — `MAX_TEXTURE_WIDTH` capped (4K+ displays,
//!   external monitors)
//!
//! Input FFT size is held at 65536 — the high-resolution waterfall
//! case, where `push_line` does real downsample work.

#![cfg(target_os = "linux")]

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use sdr_ui::spectrum::waterfall::WaterfallRenderer;

/// FFT size we feed into the waterfall — the same high-resolution
/// size the rest of the FFT bench family measures.
const FFT_SIZE: usize = 65_536;

/// Display widths under test. The history height is fixed by the
/// renderer at 1024 rows; width is the knob the user's monitor
/// drives.
const DISPLAY_WIDTHS: &[usize] = &[800, 1920, 4096];

/// Phase step for the synthetic dB input. Doesn't affect cost —
/// the hot path is data-independent (clamp + linear map +
/// colormap-array indexing).
const PHASE_STEP_RAD: f32 = 0.01;

/// Realistic min/max dB range — matches `DEFAULT_MIN_DB` /
/// `DEFAULT_MAX_DB` in `waterfall.rs`.
const DB_MIN: f32 = -70.0;
const DB_MAX: f32 = 0.0;

fn make_synthetic_fft(bins: usize) -> Vec<f32> {
    // Sweep from below-floor to above-ceiling so the normalization
    // hits every clamp branch. Bench cost is clamp-branch-
    // dependent only via prediction, so mix both directions.
    #[allow(clippy::cast_precision_loss)]
    let bins_f = bins as f32;
    (0..bins)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32;
            let sweep_db = DB_MIN - 10.0 + ((t / bins_f) * (DB_MAX - DB_MIN + 20.0));
            // Throw in a sinusoidal wobble so the compiler can't
            // hoist anything as constant.
            sweep_db + (t * PHASE_STEP_RAD).sin() * 3.0
        })
        .collect()
}

fn bench_push_line(c: &mut Criterion) {
    let mut group = c.benchmark_group("waterfall_push_line");
    let input = make_synthetic_fft(FFT_SIZE);
    group.throughput(Throughput::Elements(FFT_SIZE as u64));

    for &width in DISPLAY_WIDTHS {
        // Engine owns its pixel buffer, colormap, downsample scratch
        // — all pre-allocated in `new()`. Only `push_line` runs
        // inside the measured closure.
        let mut renderer = WaterfallRenderer::new(width);
        group.bench_function(format!("display_width={width}_fft_bins={FFT_SIZE}"), |b| {
            b.iter_batched(
                || black_box(input.clone()),
                |fft_frame| {
                    renderer.push_line(&fft_frame);
                    black_box(&renderer);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_push_line);
criterion_main!(benches);
