//! FCI FC2580 tuner driver.
//!
//! Faithful port of `tuner_fc2580.c` from librtlsdr.
//!
//! Original copyright:
//! - FCI FC2580 tuner driver, taken from the kernel driver that can be found
//!   on <http://linux.terratec.de/tv_en.html>

use crate::error::RtlSdrError;
use crate::tuner::Tuner;
use crate::usb;

// ---------------------------------------------------------------------------
// I2C address and identification
// ---------------------------------------------------------------------------

/// FC2580 I2C address.
pub const I2C_ADDR: u8 = 0xac;

/// Register address used to identify the FC2580 (chip ID register).
pub const CHECK_ADDR: u8 = 0x01;

/// Expected chip ID value read from `CHECK_ADDR`.
pub const CHECK_VAL: u8 = 0x56;

// ---------------------------------------------------------------------------
// Crystal and clock constants
// ---------------------------------------------------------------------------

/// Crystal oscillator frequency in Hz (16.384 MHz, at least on the Logilink VG0002A).
const CRYSTAL_FREQ: u32 = 16_384_000;

/// Use external clock input (0 = internal XTAL oscillator, 1 = external clock).
const USE_EXT_CLK: u8 = 0;

/// VCO border frequency in kHz: determines whether low or high VCO is used.
/// 2.6 GHz = 2_600_000 kHz.
const BORDER_FREQ: u32 = 2_600_000;

// ---------------------------------------------------------------------------
// AGC mode constants
// ---------------------------------------------------------------------------

/// Internal AGC mode.
const AGC_INTERNAL: i32 = 1;

/// External (voltage control) AGC mode.
const AGC_EXTERNAL: i32 = 2;

// ---------------------------------------------------------------------------
// Bandwidth mode constants
// ---------------------------------------------------------------------------

/// 1.53 MHz bandwidth (TDMB).
const FILTER_BW_1_53MHZ: u8 = 1;

/// 6 MHz bandwidth.
const FILTER_BW_6MHZ: u8 = 6;

/// 6.8 MHz bandwidth (7 MHz mode).
const FILTER_BW_7MHZ: u8 = 7;

/// 7.8 MHz bandwidth (8 MHz mode).
const FILTER_BW_8MHZ: u8 = 8;

// ---------------------------------------------------------------------------
// Bandwidth in Hz (for set_bw mapping)
// ---------------------------------------------------------------------------

/// Bandwidth: 1.53 MHz in Hz.
const BW_1_53MHZ_HZ: u32 = 1_530_000;

/// Bandwidth: 6 MHz in Hz.
const BW_6MHZ_HZ: u32 = 6_000_000;

/// Bandwidth: 7 MHz in Hz.
const BW_7MHZ_HZ: u32 = 7_000_000;

// Note: 8 MHz is the default/fallback in set_bw, so BW_8MHZ_HZ is not needed
// as a named constant (any value not matching 1.53/6/7 MHz maps to filter mode 8).

// ---------------------------------------------------------------------------
// Band type
// ---------------------------------------------------------------------------

/// Frequency band classification for the FC2580 tuner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Band {
    /// VHF band: f_lo <= 400 MHz.
    Vhf,
    /// UHF band: 400 MHz < f_lo <= 1000 MHz.
    Uhf,
    /// L-Band: f_lo > 1000 MHz.
    #[allow(clippy::enum_variant_names)]
    LBand,
}

// ---------------------------------------------------------------------------
// UHF sub-band frequency thresholds (in kHz)
// ---------------------------------------------------------------------------

/// UHF sub-band threshold: below this, use low-frequency UHF register config.
const UHF_LOW_THRESH_KHZ: u32 = 538_000;

/// UHF sub-band threshold: below this, use mid-frequency UHF register config.
const UHF_MID_THRESH_KHZ: u32 = 794_000;

/// UHF LNA load cap threshold (kHz): at or below this, use 0x9F; above, use 0x8F.
const UHF_LNA_CAP_THRESH_KHZ: u32 = 794_000;

// ---------------------------------------------------------------------------
// Init register addresses and values
// ---------------------------------------------------------------------------

const REG_00: u8 = 0x00;
const REG_02: u8 = 0x02;
const REG_09: u8 = 0x09;
const REG_0B: u8 = 0x0B;
const REG_0C: u8 = 0x0C;
const REG_0E: u8 = 0x0E;
const REG_12: u8 = 0x12;
const REG_14: u8 = 0x14;
const REG_16: u8 = 0x16;
const REG_18: u8 = 0x18;
const REG_1A: u8 = 0x1A;
const REG_1B: u8 = 0x1B;
const REG_1C: u8 = 0x1C;
const REG_1F: u8 = 0x1F;
const REG_21: u8 = 0x21;
const REG_22: u8 = 0x22;
const REG_25: u8 = 0x25;
const REG_27: u8 = 0x27;
const REG_28: u8 = 0x28;
const REG_29: u8 = 0x29;
const REG_2B: u8 = 0x2B;
const REG_2C: u8 = 0x2C;
const REG_2D: u8 = 0x2D;
const REG_2E: u8 = 0x2E;
const REG_2F: u8 = 0x2F;
const REG_30: u8 = 0x30;
const REG_36: u8 = 0x36;
const REG_37: u8 = 0x37;
const REG_39: u8 = 0x39;
const REG_3F: u8 = 0x3F;
const REG_44: u8 = 0x44;
const REG_45: u8 = 0x45;
const REG_4B: u8 = 0x4B;
const REG_4C: u8 = 0x4C;
const REG_50: u8 = 0x50;
const REG_53: u8 = 0x53;
const REG_58: u8 = 0x58;
const REG_5F: u8 = 0x5F;
const REG_61: u8 = 0x61;
const REG_62: u8 = 0x62;
const REG_63: u8 = 0x63;
const REG_67: u8 = 0x67;
const REG_68: u8 = 0x68;
const REG_69: u8 = 0x69;
const REG_6A: u8 = 0x6A;
const REG_6B: u8 = 0x6B;
const REG_6C: u8 = 0x6C;
const REG_6D: u8 = 0x6D;
const REG_6E: u8 = 0x6E;
const REG_6F: u8 = 0x6F;

