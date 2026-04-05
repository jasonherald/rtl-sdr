//! Fitipower FC0013 tuner driver.
//!
//! Faithful port of `tuner_fc0013.c` from librtlsdr.
//!
//! Original copyright:
//! - Copyright (C) 2012 Hans-Frieder Vogt <hfvogt@gmx.net>
//! - Copyright (C) 2010 Fitipower Integrated Technology Inc (partial driver code)
//! - Copyright (C) 2012 Steve Markgraf <steve@steve-m.de> (librtlsdr modifications)

use crate::error::RtlSdrError;
use crate::tuner::Tuner;
use crate::usb;

// ---------------------------------------------------------------------------
// I2C address and identification
// ---------------------------------------------------------------------------

/// FC0013 I2C address.
pub const I2C_ADDR: u8 = 0xc6;

/// Register address used to identify the FC0013 (chip ID register).
pub const CHECK_ADDR: u8 = 0x00;

/// Expected chip ID value read from `CHECK_ADDR`.
pub const CHECK_VAL: u8 = 0xa3;

// ---------------------------------------------------------------------------
// Register addresses
// ---------------------------------------------------------------------------

/// RF divider count-to-9 cycles (reg 0x01).
const REG_RF_A: u8 = 0x01;

/// RF divider total cycles (reg 0x02).
const REG_RF_M: u8 = 0x02;

/// Fractional divider bits 8..15 (reg 0x03).
const REG_RF_K_HIGH: u8 = 0x03;

/// Fractional divider bits 0..7 (reg 0x04).
const REG_RF_K_LOW: u8 = 0x04;

/// RF output divider A (reg 0x05).
const REG_RF_OUTDIV_A: u8 = 0x05;

/// LNA power down, RF output divider B, VCO speed, bandwidth (reg 0x06).
const REG_VCO_BW: u8 = 0x06;

/// Crystal speed / VHF filter control register (reg 0x07).
const REG_XTAL_SPEED: u8 = 0x07;

/// RC calibration push register (reg 0x10).
/// Used by `rc_cal_add`/`rc_cal_reset` (ported for completeness).
#[allow(dead_code)]
const REG_RC_CAL: u8 = 0x10;

/// Multi select / 64x divider register (reg 0x11).
const REG_MULTI_SELECT: u8 = 0x11;

/// IF gain register (reg 0x13).
const REG_IF_GAIN: u8 = 0x13;

/// AGC/LNA forcing register (reg 0x0d).
const REG_AGC_LNA_FORCE: u8 = 0x0d;

/// VCO calibration register (reg 0x0e).
const REG_VCO_CALIB: u8 = 0x0e;

/// LNA gain / UHF-VHF-GPS band register (reg 0x14).
const REG_LNA_GAIN: u8 = 0x14;

/// VHF tracking filter register (reg 0x1d).
const REG_VHF_TRACK: u8 = 0x1d;

// ---------------------------------------------------------------------------
// Init register default values
// ---------------------------------------------------------------------------

/// Number of registers written during init (regs 0x01 to 0x15 = 21).
const NUM_INIT_REGS: usize = 21;

/// Default register values for init, indexed 0..20 mapping to regs 0x01..0x15.
///
/// The C source uses `reg[0]` as a dummy (unused); we skip it and index
/// from 0 corresponding to register address 0x01.
const INIT_REGS: [u8; NUM_INIT_REGS] = [
    0x09, // reg 0x01
    0x16, // reg 0x02
    0x00, // reg 0x03
    0x00, // reg 0x04
    0x17, // reg 0x05
    0x02, // reg 0x06: LPF bandwidth
    0x0a, // reg 0x07: CHECK (xtal speed bits applied later)
    0xff, // reg 0x08: AGC clock divide by 256, AGC gain 1/256, loop BW 1/8
    0x6e, // reg 0x09: disable loop-through (enable: 0x6f)
    0xb8, // reg 0x0a: disable LO test buffer
    0x82, // reg 0x0b: CHECK
    0xfc, // reg 0x0c: AGC up-down mode (may need 0xf8)
    0x01, // reg 0x0d: AGC not forcing & LNA forcing (may need 0x02)
    0x00, // reg 0x0e
    0x00, // reg 0x0f
    0x00, // reg 0x10
    0x00, // reg 0x11
    0x00, // reg 0x12
    0x00, // reg 0x13
    0x50, // reg 0x14: DVB-t high gain, UHF (mid: 0x48, low: 0x40)
    0x01, // reg 0x15
];

