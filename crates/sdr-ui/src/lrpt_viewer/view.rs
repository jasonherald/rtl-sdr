//! The GTK widget half of the LRPT viewer (issue #819):
//! [`LrptImageView`] wraps a `DrawingArea` + renderer + the
//! shared [`LrptImage`] handle, drains new scan lines on a poll
//! tick, and owns the off-thread composite build and export
//! snapshots. Split out of `lrpt_viewer.rs` per the file-size
//! pass.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gio, glib};

use sdr_lrpt::image::IMAGE_WIDTH;
use sdr_radio::lrpt_image::LrptImage;

use super::POLL_INTERVAL_MS;
use super::composite::{
    CompositeRecipe, build_argb32_surface_from_bgra, build_bgra_composite_bytes,
};
use super::export::ExportSnapshot;
use super::renderer::{LrptImageRenderer, PushOutcome};
use crate::viewer::ViewerError;

// ─── GTK widget ────────────────────────────────────────────────────────

/// Live Meteor LRPT image viewer widget.
///
/// Holds a `DrawingArea`, a renderer, the shared
/// [`LrptImage`] handle the DSP thread is writing to, and a
/// poll-tick `glib` source. The poll tick drains any new scan
/// lines from the shared image into the renderer and queues a
/// redraw.
///
/// `Clone` is derived (existing pattern) so toolbar callbacks
/// and the channel dropdown can hold their own handles. Every
/// field is internally `Rc`-shared, so cloning is a refcount
/// bump.
#[derive(Clone)]
pub struct LrptImageView {
    drawing_area: gtk4::DrawingArea,
    renderer: Rc<RefCell<LrptImageRenderer>>,
    image: LrptImage,
    paused: Rc<Cell<bool>>,
    /// Per-APID watermark: how many lines have already been
    /// pulled from the shared image into the renderer. Mirrors
    /// the watermark map in the DSP-side `LrptDecoder` — both
    /// sides need it so the same line isn't pushed twice (and
    /// so the viewer's poll tick is O(new lines), not O(total
    /// lines)).
    last_seen_lines: Rc<RefCell<HashMap<u16, usize>>>,
    /// `glib` source IDs of timeouts spawned by the view (the
    /// drain tick) and by `open_lrpt_viewer_window` (the
    /// channel-dropdown refresh tick). [`Self::shutdown`]
    /// removes them all so the closures' `Rc` chains drop and
    /// the view + ~51 MB-per-channel surfaces don't leak past
    /// the window's close-request. Per `CodeRabbit` round 1 on
    /// PR #543.
    timeout_ids: Rc<RefCell<Vec<glib::SourceId>>>,
}

impl LrptImageView {
    /// Build a view bound to the given shared image. Spawns a
    /// poll tick on the GTK main context that drains new lines
    /// every [`POLL_INTERVAL_MS`].
    #[must_use]
    pub fn new(image: LrptImage) -> Self {
        let renderer = Rc::new(RefCell::new(LrptImageRenderer::new()));
        let paused = Rc::new(Cell::new(false));
        let last_seen_lines: Rc<RefCell<HashMap<u16, usize>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let timeout_ids: Rc<RefCell<Vec<glib::SourceId>>> = Rc::new(RefCell::new(Vec::new()));

        let drawing_area = gtk4::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        let renderer_for_draw = Rc::clone(&renderer);
        drawing_area.set_draw_func(move |_area, cr, w, h| {
            if let Err(e) = renderer_for_draw.borrow().render(cr, w, h) {
                tracing::warn!("LRPT render failed: {e}");
            }
        });

        let view = Self {
            drawing_area,
            renderer,
            image,
            paused,
            last_seen_lines,
            timeout_ids,
        };

        // Poll tick: drain new lines + queue redraw on change.
        let view_for_tick = view.clone();
        let drain_id = glib::timeout_add_local(
            std::time::Duration::from_millis(u64::from(POLL_INTERVAL_MS)),
            move || {
                view_for_tick.drain_new_lines();
                glib::ControlFlow::Continue
            },
        );
        view.timeout_ids.borrow_mut().push(drain_id);

        view
    }

