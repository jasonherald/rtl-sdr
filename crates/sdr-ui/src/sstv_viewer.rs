//! Live ISS SSTV image viewer + per-image PNG export.
//!
//! SSTV counterpart to [`crate::apt_viewer`] and [`crate::lrpt_viewer`].
//! Displays the growing SSTV image decoded by `sstv_decode_tap` via the
//! shared [`sdr_radio::sstv_image::SstvImageHandle`] as it accumulates
//! during a satellite pass. ARISS events typically send ~12 images per
//! 10-minute pass window; each image is announced by a VIS header, decoded
//! line by line (~1 line/sec for PD120), and completed at
//! `SstvEvent::ImageComplete`.
//!
//! Three pieces:
//!
//! * [`SstvImageRenderer`] — pure Cairo renderer. Owns an ARGB32
//!   [`cairo::ImageSurface`] sized for the current image (PD120: 640 × 496).
//!   No GTK dependency, fully unit-testable.
//! * [`SstvImageView`] — GTK widget wrapping a renderer. Driven by the
//!   `DspToUi::SstvLineDecoded` handler in `window.rs` which triggers a
//!   `snapshot()` + redraw on each new line. Cloneable (all state is
//!   `Rc`-shared) so toolbar closures can hold their own handle.
//! * [`open_sstv_viewer_window`] — opens the view in a non-modal transient
//!   window. Header bar: Pause / Resume + Export PNG.
//!
//! [`connect_sstv_action`] wires the `app.sstv-open` action
//! (`Ctrl+Shift+V`). Activating it opens a viewer window and registers
//! the [`SstvImageHandle`] with the DSP controller via
//! `UiToDsp::SetSstvImage`.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{cairo, gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use sdr_radio::sstv_image::{SstvImageHandle, SstvSnapshot};

use crate::messages::UiToDsp;
use crate::viewer::ViewerError;

// ─── Constants ─────────────────────────────────────────────────────────────

/// Default viewer window size. ISS SSTV in PD120 mode produces 640 × 496
/// images; headroom is given for the header bar.
const VIEWER_WINDOW_WIDTH: i32 = 700;
const VIEWER_WINDOW_HEIGHT: i32 = 560;

/// Background painted before any pixel data arrives, or around the image
/// when the window is wider/taller than the image aspect ratio.
const BACKGROUND_RGB: [f64; 3] = [0.05, 0.05, 0.06];

// ─── Pure Cairo renderer ────────────────────────────────────────────────────

/// Pure Cairo renderer for a live SSTV image.
///
/// Owns a persistent ARGB32 [`cairo::ImageSurface`]. `update_from_snapshot`
/// writes the latest rows into the surface; `render` and `export_png` read
/// from it. No allocation on each line arrival.
pub struct SstvImageRenderer {
    /// Persistent ARGB32 surface. Rebuilt when image dimensions change
    /// (e.g. on a new VIS detection for a different mode). `None` before
    /// the first snapshot arrives.
    surface: Option<cairo::ImageSurface>,
    /// Dimensions of the current surface. `(0, 0)` when `surface` is None.
    width: u32,
    height: u32,
    /// How many lines have been written so far.
    lines_written: u32,
    /// Snapshot of the most-recently received pixel data, for export.
    last_snapshot: Option<SstvSnapshot>,
}

impl Default for SstvImageRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl SstvImageRenderer {
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

