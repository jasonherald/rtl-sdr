//! Lock-based SPSC ring buffer for interleaved audio samples.
//!
//! Used by both backend implementations (`pw_impl` for PipeWire on Linux,
//! `coreaudio_impl` for AUHAL on macOS). Pre-allocated at startup; the
//! mutex is held only for memcpy duration (single-digit microseconds in
//! practice), so it never blocks the audio I/O thread for a meaningful
//! length of time. The reader uses `try_lock` and returns "0 samples"
//! on contention, which manifests as a single missed quantum (≈5–10 ms
//! of silence) — preferable to blocking the real-time callback.
//!
//! Behavior is intentionally simple:
//!
//! - **Producer side** (`write`) drops the oldest data when the buffer
//!   is full. The DSP thread always wins; we'd rather smear time than
//!   block the audio path.
//! - **Consumer side** (`read`) is non-blocking via `try_lock` and
//!   returns the number of samples actually copied (zero on contention
//!   or empty buffer).
//! - **`clear`** is used on stream start/stop to avoid replaying stale
//!   audio when the source is restarted.
//!
//! This module is `pub(crate)` — it's an internal primitive shared by
//! the two backends, not part of the crate's public surface.

/// Lock-based SPSC ring buffer for interleaved audio samples.
///
/// Pre-allocated at startup — zero allocation during streaming.
/// The mutex is held only for memcpy duration.
pub(crate) struct AudioRingBuffer {
    buf: std::sync::Mutex<AudioRingInner>,
}

struct AudioRingInner {
    data: Vec<f32>,
    read_pos: usize,
    write_pos: usize,
    count: usize,
    capacity: usize,
}

