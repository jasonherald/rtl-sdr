//! FFT spectrum plot renderer using Cairo.
//!
//! Draws the power spectrum as a filled area with a line trace on top,
//! plus horizontal dB grid lines with labels and vertical frequency grid
//! lines with frequency labels. Supports zoom via display range parameters.
//! Renders directly via Cairo draw calls — no OpenGL, no pixel buffers.

use gtk4::cairo;

use super::frequency_axis;

/// Maximum bins for display rendering.
/// FFT data wider than this is max-pooled down before drawing.
const MAX_DISPLAY_BINS: usize = 4096;

/// Number of horizontal dB grid lines.
const DB_GRID_LINE_COUNT: usize = 8;

/// Number of vertical frequency grid lines.
const FREQ_GRID_LINE_COUNT: usize = 10;

/// Font size for axis labels in Cairo units.
const LABEL_FONT_SIZE: f64 = 10.0;

/// Label color — light gray, semi-transparent.
const LABEL_COLOR: [f64; 4] = [0.6, 0.6, 0.6, 0.8];

/// Top margin in pixels reserved for frequency labels.
const FREQ_LABEL_TOP_MARGIN: f64 = 14.0;

// Colors (RGBA, 0.0..1.0)
/// Spectrum trace line color — accent blue.
const TRACE_COLOR: [f64; 4] = [0.3, 0.7, 1.0, 1.0];
/// Spectrum fill color — semi-transparent blue.
const FILL_COLOR: [f64; 4] = [0.2, 0.4, 0.8, 0.35];
/// Grid line color — dim gray.
const GRID_COLOR: [f64; 4] = [0.4, 0.4, 0.4, 0.5];
/// Background clear color — near-black.
const BACKGROUND_COLOR: [f64; 4] = [0.08, 0.08, 0.10, 1.0];

/// Downsample FFT data by max-pooling bins to fit display width.
///
/// When the input has more bins than `MAX_DISPLAY_BINS`, groups of bins
/// are reduced to a single bin by taking the maximum dB value in each group.
/// This preserves signal peaks. Returns a slice of the downsampled buffer,
/// or the original data if no downsampling is needed.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn downsample_fft<'a>(data: &'a [f32], buf: &'a mut Vec<f32>) -> &'a [f32] {
    if data.len() <= MAX_DISPLAY_BINS {
        return data;
    }
    let out_bins = MAX_DISPLAY_BINS;
    buf.resize(out_bins, f32::NEG_INFINITY);
    let ratio = data.len() as f32 / out_bins as f32;
    for (i, out) in buf.iter_mut().enumerate().take(out_bins) {
        let start = (i as f32 * ratio) as usize;
        let end = (((i + 1) as f32) * ratio) as usize;
        let end = end.min(data.len());
        let mut max_val = f32::NEG_INFINITY;
        for &v in &data[start..end] {
            if v > max_val {
                max_val = v;
            }
        }
        *out = max_val;
    }
    buf
}

/// Cairo renderer for the FFT power spectrum plot.
///
/// Renders a filled area under the spectrum curve, a line trace on top,
/// and grid lines for dB and frequency reference.
pub struct FftPlotRenderer {
    /// Pre-allocated buffer for downsampling large FFT data.
    downsample_buf: Vec<f32>,
}

impl Default for FftPlotRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl FftPlotRenderer {
    /// Create a new FFT plot renderer.
    pub fn new() -> Self {
        Self {
            downsample_buf: Vec::with_capacity(MAX_DISPLAY_BINS),
        }
    }

