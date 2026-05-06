//! RTL-SDR device — high-level API for device control.
//!
//! Ports `rtlsdr_dev_t` struct and public API functions from librtlsdr.
//! Split into sub-modules for manageability:
//! - `enumerate`: device discovery and USB string queries
//! - `frequency`: center freq, sample rate, PPM correction, offset tuning
//! - `gain`: tuner gain, gain mode, bandwidth, AGC
//! - `sampling`: direct sampling, test mode, bias-T
//! - `eeprom`: EEPROM read/write
//! - `streaming`: buffer reset, sync/async read

mod eeprom;
mod enumerate;
mod frequency;
mod gain;
mod sampling;
mod streaming;

pub use enumerate::{
    DeviceInfo, get_device_count, get_device_name, get_device_usb_strings, get_index_by_serial,
    list_devices,
};

mod builder;
pub use builder::RtlSdrDeviceBuilder;

use crate::constants::*;
use crate::error::RtlSdrError;
use crate::reg::{AsyncStatus, Block, TunerType};
use crate::tuner::Tuner;
use crate::tuner::r82xx::{R82xxConfig, R82xxPriv};
use crate::usb;

/// RTL-SDR device handle.
///
/// Ports `struct rtlsdr_dev` from librtlsdr. Manages the USB connection,
/// baseband configuration, and tuner driver.
///
/// # Send + Sync
///
/// `RtlSdrDevice` is [`Send`] — you can move it across thread
/// boundaries (e.g. into a `std::thread::spawn` worker that owns
/// the device exclusively). It is **not** [`Sync`] — the inner
/// per-tuner driver behind a `Box<dyn Tuner + Send>` doesn't
/// require `Sync`, so sharing `&RtlSdrDevice` between threads
/// would be unsound.
///
/// The supported pattern is single-owner: one thread holds the
/// `RtlSdrDevice` and serialises every control method call.
/// For background bulk reads on a worker thread without giving
/// up `&mut` on the main thread, see [`Self::usb_handle`] /
/// [`Self::BULK_ENDPOINT`] and the threading caveats in the
/// crate-level docs.
pub struct RtlSdrDevice {
    pub(crate) handle: std::sync::Arc<rusb::DeviceHandle<rusb::GlobalContext>>,
    pub(crate) tuner_type: TunerType,
    pub(crate) tuner: Option<Box<dyn Tuner>>,

    // RTL demod context
    pub(crate) rtl_xtal: u32,
    pub(crate) tun_xtal: u32,
    pub(crate) rate: u32,
    pub(crate) freq: u32,
    pub(crate) bw: u32,
    pub(crate) offs_freq: u32,
    pub(crate) corr: i32,
    pub(crate) gain: i32,
    pub(crate) direct_sampling: i32,
    pub(crate) fir: [i32; FIR_LEN],

    // Async streaming state
    #[allow(dead_code)]
    pub(crate) async_status: AsyncStatus,

    // Device info
    pub(crate) manufact: String,
    pub(crate) product: String,
    pub(crate) serial: String,

    // Device lost tracking
    pub(crate) dev_lost: bool,
    pub(crate) driver_active: bool,
}

impl RtlSdrDevice {
    /// USB bulk-IN endpoint address for IQ sample reads.
    ///
    /// Exposed as a public constant so callers using the
    /// [`Self::usb_handle`] escape hatch to do their own raw
    /// `rusb::DeviceHandle::read_bulk` calls (e.g. an `rtl_tcp`
    /// server forwarding raw samples) don't need to hard-code the
    /// magic number. Universal across all RTL-SDR variants.
    pub const BULK_ENDPOINT: u8 = crate::constants::BULK_ENDPOINT;

    /// Start a [`RtlSdrDeviceBuilder`] for opening with named
    /// selectors (index or serial).
    ///
    /// See [`RtlSdrDeviceBuilder`] for usage. `RtlSdrDevice::open`
    /// remains the lowest-overhead path for the "open the first
    /// dongle" / "open by known index" cases; the builder is for
    /// when you want to address a specific dongle by serial in a
    /// multi-device setup.
    #[must_use]
    pub fn builder() -> RtlSdrDeviceBuilder {
        RtlSdrDeviceBuilder::default()
    }

