//! Live Meteor-M LRPT image viewer + PNG export.
//!
//! LRPT counterpart to [`crate::apt_viewer`]. Displays the
//! per-APID scan-line buffers from a shared
//! [`sdr_radio::lrpt_image::LrptImage`] as they accumulate during
//! a satellite pass. Width is fixed at the LRPT scan width
//! ([`IMAGE_WIDTH`] = 1568 px); height grows downward as new
//! lines arrive from the FEC chain.
//!
//! Three pieces:
//!
//! * [`LrptImageRenderer`] — pure Cairo renderer. Owns a
//!   `HashMap<APID, ChannelSurface>` of ARGB32 surfaces, each
//!   sized for a full pass. Knows how to paint the active
//!   channel into a cairo context with auto-fit + aspect
//!   preservation. No GTK dependency, fully unit-testable.
//! * [`LrptImageView`] — GTK widget wrapping a renderer plus a
//!   poll timer that drains new scan lines from the shared
//!   [`sdr_radio::lrpt_image::LrptImage`] handle. Cloneable
//!   (all state is `Rc`-shared) so closures on toolbar buttons
//!   can hold their own handle. Polling — rather than
//!   message-pushing as APT does — matches LRPT's
//!   `Arc<Mutex<ImageAssembler>>` data-sharing model: the DSP
//!   thread mutates the shared buffer, the UI reads it.
//! * [`open_lrpt_viewer_window`] — opens the view in a
//!   non-modal transient window. Header bar carries a channel
//!   selector, Pause / Resume, and Export PNG.
//!
//! [`connect_lrpt_action`] wires the `app.lrpt-open` action
//! (`Ctrl+Shift+L`). Activating it opens a viewer window and
//! registers the shared `LrptImage` handle with the DSP
//! controller via `UiToDsp::SetLrptImage`. Closing the window
//! sends `UiToDsp::ClearLrptImage` so the decoder tap goes
//! silent until the user reopens.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{cairo, gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use sdr_lrpt::image::IMAGE_WIDTH;
use sdr_radio::lrpt_image::LrptImage;

use crate::messages::UiToDsp;

/// Maximum lines we'll keep per channel. The MSU-MR scanner on
/// Meteor-M produces AVHRR-style imagery at ~6 scan lines per
/// second per channel; a long high-elevation pass (~15 min above
/// horizon) is therefore ~5400 lines, and a typical 10-min pass
/// is ~3600. 8192 gives ~2× headroom for the longest plausible
/// pass without ever growing the surface at runtime — the
/// previous 1024 cap clipped roughly the last 80 % of even a
/// nominal pass. Per `CodeRabbit` round 2 on PR #543.
///
/// Memory cost is lazy: the per-APID Cairo surface only
/// allocates when that channel first receives a line. At the
/// cap, one channel is `IMAGE_WIDTH × MAX_LINES × 4 B` ≈ 51 MB.
/// A typical pass with three active AVHRR channels therefore
/// peaks around 150 MB, which matches the rest of the SDR
/// pipeline's working-set budget.
pub const MAX_LINES: usize = 8_192;

/// Background colour painted before any LRPT data is received
/// (and behind the image when the widget is wider than the
/// image's aspect). Near-black so the eventual greyscale image
/// stands out, matching the APT viewer's palette.
const BACKGROUND_RGB: [f64; 3] = [0.05, 0.05, 0.06];

/// Bytes per pixel for Cairo's ARGB32 surface format —
/// `B`, `G`, `R`, `A` in little-endian byte order. Pulled out
/// of the hot-path pixel-copy loop in
/// [`LrptImageRenderer::push_line`] so a future format change
/// (e.g. RGB24 for the LRPT RGB-composite mode) is a one-line
/// edit. Per `CodeRabbit` round 4 on PR #543.
const BYTES_PER_PIXEL: usize = 4;

/// Default size for the viewer window. Wider than tall because
/// LRPT scan width (1568 px) is greater than typical pass
/// heights (~600 lines) — landscape layout fills better.
const VIEWER_WINDOW_WIDTH: i32 = 900;
const VIEWER_WINDOW_HEIGHT: i32 = 600;

/// Poll interval the view uses to drain new scan lines from
/// the shared `LrptImage` and queue redraws. 250 ms (4 Hz) is
/// faster than the satellite's ~1 Hz line cadence so users see
/// data the moment it lands, without burning CPU on a tight
/// loop. 60 FPS would be wasteful — there's no smooth-motion
/// content here, just discrete row appends.
const POLL_INTERVAL_MS: u32 = 250;

/// Refresh interval for the channel-dropdown population tick.
/// Channel discovery on Meteor is rare (a handful of APIDs per
/// pass, all surfaced within the first minute), so 1 Hz is
/// plenty — anything faster would burn CPU on idle string
/// compares. Per `CodeRabbit` round 5 on PR #543.
const DROPDOWN_REFRESH_INTERVAL_MS: u32 = 1_000;

