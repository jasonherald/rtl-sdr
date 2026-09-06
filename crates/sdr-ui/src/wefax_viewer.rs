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
use libadwaita::prelude::*;

use sdr_dsp::wefax::WefaxState;
use sdr_radio::wefax_image::{WefaxImageHandle, WefaxSnapshot};

use crate::messages::UiToDsp;
use crate::viewer::{ViewerError, plain_toast, show_toast_in};

// ─── Constants ─────────────────────────────────────────────────────────────

/// Default viewer window size. WEFAX charts are
/// [`sdr_dsp::wefax::PIXELS_PER_LINE`] px wide (1809) and grow
/// arbitrarily tall as lines arrive; the drawing area scales the
/// chart to fit whatever size the window ends up.
const VIEWER_WINDOW_WIDTH: i32 = 900;
const VIEWER_WINDOW_HEIGHT: i32 = 700;

/// Background painted before any pixel data arrives, or around the
/// chart when the window's aspect ratio doesn't match the image's.
const BACKGROUND_RGB: [f64; 3] = [0.05, 0.05, 0.06];

/// Subtitle shown before the first `DspToUi::WefaxState` arrives (or
/// after [`WefaxImageView::clear`]).
const WEFAX_VIEWER_PLACEHOLDER_SUBTITLE: &str = "Idle";

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
        if snap.height <= self.lines_written && snap.width == self.width {
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
        if let Some(snap) = handle.snapshot() {
            let changed = self.renderer.borrow_mut().update_from_snapshot(snap);
            if changed && !self.paused.get() {
                self.drawing_area.queue_draw();
            }
        }
    }

    /// Wipe all buffered data and queue a redraw. Also clears the
    /// shared [`WefaxImageHandle`] (if attached via
    /// [`Self::set_handle`]) so the next [`Self::update_from_handle`]
    /// doesn't replay the rows we just cleared. Resets the
    /// window-title subtitle to "Idle".
    pub fn clear(&self) {
        if let Some(handle) = self.handle.borrow().as_ref() {
            handle.clear();
        }
        self.renderer.borrow_mut().clear();
        self.drawing_area.queue_draw();
        self.set_state_label(WefaxState::Idle);
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

// ─── Non-modal viewer window ─────────────────────────────────────────────────

/// Build the header bar's Pause/Resume toggle. Split out of
/// [`open_wefax_viewer_window`] per the 50-NLOC gate (#832).
fn build_pause_button(view: &WefaxImageView) -> gtk4::ToggleButton {
    let pause_btn = gtk4::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text("Pause / resume live chart update")
        .build();
    pause_btn.update_property(&[gtk4::accessible::Property::Label(
        "Pause or resume live WEFAX chart update",
    )]);
    let pause_view = view.clone();
    pause_btn.connect_toggled(move |btn| {
        pause_view.set_paused(btn.is_active());
    });
    pause_btn
}

/// Build the header bar's Clear button. Split out of
/// [`open_wefax_viewer_window`] per the 50-NLOC gate (#832).
fn build_clear_button(view: &WefaxImageView) -> gtk4::Button {
    let clear_btn = gtk4::Button::builder()
        .icon_name("edit-clear-all-symbolic")
        .tooltip_text("Clear the chart buffer and start fresh")
        .build();
    clear_btn.update_property(&[gtk4::accessible::Property::Label(
        "Clear WEFAX chart buffer",
    )]);
    let clear_view = view.clone();
    clear_btn.connect_clicked(move |_| {
        clear_view.clear();
    });
    clear_btn
}

/// Build the header bar's Export PNG button. Split out of
/// [`open_wefax_viewer_window`] per the 50-NLOC gate (#832).
fn build_export_button(view: &WefaxImageView, window: &adw::Window) -> gtk4::Button {
    let export_btn = gtk4::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Export the current WEFAX chart to PNG")
        .build();
    export_btn.update_property(&[gtk4::accessible::Property::Label(
        "Export WEFAX chart to PNG",
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
                Ok(()) => plain_toast(&format!("Saved {}", path_for_msg.display())),
                Err(e) => plain_toast(&format!("PNG export failed: {e}")),
            };
            if let Some(window) = window_weak.upgrade() {
                show_toast_in(&window, toast);
            }
            if let Some(btn) = btn_for_complete.upgrade() {
                btn.set_sensitive(true);
            }
        });
    });
    export_btn
}

