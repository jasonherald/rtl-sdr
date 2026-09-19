//! Live WEFAX (radiofax) chart viewer + per-chart PNG export.
//!
//! WEFAX counterpart to [`crate::sstv_viewer`]. Displays the growing
//! greyscale radiofax chart decoded by `wefax_decode_tap` via the
//! shared [`sdr_radio::wefax_image::WefaxImageHandle`] as it
//! accumulates during reception. WEFAX charts are greyscale-only —
//! one `u8` per pixel, replicated to R = G = B — where SSTV pushes
//! an RGB triple; that's the main structural difference from
//! `sstv_viewer`. The other difference: a WEFAX chart has no fixed
//! total-line-count (unlike SSTV's per-mode height), so the
//! renderer's surface grows with every snapshot rather than staying
//! a fixed size while a `lines_written` cursor advances inside it.
//!
//! Three pieces:
//!
//! * [`WefaxImageRenderer`] — pure Cairo renderer. Owns an ARGB32
//!   [`cairo::ImageSurface`] sized for the current chart
//!   ([`sdr_dsp::wefax::PIXELS_PER_LINE`] wide, growing in height as
//!   lines arrive). No GTK dependency, fully unit-testable.
//! * [`WefaxImageView`] — GTK widget wrapping a renderer. Driven by
//!   the `DspToUi::WefaxLineDecoded` handler in
//!   `window/dsp_events.rs`, which triggers a `snapshot()` + redraw
//!   on each new line. Cloneable (all state is `Rc`-shared) so
//!   toolbar closures can hold their own handle.
//! * [`open_wefax_viewer_window`] — opens the view in a non-modal
//!   transient window. Header bar: Pause / Resume + Export PNG; the
//!   window-title subtitle reflects the decoder's
//!   [`sdr_dsp::wefax::WefaxState`] phase.
//!
//! [`connect_wefax_action`] wires the `app.wefax-open` action
//! (`Ctrl+Shift+F`). Activating it opens a viewer window and
//! registers the [`WefaxImageHandle`] with the DSP controller via
//! `UiToDsp::SetWefaxImage`.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{cairo, gio, glib};
use libadwaita as adw;

use sdr_dsp::wefax::WefaxState;
use sdr_radio::wefax_image::{WefaxImageHandle, WefaxSnapshot};

use crate::viewer::ViewerError;

mod window;

pub use window::{connect_wefax_action, open_wefax_viewer_if_needed, open_wefax_viewer_window};

// ─── Constants ─────────────────────────────────────────────────────────────

/// Default viewer window size. WEFAX charts are
/// [`sdr_dsp::wefax::PIXELS_PER_LINE`] px wide (1809) and grow
/// arbitrarily tall as lines arrive — well past a typical window's
/// height for a full reception. The drawing area is wrapped in a
/// `GtkScrolledWindow` (see `window::open_wefax_viewer_window`) that
/// grows with the chart and auto-follows the newest line rather than
/// shrinking the whole image to fit.
const VIEWER_WINDOW_WIDTH: i32 = 900;
const VIEWER_WINDOW_HEIGHT: i32 = 700;

/// Background painted before any pixel data arrives, or around the
/// chart when the window's aspect ratio doesn't match the image's.
const BACKGROUND_RGB: [f64; 3] = [0.05, 0.05, 0.06];

/// Subtitle shown before the first `DspToUi::WefaxState` arrives (or
/// after [`WefaxImageView::clear`]).
const WEFAX_VIEWER_PLACEHOLDER_SUBTITLE: &str = "Idle";

/// Pixel tolerance for the "scrolled to bottom" auto-follow check in
/// [`WefaxImageView::follow_scroll_to_bottom`]. Mirrors
/// `sidebar::orbcomm_panel::SCROLL_BOTTOM_TOLERANCE_PX` —
/// `GtkAdjustment` values are fractional, so an exact compare against
/// `upper() - page_size()` would miss sub-pixel rests.
const SCROLL_BOTTOM_TOLERANCE_PX: f64 = 1.0;

// ─── Pure pixel packing ─────────────────────────────────────────────────────