    /// Register an external `glib` source (e.g. the
    /// channel-dropdown refresh tick spawned by
    /// [`open_lrpt_viewer_window`]) so it gets cleaned up by
    /// [`Self::shutdown`] alongside the internal drain tick.
    pub fn register_source(&self, id: glib::SourceId) {
        self.timeout_ids.borrow_mut().push(id);
    }

    /// Cancel every registered `glib` source. Called on the
    /// viewer window's `close-request` so the timeout closures
    /// drop their `Rc` clones of the view's inner state — without
    /// this, the view + ~51 MB-per-channel surfaces stay alive in
    /// the main context until the application exits. Safe to
    /// call more than once (subsequent calls are no-ops because
    /// the `Vec` is drained).
    pub fn shutdown(&self) {
        for id in std::mem::take(&mut *self.timeout_ids.borrow_mut()) {
            id.remove();
        }
    }

    /// The underlying `GtkDrawingArea`. Pack into a layout
    /// container, wrap in a `ScrolledWindow`, etc.
    #[must_use]
    pub fn drawing_area(&self) -> &gtk4::DrawingArea {
        &self.drawing_area
    }

    /// All APIDs the renderer has seen at least one line for.
    /// Wraps the renderer's `known_apids` for callers that hold
    /// only a `LrptImageView` (the dropdown updater).
    #[must_use]
    pub fn known_apids(&self) -> Vec<u16> {
        self.renderer.borrow().known_apids()
    }

    /// Switch which APID's channel is displayed. Returns `false`
    /// (no-op) if the renderer has never seen a line for that
    /// APID — see [`LrptImageRenderer::set_active_apid`] for
    /// rationale. Queues a redraw on success.
    pub fn set_active_apid(&self, apid: u16) -> bool {
        let ok = self.renderer.borrow_mut().set_active_apid(apid);
        if ok {
            self.drawing_area.queue_draw();
        }
        ok
    }

    /// Currently-displayed APID, if any.
    #[must_use]
    pub fn active_apid(&self) -> Option<u16> {
        self.renderer.borrow().active_apid()
    }

