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
    pub(crate) fn new(capacity: usize) -> Self {
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
    /// Mutex poisoning is silently dropped on the producer side: a panic
    /// in a previous holder of the lock should not propagate into the
    /// DSP thread, which has no useful recovery action. The frame is
    /// lost; the next frame will succeed (the lock is released even on
    /// poison).
    pub(crate) fn write(&self, samples: &[f32]) {
        let Ok(mut inner) = self.buf.lock() else {
            return;
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
    pub(crate) fn clear(&self) {
        let Ok(mut inner) = self.buf.lock() else {
            return;
        };
        inner.read_pos = 0;
        inner.write_pos = 0;
        inner.count = 0;
    }

    /// Read up to `output.len()` samples. Returns count read.
    ///
    /// Uses `try_lock` to avoid blocking the audio I/O callback thread.
    /// Returns 0 on contention; the caller renders silence for that
    /// quantum and tries again on the next callback.
    pub(crate) fn read(&self, output: &mut [f32]) -> usize {
        let Ok(mut inner) = self.buf.try_lock() else {
            return 0; // Contended — return silence this cycle.
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
}
