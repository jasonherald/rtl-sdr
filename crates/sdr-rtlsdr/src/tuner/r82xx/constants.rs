//! R82XX tuner constants — init array, frequency ranges, gain tables.
//!
//! Exact port of tuner_r82xx.c and tuner_r82xx.h static data.

/// Shadow register start offset.
pub const REG_SHADOW_START: u8 = 5;

/// Number of shadow registers.
pub const NUM_REGS: usize = 30;

/// Number of IMR calibration points.
pub const NUM_IMR: usize = 5;

/// Version number written to register.
pub const VER_NUM: u8 = 49;

/// R82XX chip variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum R82xxChip {
    R820T,
    R620D,
    R828D,
    R828,
    R828S,
    R820C,
}

/// Tuner type for configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum R82xxTunerType {
    Radio = 1,
    AnalogTv = 2,
    DigitalTv = 3,
}

/// Crystal capacitor selection values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum XtalCapValue {
    LowCap30p = 0,
    LowCap20p = 1,
    LowCap10p = 2,
    LowCap0p = 3,
    HighCap0p = 4,
}

/// Delivery system enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum DeliverySystem {
    Undefined = 0,
    DvbT = 1,
    DvbT2 = 2,
    IsdBt = 3,
}

/// Number of init register values (0x05 to 0x1f = 27 registers).
pub const NUM_INIT_REGS: usize = 27;

/// Initial register values (registers 0x05 to 0x1f).
/// Exact port of `r82xx_init_array`.
pub const R82XX_INIT_ARRAY: [u8; NUM_INIT_REGS] = [
    0x83, 0x32, 0x75, // 05 to 07
    0xc0, 0x40, 0xd6, 0x6c, // 08 to 0b
    0xf5, 0x63, 0x75, 0x68, // 0c to 0f
    0x6c, 0x83, 0x80, 0x00, // 10 to 13
    0x0f, 0x00, 0xc0, 0x30, // 14 to 17
    0x48, 0xcc, 0x60, 0x00, // 18 to 1b
    0x54, 0xae, 0x4a, 0xc0, // 1c to 1f
];

/// Frequency range configuration for RF mux and tracking filter.
#[derive(Clone, Copy, Debug)]
pub struct FreqRange {
    /// Start frequency in MHz.
    pub freq: u32,
    /// Open drain control.
    pub open_d: u8,
    /// RF mux / polymux setting.
    pub rf_mux_ploy: u8,
    /// TF band setting.
    pub tf_c: u8,
    /// Crystal cap 20pF setting.
    pub xtal_cap20p: u8,
    /// Crystal cap 10pF setting.
    pub xtal_cap10p: u8,
    /// Crystal cap 0pF setting.
    pub xtal_cap0p: u8,
}

/// Complete frequency range table. Exact port of `freq_ranges[]`.
pub static FREQ_RANGES: &[FreqRange] = &[
    FreqRange {
        freq: 0,
        open_d: 0x08,
        rf_mux_ploy: 0x02,
        tf_c: 0xdf,
        xtal_cap20p: 0x02,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 50,
        open_d: 0x08,
        rf_mux_ploy: 0x02,
        tf_c: 0xbe,
        xtal_cap20p: 0x02,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 55,
        open_d: 0x08,
        rf_mux_ploy: 0x02,
        tf_c: 0x8b,
        xtal_cap20p: 0x02,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 60,
        open_d: 0x08,
        rf_mux_ploy: 0x02,
        tf_c: 0x7b,
        xtal_cap20p: 0x02,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 65,
        open_d: 0x08,
        rf_mux_ploy: 0x02,
        tf_c: 0x69,
        xtal_cap20p: 0x02,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 70,
        open_d: 0x08,
        rf_mux_ploy: 0x02,
        tf_c: 0x58,
        xtal_cap20p: 0x02,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 75,
        open_d: 0x00,
        rf_mux_ploy: 0x02,
        tf_c: 0x44,
        xtal_cap20p: 0x02,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 80,
        open_d: 0x00,
        rf_mux_ploy: 0x02,
        tf_c: 0x44,
        xtal_cap20p: 0x02,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 90,
        open_d: 0x00,
        rf_mux_ploy: 0x02,
        tf_c: 0x34,
        xtal_cap20p: 0x01,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 100,
        open_d: 0x00,
        rf_mux_ploy: 0x02,
        tf_c: 0x34,
        xtal_cap20p: 0x01,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 110,
        open_d: 0x00,
        rf_mux_ploy: 0x02,
        tf_c: 0x24,
        xtal_cap20p: 0x01,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 120,
        open_d: 0x00,
        rf_mux_ploy: 0x02,
        tf_c: 0x24,
        xtal_cap20p: 0x01,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 140,
        open_d: 0x00,
        rf_mux_ploy: 0x02,
        tf_c: 0x14,
        xtal_cap20p: 0x01,
        xtal_cap10p: 0x01,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 180,
        open_d: 0x00,
        rf_mux_ploy: 0x02,
        tf_c: 0x13,
        xtal_cap20p: 0x00,
        xtal_cap10p: 0x00,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 220,
        open_d: 0x00,
        rf_mux_ploy: 0x02,
        tf_c: 0x13,
        xtal_cap20p: 0x00,
        xtal_cap10p: 0x00,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 250,
        open_d: 0x00,
        rf_mux_ploy: 0x02,
        tf_c: 0x11,
        xtal_cap20p: 0x00,
        xtal_cap10p: 0x00,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 280,
        open_d: 0x00,
        rf_mux_ploy: 0x02,
        tf_c: 0x00,
        xtal_cap20p: 0x00,
        xtal_cap10p: 0x00,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 310,
        open_d: 0x00,
        rf_mux_ploy: 0x41,
        tf_c: 0x00,
        xtal_cap20p: 0x00,
        xtal_cap10p: 0x00,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 450,
        open_d: 0x00,
        rf_mux_ploy: 0x41,
        tf_c: 0x00,
        xtal_cap20p: 0x00,
        xtal_cap10p: 0x00,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 588,
        open_d: 0x00,
        rf_mux_ploy: 0x40,
        tf_c: 0x00,
        xtal_cap20p: 0x00,
        xtal_cap10p: 0x00,
        xtal_cap0p: 0x00,
    },
    FreqRange {
        freq: 650,
        open_d: 0x00,
        rf_mux_ploy: 0x40,
        tf_c: 0x00,
        xtal_cap20p: 0x00,
        xtal_cap10p: 0x00,
        xtal_cap0p: 0x00,
    },
];