    /// Switch the viewer to composite mode for `recipe` and
    /// kick off an off-thread rebuild of the cached ARGB32
    /// surface. Returns `true` if the snapshot succeeded (all
    /// three source APIDs have data — the worker will install
    /// the cache when it returns); `false` if the snapshot
    /// failed (one or more source APIDs missing/empty). The
    /// canvas always queues a redraw immediately so background-
    /// vs-stale-pixels stays consistent.
    ///
    /// Two-phase work split:
    ///
    /// - **GTK main thread (synchronous, fast):** clone the
    ///   three source channel buffers under the assembler lock
    ///   (3 memcpy of `IMAGE_WIDTH * height` bytes), then mark
    ///   the renderer's selection as "composite, build for
    ///   `snap.height` in flight" and capture the generation
    ///   token. Tens of milliseconds at most, even on a full
    ///   pass.
    /// - **Worker thread (`gio::spawn_blocking`):** per-pixel
    ///   R/G/B interleave (`assemble_rgb_composite`) + ARGB32
    ///   surface build (`build_argb32_from_rgb`). Tens of
    ///   milliseconds on a full pass — that's what we're
    ///   moving off the GTK main loop. Returns the freshly-
    ///   built `cairo::ImageSurface`.
    /// - **Main thread (post-await):** if the renderer's
    ///   generation still matches the captured token (no
    ///   selection change mid-flight), call
    ///   [`LrptImageRenderer::install_composite_cache`] to
    ///   adopt the surface and queue a redraw. Otherwise drop
    ///   the surface; the user has moved on.
    ///
    /// Per CR round 5 on PR #575: previously the per-pixel
    /// interleave + Cairo surface build ran synchronously on
    /// the GTK main thread on every dropdown click + every
    /// 1 Hz refresh tick. Composite mode could hitch the UI on
    /// a full pass.
    pub fn set_composite(&self, recipe: CompositeRecipe) -> bool {
        // Phase 1 — under the assembler lock: snapshot the
        // three source channels into owned `Vec<u8>`s. Lock
        // released as soon as the closure returns.
        let snap = self.image.with_assembler(|a| {
            a.clone_channels_for_composite(recipe.r_apid, recipe.g_apid, recipe.b_apid)
        });
        let Some(snap) = snap else {
            tracing::debug!(
                ?recipe,
                "clone_channels_for_composite returned None — one or more source APIDs missing or empty",
            );
            self.renderer.borrow_mut().mark_composite_pending(recipe);
            self.drawing_area.queue_draw();
            return false;
        };
        // Pin the in-flight build height in the renderer so
        // the 1 Hz refresh-tick gate (`cached_min_height ==
        // current_min_height`) doesn't kick off a duplicate
        // worker for the same height while this one runs.
        // Capture the generation token at the same time;
        // mismatch on completion = stale = drop on the floor.
        let target_height = snap.height;
        let captured_gen = self
            .renderer
            .borrow_mut()
            .prepare_composite_build(recipe, target_height);
        // Always queue a redraw — even before the worker
        // returns, the background paint covers any previous
        // render's pixels so the user sees the canvas reset
        // rather than stale composite data hanging around
        // until the new surface lands.
        self.drawing_area.queue_draw();
        // Phase 2 — off the main thread: assemble RGB +
        // ARGB32. Phase 3 (post-await) runs back on the main
        // thread.
        let renderer = Rc::clone(&self.renderer);
        let drawing_area = self.drawing_area.clone();
        glib::spawn_future_local(async move {
            // Worker: pure CPU. Reads three source channels'
            // owned `Vec<u8>`s from the captured `snap` (no
            // mutex), packs into a flat BGRA `Vec<u8>` ready
            // for Cairo. Returns the bytes — `cairo::ImageSurface`
            // isn't `Send` so the surface itself is built on
            // the main thread post-await.
            let bytes_result = gio::spawn_blocking(move || build_bgra_composite_bytes(&snap)).await;
            // Main thread: wrap the worker's BGRA bytes in a
            // Cairo surface via `create_for_data`. Cheap (no
            // memcpy in the common stride case) — the heavy
            // per-pixel pack already ran on the worker.
            finish_composite_build(
                &renderer,
                &drawing_area,
                recipe,
                captured_gen,
                target_height,
                bytes_result,
            );
        });
        true
    }

    /// Drop composite mode; subsequent renders fall back to the
    /// active per-APID channel. Queues a redraw so the canvas
    /// updates immediately. Per #547.
    pub fn clear_composite(&self) {
        self.renderer.borrow_mut().clear_composite();
        self.drawing_area.queue_draw();
    }

    /// `true` when composite mode is currently active.
    #[must_use]
    pub fn is_composite_active(&self) -> bool {
        self.renderer.borrow().is_composite_active()
    }

    /// The active composite recipe, if any.
    #[must_use]
    pub fn active_composite(&self) -> Option<CompositeRecipe> {
        self.renderer.borrow().active_composite()
    }

    /// `min(r_lines, g_lines, b_lines)` for `recipe` queried
    /// against the live shared image *right now*. Returns `None`
    /// if any source APID is missing or empty — matches
    /// [`sdr_lrpt::image::ImageAssembler::clone_channels_for_composite`]'s
    /// contract so the dropdown-refresh tick can compare this
    /// directly to [`Self::cached_composite_min_height`] and skip
    /// a no-op rebuild. Per CR round 3 on PR #575.
    #[must_use]
    pub fn current_composite_min_height(&self, recipe: CompositeRecipe) -> Option<usize> {
        self.image.with_assembler(|a| {
            let r = a.channel(recipe.r_apid)?.lines;
            let g = a.channel(recipe.g_apid)?.lines;
            let b = a.channel(recipe.b_apid)?.lines;
            let m = r.min(g).min(b);
            if m == 0 { None } else { Some(m) }
        })
    }

