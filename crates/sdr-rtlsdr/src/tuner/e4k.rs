//! Elonics E4000 tuner driver.
//!
//! Faithful port of `tuner_e4k.c` from librtlsdr.
//!
//! Original copyright:
//! - Copyright (C) 2011-2012 by Harald Welte <laforge@gnumonks.org>
//! - Copyright (C) 2012 by Sylvain Munaut <tnt@246tNt.com>
//! - Copyright (C) 2012 by Hoernchen <la@tfc-server.de>

use crate::error::RtlSdrError;
use crate::tuner::Tuner;
use crate::usb;

// ---------------------------------------------------------------------------
// I2C address and identification
// ---------------------------------------------------------------------------

/// E4000 I2C address.
pub const I2C_ADDR: u8 = 0xc8;

/// Register address used to identify the E4000 (MASTER3 register).
pub const CHECK_ADDR: u8 = 0x02;

/// Expected chip ID value read from `CHECK_ADDR`.
pub const CHECK_VAL: u8 = 0x40;

// ---------------------------------------------------------------------------
// Register addresses
// ---------------------------------------------------------------------------

const REG_MASTER1: u8 = 0x00;
const REG_SYNTH1: u8 = 0x07;
const REG_SYNTH3: u8 = 0x09;
const REG_SYNTH4: u8 = 0x0a;
const REG_SYNTH5: u8 = 0x0b;
const REG_SYNTH7: u8 = 0x0d;
const REG_FILT1: u8 = 0x10;
const REG_FILT2: u8 = 0x11;
const REG_FILT3: u8 = 0x12;
const REG_GAIN1: u8 = 0x14;
const REG_GAIN2: u8 = 0x15;
const REG_GAIN3: u8 = 0x16;
const REG_GAIN4: u8 = 0x17;
const REG_AGC1: u8 = 0x1a;
const REG_AGC4: u8 = 0x1d;
const REG_AGC5: u8 = 0x1e;
const REG_AGC6: u8 = 0x1f;
const REG_AGC7: u8 = 0x20;
const REG_AGC11: u8 = 0x24;
// DC offset registers — used by dc_offset_gen_table (ported from C `#if 0` block)
#[allow(dead_code)]
const REG_DC1: u8 = 0x29;
#[allow(dead_code)]
const REG_DC2: u8 = 0x2a;
#[allow(dead_code)]
const REG_DC3: u8 = 0x2b;
#[allow(dead_code)]
const REG_DC4: u8 = 0x2c;
#[allow(dead_code)]
const REG_DC5: u8 = 0x2d;
#[allow(dead_code)]
const REG_DC7: u8 = 0x2f;
const REG_DCTIME1: u8 = 0x70;
const REG_DCTIME2: u8 = 0x71;
const REG_BIAS: u8 = 0x78;
const REG_CLKOUT_PWDN: u8 = 0x7a;
const REG_REF_CLK: u8 = 0x06;
const REG_CLK_INP: u8 = 0x05;

// ---------------------------------------------------------------------------
// Register bit masks and values
// ---------------------------------------------------------------------------

/// MASTER1 register: reset bit.
const MASTER1_RESET: u8 = 1 << 0;

/// MASTER1 register: normal standby bit.
const MASTER1_NORM_STBY: u8 = 1 << 1;

/// MASTER1 register: power-on-reset detect bit.
const MASTER1_POR_DET: u8 = 1 << 2;

/// FILT3 register: channel filter disable bit.
const FILT3_DISABLE: u8 = 1 << 5;

/// AGC1 register: mode mask (lower 4 bits).
const AGC1_MOD_MASK: u8 = 0x0f;

/// AGC7 register: mixer gain auto bit.
const AGC7_MIX_GAIN_AUTO: u8 = 1 << 0;

/// AGC11 register: LNA gain enhancement enable bit.
#[allow(dead_code)]
const AGC11_LNA_GAIN_ENH: u8 = 1 << 0;

/// DC5 register: range detect enable bit.
#[allow(dead_code)]
const DC5_RANGE_DET_EN: u8 = 1 << 2;

/// Clock output disable value.
const CLKOUT_DISABLE: u8 = 0x96;

// ---------------------------------------------------------------------------
// AGC mode values
// ---------------------------------------------------------------------------

/// AGC mode: serial (fully manual).
const AGC_MOD_SERIAL: u8 = 0x00;

/// AGC mode: IF serial, LNA autonomous.
const AGC_MOD_IF_SERIAL_LNA_AUTON: u8 = 0x09;

// ---------------------------------------------------------------------------
// PLL constants
// ---------------------------------------------------------------------------

/// PLL Y constant (sigma-delta modulator denominator).
const PLL_Y: u64 = 65536;

/// Minimum valid oscillator frequency (16 MHz).
const FOSC_MIN: u32 = 16_000_000;

/// Maximum valid oscillator frequency (30 MHz).
const FOSC_MAX: u32 = 30_000_000;

/// 3-phase mixing threshold frequency (350 MHz).
#[allow(dead_code)]
const THREE_PHASE_MIXING_THRESH: u32 = 350_000_000;

/// Band boundary: VHF2/VHF3 at 140 MHz.
const BAND_VHF2_MAX: u32 = 140_000_000;

/// Band boundary: VHF3/UHF at 350 MHz.
const BAND_VHF3_MAX: u32 = 350_000_000;

/// Band boundary: UHF/L at 1135 MHz.
const BAND_UHF_MAX: u32 = 1_135_000_000;

// ---------------------------------------------------------------------------
// Frequency band enumeration
// ---------------------------------------------------------------------------

/// E4000 frequency band selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Band {
    Vhf2 = 0,
    Vhf3 = 1,
    Uhf = 2,
    L = 3,
}

// ---------------------------------------------------------------------------
// IF filter type
// ---------------------------------------------------------------------------

/// E4000 IF filter selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum IfFilter {
    Mix = 0,
    Chan = 1,
    Rc = 2,
}

// ---------------------------------------------------------------------------
// Register field descriptor (for field read/write helpers)
// ---------------------------------------------------------------------------

/// Describes a bit field within a register.
struct RegField {
    reg: u8,
    shift: u8,
    width: u8,
}

// ---------------------------------------------------------------------------
// Width-to-mask lookup table
// ---------------------------------------------------------------------------

/// Bit-width to mask lookup: `WIDTH_MASK[n]` = (1 << n) - 1.
const WIDTH_MASK: [u8; 9] = [0, 1, 3, 7, 0x0f, 0x1f, 0x3f, 0x7f, 0xff];

