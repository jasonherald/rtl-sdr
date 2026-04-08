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
/// Maintains a pixel buffer in Cairo ARGB32 format. New FFT lines are
/// inserted at the top by shifting existing rows down via `copy_within`.
pub struct WaterfallRenderer {
    /// Pre-allocated ARGB32 pixel buffer (`width * HISTORY_LINES * 4` bytes).
    /// Cairo ARGB32 is premultiplied alpha, native byte order (BGRA on LE).
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
        }
    }

    /// Push one FFT frame as a new row at the top of the display buffer.
    ///
    /// The dB values are normalized to 0..255 using the current display range,
    /// mapped through the colormap, and written to the top row. Existing rows
    /// shift down by one.
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

        // Shift all existing rows down by one row (memmove).
        let row_bytes = self.display_width * 4;
        let total_bytes = self.pixel_buf.len();
        if total_bytes > row_bytes {
            self.pixel_buf
                .copy_within(0..total_bytes - row_bytes, row_bytes);
        }

        // Write the new row at the top (row 0).
        for (i, &val) in self.row_buffer.iter().take(bin_count).enumerate() {
            let color = self.colormap_bgra[val as usize];
            let idx = i * 4;
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
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
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

        // Calculate the stride Cairo requires for ARGB32.
        let Ok(stride) = cairo::Format::ARgb32.stride_for_width(self.display_width as u32) else {
            return;
        };

        // If Cairo's required stride matches our tightly-packed data, we can
        // create the surface directly. Otherwise we need to pad each row.
        let expected_stride = (self.display_width * 4) as i32;
        let surface = if stride == expected_stride {
            cairo::ImageSurface::create_for_data(
                self.pixel_buf.clone(),
                cairo::Format::ARgb32,
                self.display_width as i32,
                HISTORY_LINES as i32,
                stride,
            )
        } else {
            // Pad rows to match Cairo's required stride.
            let mut padded = vec![0u8; stride as usize * HISTORY_LINES];
            for row in 0..HISTORY_LINES {
                let src_start = row * self.display_width * 4;
                let src_end = src_start + self.display_width * 4;
                let dst_start = row * stride as usize;
                let dst_end = dst_start + self.display_width * 4;
                padded[dst_start..dst_end].copy_from_slice(&self.pixel_buf[src_start..src_end]);
            }
            cairo::ImageSurface::create_for_data(
                padded,
                cairo::Format::ARgb32,
                self.display_width as i32,
                HISTORY_LINES as i32,
                stride,
            )
        };

        let Ok(surface) = surface else { return };

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

        // Scale and translate the surface so only the visible portion fills
        // the output area.
        let _ = cr.save();

        // Y scale: stretch history lines to fill output height.
        let y_scale = f64::from(height) / HISTORY_LINES as f64;

        // X scale: the visible fraction of the surface width maps to output width.
        let x_scale = if visible_width_frac > 0.0 {
            f64::from(width) / (self.display_width as f64 * visible_width_frac)
        } else {
            f64::from(width) / self.display_width as f64
        };

        cr.scale(x_scale, y_scale);

        // Offset the surface so the visible start aligns with x=0.
        let src_offset_x = -(visible_start_frac * self.display_width as f64);
        let _ = cr.set_source_surface(&surface, src_offset_x, 0.0);

        // Use NEAREST filtering for crisp bin boundaries.
        cr.source().set_filter(cairo::Filter::Nearest);

        let _ = cr.paint();
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

    /// Export the waterfall display to a PNG file.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    pub fn export_png(&self, path: &std::path::Path) -> Result<(), String> {
        if self.display_width == 0 {
            return Err("no waterfall data".to_string());
        }
        let stride = cairo::Format::ARgb32
            .stride_for_width(self.display_width as u32)
            .map_err(|e| format!("stride: {e}"))?;
        let expected = (self.display_width * 4) as i32;
        let buf = if stride == expected {
            self.pixel_buf.clone()
        } else {
            let mut padded = vec![0u8; stride as usize * HISTORY_LINES];
            for row in 0..HISTORY_LINES {
                let src = row * self.display_width * 4;
                let dst = row * stride as usize;
                padded[dst..dst + self.display_width * 4]
                    .copy_from_slice(&self.pixel_buf[src..src + self.display_width * 4]);
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
    pub fn resize(&mut self, new_width: usize) {
        let capped_width = supported_texture_width(new_width);
        self.display_width = capped_width;
        self.pixel_buf = vec![0u8; capped_width * HISTORY_LINES * 4];
        self.row_buffer = vec![0u8; capped_width];
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
}
