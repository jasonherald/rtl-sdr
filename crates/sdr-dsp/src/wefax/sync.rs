//! WEFAX line-sync state machine: phasing lock → imaging → chart end.
//! Gates line emission until the left edge is locked and segments the
//! stream into one image per chart.
//!
//! A chart ends on any of: the 450 Hz stop tone; the *next* chart's phasing
//! preamble (after a hold-off); or a sustained loss of fax presence (the
//! silent gap between transmissions). Real overnight HF showed the stop tone
//! alone is too fragile — back-to-back charts piled up into single images
//! (#921).

use super::assembly::LineAssembler;
use super::phasing::{PhasingVote, VOTE_EVIDENCE_LINES, pulse_column};
use super::tones::ToneDetector;
use super::{START_TONE_HZ, STOP_TONE_HZ, WefaxState};

/// Lines after entering Imaging during which a phasing vote can NOT end the
/// chart. The vote locks early in a preamble (~6 pulse lines in), so the
/// rest of that same preamble is emitted as the chart's top rows; the
/// hold-off (60 s at 120 lpm) outlasts any real preamble (~30-35 s measured)
/// so a chart's own preamble tail can't immediately split it.
const IMAGING_RESTART_HOLDOFF_LINES: usize = 120;

/// Continuous seconds of fax absence that end the chart being imaged. Real
/// inter-chart gaps measured 160-250 s; the longest mid-chart fade measured
/// ~50 s. 90 s sits between them with margin on both sides.
const CHART_GAP_SECS: f64 = 90.0;

/// Continuous seconds of fax presence without a phasing lock after which
/// imaging starts anyway, at the current alignment. A missed phasing band
/// (weak signal) must not cost the whole chart — the image is merely not
/// left-edge aligned, as before #921. Also catches a chart already under way
/// when reception starts. A real preamble locks well inside this (~5-12 s).
const UNALIGNED_START_SECS: f64 = 40.0;

pub(crate) enum LineDisposition {
    Drop,
    Emit,
}

pub(crate) struct SyncMachine {
    sr: f64,
    start: ToneDetector,
    stop: ToneDetector,
    vote: PhasingVote,
    state: WefaxState,
    chart_complete: bool,
    /// Lines emitted since entering Imaging (restart hold-off).
    imaging_lines: usize,
    /// Consecutive samples with fax present / absent (presence framing).
    present_samples: u64,
    absent_samples: u64,
}

impl SyncMachine {
    pub(crate) fn new(sr: f64) -> Self {
        Self {
            sr,
            start: ToneDetector::new(sr, START_TONE_HZ),
            stop: ToneDetector::new(sr, STOP_TONE_HZ),
            vote: PhasingVote::new(),
            state: WefaxState::Idle,
            chart_complete: false,
            imaging_lines: 0,
            present_samples: 0,
            absent_samples: 0,
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
            WefaxState::Idle if start_on => self.state = WefaxState::Phasing,
            WefaxState::Phasing | WefaxState::Imaging if stop_on => self.finish_chart(),
            _ => {}
        }
    }

    /// Per-completed-line handling: vote on the phasing pulse, lock, emit.
    pub(crate) fn on_line(
        &mut self,
        line: &[u8; super::PIXELS_PER_LINE],
        assembler: &mut LineAssembler,
    ) -> LineDisposition {
        self.vote.push(pulse_column(line));
        match self.state {
            WefaxState::Idle | WefaxState::Phasing => {
                self.try_lock(assembler);
                LineDisposition::Drop
            }
            WefaxState::Imaging => self.on_imaging_line(),
            WefaxState::Stopped => {
                // Keep the vote window: if the next chart's preamble ended
                // this one, the very next line can lock on it.
                self.state = WefaxState::Idle;
                LineDisposition::Drop
            }
        }
    }

    /// Feed the fax-presence verdict covering the last `samples` samples.
    /// A sustained absence ([`CHART_GAP_SECS`]) ends the chart being imaged;
    /// sustained presence with no lock ([`UNALIGNED_START_SECS`]) starts
    /// imaging unaligned.
    pub(crate) fn on_presence(&mut self, present: bool, samples: usize) {
        let n = samples as u64;
        if present {
            self.present_samples = self.present_samples.saturating_add(n);
            self.absent_samples = 0;
        } else {
            self.absent_samples = self.absent_samples.saturating_add(n);
            self.present_samples = 0;
        }
        match self.state {
            WefaxState::Imaging if self.absent_samples >= self.secs_to_samples(CHART_GAP_SECS) => {
                self.finish_chart();
            }
            WefaxState::Idle | WefaxState::Phasing
                if self.present_samples >= self.secs_to_samples(UNALIGNED_START_SECS) =>
            {
                self.enter_imaging();
            }
            _ => {}
        }
    }

    /// Lock the left edge once the vote agrees; show Phasing on evidence.
    fn try_lock(&mut self, assembler: &mut LineAssembler) {
        if let Some(col) = self.vote.lock() {
            assembler.align_to_pulse(col);
            self.vote.clear(); // columns were measured under the old offset
            self.enter_imaging();
        } else if self.state == WefaxState::Idle && self.vote.evidence() >= VOTE_EVIDENCE_LINES {
            self.state = WefaxState::Phasing;
        }
    }

