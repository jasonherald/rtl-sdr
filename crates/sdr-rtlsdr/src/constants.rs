//! Constants ported from librtlsdr.
//!
//! Includes known device table, crystal frequencies, buffer defaults,
//! and tuner I2C addresses.

/// Known RTL-SDR dongle USB VID/PID pairs.
pub struct KnownDevice {
    pub vid: u16,
    pub pid: u16,
    pub name: &'static str,
}

/// Complete list of known RTL2832-based devices.
/// Ported from librtlsdr `known_devices[]`.
#[allow(clippy::unreadable_literal)]
pub static KNOWN_DEVICES: &[KnownDevice] = &[
    KnownDevice {
        vid: 0x0bda,
        pid: 0x2832,
        name: "Generic RTL2832U",
    },
    KnownDevice {
        vid: 0x0bda,
        pid: 0x2838,
        name: "Generic RTL2832U OEM",
    },
    KnownDevice {
        vid: 0x0413,
        pid: 0x6680,
        name: "DigitalNow Quad DVB-T PCI-E card",
    },
    KnownDevice {
        vid: 0x0413,
        pid: 0x6f0f,
        name: "Leadtek WinFast DTV Dongle mini D",
    },
    KnownDevice {
        vid: 0x0458,
        pid: 0x707f,
        name: "Genius TVGo DVB-T03 USB dongle (Ver. B)",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00a9,
        name: "Terratec Cinergy T Stick Black (rev 1)",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00b3,
        name: "Terratec NOXON DAB/DAB+ USB dongle (rev 1)",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00b4,
        name: "Terratec Deutschlandradio DAB Stick",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00b5,
        name: "Terratec NOXON DAB Stick - Radio Energy",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00b7,
        name: "Terratec Media Broadcast DAB Stick",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00b8,
        name: "Terratec BR DAB Stick",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00b9,
        name: "Terratec WDR DAB Stick",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00c0,
        name: "Terratec MuellerVerlag DAB Stick",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00c6,
        name: "Terratec Fraunhofer DAB Stick",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00d3,
        name: "Terratec Cinergy T Stick RC (Rev.3)",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00d7,
        name: "Terratec T Stick PLUS",
    },
    KnownDevice {
        vid: 0x0ccd,
        pid: 0x00e0,
        name: "Terratec NOXON DAB/DAB+ USB dongle (rev 2)",
    },
    KnownDevice {
        vid: 0x1554,
        pid: 0x5020,
        name: "PixelView PV-DT235U(RN)",
    },
    KnownDevice {
        vid: 0x15f4,
        pid: 0x0131,
        name: "Astrometa DVB-T/DVB-T2",
    },
    KnownDevice {
        vid: 0x15f4,
        pid: 0x0133,
        name: "HanfTek DAB+FM+DVB-T",
    },
    KnownDevice {
        vid: 0x185b,
        pid: 0x0620,
        name: "Compro Videomate U620F",
    },
    KnownDevice {
        vid: 0x185b,
        pid: 0x0650,
        name: "Compro Videomate U650F",
    },
    KnownDevice {
        vid: 0x185b,
        pid: 0x0680,
        name: "Compro Videomate U680F",
    },
    KnownDevice {
        vid: 0x1b80,
        pid: 0xd393,
        name: "GIGABYTE GT-U7300",
    },
    KnownDevice {
        vid: 0x1b80,
        pid: 0xd394,
        name: "DIKOM USB-DVBT HD",
    },
    KnownDevice {
        vid: 0x1b80,
        pid: 0xd395,
        name: "Peak 102569AGPK",
    },
    KnownDevice {
        vid: 0x1b80,
        pid: 0xd397,
        name: "KWorld KW-UB450-T USB DVB-T Pico TV",
    },
    KnownDevice {
        vid: 0x1b80,
        pid: 0xd398,
        name: "Zaapa ZT-MINDVBZP",
    },
    KnownDevice {
        vid: 0x1b80,
        pid: 0xd39d,
        name: "SVEON STV20 DVB-T USB & FM",
    },
    KnownDevice {
        vid: 0x1b80,
        pid: 0xd3a4,
        name: "Twintech UT-40",
    },
    KnownDevice {
        vid: 0x1b80,
        pid: 0xd3a8,
        name: "ASUS U3100MINI_PLUS_V2",
    },
    KnownDevice {
        vid: 0x1b80,
        pid: 0xd3af,
        name: "SVEON STV27 DVB-T USB & FM",
    },
    KnownDevice {
        vid: 0x1b80,
        pid: 0xd3b0,
        name: "SVEON STV21 DVB-T USB & FM",
    },
    KnownDevice {
        vid: 0x1d19,
        pid: 0x1101,
        name: "Dexatek DK DVB-T Dongle (Logilink VG0002A)",
    },
    KnownDevice {
        vid: 0x1d19,
        pid: 0x1102,
        name: "Dexatek DK DVB-T Dongle (MSI DigiVox mini II V3.0)",
    },
    KnownDevice {
        vid: 0x1d19,
        pid: 0x1103,
        name: "Dexatek Technology Ltd. DK 5217 DVB-T Dongle",
    },
    KnownDevice {
        vid: 0x1d19,
        pid: 0x1104,
        name: "MSI DigiVox Micro HD",
    },
    KnownDevice {
        vid: 0x1f4d,
        pid: 0xa803,
        name: "Sweex DVB-T USB",
    },
    KnownDevice {
        vid: 0x1f4d,
        pid: 0xb803,
        name: "GTek T803",
    },
    KnownDevice {
        vid: 0x1f4d,
        pid: 0xc803,
        name: "Lifeview LV5TDeluxe",
    },
    KnownDevice {
        vid: 0x1f4d,
        pid: 0xd286,
        name: "MyGica TD312",
    },
    KnownDevice {
        vid: 0x1f4d,
        pid: 0xd803,
        name: "PROlectrix DV107669",
    },
];

