//! RTL-SDR device — high-level API for device control.
//!
//! Cast-heavy code is inherent in a faithful hardware register port.

//!
//! Ports the rtlsdr_dev_t struct and public API functions from librtlsdr.

use crate::constants::*;
use crate::error::RtlSdrError;
use crate::reg::{Block, TunerType};
use crate::tuner::Tuner;
use crate::tuner::r82xx::{R82xxConfig, R82xxPriv};
use crate::usb;

/// RTL-SDR device handle.
///
/// Ports `struct rtlsdr_dev` from librtlsdr. Manages the USB connection,
/// baseband configuration, and tuner driver.
pub struct RtlSdrDevice {
    handle: rusb::DeviceHandle<rusb::GlobalContext>,
    tuner_type: TunerType,
    tuner: Option<Box<dyn Tuner>>,

    // RTL demod context
    rtl_xtal: u32,
    tun_xtal: u32,
    rate: u32,
    freq: u32,
    bw: u32,
    offs_freq: u32,
    corr: i32,
    gain: i32,
    direct_sampling: i32,
    fir: [i32; FIR_LEN],

    // Device info
    manufact: String,
    product: String,
}

impl RtlSdrDevice {
    /// Open an RTL-SDR device by index.
    ///
    /// Ports `rtlsdr_open`. Initializes the baseband, probes the tuner,
    /// and configures the device for SDR mode.
    pub fn open(index: u32) -> Result<Self, RtlSdrError> {
        // Find the device
        let (device, _dd) = find_device_by_index(index)?;

        // Open USB handle
        let handle = device.open()?;

        // Try to claim interface
        if handle.kernel_driver_active(0).unwrap_or(false) {
            let _ = handle.detach_kernel_driver(0);
        }
        handle.claim_interface(0)?;

        let mut dev = Self {
            handle,
            tuner_type: TunerType::Unknown,
            tuner: None,
            rtl_xtal: DEF_RTL_XTAL_FREQ,
            tun_xtal: DEF_RTL_XTAL_FREQ,
            rate: 0,
            freq: 0,
            bw: 0,
            offs_freq: 0,
            corr: 0,
            gain: 0,
            direct_sampling: 0,
            fir: FIR_DEFAULT,
            manufact: String::new(),
            product: String::new(),
        };

        // Perform a dummy write to test connectivity; reset if it fails
        if usb::write_reg(
            &dev.handle,
            Block::Usb,
            crate::reg::usb_reg::USB_SYSCTL,
            0x09,
            1,
        )
        .is_err()
        {
            tracing::warn!("dummy write failed, resetting device");
            let _ = dev.handle.reset();
        }

        // Initialize baseband
        usb::init_baseband(&dev.handle, &dev.fir)?;

        // Get manufacturer and product strings
        if let Ok(dd) = dev.handle.device().device_descriptor() {
            dev.manufact = dev
                .handle
                .read_manufacturer_string_ascii(&dd)
                .unwrap_or_default();
            dev.product = dev
                .handle
                .read_product_string_ascii(&dd)
                .unwrap_or_default();
        }

        // Probe tuners
        usb::set_i2c_repeater(&dev.handle, true)?;
        dev.probe_tuner()?;
        usb::set_i2c_repeater(&dev.handle, false)?;

        Ok(dev)
    }