// ---------------------------------------------------------------------------
// PLL settings table
// ---------------------------------------------------------------------------

/// PLL divider settings entry.
struct PllSettings {
    /// Maximum frequency in kHz (exclusive upper bound for this entry).
    freq_khz: u32,
    /// REG_SYNTH7 register value (3-phase enable + divider index).
    reg_synth7: u8,
    /// VCO frequency multiplier.
    mult: u8,
}

/// PLL divider table, ordered by ascending maximum frequency.
#[allow(clippy::identity_op)]
const PLL_VARS: [PllSettings; 10] = [
    PllSettings {
        freq_khz: 72_400,
        reg_synth7: (1 << 3) | 7,
        mult: 48,
    },
    PllSettings {
        freq_khz: 81_200,
        reg_synth7: (1 << 3) | 6,
        mult: 40,
    },
    PllSettings {
        freq_khz: 108_300,
        reg_synth7: (1 << 3) | 5,
        mult: 32,
    },
    PllSettings {
        freq_khz: 162_500,
        reg_synth7: (1 << 3) | 4,
        mult: 24,
    },
    PllSettings {
        freq_khz: 216_600,
        reg_synth7: (1 << 3) | 3,
        mult: 16,
    },
    PllSettings {
        freq_khz: 325_000,
        reg_synth7: (1 << 3) | 2,
        mult: 12,
    },
    PllSettings {
        freq_khz: 350_000,
        reg_synth7: (1 << 3) | 1,
        mult: 8,
    },
    PllSettings {
        freq_khz: 432_000,
        reg_synth7: (0 << 3) | 3,
        mult: 8,
    },
    PllSettings {
        freq_khz: 667_000,
        reg_synth7: (0 << 3) | 2,
        mult: 6,
    },
    PllSettings {
        freq_khz: 1_200_000,
        reg_synth7: (0 << 3) | 1,
        mult: 4,
    },
];

// ---------------------------------------------------------------------------
// RF filter center frequency tables
// ---------------------------------------------------------------------------

/// UHF band RF filter center frequencies in Hz.
const RF_FILT_CENTER_UHF: [u32; 16] = [
    360_000_000,
    380_000_000,
    405_000_000,
    425_000_000,
    450_000_000,
    475_000_000,
    505_000_000,
    540_000_000,
    575_000_000,
    615_000_000,
    670_000_000,
    720_000_000,
    760_000_000,
    840_000_000,
    890_000_000,
    970_000_000,
];

/// L band RF filter center frequencies in Hz.
const RF_FILT_CENTER_L: [u32; 16] = [
    1_300_000_000,
    1_320_000_000,
    1_360_000_000,
    1_410_000_000,
    1_445_000_000,
    1_460_000_000,
    1_490_000_000,
    1_530_000_000,
    1_560_000_000,
    1_590_000_000,
    1_640_000_000,
    1_660_000_000,
    1_680_000_000,
    1_700_000_000,
    1_720_000_000,
    1_750_000_000,
];

// ---------------------------------------------------------------------------
// IF filter bandwidth tables
// ---------------------------------------------------------------------------

/// Mixer filter bandwidth values in Hz.
const MIX_FILTER_BW: [u32; 16] = [
    27_000_000, 27_000_000, 27_000_000, 27_000_000, 27_000_000, 27_000_000, 27_000_000, 27_000_000,
    4_600_000, 4_200_000, 3_800_000, 3_400_000, 3_300_000, 2_700_000, 2_300_000, 1_900_000,
];

/// IF RC filter bandwidth values in Hz.
const IFRC_FILTER_BW: [u32; 16] = [
    21_400_000, 21_000_000, 17_600_000, 14_700_000, 12_400_000, 10_600_000, 9_000_000, 7_700_000,
    6_400_000, 5_300_000, 4_400_000, 3_400_000, 2_600_000, 1_800_000, 1_200_000, 1_000_000,
];

/// IF channel filter bandwidth values in Hz.
const IFCH_FILTER_BW: [u32; 32] = [
    5_500_000, 5_300_000, 5_000_000, 4_800_000, 4_600_000, 4_400_000, 4_300_000, 4_100_000,
    3_900_000, 3_800_000, 3_700_000, 3_600_000, 3_400_000, 3_300_000, 3_200_000, 3_100_000,
    3_000_000, 2_950_000, 2_900_000, 2_800_000, 2_750_000, 2_700_000, 2_600_000, 2_550_000,
    2_500_000, 2_450_000, 2_400_000, 2_300_000, 2_280_000, 2_240_000, 2_200_000, 2_150_000,
];

/// IF filter register field descriptors, indexed by `IfFilter`.
const IF_FILTER_FIELDS: [RegField; 3] = [
    RegField {
        reg: REG_FILT2,
        shift: 4,
        width: 4,
    },
    RegField {
        reg: REG_FILT3,
        shift: 0,
        width: 5,
    },
    RegField {
        reg: REG_FILT2,
        shift: 0,
        width: 4,
    },
];

// ---------------------------------------------------------------------------
// IF stage gain tables
// ---------------------------------------------------------------------------

/// IF gain stage 1 values in dB (2 entries, 1-bit field).
const IF_STAGE1_GAIN: [i8; 2] = [-3, 6];

/// IF gain stages 2 and 3 values in dB (4 entries, 2-bit field).
const IF_STAGE23_GAIN: [i8; 4] = [0, 3, 6, 9];

/// IF gain stage 4 values in dB (4 entries, 2-bit field).
const IF_STAGE4_GAIN: [i8; 4] = [0, 1, 2, 2];

/// IF gain stages 5 and 6 values in dB (8 entries, 3-bit field).
const IF_STAGE56_GAIN: [i8; 8] = [3, 6, 9, 12, 15, 15, 15, 15];

/// Maximum IF gain per stage (indexed 0..6, stage 0 unused).
/// Used by `dc_offset_gen_table` (ported from C `#if 0` block).
#[allow(dead_code)]
const IF_GAINS_MAX: [i8; 7] = [0, 6, 9, 9, 2, 15, 15];

/// IF gain stage register field descriptors (indexed 0..6, stage 0 unused).
const IF_STAGE_GAIN_REGS: [RegField; 7] = [
    RegField {
        reg: 0,
        shift: 0,
        width: 0,
    },
    RegField {
        reg: REG_GAIN3,
        shift: 0,
        width: 1,
    },
    RegField {
        reg: REG_GAIN3,
        shift: 1,
        width: 2,
    },
    RegField {
        reg: REG_GAIN3,
        shift: 3,
        width: 2,
    },
    RegField {
        reg: REG_GAIN3,
        shift: 5,
        width: 2,
    },
    RegField {
        reg: REG_GAIN4,
        shift: 0,
        width: 3,
    },
    RegField {
        reg: REG_GAIN4,
        shift: 3,
        width: 3,
    },
];

