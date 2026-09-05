//! WEFAX / HF radiofax decoder. Pure DSP: USB-demodulated fax audio →
//! greyscale scanlines. Analytic-signal FM discriminator + IOC-576 line
//! assembly at 120 lpm. No I/O, no threads. See the design spec.

mod assembly;
mod discriminator;
mod tones;

use assembly::LineAssembler;
use discriminator::Discriminator;
use sdr_types::DspError;

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

/// Free-running WEFAX decoder: FM discriminator → pixel-clock line
/// assembly. Sync (start tone / phasing lines) is added in a later task;
/// today the decoder starts in [`WefaxState::Imaging`] and emits a line
/// every `samples_per_line` input samples regardless of content.
pub struct WefaxDecoder {
    discriminator: Discriminator,
    assembler: LineAssembler,
    line_index: u32,
    state: WefaxState,
}

impl WefaxDecoder {
    /// # Errors
    ///
    /// Returns [`DspError::InvalidParameter`] if `input_rate_hz` is below
    /// [`MIN_INPUT_RATE_HZ`].
    pub fn new(input_rate_hz: u32) -> Result<Self, DspError> {
        if input_rate_hz < MIN_INPUT_RATE_HZ {
            return Err(DspError::InvalidParameter(format!(
                "WEFAX input rate {input_rate_hz} Hz below minimum {MIN_INPUT_RATE_HZ} Hz"
            )));
        }
        let sr = f64::from(input_rate_hz);
        Ok(Self {
            discriminator: Discriminator::new(sr),
            assembler: LineAssembler::new(sr / 2.0), // 120 lpm = 2 lines/s
            line_index: 0,
            state: WefaxState::Imaging, // free-running for Task 2; sync added later
        })
    }

    /// Current decoder state.
    #[must_use]
    pub fn state(&self) -> WefaxState {
        self.state
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
            let brightness = self.discriminator.push(f64::from(s));
            if let Some(pixels) = self.assembler.push(brightness) {
                if written < out.len() {
                    out[written] = WefaxLine {
                        pixels,
                        line_index: self.line_index,
                        state: self.state,
                    };
                    written += 1;
                }
                self.line_index = self.line_index.wrapping_add(1);
            }
        }
        Ok(written)
    }
}
