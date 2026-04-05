//! Fitipower FC0012 tuner driver.
//!
//! Faithful port of `tuner_fc0012.c` from librtlsdr.
//!
//! Original copyright:
//! - Copyright (C) 2012 Hans-Frieder Vogt <hfvogt@gmx.net>
//! - Copyright (C) 2012 Steve Markgraf <steve@steve-m.de> (librtlsdr modifications)

use crate::error::RtlSdrError;
use crate::tuner::Tuner;
use crate::usb;

// ---------------------------------------------------------------------------
// I2C address and identification
// ---------------------------------------------------------------------------

/// FC0012 I2C address.
pub const I2C_ADDR: u8 = 0xc6;

/// Register address used to identify the FC0012 (chip ID register).
pub const CHECK_ADDR: u8 = 0x00;

/// Expected chip ID value read from `CHECK_ADDR`.
pub const CHECK_VAL: u8 = 0xa1;

// ---------------------------------------------------------------------------
// Register addresses (only registers referenced outside the init loop)
// ---------------------------------------------------------------------------

/// RF divider count-to-9 cycles (reg 0x01, bits 0-3).
const REG_RF_A: u8 = 0x01;

/// RF divider total cycles (reg 0x02, bits 0-7).
const REG_RF_M: u8 = 0x02;

/// Fractional divider bits 8..14 (reg 0x03, bits 0-6).
const REG_RF_K_HIGH: u8 = 0x03;

/// Fractional divider bits 0..7 (reg 0x04, bits 0-7).
const REG_RF_K_LOW: u8 = 0x04;

/// RF output divider A (reg 0x05, bits 3-7).
const REG_RF_OUTDIV_A: u8 = 0x05;

/// LNA power down, RF output divider B, VCO speed, bandwidth (reg 0x06).
const REG_VCO_BW: u8 = 0x06;

/// AGC/LNA forcing register (reg 0x0d).
const REG_AGC_LNA_FORCE: u8 = 0x0d;

/// VCO calibration and voltage register (reg 0x0e).
const REG_VCO_CALIB: u8 = 0x0e;

/// LNA gain register (reg 0x13, bits 3-4 for gain, bit 7 for IX2).
const REG_LNA_GAIN: u8 = 0x13;

// ---------------------------------------------------------------------------
// Init register default values
// ---------------------------------------------------------------------------

/// Number of registers written during init (regs 0x01 to 0x15 = 21).
const NUM_INIT_REGS: usize = 21;

/// Default register values for init, indexed 0..20 mapping to regs 0x01..0x15.
///
/// Each value is annotated with its register address and meaning from the
/// original C source.
const INIT_REGS: [u8; NUM_INIT_REGS] = [
    0x05, // reg 0x01: RF_A
    0x10, // reg 0x02: RF_M
    0x00, // reg 0x03: RF_K_HIGH
    0x00, // reg 0x04: RF_K_LOW
    0x0f, // reg 0x05: RF_OUTDIV_A (may also be 0x0a)
    0x00, // reg 0x06: divider 2, VCO slow
    0x00, // reg 0x07: XTAL_SPEED (may also be 0x0f)
    0xff, // reg 0x08: AGC clock divide by 256, AGC gain 1/256, loop BW 1/8
    0x6e, // reg 0x09: disable loop-through (enable: 0x6f)
    0xb8, // reg 0x0a: disable LO test buffer
    0x82, // reg 0x0b: output clock = clock frequency (may also be 0x83)
    0xfc, // reg 0x0c: AGC up-down mode (may need 0xf8)
    0x02, // reg 0x0d: AGC not forcing & LNA forcing (DVB-T)
    0x00, // reg 0x0e: VCO calibration
    0x00, // reg 0x0f
    0x00, // reg 0x10 (may also be 0x0d)
    0x00, // reg 0x11
    0x1f, // reg 0x12: set to maximum gain
    0x00, // reg 0x13: low gain (low=0x00, high=0x10, enable IX2=0x80)
    0x00, // reg 0x14
    0x04, // reg 0x15: enable LNA COMPS
];

// ---------------------------------------------------------------------------
// Frequency divider constants
// ---------------------------------------------------------------------------