    /// Enumerate all connected RTL-SDR dongles in one call.
    ///
    /// Convenience shortcut for [`list_devices`] for callers that
    /// already have `RtlSdrDevice` in scope and don't want to
    /// import the free function separately. See
    /// [`list_devices`]'s docs for the performance note (one USB
    /// descriptor read per dongle — cache the result).
    #[must_use]
    pub fn list() -> Vec<DeviceInfo> {
        enumerate::list_devices()
    }

    /// Open an RTL-SDR device by index.
    ///
    /// Ports `rtlsdr_open`. Initializes the baseband, probes the tuner,
    /// and configures the device for SDR mode.
    pub fn open(index: u32) -> Result<Self, RtlSdrError> {
        let (device, _dd) = enumerate::find_device_by_index(index)?;

        let handle = device.open()?;

        // Check for kernel driver
        let driver_active = handle.kernel_driver_active(0).unwrap_or(false);
        if driver_active {
            let _ = handle.detach_kernel_driver(0);
        }

        handle.claim_interface(0)?;

        let mut dev = Self {
            handle: std::sync::Arc::new(handle),
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
            async_status: AsyncStatus::Inactive,
            manufact: String::new(),
            product: String::new(),
            serial: String::new(),
            dev_lost: true,
            driver_active,
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
        dev.dev_lost = false;

        // Get device manufacturer, product, and serial strings
        if let Ok(dd) = dev.handle.device().device_descriptor() {
            dev.manufact = dev
                .handle
                .read_manufacturer_string_ascii(&dd)
                .unwrap_or_default();
            dev.product = dev
                .handle
                .read_product_string_ascii(&dd)
                .unwrap_or_default();
            dev.serial = dev
                .handle
                .read_serial_number_string_ascii(&dd)
                .unwrap_or_default();
        }

        // Probe tuners
        usb::set_i2c_repeater(&dev.handle, true)?;
        dev.probe_tuner();

        // Use RTL clock value by default for tuner
        // (may have been changed by probe_tuner for R828D)
        if dev.tun_xtal == DEF_RTL_XTAL_FREQ {
            dev.tun_xtal = dev.rtl_xtal;
        }

        // Tuner-specific post-init configuration
        match dev.tuner_type {
            TunerType::R828D | TunerType::R820T => {
                // Disable Zero-IF mode
                usb::demod_write_reg(&dev.handle, 1, 0xb1, 0x1a, 1)?;
                // Only enable In-phase ADC input
                usb::demod_write_reg(&dev.handle, 0, 0x08, 0x4d, 1)?;
                // Set R82XX IF frequency
                dev.set_if_freq(R82XX_IF_FREQ)?;
                // Enable spectrum inversion
                usb::demod_write_reg(&dev.handle, 1, 0x15, 0x01, 1)?;
            }
            TunerType::Unknown => {
                // No tuner found — enable direct sampling mode
                tracing::warn!("No supported tuner found, enabling direct sampling");
                let _ = dev.set_direct_sampling(1);
            }
            _ => {}
        }

        // Initialize tuner driver
        if let Some(tuner) = &mut dev.tuner {
            tuner.init(&dev.handle)?;
        }

        usb::set_i2c_repeater(&dev.handle, false)?;

        Ok(dev)
    }

    /// Probe for supported tuner ICs.
    ///
    /// Ports the tuner probing sequence from `rtlsdr_open`.
    fn probe_tuner(&mut self) {
        // Try E4000
        if let Ok(reg) = usb::i2c_read_reg(&self.handle, E4K_I2C_ADDR, E4K_CHECK_ADDR) {
            if reg == E4K_CHECK_VAL {
                tracing::info!("Found Elonics E4000 tuner");
                self.tuner_type = TunerType::E4000;
                return;
            }
        }

        // Try FC0013
        if let Ok(reg) = usb::i2c_read_reg(&self.handle, FC0013_I2C_ADDR, FC0013_CHECK_ADDR) {
            if reg == FC0013_CHECK_VAL {
                tracing::info!("Found Fitipower FC0013 tuner");
                self.tuner_type = TunerType::Fc0013;
                return;
            }
        }

        // Try R820T
        if let Ok(reg) = usb::i2c_read_reg(&self.handle, R820T_I2C_ADDR, R82XX_CHECK_ADDR) {
            if reg == R82XX_CHECK_VAL {
                tracing::info!("Found Rafael Micro R820T tuner");
                self.tuner_type = TunerType::R820T;
                self.create_r82xx_tuner();
                return;
            }
        }

        // Try R828D
        if let Ok(reg) = usb::i2c_read_reg(&self.handle, R828D_I2C_ADDR, R82XX_CHECK_ADDR) {
            if reg == R82XX_CHECK_VAL {
                tracing::info!("Found Rafael Micro R828D tuner");
                let is_v4 = self.is_blog_v4();
                if is_v4 {
                    tracing::info!("RTL-SDR Blog V4 Detected");
                }
                self.tuner_type = TunerType::R828D;
                self.create_r82xx_tuner();
                return;
            }
        }

        // Initialize GPIOs before probing remaining tuners
        let _ = usb::set_gpio_output(&self.handle, 4);
        // Reset tuner
        let _ = usb::set_gpio_bit(&self.handle, 4, true);
        let _ = usb::set_gpio_bit(&self.handle, 4, false);

        // Try FC2580
        if let Ok(reg) = usb::i2c_read_reg(&self.handle, FC2580_I2C_ADDR, FC2580_CHECK_ADDR) {
            if (reg & 0x7f) == FC2580_CHECK_VAL {
                tracing::info!("Found FCI 2580 tuner");
                self.tuner_type = TunerType::Fc2580;
                return;
            }
        }

        // Try FC0012
        if let Ok(reg) = usb::i2c_read_reg(&self.handle, FC0012_I2C_ADDR, FC0012_CHECK_ADDR) {
            if reg == FC0012_CHECK_VAL {
                tracing::info!("Found Fitipower FC0012 tuner");
                let _ = usb::set_gpio_output(&self.handle, 6);
                self.tuner_type = TunerType::Fc0012;
                return;
            }
        }

        tracing::warn!("No supported tuner found");
    }

    /// Create R82XX tuner driver instance.
    fn create_r82xx_tuner(&mut self) {
        let (i2c_addr, chip) = match self.tuner_type {
            TunerType::R828D => {
                let is_v4 = self.is_blog_v4();
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

        let xtal = self.get_tuner_xtal();
        let config = R82xxConfig {
            i2c_addr,
            xtal,
            rafael_chip: chip,
            max_i2c_msg_len: 8,
            use_predetect: false,
        };

        let mut r82xx = R82xxPriv::new(&config);
        let is_v4 = self.is_blog_v4();
        r82xx.set_blog_v4(is_v4);
        self.tuner = Some(Box::new(r82xx));
    }

    // --- Internal helpers ---

    /// Set IF frequency.
    ///
    /// Ports `rtlsdr_set_if_freq`.
    pub(crate) fn set_if_freq(&self, freq: u32) -> Result<(), RtlSdrError> {
        let rtl_xtal = self.get_rtl_xtal();
        let if_freq = -((f64::from(freq) * (1u64 << 22) as f64) / f64::from(rtl_xtal)) as i32;

        let tmp = ((if_freq >> 16) & 0x3f) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x19, tmp, 1)?;
        let tmp = ((if_freq >> 8) & 0xff) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x1a, tmp, 1)?;
        let tmp = (if_freq & 0xff) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x1b, tmp, 1)?;

        Ok(())
    }

