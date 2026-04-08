//! Signal strength history graph — a time-series line plot showing signal
//! level (dB) over the last ~60 seconds, rendered via Cairo.

use gtk4::cairo;

/// Number of samples in the history buffer (~60 seconds at ~10 updates/sec).
const HISTORY_SIZE: usize = 600;

/// dB step between horizontal grid lines.
const DB_GRID_STEP: f32 = 20.0;

// Colors (RGBA, 0.0..1.0)
/// Trace line color — green.
const TRACE_COLOR: [f64; 4] = [0.3, 0.85, 0.4, 1.0];
/// Grid line color — dim gray.
const GRID_COLOR: [f64; 4] = [0.4, 0.4, 0.4, 0.3];
/// Background clear color — near-black.
const BACKGROUND_COLOR: [f64; 4] = [0.08, 0.08, 0.10, 1.0];

/// Cairo renderer for the signal strength history plot.
///
/// Maintains a circular buffer of dB samples and draws them as a line strip
/// with horizontal grid lines for dB reference.
pub struct SignalHistoryRenderer {
    samples: Vec<f32>,
    write_pos: usize,
    count: usize,
}

impl Default for SignalHistoryRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalHistoryRenderer {
    /// Create a new signal history renderer.
    pub fn new() -> Self {
        Self {
            samples: vec![f32::NEG_INFINITY; HISTORY_SIZE],
            write_pos: 0,
            count: 0,
        }
    }

    /// Add a signal level sample (in dB) to the circular buffer.
    pub fn push(&mut self, db: f32) {
        self.samples[self.write_pos] = db;
        self.write_pos = (self.write_pos + 1) % HISTORY_SIZE;
        if self.count < HISTORY_SIZE {
            self.count += 1;
        }
    }

    /// Render the signal history line plot using Cairo.
    ///
    /// # Arguments
    ///
    /// * `cr` — The Cairo drawing context.
    /// * `width` — Viewport width in pixels.
    /// * `height` — Viewport height in pixels.
    /// * `min_db` — Bottom of the display range in dB.
    /// * `max_db` — Top of the display range in dB.
    #[allow(clippy::cast_precision_loss)]
    pub fn render(&self, cr: &cairo::Context, width: i32, height: i32, min_db: f32, max_db: f32) {
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

        // Grid lines.
        Self::draw_grid(cr, w, h, db_range, min_db);

        // Signal trace.
        self.draw_trace(cr, w, h, db_range, min_db);
    }

    /// Draw horizontal dB grid lines at round 20 dB intervals.
    #[allow(clippy::cast_precision_loss)]
    fn draw_grid(cr: &cairo::Context, w: f64, h: f64, db_range: f32, min_db: f32) {
        cr.set_source_rgba(GRID_COLOR[0], GRID_COLOR[1], GRID_COLOR[2], GRID_COLOR[3]);
        cr.set_line_width(1.0);

        if db_range > 0.0 {
            let first = (min_db / DB_GRID_STEP).ceil() * DB_GRID_STEP;
            let mut db = first;
            while db < min_db + db_range {
                let frac = f64::from((db - min_db) / db_range);
                // y=0 is top (max_db), y=h is bottom (min_db).
                let y = (h * (1.0 - frac)).floor() + 0.5;
                cr.move_to(0.0, y);
                cr.line_to(w, y);
                db += DB_GRID_STEP;
            }
        }

        let _ = cr.stroke();
    }

    /// Draw the signal level trace as a line strip.
    #[allow(clippy::cast_precision_loss, clippy::many_single_char_names)]
    fn draw_trace(&self, cr: &cairo::Context, w: f64, h: f64, db_range: f32, min_db: f32) {
        if self.count == 0 {
            return;
        }

        let n = self.count;
        let db_range_f64 = f64::from(db_range);
        let min_db_f64 = f64::from(min_db);

        // Start from the oldest sample in the circular buffer.
        let start = if self.count < HISTORY_SIZE {
            0
        } else {
            self.write_pos
        };

        for i in 0..n {
            let idx = (start + i) % HISTORY_SIZE;
            let db = f64::from(self.samples[idx]);

            // X axis: time, oldest on left, newest on right.
            let x = w * i as f64 / (n - 1).max(1) as f64;
            // Y axis: dB mapped to pixel space (top = max_db, bottom = min_db).
            let y = h * (1.0 - ((db - min_db_f64) / db_range_f64).clamp(0.0, 1.0));

            if i == 0 {
                cr.move_to(x, y);
            } else {
                cr.line_to(x, y);
            }
        }

        cr.set_source_rgba(
            TRACE_COLOR[0],
            TRACE_COLOR[1],
            TRACE_COLOR[2],
            TRACE_COLOR[3],
        );
        cr.set_line_width(1.0);
        let _ = cr.stroke();
    }
}

#[cfg(test)]
mod tests {
    /// Compile-time validation that signal history constants are consistent.
    const _: () = {
        assert!(super::HISTORY_SIZE > 0);
    };
}
