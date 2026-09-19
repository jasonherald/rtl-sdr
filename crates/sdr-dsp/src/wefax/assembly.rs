//! Pixel clock + scanline accumulation. Maps each brightness sample to a
//! column via a fractional sample-per-line phase, averaging the samples
//! that land in each column, and emits a full line every period.

use super::PIXELS_PER_LINE;

pub(crate) struct LineAssembler {
    samples_per_line: f64,
    pos: f64,           // fractional sample position within the current line
    column_offset: i32, // phasing left-edge shift, in columns
    bins: [f64; PIXELS_PER_LINE],
    counts: [u32; PIXELS_PER_LINE],
}

impl LineAssembler {
    pub(crate) fn new(samples_per_line: f64) -> Self {
        Self {
            samples_per_line,
            pos: 0.0,
            column_offset: 0,
            bins: [0.0; PIXELS_PER_LINE],
            counts: [0; PIXELS_PER_LINE],
        }
    }

    pub(crate) fn set_column_offset(&mut self, cols: i32) {
        self.column_offset = cols;
    }

    pub(crate) fn set_samples_per_line(&mut self, spl: f64) {
        if spl > 1.0 {
            self.samples_per_line = spl;
        }
    }

    /// Accumulate one brightness sample; return a finished line when the
    /// per-line sample budget is reached.
    ///
    /// `PIXELS_PER_LINE` (1809) is far below both the `i32` and `f64`
    /// exact-integer ranges, so the column-index casts below never
    /// truncate, wrap, or lose precision in practice.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap
    )]
    pub(crate) fn push(&mut self, brightness: u8) -> Option<[u8; PIXELS_PER_LINE]> {
        let frac = self.pos / self.samples_per_line; // 0..1 across the line
        let raw_col = (frac * PIXELS_PER_LINE as f64) as i32 + self.column_offset;
        let col = raw_col.rem_euclid(PIXELS_PER_LINE as i32) as usize;
        self.bins[col] += f64::from(brightness);
        self.counts[col] += 1;
        self.pos += 1.0;
        if self.pos >= self.samples_per_line {
            self.pos -= self.samples_per_line;
            Some(self.finish_line())
        } else {
            None
        }
    }

    /// Average each column's accumulated bin, reset it, and return the
    /// finished scanline. Column averages are always in `0.0..=255.0`
    /// (they average `u8` brightness samples), so the truncation/sign-loss
    /// casts below never fire in practice.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn finish_line(&mut self) -> [u8; PIXELS_PER_LINE] {
        let mut line = [0u8; PIXELS_PER_LINE];
        for ((pixel, &bin), &count) in line.iter_mut().zip(&self.bins).zip(&self.counts) {
            if count > 0 {
                *pixel = (bin / f64::from(count)).round() as u8;
            }
        }
        self.bins = [0.0; PIXELS_PER_LINE];
        self.counts = [0; PIXELS_PER_LINE];
        line
    }
}
