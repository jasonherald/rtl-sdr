//! Double-buffer swap streaming channel.
//!
//! Ports SDR++ `dsp::stream<T>`. This is the fundamental inter-block data
//! transport. A producer writes into `write_buf`, then calls `swap(count)`
//! to publish the data. A consumer calls `read()` to wait for data,
//! processes `read_buf[..count]`, then calls `flush()` to release the buffer.

use sdr_types::STREAM_BUFFER_SIZE;
use std::sync::{Condvar, Mutex, PoisonError};

/// Double-buffer swap channel for streaming samples between DSP blocks.
///
/// Thread-safe single-producer, single-consumer channel with backpressure.
/// The producer writes to `write_buf` and swaps; the consumer reads from
/// `read_buf` after `read()` returns.
pub struct Stream<T: Copy + Send + Default + 'static> {
    write_buf: Box<[T]>,
    read_buf: Box<[T]>,
    state: Mutex<StreamState>,
    swap_cv: Condvar,
    ready_cv: Condvar,
}

#[allow(clippy::struct_excessive_bools)]
struct StreamState {
    can_swap: bool,
    data_ready: bool,
    data_size: usize,
    reader_stop: bool,
    writer_stop: bool,
}

impl<T: Copy + Send + Default + 'static> Stream<T> {
    /// Create a new stream with the default buffer size.
    pub fn new() -> Self {
        Self::with_capacity(STREAM_BUFFER_SIZE)
    }

    /// Create a new stream with the given buffer capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            write_buf: vec![T::default(); capacity].into_boxed_slice(),
            read_buf: vec![T::default(); capacity].into_boxed_slice(),
            state: Mutex::new(StreamState {
                can_swap: true,
                data_ready: false,
                data_size: 0,
                reader_stop: false,
                writer_stop: false,
            }),
            swap_cv: Condvar::new(),
            ready_cv: Condvar::new(),
        }
    }

    /// Get a mutable reference to the write buffer.
    pub fn write_buf(&mut self) -> &mut [T] {
        &mut self.write_buf
    }

    /// Get a reference to the read buffer.
    pub fn read_buf(&self) -> &[T] {
        &self.read_buf
    }

    /// Swap the write buffer with the read buffer, publishing `size` samples.
    ///
    /// Blocks until the consumer has flushed the previous data.
    /// Returns `false` if the writer was stopped (graceful shutdown).
    pub fn swap(&mut self, size: usize) -> bool {
        if size == 0 || size > self.write_buf.len() {
            return false;
        }

        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state = self
                .swap_cv
                .wait_while(state, |s| !s.can_swap && !s.writer_stop)
                .unwrap_or_else(PoisonError::into_inner);

            if state.writer_stop {
                return false;
            }

            state.data_size = size;
            std::mem::swap(&mut self.write_buf, &mut self.read_buf);
            state.can_swap = false;
            state.data_ready = true;
        }
        self.ready_cv.notify_all();

        true
    }

    /// Wait for data to be available in the read buffer.
    ///
    /// Returns the number of samples available, or `-1` if the reader was stopped.
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    pub fn read(&self) -> i32 {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let state = self
            .ready_cv
            .wait_while(state, |s| !s.data_ready && !s.reader_stop)
            .unwrap_or_else(PoisonError::into_inner);

        if state.reader_stop {
            return -1;
        }

        state.data_size as i32
    }

    /// Release the read buffer, allowing the producer to swap again.
    pub fn flush(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.data_ready = false;
            state.can_swap = true;
        }
        self.swap_cv.notify_all();
    }

    /// Signal the writer to stop (unblocks a pending `swap()`).
    pub fn stop_writer(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.writer_stop = true;
        }
        self.swap_cv.notify_all();
    }

    /// Clear the writer stop flag.
    pub fn clear_write_stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.writer_stop = false;
    }

    /// Signal the reader to stop (unblocks a pending `read()`).
    pub fn stop_reader(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.reader_stop = true;
        }
        self.ready_cv.notify_all();
    }

    /// Clear the reader stop flag.
    pub fn clear_read_stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.reader_stop = false;
    }

    /// Buffer capacity (number of samples).
    pub fn capacity(&self) -> usize {
        self.write_buf.len()
    }
}

impl<T: Copy + Send + Default + 'static> Default for Stream<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
mod tests {
    use super::*;

    #[test]
    fn test_new_default_capacity() {
        let s: Stream<f32> = Stream::new();
        assert_eq!(s.capacity(), STREAM_BUFFER_SIZE);
    }

    #[test]
    fn test_with_capacity() {
        let s: Stream<f32> = Stream::with_capacity(1024);
        assert_eq!(s.capacity(), 1024);
    }

    #[test]
    fn test_single_thread_swap_read_flush() {
        let mut s: Stream<f32> = Stream::with_capacity(256);

        s.write_buf()[0] = 42.0;
        s.write_buf()[1] = 43.0;
        assert!(s.swap(2));

        let count = s.read();
        assert_eq!(count, 2);
        assert!((s.read_buf()[0] - 42.0).abs() < f32::EPSILON);
        assert!((s.read_buf()[1] - 43.0).abs() < f32::EPSILON);

        s.flush();
    }

    #[test]
    fn test_multiple_swap_read_cycles() {
        let mut s: Stream<f32> = Stream::with_capacity(256);

        for cycle in 0..5 {
            let val = (cycle + 1) as f32;
            s.write_buf()[0] = val;
            assert!(s.swap(1));

            let count = s.read();
            assert_eq!(count, 1);
            assert!((s.read_buf()[0] - val).abs() < f32::EPSILON);
            s.flush();
        }
    }

    #[test]
    fn test_stop_writer() {
        let mut s: Stream<f32> = Stream::with_capacity(256);
        s.stop_writer();
        assert!(!s.swap(1), "swap should return false when writer stopped");
        s.clear_write_stop();
    }

    #[test]
    fn test_stop_reader() {
        let s: Stream<f32> = Stream::with_capacity(256);
        s.stop_reader();
        assert_eq!(s.read(), -1, "read should return -1 when reader stopped");
        s.clear_read_stop();
    }
}
