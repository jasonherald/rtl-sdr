//! Tuner gain control.
//!
//! Ports `rtlsdr_set_tuner_gain`, `rtlsdr_set_tuner_gain_mode`,
//! `rtlsdr_set_tuner_if_gain`, `rtlsdr_set_agc_mode`.

use crate::error::RtlSdrError;
use crate::usb;

use super::RtlSdrDevice;

impl RtlSdrDevice {
    /// Set tuner gain in tenths of dB.
    ///
    /// Ports `rtlsdr_set_tuner_gain`.
    pub fn set_tuner_gain(&mut self, gain: i32) -> Result<(), RtlSdrError> {
        if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let result = tuner.set_gain(&self.handle, gain);
            usb::set_i2c_repeater(&self.handle, false)?;

            if result.is_ok() {
                self.gain = gain;
            } else {
                self.gain = 0;
            }
            result
        } else {
            Ok(())
        }
    }

    /// Set tuner gain mode.
    ///
    /// Ports `rtlsdr_set_tuner_gain_mode`.
    /// `manual = true` for manual gain, `false` for automatic.
    pub fn set_tuner_gain_mode(&mut self, manual: bool) -> Result<(), RtlSdrError> {
        if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let result = tuner.set_gain_mode(&self.handle, manual);
            usb::set_i2c_repeater(&self.handle, false)?;
            result
        } else {
            Ok(())
        }
    }

    /// Set RTL2832 AGC mode.
    ///
    /// Ports `rtlsdr_set_agc_mode`.
    pub fn set_agc_mode(&self, on: bool) -> Result<(), RtlSdrError> {
        usb::demod_write_reg(&self.handle, 0, 0x19, if on { 0x25 } else { 0x05 }, 1)
    }
}
