//! Waterfall display renderer using Cairo.
//!
//! Renders a scrolling spectrogram: each FFT frame becomes one horizontal line
//! in a display buffer, mapped through a colormap for visualization. Uses a
//! simple shift-down approach — each new line is inserted at the top and all
//! existing rows shift down by one.

use gtk4::cairo;

use super::colormap;

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

        let bin_count = display_data.len().min(self.display_width);

        // Normalize dB values to 0..255.
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
        let row_bytes = self.display_width * 4;
        let row_start = self.top_row * row_bytes;
        for (i, &val) in self.row_buffer.iter().take(bin_count).enumerate() {
            let color = self.colormap_bgra[val as usize];
            let idx = row_start + i * 4;
            self.pixel_buf[idx] = color[0]; // B
            self.pixel_buf[idx + 1] = color[1]; // G
            self.pixel_buf[idx + 2] = color[2]; // R
            self.pixel_buf[idx + 3] = color[3]; // A
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

    pub fn set_db_range(&mut self, min_db: f32, max_db: f32) {
        if min_db.is_finite() && max_db.is_finite() && max_db > min_db {
            self.min_db = min_db;
            self.max_db = max_db;
        }
    }

    /// Build a Cairo `ImageSurface` from the pixel buffer, handling stride
    /// alignment. Cairo may require row strides wider than our tightly-packed
    /// data; when they differ, rows are padded to match.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    fn to_cairo_surface(&self) -> Result<cairo::ImageSurface, String> {
        if self.display_width == 0 {
            return Err("no waterfall data".to_string());
        }

        let stride = cairo::Format::ARgb32
            .stride_for_width(self.display_width as u32)
            .map_err(|e| format!("stride: {e}"))?;

        let packed_stride = (self.display_width * 4) as i32;
        let buf = if stride == packed_stride {
            self.pixel_buf.clone()
        } else {
            let mut padded = vec![0u8; stride as usize * HISTORY_LINES];
            let row_bytes = self.display_width * 4;
            for row in 0..HISTORY_LINES {
                let src = row * row_bytes;
                let dst = row * stride as usize;
                padded[dst..dst + row_bytes].copy_from_slice(&self.pixel_buf[src..src + row_bytes]);
            }
            padded
        };

        cairo::ImageSurface::create_for_data(
            buf,
            cairo::Format::ARgb32,
            self.display_width as i32,
            HISTORY_LINES as i32,
            stride,
        )
        .map_err(|e| format!("surface: {e}"))
    }

    /// Export the waterfall display to a PNG file.
    ///
    /// The PNG expects rows in visual order (newest on top), but the
    /// internal `pixel_buf` is in ring-buffer order keyed off
    /// `top_row`. We walk the ring once to materialize a linear copy
    /// here — one allocation per export, which is negligible for a
    /// user-triggered "save to PNG" operation.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    pub fn export_png(&self, path: &std::path::Path) -> Result<(), String> {
        if self.display_width == 0 {
            return Err("no waterfall data".to_string());
        }
        let row_bytes = self.display_width * 4;
        let mut linear = vec![0u8; row_bytes * HISTORY_LINES];
        for display_row in 0..HISTORY_LINES {
            let physical_row = (self.top_row + display_row) % HISTORY_LINES;
            let src = physical_row * row_bytes;
            let dst = display_row * row_bytes;
            linear[dst..dst + row_bytes].copy_from_slice(&self.pixel_buf[src..src + row_bytes]);
        }

        let stride = cairo::Format::ARgb32
            .stride_for_width(self.display_width as u32)
            .map_err(|e| format!("stride: {e}"))?;
        let packed_stride = (self.display_width * 4) as i32;
        let buf = if stride == packed_stride {
            linear
        } else {
            let mut padded = vec![0u8; stride as usize * HISTORY_LINES];
            for row in 0..HISTORY_LINES {
                let src = row * row_bytes;
                let dst = row * stride as usize;
                padded[dst..dst + row_bytes].copy_from_slice(&linear[src..src + row_bytes]);
            }
            padded
        };

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
mod tests {
    use super::*;

    #[test]
    fn downsample_preserves_peak() {
        let data = [0.0, 5.0, 1.0, 3.0, 2.0, 8.0, 4.0, 1.0];
        let mut buf = Vec::new();
        downsample_to(&data, &mut buf, 4);
        assert_eq!(buf.len(), 4);
        assert!((buf[0] - 5.0).abs() < f32::EPSILON);
        assert!((buf[1] - 3.0).abs() < f32::EPSILON);
        assert!((buf[2] - 8.0).abs() < f32::EPSILON);
        assert!((buf[3] - 4.0).abs() < f32::EPSILON);
    }