/// VCO frequency threshold for high-range selection (3.06 GHz).
const VCO_HIGH_THRESH: u64 = 3_060_000_000;

/// VCO re-calibration: low voltage threshold.
const VCO_VOLTAGE_LOW: u8 = 0x02;

/// VCO re-calibration: high voltage threshold.
const VCO_VOLTAGE_HIGH: u8 = 0x3c;

/// Mask for VCO voltage readback (bits 0-5).
const VCO_VOLTAGE_MASK: u8 = 0x3f;

/// VCO speed bit in register 0x06 (high VCO range).
const VCO_SPEED_BIT: u8 = 0x08;

/// Clock-out fix bit in register 0x06.
const CLOCK_OUT_BIT: u8 = 0x20;

/// VCO calibration trigger value (set high to trigger).
const VCO_CALIB_TRIGGER: u8 = 0x80;

/// VCO calibration reset value.
const VCO_CALIB_RESET: u8 = 0x00;

/// 28.8 MHz crystal select bit in register 0x07.
const XTAL_28_8_MHZ_BIT: u8 = 0x20;

/// Dual master bit in register 0x0c.
const DUAL_MASTER_BIT: u8 = 0x02;

/// Fractional divider threshold (xin >= 16384 adds 32768).
const XIN_THRESHOLD: u16 = 16384;

/// Fractional divider overflow correction.
const XIN_OVERFLOW_ADD: u16 = 32768;

/// Bandwidth bits mask in register 0x06 (bits 6-7).
const BW_MASK: u8 = 0x3f;

/// 6 MHz bandwidth setting for register 0x06.
const BW_6MHZ: u8 = 0x80;

/// 7 MHz bandwidth setting for register 0x06.
const BW_7MHZ: u8 = 0x40;

/// Bandwidth: 6 MHz in Hz.
const BW_6MHZ_HZ: u32 = 6_000_000;

/// Bandwidth: 7 MHz in Hz.
const BW_7MHZ_HZ: u32 = 7_000_000;

/// Modified register 0x05 lower bits for Realtek demod.
const REALTEK_DEMOD_BITS: u8 = 0x07;

/// LNA gain register mask (bits 0-4).
const LNA_GAIN_MASK: u8 = 0xe0;

// ---------------------------------------------------------------------------
// Frequency divider table
// ---------------------------------------------------------------------------

/// Frequency divider selection entry.
///
/// Maps a maximum frequency to the VCO multiplier and register values
/// for registers 0x05 and 0x06 that control the RF output divider.
struct FreqDivider {
    /// Maximum frequency in Hz (exclusive upper bound).
    max_freq: u32,
    /// VCO frequency multiplier.
    multi: u8,
    /// Value for register 0x05 (RF_OUTDIV_A).
    reg5: u8,
    /// Value for register 0x06 (RF_OUTDIV_B and VCO speed).
    reg6: u8,
}

/// Frequency divider table, ordered by ascending max frequency.
///
/// Each entry defines the multiplier used such that `freq * multi < 3.56 GHz`.
/// The last entry has no upper bound (used as fallback for freq >= 593334000).
const FREQ_DIVIDERS: [FreqDivider; 10] = [
    FreqDivider {
        max_freq: 37_084_000,
        multi: 96,
        reg5: 0x82,
        reg6: 0x00,
    },
    FreqDivider {
        max_freq: 55_625_000,
        multi: 64,
        reg5: 0x82,
        reg6: 0x02,
    },
    FreqDivider {
        max_freq: 74_167_000,
        multi: 48,
        reg5: 0x42,
        reg6: 0x00,
    },
    FreqDivider {
        max_freq: 111_250_000,
        multi: 32,
        reg5: 0x42,
        reg6: 0x02,
    },
    FreqDivider {
        max_freq: 148_334_000,
        multi: 24,
        reg5: 0x22,
        reg6: 0x00,
    },
    FreqDivider {
        max_freq: 222_500_000,
        multi: 16,
        reg5: 0x22,
        reg6: 0x02,
    },
    FreqDivider {
        max_freq: 296_667_000,
        multi: 12,
        reg5: 0x12,
        reg6: 0x00,
    },
    FreqDivider {
        max_freq: 445_000_000,
        multi: 8,
        reg5: 0x12,
        reg6: 0x02,
    },
    FreqDivider {
        max_freq: 593_334_000,
        multi: 6,
        reg5: 0x0a,
        reg6: 0x00,
    },
    // Fallback: freq >= 593334000
    FreqDivider {
        max_freq: u32::MAX,
        multi: 4,
        reg5: 0x0a,
        reg6: 0x02,
    },
];

