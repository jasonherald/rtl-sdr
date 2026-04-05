//! DSP block trait and worker thread infrastructure.
//!
//! Ports SDR++ `dsp::block`. A block is a processing unit that runs on its
//! own worker thread, reading from input streams and writing to output streams.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

/// Trait for a DSP processing block that runs on a worker thread.
///
/// Blocks are the fundamental processing units in the pipeline. Each block:
/// - Has a `run()` method called repeatedly by the worker thread
/// - Returns the number of samples processed, or -1 to stop
/// - Can be started/stopped dynamically
pub trait Block: Send + 'static {
    /// Execute one processing cycle.
    ///
    /// Called repeatedly by the worker thread. Returns:
    /// - Positive value: number of samples processed
    /// - -1: stop the worker thread (graceful shutdown)
    fn run(&mut self) -> i32;
}

/// Worker thread wrapper for a `Block`.
///
/// Manages the lifecycle of a worker thread that repeatedly calls `block.run()`.
pub struct BlockRunner {
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl BlockRunner {
    /// Start a block on a new worker thread.
    ///
    /// The worker thread calls `block.run()` in a loop until it returns -1
    /// or `stop()` is called.
    pub fn start<B: Block>(mut block: B) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        let thread = thread::spawn(move || {
            while running_clone.load(Ordering::Relaxed) {
                let result = block.run();
                if result < 0 {
                    break;
                }
            }
        });

        Self {
            running,
            thread: Some(thread),
        }
    }

    /// Check if the block is still running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Stop the block and join the worker thread.
    ///
    /// Sets the running flag to false and waits for the thread to finish.
    /// The block must also return -1 from `run()` to fully exit (e.g., by
    /// stopping its input stream).
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for BlockRunner {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI32;
    use std::time::Duration;

    struct CountingBlock {
        count: Arc<AtomicI32>,
        max: i32,
    }

    impl Block for CountingBlock {
        fn run(&mut self) -> i32 {
            let c = self.count.fetch_add(1, Ordering::Relaxed);
            if c >= self.max {
                return -1;
            }
            // Small sleep to avoid spinning
            thread::sleep(Duration::from_micros(100));
            1
        }
    }

    #[test]
    fn test_block_runner_runs_and_stops() {
        let count = Arc::new(AtomicI32::new(0));
        let block = CountingBlock {
            count: Arc::clone(&count),
            max: 10,
        };
        let mut runner = BlockRunner::start(block);

        // Wait for block to finish
        thread::sleep(Duration::from_millis(50));

        runner.stop();
        assert!(count.load(Ordering::Relaxed) >= 10);
    }

    #[test]
    fn test_block_runner_early_stop() {
        let count = Arc::new(AtomicI32::new(0));
        let block = CountingBlock {
            count: Arc::clone(&count),
            max: 1_000_000, // Would run forever
        };
        let mut runner = BlockRunner::start(block);

        thread::sleep(Duration::from_millis(10));
        runner.stop();

        // Should have stopped well before max
        let final_count = count.load(Ordering::Relaxed);
        assert!(final_count > 0 && final_count < 1_000_000);
    }

    #[test]
    fn test_block_runner_drop_stops() {
        let count = Arc::new(AtomicI32::new(0));
        let block = CountingBlock {
            count: Arc::clone(&count),
            max: 1_000_000,
        };
        {
            let _runner = BlockRunner::start(block);
            thread::sleep(Duration::from_millis(10));
            // Runner dropped here — should stop the thread
        }

        let final_count = count.load(Ordering::Relaxed);
        assert!(final_count > 0);
    }
}
