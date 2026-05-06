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

    /// Iterate IQ samples as a sequence of owned byte buffers.
    ///
    /// Returns an `Iterator` whose [`Iterator::next`] blocks the
    /// calling thread until one buffer's worth of samples is ready
    /// (a single `read_sync` underneath), then yields a freshly-
    /// allocated `Vec<u8>` of the actual byte count read. Each
    /// item is `Result<Vec<u8>, RtlSdrError>` so transport errors
    /// surface in-band; the iterator fuses (returns `None` from
    /// then on) after the first error or a zero-length read.
    ///
    /// This is the foundation for both sync streaming (use
    /// directly) and async streaming wrappers (the per-runtime
    /// `stream_samples_*` methods drive this iterator inside a
    /// blocking task).
    ///
    /// # Buffer size
    ///
    /// `buffer_size` is the bytes-per-yield target. The librtlsdr
    /// default is 256 KB (16 × 32 × 512). Smaller buffers give
    /// lower per-item latency but more allocator traffic; larger
    /// buffers amortise USB overhead but increase per-buffer
    /// latency. The size doesn't have to be a multiple of the USB
    /// 512-byte packet — `read_sync` returns the actual byte count
    /// — but multiples of 512 avoid short final transfers.
    ///
    /// Passing `0` selects the librtlsdr-equivalent default
    /// (256 KB) rather than requesting a zero-length buffer —
    /// matches the upstream "pass 0 for the default" ergonomic
    /// and prevents a typo from silently fusing the iterator on
    /// the first call (which would look like EOF).
    ///
    /// # Allocation
    ///
    /// Each yielded `Vec<u8>` is a fresh allocation. At the
    /// 256 KB / 65 ms cadence of typical RTL-SDR rates this is
    /// negligible (~15 allocs/sec), but for tight loops or
    /// embedded use prefer [`Self::read_sync`] directly with a
    /// reused caller-owned buffer.
    ///
    /// ```no_run
    /// # use sdr_rtlsdr::{RtlSdrDevice, RtlSdrError};
    /// # fn main() -> Result<(), RtlSdrError> {
    /// let dev = RtlSdrDevice::open(0)?;
    /// dev.reset_buffer()?;
    /// // Take the first 10 buffers — each ~65 ms at 2 Msps.
    /// for chunk in dev.iter_samples(262_144).take(10) {
    ///     let bytes = chunk?;
    ///     // process `bytes`...
    ///     # let _ = bytes;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn iter_samples(&self, buffer_size: usize) -> SampleIter<'_> {
        // Normalise zero to the librtlsdr-equivalent default
        // (256 KB). A `buffer_size == 0` typo would otherwise
        // hand `read_sync` an empty slice, which the USB
        // backend treats as an immediate zero-length read —
        // the iterator's zero-fuse path triggers, and the
        // caller sees an empty `for chunk in iter { … }` that
        // looks like EOF rather than a configuration mistake.
        // Per #632 CR round 1.
        let buffer_size = if buffer_size == 0 {
            DEFAULT_BUF_LENGTH as usize
        } else {
            buffer_size
        };
        SampleIter {
            device: Some(self),
            buffer_size,
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

/// Blocking iterator over IQ-sample buffers, returned by
/// [`RtlSdrDevice::iter_samples`].
///
/// Each [`Iterator::next`] call performs one [`RtlSdrDevice::read_sync`]
/// into a freshly-allocated `Vec<u8>` and yields it. The iterator
/// fuses on the first error or zero-length read — once `next`
/// returns `Some(Err(_))` (or `None` from a zero read), all
/// subsequent calls return `None` so callers can use the standard
/// `for chunk in iter { let chunk = chunk?; ... }` shape without
/// worrying about post-error state.
pub struct SampleIter<'a> {
    /// `None` once the iterator has fused (error or zero read).
    /// Borrows the device shared (`&`) because [`RtlSdrDevice::read_sync`]
    /// is `&self` — the underlying USB bulk transfer doesn't need
    /// mutable access.
    device: Option<&'a RtlSdrDevice>,
    buffer_size: usize,
}

impl Iterator for SampleIter<'_> {
    type Item = Result<Vec<u8>, RtlSdrError>;

    fn next(&mut self) -> Option<Self::Item> {
        let device = self.device?;
        let mut buf = vec![0u8; self.buffer_size];
        match device.read_sync(&mut buf) {
            Ok(0) => {
                // Zero-length read — treat as end-of-stream so
                // callers using `.take(N)` / `for ... in iter`
                // don't spin forever on a degenerate device.
                self.device = None;
                None
            }
            Ok(n) => {
                buf.truncate(n);
                Some(Ok(buf))
            }
            Err(e) => {
                // Fuse after first error so subsequent calls
                // return `None` rather than re-yielding the
                // same error indefinitely.
                self.device = None;
                Some(Err(e))
            }
        }
    }
}

impl std::iter::FusedIterator for SampleIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    // Pin the trait-impl contract documented on `SampleIter` —
    // standard `Iterator` + `FusedIterator` so consumers can rely
    // on `for x in iter` shape AND on the post-fuse-returns-None
    // contract without empirical testing. If a refactor ever
    // changes the iterator shape, this fires at compile time.
    const _: fn() = || {
        fn assert_iter<T: Iterator>() {}
        fn assert_fused<T: std::iter::FusedIterator>() {}
        assert_iter::<SampleIter<'_>>();
        assert_fused::<SampleIter<'_>>();
    };
}