/// Convert up to `width` greyscale samples into Cairo ARGB32 bytes
/// (little-endian BGRA), replicating each grey value across B, G, R
/// and setting A = 0xFF. A `gray` slice shorter than `width` packs
/// the missing columns as black rather than panicking — the same
/// defensive-bounds contract as the RGB copy loop in
/// `sstv_viewer::SstvImageRenderer::update_from_snapshot`.
///
/// Pure and GTK-free — the one function in this module worth unit
/// testing on its own; the GTK widgets around it are smoke-tested.
#[must_use]
pub fn gray_to_argb(gray: &[u8], width: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * 4);
    for col in 0..width {
        let g = gray.get(col).copied().unwrap_or(0);
        out.push(g); // B
        out.push(g); // G
        out.push(g); // R
        out.push(0xFF); // A
    }
    out
}

// ─── Pure Cairo renderer ────────────────────────────────────────────────────

/// Pure Cairo renderer for a live WEFAX chart.
///
/// Owns a Cairo ARGB32 [`cairo::ImageSurface`] sized to the current
/// snapshot. Unlike [`crate::sstv_viewer::SstvImageRenderer`] (fixed
/// per-mode dimensions, incremental row writes), a WEFAX chart's
/// height grows with every snapshot, so `update_from_snapshot`
/// rebuilds the surface and re-blits the full buffer whenever new
/// rows have arrived. Cadence is ~2 lines/sec (120 lpm), so a full
/// re-blit is cheap in practice.
pub struct WefaxImageRenderer {
    /// Persistent ARGB32 surface, sized to `(width, height)`. `None`
    /// before the first snapshot / after [`Self::clear`].
    surface: Option<cairo::ImageSurface>,
    /// Chart width in pixels of the current surface.
    width: u32,
    /// Chart height (rows) of the current surface.
    height: u32,
    /// Rows currently painted into `surface` — mirrors `height`
    /// after every successful update; kept as a separate field to
    /// match the shape of the SSTV renderer's "how much has been
    /// drawn" tracking.
    lines_written: u32,
    /// Most recently received snapshot, cached for PNG export.
    last_snapshot: Option<WefaxSnapshot>,
}

impl Default for WefaxImageRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl WefaxImageRenderer {
    /// Build an empty renderer (no surface allocated yet).
    #[must_use]
    pub fn new() -> Self {
        Self {
            surface: None,
            width: 0,
            height: 0,
            lines_written: 0,
            last_snapshot: None,
        }
    }

