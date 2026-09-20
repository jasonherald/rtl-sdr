//! WEFAX line-sync state machine: start tone → phasing lock → imaging →
//! stop tone. Gates line emission until the left edge is locked and
//! auto-segments charts on the stop tone.

use super::assembly::LineAssembler;
use super::phasing::{PULSE_BRIGHTNESS_THRESHOLD, PhasingTracker};
use super::tones::ToneDetector;
use super::{START_TONE_HZ, STOP_TONE_HZ, WefaxState};

/// A phasing line's bright-pixel count must stay below this fraction of the
/// line. A blank/silent line (no bright pixels) reports `pulse_column = 0.0`,
/// and a saturated all-white line has its leading bright edge at column 0 —
/// either would drag the median column offset and slant lock toward the left
/// edge. Tightened from the original 0.5 to reject a *noise* line: broadband
/// noise leaves ~30-50% of the line bright, well above a real phasing pulse's
/// ~5% narrow white bar, so the old `< 50%` guard let noise masquerade as
/// phasing and lock imaging onto static (#913 no-static hardening).
const PHASING_MAX_BRIGHT_FRACTION: f64 = 0.15;

/// The single longest contiguous bright run must hold at least this fraction
/// of *all* the line's bright pixels for it to count as a phasing pulse. A
/// real pulse is one contiguous white bar (ratio ≈ 1.0); broadband noise
/// scatters its bright pixels into many 1-2 px runs (ratio ≈ 0), so this
/// rejects noise even on the rare block whose bright fraction dips below
/// [`PHASING_MAX_BRIGHT_FRACTION`]. Paired with the fraction guard so both a
/// too-bright *and* a too-scattered line are excluded (#913).
const PHASING_MIN_RUN_CONCENTRATION: f64 = 0.5;

/// Consecutive phasing-pulse lines that, seen while Idle, enter Phasing
/// without an audio start tone. Real WEFAX carries no 300 Hz audio start
/// tone — the "300 Hz start" is the black/white keying rate of the
/// subcarrier — so the sync machine must recognize the phasing interval
/// directly from line content. A short run keeps false starts unlikely while
/// still catching the interval near its beginning.
const PHASING_ENTRY_LINES: usize = 4;

/// Largest slant estimate (pulse-column drift per line) still treated as
/// physically plausible. A real TX/RX sample-clock mismatch is well under one
/// column per line; the least-squares fit over noisy real pulse columns can
/// instead produce a slope of hundreds of columns/line. Any estimate beyond
/// this bound is rejected (slant → 0, nominal `samples_per_line = sr/2`)
/// rather than applied, because even a clamped-but-nonzero drift accumulates
/// into a diagonal shear across the whole chart. A genuinely small slant
/// within the bound is applied as-is and de-slants correctly.
pub(crate) const SLANT_PLAUSIBLE_MAX_COLS_PER_LINE: f64 = 1.0;

pub(crate) enum LineDisposition {
    Drop,
    Emit,
}

pub(crate) struct SyncMachine {
    sr: f64,
    start: ToneDetector,
    stop: ToneDetector,
    phasing: PhasingTracker,
    state: WefaxState,
    chart_complete: bool,
    /// Consecutive phasing-pulse lines seen while Idle, buffered so the run
    /// that triggers entry can be replayed into the fresh tracker (the lock
    /// needs those samples). Cleared whenever a non-pulse line breaks the run.
    idle_pulse_run: Vec<[u8; super::PIXELS_PER_LINE]>,
}

impl SyncMachine {
    pub(crate) fn new(sr: f64) -> Self {
        Self {
            sr,
            start: ToneDetector::new(sr, START_TONE_HZ),
            stop: ToneDetector::new(sr, STOP_TONE_HZ),
            phasing: PhasingTracker::new(),
            state: WefaxState::Idle,
            chart_complete: false,
            idle_pulse_run: Vec::new(),
        }
    }