/// Open the WEFAX viewer in a non-modal transient window. Returns the
/// inner [`WefaxImageView`] so the caller can pump snapshots into it.
///
/// Non-modal so the user can keep tuning while the chart builds.
pub fn open_wefax_viewer_window<W: gtk4::prelude::IsA<gtk4::Window>>(
    parent: &W,
    title: &str,
) -> (WefaxImageView, adw::Window) {
    let view = WefaxImageView::new();

    let window = adw::Window::builder()
        .title(title)
        .default_width(VIEWER_WINDOW_WIDTH)
        .default_height(VIEWER_WINDOW_HEIGHT)
        .transient_for(parent)
        .modal(false)
        .build();
    // Inherit the parent's GApplication so Wayland's
    // `xdg_toplevel_set_app_id` carries `com.sdr.rs` and the WM can
    // resolve our icon. See apt_viewer.rs for the full rationale.
    window.set_application(parent.application().as_ref());

    let header = adw::HeaderBar::new();
    let title_widget = adw::WindowTitle::new(title, WEFAX_VIEWER_PLACEHOLDER_SUBTITLE);
    header.set_title_widget(Some(&title_widget));
    view.set_title_widget(title_widget);

    header.pack_start(&build_pause_button(&view));
    header.pack_start(&build_clear_button(&view));
    header.pack_end(&build_export_button(&view, &window));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(view.drawing_area()));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar));

    window.set_content(Some(&toast_overlay));
    window.present();

    (view, window)
}

/// Default export path: `~/sdr-recordings/wefax-YYYY-MM-DD-HHMMSS.png`.
///
/// Mirrors [`crate::sstv_viewer`]'s `default_export_path` — probes
/// for existing files and appends `-1`, `-2`, … so two manual
/// exports inside the same wall-clock second don't collide.
fn default_export_path() -> PathBuf {
    let timestamp = glib::DateTime::now_local()
        .and_then(|dt| dt.format("%Y-%m-%d-%H%M%S"))
        .map_or_else(|_| "unknown".to_string(), |s| s.to_string());
    let dir = glib::home_dir().join("sdr-recordings");
    let stem = format!("wefax-{timestamp}");
    let mut path = dir.join(format!("{stem}.png"));
    let mut suffix = 1_u32;
    while path.exists() {
        path = dir.join(format!("{stem}-{suffix}.png"));
        suffix += 1;
    }
    path
}

// ─── Live viewer action ──────────────────────────────────────────────────────

/// Wire the `app.wefax-open` action onto `app`. Activating it (via
/// the app menu or `Ctrl+Shift+F`) opens a non-modal WEFAX viewer
/// window. If a viewer is already open, activating it presents
/// (focuses) the existing window and re-sends the current
/// `WefaxImageHandle` to the DSP so the viewer reflects the latest
/// state — mirrors [`crate::sstv_viewer::connect_sstv_action`].
pub fn connect_wefax_action(
    app: &adw::Application,
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    let action = gio::SimpleAction::new("wefax-open", None);
    let parent_provider = Rc::clone(parent_provider);
    let state_for_action = Rc::clone(state);
    action.connect_activate(move |_, _| {
        open_wefax_viewer_if_needed(&parent_provider, &state_for_action);
    });
    app.add_action(&action);
    app.set_accels_for_action("app.wefax-open", &["<Ctrl><Shift>f"]);
}

/// Open the WEFAX viewer window if it isn't already open, registering
/// the new view in `state.wefax_viewer` and sending `SetWefaxImage` to
/// the DSP so the decoder tap starts pushing lines into the handle.
/// No-op if a viewer is already open (re-presents it instead).
pub fn open_wefax_viewer_if_needed(
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    if state.wefax_viewer.borrow().is_some() {
        // Re-send the image handle so the tap stays wired even if a
        // future code path ever clears it (idempotent), then raise
        // the existing window. Mirrors
        // `sstv_viewer::open_sstv_viewer_if_needed`.
        state.send_dsp(UiToDsp::SetWefaxImage(state.wefax_image.handle()));
        if let Some(window) = state
            .wefax_viewer_window
            .borrow()
            .as_ref()
            .and_then(glib::WeakRef::upgrade)
        {
            window.present();
        }
        return;
    }
    let Some(parent) = parent_provider() else {
        tracing::warn!("wefax-open invoked with no main window available");
        return;
    };
    let (view, window) = open_wefax_viewer_window(&parent, "WEFAX Chart");
    // Attach the shared handle to the view so the Clear button wipes
    // the source-side pixel buffer too — otherwise the next
    // `update_from_handle` replays the old rows.
    view.set_handle(state.wefax_image.handle());
    *state.wefax_viewer.borrow_mut() = Some(view);
    *state.wefax_viewer_window.borrow_mut() = Some(window.downgrade());

    // Hand the shared handle to the DSP so the decoder tap can push
    // lines into it. The handle is a clone of the long-lived
    // singleton in `AppState::wefax_image`.
    state.send_dsp(UiToDsp::SetWefaxImage(state.wefax_image.handle()));

    let state_for_close = Rc::clone(state);
    window.connect_close_request(move |_| {
        *state_for_close.wefax_viewer.borrow_mut() = None;
        *state_for_close.wefax_viewer_window.borrow_mut() = None;
        // Closing the viewer does NOT send `ClearWefaxImage` — the
        // decoder keeps running and the shared handle keeps
        // accumulating data, mirroring the SSTV/LRPT
        // close-without-clear semantics so a future auto-save flow
        // (Task 13) still sees completed charts.
        glib::Propagation::Proceed
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
