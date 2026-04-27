//! FFT spectrum plot renderer using Cairo.
//!
//! Draws the power spectrum as a filled area with a line trace on top,
//! plus horizontal dB grid lines with labels and vertical frequency grid
//! lines with frequency labels. Supports zoom via display range parameters.
//! Renders directly via Cairo draw calls — no OpenGL, no pixel buffers.

use gtk4::cairo;

use super::ScannerAxisLock;
use super::frequency_axis;

/// Highlight-band overlay color for the active scanner channel
/// (when scanner-axis lock is engaged). Faint accent blue so
/// the band reads as a "this is what we're sampling" affordance
/// without obscuring the trace beneath it. Per issue #516.
const SCANNER_HIGHLIGHT_COLOR: [f64; 4] = [0.3, 0.7, 1.0, 0.18];

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

/// Map an FFT bin index to a pixel X within a wide locked
/// X axis. Pure projection helper extracted for unit testing —
/// the rendering call sites in [`FftPlotRenderer::render_locked`]
/// and the waterfall sparse-fill use the same math, and
/// projection drift between them would manifest as a
/// half-pixel-misaligned highlight band that's hard to spot
/// visually. Per issue #516.
///
/// # Arguments
///
/// * `bin_index` — FFT bin number, 0..n_bins-1. After fftshift,
///   bin 0 is the most negative offset from the active channel
///   centre and bin n_bins-1 is the most positive.
/// * `n_bins` — total bin count in the FFT frame.
/// * `full_bw_hz` — span of the FFT (effective sample rate
///   after decimation). Bin 0 maps to `-full_bw/2` relative to
///   the active channel; bin n_bins-1 maps to `+full_bw/2`.
/// * `active_channel_hz` — absolute centre frequency of the
///   channel the scanner is sampling.
/// * `axis_min_hz`, `axis_max_hz` — the locked X axis bounds.
/// * `w` — pixel width of the drawing area.
///
/// Returns the pixel X coordinate (may fall outside `0..w` if
/// the bin's absolute frequency is outside the locked range —
/// caller is responsible for clipping).
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub(super) fn bin_to_locked_x(
    bin_index: usize,
    n_bins: usize,
    full_bw_hz: f64,
    active_channel_hz: f64,
    axis_min_hz: f64,
    axis_max_hz: f64,
    w: f64,
) -> f64 {
    let n = (n_bins.saturating_sub(1)).max(1) as f64;
    let bin_freq_relative = -full_bw_hz / 2.0 + (bin_index as f64 / n) * full_bw_hz;
    let bin_freq_abs = active_channel_hz + bin_freq_relative;
    let span = axis_max_hz - axis_min_hz;
    if span <= 0.0 {
        return 0.0;
    }
    w * (bin_freq_abs - axis_min_hz) / span
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

    /// Render the FFT plot with the scanner X-axis lock engaged.
    ///
    /// Sibling of [`Self::render`] for scanner mode. Same Cairo
    /// commands (background → grid → trace → fill → stroke)
    /// but the projection math uses absolute frequencies
    /// against the locked range instead of center-relative
    /// offsets. The narrow FFT bins land in the active
    /// channel's slice of the wide canvas; the rest of the X
    /// axis stays at the background colour (no trace), which
    /// gives the issue #516's "active channel's slice has
    /// signal, the rest is empty" appearance.
    ///
    /// Renders a faint highlight band over the active channel's
    /// bandwidth so the user can read "this is what we're
    /// sampling right now" at a glance — same role the existing
    /// VFO overlay plays in the unlocked case.
    ///
    /// # Arguments
    ///
    /// * `cr`, `fft_data`, `width`, `height`, `min_db`, `max_db`,
    ///   `fill_enabled` — same as [`Self::render`].
    /// * `full_bandwidth` — span of the FFT frame (post-
    ///   decimation effective sample rate). Used to map bins to
    ///   relative offsets from the active channel's centre.
    /// * `lock` — current scanner-axis lock state. The render
    ///   uses `lock.min_hz`/`lock.max_hz` for the X axis,
    ///   `lock.active_channel_hz`/`bw_hz` for the trace
    ///   placement and the highlight band.
    ///
    /// Per issue #516.
    #[allow(clippy::cast_precision_loss, clippy::too_many_arguments)]
    pub fn render_locked(
        &mut self,
        cr: &cairo::Context,
        fft_data: &[f32],
        width: i32,
        height: i32,
        min_db: f32,
        max_db: f32,
        fill_enabled: bool,
        full_bandwidth: f64,
        lock: &ScannerAxisLock,
    ) {
        if width <= 0 || height <= 0 {
            return;
        }
        let db_range = max_db - min_db;
        if db_range <= 0.0 {
            return;
        }
        let w = f64::from(width);
        let h = f64::from(height);

        // Background.
        cr.set_source_rgba(
            BACKGROUND_COLOR[0],
            BACKGROUND_COLOR[1],
            BACKGROUND_COLOR[2],
            BACKGROUND_COLOR[3],
        );
        let _ = cr.paint();

        // Grid + labels span the locked range absolutely. We
        // pass `display_start_hz = 0` and `display_end_hz =
        // span` plus `center_freq_hz = lock.min_hz` to
        // `draw_grid` — the existing helper computes
        // `abs_start = center + display_start = lock.min_hz`
        // and `abs_end = center + display_end = lock.max_hz`,
        // which is exactly what we want.
        let span = lock.max_hz - lock.min_hz;
        Self::draw_grid(cr, w, h, min_db, max_db, 0.0, span, lock.min_hz);

        // Active-channel highlight band — drawn BEFORE the
        // trace so the trace renders on top. Skip when no
        // channel is active yet (lock engaged but first retune
        // hasn't completed) — empty grid only.
        if let (Some(active_hz), Some(active_bw)) =
            (lock.active_channel_hz, lock.active_channel_bw_hz)
            && span > 0.0
        {
            let band_min_x = w * (active_hz - active_bw / 2.0 - lock.min_hz) / span;
            let band_max_x = w * (active_hz + active_bw / 2.0 - lock.min_hz) / span;
            let band_w = (band_max_x - band_min_x).max(1.0);
            cr.set_source_rgba(
                SCANNER_HIGHLIGHT_COLOR[0],
                SCANNER_HIGHLIGHT_COLOR[1],
                SCANNER_HIGHLIGHT_COLOR[2],
                SCANNER_HIGHLIGHT_COLOR[3],
            );
            cr.rectangle(band_min_x, 0.0, band_w, h);
            let _ = cr.fill();
        }

        // Trace — only renders if we know which channel is
        // active. Bins project to absolute X via
        // `bin_to_locked_x`.
        let Some(active_hz) = lock.active_channel_hz else {
            return;
        };
        if fft_data.is_empty() {
            return;
        }

        let mut ds_buf = std::mem::take(&mut self.downsample_buf);
        let display_data = downsample_fft(fft_data, &mut ds_buf);

        let db_range_f64 = f64::from(db_range);
        let min_db_f64 = f64::from(min_db);
        let bin_count = display_data.len();
        for (i, &db) in display_data.iter().enumerate() {
            let x = bin_to_locked_x(
                i,
                bin_count,
                full_bandwidth,
                active_hz,
                lock.min_hz,
                lock.max_hz,
                w,
            );
            let y = h * (1.0 - ((f64::from(db) - min_db_f64) / db_range_f64).clamp(0.0, 1.0));
            if i == 0 {
                cr.move_to(x, y);
            } else {
                cr.line_to(x, y);
            }
        }

        if fill_enabled {
            // Close the fill polygon along the bottom of the
            // active channel's slice — NOT the full canvas
            // width, otherwise the fill would extend across the
            // entire locked range (visually overstating where
            // we're sampling).
            let last_x = bin_to_locked_x(
                bin_count - 1,
                bin_count,
                full_bandwidth,
                active_hz,
                lock.min_hz,
                lock.max_hz,
                w,
            );
            let first_x = bin_to_locked_x(
                0,
                bin_count,
                full_bandwidth,
                active_hz,
                lock.min_hz,
                lock.max_hz,
                w,
            );
            cr.line_to(last_x, h);
            cr.line_to(first_x, h);
            cr.close_path();
            cr.set_source_rgba(FILL_COLOR[0], FILL_COLOR[1], FILL_COLOR[2], FILL_COLOR[3]);
            let _ = cr.fill_preserve();
        }

        cr.set_source_rgba(
            TRACE_COLOR[0],
            TRACE_COLOR[1],
            TRACE_COLOR[2],
            TRACE_COLOR[3],
        );
        cr.set_line_width(1.0);
        let _ = cr.stroke();

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

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    /// Reference scenario: scanner channels span 144–148 MHz
    /// (4 MHz wide), active channel sits at 146 MHz with a
    /// 250 kHz FFT. The active centre should land at exactly
    /// the middle of an 800 px-wide canvas (50% in, 400 px).
    #[test]
    fn bin_to_locked_x_centres_active_channel() {
        let n_bins = 1024;
        let full_bw = 250_000.0;
        let active_hz = 146_000_000.0;
        let axis_min = 144_000_000.0;
        let axis_max = 148_000_000.0;
        let w = 800.0;

        // The mid-bin (n_bins/2) corresponds to bin_freq_relative = 0,
        // i.e. exactly at active_channel_hz. For odd offsets the
        // bin numbering means the closest "centre bin" is
        // n_bins/2; check that one.
        let mid_bin = n_bins / 2;
        let x = bin_to_locked_x(mid_bin, n_bins, full_bw, active_hz, axis_min, axis_max, w);
        // Expected: w * (146M - 144M) / (148M - 144M) = w * 0.5 = 400.0
        // Mid-bin is one bin above true centre because of the
        // (i / (n-1)) projection — small offset, well within
        // 1 px on an 800 px canvas.
        assert!((x - 400.0).abs() < 1.0, "expected ~400.0, got {x}");
    }

    /// Endpoint behaviour: bin 0 must land at the leading edge
    /// of the active channel's slice (active - bw/2), and the
    /// final bin must land at the trailing edge (active + bw/2).
    #[test]
    fn bin_to_locked_x_endpoints_align_with_channel_edges() {
        let n_bins = 1024;
        let full_bw = 250_000.0;
        let active_hz = 146_000_000.0;
        let axis_min = 144_000_000.0;
        let axis_max = 148_000_000.0;
        let w = 800.0;

        // bin 0 → active - bw/2 = 145.875 MHz → x = 800 * (145.875M - 144M) / 4M = 375.0
        let x_first = bin_to_locked_x(0, n_bins, full_bw, active_hz, axis_min, axis_max, w);
        assert!(
            (x_first - 375.0).abs() < 0.1,
            "expected ~375.0 for bin 0, got {x_first}",
        );

        // bin n-1 → active + bw/2 = 146.125 MHz → x = 800 * (146.125M - 144M) / 4M = 425.0
        let x_last = bin_to_locked_x(
            n_bins - 1,
            n_bins,
            full_bw,
            active_hz,
            axis_min,
            axis_max,
            w,
        );
        assert!(
            (x_last - 425.0).abs() < 0.1,
            "expected ~425.0 for last bin, got {x_last}",
        );
    }

    /// Active channel near the lower edge of the locked range.
    /// Verifies the active slice can land at the leading edge
    /// of the canvas (left side) without going negative.
    #[test]
    fn bin_to_locked_x_handles_active_at_lower_edge() {
        let n_bins = 1024;
        let full_bw = 250_000.0;
        let active_hz = 144_125_000.0; // active - bw/2 = axis_min
        let axis_min = 144_000_000.0;
        let axis_max = 148_000_000.0;
        let w = 800.0;

        let x_first = bin_to_locked_x(0, n_bins, full_bw, active_hz, axis_min, axis_max, w);
        assert!(x_first.abs() < 0.1, "expected ~0.0, got {x_first}");
    }

    /// Degenerate input: zero-width axis. Helper must not divide
    /// by zero; returning 0.0 is fine because the caller's
    /// drawing code clips degenerate dimensions anyway.
    #[test]
    fn bin_to_locked_x_zero_span_returns_zero() {
        let x = bin_to_locked_x(
            100,
            1024,
            250_000.0,
            146_000_000.0,
            146_000_000.0,
            146_000_000.0,
            800.0,
        );
        assert_eq!(x, 0.0);
    }

    /// Single-bin input: avoid div-by-zero on `(n-1)` in the
    /// projection. Returns the expected first-bin position
    /// (active - bw/2).
    #[test]
    fn bin_to_locked_x_single_bin_does_not_panic() {
        let x = bin_to_locked_x(
            0,
            1,
            250_000.0,
            146_000_000.0,
            144_000_000.0,
            148_000_000.0,
            800.0,
        );
        // active - bw/2 = 145.875 MHz → x = 800 * (145.875M - 144M) / 4M = 375.0
        assert!((x - 375.0).abs() < 0.1, "expected ~375.0, got {x}");
    }
}