/// What [`LrptImageRenderer::push_line`] did with the row.
/// Drives the caller's per-APID watermark: rows that were
/// committed (or permanently dropped because they're either
/// malformed or past the channel's [`MAX_LINES`] cap) advance the
/// watermark; transient renderer failures leave the row in the
/// source so the next poll can retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// Row was written into the per-APID Cairo surface.
    Pushed,
    /// Channel already at [`MAX_LINES`] — row intentionally
    /// dropped. Caller should advance its watermark; further
    /// data for this channel will keep hitting the cap no
    /// matter how many retries.
    Capped,
    /// Caller bug — pixel slice didn't match [`IMAGE_WIDTH`].
    /// Caller should advance its watermark; the data is
    /// malformed at the source and retrying won't help.
    InvalidLine,
    /// Transient renderer-side failure (surface allocation,
    /// stride conversion, or surface-data lock). Caller should
    /// NOT advance its watermark — the next poll might succeed
    /// (alloc relief, lock contention clearing).
    TransientFailure,
}

impl PushOutcome {
    /// `true` when the caller should advance its watermark past
    /// this row. `false` means "leave it in the source for the
    /// next poll to retry" — used only for [`Self::TransientFailure`].
    #[must_use]
    pub fn consumed(self) -> bool {
        !matches!(self, Self::TransientFailure)
    }
}

/// Pure Cairo renderer for a multi-channel LRPT image buffer.
///
/// Owns one persistent ARGB32 [`cairo::ImageSurface`] per APID,
/// each sized [`IMAGE_WIDTH`] × [`MAX_LINES`] and lazily
/// allocated on the first `push_line(apid, …)` for that APID.
/// Surfaces are kept across pushes so [`Self::render`] can paint
/// the latest state without copying — same alloc-free hot-path
/// guarantee the APT renderer offers.
pub struct LrptImageRenderer {
    channels: HashMap<u16, ChannelSurface>,
    /// APID currently selected for display. `None` if the user
    /// hasn't picked a channel yet (or the renderer is empty).
    active: Option<u16>,
}

struct ChannelSurface {
    surface: cairo::ImageSurface,
    n_lines: usize,
}

impl ChannelSurface {
    /// Allocate a fresh full-pass-sized surface for one APID.
    /// Returns `None` if Cairo can't allocate the (~6 MB) ARGB32
    /// surface — practically unreachable on any desktop machine,
    /// but the library-crate "no panic" rule still applies.
    /// Per `CodeRabbit` round 1 on PR #543: the earlier draft
    /// panicked via `.expect()` even though `sdr-ui` is a
    /// library crate. Callers (`LrptImageRenderer::push_line`)
    /// degrade gracefully — log a warning and drop the line
    /// rather than killing the GTK main loop.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn new() -> Option<Self> {
        let surface = cairo::ImageSurface::create(
            cairo::Format::ARgb32,
            IMAGE_WIDTH as i32,
            MAX_LINES as i32,
        )
        .ok()?;
        Some(Self {
            surface,
            n_lines: 0,
        })
    }
}