// ---------------------------------------------------------------------------
// Gain table
// ---------------------------------------------------------------------------

/// Supported gain values in tenths of dB.
///
/// From librtlsdr `fc0012_gains[]`:
/// -9.9 dB, -4.0 dB, 7.1 dB, 17.9 dB, 19.2 dB
pub const FC0012_GAINS: [i32; 5] = [-99, -40, 71, 179, 192];

/// Gain register value for -9.9 dB.
const GAIN_NEG_9_9_DB: u8 = 0x02;

/// Gain register value for -4.0 dB.
const GAIN_NEG_4_0_DB: u8 = 0x00;

/// Gain register value for 7.1 dB.
const GAIN_7_1_DB: u8 = 0x08;

/// Gain register value for 17.9 dB.
const GAIN_17_9_DB: u8 = 0x17;

/// Gain register value for 19.2 dB.
const GAIN_19_2_DB: u8 = 0x10;

/// Gain threshold: below this, select -9.9 dB (tenths of dB).
const GAIN_THRESH_NEG_9_9: i32 = -40;

/// Gain threshold: below this, select -4.0 dB (tenths of dB).
const GAIN_THRESH_NEG_4_0: i32 = 71;

/// Gain threshold: below this, select 7.1 dB (tenths of dB).
const GAIN_THRESH_7_1: i32 = 179;

/// Gain threshold: below this, select 17.9 dB (tenths of dB).
const GAIN_THRESH_17_9: i32 = 192;

// ---------------------------------------------------------------------------
// PLL validation limits
// ---------------------------------------------------------------------------

/// Maximum valid value for register 1 (RF_A) during PLL calculation.
const PLL_REG1_MAX: u8 = 15;

/// Minimum valid value for register 2 (RF_M) during PLL calculation.
const PLL_REG2_MIN: u8 = 0x0b;

// ---------------------------------------------------------------------------
// FC0012 tuner state
// ---------------------------------------------------------------------------

/// Fitipower FC0012 tuner driver.
///
/// Ports the `fc0012_init`, `fc0012_set_params`, and `fc0012_set_gain`
/// functions from `tuner_fc0012.c`.
pub struct Fc0012Tuner {
    /// Crystal oscillator frequency in Hz.
    xtal: u32,
    /// Current bandwidth setting in Hz (stored for `set_freq`).
    bandwidth: u32,
    /// Current tuned frequency in Hz.
    freq: u32,
}

impl Fc0012Tuner {
    /// Create a new FC0012 tuner driver.
    pub fn new(xtal: u32) -> Self {
        Self {
            xtal,
            bandwidth: BW_6MHZ_HZ,
            freq: 0,
        }
    }

    /// Write a single register via I2C.
    ///
    /// Ports `fc0012_writereg`. Method (not associated fn) for consistency
    /// with the Tuner trait pattern and future state-dependent tuner drivers.
    #[allow(clippy::unused_self)]
    fn write_reg(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
        val: u8,
    ) -> Result<(), RtlSdrError> {
        usb::i2c_write_reg(handle, I2C_ADDR, reg, val)
    }

    /// Read a single register via I2C.
    ///
    /// Ports `fc0012_readreg`. Method (not associated fn) for consistency
    /// with the Tuner trait pattern and future state-dependent tuner drivers.
    #[allow(clippy::unused_self)]
    fn read_reg(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
    ) -> Result<u8, RtlSdrError> {
        usb::i2c_read_reg(handle, I2C_ADDR, reg)
    }

