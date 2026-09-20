//! WEFAX / HF radiofax decoder. Pure DSP: USB-demodulated fax audio →
//! greyscale scanlines. Analytic-signal FM discriminator + IOC-576 line
//! assembly at 120 lpm. No I/O, no threads. See the design spec.

mod afc;
mod assembly;
mod discriminator;
mod phasing;
mod presence;
mod sync;
mod tones;

use afc::AfcMapper;
use assembly::LineAssembler;
use discriminator::Discriminator;
pub use presence::WefaxPresenceDetector;
use sdr_types::DspError;
use sync::{LineDisposition, SyncMachine};

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
mod tests;

/// Lines per minute (WMO standard fax rate handled here).
pub const WEFAX_LINES_PER_MIN: u32 = 120;
/// Index of Cooperation.
pub const WEFAX_IOC: u32 = 576;
/// Usable pixels per line = round(π · IOC).
pub const PIXELS_PER_LINE: usize = 1809;
/// Black / white / center subcarrier tones and peak deviation (Hz).
pub const SUBCARRIER_BLACK_HZ: f64 = 1500.0;
pub const SUBCARRIER_WHITE_HZ: f64 = 2300.0;
pub const SUBCARRIER_CENTER_HZ: f64 = 1900.0;
pub const SUBCARRIER_DEV_HZ: f64 = 400.0;
/// Chart framing tones (Hz).
pub const START_TONE_HZ: f64 = 300.0;
pub const STOP_TONE_HZ: f64 = 450.0;

/// Where the decoder is in the start-tone / phasing / imaging / stop cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WefaxState {
    Idle,
    Phasing,
    Imaging,
    Stopped,
}

/// One assembled scanline.
#[derive(Debug, Clone)]
pub struct WefaxLine {
    pub pixels: [u8; PIXELS_PER_LINE],
    pub line_index: u32,
    pub state: WefaxState,
}

impl Default for WefaxLine {
    fn default() -> Self {
        Self {
            pixels: [0; PIXELS_PER_LINE],
            line_index: 0,
            state: WefaxState::Imaging,
        }
    }
}

/// Minimum usable input rate (Hz) — Nyquist must clear the 2300 Hz white tone.
const MIN_INPUT_RATE_HZ: u32 = 8_000;

/// WEFAX decoder: FM discriminator → pixel-clock line assembly, gated by a
/// start-tone / phasing / stop-tone [`SyncMachine`]. When `gated` (the
/// default from [`WefaxDecoder::new`]), no lines are emitted until the
/// phasing signal locks the left edge, and the stop tone auto-segments the
/// chart. When constructed with [`WefaxDecoder::new_free_running`], the sync
/// machine is bypassed: the decoder starts in [`WefaxState::Imaging`] and
/// emits a line every `samples_per_line` input samples regardless of content.
pub struct WefaxDecoder {
    discriminator: Discriminator,
    afc: AfcMapper,
    assembler: LineAssembler,
    line_index: u32,
    sync: SyncMachine,
    gated: bool,
}

impl WefaxDecoder {
    /// Sync-gated decoder: emission waits for a phasing lock and charts are
    /// auto-segmented on the stop tone.
    ///
    /// # Errors
    ///
    /// Returns [`DspError::InvalidParameter`] if `input_rate_hz` is below
    /// [`MIN_INPUT_RATE_HZ`].
    pub fn new(input_rate_hz: u32) -> Result<Self, DspError> {
        Self::build(input_rate_hz, true)
    }

    /// Free-running decoder: no sync gating — starts in
    /// [`WefaxState::Imaging`] and emits every assembled line. Used by the
    /// offline render harness so a fixture without a clean preamble still
    /// produces a full image.
    #[must_use]
    pub fn new_free_running(input_rate_hz: u32) -> Self {
        // Free-running skips the rate guard so the render harness never
        // fails on an unusual fixture rate; the discriminator and assembler
        // handle any rate above zero, and the harness enforces a sane rate.
        let rate = input_rate_hz.max(MIN_INPUT_RATE_HZ);
        let sr = f64::from(rate);
        Self {
            discriminator: Discriminator::new(sr),
            afc: AfcMapper::new(sr),
            assembler: LineAssembler::new(sr / 2.0),
            line_index: 0,
            sync: SyncMachine::new(sr),
            gated: false,
        }
    }

    fn build(input_rate_hz: u32, gated: bool) -> Result<Self, DspError> {
        if input_rate_hz < MIN_INPUT_RATE_HZ {
            return Err(DspError::InvalidParameter(format!(
                "WEFAX input rate {input_rate_hz} Hz below minimum {MIN_INPUT_RATE_HZ} Hz"
            )));
        }
        let sr = f64::from(input_rate_hz);
        Ok(Self {
            discriminator: Discriminator::new(sr),
            afc: AfcMapper::new(sr),
            assembler: LineAssembler::new(sr / 2.0), // 120 lpm = 2 lines/s
            line_index: 0,
            sync: SyncMachine::new(sr),
            gated,
        })
    }

    /// Current decoder state. Free-running decoders always report
    /// [`WefaxState::Imaging`].
    #[must_use]
    pub fn state(&self) -> WefaxState {
        if self.gated {
            self.sync.state()
        } else {
            WefaxState::Imaging
        }
    }

    /// Take and clear the "a chart just finished" flag, set when a stop tone
    /// finalizes a chart. Always `false` for free-running decoders.
    pub fn take_chart_complete(&mut self) -> bool {
        self.sync.take_chart_complete()
    }

    /// Feed audio; append completed lines to `out`, returning the count
    /// written. Lines beyond `out.len()` are dropped (not buffered).
    ///
    /// # Errors
    ///
    /// Never fails today; returns `Result` for forward compatibility and
    /// to match the [`crate::apt::AptDecoder`] contract.
    pub fn process(&mut self, input: &[f32], out: &mut [WefaxLine]) -> Result<usize, DspError> {
        let mut written = 0usize;
        for &s in input {
            let sf = f64::from(s);
            let dev = self.discriminator.push(sf);
            let brightness = self.afc.push(dev);
            if self.gated {
                self.sync.on_sample(sf);
            }
            if let Some(pixels) = self.assembler.push(brightness) {
                if self.gated {
                    self.emit_gated(&pixels, out, &mut written);
                } else {
                    self.write_line(&pixels, out, &mut written);
                }
            }
        }
        Ok(written)
    }

    /// Route a finished line through the sync machine, emitting only when it
    /// says so and resetting the line counter once a chart completes.
    fn emit_gated(
        &mut self,
        pixels: &[u8; PIXELS_PER_LINE],
        out: &mut [WefaxLine],
        written: &mut usize,
    ) {
        let was_stopped = self.sync.state() == WefaxState::Stopped;
        if let LineDisposition::Emit = self.sync.on_line(pixels, &mut self.assembler) {
            self.write_line(pixels, out, written);
        }
        if was_stopped {
            self.line_index = 0; // start the next chart at line 0
        }
    }

    /// Append one line to `out` (if room) and advance the line counter.
    fn write_line(
        &mut self,
        pixels: &[u8; PIXELS_PER_LINE],
        out: &mut [WefaxLine],
        written: &mut usize,
    ) {
        if *written < out.len() {
            out[*written] = WefaxLine {
                pixels: *pixels,
                line_index: self.line_index,
                state: self.state(),
            };
            *written += 1;
        }
        self.line_index = self.line_index.wrapping_add(1);
    }
}