    /// The min height the renderer's cached composite surface
    /// was built against, or `None` if there's no current cache.
    /// Used by the dropdown-refresh tick to decide whether a
    /// rebuild would change anything. Per CR round 3 on PR #575.
    #[must_use]
    pub fn cached_composite_min_height(&self) -> Option<usize> {
        self.renderer.borrow().composite_min_height()
    }

    /// Pull every scan line that's new since the last call out
    /// of the shared [`LrptImage`] and into the per-APID
    /// renderer surfaces. Queues a single redraw if anything
    /// changed and the view isn't paused.
    ///
    /// `with_assembler` holds the shared mutex while the line
    /// copy runs, so we keep the closure minimal — just memcpy
    /// the row slices, no rendering work — to avoid blocking
    /// the DSP thread on the lock for any longer than the
    /// strict copy time.
    pub fn drain_new_lines(&self) {
        // Two-phase to keep the shared `LrptImage` mutex hold
        // bounded. Phase 1 (under lock): walk the assembler and
        // copy out the new rows per APID into owned `Vec<u8>`s.
        // Phase 2 (lock released): hand the rows to the renderer,
        // which may lazy-alloc a ~51 MB Cairo surface and acquire
        // its surface-data lock — neither operation is fast
        // enough to hold the assembler mutex across, since that
        // would stall the DSP-thread writer behind it. Per
        // `CodeRabbit` round 12 on PR #543. (Each phase is its
        // own helper per the 50-NLOC gate, #819.)
        let pending = self.collect_pending_channels();
        let visible_dirty = self.push_pending_channels(pending);
        if visible_dirty && !self.paused.get() {
            self.drawing_area.queue_draw();
        }
    }

    /// Phase 1 of [`Self::drain_new_lines`] — under the shared-
    /// image lock: walk the assembler and copy each channel's
    /// unseen rows into an owned [`PendingChannel`]. Split out
    /// per the 50-NLOC gate (#819).
    fn collect_pending_channels(&self) -> Vec<PendingChannel> {
        let last_seen = self.last_seen_lines.borrow();
        let mut acc: Vec<PendingChannel> = Vec::new();
        self.image.with_assembler(|a| {
            for (&apid, channel) in a.channels() {
                let already = last_seen.get(&apid).copied().unwrap_or(0);
                if channel.lines <= already {
                    continue;
                }
                // Defensive — see lrpt_decoder::harvest_new_lines
                // for the parallel guard. Structurally
                // unreachable; the warn protects against
                // a future refactor of the assembler buffer
                // that drops the "pixels grows by full-line
                // increments" invariant.
                let available_lines = channel.pixels.len() / IMAGE_WIDTH;
                if available_lines < channel.lines {
                    tracing::warn!(
                        "LRPT view: channel {apid} pixel buffer shorter than expected; truncating at line {available_lines} (claimed lines = {})",
                        channel.lines,
                    );
                }
                let end_line = channel.lines.min(available_lines);
                if end_line <= already {
                    continue;
                }
                let start = already * IMAGE_WIDTH;
                let end = end_line * IMAGE_WIDTH;
                acc.push(PendingChannel {
                    apid,
                    already,
                    pixels: channel.pixels[start..end].to_vec(),
                });
            }
        });
        acc
    }