    /// Set tuner frequency and bandwidth.
    ///
    /// Exact port of `fc0012_set_params`.
    #[allow(clippy::cast_possible_truncation)]
    fn set_params(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq: u32,
        bandwidth: u32,
    ) -> Result<(), RtlSdrError> {
        let xtal_freq_div_2 = self.xtal / 2;

        // Select frequency divider and VCO frequency
        let divider = FREQ_DIVIDERS
            .iter()
            .find(|d| freq < d.max_freq)
            .unwrap_or(&FREQ_DIVIDERS[FREQ_DIVIDERS.len() - 1]);

        let multi = divider.multi;
        let mut reg5 = divider.reg5;
        let mut reg6 = divider.reg6;

        let f_vco = u64::from(freq) * u64::from(multi);

        let mut vco_select = false;
        if f_vco >= VCO_HIGH_THRESH {
            reg6 |= VCO_SPEED_BIT;
            vco_select = true;
        }

        // From divided value (XDIV) determine the FA (am) and FP (pm) values
        let mut xdiv = (f_vco / u64::from(xtal_freq_div_2)) as u16;
        let remainder = f_vco - u64::from(xdiv) * u64::from(xtal_freq_div_2);
        if remainder >= u64::from(xtal_freq_div_2 / 2) {
            xdiv += 1;
        }

        let mut pm = xdiv / 8;
        let mut am = xdiv - (8 * pm);

        if am < 2 {
            am += 8;
            pm -= 1;
        }

        let (reg1, reg2) = if pm > 31 {
            ((am + 8 * (pm - 31)) as u8, 31u8)
        } else {
            (am as u8, pm as u8)
        };

        if reg1 > PLL_REG1_MAX || reg2 < PLL_REG2_MIN {
            return Err(RtlSdrError::Tuner(format!(
                "FC0012: no valid PLL combination found for {freq} Hz"
            )));
        }

        // Fix clock out
        reg6 |= CLOCK_OUT_BIT;

        // Calculate XIN (fractional part of Delta Sigma PLL)
        let xin_remainder =
            (f_vco - (f_vco / u64::from(xtal_freq_div_2)) * u64::from(xtal_freq_div_2)) / 1000;
        let mut xin = ((xin_remainder as u16) << 15) / ((xtal_freq_div_2 / 1000) as u16);
        if xin >= XIN_THRESHOLD {
            xin += XIN_OVERFLOW_ADD;
        }

        let reg3 = (xin >> 8) as u8;
        let reg4 = (xin & 0xff) as u8;

        // Bandwidth selection (bits 6-7 of reg6)
        reg6 &= BW_MASK;
        match bandwidth {
            BW_6MHZ_HZ => reg6 |= BW_6MHZ,
            BW_7MHZ_HZ => reg6 |= BW_7MHZ,
            _ => {} // 8 MHz: no extra bits
        }

        // Modified for Realtek demod
        reg5 |= REALTEK_DEMOD_BITS;

        // Write PLL registers 0x01 through 0x06
        self.write_reg(handle, REG_RF_A, reg1)?;
        self.write_reg(handle, REG_RF_M, reg2)?;
        self.write_reg(handle, REG_RF_K_HIGH, reg3)?;
        self.write_reg(handle, REG_RF_K_LOW, reg4)?;
        self.write_reg(handle, REG_RF_OUTDIV_A, reg5)?;
        self.write_reg(handle, REG_VCO_BW, reg6)?;

        // VCO calibration: set high then low
        self.write_reg(handle, REG_VCO_CALIB, VCO_CALIB_TRIGGER)?;
        self.write_reg(handle, REG_VCO_CALIB, VCO_CALIB_RESET)?;

        // VCO re-calibration if needed
        self.write_reg(handle, REG_VCO_CALIB, VCO_CALIB_RESET)?;

        let tmp = self.read_reg(handle, REG_VCO_CALIB)?;

        // VCO selection based on control voltage
        let voltage = tmp & VCO_VOLTAGE_MASK;

        if vco_select {
            if voltage > VCO_VOLTAGE_HIGH {
                reg6 &= !VCO_SPEED_BIT;
                self.write_reg(handle, REG_VCO_BW, reg6)?;
                self.write_reg(handle, REG_VCO_CALIB, VCO_CALIB_TRIGGER)?;
                self.write_reg(handle, REG_VCO_CALIB, VCO_CALIB_RESET)?;
            }
        } else if voltage < VCO_VOLTAGE_LOW {
            reg6 |= VCO_SPEED_BIT;
            self.write_reg(handle, REG_VCO_BW, reg6)?;
            self.write_reg(handle, REG_VCO_CALIB, VCO_CALIB_TRIGGER)?;
            self.write_reg(handle, REG_VCO_CALIB, VCO_CALIB_RESET)?;
        }

        Ok(())
    }
}