    /// Update the renderer from a live [`WefaxSnapshot`].
    ///
    /// Rebuilds the surface and re-blits every row whenever the
    /// chart has grown (more rows than last time) or its width
    /// changed (a new chart started). Returns `true` when the
    /// surface was repainted (the view should queue a redraw);
    /// `false` when nothing changed or on a surface error (logged,
    /// not propagated — a failed redraw shouldn't kill the UI).
    pub fn update_from_snapshot(&mut self, snap: WefaxSnapshot) -> bool {
        if snap.width == 0 {
            // `WefaxSnapshot` is public, so a caller could hand us a
            // zero-width snapshot; `chunks_exact(0)` below would panic.
            // There is nothing to render — drop it at the boundary.
            return false;
        }
        if snap.height < self.lines_written {
            // The shared buffer shrank — `WefaxImageHandle::take_completed`
            // reset it to start a new chart (or `clear()` reset it
            // directly). Drop the stale watermark + surface so the
            // fresh/shorter chart repaints from the top instead of
            // being silently swallowed by the `==` check below, which
            // otherwise wouldn't fire again until the new chart's
            // height happened to reach the old one's — the live-hit
            // "new lines tile in at the bottom" bug from the
            // whole-branch review.
            self.clear();
        } else if snap.height == self.lines_written && snap.width == self.width {
            self.last_snapshot = Some(snap);
            return false;
        }
        self.rebuild_surface(snap.width, snap.height);
        let Some(ref mut surface) = self.surface else {
            return false;
        };

        let stride = match usize::try_from(surface.stride()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("WEFAX renderer: invalid stride: {e}");
                return false;
            }
        };
        let mut data = match surface.data() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("WEFAX renderer: surface data lock failed: {e}");
                return false;
            }
        };

        let w = snap.width as usize;
        for (row, row_pixels) in snap.pixels.chunks_exact(w).enumerate() {
            let argb = gray_to_argb(row_pixels, w);
            let row_offset = row * stride;
            let len = argb.len().min(data.len().saturating_sub(row_offset));
            data[row_offset..row_offset + len].copy_from_slice(&argb[..len]);
        }
        drop(data);

        self.lines_written = snap.height;
        self.last_snapshot = Some(snap);
        true
    }

    /// Rebuild the Cairo surface for new chart dimensions.
    #[allow(clippy::cast_possible_wrap)]
    fn rebuild_surface(&mut self, width: u32, height: u32) {
        match cairo::ImageSurface::create(cairo::Format::ARgb32, width as i32, height as i32) {
            Ok(s) => {
                self.surface = Some(s);
                self.width = width;
                self.height = height;
            }
            Err(e) => {
                tracing::warn!("WEFAX renderer: surface creation failed ({width}×{height}): {e}");
                self.surface = None;
                self.width = 0;
                self.height = 0;
            }
        }
    }

    /// Reset to empty (ready for a new chart).
    pub fn clear(&mut self) {
        self.surface = None;
        self.width = 0;
        self.height = 0;
        self.lines_written = 0;
        self.last_snapshot = None;
    }

    /// Current painted chart dimensions `(width, rows)` — the size the
    /// wrapping drawing area's content should track. `(0, 0)` before the
    /// first snapshot / after [`Self::clear`]. Uses `lines_written` (rows
    /// actually painted) to match [`Self::render`]'s own height basis;
    /// [`WefaxImageView::set_paused`] reads this to re-sync the canvas to
    /// data accumulated while paused.
    #[must_use]
    pub fn content_dims(&self) -> (u32, u32) {
        (self.width, self.lines_written)
    }

    /// Paint the current surface into `cr`, scaled to fit `(width, height)`
    /// while preserving the chart's aspect ratio. Top-aligned so the
    /// live chart builds downward visually. No-op when no data has
    /// arrived yet.
    ///
    /// # Errors
    ///
    /// Returns [`ViewerError::Cairo`] on any Cairo operation failure.
    #[allow(clippy::cast_precision_loss)]
    pub fn render(&self, cr: &cairo::Context, width: i32, height: i32) -> Result<(), ViewerError> {
        cr.set_source_rgb(BACKGROUND_RGB[0], BACKGROUND_RGB[1], BACKGROUND_RGB[2]);
        cr.paint().map_err(|e| ViewerError::Cairo {
            op: "background paint",
            source: e,
        })?;

        let Some(ref surface) = self.surface else {
            return Ok(());
        };
        if self.lines_written == 0 || width <= 0 || height <= 0 {
            return Ok(());
        }

        let img_w = f64::from(self.width);
        let img_h = f64::from(self.lines_written);
        let scale = (f64::from(width) / img_w).min(f64::from(height) / img_h);
        let off_x = (f64::from(width) - img_w * scale) / 2.0;

        cr.save().map_err(|e| ViewerError::Cairo {
            op: "save",
            source: e,
        })?;
        cr.translate(off_x, 0.0);
        cr.scale(scale, scale);
        cr.set_source_surface(surface, 0.0, 0.0)
            .map_err(|e| ViewerError::Cairo {
                op: "set_source_surface",
                source: e,
            })?;
        cr.rectangle(0.0, 0.0, img_w, img_h);
        cr.fill().map_err(|e| ViewerError::Cairo {
            op: "image fill",
            source: e,
        })?;
        cr.restore().map_err(|e| ViewerError::Cairo {
            op: "restore",
            source: e,
        })?;
        Ok(())
    }

    /// Export the current chart to a PNG file, synchronously.
    ///
    /// **GTK callers must use `gio::spawn_blocking`** to avoid
    /// freezing the main loop during the encode.
    ///
    /// # Errors
    ///
    /// Returns [`ViewerError::EmptyChannel`] when no data has been
    /// received, or a `Cairo` / `Io` / `PngEncode` variant on failure.
    pub fn export_png(&self, path: &Path) -> Result<(), ViewerError> {
        let Some(ref snap) = self.last_snapshot else {
            return Err(ViewerError::EmptyChannel { apid: None });
        };
        if snap.height == 0 {
            return Err(ViewerError::EmptyChannel { apid: None });
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| ViewerError::Io {
                op: "create_dir_all",
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        write_wefax_gray_png(path, &snap.pixels, snap.width, snap.height)
    }

    /// Cheap clone of the latest snapshot for handing to a
    /// `gio::spawn_blocking` worker.
    #[must_use]
    pub fn snapshot_for_export(&self) -> Option<WefaxSnapshot> {
        self.last_snapshot.clone()
    }
}

/// Write a greyscale WEFAX chart to `path` as a PNG via Cairo.
///
/// Mirrors [`crate::sstv_viewer::write_sstv_rgb_png`] but packs a
/// flat `Vec<u8>` (one byte per pixel) via [`gray_to_argb`] instead
/// of RGB triples. `Cairo` objects are `!Send` but we only create
/// and use them within this function, so calling it from a
/// `gio::spawn_blocking` worker is safe.
///
/// # Errors
///
/// Returns [`ViewerError`] on any Cairo, I/O, or PNG encoding failure.
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
pub fn write_wefax_gray_png(
    path: &Path,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<(), ViewerError> {
    if pixels.is_empty() || width == 0 || height == 0 {
        return Err(ViewerError::EmptyChannel { apid: None });
    }
    let mut surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, width as i32, height as i32).map_err(
            |e| ViewerError::Cairo {
                op: "export surface",
                source: e,
            },
        )?;

    let stride = usize::try_from(surface.stride())?;
    {
        let mut data = surface.data()?;
        let w = width as usize;
        for (row, row_pixels) in pixels.chunks_exact(w).enumerate() {
            let argb = gray_to_argb(row_pixels, w);
            let row_offset = row * stride;
            let len = argb.len().min(data.len().saturating_sub(row_offset));
            data[row_offset..row_offset + len].copy_from_slice(&argb[..len]);
        }
    }
    let mut file = std::fs::File::create(path).map_err(|e| ViewerError::Io {
        op: "file create",
        path: path.to_path_buf(),
        source: e,
    })?;
    surface.write_to_png(&mut file)?;
    tracing::info!(?path, width, height, "WEFAX chart exported to PNG");
    Ok(())
}

/// Map a decoder phase to the short status label shown in the
/// viewer's window-title subtitle.
fn wefax_state_label(state: WefaxState) -> &'static str {
    match state {
        WefaxState::Idle => "Idle",
        WefaxState::Phasing => "Phasing",
        WefaxState::Imaging => "Imaging",
        WefaxState::Stopped => "Stopped",
    }
}