// ---------------------------------------------------------------------------
// Frequency-related constants
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

/// VCO calibration trigger value.
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

/// Bandwidth bits mask in register 0x06 (bits 6-7 cleared).
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

/// LNA gain register mask (bits 5-7 preserved, bits 0-4 cleared).
const LNA_GAIN_MASK: u8 = 0xe0;

/// VHF filter enable bit in register 0x07.
const VHF_FILTER_ENABLE_BIT: u8 = 0x10;

/// VHF filter disable mask for register 0x07.
const VHF_FILTER_DISABLE_MASK: u8 = 0xef;

/// UHF/GPS band mask for register 0x14 (bits 0-4 preserved).
const BAND_SELECT_MASK: u8 = 0x1f;

/// UHF enable bit in register 0x14.
const UHF_ENABLE_BIT: u8 = 0x40;

/// VHF tracking filter mask for register 0x1d (preserves bits 0-1 and 5-7).
const VHF_TRACK_MASK: u8 = 0xe3;

/// Multi=64 select bit in register 0x11.
const MULTI_64_BIT: u8 = 0x04;

/// Multi=64 disable mask for register 0x11.
const MULTI_64_DISABLE_MASK: u8 = 0xfb;

/// Manual gain mode bit in register 0x0d (bit 3).
const MANUAL_GAIN_BIT: u8 = 1 << 3;

/// Fixed IF gain value written to register 0x13.
const FIXED_IF_GAIN: u8 = 0x0a;

/// RC calibration forcing mode value for register 0x0d.
/// Used by `rc_cal_add` (ported for completeness).
#[allow(dead_code)]
const RC_CAL_FORCE: u8 = 0x11;

/// RC calibration reset value for register 0x0d.
/// Used by `rc_cal_reset` (ported for completeness).
#[allow(dead_code)]
const RC_CAL_RESET: u8 = 0x01;

/// Maximum valid RC calibration value.
/// Used by `rc_cal_add` (ported for completeness).
#[allow(dead_code)]
const RC_CAL_MAX: u8 = 0x0f;

// ---------------------------------------------------------------------------
// VHF tracking filter values
// ---------------------------------------------------------------------------

/// VHF track value for freq <= 177.5 MHz (track 7).
const VHF_TRACK_7: u8 = 0x1c;

/// VHF track value for freq <= 184.5 MHz (track 6).
const VHF_TRACK_6: u8 = 0x18;

/// VHF track value for freq <= 191.5 MHz (track 5).
const VHF_TRACK_5: u8 = 0x14;

/// VHF track value for freq <= 198.5 MHz (track 4).
const VHF_TRACK_4: u8 = 0x10;

/// VHF track value for freq <= 205.5 MHz (track 3).
const VHF_TRACK_3: u8 = 0x0c;

/// VHF track value for freq <= 219.5 MHz (track 2).
const VHF_TRACK_2: u8 = 0x08;

/// VHF track value for freq < 300 MHz (track 1).
const VHF_TRACK_1: u8 = 0x04;

/// VHF track value for UHF/GPS (same as track 7).
const VHF_TRACK_UHF_GPS: u8 = 0x1c;

/// VHF track frequency thresholds in Hz.
const VHF_TRACK_THRESH_7: u32 = 177_500_000;
const VHF_TRACK_THRESH_6: u32 = 184_500_000;
const VHF_TRACK_THRESH_5: u32 = 191_500_000;
const VHF_TRACK_THRESH_4: u32 = 198_500_000;
const VHF_TRACK_THRESH_3: u32 = 205_500_000;
const VHF_TRACK_THRESH_2: u32 = 219_500_000;