impl Tuner for Fc0012Tuner {
    /// Initialize the FC0012 tuner.
    ///
    /// Exact port of `fc0012_init`.
    fn init(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        // Start with default register values
        let mut regs = INIT_REGS;

        // 28.8 MHz crystal: set bit 5 of register 0x07
        // Index 6 in our array corresponds to register 0x07
        regs[6] |= XTAL_28_8_MHZ_BIT;

        // Dual master mode: set bit 1 of register 0x0c
        // Index 11 in our array corresponds to register 0x0c
        regs[11] |= DUAL_MASTER_BIT;

        // Write registers 0x01 through 0x15
        for (i, &val) in regs.iter().enumerate() {
            let reg_addr = (i + 1) as u8;
            self.write_reg(handle, reg_addr, val)?;
        }

        Ok(())
    }

    /// Put the tuner in standby.
    ///
    /// The FC0012 C driver does not define an explicit exit/standby function.
    /// Power down the LNA by setting bit 0 of register 0x06.
    fn exit(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        // Power down LNA (bit 0 of reg 0x06 = LNA_POWER_DOWN)
        let val = self.read_reg(handle, REG_VCO_BW)?;
        self.write_reg(handle, REG_VCO_BW, val | 0x01)?;
        Ok(())
    }

    /// Set the tuner frequency.
    ///
    /// Delegates to `set_params` with the stored bandwidth.
    fn set_freq(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq: u32,
    ) -> Result<(), RtlSdrError> {
        self.freq = freq;
        self.set_params(handle, freq, self.bandwidth)
    }

    /// Set the tuner bandwidth. Returns 0 as the IF frequency (the FC0012
    /// does not have a configurable IF output like the R82XX).
    fn set_bw(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        bw: u32,
        _sample_rate: u32,
    ) -> Result<u32, RtlSdrError> {
        self.bandwidth = bw;
        // Re-apply frequency with new bandwidth if already tuned
        if self.freq > 0 {
            self.set_params(handle, self.freq, bw)?;
        }
        // FC0012 does not have a separate IF frequency to report
        Ok(0)
    }

    /// Set the tuner gain.
    ///
    /// Exact port of `fc0012_set_gain`. Gain is in tenths of dB.
    fn set_gain(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        gain: i32,
    ) -> Result<(), RtlSdrError> {
        let mut tmp = self.read_reg(handle, REG_LNA_GAIN)?;

        // Mask off gain bits (keep bits 5-7)
        tmp &= LNA_GAIN_MASK;

        // Select gain based on thresholds (tenths of dB)
        let gain_bits = if gain < GAIN_THRESH_NEG_9_9 {
            GAIN_NEG_9_9_DB
        } else if gain < GAIN_THRESH_NEG_4_0 {
            GAIN_NEG_4_0_DB
        } else if gain < GAIN_THRESH_7_1 {
            GAIN_7_1_DB
        } else if gain < GAIN_THRESH_17_9 {
            GAIN_17_9_DB
        } else {
            GAIN_19_2_DB
        };

        tmp |= gain_bits;
        self.write_reg(handle, REG_LNA_GAIN, tmp)?;

        Ok(())
    }

    /// Update the crystal frequency.
    fn set_xtal(&mut self, xtal: u32) {
        self.xtal = xtal;
    }

