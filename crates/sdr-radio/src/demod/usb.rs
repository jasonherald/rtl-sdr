//! Upper sideband (USB) demodulator.

use sdr_dsp::demod::{SsbDemod, SsbMode};
use sdr_types::{Complex, DspError, Stereo};

use super::{DemodConfig, Demodulator, VfoReference};

/// IF sample rate for USB mode (Hz).
const USB_IF_SAMPLE_RATE: f64 = 24_000.0;

/// AF (audio) sample rate produced by USB demod (Hz).
const USB_AF_SAMPLE_RATE: f64 = 24_000.0;

/// Default channel bandwidth for USB (Hz).
const USB_DEFAULT_BANDWIDTH: f64 = 2_800.0;

/// Minimum bandwidth for USB (Hz).
const USB_MIN_BANDWIDTH: f64 = 500.0;

/// Maximum bandwidth for USB (Hz).
const USB_MAX_BANDWIDTH: f64 = 12_000.0;

/// Default frequency snap interval for USB (Hz).
const USB_SNAP_INTERVAL: f64 = 100.0;

/// Upper sideband demodulator using `SsbDemod(Usb)` from sdr-dsp.
pub struct UsbDemodulator {
    demod: SsbDemod,
    config: DemodConfig,
    mono_buf: Vec<f32>,
}

impl UsbDemodulator {
    /// Create a new USB demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the underlying SSB demod cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = SsbDemod::new(SsbMode::Usb, USB_DEFAULT_BANDWIDTH, USB_IF_SAMPLE_RATE)?;
        let config = DemodConfig {
            if_sample_rate: USB_IF_SAMPLE_RATE,
            af_sample_rate: USB_AF_SAMPLE_RATE,
            default_bandwidth: USB_DEFAULT_BANDWIDTH,
            min_bandwidth: USB_MIN_BANDWIDTH,
            max_bandwidth: USB_MAX_BANDWIDTH,
            bandwidth_locked: false,
            default_snap_interval: USB_SNAP_INTERVAL,
            vfo_reference: VfoReference::Lower,
            deemp_allowed: false,
            post_proc_enabled: true,
            default_deemp_tau: 0.0,
            fm_if_nr_allowed: false,
            nb_allowed: true,
            high_pass_allowed: true,
            squelch_allowed: false,
        };
        Ok(Self {
            demod,
            config,
            mono_buf: Vec::new(),
        })
    }
}

impl Demodulator for UsbDemodulator {
    fn process(&mut self, input: &[Complex], output: &mut [Stereo]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        self.mono_buf.resize(input.len(), 0.0);
        let count = self.demod.process(input, &mut self.mono_buf)?;
        sdr_dsp::convert::mono_to_stereo(&self.mono_buf[..count], &mut output[..count])?;
        Ok(count)
    }

    fn set_bandwidth(&mut self, bw: f64) {
        // SSB demod uses bandwidth for frequency translation offset.
        // Ignore errors from out-of-range values silently (UI should clamp).
        let _ = self.demod.set_bandwidth(bw);
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "USB"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    #[test]
    fn test_usb_config() {
        let demod = UsbDemodulator::new().unwrap();
        let cfg = demod.config();
        assert!((cfg.if_sample_rate - 24_000.0).abs() < f64::EPSILON);
        assert!((cfg.default_bandwidth - 2_800.0).abs() < f64::EPSILON);
        assert_eq!(cfg.vfo_reference, VfoReference::Lower);
    }

    #[test]
    fn test_usb_process_produces_audio() {
        let mut demod = UsbDemodulator::new().unwrap();
        let input: Vec<Complex> = (0..1000)
            .map(|i| {
                let phase = 2.0 * PI * 1000.0 * (i as f32) / 24_000.0;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![Stereo::default(); 1000];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
        let peak = output[100..]
            .iter()
            .map(|s| s.l.abs())
            .fold(0.0_f32, f32::max);
        assert!(peak > 0.3, "USB should produce audio, peak = {peak}");
    }
}