    /// Probe for supported tuner ICs.
    fn probe_tuner(&mut self) -> Result<(), RtlSdrError> {
        // Try E4000
        if let Ok(reg) = usb::i2c_read_reg(&self.handle, E4K_I2C_ADDR, E4K_CHECK_ADDR) {
            if reg == E4K_CHECK_VAL {
                tracing::info!("Found Elonics E4000 tuner");
                self.tuner_type = TunerType::E4000;
                // TODO: Initialize E4000 tuner driver
                return Ok(());
            }
        }

        // Try FC0013
        if let Ok(reg) = usb::i2c_read_reg(&self.handle, FC0013_I2C_ADDR, FC0013_CHECK_ADDR) {
            if reg == FC0013_CHECK_VAL {
                tracing::info!("Found Fitipower FC0013 tuner");
                self.tuner_type = TunerType::Fc0013;
                return Ok(());
            }
        }

        // Try R820T
        if let Ok(reg) = usb::i2c_read_reg(&self.handle, R820T_I2C_ADDR, R82XX_CHECK_ADDR) {
            if reg == R82XX_CHECK_VAL {
                tracing::info!("Found Rafael Micro R820T tuner");
                self.tuner_type = TunerType::R820T;
                self.init_r82xx_tuner()?;
                return Ok(());
            }
        }

        // Try R828D
        if let Ok(reg) = usb::i2c_read_reg(&self.handle, R828D_I2C_ADDR, R82XX_CHECK_ADDR) {
            if reg == R82XX_CHECK_VAL {
                tracing::info!("Found Rafael Micro R828D tuner");
                self.tuner_type = TunerType::R828D;
                self.init_r82xx_tuner()?;
                return Ok(());
            }
        }

        // Try FC2580 (needs GPIO reset first)
        let _ = usb::set_gpio_output(&self.handle, 4);
        let _ = usb::set_gpio_bit(&self.handle, 4, true);
        let _ = usb::set_gpio_bit(&self.handle, 4, false);

        if let Ok(reg) = usb::i2c_read_reg(&self.handle, FC2580_I2C_ADDR, FC2580_CHECK_ADDR) {
            if (reg & 0x7f) == FC2580_CHECK_VAL {
                tracing::info!("Found FCI 2580 tuner");
                self.tuner_type = TunerType::Fc2580;
                return Ok(());
            }
        }

        // Try FC0012
        if let Ok(reg) = usb::i2c_read_reg(&self.handle, FC0012_I2C_ADDR, FC0012_CHECK_ADDR) {
            if reg == FC0012_CHECK_VAL {
                tracing::info!("Found Fitipower FC0012 tuner");
                let _ = usb::set_gpio_output(&self.handle, 6);
                self.tuner_type = TunerType::Fc0012;
                return Ok(());
            }
        }

        tracing::warn!("No supported tuner found");
        Ok(())
    }

    /// Initialize the R82XX (R820T/R828D) tuner.
    fn init_r82xx_tuner(&mut self) -> Result<(), RtlSdrError> {
        let (i2c_addr, chip) = match self.tuner_type {
            TunerType::R828D => {
                // Check if Blog V4
                let is_v4 = self.manufact == "RTLSDRBlog" && self.product == "Blog V4";
                if !is_v4 {
                    self.tun_xtal = R828D_XTAL_FREQ;
                }
                (
                    R828D_I2C_ADDR,
                    crate::tuner::r82xx::constants::R82xxChip::R828D,
                )
            }
            _ => (
                R820T_I2C_ADDR,
                crate::tuner::r82xx::constants::R82xxChip::R820T,
            ),
        };

        // Configure R82XX specific baseband settings
        // Disable Zero-IF mode
        usb::demod_write_reg(&self.handle, 1, 0xb1, 0x1a, 1)?;
        // Only enable In-phase ADC input
        usb::demod_write_reg(&self.handle, 0, 0x08, 0x4d, 1)?;
        // Set IF frequency
        self.set_if_freq(R82XX_IF_FREQ)?;
        // Enable spectrum inversion
        usb::demod_write_reg(&self.handle, 1, 0x15, 0x01, 1)?;

        let xtal = self.get_tuner_xtal();
        let config = R82xxConfig {
            i2c_addr,
            xtal,
            rafael_chip: chip,
            max_i2c_msg_len: 8,
            use_predetect: false,
        };

        let mut r82xx = R82xxPriv::new(&config);
        let is_v4 = self.manufact == "RTLSDRBlog" && self.product == "Blog V4";
        r82xx.set_blog_v4(is_v4);
        r82xx.init(&self.handle)?;
        self.tuner = Some(Box::new(r82xx));

        Ok(())
    }

