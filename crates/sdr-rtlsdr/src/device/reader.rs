//! Streaming-focused handle that runs concurrently with control.
//!
//! See [`RtlSdrReader`] and [`RtlSdrDevice::reader`].

use std::sync::Arc;
use std::time::Duration;

use crate::error::RtlSdrError;

use super::RtlSdrDevice;

/// Streaming-focused handle. Acquired via [`RtlSdrDevice::reader`].
///
/// `RtlSdrReader` exists to resolve the design tension between
/// Rust's ownership model (control methods like
/// [`RtlSdrDevice::set_center_freq`] take `&mut self`; concurrent
/// streaming would require holding `self` for the duration) and
/// the underlying USB protocol's reality (bulk reads use endpoint
/// 0x81; control transfers use endpoint 0x00 — different
/// endpoints, no conflict on real hardware).
///
/// The reader internally clones the device's
/// `Arc<rusb::DeviceHandle>`, then exposes the streaming surface
/// (sync iterator + per-runtime async streams) by consuming the
/// reader. The parent retains the [`RtlSdrDevice`] for control:
///
/// ```no_run
/// # use sdr_rtlsdr::{RtlSdrDevice, RtlSdrError};
/// # fn example() -> Result<(), RtlSdrError> {
/// let mut device = RtlSdrDevice::open(0)?;
/// device.set_sample_rate(2_400_000)?;
/// device.set_center_freq(100_000_000)?;
/// device.reset_buffer()?;
///
/// // Hand a reader to a worker thread.
/// let reader = device.reader();
/// let thread = std::thread::spawn(move || {
///     for chunk in reader.iter_samples(262_144) {
///         match chunk {
///             Ok(buf) => { /* push to ring / DSP */ let _ = buf; }
///             Err(e) => { eprintln!("read error: {e}"); break; }
///         }
///     }
/// });
///
/// // Parent thread retains control of the device while the reader
/// // streams — separate USB endpoints, no rusb-level conflict.
/// device.set_center_freq(101_000_000)?;
/// device.set_tuner_gain(150)?;
/// # let _ = thread;
/// # Ok(())
/// # }
/// ```
///
/// # Concurrency safety
///
/// The shared-handle pattern (one `Arc<DeviceHandle>` reffed by
/// both the parent device and any number of readers) is what
/// upstream `librtlsdr`'s reference implementations have used for
/// years. Bulk reads on endpoint 0x81 don't interfere with
/// control transfers on endpoint 0x00 at the libusb level on
/// real hardware.
///
/// **However**, libusb's documentation does not formally
/// guarantee that concurrent bulk and control transfers on a
/// single device handle are safe. The shared-handle pattern is a
/// practical convention rather than a documented promise. If you
/// need strict by-the-book safety, sequence the operations from
/// a single thread (e.g. fully drop the reader before retuning,
/// then build a new reader). For the typical "stream while
/// retuning the satellite at AOS" pattern this works reliably on
/// the dongles in active use; verify against your specific
/// hardware in production.
///
/// # Cheap clone via the device
///
/// A [`RtlSdrReader`] is just an `Arc` clone of the device's USB
/// handle plus an init flag. Build one via [`RtlSdrDevice::reader`]
/// any time you need a fresh streaming handle — the cost is one
/// atomic increment.
#[derive(Clone)]
pub struct RtlSdrReader {
    pub(crate) handle: Arc<rusb::DeviceHandle<rusb::GlobalContext>>,
}

