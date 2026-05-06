//! Tuner driver trait and implementations.
//!
//! Each tuner IC (R820T, E4000, FC0012, etc.) implements the `Tuner` trait,
//! providing frequency, gain, and bandwidth control via I2C.

// Per-tuner backends carry the IC's I2C register tables transcribed
// from upstream `librtlsdr`. Some entries aren't called from Rust
// today but are kept for completeness so adding a hardware feature
// later is a register-table read rather than a re-port. Scoped
// `dead_code` allow per-module rather than crate-level so dead paths
// in non-port code still get caught. Per #630 CR round 2.
#[allow(dead_code)]
pub(crate) mod e4k;
#[allow(dead_code)]
pub(crate) mod fc0012;
#[allow(dead_code)]
pub(crate) mod fc0013;
#[allow(dead_code)]
pub(crate) mod fc2580;
pub(crate) mod r82xx;

use crate::error::RtlSdrError;

/// Trait for a tuner IC driver.
///
/// Tuners communicate with the RTL2832 via I2C. The I2C repeater must be
/// enabled before calling these methods and disabled after.
pub trait Tuner: Send {
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

    /// Set the gain of an IF stage, in tenths of dB.
    ///
    /// Ports `rtlsdr_set_tuner_if_gain`. Only the E4000 programs IF
    /// stages meaningfully — R820T / R828D / FC0012 / FC0013 /
    /// FC2580 have no IF-stage controls and silently no-op (matches
    /// upstream librtlsdr's per-tuner `set_if_gain` dispatch where
    /// those tuners return 0 without side effects). The default
    /// trait impl captures that no-op behavior so tuner modules
    /// only override when they actually do something.
    ///
    /// `stage` is 1-based (stage 1 through 6 on the E4000).
    /// `gain` is signed tenths of dB on the wire; E4000 converts
    /// to integer dB internally because its stage-gain LUTs are
    /// integer-valued.
    fn set_if_gain(
        &mut self,
        _handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        _stage: i32,
        _gain: i32,
    ) -> Result<(), RtlSdrError> {
        Ok(())
    }
}