    /// Render the FFT spectrum plot using Cairo.
    ///
    /// # Arguments
    ///
    /// * `cr` — The Cairo drawing context.
    /// * `fft_data` — Power spectrum values in dB (one per frequency bin, full bandwidth).
    /// * `width` — Viewport width in pixels.
    /// * `height` — Viewport height in pixels.
    /// * `min_db` — Bottom of the display range in dB.
    /// * `max_db` — Top of the display range in dB.
    /// * `fill_enabled` — Whether to draw the filled area under the trace.
    /// * `display_start_hz` — Left edge of the visible frequency range (relative to center).
    /// * `display_end_hz` — Right edge of the visible frequency range (relative to center).
    /// * `full_bandwidth` — Total FFT bandwidth in Hz (unzoomed span).
    /// * `center_freq_hz` — Tuner center frequency in Hz (for absolute frequency labels).
    #[allow(clippy::cast_precision_loss, clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        cr: &cairo::Context,
        fft_data: &[f32],
        width: i32,
        height: i32,
        min_db: f32,
        max_db: f32,
        fill_enabled: bool,
        display_start_hz: f64,
        display_end_hz: f64,
        full_bandwidth: f64,
        center_freq_hz: f64,
    ) {
        if fft_data.is_empty() || width <= 0 || height <= 0 {
            return;
        }

        let db_range = max_db - min_db;
        if db_range <= 0.0 {
            return;
        }

        let w = f64::from(width);
        let h = f64::from(height);

        // Downsample large FFTs to limit draw call count.
        let mut ds_buf = std::mem::take(&mut self.downsample_buf);
        let display_data = downsample_fft(fft_data, &mut ds_buf);

        // Background.
        cr.set_source_rgba(
            BACKGROUND_COLOR[0],
            BACKGROUND_COLOR[1],
            BACKGROUND_COLOR[2],
            BACKGROUND_COLOR[3],
        );
        let _ = cr.paint();

        // Frequency-aware grid lines with labels.
        Self::draw_grid(
            cr,
            w,
            h,
            min_db,
            max_db,
            display_start_hz,
            display_end_hz,
            center_freq_hz,
        );

        // Build the trace path once, reuse for fill and stroke.
        // Uses frequency-to-pixel mapping for zoom support.
        Self::build_trace_path(
            cr,
            display_data,
            w,
            h,
            db_range,
            min_db,
            display_start_hz,
            display_end_hz,
            full_bandwidth,
        );

        if fill_enabled {
            // Close along bottom edge and fill.
            cr.line_to(w, h);
            cr.line_to(0.0, h);
            cr.close_path();
            cr.set_source_rgba(FILL_COLOR[0], FILL_COLOR[1], FILL_COLOR[2], FILL_COLOR[3]);
            let _ = cr.fill_preserve();
        }

        // Stroke the trace line on top.
        cr.set_source_rgba(
            TRACE_COLOR[0],
            TRACE_COLOR[1],
            TRACE_COLOR[2],
            TRACE_COLOR[3],
        );
        cr.set_line_width(1.0);
        let _ = cr.stroke();

        // Return the downsample buffer to self for reuse next frame.
        let _ = display_data;
        self.downsample_buf = ds_buf;
    }

    /// Draw horizontal dB grid lines with labels and vertical frequency grid
    /// lines with frequency labels.
    #[allow(clippy::cast_precision_loss)]
    #[allow(clippy::too_many_arguments)]
    fn draw_grid(
        cr: &cairo::Context,
        w: f64,
        h: f64,
        min_db: f32,
        max_db: f32,
        display_start_hz: f64,
        display_end_hz: f64,
        center_freq_hz: f64,
    ) {
        let display_span = display_end_hz - display_start_hz;

        // --- Grid lines (drawn first, behind labels) ---
        cr.set_source_rgba(GRID_COLOR[0], GRID_COLOR[1], GRID_COLOR[2], GRID_COLOR[3]);
        cr.set_line_width(1.0);

        let db_range = f64::from(max_db - min_db);

        // Horizontal dB grid lines.
        for i in 0..=DB_GRID_LINE_COUNT {
            let frac = i as f64 / DB_GRID_LINE_COUNT as f64;
            let y = (h * frac).floor() + 0.5;
            cr.move_to(0.0, y);
            cr.line_to(w, y);
        }

        // Vertical frequency grid lines at computed positions.
        // Use absolute frequencies (center + offset) for labels.
        let abs_start = center_freq_hz + display_start_hz;
        let abs_end = center_freq_hz + display_end_hz;
        let grid_lines = if display_span > 0.0 {
            frequency_axis::compute_grid_lines(abs_start, abs_end, FREQ_GRID_LINE_COUNT)
        } else {
            Vec::new()
        };

        for &(freq_hz, _) in &grid_lines {
            let frac = (freq_hz - abs_start) / display_span;
            let x = (w * frac).floor() + 0.5;
            cr.move_to(x, 0.0);
            cr.line_to(x, h);
        }

        let _ = cr.stroke();

        // --- Labels (drawn on top of grid lines) ---
        cr.set_font_size(LABEL_FONT_SIZE);
        cr.set_source_rgba(
            LABEL_COLOR[0],
            LABEL_COLOR[1],
            LABEL_COLOR[2],
            LABEL_COLOR[3],
        );

        // Frequency labels at the top of each vertical grid line.
        for (freq_hz, label) in &grid_lines {
            let frac = (freq_hz - abs_start) / display_span;
            let x = w * frac;
            cr.move_to(x + 2.0, FREQ_LABEL_TOP_MARGIN - 2.0);
            cr.show_text(label).ok();
        }

        // dB labels at each horizontal grid line.
        if db_range > 0.0 {
            for i in 0..=DB_GRID_LINE_COUNT {
                let frac = i as f64 / DB_GRID_LINE_COUNT as f64;
                let y = h * frac;
                // frac 0 = top = max_db, frac 1 = bottom = min_db.
                let db_val = f64::from(max_db) - frac * db_range;
                let label = format!("{db_val:.0} dB");
                cr.move_to(2.0, y - 2.0);
                cr.show_text(&label).ok();
            }
        }
    }

    /// Build the spectrum trace path on the Cairo context (no fill or stroke).
    ///
    /// Maps each FFT bin to a frequency in Hz, then to a pixel X coordinate
    /// using the current display range. When zoomed in, bins outside the
    /// visible range map to offscreen positions and Cairo clips them.
    #[allow(clippy::cast_precision_loss, clippy::too_many_arguments)]
    fn build_trace_path(
        cr: &cairo::Context,
        fft_data: &[f32],
        w: f64,
        h: f64,
        db_range: f32,
        min_db: f32,
        display_start_hz: f64,
        display_end_hz: f64,
        full_bandwidth: f64,
    ) {
        let bin_count = fft_data.len();
        if bin_count == 0 {
            return;
        }

        let db_range_f64 = f64::from(db_range);
        let min_db_f64 = f64::from(min_db);
        let display_span = display_end_hz - display_start_hz;

        // If full_bandwidth is not set (0), fall back to the display span
        // (no zoom effect).
        let effective_full_bw = if full_bandwidth > 0.0 {
            full_bandwidth
        } else {
            display_span
        };

        for (i, &db) in fft_data.iter().enumerate() {
            // Map bin index to frequency: after fftshift, bin 0 = -full_bw/2,
            // bin N-1 = +full_bw/2.
            let bin_freq = -effective_full_bw / 2.0
                + (i as f64 / (bin_count - 1).max(1) as f64) * effective_full_bw;

            // Map frequency to pixel X within the current display range.
            let x = if display_span > 0.0 {
                w * (bin_freq - display_start_hz) / display_span
            } else {
                w * i as f64 / (bin_count - 1).max(1) as f64
            };

            let y = h * (1.0 - ((f64::from(db) - min_db_f64) / db_range_f64).clamp(0.0, 1.0));
            if i == 0 {
                cr.move_to(x, y);
            } else {
                cr.line_to(x, y);
            }
        }
    }
}