    #[test]
    fn downsample_non_divisible() {
        // 7 bins -> 3: ratio 2.333, buckets [0..3), [2..5), [4..7)
        let data = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0];
        let mut buf = Vec::new();
        downsample_to(&data, &mut buf, 3);
        assert_eq!(buf.len(), 3);
        assert!((buf[0] - 3.0).abs() < f32::EPSILON); // max(1, 2, 3)
        assert!((buf[1] - 5.0).abs() < f32::EPSILON); // max(3, 4, 5)
        assert!((buf[2] - 7.0).abs() < f32::EPSILON); // max(5, 6, 7)
    }

    #[test]
    fn downsample_single_output() {
        let data = [1.0, 9.0, 3.0, 2.0];
        let mut buf = Vec::new();
        downsample_to(&data, &mut buf, 1);
        assert_eq!(buf.len(), 1);
        assert!((buf[0] - 9.0).abs() < f32::EPSILON);
    }

    #[test]
    fn downsample_same_size_passthrough() {
        let data = [1.0, 2.0, 3.0];
        let mut buf = Vec::new();
        downsample_to(&data, &mut buf, 3);
        assert_eq!(buf.len(), 3);
        assert!((buf[0] - 1.0).abs() < f32::EPSILON);
        assert!((buf[1] - 2.0).abs() < f32::EPSILON);
        assert!((buf[2] - 3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rgba_to_bgra_converts_correctly() {
        let rgba = vec![[255, 128, 64, 255]];
        let bgra = rgba_to_bgra(&rgba);
        assert_eq!(bgra[0], [64, 128, 255, 255]);
    }

    #[test]
    fn supported_width_clamped() {
        assert_eq!(supported_texture_width(8192), MAX_TEXTURE_WIDTH);
        assert_eq!(supported_texture_width(1024), 1024);
    }

    /// Helper: read the BGRA pixel at the physical row / column out
    /// of the renderer's internal buffer. Tests use this to verify
    /// that the ring-buffer `top_row` advances correctly.
    fn physical_pixel(r: &WaterfallRenderer, row: usize, col: usize) -> [u8; 4] {
        let idx = (row * r.display_width + col) * 4;
        [
            r.pixel_buf[idx],
            r.pixel_buf[idx + 1],
            r.pixel_buf[idx + 2],
            r.pixel_buf[idx + 3],
        ]
    }

    #[test]
    fn ring_buffer_starts_at_zero() {
        let r = WaterfallRenderer::new(8);
        assert_eq!(r.top_row, 0);
        assert!(r.pixel_buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn ring_buffer_advances_backwards_on_push() {
        // Use a small width so the bench isn't slow, and a known dB
        // range so the normalization produces a deterministic byte.
        let mut r = WaterfallRenderer::new(4);
        r.set_db_range(0.0, 100.0);
        // One push advances `top_row` from 0 to HISTORY_LINES - 1.
        r.push_line(&[50.0, 50.0, 50.0, 50.0]);
        assert_eq!(r.top_row, HISTORY_LINES - 1);
        // Second push advances to HISTORY_LINES - 2.
        r.push_line(&[50.0, 50.0, 50.0, 50.0]);
        assert_eq!(r.top_row, HISTORY_LINES - 2);
    }

    #[test]
    fn ring_buffer_wraps_after_full_cycle() {
        let mut r = WaterfallRenderer::new(4);
        r.set_db_range(0.0, 100.0);
        for _ in 0..HISTORY_LINES {
            r.push_line(&[50.0, 50.0, 50.0, 50.0]);
        }
        // After exactly HISTORY_LINES pushes we wrap back to 0.
        assert_eq!(r.top_row, 0);
        // One more push: back to HISTORY_LINES - 1.
        r.push_line(&[50.0, 50.0, 50.0, 50.0]);
        assert_eq!(r.top_row, HISTORY_LINES - 1);
    }

    #[test]
    fn pushed_row_lands_at_top_row_offset() {
        let mut r = WaterfallRenderer::new(4);
        r.set_db_range(0.0, 100.0);

        // Push a distinctive line: normalized values are 0 / 64 / 128 /
        // 255 across the four pixels. With the default Turbo colormap
        // we don't care about exact RGB — we care that the row was
        // written at the current `top_row`, and the row BEFORE it
        // (physical row `top_row + 1`, still zeroed) is untouched.
        r.push_line(&[0.0, 25.0, 50.0, 100.0]);
        let first_top = r.top_row;
        assert_eq!(first_top, HISTORY_LINES - 1);
        // The new row is non-zero (first pixel uses colormap[0], which
        // may happen to be zero — so instead check the fourth, which
        // uses colormap[255] and is definitely non-zero for Turbo).
        let p = physical_pixel(&r, first_top, 3);
        assert!(p != [0, 0, 0, 0], "new row pixel should be non-zero");
        // And the row ADJACENT to top_row going the other way
        // (physical row 0, which will be overwritten last) is still
        // zero.
        assert_eq!(physical_pixel(&r, 0, 3), [0, 0, 0, 0]);

        // Push a second line and confirm it lands one physical row
        // up, not on top of the previous row.
        r.push_line(&[0.0, 25.0, 50.0, 100.0]);
        assert_eq!(r.top_row, HISTORY_LINES - 2);
        // Previous row unchanged.
        let p_prev = physical_pixel(&r, first_top, 3);
        assert_eq!(
            p, p_prev,
            "previous row pixel must not be touched by next push"
        );
    }

    #[test]
    fn resize_resets_ring_index() {
        let mut r = WaterfallRenderer::new(4);
        r.set_db_range(0.0, 100.0);
        r.push_line(&[50.0, 50.0, 50.0, 50.0]);
        assert_eq!(r.top_row, HISTORY_LINES - 1);
        r.resize(8);
        assert_eq!(r.top_row, 0);
        assert!(r.pixel_buf.iter().all(|&b| b == 0));
    }
}