/// VHF/UHF boundary frequency.
const VHF_UHF_BOUNDARY: u32 = 300_000_000;

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
/// Each entry defines the multiplier used such that `freq * multi < 3.56 GHz`
/// (or 3.8 GHz for the 950 MHz entry).
/// The last entry has no upper bound (used as fallback for freq >= 950 MHz).
const FREQ_DIVIDERS: [FreqDivider; 11] = [
    FreqDivider {
        max_freq: 37_084_000,
        multi: 96,
        reg5: 0x82,
        reg6: 0x00,
    },
    FreqDivider {
        max_freq: 55_625_000,
        multi: 64,
        reg5: 0x02,
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
        reg5: 0x82,
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
        reg5: 0x42,
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
        reg5: 0x22,
        reg6: 0x02,
    },
    FreqDivider {
        max_freq: 593_334_000,
        multi: 6,
        reg5: 0x0a,
        reg6: 0x00,
    },
    FreqDivider {
        max_freq: 950_000_000,
        multi: 4,
        reg5: 0x12,
        reg6: 0x02,
    },
    // Fallback: freq >= 950 MHz
    FreqDivider {
        max_freq: u32::MAX,
        multi: 2,
        reg5: 0x0a,
        reg6: 0x02,
    },
];

// ---------------------------------------------------------------------------
// LNA gain table
// ---------------------------------------------------------------------------

/// LNA gain entry: (gain in tenths of dB, register value).
///
/// From librtlsdr `fc0013_lna_gains[]`. The table is ordered by ascending
/// gain so that `set_lna_gain` can find the first entry >= requested gain.
const LNA_GAINS: [(i32, u8); 24] = [
    (-99, 0x02),
    (-73, 0x03),
    (-65, 0x05),
    (-63, 0x04),
    (-63, 0x00),
    (-60, 0x07),
    (-58, 0x01),
    (-54, 0x06),
    (58, 0x0f),
    (61, 0x0e),
    (63, 0x0d),
    (65, 0x0c),
    (67, 0x0b),
    (68, 0x0a),
    (70, 0x09),
    (71, 0x08),
    (179, 0x17),
    (181, 0x16),
    (182, 0x15),
    (184, 0x14),
    (186, 0x13),
    (188, 0x12),
    (191, 0x11),
    (197, 0x10),
];

/// Supported gain values in tenths of dB (extracted from `LNA_GAINS`).
///
/// Matches `FC0013_GAINS` in `constants.rs` for use in the tuner type
/// gain table. Deduplicated (the C table has -63 twice).
pub const FC0013_GAINS: [i32; 23] = [
    -99, -73, -65, -63, -60, -58, -54, 58, 61, 63, 65, 67, 68, 70, 71, 179, 181, 182, 184, 186,
    188, 191, 197,
];

// ---------------------------------------------------------------------------
// PLL validation limits
// ---------------------------------------------------------------------------

/// Maximum valid value for register 1 (RF_A) during PLL calculation.
const PLL_REG1_MAX: u8 = 15;

/// Minimum valid value for register 2 (RF_M) during PLL calculation.
const PLL_REG2_MIN: u8 = 0x0b;

// ---------------------------------------------------------------------------
// FC0013 tuner state
// ---------------------------------------------------------------------------

/// Fitipower FC0013 tuner driver.
///
/// Ports `fc0013_init`, `fc0013_set_params`, `fc0013_set_gain_mode`,
/// and `fc0013_set_lna_gain` from `tuner_fc0013.c`.
pub struct Fc0013Tuner {
    /// Crystal oscillator frequency in Hz.
    xtal: u32,
    /// Current bandwidth setting in Hz (stored for `set_freq`).
    bandwidth: u32,
    /// Current tuned frequency in Hz.
    freq: u32,
}

impl Fc0013Tuner {
    /// Create a new FC0013 tuner driver.
    pub fn new(xtal: u32) -> Self {
        Self {
            xtal,
            bandwidth: BW_6MHZ_HZ,
            freq: 0,
        }
    }

    /// Write a single register via I2C.
    ///
    /// Ports `fc0013_writereg`.
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
    /// Ports `fc0013_readreg`.
    #[allow(clippy::unused_self)]
    fn read_reg(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
    ) -> Result<u8, RtlSdrError> {
        usb::i2c_read_reg(handle, I2C_ADDR, reg)
    }