impl Default for LrptImageRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl LrptImageRenderer {
    /// Build an empty renderer. No channels are allocated until
    /// the first `push_line` call for each APID.
    #[must_use]
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
            active: None,
        }
    }

    /// All APIDs the renderer has seen at least one line for,
    /// in unspecified order. Used by the GTK widget to populate
    /// its channel dropdown.
    pub fn known_apids(&self) -> Vec<u16> {
        self.channels.keys().copied().collect()
    }

    /// APID currently selected for display, if any.
    #[must_use]
    pub fn active_apid(&self) -> Option<u16> {
        self.active
    }

    /// Set which APID's channel is shown. A no-op (returns
    /// `false`) if the renderer has never received a line for
    /// that APID — without a backing surface there's nothing to
    /// paint, and silently switching to a missing channel would
    /// leave the user staring at a blank canvas with no
    /// feedback.
    pub fn set_active_apid(&mut self, apid: u16) -> bool {
        if self.channels.contains_key(&apid) {
            self.active = Some(apid);
            true
        } else {
            false
        }
    }

    /// Number of scan lines buffered for `apid`, or 0 if unknown.
    #[must_use]
    pub fn n_lines(&self, apid: u16) -> usize {
        self.channels.get(&apid).map_or(0, |c| c.n_lines)
    }

    /// `true` when no APID has any scan line yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.channels.values().all(|c| c.n_lines == 0)
    }

    /// Append one scan line of width [`IMAGE_WIDTH`] to the
    /// surface for `apid`, lazy-allocating the surface on the
    /// first push for that APID. Greyscale values go into the
    /// surface's backing data as ARGB32 (B/G/R/A — Cairo's
    /// little-endian layout, alpha = `0xFF`).
    ///
    /// Returns a [`PushOutcome`] that callers (specifically
    /// [`LrptImageView::drain_new_lines`]) inspect to decide
    /// whether to advance their per-APID watermark. Pushed and
    /// permanently-dropped rows (cap reached, malformed input)
    /// advance the watermark; transient renderer failures
    /// (surface alloc, stride conversion, surface-data lock)
    /// leave the row in the source so the next poll can retry.
    /// Per `CodeRabbit` round 3 on PR #543.
    pub fn push_line(&mut self, apid: u16, pixels: &[u8]) -> PushOutcome {
        if pixels.len() != IMAGE_WIDTH {
            tracing::warn!(
                "LRPT renderer: ignoring line for APID {apid} with {} pixels (expected {IMAGE_WIDTH})",
                pixels.len(),
            );
            // Caller bug. Watermark should still advance —
            // retrying with the same malformed input will only
            // reproduce the same warn forever.
            return PushOutcome::InvalidLine;
        }
        // Lazy alloc; `ChannelSurface::new` returns `None` if
        // Cairo can't acquire the ~MAX-LINES-sized ARGB32
        // surface. Drop the line with a warn rather than
        // panicking — and report the failure as transient so
        // the next poll retries (alloc may succeed later under
        // memory pressure relief).
        let entry = match self.channels.entry(apid) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let Some(surface) = ChannelSurface::new() else {
                    tracing::warn!(
                        "LRPT renderer: surface alloc failed for APID {apid}; dropping line",
                    );
                    return PushOutcome::TransientFailure;
                };
                e.insert(surface)
            }
        };
        if entry.n_lines >= MAX_LINES {
            // Surface full — advance watermark anyway. Further
            // data for this channel will keep hitting the cap
            // no matter how many times we retry.
            return PushOutcome::Capped;
        }
        let stride = match usize::try_from(entry.surface.stride()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("LRPT renderer: invalid surface stride: {e}");
                return PushOutcome::TransientFailure;
            }
        };
        let row_offset = entry.n_lines * stride;
        let mut data = match entry.surface.data() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("LRPT renderer: surface data lock failed: {e}");
                return PushOutcome::TransientFailure;
            }
        };
        for (i, &g) in pixels.iter().enumerate() {
            let pixel_offset = row_offset + i * BYTES_PER_PIXEL;
            data[pixel_offset] = g;
            data[pixel_offset + 1] = g;
            data[pixel_offset + 2] = g;
            data[pixel_offset + 3] = 0xFF;
        }
        // `data` guard drops here, flushing the surface for cairo.
        drop(data);
        entry.n_lines += 1;
        // First-ever push for any channel — auto-select it so
        // the user sees something the moment data starts
        // flowing, without having to discover the dropdown.
        if self.active.is_none() {
            self.active = Some(apid);
        }
        PushOutcome::Pushed
    }

    /// Drop all per-channel surfaces. The `HashMap` allocation
    /// itself is preserved, but each ~6 MB surface is freed —
    /// callers (between-pass cleanup) typically rebuild from
    /// scratch as new channels reappear.
    pub fn clear(&mut self) {
        self.channels.clear();
        self.active = None;
    }

    /// Paint the active channel's image into `cr`, scaled to fit
    /// `(width, height)` while preserving the
    /// `IMAGE_WIDTH : n_lines` aspect. Centred horizontally,
    /// top-aligned vertically.
    ///
    /// Returns `Ok(())` and paints just the background when no
    /// channel is active or the active channel has no lines —
    /// callers don't need to special-case the empty state.
    ///
    /// # Errors
    ///
    /// Returns a stringified Cairo error on paint failure.
    /// Callers usually log and continue — drawing failures
    /// shouldn't kill the UI.
    #[allow(clippy::cast_precision_loss)]
    pub fn render(&self, cr: &cairo::Context, width: i32, height: i32) -> Result<(), String> {
        cr.set_source_rgb(BACKGROUND_RGB[0], BACKGROUND_RGB[1], BACKGROUND_RGB[2]);
        cr.paint().map_err(|e| format!("background paint: {e}"))?;

        let Some(apid) = self.active else {
            return Ok(());
        };
        let Some(channel) = self.channels.get(&apid) else {
            return Ok(());
        };
        if channel.n_lines == 0 || width <= 0 || height <= 0 {
            return Ok(());
        }

        let img_w = IMAGE_WIDTH as f64;
        let img_h = channel.n_lines as f64;
        let scale = (f64::from(width) / img_w).min(f64::from(height) / img_h);
        let off_x = (f64::from(width) - img_w * scale) / 2.0;

        cr.save().map_err(|e| format!("save: {e}"))?;
        cr.translate(off_x, 0.0);
        cr.scale(scale, scale);
        cr.set_source_surface(&channel.surface, 0.0, 0.0)
            .map_err(|e| format!("set_source_surface: {e}"))?;
        cr.rectangle(0.0, 0.0, img_w, img_h);
        cr.fill().map_err(|e| format!("image fill: {e}"))?;
        cr.restore().map_err(|e| format!("restore: {e}"))?;
        Ok(())
    }

    /// Save the active channel's image to a PNG file. Builds a
    /// one-shot tightly-sized export surface
    /// (`IMAGE_WIDTH × n_lines`) so the file doesn't carry
    /// padding rows past the real data.
    ///
    /// # Errors
    ///
    /// Returns a stringified error if no channel is active /
    /// has data, or from filesystem creation, surface
    /// construction, paint operations, or Cairo's PNG encoder.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    pub fn export_png(&self, path: &Path) -> Result<(), String> {
        let Some(apid) = self.active else {
            return Err("no LRPT channel selected for export".to_string());
        };
        let Some(channel) = self.channels.get(&apid) else {
            return Err(format!("LRPT channel APID {apid} has no data to export"));
        };
        if channel.n_lines == 0 {
            return Err(format!("LRPT channel APID {apid} has no data to export"));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }

        let export_surface = cairo::ImageSurface::create(
            cairo::Format::ARgb32,
            IMAGE_WIDTH as i32,
            channel.n_lines as i32,
        )
        .map_err(|e| format!("export surface: {e}"))?;
        let cr =
            cairo::Context::new(&export_surface).map_err(|e| format!("export context: {e}"))?;
        cr.set_source_surface(&channel.surface, 0.0, 0.0)
            .map_err(|e| format!("export set_source_surface: {e}"))?;
        // IMAGE_WIDTH and n_lines are well under f64's mantissa
        // — bounded by MAX_LINES — so no real precision loss.
        #[allow(clippy::cast_precision_loss)]
        cr.rectangle(0.0, 0.0, IMAGE_WIDTH as f64, channel.n_lines as f64);
        cr.fill().map_err(|e| format!("export fill: {e}"))?;
        drop(cr);

        let mut file = std::fs::File::create(path).map_err(|e| format!("file: {e}"))?;
        export_surface
            .write_to_png(&mut file)
            .map_err(|e| format!("png: {e}"))?;
        tracing::info!(
            ?path,
            apid,
            lines = channel.n_lines,
            "LRPT image exported to PNG"
        );
        Ok(())
    }
}

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
    /// the view + ~6 MB-per-channel surfaces don't leak past
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
    /// this, the view + ~6 MB-per-channel surfaces stay alive in
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
        let mut any_new = false;
        let mut last_seen = self.last_seen_lines.borrow_mut();
        let mut renderer = self.renderer.borrow_mut();
        self.image.with_assembler(|a| {
            for (&apid, channel) in a.channels() {
                let already = last_seen.get(&apid).copied().unwrap_or(0);
                if channel.lines <= already {
                    continue;
                }
                // Track lines actually consumed so the watermark
                // doesn't advance past either the bounds-guard
                // skip path OR a transient renderer failure
                // (surface alloc / stride / lock). Same shape as
                // `lrpt_decoder::harvest_new_lines` on the DSP
                // side, plus `PushOutcome::consumed()` for the
                // renderer-side failure case. Per `CodeRabbit`
                // rounds 2 + 3 on PR #543.
                let mut pushed = already;
                for line_idx in already..channel.lines {
                    let start = line_idx * IMAGE_WIDTH;
                    let end = start + IMAGE_WIDTH;
                    if end > channel.pixels.len() {
                        // Defensive — see lrpt_decoder::harvest_new_lines
                        // for the parallel guard. Structurally
                        // unreachable; the warn protects against a
                        // future refactor of the assembler buffer.
                        tracing::warn!(
                            "LRPT view: channel {apid} pixel buffer shorter than expected; skipping line {line_idx}",
                        );
                        break;
                    }
                    let outcome = renderer.push_line(apid, &channel.pixels[start..end]);
                    if !outcome.consumed() {
                        // Transient failure — leave this row in
                        // the source so the next poll retries.
                        // Don't break the outer loop; the next
                        // tick will pick up where we left off.
                        break;
                    }
                    pushed = line_idx + 1;
                }
                last_seen.insert(apid, pushed);
                any_new = true;
            }
        });
        drop(renderer);
        drop(last_seen);

        if any_new && !self.paused.get() {
            self.drawing_area.queue_draw();
        }
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
    /// poll tick. Without this, the LOS `SaveLrptPass` flow —
    /// which exports immediately on receipt of the LOS action,
    /// not on the next 250 ms poll — would systematically miss
    /// the last fraction-of-a-second of decoded data. Per
    /// `CodeRabbit` round 1 on PR #543.
    ///
    /// # Errors
    ///
    /// Propagates any error from the underlying renderer.
    pub fn export_png(&self, path: &Path) -> Result<(), String> {
        self.drain_new_lines();
        self.renderer.borrow().export_png(path)
    }
}