    /// Set IF frequency.
    ///
    /// Ports `rtlsdr_set_if_freq`.
    fn set_if_freq(&self, freq: u32) -> Result<(), RtlSdrError> {
        let rtl_xtal = self.get_rtl_xtal();
        let if_freq = ((f64::from(freq) * (1u64 << 22) as f64) / f64::from(rtl_xtal)) as i32 * -1;

        let tmp = ((if_freq >> 16) & 0x3f) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x19, tmp, 1)?;
        let tmp = ((if_freq >> 8) & 0xff) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x1a, tmp, 1)?;
        let tmp = (if_freq & 0xff) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x1b, tmp, 1)?;

        Ok(())
    }

    /// Get corrected RTL crystal frequency.
    fn get_rtl_xtal(&self) -> u32 {
        (f64::from(self.rtl_xtal) * (1.0 + f64::from(self.corr) / 1e6)) as u32
    }

    /// Get corrected tuner crystal frequency.
    fn get_tuner_xtal(&self) -> u32 {
        (f64::from(self.tun_xtal) * (1.0 + f64::from(self.corr) / 1e6)) as u32
    }

    // --- Public API ---

    /// Get the tuner type.
    pub fn tuner_type(&self) -> TunerType {
        self.tuner_type
    }

    /// Get available gain values for the current tuner (in tenths of dB).
    pub fn tuner_gains(&self) -> &[i32] {
        self.tuner_type.gains()
    }

    /// Set the sample rate in Hz.
    ///
    /// Ports `rtlsdr_set_sample_rate`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn set_sample_rate(&mut self, samp_rate: u32) -> Result<(), RtlSdrError> {
        // Validate range
        if samp_rate <= 225_000
            || samp_rate > 3_200_000
            || (samp_rate > 300_000 && samp_rate <= 900_000)
        {
            return Err(RtlSdrError::InvalidSampleRate(samp_rate));
        }

        let rsamp_ratio =
            ((f64::from(self.rtl_xtal) * (1u64 << 22) as f64) / f64::from(samp_rate)) as u32;
        let rsamp_ratio = rsamp_ratio & 0x0fff_fffc;

        let real_rsamp_ratio = rsamp_ratio | ((rsamp_ratio & 0x0800_0000) << 1);
        let real_rate =
            (f64::from(self.rtl_xtal) * (1u64 << 22) as f64 / f64::from(real_rsamp_ratio)) as u32;

        self.rate = real_rate;

        // Set bandwidth if tuner supports it
        if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let bw = if self.bw > 0 { self.bw } else { self.rate };
            let _ = tuner.set_bw(&self.handle, bw, self.rate);
            usb::set_i2c_repeater(&self.handle, false)?;
        }

        let tmp = (rsamp_ratio >> 16) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x9f, tmp, 2)?;
        let tmp = (rsamp_ratio & 0xffff) as u16;
        usb::demod_write_reg(&self.handle, 1, 0xa1, tmp, 2)?;

        // Set frequency correction
        self.set_sample_freq_correction(self.corr)?;

        // Reset demod
        usb::demod_write_reg(&self.handle, 1, 0x01, 0x14, 1)?;
        usb::demod_write_reg(&self.handle, 1, 0x01, 0x10, 1)?;

        Ok(())
    }

    /// Get the current sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Set center frequency in Hz.
    ///
    /// Ports `rtlsdr_set_center_freq`.
    pub fn set_center_freq(&mut self, freq: u32) -> Result<(), RtlSdrError> {
        if self.direct_sampling != 0 {
            self.set_if_freq(freq)?;
        } else if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let tune_freq = if freq > self.offs_freq {
                freq - self.offs_freq
            } else {
                freq
            };
            let result = tuner.set_freq(&self.handle, tune_freq);
            usb::set_i2c_repeater(&self.handle, false)?;
            result?;
        }

        self.freq = freq;
        Ok(())
    }

    /// Get the current center frequency.
    pub fn center_freq(&self) -> u32 {
        self.freq
    }

    /// Set frequency correction in PPM.
    pub fn set_freq_correction(&mut self, ppm: i32) -> Result<(), RtlSdrError> {
        if self.corr == ppm {
            return Ok(());
        }
        self.corr = ppm;
        self.set_sample_freq_correction(ppm)?;

        if self.freq > 0 {
            self.set_center_freq(self.freq)?;
        }
        Ok(())
    }

    /// Set tuner gain in tenths of dB.
    pub fn set_tuner_gain(&mut self, gain: i32) -> Result<(), RtlSdrError> {
        if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let result = tuner.set_gain(&self.handle, gain);
            usb::set_i2c_repeater(&self.handle, false)?;
            result?;
            self.gain = gain;
        }
        Ok(())
    }

    /// Set tuner gain mode (0 = auto, 1 = manual).
    pub fn set_tuner_gain_mode(&mut self, manual: bool) -> Result<(), RtlSdrError> {
        if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let result = tuner.set_gain_mode(&self.handle, manual);
            usb::set_i2c_repeater(&self.handle, false)?;
            result?;
        }
        Ok(())
    }

    /// Set AGC mode.
    pub fn set_agc_mode(&self, on: bool) -> Result<(), RtlSdrError> {
        usb::demod_write_reg(&self.handle, 0, 0x19, if on { 0x25 } else { 0x05 }, 1)
    }

    /// Set bias-T power on the default GPIO (pin 0).
    pub fn set_bias_tee(&self, on: bool) -> Result<(), RtlSdrError> {
        usb::set_gpio_output(&self.handle, 0)?;
        usb::set_gpio_bit(&self.handle, 0, on)
    }

    /// Reset the USB endpoint buffer.
    pub fn reset_buffer(&self) -> Result<(), RtlSdrError> {
        usb::write_reg(
            &self.handle,
            Block::Usb,
            crate::reg::usb_reg::USB_EPA_CTL,
            0x1002,
            2,
        )?;
        usb::write_reg(
            &self.handle,
            Block::Usb,
            crate::reg::usb_reg::USB_EPA_CTL,
            0x0000,
            2,
        )
    }

    /// Synchronous (blocking) read of IQ samples.
    ///
    /// Returns the number of bytes read.
    pub fn read_sync(&self, buf: &mut [u8]) -> Result<usize, RtlSdrError> {
        let n = self.handle.read_bulk(
            0x81,
            buf,
            std::time::Duration::from_millis(if BULK_TIMEOUT == 0 {
                5000
            } else {
                BULK_TIMEOUT
            }),
        )?;
        Ok(n)
    }

    /// Set sample frequency correction in PPM.
    fn set_sample_freq_correction(&self, ppm: i32) -> Result<(), RtlSdrError> {
        #[allow(clippy::cast_possible_truncation)]
        let offs = (f64::from(-ppm) * (1u64 << 24) as f64 / 1_000_000.0) as i16;

        let tmp = (offs & 0xff) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x3f, tmp, 1)?;
        let tmp = ((offs >> 8) & 0x3f) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x3e, tmp, 1)?;

        Ok(())
    }

    /// Get device manufacturer string.
    pub fn manufacturer(&self) -> &str {
        &self.manufact
    }

    /// Get device product string.
    pub fn product(&self) -> &str {
        &self.product
    }
}

