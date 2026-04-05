//! EEPROM read/write.
//!
//! Ports `rtlsdr_read_eeprom` and `rtlsdr_write_eeprom`.

use crate::constants::EEPROM_ADDR;
use crate::error::RtlSdrError;
use crate::reg::Block;
use crate::usb;

use super::RtlSdrDevice;

impl RtlSdrDevice {
    /// Write data to the device EEPROM.
    ///
    /// Ports `rtlsdr_write_eeprom`.
    pub fn write_eeprom(&self, data: &[u8], offset: u8) -> Result<(), RtlSdrError> {
        if (data.len() + offset as usize) > 256 {
            return Err(RtlSdrError::InvalidParameter(
                "EEPROM write exceeds 256 bytes".to_string(),
            ));
        }

        for (i, &byte) in data.iter().enumerate() {
            let addr_byte = (i as u8).wrapping_add(offset);

            // Read current value first
            let cmd = [addr_byte];
            usb::write_array(&self.handle, Block::Iic, u16::from(EEPROM_ADDR), &cmd)?;
            let mut current = [0u8; 1];
            usb::read_array(
                &self.handle,
                Block::Iic,
                u16::from(EEPROM_ADDR),
                &mut current,
            )?;

            // Only write if different
            if current[0] == byte {
                continue;
            }

            let cmd = [addr_byte, byte];
            let r = usb::write_array(&self.handle, Block::Iic, u16::from(EEPROM_ADDR), &cmd)?;
            if r != cmd.len() {
                return Err(RtlSdrError::RegisterAccess);
            }

            // Delay for EEPROM write cycle (5ms)
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        Ok(())
    }

    /// Read data from the device EEPROM.
    ///
    /// Ports `rtlsdr_read_eeprom`.
    pub fn read_eeprom(&self, data: &mut [u8], offset: u8) -> Result<(), RtlSdrError> {
        if (data.len() + offset as usize) > 256 {
            return Err(RtlSdrError::InvalidParameter(
                "EEPROM read exceeds 256 bytes".to_string(),
            ));
        }

        // Set read address
        usb::write_array(&self.handle, Block::Iic, u16::from(EEPROM_ADDR), &[offset])?;

        // Read bytes one at a time (matching C implementation)
        for byte in data.iter_mut() {
            let mut buf = [0u8; 1];
            usb::read_array(&self.handle, Block::Iic, u16::from(EEPROM_ADDR), &mut buf)?;
            *byte = buf[0];
        }

        Ok(())
    }
}