    /// Set VHF tracking filter based on frequency.
    ///
    /// Exact port of `fc0013_set_vhf_track`.
    fn set_vhf_track(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq: u32,
    ) -> Result<(), RtlSdrError> {
        let tmp = self.read_reg(handle, REG_VHF_TRACK)?;
        let tmp = tmp & VHF_TRACK_MASK;

        let track_bits = if freq <= VHF_TRACK_THRESH_7 {
            VHF_TRACK_7
        } else if freq <= VHF_TRACK_THRESH_6 {
            VHF_TRACK_6
        } else if freq <= VHF_TRACK_THRESH_5 {
            VHF_TRACK_5
        } else if freq <= VHF_TRACK_THRESH_4 {
            VHF_TRACK_4
        } else if freq <= VHF_TRACK_THRESH_3 {
            VHF_TRACK_3
        } else if freq <= VHF_TRACK_THRESH_2 {
            VHF_TRACK_2
        } else if freq < VHF_UHF_BOUNDARY {
            VHF_TRACK_1
        } else {
            // UHF and GPS
            VHF_TRACK_UHF_GPS
        };

        self.write_reg(handle, REG_VHF_TRACK, tmp | track_bits)
    }

    /// Set tuner frequency and bandwidth.
    ///
    /// Exact port of `fc0013_set_params`.
    #[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
    fn set_params(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq: u32,
        bandwidth: u32,
    ) -> Result<(), RtlSdrError> {
        let xtal_freq_div_2 = self.xtal / 2;

        // Set VHF track
        self.set_vhf_track(handle, freq)?;

        // VHF/UHF/GPS band selection
        if freq < VHF_UHF_BOUNDARY {
            // Enable VHF filter
            let tmp = self.read_reg(handle, REG_XTAL_SPEED)?;
            self.write_reg(handle, REG_XTAL_SPEED, tmp | VHF_FILTER_ENABLE_BIT)?;

            // Disable UHF & disable GPS
            let tmp = self.read_reg(handle, REG_LNA_GAIN)?;
            self.write_reg(handle, REG_LNA_GAIN, tmp & BAND_SELECT_MASK)?;
        } else {
            // freq >= 300 MHz (UHF or GPS): same handling for both <=862MHz and >862MHz
            // Disable VHF filter
            let tmp = self.read_reg(handle, REG_XTAL_SPEED)?;
            self.write_reg(handle, REG_XTAL_SPEED, tmp & VHF_FILTER_DISABLE_MASK)?;

            // Enable UHF & disable GPS
            let tmp = self.read_reg(handle, REG_LNA_GAIN)?;
            self.write_reg(
                handle,
                REG_LNA_GAIN,
                (tmp & BAND_SELECT_MASK) | UHF_ENABLE_BIT,
            )?;
        }

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
                "FC0013: no valid PLL combination found for {freq} Hz"
            )));
        }

        // Fix clock out
        reg6 |= CLOCK_OUT_BIT;

        // Calculate XIN (fractional part of Delta Sigma PLL)
        // In C, `xin << 15` promotes the uint16_t to int (32-bit) before shifting,
        // so the shift and division must be done in u32 to avoid u16 overflow.
        let xin_remainder =
            (f_vco - (f_vco / u64::from(xtal_freq_div_2)) * u64::from(xtal_freq_div_2)) / 1000;
        debug_assert!(
            xin_remainder <= 0xFFFF,
            "XIN remainder exceeds u16 range: {xin_remainder}"
        );
        let xin_wide = (u32::from(xin_remainder as u16) << 15) / (xtal_freq_div_2 / 1000);
        let mut xin = xin_wide as u16;
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

        // Multi=64 special case: set bit 2 of register 0x11
        let tmp = self.read_reg(handle, REG_MULTI_SELECT)?;
        if multi == 64 {
            self.write_reg(handle, REG_MULTI_SELECT, tmp | MULTI_64_BIT)?;
        } else {
            self.write_reg(handle, REG_MULTI_SELECT, tmp & MULTI_64_DISABLE_MASK)?;
        }

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

    /// Add an offset to the RC calibration value.
    ///
    /// Exact port of `fc0013_rc_cal_add`. Currently unused but available
    /// for future RC filter recalibration support.
    #[allow(dead_code)]
    pub(crate) fn rc_cal_add(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        rc_val: i32,
    ) -> Result<(), RtlSdrError> {
        // Push rc_cal value, then read it back
        self.write_reg(handle, REG_RC_CAL, 0x00)?;

        let rc_cal = self.read_reg(handle, REG_RC_CAL)?;
        let rc_cal = i32::from(rc_cal & RC_CAL_MAX);

        let val = rc_cal + rc_val;

        // Force rc_cal
        self.write_reg(handle, REG_AGC_LNA_FORCE, RC_CAL_FORCE)?;

        // Modify rc_cal value (clamped to 0..15)
        if val > i32::from(RC_CAL_MAX) {
            self.write_reg(handle, REG_RC_CAL, RC_CAL_MAX)?;
        } else if val < 0 {
            self.write_reg(handle, REG_RC_CAL, 0x00)?;
        } else {
            self.write_reg(handle, REG_RC_CAL, val as u8)?;
        }

        Ok(())
    }

    /// Reset the RC calibration.
    ///
    /// Exact port of `fc0013_rc_cal_reset`. Currently unused but available
    /// for future RC filter recalibration support.
    #[allow(dead_code)]
    pub(crate) fn rc_cal_reset(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        self.write_reg(handle, REG_AGC_LNA_FORCE, RC_CAL_RESET)?;
        self.write_reg(handle, REG_RC_CAL, 0x00)?;
        Ok(())
    }
}