// ─── Non-modal viewer window ───────────────────────────────────────────

/// Build the dynamic channel-picker dropdown for the viewer
/// header. APIDs aren't known at open time, so the dropdown
/// starts dimmed-but-visible and a 1 Hz `glib` timer rebuilds
/// its model whenever new APIDs appear in `view`. A parallel
/// `Vec<u16>` lets us decode the dropdown's numeric `selected`
/// index back into an APID without parsing the display string.
fn build_channel_dropdown(view: &LrptImageView) -> gtk4::DropDown {
    let model = gtk4::StringList::new(&[]);
    let dropdown = gtk4::DropDown::builder()
        .model(&model)
        .tooltip_text("Which AVHRR channel (APID) to display")
        .sensitive(false)
        .build();
    dropdown.update_property(&[gtk4::accessible::Property::Label("LRPT channel selector")]);
    let dropdown_apids: Rc<RefCell<Vec<u16>>> = Rc::new(RefCell::new(Vec::new()));

    // Selection → renderer.
    {
        let view = view.clone();
        let dropdown_apids = Rc::clone(&dropdown_apids);
        dropdown.connect_selected_notify(move |dd| {
            let idx = dd.selected() as usize;
            let apids = dropdown_apids.borrow();
            if let Some(&apid) = apids.get(idx) {
                let _ = view.set_active_apid(apid);
            }
        });
    }

    // Refresh tick — runs at 1 Hz (channel discovery is rare;
    // a faster cadence would burn CPU on idle string compares).
    // Register the source on the view so `LrptImageView::shutdown`
    // can cancel it when the window closes; otherwise the closure's
    // `view.clone()` would keep the view + ~6 MB-per-channel
    // surfaces alive forever.
    //
    // **Borrow scoping:** GTK4's `gtk4::DropDown::set_selected`
    // emits `notify::selected` SYNCHRONOUSLY inside the setter,
    // which means the `connect_selected_notify` handler above
    // re-enters this same `dropdown_apids` `RefCell` to look
    // up the APID for the new index. If we held a `borrow_mut()`
    // across `set_selected(...)`, that re-entrance would panic
    // with "already borrowed". Per `CodeRabbit` round 3 on PR
    // #543. The borrows below are kept tight: an immutable
    // `borrow()` for the equality compare, a fresh
    // `borrow_mut()` for the `clone_from`, and zero borrows
    // held during the `set_selected` calls.
    let view_for_tick = view.clone();
    let dropdown_clone = dropdown.clone();
    let refresh_id = glib::timeout_add_local(
        std::time::Duration::from_millis(u64::from(DROPDOWN_REFRESH_INTERVAL_MS)),
        move || {
            let mut current = view_for_tick.known_apids();
            current.sort_unstable();
            // Cheap sync check: bail out only if BOTH the APID
            // list AND the dropdown's selected channel still
            // match the renderer's active APID. The selected-
            // sync arm guards against an external caller
            // (`SaveLrptPass` walks active_apid through every
            // channel; future programmatic API users) changing
            // `view.active_apid()` without changing the list,
            // which the previous list-only fast-path would have
            // ignored — leaving the dropdown stuck on the wrong
            // channel until the next list change. Per
            // `CodeRabbit` round 6 on PR #543.
            let active = view_for_tick.active_apid();
            let apids_unchanged = *dropdown_apids.borrow() == current;
            #[allow(clippy::cast_possible_truncation)]
            let selected_apid = {
                let apids = dropdown_apids.borrow();
                apids.get(dropdown_clone.selected() as usize).copied()
            };
            if apids_unchanged && selected_apid == active {
                return glib::ControlFlow::Continue;
            }
            if !apids_unchanged {
                model.splice(0, model.n_items(), &[]);
                for &apid in &current {
                    model.append(&format!("APID {apid}"));
                }
                dropdown_apids.borrow_mut().clone_from(&current);
            }
            dropdown_clone.set_sensitive(!current.is_empty());
            if let Some(active) = active {
                if let Some(pos) = current.iter().position(|&a| a == active) {
                    #[allow(clippy::cast_possible_truncation)]
                    dropdown_clone.set_selected(pos as u32);
                }
            } else if !current.is_empty() {
                // No previous selection — pick the first APID
                // (sorted) so the user sees something the moment
                // data arrives. The `selected_notify` handler above
                // will route the choice into the renderer.
                dropdown_clone.set_selected(0);
            }
            glib::ControlFlow::Continue
        },
    );
    view.register_source(refresh_id);

    dropdown
}

