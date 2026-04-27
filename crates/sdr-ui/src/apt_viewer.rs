//! Live NOAA APT image viewer + PNG export.
//!
//! Displays an [`sdr_radio::apt_image::AptImage`] as it builds up
//! during a satellite pass. Width is fixed at the APT scan width
//! (2080 px); height grows downward as new lines arrive at 2 Hz.
//!
//! Three pieces:
//!
//! * [`AptImageRenderer`] — pure Cairo renderer. Owns the ARGB32
//!   pixel buffer and knows how to paint it into a cairo context
//!   with auto-fit + aspect preservation. No GTK dependency, fully
//!   unit-testable. Mirrors the waterfall renderer pattern but
//!   without the logical ring (APT passes are bounded).
//! * [`AptImageView`] — GTK widget wrapping a renderer. Push lines
//!   in via `push_line`; the view queues a redraw on each push
//!   unless paused. Cloneable (all state is `Rc`-shared) so closures
//!   on toolbar buttons can hold their own handle.
//! * [`open_apt_viewer_window`] — opens the view in a non-modal
//!   transient window so the main radio window stays interactive
//!   during a pass. Header bar carries Pause / Resume + Export PNG.
//!
//! [`connect_apt_action`] wires the `app.apt-open` action
//! (`Ctrl+Shift+A`). Activating it opens a viewer window and
//! registers it with [`crate::state::AppState::apt_viewer`] so the
//! `DspToUi::AptLine` handler in `window.rs` can route real,
//! live-decoded APT lines into it. Closing the window clears the
//! `AppState` slot — subsequent decoder lines are then dropped
//! silently until the user reopens.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{cairo, gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use sdr_dsp::apt::LINE_PIXELS;

use crate::viewer::ViewerError;

/// Maximum lines we'll keep in the renderer. NOAA APT bounds a pass
/// at ~1800 lines (15 min × 2 lines/sec); 2048 leaves headroom for
/// the longest plausible high-elevation pass without ever growing
/// the underlying surface at runtime.
pub const MAX_LINES: usize = 2_048;

/// Background colour painted before any APT data is pushed (or
/// behind the image when the widget is wider than the image's
/// aspect ratio). Near-black so the eventual greyscale image
/// stands out.
const BACKGROUND_RGB: [f64; 3] = [0.05, 0.05, 0.06];

// ─── Window + demo tuning ──────────────────────────────────────────────

/// Default size for the viewer's transient window. ~4:3 lets the
/// 2080×~1800-aspect APT image fit a usable amount of pixels at
/// most desktop resolutions without the user having to resize.
const VIEWER_WINDOW_WIDTH: i32 = 800;
const VIEWER_WINDOW_HEIGHT: i32 = 600;

/// Pure Cairo renderer for an APT scan-line buffer.
///
/// Owns a persistent ARGB32 [`cairo::ImageSurface`] sized for a full
/// pass at construction. `push_line` writes new rows directly into
/// the surface's backing data; `render` and `export_png` use the
/// surface as a paint source — no per-draw cloning of the ~17 MB
/// pixel buffer (which was a real concern: at the cap, a window
/// resize during a live pass would memcpy 14+ MB on the GTK main
/// thread per frame).
pub struct AptImageRenderer {
    /// Persistent ARGB32 surface, `LINE_PIXELS × MAX_LINES`. Rows
    /// past `n_lines` stay zeroed (alpha 0); render-time clipping
    /// keeps them out of the visible output.
    surface: cairo::ImageSurface,
    n_lines: usize,
}