    /// Update the renderer from a live [`SstvSnapshot`].
    ///
    /// If the image dimensions changed since the last update (i.e. a new
    /// VIS header for a different mode was detected), the surface is rebuilt.
    /// Writes all lines present in the snapshot into the Cairo surface.
    ///
    /// Returns `true` when at least one new line was written (the view
    /// should queue a redraw). Returns `false` on surface errors (logged,
    /// not propagated — a failed redraw shouldn't kill the UI).
    pub fn update_from_snapshot(&mut self, snap: SstvSnapshot) -> bool {
        let mut old_lines = self.lines_written;

        // Rebuild surface on dimension change (new image / new mode).
        if snap.width != self.width || snap.height != self.height {
            self.rebuild_surface(snap.width, snap.height);
            old_lines = 0;
        } else if snap.lines_written < old_lines {
            // Same dimensions but the line counter rolled back — a
            // new SSTV image started with the same mode (e.g.
            // PD120 → PD120). Without this branch the delta logic
            // below sees `new_lines < old_lines` → false → no blit,
            // and stale pixels from the previous image leak through.
            // Clear the surface and reset the cursor so the new
            // image paints from row 0. Per CodeRabbit #7 on PR #599.
            self.clear();
            old_lines = 0;
        }
        let Some(ref mut surface) = self.surface else {
            return false;
        };

        let stride = match usize::try_from(surface.stride()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("SSTV renderer: invalid stride: {e}");
                return false;
            }
        };
        let mut data = match surface.data() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("SSTV renderer: surface data lock failed: {e}");
                return false;
            }
        };

        let w = snap.width as usize;
        let new_lines = snap.lines_written;
        // Write only the lines that arrived since the last update.
        for row in (old_lines as usize)..(new_lines as usize) {
            let row_offset = row * stride;
            let px_start = row * w;
            let px_end = px_start + w;
            if px_end > snap.pixels.len() {
                break;
            }
            for (col, &[r, g, b]) in snap.pixels[px_start..px_end].iter().enumerate() {
                let p = row_offset + col * 4;
                if p + 3 < data.len() {
                    // Cairo ARGB32 little-endian: B, G, R, A.
                    data[p] = b;
                    data[p + 1] = g;
                    data[p + 2] = r;
                    data[p + 3] = 0xFF;
                }
            }
        }
        drop(data);

        let changed = new_lines > old_lines;
        self.lines_written = new_lines;
        // Move the snapshot directly into the cache; no clone. The
        // pixel borrow above ends with `drop(data)`, so NLL lets us
        // consume `snap` here. Per CR round 3 on PR #599.
        self.last_snapshot = Some(snap);
        changed
    }

    /// Rebuild the Cairo surface for new image dimensions.
    #[allow(clippy::cast_possible_wrap)]
    fn rebuild_surface(&mut self, width: u32, height: u32) {
        match cairo::ImageSurface::create(cairo::Format::ARgb32, width as i32, height as i32) {
            Ok(s) => {
                self.surface = Some(s);
                self.width = width;
                self.height = height;
                self.lines_written = 0;
            }
            Err(e) => {
                tracing::warn!("SSTV renderer: surface creation failed ({width}×{height}): {e}");
                self.surface = None;
                self.width = 0;
                self.height = 0;
                self.lines_written = 0;
            }
        }
    }

    /// Reset to empty (ready for a new pass).
    pub fn clear(&mut self) {
        if let Some(ref mut surface) = self.surface
            && let Ok(mut data) = surface.data()
        {
            data.fill(0);
        }
        self.lines_written = 0;
        self.last_snapshot = None;
    }

    /// Paint the current surface into `cr`, scaled to fit `(width, height)`
    /// while preserving the image's aspect ratio. Top-aligned so the live
    /// pass builds downward visually. No-op when no data has arrived yet.
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

    /// Export the current image to a PNG file, synchronously.
    ///
    /// **GTK callers must use `gio::spawn_blocking`** to avoid
    /// freezing the main loop during the encode.
    ///
    /// # Errors
    ///
    /// Returns [`ViewerError::EmptyChannel`] when no data has been received,
    /// or a `Cairo` / `Io` / `PngEncode` variant on failure.
    #[allow(clippy::cast_possible_wrap)]
    pub fn export_png(&self, path: &Path) -> Result<(), ViewerError> {
        let Some(ref snap) = self.last_snapshot else {
            return Err(ViewerError::EmptyChannel { apid: None });
        };
        if snap.lines_written == 0 {
            return Err(ViewerError::EmptyChannel { apid: None });
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| ViewerError::Io {
                op: "create_dir_all",
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        write_sstv_rgb_png(path, &snap.pixels, snap.width, snap.height)
    }

    /// Cheap snapshot of the latest pixel data for handing to a
    /// `gio::spawn_blocking` worker.
    #[must_use]
    pub fn snapshot_for_export(&self) -> Option<SstvSnapshot> {
        self.last_snapshot.clone()
    }
}

/// Write an RGB SSTV image to `path` as a PNG via Cairo.
///
/// Mirrors [`crate::lrpt_viewer::write_rgb_png`] but works with the
/// SSTV `Vec<[u8; 3]>` format directly. `Cairo` objects are `!Send`
/// but we only create and use them within this function, so calling
/// it from a `gio::spawn_blocking` worker is safe.
///
/// # Errors
///
/// Returns [`ViewerError`] on any Cairo, I/O, or PNG encoding failure.
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
pub fn write_sstv_rgb_png(
    path: &Path,
    pixels: &[[u8; 3]],
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
            let row_offset = row * stride;
            for (col, &[r, g, b]) in row_pixels.iter().enumerate() {
                let p = row_offset + col * 4;
                if p + 3 < data.len() {
                    data[p] = b;
                    data[p + 1] = g;
                    data[p + 2] = r;
                    data[p + 3] = 0xFF;
                }
            }
        }
    }
    let mut file = std::fs::File::create(path).map_err(|e| ViewerError::Io {
        op: "file create",
        path: path.to_path_buf(),
        source: e,
    })?;
    surface.write_to_png(&mut file)?;
    tracing::info!(?path, width, height, "SSTV image exported to PNG");
    Ok(())
}