// ---------------------------------------------------------------------------
// Init register values
// ---------------------------------------------------------------------------

const INIT_REG_00_VAL: u8 = 0x00;
const INIT_REG_12_VAL: u8 = 0x86;
const INIT_REG_14_VAL: u8 = 0x5C;
const INIT_REG_16_VAL: u8 = 0x3C;
const INIT_REG_1F_VAL: u8 = 0xD2;
const INIT_REG_09_VAL: u8 = 0xD7;
const INIT_REG_0B_VAL: u8 = 0xD5;
const INIT_REG_0C_VAL: u8 = 0x32;
const INIT_REG_0E_VAL: u8 = 0x43;
const INIT_REG_21_VAL: u8 = 0x0A;
const INIT_REG_22_VAL: u8 = 0x82;
const INIT_REG_3F_VAL: u8 = 0x88;
const INIT_REG_02_VAL: u8 = 0x0E;
const INIT_REG_58_VAL: u8 = 0x14;

// AGC register values
const AGC_INTERNAL_REG_45: u8 = 0x10;
const AGC_INTERNAL_REG_4C: u8 = 0x00;
const AGC_EXTERNAL_REG_45: u8 = 0x20;
const AGC_EXTERNAL_REG_4C: u8 = 0x02;

// ---------------------------------------------------------------------------
// Filter calibration constants
// ---------------------------------------------------------------------------

/// Filter calibration monitor register.
const FILTER_CAL_MON_MASK: u8 = 0xC0;

/// Expected calibration complete value.
const FILTER_CAL_COMPLETE: u8 = 0xC0;

/// Filter calibration trigger value.
const FILTER_CAL_TRIGGER: u8 = 0x09;

/// Filter calibration reset value.
const FILTER_CAL_RESET: u8 = 0x01;

/// Maximum number of filter calibration retries.
const FILTER_CAL_MAX_RETRIES: u8 = 5;

// ---------------------------------------------------------------------------
// Filter bandwidth register values
// ---------------------------------------------------------------------------

// BW 1.53 MHz
const FILTER_1_53_REG_36: u8 = 0x1C;
const FILTER_1_53_COEFF: u32 = 4151;
const FILTER_1_53_REG_39: u8 = 0x00;

// BW 6 MHz
const FILTER_6_REG_36: u8 = 0x18;
const FILTER_6_COEFF: u32 = 4400;
const FILTER_6_REG_39: u8 = 0x00;

// BW 7 MHz (6.8 MHz)
const FILTER_7_REG_36: u8 = 0x18;
const FILTER_7_COEFF: u32 = 3910;
const FILTER_7_REG_39: u8 = 0x80;

// BW 8 MHz (7.8 MHz)
const FILTER_8_REG_36: u8 = 0x18;
const FILTER_8_COEFF: u32 = 3300;
const FILTER_8_REG_39: u8 = 0x80;

// ---------------------------------------------------------------------------
// PLL register constants
// ---------------------------------------------------------------------------

/// VCO band select bit in register 0x02.
const VCO_BAND_SELECT_BIT: u8 = 0x08;

/// Mask to clear band bits in register 0x02 (clear bits 6-7).
const BAND_CLEAR_MASK: u8 = 0x3F;

/// VHF band bit in register 0x02 (bit 7).
const VHF_BAND_BIT: u8 = 0x80;

/// L-Band bit in register 0x02 (bit 6).
const L_BAND_BIT: u8 = 0x40;

/// R divider value encoding for R=1 in register 0x18.
const R_VAL_1_BITS: u8 = 0x00;

/// R divider value encoding for R=2 in register 0x18.
const R_VAL_2_BITS: u8 = 0x10;

/// R divider value encoding for R=4 in register 0x18.
const R_VAL_4_BITS: u8 = 0x20;

/// Pre-shift bits to prevent overflow when computing k_val.
const PLL_PRE_SHIFT_BITS: u8 = 4;

/// K value shift amount (20 - pre_shift_bits = 16).
const PLL_K_SHIFT: u8 = 20 - PLL_PRE_SHIFT_BITS;

// ---------------------------------------------------------------------------
// UHF band register values
// ---------------------------------------------------------------------------

const UHF_REG_25: u8 = 0xF0;
const UHF_REG_27: u8 = 0x77;
const UHF_REG_28: u8 = 0x53;
const UHF_REG_29: u8 = 0x60;
const UHF_REG_30: u8 = 0x09;
const UHF_REG_50: u8 = 0x8C;
const UHF_REG_53: u8 = 0x50;