impl Default for AptImageRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl AptImageRenderer {
    /// Build an empty renderer with a full-pass-sized backing surface.
    ///
    /// # Panics
    ///
    /// Panics if Cairo can't allocate the (`LINE_PIXELS` × `MAX_LINES`)
    /// ARGB32 surface — about 17 MB of zeroed memory. Realistically
    /// unreachable on any machine that's running a desktop in the
    /// first place; the alternative would be a fallible constructor
    /// that callers would have to plumb errors through for a
    /// for-all-practical-purposes-infallible allocation.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    pub fn new() -> Self {
        let surface = cairo::ImageSurface::create(
            cairo::Format::ARgb32,
            LINE_PIXELS as i32,
            MAX_LINES as i32,
        )
        .expect("APT renderer: ARGB32 surface allocation (~17 MB) failed at startup");
        Self {
            surface,
            n_lines: 0,
        }
    }

    /// Append one APT scan line of width [`LINE_PIXELS`] to the
    /// image. Greyscale values go straight into the surface's
    /// backing data as ARGB32 (B/G/R/A — Cairo's
    /// little-endian ARGB32 layout, alpha = `0xFF`). No allocation,
    /// no buffer growth — we wrote the row directly into the
    /// pre-allocated surface. No-op once [`MAX_LINES`] is reached.
    pub fn push_line(&mut self, pixels: &[u8; LINE_PIXELS]) {
        if self.n_lines >= MAX_LINES {
            return;
        }
        let stride = match usize::try_from(self.surface.stride()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("APT renderer: invalid surface stride: {e}");
                return;
            }
        };
        let row_offset = self.n_lines * stride;
        let mut data = match self.surface.data() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("APT renderer: surface data lock failed: {e}");
                return;
            }
        };
        for (i, &g) in pixels.iter().enumerate() {
            let pixel_offset = row_offset + i * 4;
            data[pixel_offset] = g;
            data[pixel_offset + 1] = g;
            data[pixel_offset + 2] = g;
            data[pixel_offset + 3] = 0xFF;
        }
        // `data` guard drops here, flushing the surface for cairo.
        drop(data);
        self.n_lines += 1;
    }

    /// Reset to an empty image. Zeroes the surface bytes (fully
    /// transparent everywhere) so subsequent passes start from a
    /// clean canvas; the surface itself is preserved.
    pub fn clear(&mut self) {
        if let Ok(mut data) = self.surface.data() {
            data.fill(0);
        }
        self.n_lines = 0;
    }

    /// Number of scan lines currently buffered.
    #[must_use]
    pub fn n_lines(&self) -> usize {
        self.n_lines
    }

    /// `true` when no lines have been pushed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.n_lines == 0
    }

    /// Paint the buffered image into `cr`, scaled to fit
    /// `(width, height)` while preserving the
    /// `LINE_PIXELS : n_lines` aspect. Centred horizontally,
    /// top-aligned vertically — the live pass naturally builds
    /// downward.
    ///
    /// Uses the persistent backing surface as the paint source plus
    /// a clipping rectangle, so this method is `&self` and runs
    /// without copying the pixel buffer.
    ///
    /// # Errors
    ///
    /// Returns [`ViewerError::Cairo`] if any of the Cairo
    /// operations (`paint`, `save`, `restore`,
    /// `set_source_surface`, `fill`) fail. Callers usually log
    /// and continue — drawing failures shouldn't kill the UI.
    /// Per issue #545 (was `Result<(), String>` before).
    #[allow(clippy::cast_precision_loss)]
    pub fn render(&self, cr: &cairo::Context, width: i32, height: i32) -> Result<(), ViewerError> {
        cr.set_source_rgb(BACKGROUND_RGB[0], BACKGROUND_RGB[1], BACKGROUND_RGB[2]);
        cr.paint().map_err(|e| ViewerError::Cairo {
            op: "background paint",
            source: e,
        })?;

        if self.n_lines == 0 || width <= 0 || height <= 0 {
            return Ok(());
        }

        let img_w = LINE_PIXELS as f64;
        let img_h = self.n_lines as f64;
        let scale = (f64::from(width) / img_w).min(f64::from(height) / img_h);
        let off_x = (f64::from(width) - img_w * scale) / 2.0;

        cr.save().map_err(|e| ViewerError::Cairo {
            op: "save",
            source: e,
        })?;
        cr.translate(off_x, 0.0);
        cr.scale(scale, scale);
        cr.set_source_surface(&self.surface, 0.0, 0.0)
            .map_err(|e| ViewerError::Cairo {
                op: "set_source_surface",
                source: e,
            })?;
        // Rectangle + fill clips the paint to the populated rows;
        // rows past n_lines stay alpha-0 in the surface and would
        // paint as nothing under `paint()` anyway, but clipping
        // here also bounds the layout area cleanly.
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

    /// Save the current image to a PNG file.
    ///
    /// Builds a one-shot tightly-sized export surface
    /// (`LINE_PIXELS × n_lines`) by painting from the persistent
    /// backing surface. Tightly-sized output keeps the PNG file
    /// from carrying tons of empty alpha-0 padding rows past the
    /// real data.
    ///
    /// # Errors
    ///
    /// Returns [`ViewerError::EmptyChannel`] (with `apid: None`
    /// — APT has no APIDs) when there's nothing to export, or a
    /// `Cairo` / `Io` / `PngEncode` variant on the failing step.
    /// Per issue #545 (was `Result<(), String>` before).
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    pub fn export_png(&self, path: &Path) -> Result<(), ViewerError> {
        if self.n_lines == 0 {
            return Err(ViewerError::EmptyChannel { apid: None });
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ViewerError::Io {
                op: "create_dir_all",
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        let export_surface = cairo::ImageSurface::create(
            cairo::Format::ARgb32,
            LINE_PIXELS as i32,
            self.n_lines as i32,
        )
        .map_err(|e| ViewerError::Cairo {
            op: "export surface",
            source: e,
        })?;
        let cr = cairo::Context::new(&export_surface).map_err(|e| ViewerError::Cairo {
            op: "export context",
            source: e,
        })?;
        cr.set_source_surface(&self.surface, 0.0, 0.0)
            .map_err(|e| ViewerError::Cairo {
                op: "export set_source_surface",
                source: e,
            })?;
        // LINE_PIXELS / n_lines are both bounded by MAX_LINES — well
        // under f64's 52-bit mantissa, no real precision loss.
        #[allow(clippy::cast_precision_loss)]
        cr.rectangle(0.0, 0.0, LINE_PIXELS as f64, self.n_lines as f64);
        cr.fill().map_err(|e| ViewerError::Cairo {
            op: "export fill",
            source: e,
        })?;
        drop(cr);

        let mut file = std::fs::File::create(path).map_err(|e| ViewerError::Io {
            op: "file create",
            path: path.to_path_buf(),
            source: e,
        })?;
        export_surface.write_to_png(&mut file)?;
        tracing::info!(?path, lines = self.n_lines, "APT image exported to PNG");
        Ok(())
    }
}

// ─── GTK widget ─────────────────────────────────────────────────────────

/// Live APT image viewer widget.
///
/// Holds a `DrawingArea` plus shared rendering state. Cloneable —
/// every clone holds an `Rc` to the same renderer + pause flag, so
/// toolbar callbacks can hold their own handle without lifetime
/// dance. Push new lines in via [`AptImageView::push_line`]; the
/// widget queues a redraw automatically (unless paused).
/// Handle for an open APT viewer. `Clone` is derived (existing
/// pattern) so the wiring layer can stash a copy in
/// [`crate::state::AppState`] for the `DspToUi::AptLine` handler
/// to push lines into — every field is already `Rc`-shared
/// internally, so cloning is a refcount bump.
#[derive(Clone)]
pub struct AptImageView {
    drawing_area: gtk4::DrawingArea,
    renderer: Rc<RefCell<AptImageRenderer>>,
    paused: Rc<Cell<bool>>,
}

impl Default for AptImageView {
    fn default() -> Self {
        Self::new()
    }
}

impl AptImageView {
    /// Build a fresh view with a blank renderer.
    #[must_use]
    pub fn new() -> Self {
        let renderer = Rc::new(RefCell::new(AptImageRenderer::new()));
        let paused = Rc::new(Cell::new(false));

        let drawing_area = gtk4::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        let renderer_for_draw = Rc::clone(&renderer);
        drawing_area.set_draw_func(move |_area, cr, w, h| {
            if let Err(e) = renderer_for_draw.borrow().render(cr, w, h) {
                tracing::warn!("APT render failed: {e}");
            }
        });

        Self {
            drawing_area,
            renderer,
            paused,
        }
    }

    /// The underlying `GtkDrawingArea`. Pack this into a layout
    /// container, wrap in a `ScrolledWindow`, etc.
    #[must_use]
    pub fn drawing_area(&self) -> &gtk4::DrawingArea {
        &self.drawing_area
    }

    /// Append one scan line. The line is *always* buffered into the
    /// renderer regardless of pause state — pausing only suppresses
    /// the `queue_draw()` call. This keeps the user from losing
    /// scanlines while inspecting the image (a paused live pass on
    /// the real radio path would otherwise produce a PNG with gaps
    /// for whatever rows arrived during the inspection window).
    pub fn push_line(&self, pixels: &[u8; LINE_PIXELS]) {
        self.renderer.borrow_mut().push_line(pixels);
        if !self.paused.get() {
            self.drawing_area.queue_draw();
        }
    }

    /// Wipe all buffered scan lines and queue a redraw.
    pub fn clear(&self) {
        self.renderer.borrow_mut().clear();
        self.drawing_area.queue_draw();
    }

    /// Toggle pause / resume. Pausing freezes the visible canvas;
    /// scanlines pushed while paused still accumulate in the
    /// renderer (so nothing is lost) and become visible on resume
    /// via a forced single redraw.
    pub fn set_paused(&self, paused: bool) {
        let was_paused = self.paused.replace(paused);
        if was_paused && !paused {
            // Resuming — force one redraw so any rows pushed during
            // the pause window become visible immediately.
            self.drawing_area.queue_draw();
        }
    }

    /// `true` if the view is currently paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    /// Save the current image to a PNG. Same error semantics as
    /// [`AptImageRenderer::export_png`].
    ///
    /// # Errors
    ///
    /// Propagates any [`ViewerError`] from the underlying
    /// renderer (per issue #545 — was `Result<(), String>`
    /// before).
    pub fn export_png(&self, path: &Path) -> Result<(), ViewerError> {
        self.renderer.borrow().export_png(path)
    }

    /// Number of lines currently in the buffer.
    #[must_use]
    pub fn n_lines(&self) -> usize {
        self.renderer.borrow().n_lines()
    }
}

// ─── Non-modal viewer window ───────────────────────────────────────────

/// Open the APT viewer in a non-modal transient window. The window
/// holds a header-bar with Pause / Resume + Export PNG, plus the
/// drawing-area canvas underneath. Returns the inner [`AptImageView`]
/// so the caller can pump lines into it; dropping the returned view
/// won't close the window (the window owns its own clones).
///
/// Non-modal so the user can keep tuning, recording, or otherwise
/// interacting with the main radio window while the APT image
/// builds up alongside.
pub fn open_apt_viewer_window<W: gtk4::prelude::IsA<gtk4::Window>>(
    parent: &W,
    title: &str,
) -> (AptImageView, adw::Window) {
    let view = AptImageView::new();

    let window = adw::Window::builder()
        .title(title)
        .default_width(VIEWER_WINDOW_WIDTH)
        .default_height(VIEWER_WINDOW_HEIGHT)
        .transient_for(parent)
        .modal(false)
        .build();

    let header = adw::HeaderBar::new();

    let pause_btn = gtk4::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text("Pause / resume the live image update")
        .build();
    // Tooltips are hover-only — set the accessible label too so screen
    // readers actually announce the icon-only buttons. Project rule
    // for any header / popover icon-only control.
    pause_btn.update_property(&[gtk4::accessible::Property::Label(
        "Pause or resume live image update",
    )]);
    let pause_view = view.clone();
    pause_btn.connect_toggled(move |btn| {
        pause_view.set_paused(btn.is_active());
    });
    header.pack_start(&pause_btn);

    let export_btn = gtk4::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Export the current APT image to PNG")
        .build();
    export_btn.update_property(&[gtk4::accessible::Property::Label("Export APT image to PNG")]);
    let export_view = view.clone();
    // Weak window ref so the closure (which lives on the export
    // button, which lives in the window) doesn't form a strong
    // retention cycle keeping the window + ~17 MB image surface
    // alive after the user closes the viewer. Upgrade-or-skip
    // means a stray click on a button mid-teardown is a no-op.
    let window_for_export = window.downgrade();
    export_btn.connect_clicked(move |_| {
        let Some(window_for_export) = window_for_export.upgrade() else {
            return;
        };
        let path = default_export_path();
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
/// `~/sdr-recordings/apt-YYYY-MM-DD-HHMMSS.png`.
fn default_export_path() -> PathBuf {
    let timestamp = glib::DateTime::now_local()
        .and_then(|dt| dt.format("%Y-%m-%d-%H%M%S"))
        .map_or_else(|_| "unknown".to_string(), |s| s.to_string());
    glib::home_dir()
        .join("sdr-recordings")
        .join(format!("apt-{timestamp}.png"))
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

// ─── Live viewer action ─────────────────────────────────────────────────

/// Wire the `app.apt-open` action onto `app`. Activating it (via the
/// app menu, the `Ctrl+Shift+A` accelerator, or future activity-bar
/// entry) opens a non-modal APT viewer window. The window is fed
/// real `AptLine`s from the DSP-thread decoder via
/// `state.apt_viewer` — the [`crate::messages::DspToUi::AptLine`]
/// handler in `window.rs` looks up the active view there and pushes
/// pixel rows into it.
///
/// If the viewer is already open, activating the action again is a
/// no-op (we don't try to refocus or re-create — the existing window
/// is already on screen and accepting lines). Closing the window
/// clears `state.apt_viewer` so the decoder's lines are dropped
/// silently until the user reopens.
pub fn connect_apt_action(
    app: &adw::Application,
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    let action = gio::SimpleAction::new("apt-open", None);
    let parent_provider = Rc::clone(parent_provider);
    let state_for_action = Rc::clone(state);
    action.connect_activate(move |_, _| {
        open_apt_viewer_if_needed(&parent_provider, &state_for_action);
    });
    app.add_action(&action);
    app.set_accels_for_action("app.apt-open", &["<Ctrl><Shift>a"]);
}

/// Open the APT viewer window if it isn't already open, registering
/// the new view in `state.apt_viewer` and wiring `close-request` to
/// clear that slot. No-op if a viewer is already open.
///
/// Pulled out of `connect_apt_action` so the auto-record-on-pass
/// path (#482b) can fire the same open flow at AOS without going
/// through the GIO action system.
pub fn open_apt_viewer_if_needed(
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    if state.apt_viewer.borrow().is_some() {
        // Already open — nothing to do. The user can find the
        // existing window via the OS window switcher.
        return;
    }
    let Some(parent) = parent_provider() else {
        tracing::warn!("apt-open invoked with no main window available");
        return;
    };
    let (view, window) = open_apt_viewer_window(&parent, "NOAA APT");
    // Stash a clone in AppState so the DSP→UI handler can find
    // it. The clone is cheap (every field is `Rc`-shared).
    *state.apt_viewer.borrow_mut() = Some(view);
    // Stash a weak ref to the window so the auto-record LOS path
    // can `.close()` the viewer after the PNG export finishes,
    // resetting it for the next pass. Weak ref so the AppState
    // slot doesn't keep the window alive past its natural
    // lifetime (the GTK toplevel registry owns the strong ref).
    // Same pattern as `lrpt_viewer_window`.
    *state.apt_viewer_window.borrow_mut() = Some(window.downgrade());

    // Clear the AppState slots when the user closes the window
    // (or when the auto-record path closes it programmatically)
    // — otherwise `DspToUi::AptLine` would keep pushing into a
    // detached widget tree until the next reopen, and the viewer
    // state would never reset for a second pass.
    let state_for_close = Rc::clone(state);
    window.connect_close_request(move |_| {
        *state_for_close.apt_viewer.borrow_mut() = None;
        *state_for_close.apt_viewer_window.borrow_mut() = None;
        glib::Propagation::Proceed
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Number of lines pushed by the renderer tests. Keeps tests fast
    /// while still exercising more than one line of buffer growth.
    const TEST_LINE_COUNT: usize = 16;

    fn synth_line(seed: u8) -> [u8; LINE_PIXELS] {
        let mut line = [0_u8; LINE_PIXELS];
        for (i, p) in line.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            {
                *p = ((i + usize::from(seed)) & 0xff) as u8;
            }
        }
        line
    }

    #[test]
    fn renderer_starts_empty() {
        let r = AptImageRenderer::new();
        assert!(r.is_empty());
        assert_eq!(r.n_lines(), 0);
    }

    #[test]
    fn push_line_increments_n_lines() {
        let mut r = AptImageRenderer::new();
        r.push_line(&synth_line(0));
        assert_eq!(r.n_lines(), 1);
        r.push_line(&synth_line(1));
        assert_eq!(r.n_lines(), 2);
    }

    #[test]
    fn surface_dimensions_match_max_lines_at_construction() {
        // The surface is allocated full-size up front so push_line
        // never has to grow it. Lock that invariant down so a future
        // refactor can't accidentally make it lazy and lose the
        // alloc-free hot-path guarantee.
        let r = AptImageRenderer::new();
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        {
            assert_eq!(r.surface.width(), LINE_PIXELS as i32);
            assert_eq!(r.surface.height(), MAX_LINES as i32);
        }
    }

    #[test]
    fn push_line_caps_at_max_lines() {
        let mut r = AptImageRenderer::new();
        for i in 0..MAX_LINES {
            #[allow(clippy::cast_possible_truncation)]
            r.push_line(&synth_line(i as u8));
        }
        assert_eq!(r.n_lines(), MAX_LINES);
        // One more push past the cap is a no-op.
        r.push_line(&synth_line(0));
        assert_eq!(r.n_lines(), MAX_LINES);
    }

    #[test]
    fn clear_resets_lines_and_zeroes_surface_pixels() {
        let mut r = AptImageRenderer::new();
        for _ in 0..TEST_LINE_COUNT {
            r.push_line(&synth_line(0xAA));
        }
        r.clear();
        assert!(r.is_empty());
        assert_eq!(r.n_lines(), 0);
        // The first row of the surface should now be all zeroes
        // (alpha-0 transparent), matching what `cairo::ImageSurface::create`
        // gives a fresh surface.
        let data = r.surface.data().unwrap();
        assert!(
            data[0..LINE_PIXELS * 4].iter().all(|&b| b == 0),
            "clear() should zero the surface bytes",
        );
    }

    #[test]
    fn pixel_layout_is_argb32_with_grayscale_in_bgr_channels() {
        let mut r = AptImageRenderer::new();
        let mut line = [0_u8; LINE_PIXELS];
        line[0] = 0x80;
        line[1] = 0xC0;
        r.push_line(&line);
        // Cairo ARGB32 little-endian: B, G, R, A
        let data = r.surface.data().unwrap();
        assert_eq!(&data[0..4], &[0x80, 0x80, 0x80, 0xFF]);
        assert_eq!(&data[4..8], &[0xC0, 0xC0, 0xC0, 0xFF]);
    }

    #[test]
    fn export_png_round_trips_to_a_real_file() {
        let mut r = AptImageRenderer::new();
        for i in 0..TEST_LINE_COUNT {
            #[allow(clippy::cast_possible_truncation)]
            r.push_line(&synth_line(i as u8));
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("sdr-ui-apt-test-{nanos}.png"));
        r.export_png(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.len() > 0, "PNG file shouldn't be empty");
        // PNG magic bytes — first 8 bytes of any valid PNG.
        let mut header = [0_u8; 8];
        let mut f = std::fs::File::open(&path).unwrap();
        f.read_exact(&mut header).unwrap();
        assert_eq!(
            &header, b"\x89PNG\r\n\x1a\n",
            "exported file isn't a valid PNG (header mismatch)",
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_png_refuses_when_buffer_is_empty() {
        let r = AptImageRenderer::new();
        let path = std::env::temp_dir().join("apt-test-empty-should-not-be-written.png");
        let result = r.export_png(&path);
        assert!(result.is_err());
        assert!(!path.exists(), "no file should be created on empty export");
    }
}
