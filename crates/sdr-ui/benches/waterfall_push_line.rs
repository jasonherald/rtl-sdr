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
//!
//! `sdr-ui` is Linux-only (the GTK4 frontend), so the bench body
//! only compiles on Linux. Each top-level item is gated with
//! `#[cfg(target_os = "linux")]` so:
//!
//! - On Linux: `criterion_main!` expands to a crate-level `fn main`
//!   and runs the bench normally.
//! - On non-Linux targets: every Linux-only item is skipped, and
//!   the no-op `fn main()` at the bottom satisfies the bench
//!   harness so a workspace clippy / build pass succeeds.
//!
//! **Why per-item gates instead of a `linux_bench` module wrapper:**
//! `criterion_main!` invoked inside a module expands to a function
//! at module scope (`linux_bench::main`), NOT to a crate-level
//! `main` — Linux CI fails with "main function not found" because
//! there's still no `main` at the crate root. Flat per-item gates
//! keep the macro invocation at file scope where its expansion
//! lands at the crate root.

#[cfg(target_os = "linux")]
use std::hint::black_box;

#[cfg(target_os = "linux")]
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
#[cfg(target_os = "linux")]
use sdr_ui::spectrum::waterfall::WaterfallRenderer;

/// FFT size we feed into the waterfall — the same high-resolution
/// size the rest of the FFT bench family measures.
#[cfg(target_os = "linux")]
const FFT_SIZE: usize = 65_536;

/// Display widths under test. The history height is fixed by the
/// renderer at 1024 rows; width is the knob the user's monitor
/// drives.
#[cfg(target_os = "linux")]
const DISPLAY_WIDTHS: &[usize] = &[800, 1920, 4096];

/// Phase step for the synthetic dB input. Doesn't affect cost —
/// the hot path is data-independent (clamp + linear map +
/// colormap-array indexing).
#[cfg(target_os = "linux")]
const PHASE_STEP_RAD: f32 = 0.01;

/// Realistic min/max dB range — matches `DEFAULT_MIN_DB` /
/// `DEFAULT_MAX_DB` in `waterfall.rs`.
#[cfg(target_os = "linux")]
const DB_MIN: f32 = -70.0;
#[cfg(target_os = "linux")]
const DB_MAX: f32 = 0.0;

/// dB padding extended below `DB_MIN` for the synthetic input.
/// Pushes the early bins below the floor so the normalization
/// hits the lower clamp branch — measuring the real cost of the
/// branch-prediction patterns the renderer sees in production.
#[cfg(target_os = "linux")]
const DB_BELOW_FLOOR_PAD: f32 = 10.0;
/// dB padding extended above `DB_MAX` for the synthetic input.
/// Pushes the late bins above the ceiling so the upper clamp
/// branch fires too. Twice the below-floor pad on purpose: the
/// upper edge clamp is the hotter branch in real-world traffic
/// (squelch-open events sit near peak), so weighting the test
/// toward it keeps the bench representative.
#[cfg(target_os = "linux")]
const DB_ABOVE_CEILING_PAD: f32 = 20.0;
/// Amplitude (dB) of the sinusoidal wobble layered on top of the
/// linear sweep. Just enough to keep the optimizer from hoisting
/// the input as a constant — content of the wobble is irrelevant
/// to bench cost.
#[cfg(target_os = "linux")]
const SWEEP_WOBBLE_DB: f32 = 3.0;

#[cfg(target_os = "linux")]
fn make_synthetic_fft(bins: usize) -> Vec<f32> {
    // Sweep from below-floor to above-ceiling so the normalization
    // hits every clamp branch. Bench cost is clamp-branch-
    // dependent only via prediction, so mix both directions.
    #[allow(clippy::cast_precision_loss)]
    let bins_f = bins as f32;
    let sweep_total_db = DB_MAX - DB_MIN + DB_BELOW_FLOOR_PAD + DB_ABOVE_CEILING_PAD;
    (0..bins)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32;
            let sweep_db = DB_MIN - DB_BELOW_FLOOR_PAD + ((t / bins_f) * sweep_total_db);
            // Throw in a sinusoidal wobble so the compiler can't
            // hoist anything as constant.
            sweep_db + (t * PHASE_STEP_RAD).sin() * SWEEP_WOBBLE_DB
        })
        .collect()
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
criterion_group!(benches, bench_push_line);
#[cfg(target_os = "linux")]
criterion_main!(benches);

// Non-Linux stub: keeps cargo's bench harness happy without
// pulling in the GTK / criterion graph that doesn't compile here.
// The Linux `criterion_main!` above provides the real `main` at
// the crate root when building on Linux, so this stub is gated to
// other targets.
#[cfg(not(target_os = "linux"))]
fn main() {}
