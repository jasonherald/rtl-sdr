//! smol `Stream` adapter for IQ-sample reads.
//!
//! Gated on `feature = "smol"`. Bridges the synchronous USB
//! bulk-read path into a `Stream` consumable from a smol-family
//! executor (smol, async-executor, async-global-executor) without
//! blocking it.
//!
//! Mirrors the async-std variant (`super::streaming_async_std`)
//! with one difference: the blocking offload uses
//! [`blocking::unblock`] (the foundation `smol::unblock` re-
//! exports) rather than async-std's spawn_blocking. Same
//! `async_channel` mpsc bridge, same back-pressure shape, same
//! drop semantics (#633 tracks libusb-cancel).
//!
//! [`blocking::unblock`] returns a [`blocking::Task`] which
//! cancels its underlying work if dropped. We call `.detach()`
//! so the worker runs to natural completion — matches the
//! fire-and-forget shape of the tokio / async-std variants.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use crate::error::RtlSdrError;

use super::RtlSdrReader;

const STREAM_BACKPRESSURE_DEPTH: usize = 4;

type BoxedReceiver = Pin<Box<async_channel::Receiver<Result<Vec<u8>, RtlSdrError>>>>;

impl RtlSdrReader {
    /// Stream IQ samples as a smol-friendly `Stream`.
    ///
    /// Same shape as `Self::stream_samples_tokio` (only present
    /// when the `tokio` feature is enabled). Differs only in
    /// which runtime drives the blocking offload.
    ///
    /// # Errors
    ///
    /// Currently never fails at the preflight stage —
    /// [`blocking::unblock`] runs on its own internal thread
    /// pool independent of any active executor. Error type kept
    /// as `Box<(RtlSdrError, Self)>` for shape parity.
    ///
    /// ```no_run
    /// # #[cfg(feature = "smol")]
    /// # async fn example() -> Result<(), sdr_rtlsdr::RtlSdrError> {
    /// use futures_core::Stream;
    /// use std::pin::Pin;
    /// use sdr_rtlsdr::RtlSdrDevice;
    ///
    /// let mut dev = RtlSdrDevice::open(0)?;
    /// dev.reset_buffer()?;
    /// let reader = dev.reader();
    /// let stream = reader.stream_samples_smol(262_144).map_err(|boxed| boxed.0)?;
    /// let mut stream: Pin<Box<dyn Stream<Item = _>>> = Box::pin(stream);
    /// # Ok(())
    /// # }
    /// ```
    pub fn stream_samples_smol(
        self,
        buffer_size: usize,
    ) -> Result<SmolSampleStream, Box<(RtlSdrError, Self)>> {
        let (tx, rx) = async_channel::bounded(STREAM_BACKPRESSURE_DEPTH);

        blocking::unblock(move || {
            let reader = self;
            let mut iter = reader.iter_samples(buffer_size);
            loop {
                if tx.is_closed() {
                    return;
                }
                match iter.next() {
                    Some(chunk) => {
                        let is_err = chunk.is_err();
                        if tx.send_blocking(chunk).is_err() {
                            return;
                        }
                        if is_err {
                            return;
                        }
                    }
                    None => return,
                }
            }
        })
        .detach();

        Ok(SmolSampleStream { rx: Box::pin(rx) })
    }
}

/// smol's `Stream` over IQ-sample buffers, returned by
/// [`RtlSdrReader::stream_samples_smol`].
pub struct SmolSampleStream {
    rx: BoxedReceiver,
}

impl Stream for SmolSampleStream {
    type Item = Result<Vec<u8>, RtlSdrError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const _: fn() = || {
        fn assert_stream<T: Stream>() {}
        fn assert_send<T: Send>() {}
        assert_stream::<SmolSampleStream>();
        assert_send::<SmolSampleStream>();
    };
}