impl Tuner for Fc0013Tuner {
    /// Initialize the FC0013 tuner.
    ///
    /// Exact port of `fc0013_init`.
    fn init(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
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
    /// The FC0013 C driver does not define an explicit exit/standby function.
    /// Power down the LNA by setting bit 0 of register 0x06.
    fn exit(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
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

    /// Set the tuner bandwidth. Returns 0 as the IF frequency (the FC0013
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
        // FC0013 does not have a separate IF frequency to report
        Ok(0)
    }

    /// Set the LNA gain.
    ///
    /// Exact port of `fc0013_set_lna_gain`. Gain is in tenths of dB.
    fn set_gain(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        gain: i32,
    ) -> Result<(), RtlSdrError> {
        let mut tmp = self.read_reg(handle, REG_LNA_GAIN)?;

        // Mask off LNA gain bits (keep bits 5-7)
        tmp &= LNA_GAIN_MASK;

        // Find the first entry whose gain >= requested, or use the last entry
        for (i, &(entry_gain, reg_val)) in LNA_GAINS.iter().enumerate() {
            if entry_gain >= gain || i + 1 == LNA_GAINS.len() {
                tmp |= reg_val;
                break;
            }
        }

        self.write_reg(handle, REG_LNA_GAIN, tmp)?;

        Ok(())
    }

    /// Update the crystal frequency.
    fn set_xtal(&mut self, xtal: u32) {
        self.xtal = xtal;
    }

    /// Set manual or automatic gain mode.
    ///
    /// Exact port of `fc0013_set_gain_mode`.
    fn set_gain_mode(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        manual: bool,
    ) -> Result<(), RtlSdrError> {
        let mut tmp = self.read_reg(handle, REG_AGC_LNA_FORCE)?;

        if manual {
            tmp |= MANUAL_GAIN_BIT;
        } else {
            tmp &= !MANUAL_GAIN_BIT;
        }

        self.write_reg(handle, REG_AGC_LNA_FORCE, tmp)?;

        // Set a fixed IF gain
        self.write_reg(handle, REG_IF_GAIN, FIXED_IF_GAIN)?;

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
        assert_eq!(FREQ_DIVIDERS[FREQ_DIVIDERS.len() - 1].max_freq, u32::MAX);
    }

    #[test]
    fn test_gains_sorted() {
        for w in FC0013_GAINS.windows(2) {
            assert!(w[0] <= w[1], "gains must be in ascending order");
        }
    }

    #[test]
    fn test_gains_count() {
        // 24 entries in C source, but -63 appears twice; deduplicated to 23
        assert_eq!(FC0013_GAINS.len(), 23);
    }

    #[test]
    fn test_lna_gains_count() {
        // Full table from C source has 24 entries (including duplicate -63)
        assert_eq!(LNA_GAINS.len(), 24);
    }

    #[test]
    fn test_lna_gains_match_c_source() {
        // Verify first, last, and a few middle entries
        assert_eq!(LNA_GAINS[0], (-99, 0x02));
        assert_eq!(LNA_GAINS[1], (-73, 0x03));
        assert_eq!(LNA_GAINS[3], (-63, 0x04));
        assert_eq!(LNA_GAINS[4], (-63, 0x00));
        assert_eq!(LNA_GAINS[8], (58, 0x0f));
        assert_eq!(LNA_GAINS[15], (71, 0x08));
        assert_eq!(LNA_GAINS[16], (179, 0x17));
        assert_eq!(LNA_GAINS[23], (197, 0x10));
    }

    #[test]
    fn test_i2c_addr() {
        assert_eq!(I2C_ADDR, 0xc6);
    }

    #[test]
    fn test_check_val() {
        assert_eq!(CHECK_VAL, 0xa3);
    }

    #[test]
    fn test_new_defaults() {
        let tuner = Fc0013Tuner::new(28_800_000);
        assert_eq!(tuner.xtal, 28_800_000);
        assert_eq!(tuner.bandwidth, BW_6MHZ_HZ);
        assert_eq!(tuner.freq, 0);
    }

    #[test]
    fn test_init_regs_xtal_bit() {
        let mut regs = INIT_REGS;
        regs[6] |= XTAL_28_8_MHZ_BIT;
        assert_eq!(regs[6] & XTAL_28_8_MHZ_BIT, XTAL_28_8_MHZ_BIT);
    }

    #[test]
    fn test_init_regs_dual_master_bit() {
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

        // 700 MHz should select multi=4
        let divider = FREQ_DIVIDERS
            .iter()
            .find(|d| 700_000_000 < d.max_freq)
            .expect("should find divider for 700 MHz");
        assert_eq!(divider.multi, 4);

        // 1 GHz should select multi=2 (fallback)
        let divider = FREQ_DIVIDERS
            .iter()
            .find(|d| 1_000_000_000 < d.max_freq)
            .expect("should find divider for 1 GHz");
        assert_eq!(divider.multi, 2);
    }

    #[test]
    fn test_fc0013_differs_from_fc0012_dividers() {
        // FC0013 has 11 entries (FC0012 has 10), including multi=2 fallback
        assert_eq!(FREQ_DIVIDERS.len(), 11);

        // FC0013 has multi=2 in its last entry (FC0012 has multi=4)
        assert_eq!(FREQ_DIVIDERS[FREQ_DIVIDERS.len() - 1].multi, 2);

        // FC0013 has multi=4 with max_freq=950_000_000 (not u32::MAX)
        assert_eq!(FREQ_DIVIDERS[9].multi, 4);
        assert_eq!(FREQ_DIVIDERS[9].max_freq, 950_000_000);
    }

    #[test]
    fn test_vhf_track_thresholds() {
        // Verify all VHF track thresholds match C source
        assert_eq!(VHF_TRACK_THRESH_7, 177_500_000);
        assert_eq!(VHF_TRACK_THRESH_6, 184_500_000);
        assert_eq!(VHF_TRACK_THRESH_5, 191_500_000);
        assert_eq!(VHF_TRACK_THRESH_4, 198_500_000);
        assert_eq!(VHF_TRACK_THRESH_3, 205_500_000);
        assert_eq!(VHF_TRACK_THRESH_2, 219_500_000);
    }

    #[test]
    fn test_init_reg_values_match_c_source() {
        // Spot-check init register values against C source
        assert_eq!(INIT_REGS[0], 0x09); // reg 0x01
        assert_eq!(INIT_REGS[1], 0x16); // reg 0x02
        assert_eq!(INIT_REGS[5], 0x02); // reg 0x06: LPF bandwidth
        assert_eq!(INIT_REGS[6], 0x0a); // reg 0x07: CHECK
        assert_eq!(INIT_REGS[7], 0xff); // reg 0x08
        assert_eq!(INIT_REGS[8], 0x6e); // reg 0x09
        assert_eq!(INIT_REGS[11], 0xfc); // reg 0x0c
        assert_eq!(INIT_REGS[12], 0x01); // reg 0x0d
        assert_eq!(INIT_REGS[19], 0x50); // reg 0x14
        assert_eq!(INIT_REGS[20], 0x01); // reg 0x15
    }
}