// ─── GTK widget ─────────────────────────────────────────────────────────────

/// Live WEFAX chart viewer widget.
///
/// Holds a `DrawingArea` plus shared rendering state. Cloneable —
/// every clone holds an `Rc` to the same renderer + pause flag, so
/// toolbar callbacks can hold their own handle without lifetime
/// dance. Driven by the `DspToUi::WefaxLineDecoded` handler in
/// `window/dsp_events.rs`, which calls
/// [`WefaxImageView::update_from_handle`] to pull the latest
/// snapshot from the shared [`WefaxImageHandle`] and queue a redraw.
#[derive(Clone)]
pub struct WefaxImageView {
    drawing_area: gtk4::DrawingArea,
    renderer: Rc<RefCell<WefaxImageRenderer>>,
    paused: Rc<Cell<bool>>,
    /// Optional shared-source handle, set via [`Self::set_handle`]
    /// after construction. [`Self::clear`] also clears the
    /// in-flight buffer here so the next [`Self::update_from_handle`]
    /// doesn't replay the rows the user just cleared. Mirrors
    /// `SstvImageView::handle`.
    handle: Rc<RefCell<Option<WefaxImageHandle>>>,
    /// Optional handle to the window's [`adw::WindowTitle`] so
    /// [`Self::set_state_label`] can refresh the subtitle. `None`
    /// for tests / detached views that don't have a window.
    title_widget: Rc<RefCell<Option<adw::WindowTitle>>>,
    /// Vertical `GtkAdjustment` of the wrapping `ScrolledWindow`, set
    /// via [`Self::set_scroll_adjustment`] once
    /// `window::open_wefax_viewer_window` builds it. `None` for tests
    /// / detached views. A WEFAX chart has no fixed height and a long
    /// reception runs well past the window's visible area —
    /// [`Self::update_from_handle`] auto-follows this to the bottom
    /// as new lines arrive, but only when the user was already there
    /// (mirrors `sidebar::orbcomm_panel::OrbcommPanelHandles::append_log_entry`'s
    /// "`was_at_bottom`" + deferred-idle pattern).
    scroll_adjustment: Rc<RefCell<Option<gtk4::Adjustment>>>,
}

impl Default for WefaxImageView {
    fn default() -> Self {
        Self::new()
    }
}