// ─── GTK widget ─────────────────────────────────────────────────────────────

/// Live SSTV image viewer widget.
///
/// Holds a `DrawingArea` plus shared rendering state. Cloneable —
/// every clone holds an `Rc` to the same renderer + pause flag, so
/// toolbar callbacks can hold their own handle without lifetime dance.
/// Driven by the `DspToUi::SstvLineDecoded` handler in `window.rs`,
/// which calls [`SstvImageView::update_from_handle`] to pull the latest
/// snapshot from the shared [`SstvImageHandle`] and queue a redraw.
#[derive(Clone)]
pub struct SstvImageView {
    drawing_area: gtk4::DrawingArea,
    renderer: Rc<RefCell<SstvImageRenderer>>,
    paused: Rc<Cell<bool>>,
    /// Optional shared-source handle. When set, [`Self::clear`] also
    /// clears the in-flight pixel buffer in the shared
    /// [`SstvImageHandle`], so the next [`Self::update_from_handle`]
    /// doesn't replay the rows the user just cleared. Set via
    /// [`Self::set_handle`] after construction.
    /// Per CR round 4 on PR #599.
    handle: Rc<RefCell<Option<SstvImageHandle>>>,
}

impl Default for SstvImageView {
    fn default() -> Self {
        Self::new()
    }
}

impl SstvImageView {
    /// Build a fresh view with a blank renderer.
    #[must_use]
    pub fn new() -> Self {
        let renderer = Rc::new(RefCell::new(SstvImageRenderer::new()));
        let paused = Rc::new(Cell::new(false));

        let drawing_area = gtk4::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        let renderer_for_draw = Rc::clone(&renderer);
        drawing_area.set_draw_func(move |_area, cr, w, h| {
            if let Err(e) = renderer_for_draw.borrow().render(cr, w, h) {
                tracing::warn!("SSTV render failed: {e}");
            }
        });

        Self {
            drawing_area,
            renderer,
            paused,
            handle: Rc::new(RefCell::new(None)),
        }
    }

    /// Attach a shared [`SstvImageHandle`] so [`Self::clear`] also
    /// wipes the source-side pixel buffer. Without this, a Clear
    /// button click would only wipe the local renderer cache and
    /// the next snapshot replay would restore every cleared row.
    /// Idempotent — replaces any previously-set handle. Per CR
    /// round 4 on PR #599.
    pub fn set_handle(&self, handle: SstvImageHandle) {
        *self.handle.borrow_mut() = Some(handle);
    }

    /// The underlying `GtkDrawingArea`. Pack this into a layout container.
    #[must_use]
    pub fn drawing_area(&self) -> &gtk4::DrawingArea {
        &self.drawing_area
    }

    /// Pull the latest snapshot from `handle` and update the renderer.
    /// Queues a redraw only if a new line arrived and the viewer is
    /// not paused. Always buffers data even when paused so nothing is
    /// lost while the user inspects the image.
    pub fn update_from_handle(&self, handle: &SstvImageHandle) {
        if let Some(snap) = handle.snapshot() {
            // Move the snapshot into the renderer; it caches a single
            // copy in `last_snapshot` for redraws and avoids cloning.
            // Per CR round 3 on PR #599.
            let changed = self.renderer.borrow_mut().update_from_snapshot(snap);
            if changed && !self.paused.get() {
                self.drawing_area.queue_draw();
            }
        }
    }

    /// Wipe all buffered data and queue a redraw. Also clears the
    /// shared [`SstvImageHandle`] (if attached via
    /// [`Self::set_handle`]) so the next [`Self::update_from_handle`]
    /// doesn't replay the rows we just cleared. Per CR round 4 on
    /// PR #599.
    pub fn clear(&self) {
        if let Some(handle) = self.handle.borrow().as_ref() {
            handle.clear();
        }
        self.renderer.borrow_mut().clear();
        self.drawing_area.queue_draw();
    }

