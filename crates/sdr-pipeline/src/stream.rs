//! Double-buffer swap streaming channel.
//!
//! Ports SDR++ `dsp::stream<T>`. This is the fundamental inter-block data
//! transport. A producer writes into `write_buf`, then calls `swap(count)`
//! to publish the data. A consumer calls `read()` to wait for data,
//! processes `read_buf[..count]`, then calls `flush()` to release the buffer.
//!
//! All methods take `&self`, enabling the stream to be shared between
//! producer and consumer threads via `Arc<Stream<T>>`.
//!
//! # Safety
//!
//! This module uses `UnsafeCell` and `unsafe impl Send/Sync` for the buffer
//! storage. This is sound because the SPSC protocol guarantees that only one
//! thread accesses each buffer at a time, enforced by the Mutex+Condvar state.

use sdr_types::STREAM_BUFFER_SIZE;
use std::cell::UnsafeCell;
use std::sync::{Condvar, Mutex, PoisonError};

/// Double-buffer swap channel for streaming samples between DSP blocks.
///
/// Thread-safe single-producer, single-consumer channel with backpressure.
/// All methods take `&self` so the stream can be shared via `Arc<Stream<T>>`
/// between producer and consumer threads.
///
/// # Safety
///
/// This type uses `UnsafeCell` for the buffer storage to allow swapping
/// under the mutex without requiring `&mut self`. The mutex in `StreamState`
/// ensures that only one thread accesses the buffers at a time during swap.
/// Between swaps, the producer exclusively owns `write_buf` and the consumer
/// exclusively owns `read_buf` — this invariant is enforced by the protocol
/// (producer calls swap, consumer calls read then flush).
#[allow(clippy::doc_markdown)]
pub struct Stream<T: Copy + Send + Default + 'static> {
    /// Two buffers — index 0 and 1. Which is "write" vs "read" is tracked
    /// by `write_idx` in the state.
    bufs: [UnsafeCell<Box<[T]>>; 2],
    state: Mutex<StreamState>,
    swap_cv: Condvar,
    ready_cv: Condvar,
}

#[allow(clippy::struct_excessive_bools)]
struct StreamState {
    /// Index of the current write buffer (0 or 1). Read buffer is `1 - write_idx`.
    write_idx: usize,
    can_swap: bool,
    data_ready: bool,
    data_size: usize,
    reader_stop: bool,
    writer_stop: bool,
    capacity: usize,
}

// Safety: Stream uses Mutex+Condvar for all shared state access.
// The UnsafeCell buffers are only accessed by one thread at a time:
// - Producer writes to bufs[write_idx] exclusively between flushes
// - Consumer reads from bufs[1-write_idx] exclusively between read() and flush()
// - swap() occurs under the mutex, switching the indices atomically
#[allow(unsafe_code)]
unsafe impl<T: Copy + Send + Default + 'static> Send for Stream<T> {}
#[allow(unsafe_code)]
unsafe impl<T: Copy + Send + Default + 'static> Sync for Stream<T> {}