/// Crystal capacitor test values for xtal_check.
/// Format: (register value, XtalCapValue enum discriminant).
pub static XTAL_CAPACITOR: &[(u8, XtalCapValue)] = &[
    (0x0b, XtalCapValue::LowCap30p),
    (0x02, XtalCapValue::LowCap20p),
    (0x01, XtalCapValue::LowCap10p),
    (0x00, XtalCapValue::LowCap0p),
    (0x10, XtalCapValue::HighCap0p),
];

/// VGA base gain (dB).
pub const VGA_BASE_GAIN: i32 = -47;

/// VGA gain steps (cumulative from VGA_BASE_GAIN).
pub const R82XX_VGA_GAIN_STEPS: [i32; 16] = [
    0, 26, 26, 30, 42, 35, 24, 13, 14, 32, 36, 34, 35, 37, 35, 36,
];

/// LNA gain steps (tenths of dB).
pub const R82XX_LNA_GAIN_STEPS: [i32; 16] =
    [0, 9, 13, 40, 38, 13, 31, 22, 26, 31, 26, 14, 19, 5, 35, 13];

/// Mixer gain steps (tenths of dB).
pub const R82XX_MIXER_GAIN_STEPS: [i32; 16] =
    [0, 5, 10, 10, 19, 9, 10, 25, 17, 10, 8, 16, 13, 6, 3, -8];

/// IF low-pass filter bandwidth table (Hz).
pub const IF_LOW_PASS_BW_TABLE: [i32; 10] = [
    1_700_000, 1_600_000, 1_550_000, 1_450_000, 1_200_000, 900_000, 700_000, 550_000, 450_000,
    350_000,
];

/// High-pass filter bandwidth contribution 1 (Hz).
pub const FILT_HP_BW1: i32 = 350_000;

/// High-pass filter bandwidth contribution 2 (Hz).
pub const FILT_HP_BW2: i32 = 380_000;

/// Frequency band constants for RTL-SDR Blog V4.
pub const HF: u8 = 1;
pub const VHF: u8 = 2;
pub const UHF: u8 = 3;

/// Bit-reversal lookup table for I2C read data.
pub const BITREV_LUT: [u8; 16] = [
    0x0, 0x8, 0x4, 0xc, 0x2, 0xa, 0x6, 0xe, 0x1, 0x9, 0x5, 0xd, 0x3, 0xb, 0x7, 0xf,
];

/// Reverse the bits of a byte (used for I2C read data).
#[inline]
pub fn bitrev(byte: u8) -> u8 {
    (BITREV_LUT[(byte & 0xf) as usize] << 4) | BITREV_LUT[(byte >> 4) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_array_length() {
        assert_eq!(R82XX_INIT_ARRAY.len(), NUM_INIT_REGS);
        // Init covers 0x05 to 0x1f = 27 registers
        assert_eq!(NUM_INIT_REGS, 27);
    }

    #[test]
    fn test_freq_ranges_ordered() {
        for w in FREQ_RANGES.windows(2) {
            assert!(w[0].freq < w[1].freq, "freq ranges must be ascending");
        }
    }

    #[test]
    fn test_bitrev() {
        assert_eq!(bitrev(0x00), 0x00);
        assert_eq!(bitrev(0xff), 0xff);
        assert_eq!(bitrev(0x01), 0x80);
        assert_eq!(bitrev(0x80), 0x01);
        assert_eq!(bitrev(0x69), 0x96); // R82XX check val
    }

    #[test]
    fn test_gain_tables_length() {
        assert_eq!(R82XX_VGA_GAIN_STEPS.len(), 16);
        assert_eq!(R82XX_LNA_GAIN_STEPS.len(), 16);
        assert_eq!(R82XX_MIXER_GAIN_STEPS.len(), 16);
    }
}
