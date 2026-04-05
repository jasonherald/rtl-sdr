//! Device enumeration and USB string queries.
//!
//! Ports `rtlsdr_get_device_count`, `rtlsdr_get_device_name`,
//! `rtlsdr_get_device_usb_strings`, `rtlsdr_get_index_by_serial`.

use crate::constants::find_known_device;
use crate::error::RtlSdrError;

/// Get the number of connected RTL-SDR devices.
///
/// Ports `rtlsdr_get_device_count`.
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
///
/// Ports `rtlsdr_get_device_name`.
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

/// Get USB strings (manufacturer, product, serial) by device index.
///
/// Ports `rtlsdr_get_device_usb_strings`. Opens the device temporarily
/// to read the descriptor strings.
pub fn get_device_usb_strings(index: u32) -> Result<(String, String, String), RtlSdrError> {
    let (device, dd) = find_device_by_index(index)?;
    let handle = device.open()?;

    let manufact = handle
        .read_manufacturer_string_ascii(&dd)
        .unwrap_or_default();
    let product = handle.read_product_string_ascii(&dd).unwrap_or_default();
    let serial = handle
        .read_serial_number_string_ascii(&dd)
        .unwrap_or_default();

    Ok((manufact, product, serial))
}

/// Find a device index by its serial number string.
///
/// Ports `rtlsdr_get_index_by_serial`.
pub fn get_index_by_serial(serial: &str) -> Result<u32, RtlSdrError> {
    let count = get_device_count();
    if count == 0 {
        return Err(RtlSdrError::DeviceNotFound(0));
    }

    for i in 0..count {
        if let Ok((_, _, dev_serial)) = get_device_usb_strings(i) {
            if dev_serial == serial {
                return Ok(i);
            }
        }
    }

    Err(RtlSdrError::InvalidParameter(format!(
        "no device with serial '{serial}'"
    )))
}

/// Find a USB device by its RTL-SDR index.
pub(crate) fn find_device_by_index(
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