// UHF sub-band: low (< 538 MHz)
const UHF_LOW_REG_5F: u8 = 0x13;
const UHF_LOW_REG_61: u8 = 0x07;
const UHF_LOW_REG_62: u8 = 0x06;
const UHF_LOW_REG_67: u8 = 0x06;
const UHF_LOW_REG_68: u8 = 0x08;
const UHF_LOW_REG_69: u8 = 0x10;
const UHF_LOW_REG_6A: u8 = 0x12;

// UHF sub-band: mid (538-794 MHz)
const UHF_MID_REG_61: u8 = 0x03;
const UHF_MID_REG_62: u8 = 0x03;
const UHF_MID_REG_67: u8 = 0x03;
const UHF_MID_REG_68: u8 = 0x05;
const UHF_MID_REG_69: u8 = 0x0C;
const UHF_MID_REG_6A: u8 = 0x0E;

// UHF sub-band: high (>= 794 MHz)
const UHF_HIGH_REG_5F: u8 = 0x15;
const UHF_HIGH_REG_61: u8 = 0x07;
const UHF_HIGH_REG_62: u8 = 0x06;
const UHF_HIGH_REG_67: u8 = 0x07;
const UHF_HIGH_REG_68: u8 = 0x09;
const UHF_HIGH_REG_69: u8 = 0x10;
const UHF_HIGH_REG_6A: u8 = 0x12;

const UHF_REG_63: u8 = 0x15;
const UHF_REG_6B: u8 = 0x0B;
const UHF_REG_6C: u8 = 0x0C;
const UHF_REG_6D: u8 = 0x78;
const UHF_REG_6E: u8 = 0x32;
const UHF_REG_6F: u8 = 0x14;

// ---------------------------------------------------------------------------
// VHF band register values
// ---------------------------------------------------------------------------

const VHF_REG_27: u8 = 0x77;
const VHF_REG_28: u8 = 0x33;
const VHF_REG_29: u8 = 0x40;
const VHF_REG_30: u8 = 0x09;
const VHF_REG_50: u8 = 0x8C;
const VHF_REG_53: u8 = 0x50;
const VHF_REG_5F: u8 = 0x0F;
const VHF_REG_61: u8 = 0x07;
const VHF_REG_62: u8 = 0x00;
const VHF_REG_63: u8 = 0x15;
const VHF_REG_67: u8 = 0x03;
const VHF_REG_68: u8 = 0x05;
const VHF_REG_69: u8 = 0x10;
const VHF_REG_6A: u8 = 0x12;
const VHF_REG_6B: u8 = 0x08;
const VHF_REG_6C: u8 = 0x0A;
const VHF_REG_6D: u8 = 0x78;
const VHF_REG_6E: u8 = 0x32;
const VHF_REG_6F: u8 = 0x54;

// ---------------------------------------------------------------------------
// L-Band register values
// ---------------------------------------------------------------------------

const LBAND_REG_2B: u8 = 0x70;
const LBAND_REG_2C: u8 = 0x37;
const LBAND_REG_2D: u8 = 0xE7;
const LBAND_REG_30: u8 = 0x09;
const LBAND_REG_44: u8 = 0x20;
const LBAND_REG_50: u8 = 0x8C;
const LBAND_REG_53: u8 = 0x50;
const LBAND_REG_5F: u8 = 0x0F;
const LBAND_REG_61: u8 = 0x0F;
const LBAND_REG_62: u8 = 0x00;
const LBAND_REG_63: u8 = 0x13;
const LBAND_REG_67: u8 = 0x00;
const LBAND_REG_68: u8 = 0x02;
const LBAND_REG_69: u8 = 0x0C;
const LBAND_REG_6A: u8 = 0x0E;
const LBAND_REG_6B: u8 = 0x08;
const LBAND_REG_6C: u8 = 0x0A;
const LBAND_REG_6D: u8 = 0xA0;
const LBAND_REG_6E: u8 = 0x50;
const LBAND_REG_6F: u8 = 0x14;

// ---------------------------------------------------------------------------
// AGC clock pre-divide ratio threshold (kHz)
// ---------------------------------------------------------------------------

/// Crystal frequency threshold for AGC clock pre-divide ratio.
const AGC_CLK_PREDIV_THRESH_KHZ: u32 = 28_000;

/// AGC clock pre-divide register value.
const AGC_CLK_PREDIV_VAL: u8 = 0x22;

// ---------------------------------------------------------------------------
// UHF LNA load cap values
// ---------------------------------------------------------------------------

/// LNA load cap value for low UHF frequencies (<= 794 MHz).
const LNA_CAP_LOW: u8 = 0x9F;

/// LNA load cap value for high UHF frequencies (> 794 MHz).
const LNA_CAP_HIGH: u8 = 0x8F;

// ---------------------------------------------------------------------------
// Gain table
// ---------------------------------------------------------------------------

/// Supported gain values in tenths of dB.
///
/// The FC2580 has no gain control in the original C driver (empty gain table).
pub const FC2580_GAINS: [i32; 1] = [0];

// ---------------------------------------------------------------------------
// FC2580 tuner state
// ---------------------------------------------------------------------------

/// FCI FC2580 tuner driver.
///
/// Ports the `fc2580_set_init`, `fc2580_set_freq`, and `fc2580_set_filter`
/// functions from `tuner_fc2580.c`.
pub struct Fc2580Tuner {
    /// Crystal oscillator frequency in Hz.
    xtal: u32,
    /// Current tuned frequency in Hz.
    freq: u32,
}

impl Fc2580Tuner {
    /// Create a new FC2580 tuner driver.
    pub fn new(xtal: u32) -> Self {
        Self { xtal, freq: 0 }
    }

