# Epic #452 Phase 3a — scope decision for GPU waterfall

Written 2026-04-24, after the CPU investigation PR (#457) merged and
recommended Phase 3a as the next direction. This doc measures the
current Cairo waterfall's CPU cost so the scope of Phase 3a can be
chosen against data rather than guesswork.

## TL;DR

- **At 1920-wide displays (full HD) the waterfall costs 0.89% of one core** —
  borderline for a major refactor.
- **At 4096-wide displays (4K, `MAX_TEXTURE_WIDTH`-capped) it costs
  3.77% of one core** — clearly worth doing.
- Scaling is super-linear with display width (800→4096 is 5.1× in
  width but 7.1× in time), confirming the memcpy in the ring-buffer
  shift-down is the dominant cost at larger widths.
- **Recommendation: proceed with Phase 3a as the large-scope
  GtkGLArea + wgpu-texture-ring-buffer port**, scoped as its own
  epic with sub-tickets for the integration work. This is a
  real user-visible improvement at 4K resolutions.

## Context

Phase 3a as #180 was originally written as "GPU colormap compute
with CPU readback into a Cairo `ImageSurface`". The ticket itself
warned that this might not be a net win because the readback cost
could exceed the compute savings. The CPU investigation PR (#457)
made that more concrete: the real architectural win is
eliminating the CPU's role in the waterfall pixel path entirely —
FFT output stays on GPU, a compute pass converts dB→RGBA into a
GPU texture, and the render pipeline samples that texture. No CPU
readback, no CPU memcpy, no Cairo `ImageSurface` construction.

But that win assumes the waterfall renders through `GtkGLArea`,
not Cairo `DrawingArea`. The current code is Cairo. Porting is a
substantial architectural change — worth doing only if the CPU
cost it eliminates is actually significant.

This doc measures that cost.

## The hot path

`WaterfallRenderer::push_line` is called once per FFT display
frame (typically 60 Hz). It:

1. Downsamples the FFT to `display_width` (max 4096).
2. Normalizes dB → 0..255 per bin.
3. **Shifts every existing row down by one via `copy_within`** —
   a `display_width · (HISTORY_LINES-1) · 4`-byte memcpy.
4. Writes the new top row via colormap lookup.

Step 3 is the suspected hot spot. At a 1920-wide display with
1024 history rows, that's ~7.9 MB copied per frame; at 4096-wide,
~16.8 MB.

## Measurements

New bench `crates/sdr-ui/benches/waterfall_push_line.rs`.
Input: synthetic 65536-bin FFT frame (the high-resolution waterfall case).
Median, release build, Zen 4 / DDR5:

| Display width | `push_line` time | At 60 fps | % of 1 core | Memcpy / frame |
|---------------|------------------|-----------|-------------|----------------|
| 800           | 88.8 µs          | 5.3 ms/sec  | **0.53%**   | 3.3 MB         |
| 1920 (Full HD) | 148.7 µs         | 8.9 ms/sec  | **0.89%**   | 7.9 MB         |
| 4096 (4K cap)  | 628.7 µs         | 37.7 ms/sec | **3.77%**   | 16.8 MB        |

**Scaling analysis.** Width doubles from 1920 to 4096 (2.13×), but
time increases 4.23×. If memcpy were the only cost we'd expect
linear (2.13×). The super-linear scaling at 4K suggests cache
effects — the 16 MB pixel_buf no longer fits in L2/L3 on the Zen 4
(Ryzen 9 PRO 8945HS has 16 MB L3 shared across cores), so memcpy
falls to DRAM bandwidth with higher latency per byte.

**Implication.** At 4K, the waterfall alone uses ~1 GB/sec of
memory bandwidth (16.8 MB × 60 fps) — a noticeable share of system
memory traffic, in addition to the 3.77% CPU time.

**Not measured here:** the Cairo `render()` call (also 60 Hz) adds
additional cost on top — `ImageSurface::create_for_data` + `paint`
+ scaling. Cairo's blit is hardware-accelerated where possible but
not free. The `push_line` numbers are a lower bound on the
waterfall's total per-frame CPU cost.

## Phase 3a scope options

### Option 1: Ship #180 as written (small scope)

GPU compute shader does dB normalize + colormap lookup, reads
back to CPU, writes into the existing Cairo `ImageSurface`.
Preserves the Cairo/`DrawingArea` architecture.

**Saves:** steps 2 + 4 of `push_line` (normalize + colormap write).
These are the cheap parts — by inspection, maybe 10-20 µs of the
148 µs at 1920 width.

**Doesn't save:** the memcpy (step 3) or the Cairo blit. Adds
GPU readback latency (~100 µs per Phase 2c measurement).

**Verdict: likely net regression** or break-even. The ticket's own
warning was accurate.

### Option 2: Ring-buffer + Cairo (intermediate scope)

Replace the shift-down `copy_within` with a ring-buffer pattern
(logical top_row pointer, wrap around). Cairo renders two regions
per frame with a clip + translate between them.

**Saves:** step 3 memcpy entirely. `push_line` drops to ~20-50 µs
at 4K (just normalize + colormap-write for one row).

**Adds:** render complexity (two-region blit per frame), but
render is only 60 Hz and each region is simpler than a single
ring-buffer copy.

**Verdict: maybe 2-3× speedup at 4K**, no wgpu dependency needed,
moderate-scope refactor. Could ship as a standalone PR without
touching the GPU work at all.

### Option 3: Full GtkGLArea + wgpu port (large scope)

Replace the Cairo `DrawingArea` with `GtkGLArea`. Share wgpu device
state with the GL context. Waterfall becomes a GPU texture ring
buffer with a compute pass writing new rows and a render pass
sampling the texture.

**Saves:** the entire CPU waterfall path — `push_line` becomes
"dispatch a 64-thread compute pass to write one row", which costs
microseconds. Frees the ~3.77% of a core at 4K AND eliminates the
~1 GB/sec memory bandwidth consumption. Sets up Phase 3b (GPU
signal-history render) and Phase 4 (GPU FIR) to feed into the same
GPU context.

**Adds:** substantial architectural work:

- wgpu → `GtkGLArea` GL interop (wgpu 29 does have
  `Instance::create_surface_from_gl_egl` / similar helpers — needs
  verification for GTK4 integration specifically)
- New shader: dB normalize + colormap lookup writing to a texture
- Render path: replace Cairo paint with GL texture sample
- Signal-history / FFT plot likely also need porting to stay
  visually consistent (currently they're Cairo too — see
  `fft_plot.rs`, `signal_history.rs`)
- Testing surface: GL context lifecycle, GTK + wgpu thread safety,
  dpi scaling, display-hotplug

**Verdict: the real architectural win**, but it's plausibly a 3–5
PR epic of its own, not one ticket. Worth doing only if 4K
resolution is a first-class target for the app.

## Recommendation

**Option 3 with explicit multi-ticket scoping.**

The data supports it — at 4K, 3.77% of a core + 1 GB/sec memory
bandwidth is real user-visible cost, especially on laptops where
power and thermal headroom matter. The Cairo renderer is not
going to get faster; this cost only grows as higher-DPI displays
become more common.

File a new epic (or expand #452) with sub-tickets:

1. **wgpu ↔ GTK4 GL interop** — prove the rendering surface works.
   Minimum viable: a blank wgpu-rendered `GtkGLArea` widget
   alongside the existing Cairo waterfall.
2. **Waterfall compute shader** — dB normalize + colormap lookup,
   writing to a GPU texture.
3. **Waterfall ring-buffer texture** — advance write-index
   semantics, tile-based render from any offset.
4. **FFT plot port** — replace the Cairo line-plot with a GL
   vertex pipeline. Likely small, rides the same GL context.
5. **Signal-history port** — same as #4.
6. **Retire Cairo for the spectrum pane** — delete the CPU
   renderers and their pixel buffers.

Phase 2c's `forward_no_readback` API is the bridge — the FFT
output buffer can feed directly into the waterfall compute shader
via wgpu buffer binding, no CPU round-trip.

### Not recommended: Options 1 or 2 as the final answer

Option 1 is known-to-be-worthless per the ticket's own warning
(confirmed by our measurements of readback cost in Phase 2c).

Option 2 is a partial fix that complicates the render path
without getting the full win. If we were committed to keeping
Cairo forever, it'd be worth doing. But if Option 3 is the
eventual direction, Option 2 is code we'd throw away.

## Non-goals for Phase 3a

- **Maintain Cairo rendering as a fallback.** If Option 3 is the
  direction, commit to it. The rendering stack split would double
  the maintenance surface for no user benefit.
- **Parallel execution with existing waterfall.** Pick one
  renderer per release; don't ship a "GPU beta mode" flag that
  users have to opt into.

## Open questions (to answer before writing the epic)

- Does wgpu 29's GTK4 `GtkGLArea` integration actually work on
  Wayland today? Needs a proof-of-concept before committing.
- Cross-platform implications — the macOS port is separate
  (`sdr-core` + `sdr-ffi` + SwiftUI frontend per epic #441); a
  GPU waterfall on Linux doesn't automatically port. Either the
  macOS path keeps its own (Metal-based?) renderer, or we split
  the rendering into a cross-platform wgpu layer and a
  platform-specific windowing layer.
- What minimum wgpu feature set do we require? `TEXTURE_BINDING_ARRAY`
  is not needed, basic storage-buffer + texture-sampling is the
  whole dependency.
