//! Demodulator trait and implementations for all radio modes.
//!
//! Each demodulator converts complex IF samples into stereo audio samples.
//! The demod mode determines the IF sample rate, bandwidth range, and
//! which DSP primitives are used internally.

mod am;
mod cw;
mod dsb;
mod lsb;
mod nfm;
mod raw;
mod usb;
mod wfm;

pub use am::AmDemodulator;
pub use cw::CwDemodulator;
pub use dsb::DsbDemodulator;
pub use lsb::LsbDemodulator;
pub use nfm::NfmDemodulator;
pub use raw::RawDemodulator;
pub use usb::UsbDemodulator;
pub use wfm::WfmDemodulator;

use sdr_types::{Complex, DspError, Stereo};

/// VFO reference point — where the VFO marker sits relative to the passband.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VfoReference {
    /// VFO is at the center of the passband.
    Center,
    /// VFO is at the lower edge (USB convention).
    Lower,
    /// VFO is at the upper edge (LSB convention).
    Upper,
}

/// Static configuration describing a demodulator's capabilities and defaults.
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct DemodConfig {
    /// IF (intermediate frequency) sample rate in Hz that this demod expects.
    pub if_sample_rate: f64,
    /// Audio sample rate produced by this demod (before AF chain resampling).
    pub af_sample_rate: f64,
    /// Default channel bandwidth in Hz.
    pub default_bandwidth: f64,
    /// Minimum allowed bandwidth in Hz.
    pub min_bandwidth: f64,
    /// Maximum allowed bandwidth in Hz.
    pub max_bandwidth: f64,
    /// Whether the bandwidth is locked (cannot be changed by the user).
    pub bandwidth_locked: bool,
    /// Default frequency snap interval in Hz (0 = no snap).
    pub default_snap_interval: f64,
    /// Where the VFO marker sits relative to the passband.
    pub vfo_reference: VfoReference,
    /// Whether deemphasis filtering is applicable to this mode.
    pub deemp_allowed: bool,
    /// Whether post-processing (AF chain) is enabled by default.
    pub post_proc_enabled: bool,
    /// Default deemphasis time constant in seconds (0 = none).
    pub default_deemp_tau: f64,
    /// Whether FM IF noise reduction is applicable.
    pub fm_if_nr_allowed: bool,
    /// Whether noise blanker is applicable.
    pub nb_allowed: bool,
    /// Whether high-pass filter is applicable.
    pub high_pass_allowed: bool,
    /// Whether squelch is applicable.
    pub squelch_allowed: bool,
}

/// Trait for all demodulator implementations.
///
/// Each demod converts complex IF samples into stereo audio. The IF sample
/// rate is fixed per mode (see [`DemodConfig::if_sample_rate`]).
pub trait Demodulator {
    /// Process complex IF samples into stereo audio.
    ///
    /// # Errors
    ///
    /// Returns `DspError` on buffer size or processing errors.
    fn process(&mut self, input: &[Complex], output: &mut [Stereo]) -> Result<usize, DspError>;

    /// Update the channel bandwidth.
    fn set_bandwidth(&mut self, bw: f64);

    /// Get the static configuration for this demod mode.
    fn config(&self) -> &DemodConfig;

    /// Human-readable name of this demod mode.
    fn name(&self) -> &'static str;
}

/// Create a boxed demodulator for the given mode.
///
/// # Errors
///
/// Returns `DspError::InvalidParameter` if the demod cannot be constructed.
pub fn create_demodulator(
    mode: sdr_types::DemodMode,
) -> Result<Box<dyn Demodulator + Send>, DspError> {
    use sdr_types::DemodMode;
    match mode {
        DemodMode::Wfm => Ok(Box::new(WfmDemodulator::new()?)),
        DemodMode::Nfm => Ok(Box::new(NfmDemodulator::new()?)),
        DemodMode::Am => Ok(Box::new(AmDemodulator::new())),
        DemodMode::Usb => Ok(Box::new(UsbDemodulator::new()?)),
        DemodMode::Lsb => Ok(Box::new(LsbDemodulator::new()?)),
        DemodMode::Dsb => Ok(Box::new(DsbDemodulator::new()?)),
        DemodMode::Cw => Ok(Box::new(CwDemodulator::new()?)),
        DemodMode::Raw => Ok(Box::new(RawDemodulator::new())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use sdr_types::DemodMode;

    #[test]
    fn test_create_all_demods() {
        let modes = [
            DemodMode::Wfm,
            DemodMode::Nfm,
            DemodMode::Am,
            DemodMode::Usb,
            DemodMode::Lsb,
            DemodMode::Dsb,
            DemodMode::Cw,
            DemodMode::Raw,
        ];
        for mode in modes {
            let demod = create_demodulator(mode);
            assert!(demod.is_ok(), "failed to create demod for {mode:?}");
        }
    }

    #[test]
    fn test_demod_configs_have_valid_ranges() {
        let modes = [
            DemodMode::Wfm,
            DemodMode::Nfm,
            DemodMode::Am,
            DemodMode::Usb,
            DemodMode::Lsb,
            DemodMode::Dsb,
            DemodMode::Cw,
            DemodMode::Raw,
        ];
        for mode in modes {
            let demod = create_demodulator(mode).unwrap();
            let cfg = demod.config();
            assert!(cfg.if_sample_rate > 0.0, "{mode:?} IF SR must be > 0");
            assert!(cfg.af_sample_rate > 0.0, "{mode:?} AF SR must be > 0");
            assert!(
                cfg.min_bandwidth <= cfg.default_bandwidth,
                "{mode:?} min_bw <= default_bw"
            );
            assert!(
                cfg.default_bandwidth <= cfg.max_bandwidth,
                "{mode:?} default_bw <= max_bw"
            );
            assert!(!demod.name().is_empty(), "{mode:?} name must not be empty");
        }
    }
}