impl Drop for RtlSdrDevice {
    fn drop(&mut self) {
        // Deinit tuner
        if let Some(tuner) = &mut self.tuner {
            let _ = usb::set_i2c_repeater(&self.handle, true);
            let _ = tuner.exit(&self.handle);
            let _ = usb::set_i2c_repeater(&self.handle, false);
        }

        // Power off demod
        let _ = usb::deinit_baseband(&self.handle);

        // Release interface
        let _ = self.handle.release_interface(0);
    }
}

// --- Static functions ---

/// Get the number of connected RTL-SDR devices.
pub fn get_device_count() -> u32 {
    let mut count = 0u32;
    if let Ok(devices) = rusb::devices() {
        for device in devices.iter() {
            if let Ok(dd) = device.device_descriptor() {
                if find_known_device(dd.vendor_id(), dd.product_id()).is_some() {
                    count += 1;
                }
            }
        }
    }
    count
}

/// Get the name of a device by index.
pub fn get_device_name(index: u32) -> String {
    let mut count = 0u32;
    if let Ok(devices) = rusb::devices() {
        for device in devices.iter() {
            if let Ok(dd) = device.device_descriptor() {
                if let Some(known) = find_known_device(dd.vendor_id(), dd.product_id()) {
                    if count == index {
                        return known.name.to_string();
                    }
                    count += 1;
                }
            }
        }
    }
    String::new()
}

/// Find a USB device by its RTL-SDR index.
fn find_device_by_index(
    index: u32,
) -> Result<(rusb::Device<rusb::GlobalContext>, rusb::DeviceDescriptor), RtlSdrError> {
    let devices = rusb::devices()?;
    let mut count = 0u32;

    for device in devices.iter() {
        if let Ok(dd) = device.device_descriptor() {
            if find_known_device(dd.vendor_id(), dd.product_id()).is_some() {
                if count == index {
                    return Ok((device, dd));
                }
                count += 1;
            }
        }
    }

    Err(RtlSdrError::DeviceNotFound(index))
}
