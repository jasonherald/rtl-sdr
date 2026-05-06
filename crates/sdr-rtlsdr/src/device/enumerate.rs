//! Device enumeration and USB string queries.
//!
//! Ports `rtlsdr_get_device_count`, `rtlsdr_get_device_name`,
//! `rtlsdr_get_device_usb_strings`, `rtlsdr_get_index_by_serial`.
//!
//! Plus [`list_devices`] — a Rust-idiomatic collected enumeration
//! that returns one [`DeviceInfo`] per dongle in a single call.

use crate::constants::find_known_device;
use crate::error::RtlSdrError;

/// One entry returned by [`list_devices`] / [`crate::RtlSdrDevice::list`].
///
/// Carries the four pieces of information you can read about a
/// dongle without opening it: its enumeration index, the
/// human-friendly device name from the USB known-devices table
/// (e.g. "Generic RTL2832U OEM"), and the USB descriptor strings
/// (manufacturer / product / serial). The serial string is what
/// you'd hand to [`crate::RtlSdrDevice::builder`] /
/// [`get_index_by_serial`] to open a specific dongle when more
/// than one is plugged in.
///
/// USB string fields fall back to an empty `String` when the
/// descriptor read fails (e.g. permissions, transient bus error)
/// — the entry still appears so you can see something is plugged
/// in even if the strings aren't readable from this process.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceInfo {
    /// Zero-based enumeration index — the same value you'd pass to
    /// [`crate::RtlSdrDevice::open`].
    pub index: u32,
    /// Human-friendly device name from the known-devices table
    /// (e.g. "Realtek RTL2838UHIDIR"). Equivalent to
    /// [`get_device_name`] but already populated on the entry.
    pub name: String,
    /// USB manufacturer descriptor string. May be empty if the
    /// descriptor read failed.
    pub manufacturer: String,
    /// USB product descriptor string. May be empty if the
    /// descriptor read failed.
    pub product: String,
    /// USB serial-number descriptor string. May be empty if the
    /// descriptor read failed (rare in practice — most RTL-SDR
    /// flashes ship with a unique serial). Use
    /// [`crate::RtlSdrDevice::builder`]`.serial(...)` to open by
    /// serial when more than one dongle is plugged in.
    pub serial: String,
}

/// Enumerate all connected RTL-SDR dongles in one call.
///
/// More ergonomic than the count + per-index pair when the caller
/// just wants "tell me what's plugged in." Internally this is
/// [`get_device_count`] plus per-index [`get_device_name`] +
/// [`get_device_usb_strings`], collected into a `Vec`. The
/// returned slice is in enumeration-index order, so
/// `list_devices()[i].index == i as u32` for any `i` in range.
///
/// Returns an empty `Vec` when no devices are present (matches
/// the implicit "count is 0" path of the underlying enumerate).
///
/// # Performance
///
/// This walks the USB device tree and, for each match, *opens*
/// the device briefly to read its USB descriptor strings —
/// strings aren't cached in the bus topology, the kernel has to
/// be asked. Roughly `O(n_dongles)` USB control transfers. Cheap
/// for the common 1-or-2-dongle case but not something to call
/// in a tight loop. Cache the result.
#[must_use]
pub fn list_devices() -> Vec<DeviceInfo> {
    let count = get_device_count();
    (0..count)
        .map(|index| {
            let name = get_device_name(index);
            let (manufacturer, product, serial) = get_device_usb_strings(index)
                .unwrap_or_else(|_| (String::new(), String::new(), String::new()));
            DeviceInfo {
                index,
                name,
                manufacturer,
                product,
                serial,
            }
        })
        .collect()
}

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
