//! Sampling mode control — direct sampling, test mode, bias-T.
//!
//! Ports `rtlsdr_set_direct_sampling`, `rtlsdr_set_testmode`,
//! `rtlsdr_set_bias_tee`, `rtlsdr_set_bias_tee_gpio`.

use crate::constants::R82XX_IF_FREQ;
use crate::error::RtlSdrError;
use crate::reg::TunerType;
use crate::usb;

use super::RtlSdrDevice;

impl RtlSdrDevice {
    /// Set direct sampling mode.
    ///
    /// Ports `rtlsdr_set_direct_sampling`.
    /// - 0: disabled (use tuner)
    /// - 1: I-ADC input
    /// - 2: Q-ADC input
    pub fn set_direct_sampling(&mut self, on: i32) -> Result<(), RtlSdrError> {
        if on != 0 {
            // Disable tuner
            if let Some(tuner) = &mut self.tuner {
                usb::set_i2c_repeater(&self.handle, true)?;
                let _ = tuner.exit(&self.handle);
                usb::set_i2c_repeater(&self.handle, false)?;
            }

            // Disable Zero-IF mode
            usb::demod_write_reg(&self.handle, 1, 0xb1, 0x1a, 1)?;

            // Disable spectrum inversion
            usb::demod_write_reg(&self.handle, 1, 0x15, 0x00, 1)?;

            // Only enable In-phase ADC input
            usb::demod_write_reg(&self.handle, 0, 0x08, 0x4d, 1)?;

            // Swap I and Q ADC (allows selecting between two inputs)
            usb::demod_write_reg(&self.handle, 0, 0x06, if on > 1 { 0x90 } else { 0x80 }, 1)?;

            tracing::info!("Enabled direct sampling mode, input {on}");
            self.direct_sampling = on;
        } else {
            // Re-enable tuner
            if let Some(tuner) = &mut self.tuner {
                usb::set_i2c_repeater(&self.handle, true)?;
                let _ = tuner.init(&self.handle);
                usb::set_i2c_repeater(&self.handle, false)?;
            }

            if self.tuner_type == TunerType::R820T || self.tuner_type == TunerType::R828D {
                self.set_if_freq(R82XX_IF_FREQ)?;

                // Enable spectrum inversion
                usb::demod_write_reg(&self.handle, 1, 0x15, 0x01, 1)?;
            } else {
                self.set_if_freq(0)?;

                // Enable In-phase + Quadrature ADC input
                usb::demod_write_reg(&self.handle, 0, 0x08, 0xcd, 1)?;

                // Enable Zero-IF mode
                usb::demod_write_reg(&self.handle, 1, 0xb1, 0x1b, 1)?;
            }

            // opt_adc_iq = 0, default ADC_I/ADC_Q datapath
            usb::demod_write_reg(&self.handle, 0, 0x06, 0x80, 1)?;

            tracing::info!("Disabled direct sampling mode");
            self.direct_sampling = 0;
        }

        self.set_center_freq(self.freq)?;

        Ok(())
    }

    /// Set test mode (8-bit counter output).
    ///
    /// Ports `rtlsdr_set_testmode`.
    pub fn set_testmode(&self, on: bool) -> Result<(), RtlSdrError> {
        usb::demod_write_reg(&self.handle, 0, 0x19, if on { 0x03 } else { 0x05 }, 1)
    }

    /// Set bias-T power on a specific GPIO pin.
    ///
    /// Ports `rtlsdr_set_bias_tee_gpio`.
    pub fn set_bias_tee_gpio(&self, gpio: u8, on: bool) -> Result<(), RtlSdrError> {
        usb::set_gpio_output(&self.handle, gpio)?;
        usb::set_gpio_bit(&self.handle, gpio, on)
    }

    /// Set bias-T power on the default GPIO (pin 0).
    ///
    /// Ports `rtlsdr_set_bias_tee`.
    pub fn set_bias_tee(&self, on: bool) -> Result<(), RtlSdrError> {
        self.set_bias_tee_gpio(0, on)
    }
}