    /// Phase 2 of [`Self::drain_new_lines`] — outside the shared-
    /// image lock: hand each pending channel's rows to the
    /// renderer and advance the per-APID watermark. Returns
    /// whether the currently-visible channel gained a row (the
    /// caller's queue-redraw gate). Split out per the 50-NLOC
    /// gate (#819).
    ///
    /// Only the renderer's currently-active APID is painted
    /// by `LrptImageRenderer::render`, so the redraw should
    /// fire ONLY when that channel got a row this tick.
    /// Hidden APIDs that just gained rows are off-screen —
    /// their data lands in the per-channel surface but isn't
    /// visible until the user picks them in the dropdown,
    /// and the dropdown's own `selected_notify` handler will
    /// queue a redraw when that happens. Per `CodeRabbit`
    /// round 16 on PR #543.
    ///
    /// The auto-select transition (active was None, first
    /// ever push promotes it to Some(apid)) is covered by
    /// the per-channel comparison below: after `push_line`
    /// the renderer's `active_apid()` matches `p.apid`, so
    /// the same `painted_any && active == Some(p.apid)` gate
    /// catches the auto-select case naturally.
    fn push_pending_channels(&self, pending: Vec<PendingChannel>) -> bool {
        let mut visible_dirty = false;
        let mut last_seen = self.last_seen_lines.borrow_mut();
        let mut renderer = self.renderer.borrow_mut();
        for p in pending {
            // Track lines actually consumed so the watermark
            // doesn't advance past either the bounds-guard
            // skip path OR a transient renderer failure
            // (surface alloc / stride / lock). Same shape as
            // `lrpt_decoder::harvest_new_lines` on the DSP
            // side, plus `PushOutcome::consumed()` for the
            // renderer-side failure case. Per `CodeRabbit`
            // rounds 2 + 3 on PR #543.
            //
            // `painted_any` only flips on `PushOutcome::Pushed`.
            // `Capped` / `InvalidLine` advance the watermark (so
            // the row is "consumed" — see `PushOutcome::consumed`)
            // but don't change the visible canvas, and
            // `TransientFailure` doesn't even advance. Without
            // this distinction, a channel parked at MAX_LINES
            // would queue a redraw every 250 ms tick forever —
            // wasted GPU work for an unchanged image. Per
            // `CodeRabbit` round 9 on PR #543.
            let mut painted_any = false;
            let mut pushed = p.already;
            // `chunks_exact` views the flat tail buffer as
            // per-row slices without further allocation. Per
            // `CodeRabbit` round 17 on PR #543.
            for (offset, row) in p.pixels.as_chunks::<IMAGE_WIDTH>().0.iter().enumerate() {
                let outcome = renderer.push_line(p.apid, row);
                if !outcome.consumed() {
                    // Transient failure — leave this row in the
                    // source so the next poll retries.
                    break;
                }
                if matches!(outcome, PushOutcome::Pushed) {
                    painted_any = true;
                }
                pushed = p.already + offset + 1;
            }
            last_seen.insert(p.apid, pushed);
            if painted_any && renderer.active_apid() == Some(p.apid) {
                visible_dirty = true;
            }
        }
        visible_dirty
    }

    /// Clear all buffered lines and reset the watermark map,
    /// AND clear the backing shared `LrptImage` so the next
    /// drain tick can't replay any rows that were still in the
    /// shared assembler at the time of the clear. Without that,
    /// reopening the viewer mid-pass — or starting a new pass
    /// while the wiring layer hasn't yet cleared the shared
    /// image itself — would repopulate the canvas with the
    /// previous pass's pixels and contaminate later exports.
    /// Per `CodeRabbit` round 1 on PR #543.
    ///
    /// Between-pass cleanup; the next pass starts on a clean
    /// canvas. Idempotent — calling twice is harmless.
    pub fn clear(&self) {
        self.image.clear();
        self.renderer.borrow_mut().clear();
        self.last_seen_lines.borrow_mut().clear();
        self.drawing_area.queue_draw();
    }

    /// Toggle pause / resume. Pausing freezes the visible
    /// canvas; new lines pulled while paused still accumulate
    /// in the renderer (so nothing is lost) and become visible
    /// on resume via a forced single redraw.
    pub fn set_paused(&self, paused: bool) {
        let was_paused = self.paused.replace(paused);
        if was_paused && !paused {
            self.drawing_area.queue_draw();
        }
    }