/// Build the Pause / Resume toggle for the viewer header.
/// Pull-out so [`open_lrpt_viewer_window`] stays under the
/// 100-line clippy threshold.
fn build_pause_button(view: &LrptImageView) -> gtk4::ToggleButton {
    let btn = gtk4::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text("Pause / resume the live image update")
        .build();
    btn.update_property(&[gtk4::accessible::Property::Label(
        "Pause or resume live image update",
    )]);
    let view = view.clone();
    btn.connect_toggled(move |b| {
        view.set_paused(b.is_active());
    });
    btn
}

/// Open the LRPT viewer in a non-modal transient window. The
/// window holds a header bar with a channel dropdown,
/// Pause / Resume, and Export PNG, plus the drawing-area
/// canvas underneath.
///
/// Non-modal so the user can keep tuning, recording, or
/// otherwise interacting with the main radio window while the
/// LRPT image builds up alongside.
pub fn open_lrpt_viewer_window<W: gtk4::prelude::IsA<gtk4::Window>>(
    parent: &W,
    title: &str,
    image: LrptImage,
) -> (LrptImageView, adw::Window) {
    let view = LrptImageView::new(image);

    let window = adw::Window::builder()
        .title(title)
        .default_width(VIEWER_WINDOW_WIDTH)
        .default_height(VIEWER_WINDOW_HEIGHT)
        .transient_for(parent)
        .modal(false)
        .build();

    let header = adw::HeaderBar::new();

    let channel_dropdown = build_channel_dropdown(&view);
    header.pack_start(&channel_dropdown);

    let pause_btn = build_pause_button(&view);
    header.pack_start(&pause_btn);

    // ── Export PNG ────────────────────────────────────────
    let export_btn = gtk4::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Export the current LRPT channel image to PNG")
        .build();
    export_btn.update_property(&[gtk4::accessible::Property::Label(
        "Export LRPT channel image to PNG",
    )]);
    let export_view = view.clone();
    let window_for_export = window.downgrade();
    export_btn.connect_clicked(move |_| {
        let Some(window_for_export) = window_for_export.upgrade() else {
            return;
        };
        // Drain pending rows BEFORE deriving the export path —
        // `drain_new_lines` may auto-select the first decoded
        // APID (the renderer's `push_line` does so on first
        // push to any channel), and we want the filename to
        // reflect that resolved channel rather than the
        // pre-drain `None` (which would land at
        // `lrpt-unknown-...png`). `LrptImageView::export_png`
        // also drains internally, but by that point we'd have
        // already baked the stale APID into the path. Per
        // `CodeRabbit` round 6 on PR #543.
        export_view.drain_new_lines();
        let path = default_export_path(export_view.active_apid());
        let toast = match export_view.export_png(&path) {
            Ok(()) => adw::Toast::builder()
                .title(format!("Saved {}", path.display()))
                .build(),
            Err(e) => adw::Toast::builder()
                .title(format!("PNG export failed: {e}"))
                .build(),
        };
        show_toast_in(&window_for_export, toast);
    });
    header.pack_end(&export_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(view.drawing_area()));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar));

    window.set_content(Some(&toast_overlay));
    window.present();

    (view, window)
}

