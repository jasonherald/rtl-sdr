//! Phasing-signal detection. During a chart's phasing preamble every line is
//! mostly black with one white pulse (~5% of the line) at the transmitter's
//! line start; locking the left edge means finding that pulse's column.
//!
//! Real HF phasing is noisy: speckle scatters bright pixels across the line
//! and breaks the pulse up on many lines, so no single line is trustworthy
//! and a run of *consecutive* clean lines may never occur (#921). Instead,
//! each line is classified on its own ([`pulse_column`]) and a
//! [`PhasingVote`] over a sliding window locks once enough lines agree on
//! one column.

use std::collections::VecDeque;

use super::PIXELS_PER_LINE;

/// Brightness (0-255) at or above which a pixel counts as part of the
/// phasing pulse; the pulse is near-white against a near-black line.
pub(crate) const PULSE_BRIGHTNESS_THRESHOLD: u8 = 180;

/// Shortest bright run accepted as a phasing pulse. A real pulse is ~5% of
/// the line (~90 px); static never produces a run anywhere near this long
/// (measured 1-3 px in real inter-chart noise), so the absolute run length —
/// not the fraction of the line that is bright — is what rejects noise.
const PULSE_MIN_RUN_PX: usize = 40;

/// Longest bright run accepted as a phasing pulse. Rejects white chart
/// background and saturated lines, whose runs span hundreds of pixels.
const PULSE_MAX_RUN_PX: usize = 200;

/// Bright-pixel fraction above which a line is treated as content (white
/// background, flooded lines) rather than a mostly-black phasing line. Real
/// weak phasing lines carry up to ~23% bright speckle around the pulse.
const PULSE_MAX_BRIGHT_FRACTION: f64 = 0.30;

/// Lines in the sliding vote window (~8 s at 120 lpm).
const VOTE_WINDOW_LINES: usize = 16;

/// Agreeing pulse lines within the window that lock the left edge. Tuned
/// against every chart boundary in seven overnight NMF Boston captures: it
/// fired on every clearly visible phasing band and never inside chart
/// content (surface/analysis charts and satellite IR imagery included).
pub(crate) const VOTE_LOCK_LINES: usize = 6;

/// Agreeing pulse lines that count as evidence a phasing preamble has begun
/// (drives the Idle → Phasing state shown to the user and the auto-catcher).
pub(crate) const VOTE_EVIDENCE_LINES: usize = 3;

/// Pulse columns within this circular distance (px) of each other agree.
/// Real pulse leading edges jitter by a few px line to line.
const VOTE_COLUMN_TOLERANCE_PX: usize = 12;

