//! Waterfall display renderer using Cairo.
//!
//! Renders a scrolling spectrogram: each FFT frame becomes one horizontal
//! line in a display buffer, mapped through a colormap for visualization.
//! The buffer is laid out in Cairo ARGB32 format as a **logical ring**
//! — new lines are written at a rotating `top_row` offset and
//! [`WaterfallRenderer::render`] paints the source surface in two clipped
//! regions to stitch the wrap back into visual order (newest on top).
//!
//! This avoids the per-frame memmove that would otherwise cost
//! `display_width · (HISTORY_LINES-1) · 4` bytes of ring-buffer
//! shift-down every frame — ~1 GB/sec at 4K under naive shift-down,
//! per the epic #452 investigation (PR #458).

use gtk4::cairo;

use super::ScannerAxisLock;
use super::colormap;
use super::fft_plot::bin_to_locked_x;

/// Number of history lines stored in the display buffer.
const HISTORY_LINES: usize = 1024;

/// Maximum waterfall display width. FFT data wider than this is downsampled.
pub const MAX_TEXTURE_WIDTH: usize = 4096;

/// Default minimum display level in dB.
const DEFAULT_MIN_DB: f32 = -70.0;
/// Default maximum display level in dB.
const DEFAULT_MAX_DB: f32 = 0.0;

/// Background clear color — near-black, matching FFT plot.
const BACKGROUND_COLOR: [f64; 4] = [0.08, 0.08, 0.10, 1.0];

/// Downsample FFT data by max-pooling bins to a target width.
///
/// Groups of input bins are reduced to one output bin by taking the maximum
/// dB value in each group, preserving signal peaks for display.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn downsample_to(data: &[f32], buf: &mut Vec<f32>, target_width: usize) {
    buf.resize(target_width, f32::NEG_INFINITY);
    let ratio = data.len() as f32 / target_width as f32;
    for (i, out) in buf.iter_mut().enumerate().take(target_width) {
        let start = (i as f32 * ratio) as usize;
        let end = (((i + 1) as f32) * ratio).ceil() as usize;
        let end = end.min(data.len());
        let mut max_val = f32::NEG_INFINITY;
        for &v in &data[start..end] {
            if v > max_val {
                max_val = v;
            }
        }
        *out = max_val;
    }
}

/// Clamp texture width to the application limit.
fn supported_texture_width(requested: usize) -> usize {
    requested.min(MAX_TEXTURE_WIDTH)
}

/// Public version of `supported_texture_width` for use by `mod.rs`.
pub fn supported_texture_width_for(requested: usize) -> usize {
    supported_texture_width(requested)
}

/// Cairo-based renderer for the scrolling waterfall spectrogram.
///
/// Maintains a pixel buffer in Cairo ARGB32 format as a **logical ring
/// buffer**. New FFT lines are written at a rotating `top_row` offset
/// and `render()` paints the source surface in two clipped regions to
/// stitch the wrap back into visual-order. This replaces the previous
/// shift-down-every-row approach, which dominated CPU time at large
/// display widths (~1 GB/sec of memcpy at 4K per the epic #452
/// investigation, PR #458).
pub struct WaterfallRenderer {
    /// Pre-allocated ARGB32 pixel buffer (`width * HISTORY_LINES * 4` bytes).
    /// Cairo ARGB32 is premultiplied alpha, native byte order (BGRA on LE).
    ///
    /// Physical layout is decoupled from visual order — the row at
    /// display position 0 lives at physical row `top_row`, and rows
    /// wrap modulo `HISTORY_LINES`. See `render()` for the wrap-aware
    /// paint.
    pixel_buf: Vec<u8>,
    /// Pre-allocated buffer for uploading one row of normalized pixel data.
    row_buffer: Vec<u8>,
    /// Width of the display in pixels (= number of FFT bins, capped).
    display_width: usize,
    /// Pre-allocated 256-entry RGBA colormap (stored as `[B, G, R, A]` for
    /// Cairo ARGB32 native byte order on little-endian).
    colormap_bgra: Vec<[u8; 4]>,
    /// Pre-allocated buffer for downsampling large FFT data.
    downsample_buf: Vec<f32>,
    /// Display range in dB.
    min_db: f32,
    max_db: f32,
    /// Physical row index of the most recent (top-of-display) line.
    /// Advances backwards on each `push_line` so the newest row overwrites
    /// the oldest — exactly the shift-down semantics without the memcpy.
    ///
    /// Invariant: `0 ≤ top_row < HISTORY_LINES`.
    top_row: usize,
}

