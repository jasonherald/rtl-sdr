//! WEFAX line-sync state machine: start tone → phasing lock → imaging →
//! stop tone. Gates line emission until the left edge is locked and
//! auto-segments charts on the stop tone.

use super::assembly::LineAssembler;
use super::phasing::PhasingTracker;
use super::tones::ToneDetector;
use super::{START_TONE_HZ, STOP_TONE_HZ, WefaxState};

/// Brightness (0-255) at or above which a pixel counts as part of the
/// phasing pulse. A genuine phasing line is a narrow bright pulse against a
/// near-black background.
const PHASING_PULSE_BRIGHTNESS: u8 = 180;
/// A phasing line is only observed when its bright-pixel count is nonzero
/// but stays below this fraction of the line. A blank/silent line (no bright
/// pixels) reports `pulse_column = 0.0`, and a saturated all-white line has
/// its leading bright edge at column 0 — either would drag the median column
/// offset and slant lock toward the left edge, so both are skipped.
const PHASING_MAX_BRIGHT_FRACTION: f64 = 0.5;

/// Consecutive phasing-pulse lines that, seen while Idle, enter Phasing
/// without an audio start tone. Real WEFAX carries no 300 Hz audio start
/// tone — the "300 Hz start" is the black/white keying rate of the
/// subcarrier — so the sync machine must recognize the phasing interval
/// directly from line content. A short run keeps false starts unlikely while
/// still catching the interval near its beginning.
const PHASING_ENTRY_LINES: usize = 4;

/// Maximum physically-plausible phasing slant (pulse-column drift per line).
/// A real TX/RX sample-clock mismatch is far below one column per line; the
/// least-squares fit over noisy real pulse columns can instead produce a
/// slope of hundreds of columns/line. Clamping the slant to this bound keeps
/// the reprogrammed samples-per-line within ≈1% of `sr/2`, so a garbage
/// estimate can never shear the chart into incoherence.
pub(crate) const SLANT_MAX_COLS_PER_LINE: f64 = 2.0;

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
            let spl = clamped_samples_per_line(self.sr, self.phasing.slant_columns_per_line());
            assembler.set_samples_per_line(spl);
            self.state = WefaxState::Imaging;
        }
    }
}

/// Corrected samples-per-line from the phasing slant, with the slant clamped
/// to `±SLANT_MAX_COLS_PER_LINE` first. On real captures the leading-edge
/// pulse-column measurement is jumpy, so the least-squares slope can come out
/// hundreds of columns/line — which, applied unclamped, reprograms
/// samples-per-line far from `sr/2` and shears the image into incoherence.
/// Clamping bounds the correction to ≈1% of `sr/2`, keeping the image
/// coherent no matter how noisy the lock is.
///
/// `PIXELS_PER_LINE` (1809) is far below `f64`'s exact-integer range, so the
/// cast never loses precision in practice.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn clamped_samples_per_line(sr: f64, slant_cols_per_line: f64) -> f64 {
    let slant = slant_cols_per_line.clamp(-SLANT_MAX_COLS_PER_LINE, SLANT_MAX_COLS_PER_LINE);
    let base = sr / 2.0;
    base - slant * (base / super::PIXELS_PER_LINE as f64)
}

/// True when a line looks like a genuine phasing pulse: at least one bright
/// pixel, but fewer than half the line bright (rejecting a flooded/white
/// line whose leading edge would falsely lock the offset at column 0).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn is_phasing_pulse(line: &[u8; super::PIXELS_PER_LINE]) -> bool {
    let bright = line
        .iter()
        .filter(|&&v| v >= PHASING_PULSE_BRIGHTNESS)
        .count();
    let max_bright = (super::PIXELS_PER_LINE as f64 * PHASING_MAX_BRIGHT_FRACTION) as usize;
    bright > 0 && bright < max_bright
}