    /// Toggle pause / resume. Pausing freezes the visible canvas;
    /// snapshots pushed while paused still accumulate so nothing is
    /// lost, and become visible on resume via a forced single redraw.
    pub fn set_paused(&self, paused: bool) {
        let was_paused = self.paused.replace(paused);
        if was_paused && !paused {
            self.drawing_area.queue_draw();
        }
    }

    /// `true` when the view is currently paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    /// Asynchronously export the current image to PNG.
    ///
    /// Snapshots the pixel data on the GTK main thread (cheap clone),
    /// spawns a `gio::spawn_blocking` worker for the CPU-heavy encode,
    /// then marshals the result back to the main context where
    /// `on_complete` fires. Per the APT viewer's async export pattern.
    ///
    /// `on_complete` runs on the GTK main thread, so it can safely
    /// capture `Rc`-shared widgets and post toasts.
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
        glib::spawn_future_local(async move {
            let join = gio::spawn_blocking(move || {
                // Ensure the parent directory exists. First-run exports
                // target `~/sdr-recordings/sstv-iss-{ts}/` which the
                // recorder creates at AOS, but a manual export to a
                // brand-new directory should work too. Per CR round 2
                // on PR #599.
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent).map_err(|e| ViewerError::Io {
                        op: "create_dir_all",
                        path: parent.to_path_buf(),
                        source: e,
                    })?;
                }
                write_sstv_rgb_png(&path, &snap.pixels, snap.width, snap.height)
            })
            .await;
            let result = match join {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("SSTV PNG export worker panicked: {e:?}");
                    Err(ViewerError::InvalidBuffer(
                        "PNG export worker panicked — see logs".to_string(),
                    ))
                }
            };
            on_complete(result);
        });
    }
}

// ─── Non-modal viewer window ─────────────────────────────────────────────────