/// Leading-edge column of `line`'s phasing pulse, or `None` if the line
/// doesn't look like a phasing line. The pulse is the *longest* bright run —
/// not the first bright pixel, which on a real noisy line is usually a stray
/// speck far to the left of the pulse.
pub(crate) fn pulse_column(line: &[u8; PIXELS_PER_LINE]) -> Option<usize> {
    let (mut bright, mut run, mut best_len, mut best_start) = (0usize, 0usize, 0usize, 0usize);
    for (i, &v) in line.iter().enumerate() {
        if v >= PULSE_BRIGHTNESS_THRESHOLD {
            bright += 1;
            run += 1;
            if run > best_len {
                best_len = run;
                best_start = i + 1 - run;
            }
        } else {
            run = 0;
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let fraction = bright as f64 / PIXELS_PER_LINE as f64;
    let plausible = (PULSE_MIN_RUN_PX..=PULSE_MAX_RUN_PX).contains(&best_len)
        && fraction < PULSE_MAX_BRIGHT_FRACTION;
    plausible.then_some(best_start)
}

/// Circular distance between two columns on the wrap-around line.
fn circular_distance(a: usize, b: usize) -> usize {
    let d = a.abs_diff(b);
    d.min(PIXELS_PER_LINE - d)
}

/// Sliding-window vote over per-line pulse columns.
pub(crate) struct PhasingVote {
    cols: VecDeque<Option<usize>>,
}

impl PhasingVote {
    pub(crate) fn new() -> Self {
        Self {
            cols: VecDeque::with_capacity(VOTE_WINDOW_LINES),
        }
    }

    /// Record one line's classification, dropping the oldest past the window.
    pub(crate) fn push(&mut self, col: Option<usize>) {
        if self.cols.len() == VOTE_WINDOW_LINES {
            self.cols.pop_front();
        }
        self.cols.push_back(col);
    }

    /// Forget the window — its columns are stale once the left edge moves.
    pub(crate) fn clear(&mut self) {
        self.cols.clear();
    }

    /// Largest set of pulse columns in the window agreeing within the
    /// tolerance: its size and its (circular) median column.
    fn best_cluster(&self) -> (usize, Option<usize>) {
        let pulses: Vec<usize> = self.cols.iter().flatten().copied().collect();
        let Some(&center) = pulses.iter().max_by_key(|&&c| {
            pulses
                .iter()
                .filter(|&&o| circular_distance(c, o) <= VOTE_COLUMN_TOLERANCE_PX)
                .count()
        }) else {
            return (0, None);
        };
        // Unwrap each agreeing column relative to `center` so a cluster
        // straddling column 0 still has a well-defined median.
        #[allow(clippy::cast_possible_wrap)]
        let mut offsets: Vec<i64> = pulses
            .iter()
            .filter(|&&o| circular_distance(center, o) <= VOTE_COLUMN_TOLERANCE_PX)
            .map(|&o| {
                let d = o as i64 - center as i64;
                let w = PIXELS_PER_LINE as i64;
                if d > w / 2 {
                    d - w
                } else if d < -w / 2 {
                    d + w
                } else {
                    d
                }
            })
            .collect();
        offsets.sort_unstable();
        let median = offsets[offsets.len() / 2];
        #[allow(
            clippy::cast_possible_wrap,
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation
        )]
        let col = (center as i64 + median).rem_euclid(PIXELS_PER_LINE as i64) as usize;
        (offsets.len(), Some(col))
    }

    /// Agreeing pulse lines in the window (evidence of a preamble).
    pub(crate) fn evidence(&self) -> usize {
        self.best_cluster().0
    }

    /// The pulse column once at least [`VOTE_LOCK_LINES`] lines agree.
    pub(crate) fn lock(&self) -> Option<usize> {
        match self.best_cluster() {
            (n, col) if n >= VOTE_LOCK_LINES => col,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wefax::test_fixtures as fx;

    fn dark_with_run(start: usize, width: usize) -> [u8; PIXELS_PER_LINE] {
        let mut l = [10u8; PIXELS_PER_LINE];
        for px in l.iter_mut().skip(start).take(width) {
            *px = 250;
        }
        l
    }

    #[test]
    fn canonical_pulse_reports_its_leading_edge() {
        assert_eq!(pulse_column(&dark_with_run(300, 90)), Some(300));
    }

    #[test]
    fn stray_specks_left_of_the_pulse_do_not_move_the_column() {
        // The pre-#921 column was the first bright pixel — on real lines a
        // speck near column 0 — which dragged the lock far from the pulse.
        let mut l = dark_with_run(1414, 90);
        for c in [3, 22, 85, 400] {
            l[c] = 250;
        }
        assert_eq!(pulse_column(&l), Some(1414));
    }

    #[test]
    fn noise_and_content_lines_are_not_pulses() {
        let mut scattered = [10u8; PIXELS_PER_LINE];
        for px in scattered.iter_mut().step_by(3) {
            *px = 250; // ~33% bright in 1-px runs, like static
        }
        assert_eq!(pulse_column(&scattered), None, "static");
        assert_eq!(pulse_column(&[10u8; PIXELS_PER_LINE]), None, "blank");
        assert_eq!(pulse_column(&[250u8; PIXELS_PER_LINE]), None, "flooded");
        assert_eq!(pulse_column(&dark_with_run(300, 20)), None, "too short");
        assert_eq!(pulse_column(&dark_with_run(300, 400)), None, "too wide");
    }

    #[test]
    fn run_bounds_are_inclusive() {
        assert!(pulse_column(&dark_with_run(300, PULSE_MIN_RUN_PX)).is_some());
        assert!(pulse_column(&dark_with_run(300, PULSE_MAX_RUN_PX)).is_some());
    }

    fn vote_over(cols: &[Option<usize>]) -> PhasingVote {
        let mut v = PhasingVote::new();
        for &c in cols {
            v.push(c);
        }
        v
    }

    #[test]
    fn vote_locks_once_enough_lines_agree() {
        let five = [
            Some(500),
            None,
            Some(503),
            Some(498),
            None,
            Some(501),
            Some(502),
        ];
        assert_eq!(vote_over(&five).lock(), None, "5 agreeing is not enough");
        let mut six = five.to_vec();
        six.push(Some(499));
        let col = vote_over(&six).lock().expect("6 agreeing locks");
        assert!((498..=503).contains(&col), "median column, got {col}");
    }

    #[test]
    fn vote_ignores_outlier_columns() {
        let cols = [
            Some(500),
            Some(1650),
            Some(501),
            Some(20),
            Some(499),
            Some(500),
            Some(502),
            Some(900),
            Some(501),
        ];
        let col = vote_over(&cols).lock().expect("the 500-cluster locks");
        assert!((499..=502).contains(&col), "got {col}");
    }

    #[test]
    fn vote_handles_a_cluster_straddling_the_line_edge() {
        let cols = [
            Some(1805),
            Some(2),
            Some(1807),
            Some(4),
            Some(1806),
            Some(1),
        ];
        let col = vote_over(&cols).lock().expect("wrap-around cluster locks");
        assert!(circular_distance(col, 0) <= 5, "near the edge, got {col}");
    }

    #[test]
    fn vote_handles_a_straddling_cluster_centered_on_the_high_side() {
        // Mirror of the case above: the cluster centre lands on the
        // high side (the last-listed member of a tie), so low columns
        // unwrap upward (`d < -w/2` → `d + w`).
        let cols = [
            Some(2),
            Some(1805),
            Some(4),
            Some(1807),
            Some(1),
            Some(1806),
        ];
        let col = vote_over(&cols).lock().expect("wrap-around cluster locks");
        assert!(circular_distance(col, 0) <= 5, "near the edge, got {col}");
    }

    #[test]
    fn old_lines_fall_out_of_the_window() {
        let mut v = vote_over(&[Some(500); VOTE_LOCK_LINES]);
        assert!(v.lock().is_some());
        for _ in 0..VOTE_WINDOW_LINES {
            v.push(None);
        }
        assert_eq!(v.lock(), None);
    }

    fn lock_on_fixture(bytes: &[u8]) -> Option<usize> {
        let mut v = PhasingVote::new();
        fx::lines(bytes).iter().find_map(|l| {
            v.push(pulse_column(l));
            v.lock()
        })
    }

    #[test]
    fn real_strong_band_locks_on_the_pulse_column() {
        let col = lock_on_fixture(fx::PHASING_STRONG).expect("locks");
        assert!(
            (1409..=1419).contains(&col),
            "pulse edge ~1411-1417, got {col}"
        );
    }

    #[test]
    fn real_weak_band_locks_on_the_pulse_column() {
        let col = lock_on_fixture(fx::PHASING_WEAK).expect("locks");
        assert!(
            (1588..=1602).contains(&col),
            "pulse edge ~1591-1598, got {col}"
        );
    }
}