impl WaterfallRenderer {
    /// Create a new waterfall renderer.
    ///
    /// # Arguments
    ///
    /// * `requested_width` — Number of FFT bins (display pixel width).
    pub fn new(requested_width: usize) -> Self {
        let width = supported_texture_width(requested_width);
        let colormap_rgba = colormap::generate_colormap(colormap::ColormapStyle::Turbo);
        let colormap_bgra = rgba_to_bgra(&colormap_rgba);

        Self {
            pixel_buf: vec![0u8; width * HISTORY_LINES * 4],
            row_buffer: vec![0u8; width],
            display_width: width,
            colormap_bgra,
            downsample_buf: Vec::with_capacity(MAX_TEXTURE_WIDTH),
            min_db: DEFAULT_MIN_DB,
            max_db: DEFAULT_MAX_DB,
            top_row: 0,
        }
    }

    /// Push one FFT frame as a new row at the top of the display buffer.
    ///
    /// The dB values are normalized to 0..255 using the current display range,
    /// mapped through the colormap, and written to the physical row at
    /// `top_row` after advancing the ring-buffer index backwards. The
    /// display rendering path (`render()`) handles the wrap-around so
    /// the visual result is the same as the old shift-down approach.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn push_line(&mut self, fft_data: &[f32]) {
        let db_range = self.max_db - self.min_db;
        if !db_range.is_finite() || db_range <= 0.0 {
            return;
        }

        // Downsample if FFT bins exceed display width.
        let display_data = if fft_data.len() > self.display_width {
            downsample_to(fft_data, &mut self.downsample_buf, self.display_width);
            &self.downsample_buf
        } else {
            fft_data
        };

        // Normalize dB values to 0..255. Positions past the FFT's
        // actual bin count stay at `0` from the `fill(0)` — those
        // render as `colormap[0]` (the "no signal" floor color),
        // which is the semantically correct treatment for frequency
        // regions with no real data.
        let bin_count = display_data.len().min(self.display_width);
        self.row_buffer.fill(0);
        for (i, &db) in display_data.iter().take(bin_count).enumerate() {
            let normalized = ((db - self.min_db) / db_range).clamp(0.0, 1.0);
            self.row_buffer[i] = (normalized * 255.0).round() as u8;
        }

        // Advance the ring-buffer index backwards (wrapping). The new
        // line goes into the slot previously holding the oldest row —
        // this is the key move that eliminates the per-frame memmove
        // of `display_width · (HISTORY_LINES-1) · 4` bytes that used to
        // cost ~1 GB/sec at 4K.
        self.top_row = (self.top_row + HISTORY_LINES - 1) % HISTORY_LINES;