/// Default path the Export PNG button writes to:
/// `~/sdr-recordings/lrpt-{apid}-YYYY-MM-DD-HHMMSS.png`.
fn default_export_path(apid: Option<u16>) -> PathBuf {
    let timestamp = glib::DateTime::now_local()
        .and_then(|dt| dt.format("%Y-%m-%d-%H%M%S"))
        .map_or_else(|_| "unknown".to_string(), |s| s.to_string());
    let apid_part = apid.map_or_else(|| "unknown".to_string(), |a| format!("apid{a}"));
    glib::home_dir()
        .join("sdr-recordings")
        .join(format!("lrpt-{apid_part}-{timestamp}.png"))
}

/// Walk the window content tree looking for a [`adw::ToastOverlay`]
/// to display `toast` in. Falls through silently if the layout
/// changes — toasts are best-effort feedback, not load-bearing UI.
fn show_toast_in<W: gtk4::prelude::IsA<gtk4::Window>>(window: &W, toast: adw::Toast) {
    if let Some(child) = window.as_ref().child()
        && let Some(overlay) = child.downcast_ref::<adw::ToastOverlay>()
    {
        overlay.add_toast(toast);
    }
}

// ─── Live viewer action ────────────────────────────────────────────────

/// Wire the `app.lrpt-open` action onto `app`. Activating it
/// (via the app menu, the `Ctrl+Shift+L` accelerator, or future
/// activity-bar entry) opens a non-modal LRPT viewer window
/// and informs the DSP controller about the shared image
/// handle so the LRPT decoder tap starts pushing scan lines
/// into it. Closing the window clears both the `AppState` slot
/// and the DSP-side handle.
pub fn connect_lrpt_action(
    app: &adw::Application,
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    let action = gio::SimpleAction::new("lrpt-open", None);
    let parent_provider = Rc::clone(parent_provider);
    let state_for_action = Rc::clone(state);
    action.connect_activate(move |_, _| {
        open_lrpt_viewer_if_needed(&parent_provider, &state_for_action);
    });
    app.add_action(&action);
    app.set_accels_for_action("app.lrpt-open", &["<Ctrl><Shift>l"]);
}