    /// Set sample frequency correction in PPM.
    ///
    /// Ports `rtlsdr_set_sample_freq_correction`.
    pub(crate) fn set_sample_freq_correction(&self, ppm: i32) -> Result<(), RtlSdrError> {
        let offs = (f64::from(-ppm) * (1u64 << 24) as f64 / 1_000_000.0) as i16;

        let tmp = (offs & 0xff) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x3f, tmp, 1)?;
        let tmp = ((offs >> 8) & 0x3f) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x3e, tmp, 1)?;

        Ok(())
    }

    /// Get corrected RTL crystal frequency.
    ///
    /// Ports `APPLY_PPM_CORR` macro from `rtlsdr_get_xtal_freq`.
    pub(crate) fn get_rtl_xtal(&self) -> u32 {
        (f64::from(self.rtl_xtal) * (1.0 + f64::from(self.corr) / 1e6)) as u32
    }

    /// Get corrected tuner crystal frequency.
    pub(crate) fn get_tuner_xtal(&self) -> u32 {
        (f64::from(self.tun_xtal) * (1.0 + f64::from(self.corr) / 1e6)) as u32
    }

    // --- Public getters ---

    /// Get the tuner type.
    pub fn tuner_type(&self) -> TunerType {
        self.tuner_type
    }

    /// Get available gain values (tenths of dB).
    pub fn tuner_gains(&self) -> &[i32] {
        self.tuner_type.gains()
    }

    /// Find the closest available tuner gain to a desired value.
    ///
    /// `desired_tenths_db` is the target gain in tenths-of-dB (the
    /// same unit [`Self::set_tuner_gain`] takes). Returns the
    /// gain step from [`Self::tuner_gains`] that's nearest to the
    /// requested value. Ties go to the lower step (deterministic
    /// `min_by_key` behaviour). Useful when an app's UI lets the
    /// user pick a "rough" dB value (slider, dropdown of round
    /// numbers) and you want the actual closest hardware-accepted
    /// step without rolling your own search over the gain table.
    ///
    /// Each tuner family has its own discrete gain table — the
    /// R820T2 has 29 steps from 0.0 to 49.6 dB, the E4000 has 14
    /// steps from -1 to 49 dB, etc. The result is always one of
    /// those exact values, never an interpolation.
    ///
    /// Returns `0` (a no-op gain) when the tuner has no gain table
    /// at all — in practice only the `Unknown` tuner type, which
    /// means the device hasn't been probed or the IC isn't in our
    /// known-tuners list.
    ///
    /// ```no_run
    /// # use sdr_rtlsdr::{RtlSdrDevice, RtlSdrError};
    /// # fn main() -> Result<(), RtlSdrError> {
    /// let mut dev = RtlSdrDevice::open(0)?;
    /// dev.set_tuner_gain_mode(true)?;
    /// // User picked "around 15 dB" in the UI; pick the actual step.
    /// let step = dev.closest_gain(150);
    /// dev.set_tuner_gain(step)?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn closest_gain(&self, desired_tenths_db: i32) -> i32 {
        closest_gain_in(self.tuner_gains(), desired_tenths_db)
    }

    /// Get device manufacturer string.
    pub fn manufacturer(&self) -> &str {
        &self.manufact
    }

    /// Get device product string.
    pub fn product(&self) -> &str {
        &self.product
    }

    /// Get device serial string.
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// Get the current center frequency.
    pub fn center_freq(&self) -> u32 {
        self.freq
    }

    /// Get the current sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Get the current frequency correction in PPM.
    pub fn freq_correction(&self) -> i32 {
        self.corr
    }

    /// Get the current tuner gain.
    pub fn tuner_gain(&self) -> i32 {
        self.gain
    }

    /// Get the current direct sampling mode.
    pub fn direct_sampling(&self) -> i32 {
        self.direct_sampling
    }

    /// Get the current offset tuning state.
    pub fn offset_tuning(&self) -> bool {
        self.offs_freq > 0
    }

    /// Get corrected xtal frequencies.
    ///
    /// Ports `rtlsdr_get_xtal_freq`.
    pub fn xtal_freq(&self) -> (u32, u32) {
        (self.get_rtl_xtal(), self.get_tuner_xtal())
    }

    /// Set RTL and/or tuner crystal frequencies.
    ///
    /// Ports `rtlsdr_set_xtal_freq`.
    pub fn set_xtal_freq(&mut self, rtl_freq: u32, tuner_freq: u32) -> Result<(), RtlSdrError> {
        if rtl_freq > 0 && (rtl_freq < MIN_RTL_XTAL_FREQ || rtl_freq > MAX_RTL_XTAL_FREQ) {
            return Err(RtlSdrError::InvalidParameter(format!(
                "RTL xtal freq out of range: {rtl_freq}"
            )));
        }

        if rtl_freq > 0 && self.rtl_xtal != rtl_freq {
            self.rtl_xtal = rtl_freq;
            if self.rate > 0 {
                self.set_sample_rate(self.rate)?;
            }
        }

        if self.tun_xtal != tuner_freq {
            self.tun_xtal = if tuner_freq == 0 {
                self.rtl_xtal
            } else {
                tuner_freq
            };

            if self.freq > 0 {
                self.set_center_freq(self.freq)?;
            }
        }

        Ok(())
    }

    /// Check if this is an RTL-SDR Blog V4 device.
    pub fn is_blog_v4(&self) -> bool {
        self.manufact == "RTLSDRBlog" && self.product == "Blog V4"
    }

    /// Check if the device matches a manufacturer/product pair.
    ///
    /// Ports `rtlsdr_check_dongle_model`.
    pub fn check_dongle_model(&self, manufact: &str, product: &str) -> bool {
        self.manufact == manufact && self.product == product
    }
}

