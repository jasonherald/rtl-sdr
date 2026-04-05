//! RTL2832U register definitions.
//!
//! Exact port of librtlsdr register enums and block IDs.

/// USB register addresses.
#[allow(dead_code)]
pub mod usb_reg {
    pub const USB_SYSCTL: u16 = 0x2000;
    pub const USB_CTRL: u16 = 0x2010;
    pub const USB_STAT: u16 = 0x2014;
    pub const USB_EPA_CFG: u16 = 0x2144;
    pub const USB_EPA_CTL: u16 = 0x2148;
    pub const USB_EPA_MAXPKT: u16 = 0x2158;
    pub const USB_EPA_MAXPKT_2: u16 = 0x215a;
    pub const USB_EPA_FIFO_CFG: u16 = 0x2160;
}

/// System register addresses.
#[allow(dead_code)]
pub mod sys_reg {
    pub const DEMOD_CTL: u16 = 0x3000;
    pub const GPO: u16 = 0x3001;
    pub const GPI: u16 = 0x3002;
    pub const GPOE: u16 = 0x3003;
    pub const GPD: u16 = 0x3004;
    pub const SYSINTE: u16 = 0x3005;
    pub const SYSINTS: u16 = 0x3006;
    pub const GP_CFG0: u16 = 0x3007;
    pub const GP_CFG1: u16 = 0x3008;
    pub const SYSINTE_1: u16 = 0x3009;
    pub const SYSINTS_1: u16 = 0x300a;
    pub const DEMOD_CTL_1: u16 = 0x300b;
    pub const IR_SUSPEND: u16 = 0x300c;
}

/// Block IDs for register addressing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Block {
    /// Demodulator block.
    Demod = 0,
    /// USB block.
    Usb = 1,
    /// System block.
    Sys = 2,
    /// Tuner block.
    Tuner = 3,
    /// ROM block.
    Rom = 4,
    /// IR block.
    Ir = 5,
    /// I2C block.
    Iic = 6,
}

/// Tuner type detected on the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunerType {
    /// No tuner detected.
    Unknown,
    /// Elonics E4000.
    E4000,
    /// Fitipower FC0012.
    Fc0012,
    /// Fitipower FC0013.
    Fc0013,
    /// Fitipower FC2580.
    Fc2580,
    /// Rafael Micro R820T.
    R820T,
    /// Rafael Micro R828D.
    R828D,
}

impl TunerType {
    /// Get the gain table for this tuner type (in tenths of dB).
    pub fn gains(&self) -> &'static [i32] {
        use crate::constants::*;
        match self {
            Self::E4000 => E4K_GAINS,
            Self::Fc0012 => FC0012_GAINS,
            Self::Fc0013 => FC0013_GAINS,
            Self::Fc2580 => FC2580_GAINS,
            Self::R820T | Self::R828D => R82XX_GAINS,
            Self::Unknown => &[0],
        }
    }
}

/// Async streaming status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsyncStatus {
    /// Not streaming.
    Inactive,
    /// Cancellation in progress.
    Canceling,
    /// Streaming active.
    Running,
}