    pub(crate) fn state(&self) -> WefaxState {
        self.state
    }

    pub(crate) fn take_chart_complete(&mut self) -> bool {
        std::mem::take(&mut self.chart_complete)
    }

    /// Per-sample tone tracking drives Idle→Phasing and *→Stopped.
    pub(crate) fn on_sample(&mut self, s: f64) {
        let start_on = self.start.push(s);
        let stop_on = self.stop.push(s);
        match self.state {
            WefaxState::Idle if start_on => {
                self.state = WefaxState::Phasing;
                self.phasing = PhasingTracker::new();
            }
            WefaxState::Phasing | WefaxState::Imaging if stop_on => {
                self.state = WefaxState::Stopped;
                self.chart_complete = true;
            }
            _ => {}
        }
    }

    /// Per-completed-line handling: build the phasing lock, then emit.
    pub(crate) fn on_line(
        &mut self,
        line: &[u8; super::PIXELS_PER_LINE],
        assembler: &mut LineAssembler,
    ) -> LineDisposition {
        match self.state {
            WefaxState::Phasing => {
                self.observe_phasing_line(line, assembler);
                LineDisposition::Drop
            }
            WefaxState::Imaging => LineDisposition::Emit,
            WefaxState::Stopped => {
                self.state = WefaxState::Idle;
                LineDisposition::Drop
            }
            WefaxState::Idle => {
                self.observe_idle_line(line, assembler);
                LineDisposition::Drop
            }
        }
    }

    /// While Idle, detect the phasing interval directly from line content: a
    /// run of [`PHASING_ENTRY_LINES`] consecutive phasing-pulse lines enters
    /// Phasing without needing an audio start tone (real WEFAX has none). The
    /// buffered run is replayed into the fresh tracker so the subsequent lock
    /// has those lines' samples; imaging then proceeds via the normal
    /// Phasing→Imaging lock.
    fn observe_idle_line(
        &mut self,
        line: &[u8; super::PIXELS_PER_LINE],
        assembler: &mut LineAssembler,
    ) {
        if !is_phasing_pulse(line) {
            self.idle_pulse_run.clear();
            return;
        }
        self.idle_pulse_run.push(*line);
        if self.idle_pulse_run.len() < PHASING_ENTRY_LINES {
            return;
        }
        self.state = WefaxState::Phasing;
        self.phasing = PhasingTracker::new();
        let run = std::mem::take(&mut self.idle_pulse_run);
        for l in &run {
            self.observe_phasing_line(l, assembler);
        }
    }

    /// Feed one phasing line to the tracker (only if it carries a genuine
    /// bright pulse) and, once locked, program the assembler and advance to
    /// [`WefaxState::Imaging`].
    fn observe_phasing_line(
        &mut self,
        line: &[u8; super::PIXELS_PER_LINE],
        assembler: &mut LineAssembler,
    ) {
        if !is_phasing_pulse(line) {
            return; // blank or saturated line — would skew the median/slant lock
        }
        self.phasing.observe_line(line);
        if let Some(off) = self.phasing.column_offset() {
            assembler.set_column_offset(off);
            let spl = corrected_samples_per_line(self.sr, self.phasing.slant_columns_per_line());
            assembler.set_samples_per_line(spl);
            self.state = WefaxState::Imaging;
        }
    }
}

/// Corrected samples-per-line from the phasing slant, with an implausible
/// estimate *rejected* (not clamped) to zero. On real captures the
/// leading-edge pulse-column measurement is jumpy, so the least-squares slope
/// can come out hundreds of columns/line; even clamped to a small nonzero
/// bound it would accumulate into a diagonal shear across the chart. When the
/// raw slant exceeds [`SLANT_PLAUSIBLE_MAX_COLS_PER_LINE`] it is treated as
/// unreliable and dropped to `0.0`, so `samples_per_line == sr/2` and the
/// image stays straight. A genuinely small, plausible slant is applied as-is.
///
/// `PIXELS_PER_LINE` (1809) is far below `f64`'s exact-integer range, so the
/// cast never loses precision in practice.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn corrected_samples_per_line(sr: f64, slant_cols_per_line: f64) -> f64 {
    let slant = if slant_cols_per_line.abs() > SLANT_PLAUSIBLE_MAX_COLS_PER_LINE {
        0.0 // unreliable noisy estimate — nominal rate, no shear
    } else {
        slant_cols_per_line
    };
    let base = sr / 2.0;
    base - slant * (base / super::PIXELS_PER_LINE as f64)
}