impl WefaxImageView {
    /// Build a fresh view with a blank renderer.
    #[must_use]
    pub fn new() -> Self {
        let renderer = Rc::new(RefCell::new(WefaxImageRenderer::new()));
        let paused = Rc::new(Cell::new(false));

        let drawing_area = gtk4::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        let renderer_for_draw = Rc::clone(&renderer);
        drawing_area.set_draw_func(move |_area, cr, w, h| {
            if let Err(e) = renderer_for_draw.borrow().render(cr, w, h) {
                tracing::warn!("WEFAX render failed: {e}");
            }
        });

        Self {
            drawing_area,
            renderer,
            paused,
            handle: Rc::new(RefCell::new(None)),
            title_widget: Rc::new(RefCell::new(None)),
            scroll_adjustment: Rc::new(RefCell::new(None)),
        }
    }

    /// Attach a shared [`WefaxImageHandle`] so [`Self::clear`] also
    /// wipes the source-side pixel buffer.
    pub fn set_handle(&self, handle: WefaxImageHandle) {
        *self.handle.borrow_mut() = Some(handle);
    }

    /// Attach the window's [`adw::WindowTitle`] so
    /// [`Self::set_state_label`] can refresh the subtitle.
    /// Idempotent — replaces any previously-attached title.
    pub fn set_title_widget(&self, title: adw::WindowTitle) {
        *self.title_widget.borrow_mut() = Some(title);
    }

    /// Attach the vertical `GtkAdjustment` of the wrapping
    /// `ScrolledWindow` so [`Self::update_from_handle`] can
    /// auto-follow the newest line. Idempotent — replaces any
    /// previously-attached adjustment.
    pub fn set_scroll_adjustment(&self, adjustment: gtk4::Adjustment) {
        *self.scroll_adjustment.borrow_mut() = Some(adjustment);
    }

    /// Update the viewer's window-title subtitle to reflect the
    /// decoder's phase (Idle / Phasing / Imaging / Stopped). No-op
    /// if no title widget has been attached (test / detached views).
    /// Called from `window/dsp_events.rs`'s `DspToUi::WefaxState` arm.
    pub fn set_state_label(&self, state: WefaxState) {
        if let Some(title) = self.title_widget.borrow().as_ref() {
            title.set_subtitle(wefax_state_label(state));
        }
    }

    /// The underlying `GtkDrawingArea`. Pack this into a layout container.
    #[must_use]
    pub fn drawing_area(&self) -> &gtk4::DrawingArea {
        &self.drawing_area
    }

    /// Pull the latest snapshot from `handle` and update the renderer.
    /// Queues a redraw only if the surface was repainted and the
    /// viewer is not paused. Always buffers data even when paused so
    /// nothing is lost while the user inspects the chart.
    pub fn update_from_handle(&self, handle: &WefaxImageHandle) {
        let Some(snap) = handle.snapshot() else {
            return;
        };
        let (width, height) = (snap.width, snap.height);
        let changed = self.renderer.borrow_mut().update_from_snapshot(snap);
        if !changed {
            return;
        }
        // While paused, the renderer keeps accumulating (above) but the
        // visible canvas stays frozen — `set_paused` re-syncs it on
        // resume. The size-growth MUST stay inside this gate: growing a
        // `DrawingArea`'s content size forces GTK to repaint the surface
        // even without an explicit `queue_draw`, so mutating it while
        // paused defeats Pause (the regression fixed here — it used to
        // run unconditionally before the gate).
        if self.paused.get() {
            return;
        }
        self.apply_to_canvas(width, height);
    }

    /// Grow the drawing area to the chart's current pixel size, queue a
    /// repaint, and auto-follow to the newest line. The single choke
    /// point for every visible-canvas mutation, so the `paused` gate has
    /// one place to guard. A WEFAX chart has no fixed height and easily
    /// outgrows the window over a full reception; `render()` still
    /// scale-fits defensively, but with the allocation tracking the
    /// surface 1:1 the chart paints at native resolution rather than
    /// shrinking away.
    fn apply_to_canvas(&self, width: u32, height: u32) {
        self.drawing_area
            .set_content_width(i32::try_from(width).unwrap_or(i32::MAX));
        self.drawing_area
            .set_content_height(i32::try_from(height).unwrap_or(i32::MAX));
        self.drawing_area.queue_draw();
        self.follow_scroll_to_bottom();
    }