// ---------------------------------------------------------------------------
// LNA gain table
// ---------------------------------------------------------------------------

/// LNA gain table: pairs of (gain in tenths of dB, register value).
const LNA_GAIN: [(i32, u8); 13] = [
    (-50, 0),
    (-25, 1),
    (0, 4),
    (25, 5),
    (50, 6),
    (75, 7),
    (100, 8),
    (125, 9),
    (150, 10),
    (175, 11),
    (200, 12),
    (250, 13),
    (300, 14),
];

/// LNA gain mask in GAIN1 register (lower 4 bits).
const LNA_GAIN_MASK: u8 = 0x0f;

// ---------------------------------------------------------------------------
// Enhancement gain table
// ---------------------------------------------------------------------------

/// Enhancement gain values in tenths of dB.
/// Used by `set_enh_gain` (ported from C `#if 0` block).
#[allow(dead_code)]
const ENH_GAIN: [i32; 4] = [10, 30, 50, 70];

/// Enhancement gain mask in AGC11 register (lower 3 bits).
#[allow(dead_code)]
const ENH_GAIN_MASK: u8 = 0x07;

// ---------------------------------------------------------------------------
// Mixer gain constants
// ---------------------------------------------------------------------------

/// Mixer gain value for 4 dB.
const MIXER_GAIN_4DB: i8 = 4;

/// Mixer gain value for 12 dB.
const MIXER_GAIN_12DB: i8 = 12;

/// Mixer gain threshold for `set_gain` (tenths of dB): above 340, use 12 dB.
const MIXER_GAIN_THRESH: i32 = 340;

/// Maximum LNA gain value for `set_gain` (tenths of dB).
const MAX_LNA_GAIN: i32 = 300;

// ---------------------------------------------------------------------------
// DC offset calibration gain combinations
// ---------------------------------------------------------------------------

/// DC offset calibration gain combination entry.
/// Used by `dc_offset_gen_table` (ported from C `#if 0` block).
#[allow(dead_code)]
struct DcGainComb {
    mixer_gain: i8,
    if1_gain: i8,
    reg: u8,
}

/// DC offset calibration gain combinations.
#[allow(dead_code)]
const DC_GAIN_COMB: [DcGainComb; 4] = [
    DcGainComb {
        mixer_gain: 4,
        if1_gain: -3,
        reg: 0x50,
    },
    DcGainComb {
        mixer_gain: 4,
        if1_gain: 6,
        reg: 0x51,
    },
    DcGainComb {
        mixer_gain: 12,
        if1_gain: -3,
        reg: 0x52,
    },
    DcGainComb {
        mixer_gain: 12,
        if1_gain: 6,
        reg: 0x53,
    },
];

// ---------------------------------------------------------------------------
// Gains table (public, for device enumeration)
// ---------------------------------------------------------------------------

/// Supported gain values in tenths of dB, matching librtlsdr `e4k_gains[]`.
pub const E4K_GAINS: [i32; 14] = [
    -10, 15, 40, 65, 90, 115, 140, 165, 190, 215, 240, 290, 340, 420,
];

// ---------------------------------------------------------------------------
// Magic init register writes
// ---------------------------------------------------------------------------

/// Magic initialization register writes (address, value) pairs.
const MAGIC_INIT_REGS: [(u8, u8); 8] = [
    (0x7e, 0x01),
    (0x7f, 0xfe),
    (0x82, 0x00),
    (0x86, 0x50), // polarity A
    (0x87, 0x20),
    (0x88, 0x01),
    (0x9f, 0x7f),
    (0xa0, 0x07),
];

// ---------------------------------------------------------------------------
// Init constants
// ---------------------------------------------------------------------------

/// AGC4 high threshold value during init.
const INIT_AGC4_HIGH_THRESH: u8 = 0x10;

/// AGC5 low threshold value during init.
const INIT_AGC5_LOW_THRESH: u8 = 0x04;

/// AGC6 LNA calib + loop rate value during init.
const INIT_AGC6_LNA_CALIB: u8 = 0x1a;

/// IF filter bandwidth for mixer filter during init (1900 kHz).
const INIT_IF_FILTER_MIX_BW: u32 = 1_900_000;

/// IF filter bandwidth for RC filter during init (1000 kHz).
const INIT_IF_FILTER_RC_BW: u32 = 1_000_000;

/// IF filter bandwidth for channel filter during init (2150 kHz).
const INIT_IF_FILTER_CHAN_BW: u32 = 2_150_000;

/// Initial IF gain stage 1 value (dB).
const INIT_IF_GAIN_STAGE1: i8 = 6;

/// Initial IF gain stages 2-4 value (dB).
const INIT_IF_GAIN_STAGES_2_4: i8 = 0;

/// Initial IF gain stages 5-6 value (dB).
const INIT_IF_GAIN_STAGES_5_6: i8 = 9;

// ---------------------------------------------------------------------------
// PLL parameters
// ---------------------------------------------------------------------------

/// Computed PLL parameters for the E4000 tuner.
#[derive(Debug, Clone, Copy)]
struct PllParams {
    /// Oscillator frequency in Hz.
    fosc: u32,
    /// Actual tuned LO frequency in Hz.
    flo: u32,
    /// Integer part of VCO multiplier.
    z: u8,
    /// Fractional part of VCO multiplier.
    x: u16,
    /// VCO divisor (used in PLL computation, stored for completeness).
    #[allow(dead_code)]
    r: u8,
    /// REG_SYNTH7 register value.
    r_idx: u8,
}

// ---------------------------------------------------------------------------
// E4000 tuner state
// ---------------------------------------------------------------------------

/// Elonics E4000 tuner driver.
///
/// Ports `e4k_init`, `e4k_tune_freq`, gain control, and filter configuration
/// from `tuner_e4k.c`.
pub struct E4kTuner {
    /// Crystal oscillator frequency in Hz.
    xtal: u32,
    /// Current PLL parameters.
    pll: PllParams,
    /// Current band selection.
    band: Band,
}

impl E4kTuner {
    /// Create a new E4000 tuner driver.
    pub fn new(xtal: u32) -> Self {
        Self {
            xtal,
            pll: PllParams {
                fosc: xtal,
                flo: 0,
                z: 0,
                x: 0,
                r: 2,
                r_idx: 0,
            },
            band: Band::Vhf2,
        }
    }