impl Drop for RtlSdrDevice {
    fn drop(&mut self) {
        if !self.dev_lost {
            // Wait for async to complete
            // (in practice async is handled by the caller stopping first)

            // Deinit tuner
            if let Some(tuner) = &mut self.tuner {
                let _ = usb::set_i2c_repeater(&self.handle, true);
                let _ = tuner.exit(&self.handle);
                let _ = usb::set_i2c_repeater(&self.handle, false);
            }

            // Power off demod
            let _ = usb::deinit_baseband(&self.handle);
        }

        // Release interface
        let _ = self.handle.release_interface(0);

        // Reattach kernel driver if we detached it
        if self.driver_active {
            let _ = self.handle.attach_kernel_driver(0);
        }
    }
}

/// Find the closest entry in a tuner-gain table to `desired`,
/// returning `0` for an empty table. Pulled out of
/// [`RtlSdrDevice::closest_gain`] so the algorithm can be unit-
/// tested without constructing a live device.
///
/// Ties go to the lower step (deterministic `min_by_key` —
/// stable iterator order means the first equally-distant entry
/// in the table wins, and tables are stored in ascending order).
fn closest_gain_in(gains: &[i32], desired: i32) -> i32 {
    gains
        .iter()
        .copied()
        .min_by_key(|&g| (g - desired).abs())
        .unwrap_or(0)
}

