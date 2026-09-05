//! WEFAX / HF radiofax decoder. Pure DSP: USB-demodulated fax audio →
//! greyscale scanlines. Analytic-signal FM discriminator + IOC-576 line
//! assembly at 120 lpm. No I/O, no threads. See the design spec.

// `dead_code` allowed only while this is a scaffold: `Discriminator` is
// `pub(crate)` and wired into the line-assembly pipeline in Task 2.
#[allow(dead_code)]
mod discriminator;

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