    // -----------------------------------------------------------------------
    // Low-level I2C register access
    // -----------------------------------------------------------------------

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

    /// Set or clear masked bits inside a register (read-modify-write).
    ///
    /// Ports `e4k_reg_set_mask`.
    fn reg_set_mask(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
        mask: u8,
        val: u8,
    ) -> Result<(), RtlSdrError> {
        let tmp = self.read_reg(handle, reg)?;

        if (tmp & mask) == (val & mask) {
            return Ok(());
        }

        self.write_reg(handle, reg, (tmp & !mask) | (val & mask))
    }

    /// Write a value to a register bit field.
    ///
    /// Ports `e4k_field_write`.
    fn field_write(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        field: &RegField,
        val: u8,
    ) -> Result<(), RtlSdrError> {
        let mask = WIDTH_MASK[field.width as usize] << field.shift;
        self.reg_set_mask(handle, field.reg, mask, val << field.shift)
    }

    // -----------------------------------------------------------------------
    // Filter control
    // -----------------------------------------------------------------------

    /// Find the closest index in an array to a target frequency.
    ///
    /// Ports `closest_arr_idx`.
    fn closest_arr_idx(arr: &[u32], freq: u32) -> usize {
        let mut best_idx = 0;
        let mut best_delta = u32::MAX;

        for (i, &center) in arr.iter().enumerate() {
            let delta = freq.abs_diff(center);
            if delta < best_delta {
                best_delta = delta;
                best_idx = i;
            }
        }

        best_idx
    }

    /// Choose the 4-bit RF filter index for a given band and frequency.
    ///
    /// Ports `choose_rf_filter`.
    fn choose_rf_filter(band: Band, freq: u32) -> u8 {
        match band {
            Band::Vhf2 | Band::Vhf3 => 0,
            Band::Uhf => Self::closest_arr_idx(&RF_FILT_CENTER_UHF, freq) as u8,
            Band::L => Self::closest_arr_idx(&RF_FILT_CENTER_L, freq) as u8,
        }
    }

    /// Set the RF filter based on current band and tuned frequency.
    ///
    /// Ports `e4k_rf_filter_set`.
    fn rf_filter_set(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        let rc = Self::choose_rf_filter(self.band, self.pll.flo);
        self.reg_set_mask(handle, REG_FILT1, 0x0f, rc)
    }

    /// Find the closest IF filter bandwidth index.
    ///
    /// Ports `find_if_bw`.
    fn find_if_bw(filter: IfFilter, bw: u32) -> u8 {
        let arr: &[u32] = match filter {
            IfFilter::Mix => &MIX_FILTER_BW,
            IfFilter::Chan => &IFCH_FILTER_BW,
            IfFilter::Rc => &IFRC_FILTER_BW,
        };
        Self::closest_arr_idx(arr, bw) as u8
    }

    /// Set the IF filter bandwidth.
    ///
    /// Ports `e4k_if_filter_bw_set`.
    fn if_filter_bw_set(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        filter: IfFilter,
        bandwidth: u32,
    ) -> Result<(), RtlSdrError> {
        let bw_idx = Self::find_if_bw(filter, bandwidth);
        let field = &IF_FILTER_FIELDS[filter as usize];
        self.field_write(handle, field, bw_idx)
    }

    /// Enable or disable the channel filter.
    ///
    /// Ports `e4k_if_filter_chan_enable`.
    fn if_filter_chan_enable(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        on: bool,
    ) -> Result<(), RtlSdrError> {
        self.reg_set_mask(
            handle,
            REG_FILT3,
            FILT3_DISABLE,
            if on { 0 } else { FILT3_DISABLE },
        )
    }

    // -----------------------------------------------------------------------
    // PLL / frequency control
    // -----------------------------------------------------------------------

    /// Compute PLL parameters for a target frequency.
    ///
    /// Ports `e4k_compute_pll_params`. Returns `None` if the oscillator
    /// frequency is out of range.
    #[allow(clippy::cast_possible_truncation)]
    fn compute_pll_params(fosc: u32, intended_flo: u32) -> Option<PllParams> {
        // Validate oscillator frequency
        if fosc < FOSC_MIN || fosc > FOSC_MAX {
            return None;
        }

        let mut r: u8 = 2;
        let mut r_idx: u8 = 0;

        // Find the appropriate PLL divider settings
        let intended_flo_khz = intended_flo / 1000;
        for entry in &PLL_VARS {
            if intended_flo_khz < entry.freq_khz {
                r_idx = entry.reg_synth7;
                r = entry.mult;
                break;
            }
        }

        // Compute VCO frequency (need 64-bit: flo_max=1700MHz * r_max=48)
        let intended_fvco = u64::from(intended_flo) * u64::from(r);

        // Compute integer component of multiplier
        let z = intended_fvco / u64::from(fosc);

        // Compute fractional part (remainder < fosc, so x < PLL_Y, fits in u16)
        let remainder = intended_fvco - u64::from(fosc) * z;
        let x_raw = (remainder * PLL_Y) / u64::from(fosc);
        if x_raw > u64::from(u16::MAX) || z > 255 {
            return None; // PLL parameters out of range for this frequency
        }
        let x = x_raw as u16;

        // Compute actual LO frequency (u64 throughout to prevent overflow)
        let fvco = u64::from(fosc) * z + (u64::from(fosc) * u64::from(x)) / PLL_Y;
        let flo = (fvco / u64::from(r)) as u32;
        Some(PllParams {
            fosc,
            flo,
            z: z as u8,
            x,
            r,
            r_idx,
        })
    }

    /// Set the frequency band and write the band register.
    ///
    /// Ports `e4k_band_set`.
    fn band_set(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        band: Band,
    ) -> Result<(), RtlSdrError> {
        // Set bias register based on band
        match band {
            Band::Vhf2 | Band::Vhf3 | Band::Uhf => {
                self.write_reg(handle, REG_BIAS, 3)?;
            }
            Band::L => {
                self.write_reg(handle, REG_BIAS, 0)?;
            }
        }

        // Workaround: reset SYNTH1 band bits before writing to avoid
        // gap between 325-350 MHz
        self.reg_set_mask(handle, REG_SYNTH1, 0x06, 0)?;
        self.reg_set_mask(handle, REG_SYNTH1, 0x06, (band as u8) << 1)?;

        self.band = band;
        Ok(())
    }