impl RtlSdrReader {
    /// Default per-read USB transfer timeout used by sync reads.
    /// Matches [`RtlSdrDevice::read_sync`]'s timeout for symmetry.
    /// Per-call control: the per-runtime stream variants use a
    /// shorter polling timeout for cancellation responsiveness;
    /// the synchronous iterator uses this longer timeout because
    /// it has no async cancellation pathway.
    const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Synchronous bulk read into a caller-owned buffer.
    ///
    /// Mirror of [`RtlSdrDevice::read_sync`] with the same
    /// semantics, exposed on the Reader so streaming code that
    /// already has a Reader doesn't need to round-trip through
    /// the device.
    ///
    /// # Errors
    ///
    /// - [`RtlSdrError::DeviceLost`] if the dongle was
    ///   disconnected.
    /// - [`RtlSdrError::Usb`] for any other rusb transport
    ///   error.
    pub fn read_sync(&self, buf: &mut [u8]) -> Result<usize, RtlSdrError> {
        match self
            .handle
            .read_bulk(RtlSdrDevice::BULK_ENDPOINT, buf, Self::DEFAULT_TIMEOUT)
        {
            Ok(n) => Ok(n),
            Err(rusb::Error::NoDevice) => Err(RtlSdrError::DeviceLost),
            Err(e) => Err(e.into()),
        }
    }

    /// Sync iterator over IQ-sample buffers, consuming the
    /// reader.
    ///
    /// Each [`Iterator::next`] performs one [`Self::read_sync`]
    /// into a freshly-allocated `Vec<u8>` and yields it. Same
    /// fuse-on-error semantics as [`RtlSdrDevice::iter_samples`]:
    /// returns `None` permanently after the first error or
    /// zero-length read.
    ///
    /// Consumes the reader so the iterator owns the
    /// `Arc<DeviceHandle>` clone — usable across thread
    /// boundaries (`'static`-friendly, sendable).
    ///
    /// # Buffer size
    ///
    /// Same guidance as [`RtlSdrDevice::iter_samples`] — 256 KB
    /// (`262_144`) is the librtlsdr-equivalent default. Passing
    /// `0` selects the default.
    #[must_use]
    pub fn iter_samples(self, buffer_size: usize) -> ReaderIter {
        let buffer_size = if buffer_size == 0 {
            crate::constants::DEFAULT_BUF_LENGTH as usize
        } else {
            buffer_size
        };
        ReaderIter {
            reader: Some(self),
            buffer_size,
        }
    }
}

/// Owned, sendable iterator over IQ-sample buffers, returned by
/// [`RtlSdrReader::iter_samples`].
///
/// Differs from [`crate::SampleIter`] in that it owns the reader
/// (and thus the underlying `Arc<DeviceHandle>` clone) rather
/// than borrowing the device — so it satisfies `'static` and can
/// be sent to other threads / async runtimes. Same
/// `FusedIterator` contract: `None` permanently after the first
/// error or zero read.
pub struct ReaderIter {
    /// `None` once the iterator has fused.
    reader: Option<RtlSdrReader>,
    buffer_size: usize,
}

impl Iterator for ReaderIter {
    type Item = Result<Vec<u8>, RtlSdrError>;

    fn next(&mut self) -> Option<Self::Item> {
        let reader = self.reader.as_ref()?;
        let mut buf = vec![0u8; self.buffer_size];
        match reader.read_sync(&mut buf) {
            Ok(0) => {
                self.reader = None;
                None
            }
            Ok(n) => {
                buf.truncate(n);
                Some(Ok(buf))
            }
            Err(e) => {
                self.reader = None;
                Some(Err(e))
            }
        }
    }
}

impl std::iter::FusedIterator for ReaderIter {}

#[cfg(test)]
mod tests {
    use super::*;

    // Pin the trait + marker contract: ReaderIter is Iterator +
    // FusedIterator + Send. The Send guarantee is the whole
    // point of the Reader split — the iterator must move freely
    // between threads / async runtimes.
    const _: fn() = || {
        fn assert_iter<T: Iterator>() {}
        fn assert_fused<T: std::iter::FusedIterator>() {}
        fn assert_send<T: Send>() {}
        assert_iter::<ReaderIter>();
        assert_fused::<ReaderIter>();
        assert_send::<ReaderIter>();
        assert_send::<RtlSdrReader>();
    };
}