    /// Auto-scroll the wrapping `ScrolledWindow` to the newest line,
    /// but only when the user was already scrolled to the bottom —
    /// mirrors
    /// `sidebar::orbcomm_panel::OrbcommPanelHandles::append_log_entry`'s
    /// "`was_at_bottom`" + deferred-idle pattern. The scroll is
    /// deferred to the next main-loop idle because `GtkScrolledWindow`
    /// recomputes its adjustment bounds on the next size-allocate
    /// pass, not synchronously inside `set_content_height`. No-op if
    /// no adjustment has been attached (test / detached views, or a
    /// viewer window not yet built).
    fn follow_scroll_to_bottom(&self) {
        let Some(adj) = self.scroll_adjustment.borrow().clone() else {
            return;
        };
        let was_at_bottom = (adj.value() + adj.page_size() - adj.upper()).abs()
            < SCROLL_BOTTOM_TOLERANCE_PX
            || adj.upper() <= adj.page_size();
        if !was_at_bottom {
            return;
        }
        let adj_weak = adj.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(adj) = adj_weak.upgrade() {
                adj.set_value(adj.upper());
            }
        });
    }

    /// Wipe all buffered data and queue a redraw. Also clears the
    /// shared [`WefaxImageHandle`] (if attached via
    /// [`Self::set_handle`]) so the next [`Self::update_from_handle`]
    /// doesn't replay the rows we just cleared. Resets the
    /// window-title subtitle to "Idle" and collapses the drawing
    /// area's content size + scroll position back to the top so a
    /// new chart starts fresh rather than leaving the old chart's
    /// scrollbar / position behind.
    pub fn clear(&self) {
        if let Some(handle) = self.handle.borrow().as_ref() {
            handle.clear();
        }
        self.renderer.borrow_mut().clear();
        self.drawing_area.set_content_width(0);
        self.drawing_area.set_content_height(0);
        if let Some(adj) = self.scroll_adjustment.borrow().as_ref() {
            adj.set_value(0.0);
        }
        self.drawing_area.queue_draw();
        self.set_state_label(WefaxState::Idle);
    }

    /// Toggle pause / resume. Pausing freezes the visible canvas;
    /// snapshots pushed while paused still accumulate so nothing is
    /// lost, and become visible on resume via a forced single redraw.
    pub fn set_paused(&self, paused: bool) {
        let was_paused = self.paused.replace(paused);
        if was_paused && !paused {
            // Resume: the canvas was frozen at its pre-pause size while
            // updates accumulated in the renderer. Re-sync the drawing
            // area to the renderer's current dimensions (grown while
            // paused) and repaint, so the accumulated lines become
            // visible instead of being clipped to the stale content size.
            let (width, height) = self.renderer.borrow().content_dims();
            self.apply_to_canvas(width, height);
        }
    }

    /// `true` when the view is currently paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    /// Asynchronously export the current chart to PNG.
    ///
    /// Snapshots the pixel data on the GTK main thread (cheap clone),
    /// spawns a `gio::spawn_blocking` worker for the CPU-heavy encode,
    /// then marshals the result back to the main context where
    /// `on_complete` fires. Mirrors
    /// [`crate::sstv_viewer::SstvImageView::export_png_async`].
    pub fn export_png_async(
        &self,
        path: PathBuf,
        on_complete: impl FnOnce(Result<(), ViewerError>) + 'static,
    ) {
        let snap = self.renderer.borrow().snapshot_for_export();
        let Some(snap) = snap else {
            on_complete(Err(ViewerError::EmptyChannel { apid: None }));
            return;
        };
        if snap.height == 0 {
            on_complete(Err(ViewerError::EmptyChannel { apid: None }));
            return;
        }
        glib::spawn_future_local(async move {
            let join = gio::spawn_blocking(move || {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent).map_err(|e| ViewerError::Io {
                        op: "create_dir_all",
                        path: parent.to_path_buf(),
                        source: e,
                    })?;
                }
                write_wefax_gray_png(&path, &snap.pixels, snap.width, snap.height)
            })
            .await;
            let result = match join {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("WEFAX PNG export worker panicked: {e:?}");
                    Err(ViewerError::InvalidBuffer(
                        "PNG export worker panicked — see logs".to_string(),
                    ))
                }
            };
            on_complete(result);
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