    /// Program PLL parameters into the tuner and set band/filter.
    ///
    /// Ports `e4k_tune_params`.
    fn tune_params(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        p: &PllParams,
    ) -> Result<(), RtlSdrError> {
        // Program R + 3phase/2phase
        self.write_reg(handle, REG_SYNTH7, p.r_idx)?;
        // Program Z
        self.write_reg(handle, REG_SYNTH3, p.z)?;
        // Program X
        self.write_reg(handle, REG_SYNTH4, (p.x & 0xff) as u8)?;
        self.write_reg(handle, REG_SYNTH5, (p.x >> 8) as u8)?;

        // Store PLL params
        self.pll = *p;

        // Set the band based on frequency
        let band = if self.pll.flo < BAND_VHF2_MAX {
            Band::Vhf2
        } else if self.pll.flo < BAND_VHF3_MAX {
            Band::Vhf3
        } else if self.pll.flo < BAND_UHF_MAX {
            Band::Uhf
        } else {
            Band::L
        };
        self.band_set(handle, band)?;

        // Select and set proper RF filter
        self.rf_filter_set(handle)?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Gain control
    // -----------------------------------------------------------------------

    /// Find the index of a gain value in a stage gain table.
    ///
    /// Ports `find_stage_gain`.
    fn find_stage_gain(stage: u8, val: i8) -> Result<u8, RtlSdrError> {
        let arr: &[i8] = match stage {
            1 => &IF_STAGE1_GAIN,
            2 | 3 => &IF_STAGE23_GAIN,
            4 => &IF_STAGE4_GAIN,
            5 | 6 => &IF_STAGE56_GAIN,
            _ => {
                return Err(RtlSdrError::Tuner(format!(
                    "E4K: invalid IF gain stage {stage}"
                )));
            }
        };

        for (i, &g) in arr.iter().enumerate() {
            if g == val {
                return Ok(i as u8);
            }
        }

        Err(RtlSdrError::Tuner(format!(
            "E4K: invalid gain value {val} dB for IF stage {stage}"
        )))
    }

    /// Set the gain of one of the IF gain stages (1..6).
    ///
    /// Ports `e4k_if_gain_set`.
    fn if_gain_set(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        stage: u8,
        value: i8,
    ) -> Result<(), RtlSdrError> {
        let idx = Self::find_stage_gain(stage, value)?;

        let field = &IF_STAGE_GAIN_REGS[stage as usize];
        let mask = WIDTH_MASK[field.width as usize] << field.shift;

        self.reg_set_mask(handle, field.reg, mask, idx << field.shift)
    }

    /// Set the LNA gain.
    ///
    /// Ports `e4k_set_lna_gain`. Gain is in tenths of dB.
    fn set_lna_gain(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        gain: i32,
    ) -> Result<(), RtlSdrError> {
        for &(g, reg_val) in &LNA_GAIN {
            if g == gain {
                return self.reg_set_mask(handle, REG_GAIN1, LNA_GAIN_MASK, reg_val);
            }
        }

        Err(RtlSdrError::Tuner(format!(
            "E4K: invalid LNA gain value {gain} (tenths of dB)"
        )))
    }

    /// Set the mixer gain (4 or 12 dB).
    ///
    /// Ports `e4k_mixer_gain_set`.
    fn mixer_gain_set(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        value: i8,
    ) -> Result<(), RtlSdrError> {
        let bit = match value {
            MIXER_GAIN_4DB => 0u8,
            MIXER_GAIN_12DB => 1u8,
            _ => {
                return Err(RtlSdrError::Tuner(format!(
                    "E4K: invalid mixer gain {value} dB"
                )));
            }
        };

        self.reg_set_mask(handle, REG_GAIN2, 1, bit)
    }

    /// Set the enhancement gain.
    ///
    /// Ports `e4k_set_enh_gain`. Gain is in tenths of dB; 0 = off.
    /// Not called in the default configuration (C source has this in `#if 0`).
    #[allow(dead_code)]
    fn set_enh_gain(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        gain: i32,
    ) -> Result<(), RtlSdrError> {
        for (i, &g) in ENH_GAIN.iter().enumerate() {
            if g == gain {
                let val = AGC11_LNA_GAIN_ENH | ((i as u8) << 1);
                return self.reg_set_mask(handle, REG_AGC11, ENH_GAIN_MASK, val);
            }
        }

        // Disable enhancement gain
        self.reg_set_mask(handle, REG_AGC11, ENH_GAIN_MASK, 0)?;

        if gain == 0 {
            Ok(())
        } else {
            Err(RtlSdrError::Tuner(format!(
                "E4K: invalid enhancement gain {gain} (tenths of dB)"
            )))
        }
    }

    /// Enable or disable manual gain mode.
    ///
    /// Ports `e4k_enable_manual_gain`.
    fn enable_manual_gain(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        manual: bool,
    ) -> Result<(), RtlSdrError> {
        if manual {
            // Set LNA mode to manual (serial)
            self.reg_set_mask(handle, REG_AGC1, AGC1_MOD_MASK, AGC_MOD_SERIAL)?;
            // Set Mixer Gain Control to manual
            self.reg_set_mask(handle, REG_AGC7, AGC7_MIX_GAIN_AUTO, 0)?;
        } else {
            // Set LNA mode to auto
            self.reg_set_mask(handle, REG_AGC1, AGC1_MOD_MASK, AGC_MOD_IF_SERIAL_LNA_AUTON)?;
            // Set Mixer Gain Control to auto
            self.reg_set_mask(handle, REG_AGC7, AGC7_MIX_GAIN_AUTO, 1)?;
            // Disable enhancement gain
            self.reg_set_mask(handle, REG_AGC11, ENH_GAIN_MASK, 0)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // DC offset calibration
    // -----------------------------------------------------------------------

    /// Trigger a DC offset calibration.
    ///
    /// Ports `e4k_dc_offset_calibrate`.
    /// Not called in the default configuration (C source has this in `#if 0`).
    #[allow(dead_code)]
    fn dc_offset_calibrate(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        // Make sure the DC range detector is enabled
        self.reg_set_mask(handle, REG_DC5, DC5_RANGE_DET_EN, DC5_RANGE_DET_EN)?;
        self.write_reg(handle, REG_DC1, 0x01)
    }

    /// Generate the DC offset lookup table.
    ///
    /// Ports `e4k_dc_offset_gen_table`.
    /// Not called in the default configuration (C source has this in `#if 0`).
    #[allow(dead_code)]
    fn dc_offset_gen_table(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        // Disable auto mixer gain
        self.reg_set_mask(handle, REG_AGC7, AGC7_MIX_GAIN_AUTO, 0)?;

        // Set LNA/IF gain to full manual
        self.reg_set_mask(handle, REG_AGC1, AGC1_MOD_MASK, AGC_MOD_SERIAL)?;

        // Set all 'other' gains to maximum
        for stage in 2..=6u8 {
            self.if_gain_set(handle, stage, IF_GAINS_MAX[stage as usize])?;
        }

        // Iterate over all mixer + if_stage_1 gain combinations
        for comb in &DC_GAIN_COMB {
            // Set the combination of mixer / if1 gain
            self.mixer_gain_set(handle, comb.mixer_gain)?;
            self.if_gain_set(handle, 1, comb.if1_gain)?;

            // Perform actual calibration
            self.dc_offset_calibrate(handle)?;

            // Extract I/Q offset and range values
            let offs_i = self.read_reg(handle, REG_DC2)? & 0x3f;
            let offs_q = self.read_reg(handle, REG_DC3)? & 0x3f;
            let range = self.read_reg(handle, REG_DC4)?;
            let range_i = range & 0x03;
            let range_q = (range >> 4) & 0x03;

            // Write into the lookup table
            // TO_LUT(offset, range) = offset | (range << 6)
            self.write_reg(handle, comb.reg, offs_q | (range_q << 6))?;
            self.write_reg(handle, comb.reg + 0x10, offs_i | (range_i << 6))?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Standby
    // -----------------------------------------------------------------------

    /// Enable or disable standby mode.
    ///
    /// Ports `e4k_standby`.
    fn standby(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        enable: bool,
    ) -> Result<(), RtlSdrError> {
        self.reg_set_mask(
            handle,
            REG_MASTER1,
            MASTER1_NORM_STBY,
            if enable { 0 } else { MASTER1_NORM_STBY },
        )
    }

    // -----------------------------------------------------------------------
    // Magic init
    // -----------------------------------------------------------------------

    /// Write magic initialization values.
    ///
    /// Ports `magic_init`.
    fn magic_init(
        &self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        for &(reg, val) in &MAGIC_INIT_REGS {
            self.write_reg(handle, reg, val)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tuner trait implementation
// ---------------------------------------------------------------------------

impl Tuner for E4kTuner {
    /// Initialize the E4000 tuner.
    ///
    /// Exact port of `e4k_init`.
    fn init(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        // Make a dummy I2C read (will not be ACKed)
        let _ = self.read_reg(handle, 0);

        // Reset everything and clear POR indicator
        self.write_reg(
            handle,
            REG_MASTER1,
            MASTER1_RESET | MASTER1_NORM_STBY | MASTER1_POR_DET,
        )?;

        // Configure clock input
        self.write_reg(handle, REG_CLK_INP, 0x00)?;

        // Disable clock output
        self.write_reg(handle, REG_REF_CLK, 0x00)?;
        self.write_reg(handle, REG_CLKOUT_PWDN, CLKOUT_DISABLE)?;

        // Write magic initialization values
        self.magic_init(handle)?;

        // Set AGC thresholds
        self.write_reg(handle, REG_AGC4, INIT_AGC4_HIGH_THRESH)?;
        self.write_reg(handle, REG_AGC5, INIT_AGC5_LOW_THRESH)?;
        self.write_reg(handle, REG_AGC6, INIT_AGC6_LNA_CALIB)?;

        // Set LNA mode to manual (serial)
        self.reg_set_mask(handle, REG_AGC1, AGC1_MOD_MASK, AGC_MOD_SERIAL)?;

        // Set Mixer Gain Control to manual
        self.reg_set_mask(handle, REG_AGC7, AGC7_MIX_GAIN_AUTO, 0)?;

        // Use auto-gain as default
        self.enable_manual_gain(handle, false)?;

        // Select moderate gain levels
        self.if_gain_set(handle, 1, INIT_IF_GAIN_STAGE1)?;
        self.if_gain_set(handle, 2, INIT_IF_GAIN_STAGES_2_4)?;
        self.if_gain_set(handle, 3, INIT_IF_GAIN_STAGES_2_4)?;
        self.if_gain_set(handle, 4, INIT_IF_GAIN_STAGES_2_4)?;
        self.if_gain_set(handle, 5, INIT_IF_GAIN_STAGES_5_6)?;
        self.if_gain_set(handle, 6, INIT_IF_GAIN_STAGES_5_6)?;

        // Set the most narrow filters we can use
        self.if_filter_bw_set(handle, IfFilter::Mix, INIT_IF_FILTER_MIX_BW)?;
        self.if_filter_bw_set(handle, IfFilter::Rc, INIT_IF_FILTER_RC_BW)?;
        self.if_filter_bw_set(handle, IfFilter::Chan, INIT_IF_FILTER_CHAN_BW)?;
        self.if_filter_chan_enable(handle, true)?;

        // Disable time variant DC correction and LUT
        self.reg_set_mask(handle, REG_DC5, 0x03, 0)?;
        self.reg_set_mask(handle, REG_DCTIME1, 0x03, 0)?;
        self.reg_set_mask(handle, REG_DCTIME2, 0x03, 0)?;

        Ok(())
    }

    /// Put the tuner in standby (exit).
    ///
    /// Ports `e4000_exit` which calls `e4k_standby(e4k, 1)`.
    fn exit(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        self.standby(handle, true)
    }

    /// Set the tuner frequency in Hz.
    ///
    /// Ports `e4k_tune_freq`.
    fn set_freq(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq: u32,
    ) -> Result<(), RtlSdrError> {
        // Determine PLL parameters
        let p = Self::compute_pll_params(self.pll.fosc, freq).ok_or_else(|| {
            RtlSdrError::Tuner(format!("E4K: cannot compute PLL params for {freq} Hz"))
        })?;

        // Actually tune to those parameters
        self.tune_params(handle, &p)?;

        // Check PLL lock
        let synth1 = self.read_reg(handle, REG_SYNTH1)?;
        if synth1 & 0x01 == 0 {
            return Err(RtlSdrError::Tuner(format!(
                "E4K: PLL not locked for {freq} Hz"
            )));
        }

        Ok(())
    }

    /// Set the tuner bandwidth in Hz. Returns 0 as the IF frequency.
    ///
    /// Ports `e4000_set_bw` from `librtlsdr.c`.
    fn set_bw(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        bw: u32,
        _sample_rate: u32,
    ) -> Result<u32, RtlSdrError> {
        self.if_filter_bw_set(handle, IfFilter::Mix, bw)?;
        self.if_filter_bw_set(handle, IfFilter::Rc, bw)?;
        self.if_filter_bw_set(handle, IfFilter::Chan, bw)?;
        Ok(0)
    }

    /// Set the tuner gain in tenths of dB.
    ///
    /// Ports `e4000_set_gain` from `librtlsdr.c`.
    fn set_gain(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        gain: i32,
    ) -> Result<(), RtlSdrError> {
        let mixgain: i8 = if gain > MIXER_GAIN_THRESH {
            MIXER_GAIN_12DB
        } else {
            MIXER_GAIN_4DB
        };

        // LNA gain: gain minus mixer contribution, capped at 300 (tenths of dB)
        let lna_gain = (gain - i32::from(mixgain) * 10).min(MAX_LNA_GAIN);
        self.set_lna_gain(handle, lna_gain)?;
        self.mixer_gain_set(handle, mixgain)?;

        Ok(())
    }

    /// Update the crystal frequency.
    fn set_xtal(&mut self, xtal: u32) {
        self.xtal = xtal;
        self.pll.fosc = xtal;
    }

    /// Set manual or automatic gain mode.
    ///
    /// Ports `e4000_set_gain_mode` which calls `e4k_enable_manual_gain`.
    fn set_gain_mode(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        manual: bool,
    ) -> Result<(), RtlSdrError> {
        self.enable_manual_gain(handle, manual)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_i2c_addr() {
        assert_eq!(I2C_ADDR, 0xc8);
    }

    #[test]
    fn test_check_val() {
        assert_eq!(CHECK_VAL, 0x40);
    }

    #[test]
    fn test_check_addr() {
        assert_eq!(CHECK_ADDR, 0x02);
    }

    #[test]
    fn test_gains_sorted() {
        for w in E4K_GAINS.windows(2) {
            assert!(w[0] < w[1], "gains must be in ascending order");
        }
    }

    #[test]
    fn test_gains_count() {
        assert_eq!(E4K_GAINS.len(), 14);
    }

    #[test]
    fn test_gains_values() {
        // Verify exact gain values from C source (tenths of dB)
        assert_eq!(E4K_GAINS[0], -10);
        assert_eq!(E4K_GAINS[13], 420);
    }

    #[test]
    fn test_new_defaults() {
        let tuner = E4kTuner::new(28_800_000);
        assert_eq!(tuner.xtal, 28_800_000);
        assert_eq!(tuner.pll.fosc, 28_800_000);
        assert_eq!(tuner.pll.flo, 0);
    }

    #[test]
    fn test_pll_vars_ordered() {
        for w in PLL_VARS.windows(2) {
            assert!(
                w[0].freq_khz < w[1].freq_khz,
                "PLL vars must be in ascending frequency order"
            );
        }
    }

    #[test]
    fn test_compute_pll_params_valid() {
        // 100 MHz with 28.8 MHz crystal
        let params = E4kTuner::compute_pll_params(28_800_000, 100_000_000);
        assert!(params.is_some());
        let p = params.expect("should compute PLL params");
        assert_eq!(p.fosc, 28_800_000);
        // The actual frequency should be close to 100 MHz
        assert!(
            p.flo.abs_diff(100_000_000) < 100_000,
            "flo should be near 100 MHz, got {}",
            p.flo
        );
    }

    #[test]
    fn test_compute_pll_params_invalid_fosc() {
        // Oscillator too low
        assert!(E4kTuner::compute_pll_params(10_000_000, 100_000_000).is_none());
        // Oscillator too high
        assert!(E4kTuner::compute_pll_params(40_000_000, 100_000_000).is_none());
    }

    #[test]
    fn test_compute_pll_params_various_frequencies() {
        let fosc = 28_800_000;

        // VHF2 range
        let p = E4kTuner::compute_pll_params(fosc, 70_000_000);
        assert!(p.is_some());
        let p = p.expect("VHF2 params");
        assert!(p.flo.abs_diff(70_000_000) < 100_000);

        // UHF range
        let p = E4kTuner::compute_pll_params(fosc, 500_000_000);
        assert!(p.is_some());
        let p = p.expect("UHF params");
        assert!(p.flo.abs_diff(500_000_000) < 100_000);

        // L band range
        let p = E4kTuner::compute_pll_params(fosc, 1_200_000_000);
        assert!(p.is_some());
    }

    #[test]
    fn test_closest_arr_idx() {
        // Test with UHF filter centers
        let idx = E4kTuner::closest_arr_idx(&RF_FILT_CENTER_UHF, 360_000_000);
        assert_eq!(idx, 0);

        let idx = E4kTuner::closest_arr_idx(&RF_FILT_CENTER_UHF, 970_000_000);
        assert_eq!(idx, 15);

        // Test with a frequency between entries
        let idx = E4kTuner::closest_arr_idx(&RF_FILT_CENTER_UHF, 400_000_000);
        // Should be closer to 405 MHz (index 2) than 380 MHz (index 1)
        assert_eq!(idx, 2);
    }

    #[test]
    fn test_choose_rf_filter() {
        // VHF bands always return 0
        assert_eq!(E4kTuner::choose_rf_filter(Band::Vhf2, 100_000_000), 0);
        assert_eq!(E4kTuner::choose_rf_filter(Band::Vhf3, 200_000_000), 0);

        // UHF returns an index
        let idx = E4kTuner::choose_rf_filter(Band::Uhf, 500_000_000);
        assert!(idx < 16);

        // L band returns an index
        let idx = E4kTuner::choose_rf_filter(Band::L, 1_500_000_000);
        assert!(idx < 16);
    }

    #[test]
    fn test_find_if_bw() {
        // Mixer filter: closest to 1900 kHz should be last entry (index 15)
        let idx = E4kTuner::find_if_bw(IfFilter::Mix, 1_900_000);
        assert_eq!(idx, 15);

        // Channel filter: closest to 2150 kHz should be last entry (index 31)
        let idx = E4kTuner::find_if_bw(IfFilter::Chan, 2_150_000);
        assert_eq!(idx, 31);

        // RC filter: closest to 1000 kHz should be last entry (index 15)
        let idx = E4kTuner::find_if_bw(IfFilter::Rc, 1_000_000);
        assert_eq!(idx, 15);
    }

    #[test]
    fn test_find_stage_gain_valid() {
        // Stage 1: -3 -> index 0, 6 -> index 1
        assert_eq!(E4kTuner::find_stage_gain(1, -3).expect("ok"), 0);
        assert_eq!(E4kTuner::find_stage_gain(1, 6).expect("ok"), 1);

        // Stage 2: 0 -> index 0, 9 -> index 3
        assert_eq!(E4kTuner::find_stage_gain(2, 0).expect("ok"), 0);
        assert_eq!(E4kTuner::find_stage_gain(2, 9).expect("ok"), 3);

        // Stage 5: 3 -> index 0, 15 -> index 4
        assert_eq!(E4kTuner::find_stage_gain(5, 3).expect("ok"), 0);
        assert_eq!(E4kTuner::find_stage_gain(5, 15).expect("ok"), 4);
    }

    #[test]
    fn test_find_stage_gain_invalid_stage() {
        assert!(E4kTuner::find_stage_gain(0, 0).is_err());
        assert!(E4kTuner::find_stage_gain(7, 0).is_err());
    }

    #[test]
    fn test_find_stage_gain_invalid_value() {
        assert!(E4kTuner::find_stage_gain(1, 0).is_err());
        assert!(E4kTuner::find_stage_gain(2, 5).is_err());
    }

    #[test]
    fn test_lna_gain_table() {
        // Verify all entries have valid register values (0..14)
        for &(_, reg_val) in &LNA_GAIN {
            assert!(reg_val <= 14, "LNA gain register value out of range");
        }
        // Verify sorted by gain
        for w in LNA_GAIN.windows(2) {
            assert!(w[0].0 < w[1].0, "LNA gain table must be sorted");
        }
    }

    #[test]
    fn test_enh_gain_table() {
        assert_eq!(ENH_GAIN.len(), 4);
        assert_eq!(ENH_GAIN[0], 10);
        assert_eq!(ENH_GAIN[3], 70);
    }

    #[test]
    fn test_if_filter_bw_tables_lengths() {
        assert_eq!(MIX_FILTER_BW.len(), 16);
        assert_eq!(IFRC_FILTER_BW.len(), 16);
        assert_eq!(IFCH_FILTER_BW.len(), 32);
    }

    #[test]
    fn test_rf_filter_center_tables_lengths() {
        assert_eq!(RF_FILT_CENTER_UHF.len(), 16);
        assert_eq!(RF_FILT_CENTER_L.len(), 16);
    }

    #[test]
    fn test_width_mask() {
        assert_eq!(WIDTH_MASK[0], 0);
        assert_eq!(WIDTH_MASK[1], 1);
        assert_eq!(WIDTH_MASK[4], 0x0f);
        assert_eq!(WIDTH_MASK[8], 0xff);
    }

    #[test]
    fn test_magic_init_regs_count() {
        assert_eq!(MAGIC_INIT_REGS.len(), 8);
    }

    #[test]
    fn test_dc_gain_comb() {
        assert_eq!(DC_GAIN_COMB.len(), 4);
        assert_eq!(DC_GAIN_COMB[0].reg, 0x50);
        assert_eq!(DC_GAIN_COMB[3].reg, 0x53);
    }

    #[test]
    fn test_if_gains_max() {
        assert_eq!(IF_GAINS_MAX[1], 6);
        assert_eq!(IF_GAINS_MAX[2], 9);
        assert_eq!(IF_GAINS_MAX[5], 15);
        assert_eq!(IF_GAINS_MAX[6], 15);
    }

    #[test]
    fn test_set_xtal() {
        let mut tuner = E4kTuner::new(28_800_000);
        tuner.set_xtal(26_000_000);
        assert_eq!(tuner.xtal, 26_000_000);
        assert_eq!(tuner.pll.fosc, 26_000_000);
    }

    #[test]
    fn test_band_boundaries() {
        // Verify band boundary constants match C source
        assert_eq!(BAND_VHF2_MAX, 140_000_000);
        assert_eq!(BAND_VHF3_MAX, 350_000_000);
        assert_eq!(BAND_UHF_MAX, 1_135_000_000);
    }

    #[test]
    fn test_pll_params_r_values() {
        // Check that PLL multiplier values match the C source
        assert_eq!(PLL_VARS[0].mult, 48);
        assert_eq!(PLL_VARS[1].mult, 40);
        assert_eq!(PLL_VARS[2].mult, 32);
        assert_eq!(PLL_VARS[3].mult, 24);
        assert_eq!(PLL_VARS[4].mult, 16);
        assert_eq!(PLL_VARS[5].mult, 12);
        assert_eq!(PLL_VARS[6].mult, 8);
        assert_eq!(PLL_VARS[7].mult, 8);
        assert_eq!(PLL_VARS[8].mult, 6);
        assert_eq!(PLL_VARS[9].mult, 4);
    }

    #[test]
    fn test_pll_params_synth7_values() {
        // First 7 entries have 3-phase bit set (bit 3)
        for entry in &PLL_VARS[..7] {
            assert_ne!(
                entry.reg_synth7 & 0x08,
                0,
                "first 7 entries should have 3-phase bit"
            );
        }
        // Last 3 entries do NOT have 3-phase bit set
        for entry in &PLL_VARS[7..] {
            assert_eq!(
                entry.reg_synth7 & 0x08,
                0,
                "last 3 entries should not have 3-phase bit"
            );
        }
    }

    #[test]
    fn test_if_stage_gain_regs() {
        // Stage 0 is unused (dummy)
        assert_eq!(IF_STAGE_GAIN_REGS[0].reg, 0);

        // Stages 1-4 use GAIN3
        for i in 1..=4 {
            assert_eq!(IF_STAGE_GAIN_REGS[i].reg, REG_GAIN3);
        }

        // Stages 5-6 use GAIN4
        for i in 5..=6 {
            assert_eq!(IF_STAGE_GAIN_REGS[i].reg, REG_GAIN4);
        }
    }

    #[test]
    fn test_if_filter_fields() {
        assert_eq!(IF_FILTER_FIELDS[IfFilter::Mix as usize].reg, REG_FILT2);
        assert_eq!(IF_FILTER_FIELDS[IfFilter::Mix as usize].shift, 4);
        assert_eq!(IF_FILTER_FIELDS[IfFilter::Mix as usize].width, 4);

        assert_eq!(IF_FILTER_FIELDS[IfFilter::Chan as usize].reg, REG_FILT3);
        assert_eq!(IF_FILTER_FIELDS[IfFilter::Chan as usize].shift, 0);
        assert_eq!(IF_FILTER_FIELDS[IfFilter::Chan as usize].width, 5);

        assert_eq!(IF_FILTER_FIELDS[IfFilter::Rc as usize].reg, REG_FILT2);
        assert_eq!(IF_FILTER_FIELDS[IfFilter::Rc as usize].shift, 0);
        assert_eq!(IF_FILTER_FIELDS[IfFilter::Rc as usize].width, 4);
    }
}