impl AudioRingBuffer {
    /// Create a new ring buffer with the given capacity in `f32` samples.
    /// For interleaved stereo audio, capacity should be `frames * 2`.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero. The write/read paths use `% capacity`
    /// arithmetic and would panic on the first call against a zero-cap
    /// ring; failing fast at construction surfaces the bug at the call
    /// site instead of inside the audio I/O thread later. The constant
    /// callers (`pw_impl::RING_CAPACITY`, `coreaudio_impl::RING_CAPACITY`)
    /// always pass non-zero values, so in practice this assert only
    /// fires for tests or future callers that compute capacity dynamically.
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "AudioRingBuffer capacity must be non-zero");
        Self {
            buf: std::sync::Mutex::new(AudioRingInner {
                data: vec![0.0; capacity],
                read_pos: 0,
                write_pos: 0,
                count: 0,
                capacity,
            }),
        }
    }

    /// Write samples into the ring buffer. Drops oldest data if full.
    ///
    /// Mutex poisoning is recovered via [`PoisonError::into_inner`]
    /// rather than dropped silently. A panic in a previous holder of
    /// the lock should not permanently mute the buffer for the rest
    /// of the process — the next frame must still get through.
    pub(crate) fn write(&self, samples: &[f32]) {
        let mut inner = match self.buf.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        let cap = inner.capacity;
        let mut wp = inner.write_pos;
        let mut rp = inner.read_pos;
        let mut cnt = inner.count;
        for &s in samples {
            inner.data[wp] = s;
            wp = (wp + 1) % cap;
            if cnt < cap {
                cnt += 1;
            } else {
                rp = (rp + 1) % cap;
            }
        }
        inner.write_pos = wp;
        inner.read_pos = rp;
        inner.count = cnt;
    }

    /// Clear the ring buffer (used on start/stop to avoid replaying stale audio).
    ///
    /// Mutex poisoning is recovered the same way as [`Self::write`].
    pub(crate) fn clear(&self) {
        let mut inner = match self.buf.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.read_pos = 0;
        inner.write_pos = 0;
        inner.count = 0;
    }

    /// Read up to `output.len()` samples. Returns count read.
    ///
    /// Uses `try_lock` to avoid blocking the audio I/O callback thread.
    /// Returns 0 on contention (`WouldBlock`); the caller renders
    /// silence for that quantum and tries again on the next callback.
    /// Mutex poisoning is recovered via `into_inner()` so a previous
    /// panic does not permanently mute the audio path.
    pub(crate) fn read(&self, output: &mut [f32]) -> usize {
        use std::sync::TryLockError;

        let mut inner = match self.buf.try_lock() {
            Ok(inner) => inner,
            Err(TryLockError::WouldBlock) => return 0, // Contended this cycle.
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        let to_read = output.len().min(inner.count);
        let mut rp = inner.read_pos;
        let cap = inner.capacity;
        for out in output.iter_mut().take(to_read) {
            *out = inner.data[rp];
            rp = (rp + 1) % cap;
        }
        inner.read_pos = rp;
        inner.count -= to_read;
        to_read
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    // The ring buffer copies samples bit-for-bit; comparing exact f32
    // values here is correct because no arithmetic is performed on the
    // values between write and read.
    clippy::float_cmp,
)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trip() {
        let ring = AudioRingBuffer::new(16);
        ring.write(&[1.0, 2.0, 3.0, 4.0]);

        let mut out = [0.0_f32; 4];
        let n = ring.read(&mut out);

        assert_eq!(n, 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn read_returns_zero_when_empty() {
        let ring = AudioRingBuffer::new(16);
        let mut out = [0.0_f32; 4];
        assert_eq!(ring.read(&mut out), 0);
    }

    #[test]
    fn read_partial_when_buffer_smaller_than_request() {
        let ring = AudioRingBuffer::new(16);
        ring.write(&[1.0, 2.0]);

        let mut out = [0.0_f32; 4];
        let n = ring.read(&mut out);

        assert_eq!(n, 2);
        assert_eq!(out[..2], [1.0, 2.0]);
        // Trailing slots are untouched.
        assert_eq!(out[2..], [0.0, 0.0]);
    }

    #[test]
    fn write_overflow_drops_oldest() {
        // Capacity 4: write 6, expect to read the most recent 4.
        let ring = AudioRingBuffer::new(4);
        ring.write(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);

        let mut out = [0.0_f32; 4];
        let n = ring.read(&mut out);

        assert_eq!(n, 4);
        assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn clear_resets_state() {
        let ring = AudioRingBuffer::new(16);
        ring.write(&[1.0, 2.0, 3.0]);
        ring.clear();

        let mut out = [0.0_f32; 4];
        assert_eq!(ring.read(&mut out), 0);
    }

    #[test]
    fn write_then_read_then_write_continues_correctly() {
        let ring = AudioRingBuffer::new(8);
        ring.write(&[1.0, 2.0, 3.0]);

        let mut out = [0.0_f32; 2];
        assert_eq!(ring.read(&mut out), 2);
        assert_eq!(out, [1.0, 2.0]);

        ring.write(&[4.0, 5.0]);

        let mut out = [0.0_f32; 4];
        let n = ring.read(&mut out);
        assert_eq!(n, 3);
        assert_eq!(out[..3], [3.0, 4.0, 5.0]);
    }

    #[test]
    fn wraparound_across_buffer_end() {
        // Capacity 4. Write 3, read 3, then write 4 — the writes wrap.
        let ring = AudioRingBuffer::new(4);
        ring.write(&[1.0, 2.0, 3.0]);

        let mut out = [0.0_f32; 3];
        ring.read(&mut out);

        ring.write(&[10.0, 20.0, 30.0, 40.0]);

        let mut out = [0.0_f32; 4];
        let n = ring.read(&mut out);
        assert_eq!(n, 4);
        assert_eq!(out, [10.0, 20.0, 30.0, 40.0]);
    }

    #[test]
    #[should_panic(expected = "capacity must be non-zero")]
    fn zero_capacity_panics_at_construction() {
        // Surfaces the bug at the call site instead of letting an
        // invalid ring survive until the first write tries to do
        // `% 0` on the audio I/O thread.
        let _ = AudioRingBuffer::new(0);
    }
}
