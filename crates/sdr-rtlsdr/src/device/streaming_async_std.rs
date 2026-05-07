//! async-std `Stream` adapter for IQ-sample reads.
//!
//! Gated on `feature = "async-std"`. Bridges the synchronous USB
//! bulk-read path into a `Stream` consumable from an async-std
//! executor without blocking it.
//!
//! Mirrors the tokio variant (`super::streaming_tokio`) with two
//! differences:
//!
//! - **Blocking offload** uses [`async_std::task::spawn_blocking`]
//!   instead of `tokio::task::spawn_blocking`.
//! - **mpsc bridge** uses the runtime-agnostic [`async_channel`]
//!   crate. `Receiver` is `!Unpin`; we store it as `Pin<Box<…>>`
//!   so we can pin-project safely without unsafe code or a
//!   `pin-project` macro dep.
//!
//! Same back-pressure-by-default channel depth, same drop
//! semantics (~65 ms between-reads, up to one read timeout for
//! mid-read; #633 tracks proper libusb-cancel).

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use crate::error::RtlSdrError;

use super::RtlSdrReader;

const STREAM_BACKPRESSURE_DEPTH: usize = 4;

type BoxedReceiver = Pin<Box<async_channel::Receiver<Result<Vec<u8>, RtlSdrError>>>>;

impl RtlSdrReader {
    /// Stream IQ samples as an async-std-friendly `Stream`.
    ///
    /// Same shape as `Self::stream_samples_tokio` (only present
    /// when the `tokio` feature is enabled). Differs only in
    /// which runtime drives the blocking offload.
    ///
    /// # Errors
    ///
    /// Currently never fails at the preflight stage —
    /// `async_std::task::spawn_blocking` works without an
    /// active executor. Error type kept as
    /// `Box<(RtlSdrError, Self)>` for shape parity with the
    /// other runtime variants.
    ///
    /// ```no_run
    /// # #[cfg(feature = "async-std")]
    /// # async fn example() -> Result<(), sdr_rtlsdr::RtlSdrError> {
    /// use futures_core::Stream;
    /// use std::pin::Pin;
    /// use sdr_rtlsdr::RtlSdrDevice;
    ///
    /// let mut dev = RtlSdrDevice::open(0)?;
    /// dev.reset_buffer()?;
    /// let reader = dev.reader();
    /// let stream = reader.stream_samples_async_std(262_144).map_err(|boxed| boxed.0)?;
    /// let mut stream: Pin<Box<dyn Stream<Item = _>>> = Box::pin(stream);
    /// # Ok(())
    /// # }
    /// ```
    pub fn stream_samples_async_std(
        self,
        buffer_size: usize,
    ) -> Result<AsyncStdSampleStream, Box<(RtlSdrError, Self)>> {
        let (tx, rx) = async_channel::bounded(STREAM_BACKPRESSURE_DEPTH);

        async_std::task::spawn_blocking(move || {
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
        });

        Ok(AsyncStdSampleStream { rx: Box::pin(rx) })
    }
}

/// async-std's `Stream` over IQ-sample buffers, returned by
/// [`RtlSdrReader::stream_samples_async_std`].
pub struct AsyncStdSampleStream {
    rx: BoxedReceiver,
}

impl Stream for AsyncStdSampleStream {
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
        assert_stream::<AsyncStdSampleStream>();
        assert_send::<AsyncStdSampleStream>();
    };
}