    /// Write a single register via I2C.
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
    #[allow(clippy::unused_self)]
    fn read_reg(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
    ) -> Result<u8, RtlSdrError> {
        usb::i2c_read_reg(handle, I2C_ADDR, reg)
    }

    /// Determine the band for a given frequency in kHz.
    fn classify_band(f_lo_khz: u32) -> Band {
        if f_lo_khz > 1_000_000 {
            Band::LBand
        } else if f_lo_khz > 400_000 {
            Band::Uhf
        } else {
            Band::Vhf
        }
    }

    /// Set the channel selection filter bandwidth.
    ///
    /// Exact port of `fc2580_set_filter`.
    #[allow(clippy::cast_possible_truncation)]
    fn set_filter(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        filter_bw: u8,
        freq_xtal_khz: u32,
    ) -> Result<(), RtlSdrError> {
        match filter_bw {
            FILTER_BW_1_53MHZ => {
                self.write_reg(handle, REG_36, FILTER_1_53_REG_36)?;
                self.write_reg(
                    handle,
                    REG_37,
                    (FILTER_1_53_COEFF * freq_xtal_khz / 1_000_000) as u8,
                )?;
                self.write_reg(handle, REG_39, FILTER_1_53_REG_39)?;
                self.write_reg(handle, REG_2E, FILTER_CAL_TRIGGER)?;
            }
            FILTER_BW_6MHZ => {
                self.write_reg(handle, REG_36, FILTER_6_REG_36)?;
                self.write_reg(
                    handle,
                    REG_37,
                    (FILTER_6_COEFF * freq_xtal_khz / 1_000_000) as u8,
                )?;
                self.write_reg(handle, REG_39, FILTER_6_REG_39)?;
                self.write_reg(handle, REG_2E, FILTER_CAL_TRIGGER)?;
            }
            FILTER_BW_7MHZ => {
                self.write_reg(handle, REG_36, FILTER_7_REG_36)?;
                self.write_reg(
                    handle,
                    REG_37,
                    (FILTER_7_COEFF * freq_xtal_khz / 1_000_000) as u8,
                )?;
                self.write_reg(handle, REG_39, FILTER_7_REG_39)?;
                self.write_reg(handle, REG_2E, FILTER_CAL_TRIGGER)?;
            }
            FILTER_BW_8MHZ => {
                self.write_reg(handle, REG_36, FILTER_8_REG_36)?;
                self.write_reg(
                    handle,
                    REG_37,
                    (FILTER_8_COEFF * freq_xtal_khz / 1_000_000) as u8,
                )?;
                self.write_reg(handle, REG_39, FILTER_8_REG_39)?;
                self.write_reg(handle, REG_2E, FILTER_CAL_TRIGGER)?;
            }
            _ => {
                return Err(RtlSdrError::Tuner(format!(
                    "FC2580: unsupported filter bandwidth mode {filter_bw}"
                )));
            }
        }

        // Filter calibration: poll up to 5 times, re-trigger if not complete
        for _ in 0..FILTER_CAL_MAX_RETRIES {
            // USB latency serves as the wait (original C: fc2580_wait_msec 5ms)
            let cal_mon = self.read_reg(handle, REG_2F)?;
            if (cal_mon & FILTER_CAL_MON_MASK) == FILTER_CAL_COMPLETE {
                break;
            }
            self.write_reg(handle, REG_2E, FILTER_CAL_RESET)?;
            self.write_reg(handle, REG_2E, FILTER_CAL_TRIGGER)?;
        }

        self.write_reg(handle, REG_2E, FILTER_CAL_RESET)?;

        Ok(())
    }

    /// Write UHF band registers. Extracted from `set_freq_internal`.
    fn write_uhf_band_regs(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        f_lo_khz: u32,
        freq_xtal_khz: u32,
    ) -> Result<(), RtlSdrError> {
        self.write_reg(handle, REG_25, UHF_REG_25)?;
        self.write_reg(handle, REG_27, UHF_REG_27)?;
        self.write_reg(handle, REG_28, UHF_REG_28)?;
        self.write_reg(handle, REG_29, UHF_REG_29)?;
        self.write_reg(handle, REG_30, UHF_REG_30)?;
        self.write_reg(handle, REG_50, UHF_REG_50)?;
        self.write_reg(handle, REG_53, UHF_REG_53)?;

        if f_lo_khz < UHF_LOW_THRESH_KHZ {
            self.write_reg(handle, REG_5F, UHF_LOW_REG_5F)?;
        } else {
            self.write_reg(handle, REG_5F, UHF_HIGH_REG_5F)?;
        }

        if f_lo_khz < UHF_LOW_THRESH_KHZ {
            self.write_reg(handle, REG_61, UHF_LOW_REG_61)?;
            self.write_reg(handle, REG_62, UHF_LOW_REG_62)?;
            self.write_reg(handle, REG_67, UHF_LOW_REG_67)?;
            self.write_reg(handle, REG_68, UHF_LOW_REG_68)?;
            self.write_reg(handle, REG_69, UHF_LOW_REG_69)?;
            self.write_reg(handle, REG_6A, UHF_LOW_REG_6A)?;
        } else if f_lo_khz < UHF_MID_THRESH_KHZ {
            self.write_reg(handle, REG_61, UHF_MID_REG_61)?;
            self.write_reg(handle, REG_62, UHF_MID_REG_62)?;
            self.write_reg(handle, REG_67, UHF_MID_REG_67)?;
            self.write_reg(handle, REG_68, UHF_MID_REG_68)?;
            self.write_reg(handle, REG_69, UHF_MID_REG_69)?;
            self.write_reg(handle, REG_6A, UHF_MID_REG_6A)?;
        } else {
            self.write_reg(handle, REG_61, UHF_HIGH_REG_61)?;
            self.write_reg(handle, REG_62, UHF_HIGH_REG_62)?;
            self.write_reg(handle, REG_67, UHF_HIGH_REG_67)?;
            self.write_reg(handle, REG_68, UHF_HIGH_REG_68)?;
            self.write_reg(handle, REG_69, UHF_HIGH_REG_69)?;
            self.write_reg(handle, REG_6A, UHF_HIGH_REG_6A)?;
        }

        self.write_reg(handle, REG_63, UHF_REG_63)?;
        self.write_reg(handle, REG_6B, UHF_REG_6B)?;
        self.write_reg(handle, REG_6C, UHF_REG_6C)?;
        self.write_reg(handle, REG_6D, UHF_REG_6D)?;
        self.write_reg(handle, REG_6E, UHF_REG_6E)?;
        self.write_reg(handle, REG_6F, UHF_REG_6F)?;
        self.set_filter(handle, FILTER_BW_8MHZ, freq_xtal_khz)
    }

