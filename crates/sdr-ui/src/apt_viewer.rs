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

use sdr_dsp::apt::{AptLine, LINE_PIXELS};
use sdr_radio::apt_image::{AptImage, BrightnessMode, rotate_180_per_channel};

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
    /// keeps them out of the visible output. Holds the live
    /// preview pixels (per-line min/max normalized) so the user
    /// gets immediate feedback as lines come in.
    surface: cairo::ImageSurface,
    n_lines: usize,
    /// Parallel storage of raw f32 envelope samples + per-line
    /// metadata, used at PNG-export time to apply image-wide
    /// brightness modes (per [`BrightnessMode`]). Lives separately
    /// from the Cairo surface because the live preview uses
    /// per-line normalization (already-u8) while export uses
    /// image-wide normalization (re-derived from `raw_samples`).
    /// Reset via [`AptImageRenderer::clear`].
    apt_image: AptImage,
    /// Whether this pass was on the satellite's ascending leg
    /// (heading north) — set at AOS by the wiring layer's
    /// `RecorderAction::StartAutoRecord` handler via
    /// [`AptImageView::set_rotate_180`]. The toolbar `Export PNG`
    /// button reads this so a manual export rotates the same way
    /// as the auto-record save would. Reset to `false` on
    /// [`AptImageRenderer::clear`] (per-pass scoping). Per CR round
    /// 1 on PR #571.
    rotate_180: bool,
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
            apt_image: AptImage::with_capacity(std::time::Instant::now(), MAX_LINES),
            rotate_180: false,
        }
    }

    /// Append one APT scan line. The line's per-line-normalized
    /// `pixels` go straight into the live preview surface as
    /// ARGB32. The line's raw f32 `raw_samples` are stored in the
    /// parallel `AptImage` for image-wide brightness mapping at
    /// PNG-export time. No allocation, no buffer growth — we write
    /// the row directly into the pre-allocated surface. No-op once
    /// [`MAX_LINES`] is reached.
    pub fn push_line(&mut self, line: &AptLine) {
        // Cap on the export-state mirror's length, NOT `n_lines`.
        // `n_lines` only advances on a successful Cairo preview
        // write — if the preview rail has been failing for a
        // stretch the mirror could otherwise grow unbounded past
        // `MAX_LINES`. Gating on `apt_image.len()` keeps both rails
        // bounded by the same cap independent of preview health.
        // Per CR round 5 on PR #571.
        if self.apt_image.len() >= MAX_LINES {
            return;
        }
        // Try the live-preview write first, but never let a Cairo
        // failure here strip the line out of the export-state mirror
        // (`AptImage`). The two are independent rails: the preview
        // drives the on-screen viewer; the mirror drives PNG export
        // and auto-record save. A transient surface-data lock
        // failure should at worst cost us one rendered row in the
        // live view — it must not silently delete the line from
        // disk output. Per CR round 3 on PR #571.
        //
        // Also defend the preview surface: only attempt the row
        // write while `n_lines < MAX_LINES` so a stuck-failing
        // mirror push (we'd never have reached this point if the
        // mirror had hit the cap, but defensive) can't drive the
        // surface write past row index `MAX_LINES - 1`.
        let preview_ok = if self.n_lines < MAX_LINES {
            self.write_line_to_surface(line)
        } else {
            false
        };
        if preview_ok {
            self.n_lines += 1;
        }

        // Always mirror into the AptImage for image-wide PNG export,
        // regardless of preview success. `apt_image::push_line`
        // honors the sync_quality threshold independently — a line
        // we drew via the per-line surface may still go to the image
        // as gap-filled if quality is low. The two paths can
        // disagree on a borderline line; this is intentional (live
        // preview is forgiving, export is strict).
        self.apt_image.push_line(line, std::time::Instant::now());
    }

    /// Writes one APT line into the persistent ARGB32 surface at
    /// row `self.n_lines`. Returns `false` (logging a warning) on
    /// any Cairo failure so the caller can decide what to do —
    /// `push_line` skips advancing `n_lines` but still mirrors into
    /// the export image. Per CR round 3 on PR #571.
    fn write_line_to_surface(&mut self, line: &AptLine) -> bool {
        let stride = match usize::try_from(self.surface.stride()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("APT renderer: invalid surface stride: {e}");
                return false;
            }
        };
        let row_offset = self.n_lines * stride;
        let mut data = match self.surface.data() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("APT renderer: surface data lock failed: {e}");
                return false;
            }
        };
        for (i, &g) in line.pixels.iter().enumerate() {
            let pixel_offset = row_offset + i * 4;
            data[pixel_offset] = g;
            data[pixel_offset + 1] = g;
            data[pixel_offset + 2] = g;
            data[pixel_offset + 3] = 0xFF;
        }
        // `data` guard drops here, flushing the surface for cairo.
        drop(data);
        true
    }

    /// Reset to an empty image. Zeroes the surface bytes (fully
    /// transparent everywhere) so subsequent passes start from a
    /// clean canvas; the surface itself is preserved. Also resets
    /// the parallel `AptImage` so PNG export sees only the new
    /// pass's lines (per-pass calibration). Preserves the
    /// `rotate_180` flag — the AOS flow always calls
    /// [`AptImageView::set_rotate_180`] right after `clear`, so
    /// the AOS path doesn't depend on this reset; meanwhile a
    /// mid-pass user Clear must NOT lose the AOS-stamped pass
    /// orientation, otherwise a manual export of a half-cleared
    /// ascending pass comes out unrotated. Per CR round 4 on
    /// PR #571.
    pub fn clear(&mut self) {
        if let Ok(mut data) = self.surface.data() {
            data.fill(0);
        }
        self.n_lines = 0;
        // Replace the AptImage rather than mutate — keeps the
        // pass-start `Instant` semantically correct as "moment of clear"
        // (= moment a new pass started capturing).
        self.apt_image = AptImage::with_capacity(std::time::Instant::now(), MAX_LINES);
    }

    /// Set the rotate-180 flag for the in-progress pass. Called by
    /// the wiring layer's `RecorderAction::StartAutoRecord` handler
    /// after it computes [`sdr_sat::is_ascending`] for this pass.
    /// The flag is read by the toolbar `Export PNG` button so manual
    /// exports match the auto-record orientation. Per CR round 1 on
    /// PR #571.
    pub fn set_rotate_180(&mut self, rotate_180: bool) {
        self.rotate_180 = rotate_180;
    }

    /// Currently-stashed rotate-180 flag for the in-progress pass.
    #[must_use]
    pub fn rotate_180(&self) -> bool {
        self.rotate_180
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
        self.export_png_with_mode(path, BrightnessMode::default())
    }

    /// Export the assembled APT image to a PNG file with image-wide
    /// brightness mapping per the requested [`BrightnessMode`].
    ///
    /// Uses the parallel `AptImage`'s `finalize_grayscale` to
    /// re-derive pixels from the raw f32 envelope samples — gives
    /// proper image-wide contrast (vs. the live surface's per-line
    /// normalization). Per B1 of the noaa-apt parity work.
    ///
    /// Reads the renderer's stored `rotate_180` flag (set at AOS via
    /// [`AptImageView::set_rotate_180`]) so a manual export from the
    /// viewer toolbar matches what the auto-record save would
    /// produce — was hard-coded `false` before, leaving manual
    /// exports of ascending passes upside-down. Per CR round 1 on
    /// PR #571.
    pub fn export_png_with_mode(
        &self,
        path: &Path,
        mode: BrightnessMode,
    ) -> Result<(), ViewerError> {
        self.export_png_full(path, mode, self.rotate_180)
    }

    /// Like [`Self::export_png_with_mode`] but additionally rotates
    /// each video channel 180° if `rotate_180` is true. Use for
    /// ascending passes (heading north) — see
    /// [`rotate_180_per_channel`] for the layout rationale. Per B2
    /// of the noaa-apt parity work.
    ///
    /// Synchronous — runs the full finalize / rotate / encode chain
    /// inline. Fine for offline tools and unit tests. **Live GTK
    /// callers must use [`AptImageView::export_png_full_async`]**
    /// instead, which snapshots the image on the main thread and
    /// runs the encoder in `gio::spawn_blocking`. A 1500-line PNG
    /// encode takes hundreds of milliseconds — long enough to
    /// freeze the UI noticeably. Per CR round 1 on PR #571.
    pub fn export_png_full(
        &self,
        path: &Path,
        mode: BrightnessMode,
        rotate_180: bool,
    ) -> Result<(), ViewerError> {
        // Delegate to the free function so the sync and async paths
        // share one implementation. The free function takes
        // `&AptImage` — the sync path borrows directly; the async
        // path moves an owned snapshot into the worker closure and
        // borrows from there. Per CR round 1 on PR #571.
        render_and_save_apt_png(&self.apt_image, path, mode, rotate_180)
    }

    /// Cheap clone of the renderer's `AptImage` for handing to a
    /// `gio::spawn_blocking` worker. Cost is one large `memcpy`
    /// (~10 KB / line × `n_lines`, ~15 MB at `MAX_LINES`); cheap
    /// compared to the multi-second alternative of running the
    /// encode synchronously on the GTK main loop. Per CR round 1
    /// on PR #571.
    pub fn snapshot_apt_image(&self) -> AptImage {
        self.apt_image.clone()
    }
}

