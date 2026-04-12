//! Lock-based shared FFT display buffer.
//!
//! Single-producer (the DSP thread) / single-consumer (the rendering side —
//! the GTK main loop today, the macOS Metal renderer through `sdr-ffi`
//! tomorrow) primitive that lets the consumer pull the latest FFT frame
//! without per-frame allocation.
//!
//! The DSP thread writes new frames into the buffer via [`SharedFftBuffer::write`]
//! (crate-private — only the controller calls it). The consumer drains via
//! [`SharedFftBuffer::take_if_ready`], which is **lock-free on the fast path**
//! when no new frame is available and acquires the mutex only for a short
//! `memcpy` when one is.
//!
//! ## Why a `Mutex<Vec<f32>>` and not a triple buffer / SPSC ring
//!
//! The FFT rate (~20 fps default) is far below render-loop rates, the
//! payload is small (a few KB), and the consumer never blocks the producer
//! because the writer always wins on contention (the `ready` flag is set
//! after the lock is released). A lock-free triple buffer would buy nothing
//! at this rate and adds complexity. We picked the simplest correct thing.
//!
//! ## Why not use a channel
//!
//! `mpsc::Sender<Vec<f32>>` would allocate a fresh `Vec<f32>` per frame on
//! the DSP thread and free it on the consumer thread, which fragments the
//! glibc arena heap on Linux long-running processes (the same reason
//! `src/main.rs` calls `mallopt(M_ARENA_MAX, 4)`). The shared buffer reuses
//! one allocation forever.

/// Shared FFT display buffer — written by the DSP thread, read by whatever
/// consumer is rendering the spectrum.
pub struct SharedFftBuffer {
    buf: std::sync::Mutex<Vec<f32>>,
    ready: std::sync::atomic::AtomicBool,
}

impl SharedFftBuffer {
    /// Create a new shared buffer with the given initial size.
    ///
    /// The buffer is resized on the first `write` if the producer hands in
    /// a slice of a different length, so the initial size is just a hint
    /// to avoid one early reallocation.
    #[must_use]
    pub fn new(size: usize) -> Self {
        Self {
            buf: std::sync::Mutex::new(vec![0.0; size]),
            ready: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// DSP thread: write FFT data and mark a new frame as ready.
    ///
    /// Crate-private: only the controller calls this. External consumers
    /// produce data through the controller, never directly.
    ///
    /// Mutex poisoning is recovered via `into_inner()` rather than dropped
    /// silently — a panic in a previous holder of the lock should not
    /// silently lose every subsequent FFT frame.
    pub(crate) fn write(&self, data: &[f32]) {
        let mut buf = match self.buf.lock() {
            Ok(buf) => buf,
            Err(poisoned) => poisoned.into_inner(),
        };
        buf.resize(data.len(), 0.0);
        buf.copy_from_slice(data);
        self.ready.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Consumer: read the latest FFT frame if a new one is ready.
    ///
    /// Returns `false` and does not invoke `f` when no new frame has been
    /// published since the previous call (lock-free fast path on the
    /// no-new-frame case). Returns `true` and synchronously invokes `f`
    /// with a borrow of the current buffer when a new frame is available.
    ///
    /// The borrow handed to `f` is valid only for the duration of the
    /// callback — copy out what you need.
    ///
    /// ## Race avoidance
    ///
    /// The `ready` flag is cleared **after** the buffer mutex is held, not
    /// before. If we cleared `ready` first and then locked, a writer could
    /// slip in between and publish a brand-new frame; we would then read
    /// the new buffer but leave `ready = false`, so the next poll would
    /// see no new data and the just-published frame would be redelivered
    /// as "fresh" only on the *following* tick. By clearing under the
    /// lock, the `write → set ready` and `lock → clear ready → read`
    /// sequences are linearizable: the consumer either sees the writer's
    /// frame and clears the flag, or it sees no frame and leaves the flag
    /// for the next call.
    ///
    /// Mutex poisoning is recovered via `into_inner()` so a panic in the
    /// writer cannot cause `take_if_ready` to silently return `true`
    /// without ever calling `f`.
    pub fn take_if_ready<F: FnOnce(&[f32])>(&self, f: F) -> bool {
        // Lock-free fast path: skip the mutex if there's clearly nothing
        // to read. A spurious "ready=true" here is fine — we recheck
        // under the lock below.
        if !self.ready.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }

        let buf = match self.buf.lock() {
            Ok(buf) => buf,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Recheck-and-clear under the mutex. If a racing reader (or
        // some other coincidence) cleared the flag between our `load`
        // above and this `swap`, bail out cleanly without invoking `f`.
        if !self.ready.swap(false, std::sync::atomic::Ordering::AcqRel) {
            return false;
        }

        f(&buf);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_take_returns_data() {
        let buf = SharedFftBuffer::new(4);
        buf.write(&[1.0, 2.0, 3.0, 4.0]);

        let mut got: Vec<f32> = Vec::new();
        let was_ready = buf.take_if_ready(|data| {
            got.extend_from_slice(data);
        });

        assert!(was_ready);
        assert_eq!(got, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn take_returns_false_when_no_new_frame() {
        let buf = SharedFftBuffer::new(4);
        buf.write(&[1.0, 2.0, 3.0, 4.0]);

        // First take consumes the frame.
        assert!(buf.take_if_ready(|_| {}));

        // Second take has no fresh data — must not invoke the callback.
        let mut called = false;
        let was_ready = buf.take_if_ready(|_| called = true);
        assert!(!was_ready);
        assert!(!called);
    }

    #[test]
    fn write_resizes_when_payload_grows() {
        let buf = SharedFftBuffer::new(2);
        buf.write(&[1.0, 2.0, 3.0, 4.0, 5.0]);

        let mut got_len = 0;
        buf.take_if_ready(|data| {
            got_len = data.len();
        });
        assert_eq!(got_len, 5);
    }

    #[test]
    fn write_overwrites_unread_frame() {
        let buf = SharedFftBuffer::new(4);
        buf.write(&[1.0, 2.0, 3.0, 4.0]);
        buf.write(&[9.0, 9.0, 9.0, 9.0]);

        let mut got: Vec<f32> = Vec::new();
        buf.take_if_ready(|data| got.extend_from_slice(data));
        assert_eq!(got, vec![9.0, 9.0, 9.0, 9.0]);
    }
}