    /// Write VHF band registers. Extracted from `set_freq_internal`.
    fn write_vhf_band_regs(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq_xtal_khz: u32,
    ) -> Result<(), RtlSdrError> {
        self.write_reg(handle, REG_27, VHF_REG_27)?;
        self.write_reg(handle, REG_28, VHF_REG_28)?;
        self.write_reg(handle, REG_29, VHF_REG_29)?;
        self.write_reg(handle, REG_30, VHF_REG_30)?;
        self.write_reg(handle, REG_50, VHF_REG_50)?;
        self.write_reg(handle, REG_53, VHF_REG_53)?;
        self.write_reg(handle, REG_5F, VHF_REG_5F)?;
        self.write_reg(handle, REG_61, VHF_REG_61)?;
        self.write_reg(handle, REG_62, VHF_REG_62)?;
        self.write_reg(handle, REG_63, VHF_REG_63)?;
        self.write_reg(handle, REG_67, VHF_REG_67)?;
        self.write_reg(handle, REG_68, VHF_REG_68)?;
        self.write_reg(handle, REG_69, VHF_REG_69)?;
        self.write_reg(handle, REG_6A, VHF_REG_6A)?;
        self.write_reg(handle, REG_6B, VHF_REG_6B)?;
        self.write_reg(handle, REG_6C, VHF_REG_6C)?;
        self.write_reg(handle, REG_6D, VHF_REG_6D)?;
        self.write_reg(handle, REG_6E, VHF_REG_6E)?;
        self.write_reg(handle, REG_6F, VHF_REG_6F)?;
        self.set_filter(handle, FILTER_BW_7MHZ, freq_xtal_khz)
    }

    /// Write L-Band registers. Extracted from `set_freq_internal`.
    fn write_lband_regs(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq_xtal_khz: u32,
    ) -> Result<(), RtlSdrError> {
        self.write_reg(handle, REG_2B, LBAND_REG_2B)?;
        self.write_reg(handle, REG_2C, LBAND_REG_2C)?;
        self.write_reg(handle, REG_2D, LBAND_REG_2D)?;
        self.write_reg(handle, REG_30, LBAND_REG_30)?;
        self.write_reg(handle, REG_44, LBAND_REG_44)?;
        self.write_reg(handle, REG_50, LBAND_REG_50)?;
        self.write_reg(handle, REG_53, LBAND_REG_53)?;
        self.write_reg(handle, REG_5F, LBAND_REG_5F)?;
        self.write_reg(handle, REG_61, LBAND_REG_61)?;
        self.write_reg(handle, REG_62, LBAND_REG_62)?;
        self.write_reg(handle, REG_63, LBAND_REG_63)?;
        self.write_reg(handle, REG_67, LBAND_REG_67)?;
        self.write_reg(handle, REG_68, LBAND_REG_68)?;
        self.write_reg(handle, REG_69, LBAND_REG_69)?;
        self.write_reg(handle, REG_6A, LBAND_REG_6A)?;
        self.write_reg(handle, REG_6B, LBAND_REG_6B)?;
        self.write_reg(handle, REG_6C, LBAND_REG_6C)?;
        self.write_reg(handle, REG_6D, LBAND_REG_6D)?;
        self.write_reg(handle, REG_6E, LBAND_REG_6E)?;
        self.write_reg(handle, REG_6F, LBAND_REG_6F)?;
        self.set_filter(handle, FILTER_BW_1_53MHZ, freq_xtal_khz)
    }