/// Default RTL2832 crystal frequency (28.8 MHz).
pub const DEF_RTL_XTAL_FREQ: u32 = 28_800_000;

/// Minimum acceptable RTL crystal frequency.
pub const MIN_RTL_XTAL_FREQ: u32 = DEF_RTL_XTAL_FREQ - 1000;

/// Maximum acceptable RTL crystal frequency.
pub const MAX_RTL_XTAL_FREQ: u32 = DEF_RTL_XTAL_FREQ + 1000;

/// R828D crystal frequency (16 MHz).
pub const R828D_XTAL_FREQ: u32 = 16_000_000;

/// Default number of async transfer buffers.
pub const DEFAULT_BUF_NUMBER: u32 = 15;

/// Default async transfer buffer length (bytes).
/// 16 * 32 * 512 = 262144 bytes
pub const DEFAULT_BUF_LENGTH: u32 = 16 * 32 * 512;

/// USB control transfer timeout (ms).
pub const CTRL_TIMEOUT: u64 = 300;

/// USB bulk transfer timeout (ms). 0 = no timeout.
pub const BULK_TIMEOUT: u64 = 0;

/// EEPROM I2C address.
pub const EEPROM_ADDR: u8 = 0xa0;

/// FIR filter length.
pub const FIR_LEN: usize = 16;

/// Default FIR coefficients (DAB/FM mode).
/// First 8 are 8-bit signed, next 8 are 12-bit signed.
pub const FIR_DEFAULT: [i32; FIR_LEN] = [
    -54, -36, -41, -40, -32, -14, 14, 53, // 8 bit signed
    101, 156, 215, 273, 327, 372, 404, 421, // 12 bit signed
];

// --- Tuner I2C addresses and check registers ---

/// E4000 tuner I2C address.
pub const E4K_I2C_ADDR: u8 = 0xc8;
/// E4000 check register address.
pub const E4K_CHECK_ADDR: u8 = 0x02;
/// E4000 expected check value.
pub const E4K_CHECK_VAL: u8 = 0x40;

/// FC0012 tuner I2C address.
pub const FC0012_I2C_ADDR: u8 = 0xc6;
/// FC0012 check register address.
pub const FC0012_CHECK_ADDR: u8 = 0x00;
/// FC0012 expected check value.
pub const FC0012_CHECK_VAL: u8 = 0xa1;

/// FC0013 tuner I2C address.
pub const FC0013_I2C_ADDR: u8 = 0xc6;
/// FC0013 check register address.
pub const FC0013_CHECK_ADDR: u8 = 0x00;
/// FC0013 expected check value.
pub const FC0013_CHECK_VAL: u8 = 0xa3;

/// FC2580 tuner I2C address.
pub const FC2580_I2C_ADDR: u8 = 0xac;
/// FC2580 check register address.
pub const FC2580_CHECK_ADDR: u8 = 0x01;
/// FC2580 expected check value.
pub const FC2580_CHECK_VAL: u8 = 0x56;

/// R820T tuner I2C address.
pub const R820T_I2C_ADDR: u8 = 0x34;
/// R828D tuner I2C address.
pub const R828D_I2C_ADDR: u8 = 0x74;
/// R82XX check register address.
pub const R82XX_CHECK_ADDR: u8 = 0x00;
/// R82XX expected check value.
pub const R82XX_CHECK_VAL: u8 = 0x69;
/// R82XX IF frequency (Hz).
pub const R82XX_IF_FREQ: u32 = 3_570_000;

// --- Gain tables (in tenths of dB) ---

/// E4000 gain values (tenths of dB).
pub const E4K_GAINS: &[i32] = &[
    -10, 15, 40, 65, 90, 115, 140, 165, 190, 215, 240, 290, 340, 420,
];

/// FC0012 gain values (tenths of dB).
pub const FC0012_GAINS: &[i32] = &[-99, -40, 71, 179, 192];

/// FC0013 gain values (tenths of dB).
pub const FC0013_GAINS: &[i32] = &[
    -99, -73, -65, -63, -60, -58, -54, 58, 61, 63, 65, 67, 68, 70, 71, 179, 181, 182, 184, 186,
    188, 191, 197,
];

/// FC2580 gain values (tenths of dB).
pub const FC2580_GAINS: &[i32] = &[0];

/// R82XX (R820T/R828D) gain values (tenths of dB).
pub const R82XX_GAINS: &[i32] = &[
    0, 9, 14, 27, 37, 77, 87, 125, 144, 157, 166, 197, 207, 229, 254, 280, 297, 328, 338, 364, 372,
    386, 402, 421, 434, 439, 445, 480, 496,
];

/// Look up a known device by VID/PID.
pub fn find_known_device(vid: u16, pid: u16) -> Option<&'static KnownDevice> {
    KNOWN_DEVICES.iter().find(|d| d.vid == vid && d.pid == pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_known_device() {
        assert!(find_known_device(0x0bda, 0x2832).is_some());
        assert!(find_known_device(0x0bda, 0x2838).is_some());
        assert!(find_known_device(0xffff, 0xffff).is_none());
    }

    #[test]
    fn test_fir_default_length() {
        assert_eq!(FIR_DEFAULT.len(), FIR_LEN);
    }

    #[test]
    fn test_known_devices_count() {
        // librtlsdr has 42 known devices
        assert_eq!(KNOWN_DEVICES.len(), 42);
    }
}