/// Send-safe PNG export. Takes an owned `AptImage` so it can be
/// moved into a `gio::spawn_blocking` worker and run completely off
/// the GTK main thread. Performs:
///
/// 1. `finalize_grayscale(mode)` — image-wide brightness mapping
///    (sort for percentile, etc.).
/// 2. `rotate_180_per_channel` if requested (vertical+horizontal
///    flip of the two video sub-rectangles, leaving sync /
///    telemetry strips alone).
/// 3. Cairo ARGB32 surface build (every pixel R=G=B grey, A=0xFF).
/// 4. PNG encode via Cairo's `write_to_png`.
///
/// Cairo objects are `!Send`, but we only ever create + use them
/// inside this single function call, so they never cross thread
/// boundaries — running this on a worker thread is fine.
///
/// # Errors
///
/// Returns [`ViewerError::EmptyChannel`] (with `apid: None` — APT
/// has no APIDs) when the image has no lines, or a downstream
/// `Cairo` / `Io` / `PngEncode` variant on the failing step.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub fn render_and_save_apt_png(
    image: &AptImage,
    path: &Path,
    mode: BrightnessMode,
    rotate_180: bool,
) -> Result<(), ViewerError> {
    if image.is_empty() {
        return Err(ViewerError::EmptyChannel { apid: None });
    }
    // For bare filenames like "foo.png", `path.parent()` returns
    // `Some("")`, and `create_dir_all("")` errors with NotFound.
    // Skip the call entirely when the parent is empty — the file
    // will be created in the current working directory. Per CR
    // round 2 on PR #571.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| ViewerError::Io {
            op: "create_dir_all",
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let height = image.len();
    let mut pixels = image.finalize_grayscale(mode);
    debug_assert_eq!(pixels.len(), AptImage::WIDTH * height);
    if rotate_180 {
        rotate_180_per_channel(&mut pixels, height);
    }

    // Build a fresh ARGB32 surface from the grayscale Vec<u8>.
    // Cairo's surface is the simplest path to PNG encoding from
    // GTK code — it's already the export format we use elsewhere
    // (LRPT viewer). Each pixel becomes opaque grey (R=G=B, A=0xFF).
    let mut export_surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, LINE_PIXELS as i32, height as i32)
            .map_err(|e| ViewerError::Cairo {
                op: "export surface",
                source: e,
            })?;
    let stride = usize::try_from(export_surface.stride())?;
    {
        let mut data = export_surface.data()?;
        for (row, line) in pixels.chunks_exact(LINE_PIXELS).enumerate() {
            let row_offset = row * stride;
            for (col, &g) in line.iter().enumerate() {
                let p = row_offset + col * 4;
                data[p] = g;
                data[p + 1] = g;
                data[p + 2] = g;
                data[p + 3] = 0xFF;
            }
        }
    }

    let mut file = std::fs::File::create(path).map_err(|e| ViewerError::Io {
        op: "file create",
        path: path.to_path_buf(),
        source: e,
    })?;
    export_surface.write_to_png(&mut file)?;
    tracing::info!(
        ?path,
        lines = height,
        ?mode,
        rotate_180,
        "APT image exported to PNG (image-wide brightness)"
    );
    Ok(())
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
    pub fn push_line(&self, line: &AptLine) {
        self.renderer.borrow_mut().push_line(line);
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

    /// Save with image-wide brightness mapping + optional 180°
    /// rotation, **synchronously**. Tests use this for round-trip
    /// validation; runtime callers must use
    /// [`Self::export_png_full_async`] instead so the GTK main
    /// loop doesn't freeze for the duration of the encode. Per CR
    /// round 1 on PR #571.
    ///
    /// # Errors
    ///
    /// Propagates any [`ViewerError`] from the underlying renderer.
    pub fn export_png_full(
        &self,
        path: &Path,
        mode: BrightnessMode,
        rotate_180: bool,
    ) -> Result<(), ViewerError> {
        self.renderer
            .borrow()
            .export_png_full(path, mode, rotate_180)
    }

    /// Asynchronously export the assembled APT image to PNG.
    ///
    /// Snapshots the current image state on the GTK main thread
    /// (cheap clone, ~15 MB worst-case for a full pass), spawns
    /// a `gio::spawn_blocking` worker for the CPU-heavy encode,
    /// then marshals the result back to the main context where
    /// `on_complete` fires. The main loop stays responsive
    /// throughout — the user can interact with the rest of the
    /// UI while the PNG is being written. Per CR round 1 on PR
    /// #571.
    ///
    /// `on_complete` runs on the GTK main thread, so it can
    /// safely capture `Rc`-shared widgets / `WeakRef`s and post
    /// toasts / close windows / etc.
    pub fn export_png_full_async(
        &self,
        path: PathBuf,
        mode: BrightnessMode,
        rotate_180: bool,
        on_complete: impl FnOnce(Result<(), ViewerError>) + 'static,
    ) {
        let snapshot = self.renderer.borrow().snapshot_apt_image();
        glib::spawn_future_local(async move {
            let join = gio::spawn_blocking(move || {
                render_and_save_apt_png(&snapshot, &path, mode, rotate_180)
            })
            .await;
            // `gio::spawn_blocking::JoinHandle` returns `Result<T, JoinError>`
            // where the outer error is "the worker panicked". Treat a
            // panic as an Internal-style failure so the on_complete
            // callback's caller can still toast it.
            let result = match join {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("APT PNG export worker panicked: {e:?}");
                    Err(ViewerError::InvalidBuffer(
                        "PNG export worker panicked — see logs".to_string(),
                    ))
                }
            };
            on_complete(result);
        });
    }

    /// Number of lines currently in the buffer.
    #[must_use]
    pub fn n_lines(&self) -> usize {
        self.renderer.borrow().n_lines()
    }

    /// Set the rotate-180 flag on the underlying renderer for the
    /// in-progress pass. Called by the wiring layer's
    /// `RecorderAction::StartAutoRecord` handler so the toolbar's
    /// manual `Export PNG` button uses the same orientation as the
    /// auto-record save. Per CR round 1 on PR #571.
    pub fn set_rotate_180(&self, rotate_180: bool) {
        self.renderer.borrow_mut().set_rotate_180(rotate_180);
    }

    /// Read the renderer's stored `rotate_180` flag. Used by the
    /// toolbar `Export PNG` button so manual exports respect the
    /// auto-record orientation pinned at AOS. Per CR round 2 on PR
    /// #571.
    #[must_use]
    pub fn rotate_180(&self) -> bool {
        self.renderer.borrow().rotate_180()
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

    // Clear button — wipes the buffered scan lines and queues a
    // redraw. Useful when (a) the user just exported a pass and
    // wants a clean canvas before the next AOS, or (b) a static-
    // noise patch from a non-pass test filled the surface and
    // they want a fresh start before the next real signal
    // arrives. Without this, the only recourse is closing the
    // viewer window and waiting for the next AOS to re-open it.
    // The `AptImageView::clear` primitive already exists; this
    // just wires it to a control. Per issue #515.
    let clear_btn = gtk4::Button::builder()
        .icon_name("edit-clear-all-symbolic")
        .tooltip_text("Clear the image buffer and start fresh")
        .build();
    clear_btn.update_property(&[gtk4::accessible::Property::Label("Clear APT image buffer")]);
    let clear_view = view.clone();
    clear_btn.connect_clicked(move |_| {
        clear_view.clear();
    });
    header.pack_start(&clear_btn);

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
    // Disable the button on click and re-enable in the completion
    // callback so a fast double-click can't fire two concurrent
    // writers against the same auto-generated path. The default
    // filename is second-granularity, and `gio::spawn_blocking`
    // doesn't serialize work — without re-entry blocking, two
    // workers could truncate or interleave bytes in the same PNG.
    // The button uses a WeakRef so the callback closure doesn't
    // form a retention cycle (the button owns the closure via
    // `connect_clicked`). Per CR round 5 on PR #571.
    let export_btn_weak = export_btn.downgrade();
    export_btn.connect_clicked(move |_| {
        let Some(window_for_export) = window_for_export.upgrade() else {
            return;
        };
        let Some(btn) = export_btn_weak.upgrade() else {
            return;
        };
        if !btn.is_sensitive() {
            // A previous click is still saving. The visual signal
            // (greyed-out button) is the user-facing cue; this
            // guard makes the no-op explicit even if some
            // accessibility path bypasses sensitivity styling.
            return;
        }
        btn.set_sensitive(false);
        let btn_for_complete = btn.downgrade();
        let path = default_export_path();
        let path_for_msg = path.clone();
        let window_weak = window_for_export.downgrade();
        // Read the renderer's stored orientation so manual toolbar
        // exports match the auto-record save's orientation (set at
        // AOS based on the pass direction). Per CR round 2 on PR
        // #571.
        let rotate_180 = export_view.rotate_180();
        // Snapshot + offload to worker. On the click-event main-thread
        // path we'd otherwise burn ~hundreds of ms encoding the PNG,
        // freezing the UI. Per CR round 1 on PR #571.
        export_view.export_png_full_async(
            path,
            BrightnessMode::default(),
            rotate_180,
            move |result| {
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
                // Re-enable the export button on both Ok and Err
                // so a permanent disable can't strand the user
                // after a transient export failure.
                if let Some(btn) = btn_for_complete.upgrade() {
                    btn.set_sensitive(true);
                }
            },
        );
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

    /// Build a synthetic `AptLine` with deterministic per-line content.
    /// Uses high `sync_quality` so the line passes the gap-fill threshold
    /// in the parallel `AptImage`. `raw_samples` mirror `pixels` to keep
    /// `finalize_grayscale` predictable.
    fn synth_line(seed: u8) -> AptLine {
        let mut line = AptLine {
            sync_quality: 0.95,
            ..AptLine::default()
        };
        for (i, p) in line.pixels.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            {
                *p = ((i + usize::from(seed)) & 0xff) as u8;
            }
        }
        for (i, s) in line.raw_samples.iter_mut().enumerate() {
            #[allow(
                clippy::cast_precision_loss,
                reason = "value is in [0, 255]; fits f32 mantissa exactly"
            )]
            let v = ((i + usize::from(seed)) & 0xff) as f32;
            *s = v;
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
        let mut line = AptLine {
            sync_quality: 0.95,
            ..AptLine::default()
        };
        line.pixels[0] = 0x80;
        line.pixels[1] = 0xC0;
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