    /// Set the LO frequency.
    ///
    /// Exact port of `fc2580_set_freq`. `f_lo_khz` is in kHz, `freq_xtal_khz` is in kHz.
    #[allow(clippy::cast_possible_truncation)]
    fn set_freq_internal(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        f_lo_khz: u32,
        freq_xtal_khz: u32,
    ) -> Result<(), RtlSdrError> {
        let band = Self::classify_band(f_lo_khz);

        // Calculate VCO frequency
        let f_vco: u32 = match band {
            Band::Uhf => f_lo_khz * 4,
            Band::LBand => f_lo_khz * 2,
            Band::Vhf => f_lo_khz * 12,
        };

        // Calculate R divider
        let r_val: u32 = if f_vco >= 2 * 76 * freq_xtal_khz {
            1
        } else if f_vco >= 76 * freq_xtal_khz {
            2
        } else {
            4
        };

        let f_comp = freq_xtal_khz / r_val;
        let n_val = (f_vco / 2) / f_comp;

        let f_diff = f_vco - 2 * f_comp * n_val;
        let f_diff_shifted = f_diff << PLL_K_SHIFT;
        let f_comp_shifted = (2 * f_comp) >> PLL_PRE_SHIFT_BITS;
        let mut k_val = f_diff_shifted / f_comp_shifted;

        if f_diff_shifted - k_val * f_comp_shifted >= (f_comp >> PLL_PRE_SHIFT_BITS) {
            k_val += 1;
        }

        // Base value for register 0x02
        let mut data_0x02: u8 = (USE_EXT_CLK << 5) | INIT_REG_02_VAL;

        // Select VCO Band
        if f_vco >= BORDER_FREQ {
            data_0x02 |= VCO_BAND_SELECT_BIT;
        } else {
            data_0x02 &= !VCO_BAND_SELECT_BIT;
        }

        // Band-specific register configuration
        match band {
            Band::Uhf => {
                data_0x02 &= BAND_CLEAR_MASK;
                self.write_uhf_band_regs(handle, f_lo_khz, freq_xtal_khz)?;
            }
            Band::Vhf => {
                data_0x02 = (data_0x02 & BAND_CLEAR_MASK) | VHF_BAND_BIT;
                self.write_vhf_band_regs(handle, freq_xtal_khz)?;
            }
            Band::LBand => {
                data_0x02 = (data_0x02 & BAND_CLEAR_MASK) | L_BAND_BIT;
                self.write_lband_regs(handle, freq_xtal_khz)?;
            }
        }

        // AGC clock pre-divide ratio
        if freq_xtal_khz >= AGC_CLK_PREDIV_THRESH_KHZ {
            self.write_reg(handle, REG_4B, AGC_CLK_PREDIV_VAL)?;
        }

        // VCO Band and PLL setting
        self.write_reg(handle, REG_02, data_0x02)?;

        // Register 0x18: R divider encoding + high part of K value
        let r_bits = match r_val {
            1 => R_VAL_1_BITS,
            2 => R_VAL_2_BITS,
            _ => R_VAL_4_BITS,
        };
        let data_0x18 = r_bits + (k_val >> 16) as u8;
        self.write_reg(handle, REG_18, data_0x18)?;

        // Load middle part of K value
        self.write_reg(handle, REG_1A, (k_val >> 8) as u8)?;

        // Load lower part of K value
        self.write_reg(handle, REG_1B, k_val as u8)?;

        // Load N value
        self.write_reg(handle, REG_1C, n_val as u8)?;

        // UHF LNA Load Cap
        if band == Band::Uhf {
            let lna_cap = if f_lo_khz <= UHF_LNA_CAP_THRESH_KHZ {
                LNA_CAP_LOW
            } else {
                LNA_CAP_HIGH
            };
            self.write_reg(handle, REG_2D, lna_cap)?;
        }

        Ok(())
    }

    /// Perform tuner initialization.
    ///
    /// Exact port of `fc2580_set_init`.
    fn set_init(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        ifagc_mode: i32,
        freq_xtal_khz: u32,
    ) -> Result<(), RtlSdrError> {
        self.write_reg(handle, REG_00, INIT_REG_00_VAL)?;
        self.write_reg(handle, REG_12, INIT_REG_12_VAL)?;
        self.write_reg(handle, REG_14, INIT_REG_14_VAL)?;
        self.write_reg(handle, REG_16, INIT_REG_16_VAL)?;
        self.write_reg(handle, REG_1F, INIT_REG_1F_VAL)?;
        self.write_reg(handle, REG_09, INIT_REG_09_VAL)?;
        self.write_reg(handle, REG_0B, INIT_REG_0B_VAL)?;
        self.write_reg(handle, REG_0C, INIT_REG_0C_VAL)?;
        self.write_reg(handle, REG_0E, INIT_REG_0E_VAL)?;
        self.write_reg(handle, REG_21, INIT_REG_21_VAL)?;
        self.write_reg(handle, REG_22, INIT_REG_22_VAL)?;

        if ifagc_mode == AGC_INTERNAL {
            self.write_reg(handle, REG_45, AGC_INTERNAL_REG_45)?;
            self.write_reg(handle, REG_4C, AGC_INTERNAL_REG_4C)?;
        } else if ifagc_mode == AGC_EXTERNAL {
            self.write_reg(handle, REG_45, AGC_EXTERNAL_REG_45)?;
            self.write_reg(handle, REG_4C, AGC_EXTERNAL_REG_4C)?;
        }

        self.write_reg(handle, REG_3F, INIT_REG_3F_VAL)?;
        self.write_reg(handle, REG_02, INIT_REG_02_VAL)?;
        self.write_reg(handle, REG_58, INIT_REG_58_VAL)?;

        // Default bandwidth: 7.8 MHz (filter_bw = 8)
        self.set_filter(handle, FILTER_BW_8MHZ, freq_xtal_khz)?;

        Ok(())
    }
}

