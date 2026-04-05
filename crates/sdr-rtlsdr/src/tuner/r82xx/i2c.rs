//! R82XX I2C communication with shadow register optimization.
//!
//! Exact port of the shadow_store, shadow_equal, r82xx_write,
//! r82xx_read, and r82xx_write_reg_mask functions.

use crate::error::RtlSdrError;
use crate::usb;

use super::R82xxPriv;
use super::constants::{NUM_REGS, REG_SHADOW_START, bitrev};

impl R82xxPriv {
    /// Store values in shadow registers.
    ///
    /// Ports `shadow_store`.
    pub(super) fn shadow_store(&mut self, reg: u8, val: &[u8]) {
        let mut r = reg as i32 - i32::from(REG_SHADOW_START);
        let mut offset = 0usize;
        let mut len = val.len() as i32;

        if r < 0 {
            len += r;
            offset = (-r) as usize;
            r = 0;
        }
        if len <= 0 {
            return;
        }
        let r = r as usize;
        if len > (NUM_REGS - r) as i32 {
            len = (NUM_REGS - r) as i32;
        }
        let len = len as usize;
        self.regs[r..r + len].copy_from_slice(&val[offset..offset + len]);
    }

    /// Check if shadow registers match the given values.
    ///
    /// Ports `shadow_equal`.
    pub(super) fn shadow_equal(&self, reg: u8, val: &[u8]) -> bool {
        let r = reg as i32 - i32::from(REG_SHADOW_START);
        let len = val.len() as i32;

        if r < 0 || len < 0 || len > (NUM_REGS as i32 - r) {
            return false;
        }
        let r = r as usize;
        let len = len as usize;
        self.regs[r..r + len] == val[..len]
    }

    /// Write to R82XX registers via I2C with shadow optimization.
    ///
    /// Ports `r82xx_write`. Skips writes if shadow matches.
    /// Splits large writes to respect max_i2c_msg_len.
    pub(super) fn write(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
        val: &[u8],
    ) -> Result<(), RtlSdrError> {
        // Skip if shadow matches
        if self.shadow_equal(reg, val) {
            return Ok(());
        }

        // Store shadow
        self.shadow_store(reg, val);

        let max_msg = self.max_i2c_msg_len;
        let mut pos = 0usize;
        let mut current_reg = reg;
        let mut remaining = val.len();

        while remaining > 0 {
            let size = remaining.min(max_msg - 1);

            // Build I2C message: [reg, data...]
            self.buf[0] = current_reg;
            self.buf[1..1 + size].copy_from_slice(&val[pos..pos + size]);

            let rc = usb::i2c_write(handle, self.i2c_addr, &self.buf[..size + 1])?;
            if rc != size + 1 {
                return Err(RtlSdrError::Tuner(format!(
                    "i2c write failed: wrote {rc}, expected {}",
                    size + 1
                )));
            }

            current_reg += size as u8;
            remaining -= size;
            pos += size;
        }

        Ok(())
    }

    /// Write a single register.
    ///
    /// Ports `r82xx_write_reg`.
    pub(super) fn write_reg(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
        val: u8,
    ) -> Result<(), RtlSdrError> {
        self.write(handle, reg, &[val])
    }

    /// Read from cached shadow register.
    ///
    /// Ports `r82xx_read_cache_reg`.
    pub(super) fn read_cache_reg(&self, reg: u8) -> Option<u8> {
        let r = reg as i32 - i32::from(REG_SHADOW_START);
        if r >= 0 && (r as usize) < NUM_REGS {
            Some(self.regs[r as usize])
        } else {
            None
        }
    }

    /// Write a register with bit mask (read-modify-write from cache).
    ///
    /// Ports `r82xx_write_reg_mask`.
    pub(super) fn write_reg_mask(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
        val: u8,
        bit_mask: u8,
    ) -> Result<(), RtlSdrError> {
        let cached = self
            .read_cache_reg(reg)
            .ok_or_else(|| RtlSdrError::Tuner(format!("no cached value for reg 0x{reg:02x}")))?;

        let new_val = (cached & !bit_mask) | (val & bit_mask);
        self.write(handle, reg, &[new_val])
    }

    /// Read registers from the tuner via I2C.
    ///
    /// Ports `r82xx_read`. Data is bit-reversed per R82XX convention.
    pub(super) fn read(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
        out: &mut [u8],
    ) -> Result<(), RtlSdrError> {
        // Write register address
        self.buf[0] = reg;
        let rc = usb::i2c_write(handle, self.i2c_addr, &self.buf[..1])?;
        if rc != 1 {
            return Err(RtlSdrError::Tuner(format!(
                "i2c read addr write failed: {rc}"
            )));
        }

        // Read data
        let rc = usb::i2c_read(handle, self.i2c_addr, &mut self.buf[1..1 + out.len()])?;
        if rc != out.len() {
            return Err(RtlSdrError::Tuner(format!(
                "i2c read data failed: got {rc}, expected {}",
                out.len()
            )));
        }

        // Bit-reverse the data
        for (i, byte) in self.buf[1..1 + out.len()].iter().enumerate() {
            out[i] = bitrev(*byte);
        }

        Ok(())
    }
}