/// True when a line looks like a genuine phasing pulse: a *narrow*
/// ([`PHASING_MAX_BRIGHT_FRACTION`]) run of bright pixels *concentrated* in a
/// single contiguous bar ([`PHASING_MIN_RUN_CONCENTRATION`]). The narrowness
/// guard rejects a flooded/white line and a half-bright noise line; the
/// concentration guard rejects a sparse-but-scattered noise line whose bright
/// pixels never form a dominant run. Together they keep the phasing lock from
/// ever engaging on broadband noise (#913 no-static hardening).
#[allow(clippy::cast_precision_loss)]
fn is_phasing_pulse(line: &[u8; super::PIXELS_PER_LINE]) -> bool {
    let mut bright = 0usize;
    let mut longest_run = 0usize;
    let mut run = 0usize;
    for &v in line {
        if v >= PULSE_BRIGHTNESS_THRESHOLD {
            bright += 1;
            run += 1;
            longest_run = longest_run.max(run);
        } else {
            run = 0;
        }
    }
    if bright == 0 {
        return false;
    }
    let fraction = bright as f64 / super::PIXELS_PER_LINE as f64;
    if fraction >= PHASING_MAX_BRIGHT_FRACTION {
        return false;
    }
    longest_run as f64 >= PHASING_MIN_RUN_CONCENTRATION * bright as f64
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::wefax::PIXELS_PER_LINE;

    /// A mostly-dark line with one contiguous bright run — a real pulse.
    fn dark_with_pulse(start: usize, width: usize) -> [u8; PIXELS_PER_LINE] {
        let mut l = [10u8; PIXELS_PER_LINE];
        for px in l.iter_mut().skip(start).take(width) {
            *px = 250;
        }
        l
    }

    /// Bright pixels scattered every `step` columns (1-px runs) — a noise line.
    fn scattered_bright(step: usize) -> [u8; PIXELS_PER_LINE] {
        let mut l = [10u8; PIXELS_PER_LINE];
        for (i, px) in l.iter_mut().enumerate() {
            if i % step == 0 {
                *px = 250;
            }
        }
        l
    }

    #[test]
    fn narrow_contiguous_pulse_is_a_phasing_pulse() {
        // The canonical WEFAX phasing pulse: ~5% of the line, one run.
        assert!(is_phasing_pulse(&dark_with_pulse(300, 90)));
    }

    #[test]
    fn half_bright_scattered_noise_is_not_a_phasing_pulse() {
        // ~33% bright, scattered into 1-px runs. The old `< 50%` guard passed
        // this; it must not lock imaging onto noise.
        assert!(!is_phasing_pulse(&scattered_bright(3)));
    }

    #[test]
    fn sparse_scattered_bright_is_not_a_phasing_pulse() {
        // ~10% bright — below any narrowness fraction — but with no dominant
        // contiguous run, so the concentration gate still rejects it.
        assert!(!is_phasing_pulse(&scattered_bright(10)));
    }

    #[test]
    fn flooded_white_line_is_not_a_phasing_pulse() {
        assert!(!is_phasing_pulse(&[250u8; PIXELS_PER_LINE]));
    }

    #[test]
    fn blank_line_is_not_a_phasing_pulse() {
        assert!(!is_phasing_pulse(&[10u8; PIXELS_PER_LINE]));
    }
}