impl Tuner for Fc2580Tuner {
    /// Initialize the FC2580 tuner.
    ///
    /// Exact port of `fc2580_Initialize`.
    fn init(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        // AGC mode: external (matching the C source TODO comment)
        let agc_mode = AGC_EXTERNAL;

        // Crystal frequency in kHz: round(CrystalFreqHz / 1000)
        let crystal_freq_khz = (CRYSTAL_FREQ + 500) / 1000;

        self.set_init(handle, agc_mode, crystal_freq_khz)
    }

    /// Put the tuner in standby.
    ///
    /// The FC2580 C driver does not define an explicit exit/standby function.
    fn exit(
        &mut self,
        _handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        Ok(())
    }

    /// Set the tuner frequency in Hz.
    ///
    /// Exact port of `fc2580_SetRfFreqHz`.
    fn set_freq(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq: u32,
    ) -> Result<(), RtlSdrError> {
        self.freq = freq;

        // Convert Hz to kHz: round(freq / 1000)
        let rf_freq_khz = (freq + 500) / 1000;
        let crystal_freq_khz = (CRYSTAL_FREQ + 500) / 1000;

        self.set_freq_internal(handle, rf_freq_khz, crystal_freq_khz)
    }

    /// Set the tuner bandwidth. Returns 0 as the IF frequency.
    ///
    /// Exact port of `fc2580_SetBandwidthMode`.
    fn set_bw(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        bw: u32,
        _sample_rate: u32,
    ) -> Result<u32, RtlSdrError> {
        let crystal_freq_khz = (CRYSTAL_FREQ + 500) / 1000;

        // Map bandwidth in Hz to the filter mode byte
        let filter_mode = match bw {
            BW_1_53MHZ_HZ => FILTER_BW_1_53MHZ,
            BW_6MHZ_HZ => FILTER_BW_6MHZ,
            BW_7MHZ_HZ => FILTER_BW_7MHZ,
            _ => FILTER_BW_8MHZ,
        };

        self.set_filter(handle, filter_mode, crystal_freq_khz)?;

        // FC2580 does not have a separate IF frequency to report
        Ok(0)
    }

    /// Set the tuner gain in tenths of dB.
    ///
    /// The FC2580 has no gain control in the original C driver.
    fn set_gain(
        &mut self,
        _handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        _gain: i32,
    ) -> Result<(), RtlSdrError> {
        Ok(())
    }

    /// Update the crystal frequency.
    fn set_xtal(&mut self, xtal: u32) {
        self.xtal = xtal;
    }

    /// Set manual or automatic gain mode.
    ///
    /// The FC2580 uses external AGC; gain mode switching is a no-op.
    fn set_gain_mode(
        &mut self,
        _handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        _manual: bool,
    ) -> Result<(), RtlSdrError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_i2c_addr() {
        assert_eq!(I2C_ADDR, 0xac);
    }

    #[test]
    fn test_check_addr() {
        assert_eq!(CHECK_ADDR, 0x01);
    }

    #[test]
    fn test_check_val() {
        assert_eq!(CHECK_VAL, 0x56);
    }

    #[test]
    fn test_crystal_freq() {
        // 16.384 MHz
        assert_eq!(CRYSTAL_FREQ, 16_384_000);
    }

    #[test]
    fn test_crystal_freq_khz_rounding() {
        // round(16384000 / 1000) = 16384
        let khz = (CRYSTAL_FREQ + 500) / 1000;
        assert_eq!(khz, 16384);
    }

    #[test]
    fn test_band_classification_vhf() {
        // <= 400 MHz = VHF
        assert_eq!(Fc2580Tuner::classify_band(100_000), Band::Vhf);
        assert_eq!(Fc2580Tuner::classify_band(400_000), Band::Vhf);
    }

    #[test]
    fn test_band_classification_uhf() {
        // 400 MHz < f <= 1000 MHz = UHF
        assert_eq!(Fc2580Tuner::classify_band(400_001), Band::Uhf);
        assert_eq!(Fc2580Tuner::classify_band(500_000), Band::Uhf);
        assert_eq!(Fc2580Tuner::classify_band(1_000_000), Band::Uhf);
    }

    #[test]
    fn test_band_classification_lband() {
        // > 1000 MHz = L-Band
        assert_eq!(Fc2580Tuner::classify_band(1_000_001), Band::LBand);
        assert_eq!(Fc2580Tuner::classify_band(2_000_000), Band::LBand);
    }

    #[test]
    fn test_new_defaults() {
        let tuner = Fc2580Tuner::new(28_800_000);
        assert_eq!(tuner.xtal, 28_800_000);
        assert_eq!(tuner.freq, 0);
    }

    #[test]
    fn test_gains_count() {
        // FC2580 has no gain control (single zero entry)
        assert_eq!(FC2580_GAINS.len(), 1);
        assert_eq!(FC2580_GAINS[0], 0);
    }

    #[test]
    fn test_border_freq() {
        // 2.6 GHz in kHz
        assert_eq!(BORDER_FREQ, 2_600_000);
    }

    #[test]
    fn test_vco_calculation_uhf() {
        // UHF: f_vco = f_lo * 4
        let f_lo_khz: u32 = 500_000; // 500 MHz
        let f_vco = f_lo_khz * 4;
        assert_eq!(f_vco, 2_000_000); // 2 GHz in kHz
    }

    #[test]
    fn test_vco_calculation_lband() {
        // L-Band: f_vco = f_lo * 2
        let f_lo_khz: u32 = 1_500_000; // 1.5 GHz
        let f_vco = f_lo_khz * 2;
        assert_eq!(f_vco, 3_000_000); // 3 GHz in kHz
    }

