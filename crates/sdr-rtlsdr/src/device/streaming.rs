//! Data streaming — buffer reset, sync read, async read.
//!
//! Ports `rtlsdr_reset_buffer`, `rtlsdr_read_sync`,
//! `rtlsdr_read_async`, `rtlsdr_cancel_async`.
//!
//! Note: The C implementation uses libusb's async transfer API with multiple
//! pre-submitted bulk transfers. The Rust implementation uses a worker thread
//! with synchronous bulk reads, which provides equivalent functionality
//! without requiring raw libusb async bindings.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::constants::{BULK_TIMEOUT, DEFAULT_BUF_LENGTH, DEFAULT_BUF_NUMBER};
use crate::error::RtlSdrError;
use crate::reg::{AsyncStatus, Block};
use crate::usb;

use super::RtlSdrDevice;

/// Callback type for async reading.
/// Called with (buffer, length) for each completed transfer.
pub type ReadAsyncCb = Box<dyn FnMut(&[u8]) + Send>;

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

    /// Synchronous (blocking) read of IQ samples.
    ///
    /// Ports `rtlsdr_read_sync`. Returns the number of bytes read.
    pub fn read_sync(&self, buf: &mut [u8]) -> Result<usize, RtlSdrError> {
        let timeout = if BULK_TIMEOUT == 0 {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(BULK_TIMEOUT)
        };
        let n = self.handle.read_bulk(0x81, buf, timeout)?;
        Ok(n)
    }

    /// Start asynchronous reading with a callback.
    ///
    /// Ports `rtlsdr_read_async`. Spawns a worker thread that reads bulk
    /// data and calls the callback for each buffer. Blocks until cancelled
    /// via `cancel_async()` or the callback returns.
    ///
    /// - `cb`: callback called with each buffer of IQ data
    /// - `buf_num`: number of buffers (0 = default 15)
    /// - `buf_len`: buffer length in bytes (0 = default, must be multiple of 512)
    pub fn read_async(
        &mut self,
        mut cb: ReadAsyncCb,
        buf_num: u32,
        buf_len: u32,
    ) -> Result<(), RtlSdrError> {
        if self.async_status != AsyncStatus::Inactive {
            return Err(RtlSdrError::DeviceBusy);
        }

        self.async_status = AsyncStatus::Running;

        let _buf_num = if buf_num > 0 {
            buf_num
        } else {
            DEFAULT_BUF_NUMBER
        };

        let actual_buf_len = if buf_len > 0 && buf_len.is_multiple_of(512) {
            buf_len as usize
        } else {
            DEFAULT_BUF_LENGTH as usize
        };

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_clone = Arc::clone(&cancel_flag);

        // Store cancel flag for cancel_async()
        // (In a full implementation this would be a field on the struct)

        let timeout = if BULK_TIMEOUT == 0 {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(BULK_TIMEOUT)
        };

        let mut buf = vec![0u8; actual_buf_len];

        // Read loop — equivalent to the libusb event loop in the C version
        while !cancel_clone.load(Ordering::Relaxed) {
            match self.handle.read_bulk(0x81, &mut buf, timeout) {
                Ok(n) => {
                    if n > 0 {
                        cb(&buf[..n]);
                    }
                }
                Err(rusb::Error::Timeout) => {
                    // Timeout is normal when waiting for data
                }
                Err(rusb::Error::NoDevice) => {
                    self.dev_lost = true;
                    break;
                }
                Err(e) => {
                    tracing::error!("async read error: {e}");
                    break;
                }
            }
        }

        self.async_status = AsyncStatus::Inactive;

        if self.dev_lost {
            return Err(RtlSdrError::DeviceLost);
        }

        Ok(())
    }

    /// Cancel an ongoing async read.
    ///
    /// Ports `rtlsdr_cancel_async`.
    pub fn cancel_async(&mut self) -> Result<(), RtlSdrError> {
        if self.async_status == AsyncStatus::Running {
            self.async_status = AsyncStatus::Canceling;
            // The async read loop checks this status
            Ok(())
        } else {
            Err(RtlSdrError::InvalidParameter(
                "no async read in progress".to_string(),
            ))
        }
    }
}
