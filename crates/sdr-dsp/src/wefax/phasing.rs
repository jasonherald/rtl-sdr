//! Phasing-signal tracker: find the recurring white pulse in mostly-dark
//! phasing lines. Its column is the left-edge (column-0) offset; its drift
//! across lines is the slant (TX/RX clock mismatch).

use super::PIXELS_PER_LINE;

/// Minimum phasing lines before the column offset is considered stable.
const MIN_LINES_FOR_LOCK: usize = 6;

/// Brightness (0-255) at or above which a pixel counts as part of the
/// phasing pulse; the pulse is near-white against a near-black line. Shared
/// with `sync.rs` so the pulse-column search and the line-validity guard use
/// one threshold and one (inclusive) comparison — otherwise a line whose
/// brightest pixel is exactly this value passes the guard but finds no
/// column, skewing the left-edge offset to column 0.
pub(crate) const PULSE_BRIGHTNESS_THRESHOLD: u8 = 180;

pub(crate) struct PhasingTracker {
    pulse_cols: Vec<f64>, // one detected pulse column per observed line
}

// `PIXELS_PER_LINE` (1809) and line-index counts used below are far below
// `f64`'s exact-integer range, and the rounded column offset is always a
// small non-negative pixel index, so these casts never lose precision or
// sign in practice.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
impl PhasingTracker {
    pub(crate) fn new() -> Self {
        Self {
            pulse_cols: Vec::new(),
        }
    }

    pub(crate) fn observe_line(&mut self, line: &[u8; PIXELS_PER_LINE]) {
        self.pulse_cols.push(pulse_column(line));
    }

    /// Median pulse column once enough phasing lines are seen.
    pub(crate) fn column_offset(&self) -> Option<i32> {
        if self.pulse_cols.len() < MIN_LINES_FOR_LOCK {
            return None;
        }
        let mut v = self.pulse_cols.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(v[v.len() / 2].round() as i32)
    }

    /// Least-squares slope of pulse column vs line index (columns/line).
    pub(crate) fn slant_columns_per_line(&self) -> f64 {
        let n = self.pulse_cols.len();
        if n < 2 {
            return 0.0;
        }
        let nf = n as f64;
        let mean_x = (n as f64 - 1.0) / 2.0;
        let mean_y = self.pulse_cols.iter().sum::<f64>() / nf;
        let (mut num, mut den) = (0.0, 0.0);
        for (i, &y) in self.pulse_cols.iter().enumerate() {
            let dx = i as f64 - mean_x;
            num += dx * (y - mean_y);
            den += dx * dx;
        }
        if den.abs() < 1e-9 { 0.0 } else { num / den }
    }
}

/// Leading-edge column of the brightest short run in a line.
///
/// The tracked column feeds the left-edge (column-0) offset, so the
/// *leading* edge of the bright run — not its centroid — is the right
/// reference point: it is the sample where the pulse first turns on,
/// independent of how wide the run happens to be.
#[allow(clippy::cast_precision_loss, clippy::cast_lossless)]
fn pulse_column(line: &[u8; PIXELS_PER_LINE]) -> f64 {
    line.iter()
        .position(|&v| v >= PULSE_BRIGHTNESS_THRESHOLD)
        .map_or(0.0, |c| c as f64)
}