impl<T: Copy + Send + Default + 'static> Stream<T> {
    /// Create a new stream with the default buffer size.
    pub fn new() -> Self {
        Self::with_capacity(STREAM_BUFFER_SIZE)
    }

    /// Create a new stream with the given buffer capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bufs: [
                UnsafeCell::new(vec![T::default(); capacity].into_boxed_slice()),
                UnsafeCell::new(vec![T::default(); capacity].into_boxed_slice()),
            ],
            state: Mutex::new(StreamState {
                write_idx: 0,
                can_swap: true,
                data_ready: false,
                data_size: 0,
                reader_stop: false,
                writer_stop: false,
                capacity,
            }),
            swap_cv: Condvar::new(),
            ready_cv: Condvar::new(),
        }
    }

    /// Get a mutable reference to the write buffer.
    ///
    /// # Safety
    ///
    /// Safe because the producer is the only thread that accesses the write
    /// buffer between `swap()` calls. The SPSC protocol guarantees this.
    #[allow(unsafe_code, clippy::mut_from_ref, clippy::doc_markdown)]
    pub fn write_buf(&self) -> &mut [T] {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let idx = state.write_idx;
        drop(state);
        // Safety: only the producer thread calls write_buf(), and it's the
        // only thread accessing bufs[write_idx] between swap() calls.
        unsafe { &mut *self.bufs[idx].get() }
    }

    /// Get a reference to the read buffer.
    ///
    /// Valid after `read()` returns a positive count, until `flush()` is called.
    #[allow(unsafe_code)]
    pub fn read_buf(&self) -> &[T] {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let idx = 1 - state.write_idx;
        drop(state);
        // Safety: only the consumer thread calls read_buf(), and it's the
        // only thread accessing bufs[1-write_idx] between read() and flush().
        unsafe { &*self.bufs[idx].get() }
    }

    /// Swap the write buffer with the read buffer, publishing `size` samples.
    ///
    /// Blocks until the consumer has flushed the previous data.
    /// Returns `false` if the writer was stopped or `size` is invalid
    /// (zero or exceeds capacity).
    pub fn swap(&self, size: usize) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        if size == 0 || size > state.capacity {
            return false;
        }

        state = self
            .swap_cv
            .wait_while(state, |s| !s.can_swap && !s.writer_stop)
            .unwrap_or_else(PoisonError::into_inner);

        if state.writer_stop {
            return false;
        }

        // Swap buffers by flipping the index
        state.data_size = size;
        state.write_idx = 1 - state.write_idx;
        state.can_swap = false;
        state.data_ready = true;

        drop(state);
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
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.capacity
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
    use std::sync::Arc;
    use std::thread;

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
        let s: Stream<f32> = Stream::with_capacity(256);

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
        let s: Stream<f32> = Stream::with_capacity(256);

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
    fn test_swap_zero_size_returns_false() {
        let s: Stream<f32> = Stream::with_capacity(256);
        assert!(!s.swap(0));
    }

    #[test]
    fn test_swap_exceeds_capacity_returns_false() {
        let s: Stream<f32> = Stream::with_capacity(256);
        assert!(!s.swap(257));
    }

    #[test]
    fn test_stop_writer() {
        let s: Stream<f32> = Stream::with_capacity(256);
        s.stop_writer();
        assert!(!s.swap(1), "swap should return false when writer stopped");
        s.clear_write_stop();
        // After clearing, swap should work again
        s.write_buf()[0] = 1.0;
        assert!(s.swap(1), "swap should succeed after clear_write_stop");
    }

    #[test]
    fn test_stop_reader() {
        let s: Stream<f32> = Stream::with_capacity(256);
        s.stop_reader();
        assert_eq!(s.read(), -1, "read should return -1 when reader stopped");
        s.clear_read_stop();
        // After clearing, read should work again (need data first)
        s.write_buf()[0] = 1.0;
        s.swap(1);
        assert_eq!(s.read(), 1, "read should succeed after clear_read_stop");
    }

    #[test]
    fn test_producer_consumer_threads() {
        let stream = Arc::new(Stream::<f32>::with_capacity(256));

        let producer_stream = Arc::clone(&stream);
        let consumer_stream = Arc::clone(&stream);

        let n_iterations = 10;

        // Producer thread
        let producer = thread::spawn(move || {
            for i in 0..n_iterations {
                producer_stream.write_buf()[0] = (i + 1) as f32;
                assert!(producer_stream.swap(1));
            }
        });

        // Consumer thread
        let consumer = thread::spawn(move || {
            let mut total = 0;
            for _ in 0..n_iterations {
                let count = consumer_stream.read();
                assert!(count > 0);
                total += count;
                consumer_stream.flush();
            }
            assert_eq!(total, n_iterations);
        });

        producer.join().unwrap();
        consumer.join().unwrap();
    }

    #[test]
    fn test_data_integrity_across_threads() {
        let stream = Arc::new(Stream::<f32>::with_capacity(256));

        let producer_stream = Arc::clone(&stream);
        let consumer_stream = Arc::clone(&stream);

        let n_iterations = 100;

        let producer = thread::spawn(move || {
            for i in 0..n_iterations {
                let buf = producer_stream.write_buf();
                buf[0] = i as f32;
                buf[1] = (i * 2) as f32;
                assert!(producer_stream.swap(2));
            }
        });

        let consumer = thread::spawn(move || {
            for i in 0..n_iterations {
                let count = consumer_stream.read();
                assert_eq!(count, 2);
                let buf = consumer_stream.read_buf();
                assert!((buf[0] - i as f32).abs() < f32::EPSILON);
                assert!((buf[1] - (i * 2) as f32).abs() < f32::EPSILON);
                consumer_stream.flush();
            }
        });

        producer.join().unwrap();
        consumer.join().unwrap();
    }
}