    /// Set manual or automatic gain mode.
    ///
    /// The FC0012 uses register 0x0d to control AGC/LNA forcing.
    fn set_gain_mode(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        manual: bool,
    ) -> Result<(), RtlSdrError> {
        if manual {
            // LNA forcing on, AGC not forcing (DVB-T mode from init: 0x02)
            self.write_reg(handle, REG_AGC_LNA_FORCE, 0x02)?;
        } else {
            // Disable LNA forcing for automatic gain
            self.write_reg(handle, REG_AGC_LNA_FORCE, 0x00)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_regs_length() {
        assert_eq!(INIT_REGS.len(), NUM_INIT_REGS);
        // Covers registers 0x01 through 0x15 = 21 registers
        assert_eq!(NUM_INIT_REGS, 21);
    }

    #[test]
    fn test_freq_dividers_ordered() {
        for w in FREQ_DIVIDERS.windows(2) {
            assert!(
                w[0].max_freq < w[1].max_freq,
                "frequency dividers must be in ascending order"
            );
        }
    }

    #[test]
    fn test_freq_dividers_cover_full_range() {
        // Last entry should cover everything up to u32::MAX
        assert_eq!(FREQ_DIVIDERS[FREQ_DIVIDERS.len() - 1].max_freq, u32::MAX);
    }

    #[test]
    fn test_gains_sorted() {
        for w in FC0012_GAINS.windows(2) {
            assert!(w[0] < w[1], "gains must be in ascending order");
        }
    }

    #[test]
    fn test_gains_count() {
        assert_eq!(FC0012_GAINS.len(), 5);
    }

    #[test]
    fn test_gains_values() {
        // Verify exact gain values from C source (tenths of dB)
        assert_eq!(FC0012_GAINS[0], -99); // -9.9 dB
        assert_eq!(FC0012_GAINS[1], -40); // -4.0 dB
        assert_eq!(FC0012_GAINS[2], 71); //  7.1 dB
        assert_eq!(FC0012_GAINS[3], 179); // 17.9 dB
        assert_eq!(FC0012_GAINS[4], 192); // 19.2 dB
    }

    #[test]
    fn test_i2c_addr() {
        assert_eq!(I2C_ADDR, 0xc6);
    }

    #[test]
    fn test_check_val() {
        assert_eq!(CHECK_VAL, 0xa1);
    }

    #[test]
    fn test_new_defaults() {
        let tuner = Fc0012Tuner::new(28_800_000);
        assert_eq!(tuner.xtal, 28_800_000);
        assert_eq!(tuner.bandwidth, BW_6MHZ_HZ);
        assert_eq!(tuner.freq, 0);
    }

    #[test]
    fn test_init_regs_xtal_bit() {
        // Register 0x07 is at index 6 in INIT_REGS.
        // After applying XTAL_28_8_MHZ_BIT, bit 5 should be set.
        let mut regs = INIT_REGS;
        regs[6] |= XTAL_28_8_MHZ_BIT;
        assert_eq!(regs[6] & XTAL_28_8_MHZ_BIT, XTAL_28_8_MHZ_BIT);
    }

    #[test]
    fn test_init_regs_dual_master_bit() {
        // Register 0x0c is at index 11 in INIT_REGS.
        // After applying DUAL_MASTER_BIT, bit 1 should be set.
        let mut regs = INIT_REGS;
        regs[11] |= DUAL_MASTER_BIT;
        assert_eq!(regs[11] & DUAL_MASTER_BIT, DUAL_MASTER_BIT);
    }

    #[test]
    fn test_freq_divider_selection() {
        // 100 MHz should select multi=32
        let divider = FREQ_DIVIDERS
            .iter()
            .find(|d| 100_000_000 < d.max_freq)
            .expect("should find divider for 100 MHz");
        assert_eq!(divider.multi, 32);

        // 500 MHz should select multi=6
        let divider = FREQ_DIVIDERS
            .iter()
            .find(|d| 500_000_000 < d.max_freq)
            .expect("should find divider for 500 MHz");
        assert_eq!(divider.multi, 6);

        // 30 MHz should select multi=96
        let divider = FREQ_DIVIDERS
            .iter()
            .find(|d| 30_000_000 < d.max_freq)
            .expect("should find divider for 30 MHz");
        assert_eq!(divider.multi, 96);

        // 700 MHz should select multi=4 (fallback)
        let divider = FREQ_DIVIDERS
            .iter()
            .find(|d| 700_000_000 < d.max_freq)
            .expect("should find divider for 700 MHz");
        assert_eq!(divider.multi, 4);
    }

    #[test]
    fn test_gain_threshold_values() {
        // Verify gain thresholds match the C source boundary values (tenths of dB)
        assert_eq!(GAIN_THRESH_NEG_9_9, -40);
        assert_eq!(GAIN_THRESH_NEG_4_0, 71);
        assert_eq!(GAIN_THRESH_7_1, 179);
        assert_eq!(GAIN_THRESH_17_9, 192);
    }
}
