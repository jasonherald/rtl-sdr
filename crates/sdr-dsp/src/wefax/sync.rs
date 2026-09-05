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
            WefaxState::Idle => LineDisposition::Drop,
        }
    }

    /// Feed one phasing line to the tracker (only if it carries a genuine
    /// bright pulse) and, once locked, program the assembler and advance to
    /// [`WefaxState::Imaging`].
    ///
    /// `PIXELS_PER_LINE` (1809) is far below `f64`'s exact-integer range, so
    /// the samples-per-line cast never loses precision in practice.
    #[allow(clippy::cast_precision_loss)]
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
            let spl = self.sr / 2.0
                - self.phasing.slant_columns_per_line()
                    * (self.sr / 2.0 / super::PIXELS_PER_LINE as f64);
            assembler.set_samples_per_line(spl);
            self.state = WefaxState::Imaging;
        }
    }
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
