//! Tuner driver trait and implementations.
//!
//! Each tuner IC (R820T, E4000, FC0012, etc.) implements the `Tuner` trait,
//! providing frequency, gain, and bandwidth control via I2C.

pub mod r82xx;

use crate::error::RtlSdrError;

/// Trait for a tuner IC driver.
///
/// Tuners communicate with the RTL2832 via I2C. The I2C repeater must be
/// enabled before calling these methods and disabled after.
pub trait Tuner {
    /// Initialize the tuner.
    fn init(&mut self, handle: &rusb::DeviceHandle<rusb::GlobalContext>)
    -> Result<(), RtlSdrError>;

    /// Put the tuner in standby / exit.
    fn exit(&mut self, handle: &rusb::DeviceHandle<rusb::GlobalContext>)
    -> Result<(), RtlSdrError>;

    /// Set the tuner frequency in Hz.
    fn set_freq(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq: u32,
    ) -> Result<(), RtlSdrError>;

    /// Set the tuner bandwidth in Hz. Returns the IF frequency to use.
    fn set_bw(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        bw: u32,
        sample_rate: u32,
    ) -> Result<u32, RtlSdrError>;

    /// Set the tuner gain in tenths of dB.
    fn set_gain(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        gain: i32,
    ) -> Result<(), RtlSdrError>;

    /// Update the crystal frequency (for PPM correction propagation).
    fn set_xtal(&mut self, xtal: u32);

    /// Set manual (1) or automatic (0) gain mode.
    fn set_gain_mode(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        manual: bool,
    ) -> Result<(), RtlSdrError>;
}