    /// `true` if the view is currently paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    /// Save the active channel's image to a PNG. Same error
    /// semantics as [`LrptImageRenderer::export_png`].
    ///
    /// Drains any pending rows from the shared `LrptImage`
    /// into the renderer first, so the export captures the tail
    /// of the pass even if it arrived after the most recent
    /// poll tick. Without this, an immediate-export flow would
    /// systematically miss the last fraction-of-a-second of
    /// decoded data. Per `CodeRabbit` round 1 on PR #543.
    ///
    /// **Main-thread only.** `drain_new_lines` invokes
    /// `DrawingArea::queue_draw`, which GTK4 requires on the
    /// main thread, so this method cannot be moved to
    /// `gio::spawn_blocking` directly. It also performs
    /// synchronous Cairo PNG encoding + filesystem I/O — large
    /// images (~50 MB cap) will freeze the GTK main loop while
    /// it runs.
    ///
    /// For off-main-thread use the production paths take two
    /// different routes:
    ///
    /// - The manual Export PNG button in
    ///   [`open_lrpt_viewer_window`] calls
    ///   [`Self::snapshot_active_channel`] on the main thread
    ///   (cheap mutex-clone, also drains rows + queues the
    ///   redraw), then writes the PNG inside
    ///   `gio::spawn_blocking` via [`write_greyscale_png`].
    /// - The recorder's `RecorderAction::SaveLrptPass` handler
    ///   in `window.rs` snapshots per-APID `ChannelBuffer`s
    ///   directly from `AppState::lrpt_image` (it doesn't go
    ///   through the viewer at all — the LOS save needs to
    ///   work even when the user has closed the window
    ///   mid-pass), then writes one PNG per channel inside
    ///   `gio::spawn_blocking` via the same
    ///   [`write_greyscale_png`].
    ///
    /// Kept as a convenience for any future caller that
    /// genuinely wants the synchronous main-thread path (small
    /// test exports, scripted batch flows). Per `CodeRabbit`
    /// rounds 15 + 16 on PR #543.
    ///
    /// # Errors
    ///
    /// Propagates any [`ViewerError`] from the underlying
    /// renderer (per issue #545 — was `Result<(), String>`
    /// before).
    pub fn export_png(&self, path: &Path) -> Result<(), ViewerError> {
        self.drain_new_lines();
        self.renderer.borrow().export_png(path)
    }

    /// Snapshot the currently-active channel's pixel data into
    /// an owned `(apid, ChannelBuffer)` pair. Used by callers that
    /// only ever need the per-APID greyscale path (the composite-
    /// aware export button uses [`Self::snapshot_for_export`]
    /// instead). Drains pending rows from the shared `LrptImage`
    /// first so the snapshot captures the tail of the pass.
    ///
    /// Returns `None` if no APID is currently selected, or if
    /// the active APID has no decoded rows in the shared image.
    pub fn snapshot_active_channel(&self) -> Option<(u16, sdr_lrpt::image::ChannelBuffer)> {
        self.drain_new_lines();
        let apid = self.renderer.borrow().active_apid()?;
        let snap = self.image.snapshot_channel(apid)?;
        if snap.lines == 0 {
            return None;
        }
        Some((apid, snap))
    }

    /// Snapshot the viewer's current display state for off-main-
    /// thread PNG export. Returns either an [`ExportSnapshot::Channel`]
    /// (per-APID greyscale path, same as
    /// [`Self::snapshot_active_channel`]) or an
    /// [`ExportSnapshot::Composite`] (RGB composite path) depending
    /// on whether the user has a composite recipe selected. Drains
    /// pending rows from the shared `LrptImage` first so the
    /// snapshot captures the tail of the pass.
    ///
    /// Returns `None` if there's nothing to export — either no
    /// channel/composite selected, an active per-APID channel
    /// with no decoded lines, or a composite recipe whose
    /// source APIDs aren't all populated yet. Per CR round 2
    /// on PR #575.
    pub fn snapshot_for_export(&self) -> Option<ExportSnapshot> {
        self.drain_new_lines();
        if let Some(recipe) = self.renderer.borrow().active_composite() {
            // Composite selection is authoritative. If
            // `clone_channels_for_composite` returns `None` (one
            // or more source APIDs missing/empty), return `None`
            // — never fall back to the per-APID path. The
            // dropdown still says "Composite — ..." and exporting
            // the last greyscale APID under that label would
            // silently mislead the user (the on-screen canvas
            // doesn't fall back either; both stay consistent
            // until the composite is buildable). The export-
            // button toast handler surfaces the resulting `None`
            // as "No LRPT image data to export yet". Per CR
            // round 4 on PR #575.
            let snap = self.image.with_assembler(|a| {
                a.clone_channels_for_composite(recipe.r_apid, recipe.g_apid, recipe.b_apid)
            });
            return snap.map(|snapshot| ExportSnapshot::Composite { recipe, snapshot });
        }
        let (apid, buffer) = self.snapshot_active_channel()?;
        Some(ExportSnapshot::Channel { apid, buffer })
    }
}

