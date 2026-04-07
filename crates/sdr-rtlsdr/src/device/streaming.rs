//! Data streaming — buffer reset, sync read, async read.
//!
//! Ports `rtlsdr_reset_buffer`, `rtlsdr_read_sync`,
//! `rtlsdr_read_async`, `rtlsdr_cancel_async`.
//!
//! Note: The C implementation uses libusb's async transfer API with multiple
//! pre-submitted bulk transfers. The Rust implementation uses a blocking
//! read loop that checks a shared cancellation flag. True async support
//! will be added when the pipeline is wired up with worker threads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::constants::{BULK_TIMEOUT, DEFAULT_BUF_LENGTH};
use crate::error::RtlSdrError;
use crate::reg::Block;
use crate::usb;

use super::RtlSdrDevice;

/// Callback type for async reading.
/// Called with a byte slice of IQ data for each completed bulk transfer.
pub type ReadAsyncCb = Box<dyn FnMut(&[u8]) + Send>;

/// Maximum allowed buffer length for async reads (16 MB).
const MAX_BUF_LENGTH: u32 = 16 * 1024 * 1024;

/// USB bulk transfer alignment requirement (bytes).
const BULK_ALIGNMENT: u32 = 512;

/// Async read loop timeout for cancel flag polling.
const ASYNC_POLL_TIMEOUT: Duration = Duration::from_secs(1);

impl RtlSdrDevice {
    /// Reset the USB endpoint buffer.
    ///
    /// Ports `rtlsdr_reset_buffer`.
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

    /// Get a shared reference to the USB handle for spawning a reader thread.
    ///
    /// The returned Arc can be sent to another thread for concurrent bulk reads
    /// while the main thread retains access for control transfers.
    pub fn usb_handle(&self) -> std::sync::Arc<rusb::DeviceHandle<rusb::GlobalContext>> {
        std::sync::Arc::clone(&self.handle)
    }

    /// Synchronous (blocking) read of IQ samples.
    ///
    /// Ports `rtlsdr_read_sync`. Returns the number of bytes read.
    pub fn read_sync(&self, buf: &mut [u8]) -> Result<usize, RtlSdrError> {
        let timeout = if BULK_TIMEOUT == 0 {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(BULK_TIMEOUT)
        };
        match self
            .handle
            .read_bulk(crate::constants::BULK_ENDPOINT, buf, timeout)
        {
            Ok(n) => Ok(n),
            Err(rusb::Error::NoDevice) => Err(RtlSdrError::DeviceLost),
            Err(e) => Err(e.into()),
        }
    }

    /// Read IQ samples in a blocking loop, calling the callback for each buffer.
    ///
    /// This is a simplified port of `rtlsdr_read_async`. It blocks the calling
    /// thread and reads bulk data, calling `cb` for each completed buffer.
    /// Use `cancel_flag` to signal cancellation from another thread.
    ///
    /// - `cb`: callback called with each buffer of IQ data
    /// - `cancel_flag`: set to `true` from another thread to stop reading
    /// - `buf_len`: buffer length in bytes (0 = default, must be multiple of 512)
    pub fn read_async_blocking(
        &self,
        mut cb: ReadAsyncCb,
        cancel_flag: &AtomicBool,
        buf_len: u32,
    ) -> Result<(), RtlSdrError> {
        let actual_buf_len = if buf_len == 0 {
            DEFAULT_BUF_LENGTH as usize
        } else if !buf_len.is_multiple_of(BULK_ALIGNMENT) || buf_len > MAX_BUF_LENGTH {
            return Err(RtlSdrError::InvalidParameter(format!(
                "buf_len must be a multiple of {BULK_ALIGNMENT} and <= {MAX_BUF_LENGTH}, got {buf_len}"
            )));
        } else {
            buf_len as usize
        };

        let timeout = ASYNC_POLL_TIMEOUT;
        let mut buf = vec![0u8; actual_buf_len];

        while !cancel_flag.load(Ordering::Relaxed) {
            match self
                .handle
                .read_bulk(crate::constants::BULK_ENDPOINT, &mut buf, timeout)
            {
                Ok(n) if n > 0 => {
                    cb(&buf[..n]);
                }
                // Zero-length read or timeout — check cancel flag and retry
                Ok(_) | Err(rusb::Error::Timeout) => {}
                Err(rusb::Error::NoDevice) => {
                    return Err(RtlSdrError::DeviceLost);
                }
                Err(e) => {
                    tracing::error!("bulk read error: {e}");
                    return Err(RtlSdrError::Usb(e));
                }
            }
        }

        Ok(())
    }
}