/// Open the LRPT viewer window if it isn't already open.
/// Registers the new view in `state.lrpt_viewer`, hands the
/// shared image to the DSP thread, and wires `close-request`
/// to tear both down.
///
/// Pulled out of [`connect_lrpt_action`] so the auto-record
/// path (Task 7.5) can fire the same open flow at AOS without
/// going through the GIO action system. Mirrors the APT
/// viewer's [`crate::apt_viewer::open_apt_viewer_if_needed`].
pub fn open_lrpt_viewer_if_needed(
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    if state.lrpt_viewer.borrow().is_some() {
        return;
    }
    let Some(parent) = parent_provider() else {
        tracing::warn!("lrpt-open invoked with no main window available");
        return;
    };
    let image = state.lrpt_image.clone();
    let (view, window) = open_lrpt_viewer_window(&parent, "Meteor-M LRPT", image.clone());
    *state.lrpt_viewer.borrow_mut() = Some(view);
    state.send_dsp(UiToDsp::SetLrptImage(image));

    let state_for_close = Rc::clone(state);
    window.connect_close_request(move |_| {
        // Cancel the view's drain + dropdown-refresh timeouts
        // BEFORE we drop the AppState slot; otherwise their
        // closures' `Rc<view>` clones keep the view + ~6 MB-
        // per-channel surfaces alive until the application
        // exits. Per `CodeRabbit` round 1 on PR #543.
        if let Some(view) = state_for_close.lrpt_viewer.borrow().as_ref() {
            view.shutdown();
        }
        *state_for_close.lrpt_viewer.borrow_mut() = None;
        state_for_close.send_dsp(UiToDsp::ClearLrptImage);
        glib::Propagation::Proceed
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// APID used in renderer tests. AVHRR convention: 64 = ch1.
    /// Same value the rest of the LRPT test suite uses for
    /// single-channel cases.
    const APID_TEST: u16 = 64;
    /// Secondary APID for multi-channel checks.
    const APID_TEST_2: u16 = 65;
    /// Pixel marker — distinct from 0/0xFF so a regression that
    /// returned a default-allocated surface would fail loudly.
    const TEST_PIXEL: u8 = 0x42;
    const TEST_PIXEL_2: u8 = 0xC0;

    fn synth_line(value: u8) -> Vec<u8> {
        vec![value; IMAGE_WIDTH]
    }

    #[test]
    fn renderer_starts_empty_with_no_active_channel() {
        let r = LrptImageRenderer::new();
        assert!(r.is_empty());
        assert!(r.active_apid().is_none());
        assert!(r.known_apids().is_empty());
    }

    #[test]
    fn push_line_lazily_allocates_surface_per_apid() {
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        assert_eq!(r.n_lines(APID_TEST), 1);
        // The other APID has never been pushed — n_lines returns 0
        // and the channel doesn't exist yet.
        assert_eq!(r.n_lines(APID_TEST_2), 0);
        assert_eq!(r.known_apids(), vec![APID_TEST]);
    }

    #[test]
    fn first_push_auto_selects_that_apid() {
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        // The user shouldn't have to manually pick a channel
        // before any data is visible — pushing the first line
        // for any APID auto-selects it.
        assert_eq!(r.active_apid(), Some(APID_TEST));
    }

    #[test]
    fn subsequent_push_for_different_apid_doesnt_steal_active() {
        // First-push auto-select shouldn't keep firing — the
        // user's pick (or the initial pick) stays sticky as
        // additional channels appear.
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        r.push_line(APID_TEST_2, &synth_line(TEST_PIXEL_2));
        assert_eq!(r.active_apid(), Some(APID_TEST));
    }

    #[test]
    fn push_line_caps_at_max_lines_per_channel() {
        let mut r = LrptImageRenderer::new();
        for _ in 0..MAX_LINES {
            assert_eq!(
                r.push_line(APID_TEST, &synth_line(TEST_PIXEL)),
                PushOutcome::Pushed,
            );
        }
        assert_eq!(r.n_lines(APID_TEST), MAX_LINES);
        // One more push past the cap reports `Capped` — caller's
        // watermark should still advance (further pushes won't
        // succeed no matter how many retries).
        assert_eq!(
            r.push_line(APID_TEST, &synth_line(TEST_PIXEL)),
            PushOutcome::Capped,
        );
        assert_eq!(r.n_lines(APID_TEST), MAX_LINES);
    }

    #[test]
    fn push_line_with_wrong_width_is_dropped() {
        let mut r = LrptImageRenderer::new();
        // IMAGE_WIDTH is 1568; deliberately pass a short slice.
        assert_eq!(
            r.push_line(APID_TEST, &[TEST_PIXEL; 16]),
            PushOutcome::InvalidLine,
        );
        // No surface allocated, no line counted.
        assert_eq!(r.n_lines(APID_TEST), 0);
        assert!(r.known_apids().is_empty());
    }

    #[test]
    fn push_outcome_consumed_pins_watermark_semantics() {
        // Pin the contract `LrptImageView::drain_new_lines`
        // depends on: only `TransientFailure` leaves the row
        // in the source for retry. Per `CodeRabbit` round 3
        // on PR #543.
        assert!(PushOutcome::Pushed.consumed());
        assert!(PushOutcome::Capped.consumed());
        assert!(PushOutcome::InvalidLine.consumed());
        assert!(!PushOutcome::TransientFailure.consumed());
    }

    #[test]
    fn set_active_apid_only_succeeds_for_known_channels() {
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        // Existing APID — switch succeeds.
        assert!(r.set_active_apid(APID_TEST));
        assert_eq!(r.active_apid(), Some(APID_TEST));
        // Unknown APID — switch refused, active stays put.
        assert!(!r.set_active_apid(APID_TEST_2));
        assert_eq!(r.active_apid(), Some(APID_TEST));
    }

    #[test]
    fn clear_drops_all_channels_and_active_selection() {
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        r.push_line(APID_TEST_2, &synth_line(TEST_PIXEL_2));
        r.clear();
        assert!(r.is_empty());
        assert!(r.active_apid().is_none());
        assert!(r.known_apids().is_empty());
    }

    #[test]
    fn pixel_layout_is_argb32_with_grayscale_in_bgr_channels() {
        // Same invariant as the APT renderer test: Cairo's
        // ARGB32 little-endian layout = B, G, R, A. Every
        // channel of the input greyscale value goes into all
        // three colour bytes; alpha is opaque.
        let mut r = LrptImageRenderer::new();
        let mut line = vec![0_u8; IMAGE_WIDTH];
        line[0] = 0x80;
        line[1] = 0xC0;
        r.push_line(APID_TEST, &line);
        let surface = &mut r.channels.get_mut(&APID_TEST).unwrap().surface;
        let data = surface.data().unwrap();
        assert_eq!(&data[0..4], &[0x80, 0x80, 0x80, 0xFF]);
        assert_eq!(&data[4..8], &[0xC0, 0xC0, 0xC0, 0xFF]);
    }

    #[test]
    fn export_png_refuses_when_no_active_channel() {
        let r = LrptImageRenderer::new();
        let path = std::env::temp_dir().join("lrpt-test-no-active-should-not-be-written.png");
        let result = r.export_png(&path);
        assert!(result.is_err());
        assert!(!path.exists(), "no file should be created on empty export");
    }

    #[test]
    fn export_png_refuses_when_active_channel_has_no_data() {
        // Force-set active to an APID we never pushed to (via
        // the test-only path: renderer's HashMap entry exists
        // because we push one line then... wait, no — we need
        // a way to test "active set, but channel empty". Push
        // then clear partway: clear() drops active too, so
        // that's not it. Instead use the renderer's contract:
        // set_active_apid can't succeed for an unknown channel
        // either, so the only reachable "active set, n_lines==0"
        // case is "freshly pushed once, then..." — actually
        // n_lines becomes 1 the moment we push. So the first
        // branch (no active) is the only reachable empty error.
        // We test the second branch by directly mutating the
        // channel's n_lines back to 0 via the test-only access
        // below.
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        r.channels.get_mut(&APID_TEST).unwrap().n_lines = 0;
        let path = std::env::temp_dir().join("lrpt-test-empty-channel-should-not-be-written.png");
        let result = r.export_png(&path);
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn export_png_round_trips_to_a_real_file() {
        use std::io::Read;
        let mut r = LrptImageRenderer::new();
        for _ in 0..16 {
            r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("sdr-ui-lrpt-test-{nanos}.png"));
        r.export_png(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.len() > 0, "PNG file shouldn't be empty");
        let mut header = [0_u8; 8];
        let mut f = std::fs::File::open(&path).unwrap();
        f.read_exact(&mut header).unwrap();
        assert_eq!(
            &header, b"\x89PNG\r\n\x1a\n",
            "exported file isn't a valid PNG (header mismatch)",
        );
        let _ = std::fs::remove_file(&path);
    }
}