    #[test]
    fn test_vco_calculation_vhf() {
        // VHF: f_vco = f_lo * 12
        let f_lo_khz: u32 = 200_000; // 200 MHz
        let f_vco = f_lo_khz * 12;
        assert_eq!(f_vco, 2_400_000); // 2.4 GHz in kHz
    }

    #[test]
    fn test_r_val_selection() {
        let freq_xtal_khz: u32 = 16384;

        // Test case where f_vco >= 2*76*freq_xtal -> r_val = 1
        let f_vco_high: u32 = 2 * 76 * freq_xtal_khz;
        let r_val = if f_vco_high >= 2 * 76 * freq_xtal_khz {
            1u32
        } else if f_vco_high >= 76 * freq_xtal_khz {
            2
        } else {
            4
        };
        assert_eq!(r_val, 1);

        // Test case where 76*freq_xtal <= f_vco < 2*76*freq_xtal -> r_val = 2
        let f_vco_mid: u32 = 76 * freq_xtal_khz;
        let r_val = if f_vco_mid >= 2 * 76 * freq_xtal_khz {
            1u32
        } else if f_vco_mid >= 76 * freq_xtal_khz {
            2
        } else {
            4
        };
        assert_eq!(r_val, 2);

        // Test case where f_vco < 76*freq_xtal -> r_val = 4
        let f_vco_low: u32 = 76 * freq_xtal_khz - 1;
        let r_val = if f_vco_low >= 2 * 76 * freq_xtal_khz {
            1u32
        } else if f_vco_low >= 76 * freq_xtal_khz {
            2
        } else {
            4
        };
        assert_eq!(r_val, 4);
    }

    #[test]
    fn test_pll_k_shift() {
        assert_eq!(PLL_K_SHIFT, 16);
    }

    #[test]
    fn test_r_val_bits_encoding() {
        assert_eq!(R_VAL_1_BITS, 0x00);
        assert_eq!(R_VAL_2_BITS, 0x10);
        assert_eq!(R_VAL_4_BITS, 0x20);
    }

    #[test]
    fn test_filter_coefficients() {
        // Verify filter coefficient calculations for 16384 kHz crystal
        let freq_xtal_khz: u32 = 16384;

        // BW 1.53 MHz: 4151 * 16384 / 1000000 = 68 (truncated)
        let val = (FILTER_1_53_COEFF * freq_xtal_khz / 1_000_000) as u8;
        assert_eq!(val, 68);

        // BW 6 MHz: 4400 * 16384 / 1000000 = 72 (truncated)
        let val = (FILTER_6_COEFF * freq_xtal_khz / 1_000_000) as u8;
        assert_eq!(val, 72);

        // BW 7 MHz: 3910 * 16384 / 1000000 = 64 (truncated)
        let val = (FILTER_7_COEFF * freq_xtal_khz / 1_000_000) as u8;
        assert_eq!(val, 64);

        // BW 8 MHz: 3300 * 16384 / 1000000 = 54 (truncated)
        let val = (FILTER_8_COEFF * freq_xtal_khz / 1_000_000) as u8;
        assert_eq!(val, 54);
    }

    #[test]
    fn test_agc_mode_constants() {
        assert_eq!(AGC_INTERNAL, 1);
        assert_eq!(AGC_EXTERNAL, 2);
    }

    #[test]
    fn test_band_bits() {
        // Verify band bit assignments
        let base: u8 = 0x0E;

        // UHF: clear bits 6-7
        let uhf = base & BAND_CLEAR_MASK;
        assert_eq!(uhf & 0xC0, 0x00);

        // VHF: set bit 7
        let vhf = (base & BAND_CLEAR_MASK) | VHF_BAND_BIT;
        assert_eq!(vhf & 0xC0, 0x80);

        // L-Band: set bit 6
        let lband = (base & BAND_CLEAR_MASK) | L_BAND_BIT;
        assert_eq!(lband & 0xC0, 0x40);
    }

    #[test]
    fn test_uhf_lna_cap_values() {
        assert_eq!(LNA_CAP_LOW, 0x9F);
        assert_eq!(LNA_CAP_HIGH, 0x8F);
    }

    #[test]
    fn test_filter_cal_mon_mask() {
        // Calibration complete when bits 6-7 are both set
        assert_eq!(FILTER_CAL_MON_MASK, 0xC0);
        assert_eq!(FILTER_CAL_COMPLETE, 0xC0);

        // Test: calibration complete
        assert_eq!(0xC0_u8 & FILTER_CAL_MON_MASK, FILTER_CAL_COMPLETE);
        assert_eq!(0xDF_u8 & FILTER_CAL_MON_MASK, FILTER_CAL_COMPLETE);

        // Test: calibration not complete
        assert_ne!(0x80_u8 & FILTER_CAL_MON_MASK, FILTER_CAL_COMPLETE);
        assert_ne!(0x40_u8 & FILTER_CAL_MON_MASK, FILTER_CAL_COMPLETE);
        assert_ne!(0x3F_u8 & FILTER_CAL_MON_MASK, FILTER_CAL_COMPLETE);
    }

    #[test]
    fn test_set_xtal() {
        let mut tuner = Fc2580Tuner::new(16_384_000);
        assert_eq!(tuner.xtal, 16_384_000);
        tuner.set_xtal(28_800_000);
        assert_eq!(tuner.xtal, 28_800_000);
    }
}
