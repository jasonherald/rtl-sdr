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
//! `connect_demo_action` wires a temporary `app.apt-demo` action
//! that pumps a synthetic gradient pass through a freshly-opened
//! window — useful for visual smoke-testing tonight, replaced by
//! the real radio-side wiring in #482 (auto-record on overhead pass).

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{cairo, gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use sdr_dsp::apt::LINE_PIXELS;

/// Maximum lines we'll keep in the renderer. NOAA APT bounds a pass
/// at ~1800 lines (15 min × 2 lines/sec); 2048 leaves headroom for
/// the longest plausible high-elevation pass without ever growing
/// the underlying Vec at runtime.
pub const MAX_LINES: usize = 2_048;

/// Size of the pre-allocated pixel buffer in bytes. ARGB32 = 4 bytes
/// per pixel × 2080 px wide × 2048 lines max ≈ 17 MB. Pre-reserved
/// at construction so `push_line` never allocates during a live pass.
const PIXEL_BUF_BYTES: usize = LINE_PIXELS * 4 * MAX_LINES;

/// Background colour painted before any APT data is pushed (or
/// behind the image when the widget is wider than the image's
/// aspect ratio). Near-black so the eventual greyscale image
/// stands out.
const BACKGROUND_RGB: [f64; 3] = [0.05, 0.05, 0.06];

/// Pure Cairo renderer for an APT scan-line buffer.
///
/// Owns an ARGB32 pixel buffer, knows how to push grayscale lines
/// into it (expanded to ARGB32 with full alpha), render to a Cairo
/// context with auto-fit, and export to PNG.
pub struct AptImageRenderer {
    pixel_buf: Vec<u8>,
    n_lines: usize,
}

impl Default for AptImageRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl AptImageRenderer {
    /// Build an empty renderer. Pre-allocates the full pixel buffer
    /// so `push_line` is alloc-free in steady state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pixel_buf: Vec::with_capacity(PIXEL_BUF_BYTES),
            n_lines: 0,
        }
    }

    /// Append one APT scan line of width [`LINE_PIXELS`] to the image.
    /// Pixels are stored as ARGB32 (each greyscale value goes into
    /// B, G, R; alpha is `0xFF`). No-op once [`MAX_LINES`] is reached
    /// — a real pass never gets close to the cap, but the bound
    /// keeps memory deterministic.
    pub fn push_line(&mut self, pixels: &[u8; LINE_PIXELS]) {
        if self.n_lines >= MAX_LINES {
            return;
        }
        for &g in pixels {
            // Cairo ARGB32 on little-endian is laid out as B, G, R, A.
            self.pixel_buf.push(g);
            self.pixel_buf.push(g);
            self.pixel_buf.push(g);
            self.pixel_buf.push(0xFF);
        }
        self.n_lines += 1;
    }

    /// Reset to an empty buffer. Capacity is retained — the next
    /// pass reuses the same allocation.
    pub fn clear(&mut self) {
        self.pixel_buf.clear();
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

    /// Paint the buffered image into `cr`, scaled to fit `(width,
    /// height)` while preserving the `LINE_PIXELS : n_lines` aspect.
    /// The image is centered horizontally and top-aligned vertically
    /// — the live pass naturally builds downward, and a top-aligned
    /// view lets the user see the latest line at the bottom of the
    /// painted area.
    ///
    /// # Errors
    ///
    /// Returns a stringified Cairo error if surface construction or
    /// painting fails. Callers usually want to log the error and
    /// continue — drawing failures shouldn't kill the UI.
    #[allow(clippy::cast_precision_loss)]
    pub fn render(&self, cr: &cairo::Context, width: i32, height: i32) -> Result<(), String> {
        cr.set_source_rgb(BACKGROUND_RGB[0], BACKGROUND_RGB[1], BACKGROUND_RGB[2]);
        cr.paint().map_err(|e| format!("background paint: {e}"))?;

        if self.n_lines == 0 || width <= 0 || height <= 0 {
            return Ok(());
        }

        let surface = self.to_cairo_surface()?;
        let img_w = LINE_PIXELS as f64;
        let img_h = self.n_lines as f64;
        let scale = (f64::from(width) / img_w).min(f64::from(height) / img_h);
        let draw_w = img_w * scale;
        let off_x = (f64::from(width) - draw_w) / 2.0;

        cr.save().map_err(|e| format!("save: {e}"))?;
        cr.translate(off_x, 0.0);
        cr.scale(scale, scale);
        cr.set_source_surface(&surface, 0.0, 0.0)
            .map_err(|e| format!("set_source_surface: {e}"))?;
        cr.paint().map_err(|e| format!("image paint: {e}"))?;
        cr.restore().map_err(|e| format!("restore: {e}"))?;
        Ok(())
    }

    /// Save the current image to a PNG file.
    ///
    /// # Errors
    ///
    /// Returns a stringified error from filesystem creation, surface
    /// construction, or Cairo's PNG encoder.
    pub fn export_png(&self, path: &Path) -> Result<(), String> {
        if self.n_lines == 0 {
            return Err("no APT image data to export".to_string());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        let surface = self.to_cairo_surface()?;
        let mut file = std::fs::File::create(path).map_err(|e| format!("file: {e}"))?;
        surface
            .write_to_png(&mut file)
            .map_err(|e| format!("png: {e}"))?;
        tracing::info!(?path, lines = self.n_lines, "APT image exported to PNG");
        Ok(())
    }

    /// Build a Cairo `ImageSurface` over the current pixel buffer.
    /// Pads to Cairo's expected stride if it differs from our packed
    /// `LINE_PIXELS * 4` layout (it usually doesn't for sane widths,
    /// but the pad path matches the waterfall renderer's careful
    /// handling).
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn to_cairo_surface(&self) -> Result<cairo::ImageSurface, String> {
        let stride = cairo::Format::ARgb32
            .stride_for_width(LINE_PIXELS as u32)
            .map_err(|e| format!("stride: {e}"))?;
        let packed_stride = (LINE_PIXELS * 4) as i32;
        let buf = if stride == packed_stride {
            self.pixel_buf.clone()
        } else {
            let stride_usize = usize::try_from(stride).map_err(|e| format!("stride: {e}"))?;
            let row_bytes = LINE_PIXELS * 4;
            let mut padded = vec![0_u8; stride_usize * self.n_lines];
            for row in 0..self.n_lines {
                let src = row * row_bytes;
                let dst = row * stride_usize;
                padded[dst..dst + row_bytes].copy_from_slice(&self.pixel_buf[src..src + row_bytes]);
            }
            padded
        };
        cairo::ImageSurface::create_for_data(
            buf,
            cairo::Format::ARgb32,
            LINE_PIXELS as i32,
            self.n_lines as i32,
            stride,
        )
        .map_err(|e| format!("surface: {e}"))
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

    /// Append one scan line and queue a redraw.
    ///
    /// Honors the pause toggle: while paused, lines are silently
    /// dropped so the view freezes on whatever was last shown. Real
    /// captures should probably keep accumulating in the underlying
    /// `AptImage` and just stop *visually* updating — that's a
    /// follow-up; for a live pass viewer the simple "freeze the
    /// canvas" semantics are fine.
    pub fn push_line(&self, pixels: &[u8; LINE_PIXELS]) {
        if self.paused.get() {
            return;
        }
        self.renderer.borrow_mut().push_line(pixels);
        self.drawing_area.queue_draw();
    }

    /// Wipe all buffered scan lines and queue a redraw.
    pub fn clear(&self) {
        self.renderer.borrow_mut().clear();
        self.drawing_area.queue_draw();
    }

    /// Toggle pause / resume. Paused views ignore `push_line` calls.
    pub fn set_paused(&self, paused: bool) {
        self.paused.set(paused);
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
    /// Propagates any error from the underlying renderer.
    pub fn export_png(&self, path: &Path) -> Result<(), String> {
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
) -> AptImageView {
    let view = AptImageView::new();

    let window = adw::Window::builder()
        .title(title)
        .default_width(800)
        .default_height(600)
        .transient_for(parent)
        .modal(false)
        .build();

    let header = adw::HeaderBar::new();

    let pause_btn = gtk4::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text("Pause / resume the live image update")
        .build();
    let pause_view = view.clone();
    pause_btn.connect_toggled(move |btn| {
        pause_view.set_paused(btn.is_active());
    });
    header.pack_start(&pause_btn);

    let export_btn = gtk4::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Export the current APT image to PNG")
        .build();
    let export_view = view.clone();
    let window_for_export = window.clone();
    export_btn.connect_clicked(move |_| {
        let path = default_export_path();
        match export_view.export_png(&path) {
            Ok(()) => {
                let toast = adw::Toast::builder()
                    .title(format!("Saved {}", path.display()))
                    .build();
                show_toast_in(&window_for_export, toast);
            }
            Err(e) => {
                let toast = adw::Toast::builder()
                    .title(format!("PNG export failed: {e}"))
                    .build();
                show_toast_in(&window_for_export, toast);
            }
        }
    });
    header.pack_end(&export_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(view.drawing_area()));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar));

    window.set_content(Some(&toast_overlay));
    window.present();

    view
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

// ─── Demo action (smoke-test wiring) ───────────────────────────────────

/// Wire the temporary `app.apt-demo` action onto `app`. Activating
/// it opens an APT viewer window and pumps a synthetic gradient
/// pass into it at the real APT cadence (2 lines / sec). Useful for
/// visual smoke-testing the renderer + window plumbing tonight; the
/// real radio-side wiring lands in #482 (auto-record on overhead
/// pass) and this action goes away.
pub fn connect_demo_action(
    app: &adw::Application,
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
) {
    let action = gio::SimpleAction::new("apt-demo", None);
    let parent_provider = Rc::clone(parent_provider);
    action.connect_activate(glib::clone!(
        #[strong]
        parent_provider,
        move |_, _| {
            let Some(parent) = parent_provider() else {
                tracing::warn!("apt-demo invoked with no main window available");
                return;
            };
            spawn_demo_pass(&parent);
        }
    ));
    app.add_action(&action);
    app.set_accels_for_action("app.apt-demo", &["<Ctrl><Shift>a"]);
}

/// Open a viewer window and start a 500 ms timeout that pumps one
/// synthetic line into it per tick (2 lines/sec, matching real APT).
/// Each line is a 2080-pixel left-to-right grayscale gradient with a
/// row-dependent vertical fade — easy to eyeball as "yep, lines are
/// arriving in order, the renderer is fitting correctly, the image
/// is building downward".
fn spawn_demo_pass<W: gtk4::prelude::IsA<gtk4::Window>>(parent: &W) {
    let view = open_apt_viewer_window(parent, "NOAA APT — Demo Pass (synthetic)");
    let row = Rc::new(Cell::new(0_u32));
    glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
        let mut pixels = [0_u8; LINE_PIXELS];
        let r = row.get();
        // Left-to-right horizontal gradient + a vertical fade so each
        // row is visibly different from the last — enough variation
        // that any rendering bug (off-by-one, wrong stride, scale
        // direction wrong) shows up obviously.
        #[allow(clippy::cast_possible_truncation)]
        for (i, p) in pixels.iter_mut().enumerate() {
            let h = (i * 255 / LINE_PIXELS) as u32;
            let v = (r.wrapping_mul(7)) % 200; // slow-cycling brightness offset
            *p = (((h + v) % 256) & 0xff) as u8;
        }
        view.push_line(&pixels);
        row.set(r.wrapping_add(1));
        // Stop after 2 minutes worth of synthetic pass (240 lines) —
        // long enough to verify auto-fit kicks in, short enough that
        // the demo doesn't run forever.
        if r >= 240 {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
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
    fn push_line_extends_buffer_by_one_line() {
        let mut r = AptImageRenderer::new();
        let initial_capacity = r.pixel_buf.capacity();
        r.push_line(&synth_line(0));
        assert_eq!(r.n_lines(), 1);
        assert_eq!(r.pixel_buf.len(), LINE_PIXELS * 4);
        // Pre-allocation must hold for at least the small test push.
        assert_eq!(
            r.pixel_buf.capacity(),
            initial_capacity,
            "push_line shouldn't realloc against the pre-allocated buffer",
        );
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
        assert_eq!(r.pixel_buf.len(), LINE_PIXELS * 4 * MAX_LINES);
    }

    #[test]
    fn clear_resets_lines_but_keeps_capacity() {
        let mut r = AptImageRenderer::new();
        for _ in 0..TEST_LINE_COUNT {
            r.push_line(&synth_line(0));
        }
        let cap = r.pixel_buf.capacity();
        r.clear();
        assert!(r.is_empty());
        assert_eq!(r.n_lines(), 0);
        assert_eq!(
            r.pixel_buf.capacity(),
            cap,
            "clear shouldn't release the pre-allocated buffer",
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
        assert_eq!(&r.pixel_buf[0..4], &[0x80, 0x80, 0x80, 0xFF]);
        assert_eq!(&r.pixel_buf[4..8], &[0xC0, 0xC0, 0xC0, 0xFF]);
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