    /// While Imaging, a phasing vote after the hold-off is the *next*
    /// chart's preamble: finish this chart (dropping the line) so the next
    /// one locks and aligns on it.
    fn on_imaging_line(&mut self) -> LineDisposition {
        self.imaging_lines = self.imaging_lines.saturating_add(1);
        if self.imaging_lines > IMAGING_RESTART_HOLDOFF_LINES && self.vote.lock().is_some() {
            self.finish_chart();
            return LineDisposition::Drop;
        }
        LineDisposition::Emit
    }

    fn enter_imaging(&mut self) {
        self.state = WefaxState::Imaging;
        self.imaging_lines = 0;
        // The chart-gap timer measures absence *during* this chart: an
        // absence that preceded the lock must not cut the new chart short.
        self.absent_samples = 0;
    }

    fn finish_chart(&mut self) {
        self.state = WefaxState::Stopped;
        self.chart_complete = true;
        // The unaligned-start countdown belongs to the next chart: presence
        // counted during this one must not skip the next chart's phasing.
        self.present_samples = 0;
    }

    /// Whole samples in `secs` at this machine's rate (always small and
    /// non-negative, so the cast is exact in practice).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn secs_to_samples(&self, secs: f64) -> u64 {
        (self.sr * secs) as u64
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
mod tests {
    use super::*;
    use crate::wefax::PIXELS_PER_LINE;
    use crate::wefax::test_fixtures as fx;

    const SR: f64 = 24_000.0;

    /// A mostly-dark line with one contiguous ~90 px bright pulse.
    fn pulse_line(start: usize) -> [u8; PIXELS_PER_LINE] {
        let mut l = [10u8; PIXELS_PER_LINE];
        for px in l.iter_mut().skip(start).take(90) {
            *px = 250;
        }
        l
    }

    /// Ordinary mid-grey chart content (no pulse).
    fn content_line() -> [u8; PIXELS_PER_LINE] {
        [128u8; PIXELS_PER_LINE]
    }

    fn machine() -> (SyncMachine, LineAssembler) {
        (SyncMachine::new(SR), LineAssembler::new(SR / 2.0))
    }

    /// Feed lines; return how many were emitted.
    fn feed(
        sm: &mut SyncMachine,
        asm: &mut LineAssembler,
        lines: &[[u8; PIXELS_PER_LINE]],
    ) -> usize {
        lines
            .iter()
            .filter(|l| matches!(sm.on_line(l, asm), LineDisposition::Emit))
            .count()
    }

    /// A machine locked on a synthetic preamble, past the restart hold-off.
    fn imaging_past_holdoff() -> (SyncMachine, LineAssembler) {
        let (mut sm, mut asm) = machine();
        feed(&mut sm, &mut asm, &vec![pulse_line(300); 12]);
        assert_eq!(sm.state(), WefaxState::Imaging, "synthetic preamble locks");
        feed(
            &mut sm,
            &mut asm,
            &vec![content_line(); IMAGING_RESTART_HOLDOFF_LINES + 5],
        );
        assert!(!sm.take_chart_complete());
        (sm, asm)
    }

    /// Feed real rows to a fresh machine; report whether it ever imaged.
    fn locks_from_idle(bytes: &[u8]) -> bool {
        let (mut sm, mut asm) = machine();
        fx::lines(bytes).iter().any(|l| {
            sm.on_line(l, &mut asm);
            sm.state() == WefaxState::Imaging
        })
    }

    #[test]
    fn real_strong_phasing_band_locks() {
        assert!(locks_from_idle(fx::PHASING_STRONG));
    }

    #[test]
    fn real_weak_phasing_band_locks() {
        // Speckle breaks the pulse up on most lines of this band (bright
        // fraction up to ~23%, pulse only ~25-45% of the bright pixels), yet
        // a ~90 px bar at a steady column is plainly present.
        assert!(locks_from_idle(fx::PHASING_WEAK));
    }

    #[test]
    fn real_chart_content_never_locks_from_idle() {
        for (i, bytes) in fx::CONTENT.iter().enumerate() {
            assert!(
                !locks_from_idle(bytes),
                "content fixture {i} falsely locked"
            );
        }
    }

    #[test]
    fn real_chart_content_never_splits_an_imaging_chart() {
        let (mut sm, mut asm) = imaging_past_holdoff();
        for bytes in fx::CONTENT {
            feed(&mut sm, &mut asm, &fx::lines(bytes));
        }
        assert!(
            !sm.take_chart_complete(),
            "real content must not end the chart"
        );
        assert_eq!(sm.state(), WefaxState::Imaging);
    }

    #[test]
    fn evidence_shows_phasing_before_the_lock() {
        let (mut sm, mut asm) = machine();
        feed(&mut sm, &mut asm, &vec![pulse_line(300); 3]);
        assert_eq!(sm.state(), WefaxState::Phasing);
        feed(&mut sm, &mut asm, &vec![pulse_line(300); 3]);
        assert_eq!(sm.state(), WefaxState::Imaging);
    }