/// One channel's unseen rows, copied out of the shared assembler
/// under its lock by [`LrptImageView::collect_pending_channels`]
/// and consumed lock-free by
/// [`LrptImageView::push_pending_channels`]. Hoisted from a
/// function-local struct when `drain_new_lines` was split per the
/// 50-NLOC gate (#819).
struct PendingChannel {
    apid: u16,
    already: usize,
    /// Flat tail of the channel's pixel buffer — every
    /// row from `already` to `min(channel.lines,
    /// available_lines)` packed contiguously, ready for
    /// `chunks_exact(IMAGE_WIDTH)` in phase 2. One heap
    /// alloc per APID per drain instead of one per row;
    /// matters on viewer reopen mid-pass when there can
    /// be thousands of unseen rows for a single APID
    /// and the per-row alloc would churn the allocator
    /// at 4 Hz under the shared-image mutex. Per
    /// `CodeRabbit` round 17 on PR #543.
    pixels: Vec<u8>,
}

/// Post-await half of [`LrptImageView::set_composite`]: wrap the
/// worker's BGRA bytes in a Cairo surface on the main thread and
/// install it as the composite cache — or, on worker panic /
/// surface-build failure, reset the in-flight build target so the
/// next 1 Hz tick retries. Every failure path gates on the
/// generation match so it can't clobber a newer build that
/// started while this one was running. Split out per the 50-NLOC
/// gate (#819, PR #880 Codacy precedent).
fn finish_composite_build(
    renderer: &Rc<RefCell<LrptImageRenderer>>,
    drawing_area: &gtk4::DrawingArea,
    recipe: CompositeRecipe,
    captured_gen: u64,
    target_height: usize,
    bytes_result: Result<Vec<u8>, Box<dyn std::any::Any + Send>>,
) {
    // Main thread: wrap the worker's BGRA bytes in a Cairo
    // surface via `create_for_data`. Cheap (no memcpy in the
    // common stride case) — the heavy per-pixel pack already ran
    // on the worker.
    let surface_result = match bytes_result {
        Ok(bgra) => build_argb32_surface_from_bgra(bgra, IMAGE_WIDTH, target_height),
        Err(panic) => {
            tracing::warn!(?recipe, "composite worker panicked: {panic:?}");
            let mut r = renderer.borrow_mut();
            if r.composite_gen() == captured_gen {
                r.mark_composite_pending(recipe);
            }
            return;
        }
    };
    match surface_result {
        Ok(surface) => {
            let installed = renderer.borrow_mut().install_composite_cache(
                recipe,
                captured_gen,
                target_height,
                surface,
            );
            if installed {
                drawing_area.queue_draw();
            } else {
                tracing::debug!(
                    ?recipe,
                    captured_gen,
                    "stale composite worker — selection changed mid-flight, dropping built surface",
                );
            }
        }
        Err(e) => {
            tracing::warn!(?recipe, error = %e, "composite ARGB32 surface build failed");
            // Reset the in-flight target so the next 1 Hz tick
            // retries — only if the user hasn't moved on. Gate on
            // generation match to avoid clobbering a newer build
            // that started while ours was running.
            let mut r = renderer.borrow_mut();
            if r.composite_gen() == captured_gen {
                r.mark_composite_pending(recipe);
            }
        }
    }
}