// Pin the `Send`-but-not-`Sync` contract documented on the
// `RtlSdrDevice` struct. If a future field change ever adds a
// non-`Send` member (e.g. a `Cell<…>` or `Rc<…>`), this assertion
// fires at compile time so we notice before semver-breaking
// downstream consumers who relied on moving the device into a
// worker thread.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<RtlSdrDevice>();
};

#[cfg(test)]
mod closest_gain_tests {
    use super::closest_gain_in;

    // R820T2 gain table (29 steps, tenths of dB) — pinned here
    // rather than imported so the test exercises a known-real
    // table shape independent of any future tuner-table edits.
    const R820T2_GAINS: &[i32] = &[
        0, 9, 14, 27, 37, 77, 87, 125, 144, 157, 166, 197, 207, 229, 254, 280, 297, 328, 338, 364,
        372, 386, 402, 421, 434, 439, 445, 480, 496,
    ];

    #[test]
    fn empty_table_returns_zero() {
        assert_eq!(closest_gain_in(&[], 250), 0);
        assert_eq!(closest_gain_in(&[], 0), 0);
        assert_eq!(closest_gain_in(&[], -100), 0);
    }

    #[test]
    fn exact_match_returns_self() {
        for &g in R820T2_GAINS {
            assert_eq!(
                closest_gain_in(R820T2_GAINS, g),
                g,
                "exact value {g} should round to itself"
            );
        }
    }

    #[test]
    fn rounds_to_nearest_step() {
        // 150 is between 144 and 157 — closer to 157 (Δ=7) than
        // to 144 (Δ=6). Wait — that's |150-144|=6 vs |150-157|=7,
        // so 144 wins.
        assert_eq!(closest_gain_in(R820T2_GAINS, 150), 144);

        // 152 is exactly between 144 (Δ=8) and 157 (Δ=5) → 157.
        assert_eq!(closest_gain_in(R820T2_GAINS, 152), 157);

        // 100 is between 87 (Δ=13) and 125 (Δ=25) → 87.
        assert_eq!(closest_gain_in(R820T2_GAINS, 100), 87);
    }

    #[test]
    fn out_of_range_clamps_to_endpoint() {
        // Below the minimum: clamp to first entry.
        assert_eq!(closest_gain_in(R820T2_GAINS, -1000), 0);
        // Above the maximum: clamp to last entry.
        assert_eq!(closest_gain_in(R820T2_GAINS, 10_000), 496);
    }

    #[test]
    fn ties_resolve_deterministically() {
        // Symmetric gap: 50 is exactly between 0 and 100. With
        // `min_by_key` over a stable iterator, the first
        // equally-distant entry wins → 0.
        let table = &[0, 100];
        assert_eq!(closest_gain_in(table, 50), 0);
    }
}