    #[test]
    fn next_preamble_after_holdoff_finishes_the_chart_and_relocks() {
        let (mut sm, mut asm) = imaging_past_holdoff();
        let emitted = feed(&mut sm, &mut asm, &vec![pulse_line(700); 6]);
        assert!(
            sm.take_chart_complete(),
            "next chart's preamble finishes this one"
        );
        assert_eq!(emitted, 5, "the finishing line itself is dropped");
        // The retained vote window re-locks on the new preamble at once.
        feed(&mut sm, &mut asm, &vec![pulse_line(700); 2]);
        assert_eq!(
            sm.state(),
            WefaxState::Imaging,
            "next chart locks immediately"
        );
    }

    #[test]
    fn own_preamble_tail_within_holdoff_does_not_split() {
        let (mut sm, mut asm) = machine();
        feed(&mut sm, &mut asm, &vec![pulse_line(300); 6]);
        assert_eq!(sm.state(), WefaxState::Imaging);
        // The rest of a long (~50 s) preamble keeps coming after the lock.
        feed(&mut sm, &mut asm, &vec![pulse_line(300); 100]);
        assert!(
            !sm.take_chart_complete(),
            "own preamble tail must not split the chart"
        );
        assert_eq!(sm.state(), WefaxState::Imaging);
    }

    /// Present/absent in 1 s steps.
    fn presence_for(sm: &mut SyncMachine, present: bool, secs: usize) {
        for _ in 0..secs {
            sm.on_presence(present, SR as usize);
        }
    }

    #[test]
    fn sustained_absence_ends_the_chart() {
        let (mut sm, _asm) = imaging_past_holdoff();
        presence_for(&mut sm, false, CHART_GAP_SECS as usize - 1);
        assert!(!sm.take_chart_complete(), "shorter than the gap threshold");
        presence_for(&mut sm, false, 1);
        assert!(sm.take_chart_complete(), "a full gap ends the chart");
        assert_eq!(sm.state(), WefaxState::Stopped);
    }

    #[test]
    fn a_fade_shorter_than_the_gap_does_not_end_the_chart() {
        let (mut sm, _asm) = imaging_past_holdoff();
        for _ in 0..5 {
            presence_for(&mut sm, false, 60); // long fade…
            presence_for(&mut sm, true, 1); // …then the fax is back
        }
        assert!(!sm.take_chart_complete());
        assert_eq!(sm.state(), WefaxState::Imaging);
    }

    #[test]
    fn absence_while_idle_never_completes_a_chart() {
        let (mut sm, _asm) = machine();
        presence_for(&mut sm, false, 600);
        assert!(!sm.take_chart_complete());
        assert_eq!(sm.state(), WefaxState::Idle);
    }

    #[test]
    fn sustained_presence_without_a_lock_starts_imaging_unaligned() {
        let (mut sm, _asm) = machine();
        presence_for(&mut sm, true, UNALIGNED_START_SECS as usize - 1);
        assert_eq!(sm.state(), WefaxState::Idle, "still hunting for phasing");
        presence_for(&mut sm, true, 1);
        assert_eq!(sm.state(), WefaxState::Imaging, "fallback: image unaligned");
    }

    #[test]
    fn a_finished_chart_restarts_the_unaligned_countdown() {
        // Fax present throughout: imaging starts on the fallback and the
        // next chart's preamble ends it. The presence already counted must
        // not carry over, or the next chart would start unaligned instead
        // of waiting for its own phasing.
        let (mut sm, mut asm) = machine();
        presence_for(&mut sm, true, UNALIGNED_START_SECS as usize);
        assert_eq!(sm.state(), WefaxState::Imaging);
        feed(
            &mut sm,
            &mut asm,
            &vec![content_line(); IMAGING_RESTART_HOLDOFF_LINES + 5],
        );
        feed(&mut sm, &mut asm, &vec![pulse_line(700); 6]);
        assert!(sm.take_chart_complete());
        feed(&mut sm, &mut asm, &[content_line()]); // Stopped → Idle
        presence_for(&mut sm, true, 1);
        assert_eq!(
            sm.state(),
            WefaxState::Idle,
            "the next chart waits for phasing, not the fallback"
        );
    }

    #[test]
    fn absence_before_a_lock_does_not_count_against_the_new_chart() {
        let (mut sm, mut asm) = machine();
        presence_for(&mut sm, false, 300);
        feed(&mut sm, &mut asm, &vec![pulse_line(300); 6]);
        assert_eq!(sm.state(), WefaxState::Imaging);
        presence_for(&mut sm, false, 1);
        assert_eq!(
            sm.state(),
            WefaxState::Imaging,
            "the chart-gap timer starts at the lock"
        );
    }

    #[test]
    fn interrupted_presence_restarts_the_unaligned_countdown() {
        let (mut sm, _asm) = machine();
        presence_for(&mut sm, true, 30);
        presence_for(&mut sm, false, 1);
        presence_for(&mut sm, true, 30);
        assert_eq!(sm.state(), WefaxState::Idle);
    }
}