/// Open the SSTV viewer in a non-modal transient window. Returns the inner
/// [`SstvImageView`] so the caller can pump snapshots into it.
///
/// Non-modal so the user can keep tuning while the SSTV image builds.
pub fn open_sstv_viewer_window<W: gtk4::prelude::IsA<gtk4::Window>>(
    parent: &W,
    title: &str,
) -> (SstvImageView, adw::Window) {
    let view = SstvImageView::new();

    let window = adw::Window::builder()
        .title(title)
        .default_width(VIEWER_WINDOW_WIDTH)
        .default_height(VIEWER_WINDOW_HEIGHT)
        .transient_for(parent)
        .modal(false)
        .build();

    let header = adw::HeaderBar::new();

    // Pause / Resume toggle.
    let pause_btn = gtk4::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text("Pause / resume live image update")
        .build();
    pause_btn.update_property(&[gtk4::accessible::Property::Label(
        "Pause or resume live SSTV image update",
    )]);
    let pause_view = view.clone();
    pause_btn.connect_toggled(move |btn| {
        pause_view.set_paused(btn.is_active());
    });
    header.pack_start(&pause_btn);

    // Clear button — wipes the current image buffer.
    let clear_btn = gtk4::Button::builder()
        .icon_name("edit-clear-all-symbolic")
        .tooltip_text("Clear the image buffer and start fresh")
        .build();
    clear_btn.update_property(&[gtk4::accessible::Property::Label("Clear SSTV image buffer")]);
    let clear_view = view.clone();
    clear_btn.connect_clicked(move |_| {
        clear_view.clear();
    });
    header.pack_start(&clear_btn);

    // Export PNG button.
    let export_btn = gtk4::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Export the current SSTV image to PNG")
        .build();
    export_btn.update_property(&[gtk4::accessible::Property::Label(
        "Export SSTV image to PNG",
    )]);
    let export_view = view.clone();
    let window_for_export = window.downgrade();
    let export_btn_weak = export_btn.downgrade();
    export_btn.connect_clicked(move |_| {
        let Some(window_for_export) = window_for_export.upgrade() else {
            return;
        };
        let Some(btn) = export_btn_weak.upgrade() else {
            return;
        };
        if !btn.is_sensitive() {
            return;
        }
        btn.set_sensitive(false);
        let btn_for_complete = btn.downgrade();
        let path = default_export_path();
        let path_for_msg = path.clone();
        let window_weak = window_for_export.downgrade();
        export_view.export_png_async(path, move |result| {
            let toast = match result {
                Ok(()) => adw::Toast::builder()
                    .title(format!("Saved {}", path_for_msg.display()))
                    .build(),
                Err(e) => adw::Toast::builder()
                    .title(format!("PNG export failed: {e}"))
                    .build(),
            };
            if let Some(window) = window_weak.upgrade() {
                show_toast_in(&window, toast);
            }
            if let Some(btn) = btn_for_complete.upgrade() {
                btn.set_sensitive(true);
            }
        });
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

/// Default export path: `~/sdr-recordings/sstv-YYYY-MM-DD-HHMMSS.png`.
fn default_export_path() -> PathBuf {
    let timestamp = glib::DateTime::now_local()
        .and_then(|dt| dt.format("%Y-%m-%d-%H%M%S"))
        .map_or_else(|_| "unknown".to_string(), |s| s.to_string());
    glib::home_dir()
        .join("sdr-recordings")
        .join(format!("sstv-{timestamp}.png"))
}

/// Show `toast` inside the window's [`adw::ToastOverlay`], if present.
fn show_toast_in<W: gtk4::prelude::IsA<gtk4::Window>>(window: &W, toast: adw::Toast) {
    if let Some(child) = window.as_ref().child()
        && let Some(overlay) = child.downcast_ref::<adw::ToastOverlay>()
    {
        overlay.add_toast(toast);
    }
}

// ─── Live viewer action ──────────────────────────────────────────────────────

/// Wire the `app.sstv-open` action onto `app`. Activating it (via the
/// app menu or `Ctrl+Shift+V`) opens a non-modal SSTV viewer window.
/// If a viewer is already open, activating it presents (focuses) the
/// existing window and re-sends the current `SstvImageHandle` to the
/// DSP so the viewer reflects the latest state — mirrors the LRPT
/// viewer's behavior (CR round 1 on PR #599).
pub fn connect_sstv_action(
    app: &adw::Application,
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    let action = gio::SimpleAction::new("sstv-open", None);
    let parent_provider = Rc::clone(parent_provider);
    let state_for_action = Rc::clone(state);
    action.connect_activate(move |_, _| {
        open_sstv_viewer_if_needed(&parent_provider, &state_for_action);
    });
    app.add_action(&action);
    app.set_accels_for_action("app.sstv-open", &["<Ctrl><Shift>v"]);
}

/// Open the SSTV viewer window if it isn't already open, registering
/// the new view in `state.sstv_viewer` and sending `SetSstvImage` to
/// the DSP so the decoder tap starts pushing lines into the handle.
/// No-op if a viewer is already open.
pub fn open_sstv_viewer_if_needed(
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    if state.sstv_viewer.borrow().is_some() {
        // Re-send the image handle so the tap stays wired even if
        // a future code path ever clears it (idempotent). Then
        // raise the existing window so `Ctrl+Shift+V` actually
        // surfaces a buried / minimised viewer rather than being a
        // silent no-op. Weak-ref upgrade fails closed: if the
        // window was garbage-collected but the AppState slot wasn't
        // cleared yet, we just skip. Mirrors the LRPT viewer's
        // present-on-repeat-open pattern (CodeRabbit round 13 on
        // PR #543). Per CodeRabbit #8 on PR #599.
        state.send_dsp(UiToDsp::SetSstvImage(state.sstv_image.handle()));
        if let Some(window) = state
            .sstv_viewer_window
            .borrow()
            .as_ref()
            .and_then(glib::WeakRef::upgrade)
        {
            window.present();
        }
        return;
    }
    let Some(parent) = parent_provider() else {
        tracing::warn!("sstv-open invoked with no main window available");
        return;
    };
    let (view, window) = open_sstv_viewer_window(&parent, "ISS SSTV");
    // Attach the shared handle to the view so the Clear button
    // and any other `view.clear()` callsite wipes the source-side
    // pixel buffer too — otherwise the next `update_from_handle`
    // replays the old rows. Per CR round 4 on PR #599.
    view.set_handle(state.sstv_image.handle());
    *state.sstv_viewer.borrow_mut() = Some(view);
    *state.sstv_viewer_window.borrow_mut() = Some(window.downgrade());

    // Hand the shared handle to the DSP so the decoder tap can push
    // lines into it. The handle is a clone of the long-lived singleton
    // in `AppState::sstv_image`.
    state.send_dsp(UiToDsp::SetSstvImage(state.sstv_image.handle()));

    let state_for_close = Rc::clone(state);
    window.connect_close_request(move |_| {
        *state_for_close.sstv_viewer.borrow_mut() = None;
        *state_for_close.sstv_viewer_window.borrow_mut() = None;
        // When the user closes the viewer, don't send ClearSstvImage —
        // the decoder keeps running and the shared handle keeps
        // accumulating data so the recorder's LOS save still has
        // pixels. This mirrors the LRPT viewer's close-without-clear
        // semantics (per CodeRabbit rounds 7 + 8 on PR #543).
        glib::Propagation::Proceed
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Build an `SstvSnapshot` with `lines_written` rows of solid grey
    /// (so writes are deterministic). PD120 / PD180 width 640.
    fn snap(width: u32, height: u32, lines_written: u32) -> SstvSnapshot {
        let n = (width as usize) * (height as usize);
        SstvSnapshot {
            width,
            height,
            pixels: vec![[0x80, 0x80, 0x80]; n],
            lines_written,
        }
    }

    #[test]
    fn renderer_initial_state_is_empty() {
        let r = SstvImageRenderer::new();
        assert!(r.surface.is_none());
        assert_eq!(r.width, 0);
        assert_eq!(r.height, 0);
        assert_eq!(r.lines_written, 0);
        assert!(r.last_snapshot.is_none());
    }

    #[test]
    fn renderer_first_snapshot_allocates_surface() {
        let mut r = SstvImageRenderer::new();
        let changed = r.update_from_snapshot(snap(640, 496, 1));
        assert!(
            changed,
            "first snapshot with one line should report changed"
        );
        assert!(
            r.surface.is_some(),
            "surface should allocate on first update"
        );
        assert_eq!(r.width, 640);
        assert_eq!(r.height, 496);
        assert_eq!(r.lines_written, 1);
        assert!(r.last_snapshot.is_some());
    }

    #[test]
    fn renderer_incremental_advances_only_by_delta() {
        // Two updates, each advancing the line counter, should be
        // additive without re-painting already-written rows.
        let mut r = SstvImageRenderer::new();
        let _ = r.update_from_snapshot(snap(640, 496, 100));
        assert_eq!(r.lines_written, 100);

        let changed = r.update_from_snapshot(snap(640, 496, 250));
        assert!(changed, "advancing should report changed");
        assert_eq!(r.lines_written, 250);
    }

    #[test]
    fn renderer_no_advance_reports_unchanged() {
        let mut r = SstvImageRenderer::new();
        let _ = r.update_from_snapshot(snap(640, 496, 100));

        let changed = r.update_from_snapshot(snap(640, 496, 100));
        assert!(!changed, "same lines_written should report unchanged");
        assert_eq!(r.lines_written, 100);
    }

    #[test]
    fn renderer_same_dimension_rollover_clears_for_new_image() {
        // PD120 → PD120 (same dims) starts a fresh image — slowrx
        // resets the line counter on the new VIS detection. The
        // renderer must detect the rollover and clear stale rows.
        // Per CR round 1 #7 on PR #599; this test pins that fix.
        let mut r = SstvImageRenderer::new();
        let _ = r.update_from_snapshot(snap(640, 496, 496)); // full first image
        assert_eq!(r.lines_written, 496);

        // New image starts: lines_written drops back to 1.
        let changed = r.update_from_snapshot(snap(640, 496, 1));
        assert!(changed, "rollover with one new line should report changed");
        assert_eq!(r.lines_written, 1, "counter reset to new image's progress");
    }

    #[test]
    fn renderer_dimension_change_rebuilds_surface() {
        // PD120 (640×496) → some hypothetical 320×240 mode would
        // resize. Surface gets rebuilt; old contents discarded.
        let mut r = SstvImageRenderer::new();
        let _ = r.update_from_snapshot(snap(640, 496, 100));
        let _ = r.update_from_snapshot(snap(320, 240, 50));
        assert_eq!(r.width, 320);
        assert_eq!(r.height, 240);
        assert_eq!(r.lines_written, 50);
    }

    #[test]
    fn renderer_clear_resets_to_empty() {
        let mut r = SstvImageRenderer::new();
        let _ = r.update_from_snapshot(snap(640, 496, 100));
        r.clear();
        assert_eq!(r.lines_written, 0);
        assert!(r.last_snapshot.is_none());
    }
}