        // Write the new row into the physical slot for `top_row`.
        // Iterate the FULL `display_width`, not just `bin_count` —
        // this matters specifically because the ring-buffer path
        // recycles old slots. Skipping the tail would leak the
        // oldest row's right-edge pixels into the newest row, visible
        // on narrow-FFT frames (or during FFT size changes).
        let row_bytes = self.display_width * 4;
        let row_start = self.top_row * row_bytes;
        for (i, &val) in self.row_buffer.iter().enumerate() {
            let color = self.colormap_bgra[val as usize];
            let idx = row_start + i * 4;
            self.pixel_buf[idx] = color[0]; // B
            self.pixel_buf[idx + 1] = color[1]; // G
            self.pixel_buf[idx + 2] = color[2]; // R
            self.pixel_buf[idx + 3] = color[3]; // A
        }
    }

    /// Push a new FFT line under the scanner X-axis lock. Sibling
    /// of [`Self::push_line`]. The narrow FFT data lands in the
    /// active channel's pixel slice of the wider locked range;
    /// the rest of the row gets `colormap[0]` (the "no signal"
    /// floor colour, dark grey) so historical rows render as a
    /// sparse spatial picture of the channels the scanner has
    /// touched. Per issue #516.
    ///
    /// Behaviour notes:
    /// - When `lock.active_channel_hz` is `None` (scanner mode
    ///   engaged but no channel currently sampled), the entire
    ///   row gets `colormap[0]`. This still advances the ring
    ///   buffer so historical rows scroll downward at the
    ///   normal cadence even during retunes between channels.
    /// - Bins falling outside `[lock.min_hz, lock.max_hz]` are
    ///   silently dropped — happens when the FFT span exceeds
    ///   the locked range (rare; LF/HF dongles with the
    ///   scanner band entirely fitting inside one FFT frame).
    /// - Per-pixel max-pool: when multiple bins map to the
    ///   same pixel (channel narrower than the FFT), the
    ///   loudest bin wins. Same trade-off as `downsample_to`
    ///   for the unlocked path — preserves peaks.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn push_line_locked(&mut self, fft_data: &[f32], full_bw_hz: f64, lock: &ScannerAxisLock) {
        let db_range = self.max_db - self.min_db;
        if !db_range.is_finite() || db_range <= 0.0 {
            return;
        }

        // Step 1 — fill the row with the dark-grey "no signal"
        // floor. We always do this even when active is None so
        // the ring buffer gets fresh content (rather than a
        // stale row from N rotations ago) on every push.
        self.row_buffer.fill(0);

        // Zero-width guard. `new(0)` / `resize(0)` produce a
        // renderer with `display_width == 0`; the projection
        // below would underflow `w - 1.0` to negative and cast
        // to a huge `usize`, panicking on the
        // `self.row_buffer[pixel_idx]` index. Skip the projection
        // and the pixel-row write — but DO advance the ring so
        // historical rows still scroll downward at the normal
        // cadence. Per `CodeRabbit` round 2 on PR #562.
        if self.display_width == 0 {
            self.top_row = (self.top_row + HISTORY_LINES - 1) % HISTORY_LINES;
            return;
        }

        // Step 2 — when a channel is active, project narrow
        // bins into the active slice's pixels.
        if let Some(active_hz) = lock.active_channel_hz {
            let n_bins = fft_data.len();
            let w = self.display_width as f64;
            for (i, &db) in fft_data.iter().enumerate() {
                let x = bin_to_locked_x(
                    i,
                    n_bins,
                    full_bw_hz,
                    active_hz,
                    lock.min_hz,
                    lock.max_hz,
                    w,
                );
                // Allow `x == w` so a bin landing exactly on
                // the upper-bound pixel (boundary-aligned
                // inputs — last bin maps to `active + bw/2`,
                // which can equal `axis_max_hz` when the
                // active channel sits at the upper envelope
                // edge) still renders. Clamp via `min(w-1)` so
                // the cast below is in-range. Per `CodeRabbit`
                // round 1 on PR #562.
                if !x.is_finite() || x < 0.0 || x > w {
                    continue;
                }
                let pixel_idx = x.min(w - 1.0) as usize;
                let normalized = ((db - self.min_db) / db_range).clamp(0.0, 1.0);
                let val = (normalized * 255.0).round() as u8;
                // Per-pixel max-pool: narrow channels squeeze
                // many bins per pixel, and we want the loudest
                // to win. Same rationale as `downsample_to` for
                // the unlocked path. Per issue #516.
                if val > self.row_buffer[pixel_idx] {
                    self.row_buffer[pixel_idx] = val;
                }
            }
        }

        // Step 3 — advance the ring + write pixels. Identical
        // to the unlocked tail-end of `push_line`.
        self.top_row = (self.top_row + HISTORY_LINES - 1) % HISTORY_LINES;
        let row_bytes = self.display_width * 4;
        let row_start = self.top_row * row_bytes;
        for (i, &val) in self.row_buffer.iter().enumerate() {
            let color = self.colormap_bgra[val as usize];
            let idx = row_start + i * 4;
            self.pixel_buf[idx] = color[0];
            self.pixel_buf[idx + 1] = color[1];
            self.pixel_buf[idx + 2] = color[2];
            self.pixel_buf[idx + 3] = color[3];
        }
    }

    /// Render the waterfall display to the given Cairo context.
    ///
    /// Blits the pixel buffer as a Cairo `ImageSurface` scaled to the
    /// requested output size. When zoomed in, only the visible frequency
    /// portion is shown by translating and scaling the source surface.
    ///
    /// Because the pixel buffer is a logical ring (see `pixel_buf`
    /// doc comment), the paint is split into two clipped regions —
    /// one covering physical rows `[top_row..HISTORY_LINES)` at the top
    /// of the display, and one covering `[0..top_row)` at the bottom.
    /// Each region uses the same source surface with a different
    /// `set_source_surface` origin to translate physical rows into
    /// display positions. Cairo does the composited paint in hardware
    /// where available.
    #[allow(clippy::cast_precision_loss)]
    pub fn render(
        &self,
        cr: &cairo::Context,
        width: i32,
        height: i32,
        display_start_hz: f64,
        display_end_hz: f64,
        full_bandwidth: f64,
    ) {
        if width <= 0 || height <= 0 || self.display_width == 0 {
            return;
        }

        // Background.
        cr.set_source_rgba(
            BACKGROUND_COLOR[0],
            BACKGROUND_COLOR[1],
            BACKGROUND_COLOR[2],
            BACKGROUND_COLOR[3],
        );
        let _ = cr.paint();

        let Ok(surface) = self.to_cairo_surface() else {
            return;
        };

        // Compute the visible portion of the full-bandwidth surface.
        // The pixel buffer spans -full_bw/2 .. +full_bw/2.
        // The display range is display_start_hz .. display_end_hz.
        let effective_full_bw = if full_bandwidth > 0.0 {
            full_bandwidth
        } else {
            display_end_hz - display_start_hz
        };

        let full_start_hz = -effective_full_bw / 2.0;

        // Fractional position of the visible range within the full surface.
        let visible_start_frac = if effective_full_bw > 0.0 {
            (display_start_hz - full_start_hz) / effective_full_bw
        } else {
            0.0
        };
        let visible_end_frac = if effective_full_bw > 0.0 {
            (display_end_hz - full_start_hz) / effective_full_bw
        } else {
            1.0
        };

        let visible_width_frac = visible_end_frac - visible_start_frac;

        // Scale so HISTORY_LINES rows × visible display_width cols map
        // to the requested output rect.
        let _ = cr.save();
        let y_scale = f64::from(height) / HISTORY_LINES as f64;
        let x_scale = if visible_width_frac > 0.0 {
            f64::from(width) / (self.display_width as f64 * visible_width_frac)
        } else {
            f64::from(width) / self.display_width as f64
        };
        cr.scale(x_scale, y_scale);

        // Zoom/pan source offset in pre-scale (source-pixel) units.
        let src_offset_x = -(visible_start_frac * self.display_width as f64);

        // Extent of the display in pre-scale units — the clip rects
        // reference this so they cover exactly the visible region.
        let display_extent_x = f64::from(width) / x_scale;

        // Region A: physical rows [top_row..HISTORY_LINES) → display
        // rows [0..region_a_rows). The source-surface origin's Y is
        // set to `-top_row` so that user-space (display) row 0 samples
        // source row `top_row` — i.e., the newest line lands at the
        // top of the waterfall, exactly like the old shift-down.
        let region_a_rows = HISTORY_LINES - self.top_row;
        if region_a_rows > 0 {
            let _ = cr.save();
            cr.rectangle(0.0, 0.0, display_extent_x, region_a_rows as f64);
            cr.clip();
            let _ = cr.set_source_surface(&surface, src_offset_x, -(self.top_row as f64));
            cr.source().set_filter(cairo::Filter::Nearest);
            let _ = cr.paint();
            let _ = cr.restore();
        }

        // Region B: physical rows [0..top_row) → display rows
        // [region_a_rows..HISTORY_LINES). Source origin Y is set to
        // `region_a_rows` so that user-space row `region_a_rows` samples
        // source row 0 — i.e., the wrap stitches the oldest block
        // seamlessly below region A.
        if self.top_row > 0 {
            let _ = cr.save();
            cr.rectangle(
                0.0,
                region_a_rows as f64,
                display_extent_x,
                self.top_row as f64,
            );
            cr.clip();
            let _ = cr.set_source_surface(&surface, src_offset_x, region_a_rows as f64);
            cr.source().set_filter(cairo::Filter::Nearest);
            let _ = cr.paint();
            let _ = cr.restore();
        }

        let _ = cr.restore();
    }

    /// Update the colormap with a new style.
    pub fn set_colormap(&mut self, style: colormap::ColormapStyle) {
        let map = colormap::generate_colormap(style);
        self.colormap_bgra = rgba_to_bgra(&map);
    }

    /// Wipe the entire waterfall surface back to its as-built blank
    /// state. Used when the waterfall master toggle goes off
    /// (#646) so the user doesn't see a frozen snapshot of pre-
    /// disable data while the FFT compute is suspended. Re-enabling
    /// the toggle starts painting from the top — same behavior as
    /// a fresh launch with no data yet.
    pub fn clear(&mut self) {
        self.pixel_buf.fill(0);
        self.top_row = 0;
    }

    pub fn set_db_range(&mut self, min_db: f32, max_db: f32) {
        if min_db.is_finite() && max_db.is_finite() && max_db > min_db {
            self.min_db = min_db;
            self.max_db = max_db;
        }
    }

    /// Query Cairo's required stride for our ARGB32 row width.
    /// Returns `(stride_bytes, packed_stride_bytes)` so callers can
    /// decide whether to use a zero-copy packed buffer or a padded
    /// copy.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn argb32_strides(display_width: usize) -> Result<(i32, i32), String> {
        let stride = cairo::Format::ARgb32
            .stride_for_width(display_width as u32)
            .map_err(|e| format!("stride: {e}"))?;
        let packed_stride = (display_width * 4) as i32;
        Ok((stride, packed_stride))
    }

    /// Re-pack `linear` (tightly-packed `display_width * 4` bytes
    /// per row) into whatever stride Cairo demands for the given
    /// width. Returns either the input `Vec` unchanged (when
    /// Cairo's stride matches the packed stride — the common case
    /// on typical widths) or a fresh padded copy.
    ///
    /// Used by both `to_cairo_surface` and `export_png` to keep
    /// the ARGB32 packing logic in one place.
    #[allow(clippy::cast_sign_loss)]
    fn pack_argb32_for_cairo(
        mut linear: Vec<u8>,
        display_width: usize,
        stride: i32,
        packed_stride: i32,
    ) -> Vec<u8> {
        if stride == packed_stride {
            return linear;
        }
        let row_bytes = display_width * 4;
        // `stride` comes from `cairo::Format::stride_for_width`, which
        // returns `Err` on negative values — safe to treat as unsigned.
        let stride_usize = stride as usize;
        let mut padded = vec![0u8; stride_usize * HISTORY_LINES];
        for row in 0..HISTORY_LINES {
            let src = row * row_bytes;
            let dst = row * stride_usize;
            padded[dst..dst + row_bytes].copy_from_slice(&linear[src..src + row_bytes]);
        }
        // Drop the unpadded copy explicitly rather than relying on
        // it falling out of scope — makes the "we're abandoning
        // this buffer" intent obvious.
        linear.clear();
        padded
    }

    /// Build a Cairo `ImageSurface` over the ring-ordered pixel
    /// buffer. `render()` handles the ring wrap via two clipped
    /// paints, so no re-ordering happens here.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn to_cairo_surface(&self) -> Result<cairo::ImageSurface, String> {
        if self.display_width == 0 {
            return Err("no waterfall data".to_string());
        }
        let (stride, packed_stride) = Self::argb32_strides(self.display_width)?;
        let buf = Self::pack_argb32_for_cairo(
            self.pixel_buf.clone(),
            self.display_width,
            stride,
            packed_stride,
        );

        cairo::ImageSurface::create_for_data(
            buf,
            cairo::Format::ARgb32,
            self.display_width as i32,
            HISTORY_LINES as i32,
            stride,
        )
        .map_err(|e| format!("surface: {e}"))
    }

    /// Walk the ring buffer once to produce a linearly-ordered copy
    /// of `pixel_buf` — display row 0 on top, row `HISTORY_LINES-1`
    /// on the bottom. Used by [`Self::export_png`] and covered by a
    /// dedicated test so the ordering is verifiable without
    /// round-tripping through PNG I/O.
    fn linearized_pixel_buf(&self) -> Vec<u8> {
        let row_bytes = self.display_width * 4;
        let mut linear = vec![0u8; row_bytes * HISTORY_LINES];
        for display_row in 0..HISTORY_LINES {
            let physical_row = (self.top_row + display_row) % HISTORY_LINES;
            let src = physical_row * row_bytes;
            let dst = display_row * row_bytes;
            linear[dst..dst + row_bytes].copy_from_slice(&self.pixel_buf[src..src + row_bytes]);
        }
        linear
    }

    /// Export the waterfall display to a PNG file.
    ///
    /// The PNG expects rows in visual order (newest on top), but the
    /// internal `pixel_buf` is in ring-buffer order keyed off
    /// `top_row` — so this uses [`Self::linearized_pixel_buf`] to
    /// materialize one allocation of correctly-ordered pixels before
    /// handing it to Cairo. The allocation cost is negligible for a
    /// user-triggered "save to PNG" operation.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    pub fn export_png(&self, path: &std::path::Path) -> Result<(), String> {
        if self.display_width == 0 {
            return Err("no waterfall data".to_string());
        }

        let linear = self.linearized_pixel_buf();
        let (stride, packed_stride) = Self::argb32_strides(self.display_width)?;
        let buf = Self::pack_argb32_for_cairo(linear, self.display_width, stride, packed_stride);

        let surface = cairo::ImageSurface::create_for_data(
            buf,
            cairo::Format::ARgb32,
            self.display_width as i32,
            HISTORY_LINES as i32,
            stride,
        )
        .map_err(|e| format!("surface: {e}"))?;

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut file = std::fs::File::create(path).map_err(|e| format!("file: {e}"))?;
        surface
            .write_to_png(&mut file)
            .map_err(|e| format!("png: {e}"))?;
        tracing::info!(?path, "waterfall exported to PNG");
        Ok(())
    }

    /// Current display width in bins.
    pub fn texture_width(&self) -> usize {
        self.display_width
    }

    /// Resize the waterfall for a new FFT size.
    ///
    /// Resets history on every call to clear mixed-resolution data.
    /// The ring-buffer index is reset as well so post-resize renders
    /// start from a consistent state.
    pub fn resize(&mut self, new_width: usize) {
        let capped_width = supported_texture_width(new_width);
        self.display_width = capped_width;
        self.pixel_buf = vec![0u8; capped_width * HISTORY_LINES * 4];
        self.row_buffer = vec![0u8; capped_width];
        self.top_row = 0;
        tracing::debug!(width = capped_width, "waterfall display reset");
    }
}

/// Convert RGBA colormap entries to Cairo's native ARGB32 byte order (BGRA
/// on little-endian systems). All entries are fully opaque so premultiplied
/// alpha is a no-op.
fn rgba_to_bgra(rgba: &[[u8; 4]]) -> Vec<[u8; 4]> {
    rgba.iter().map(|&[r, g, b, a]| [b, g, r, a]).collect()
}

#[cfg(test)]
mod tests;
