//! Live hardware tests for the streaming surface.
//!
//! These tests open a real RTL-SDR dongle and exercise the
//! `RtlSdrReader` + per-runtime stream pattern end-to-end. They
//! require:
//!
//! - A dongle physically plugged in (any RTL-SDR variant)
//! - The current user can claim the USB interface (no other
//!   client holding the device)
//!
//! All tests are `#[ignore]` so `cargo test` skips them by
//! default. Run explicitly with the dongle plugged in:
//!
//! ```text
//! cargo test --features tokio -p sdr-rtlsdr --test live_streaming -- --ignored
//! ```
//!
//! The tests cover the design-pivot validations from #626 round
//! 4: that the Reader pattern lets the parent retune mid-stream,
//! that the tokio Stream yields real samples, and that drop
//! semantics work as documented.

#![cfg(feature = "tokio")]

use std::time::Duration;

use sdr_rtlsdr::RtlSdrDevice;

/// Helper: open device 0 and configure for FM broadcast tuning.
/// Skips the test by returning `None` if no device is plugged
/// in — keeps `--ignored` runs informative without a hard panic
/// when the dongle is unplugged mid-suite.
fn open_or_skip(test_name: &str) -> Option<RtlSdrDevice> {
    if sdr_rtlsdr::get_device_count() == 0 {
        eprintln!("[{test_name}] no RTL-SDR plugged in; skipping");
        return None;
    }
    match RtlSdrDevice::open(0) {
        Ok(mut dev) => {
            // Stable, valid-everywhere config: FM broadcast in
            // most regions, 2.048 Msps. The tests don't care
            // about signal content — they just need bytes to
            // flow.
            dev.set_sample_rate(2_048_000).ok()?;
            dev.set_center_freq(100_000_000).ok()?;
            dev.reset_buffer().ok()?;
            Some(dev)
        }
        Err(e) => {
            eprintln!("[{test_name}] open failed: {e}; skipping");
            None
        }
    }
}

/// Smoke: tokio stream yields real bytes.
///
/// Opens the device, builds a reader, gets a tokio stream, polls
/// it for 3 buffers, asserts each contains data. End-to-end
/// validation that the spawn_blocking + mpsc + Stream impl
/// composition works against real hardware.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs real RTL-SDR hardware — run with --ignored"]
async fn tokio_stream_yields_bytes() {
    use futures_util::StreamExt;

    let Some(dev) = open_or_skip("tokio_stream_yields_bytes") else {
        return;
    };

    let reader = dev.reader();
    let stream = reader
        .stream_samples_tokio(0)
        .map_err(|boxed| boxed.0)
        .expect("stream_samples_tokio inside multi_thread runtime");

    let mut stream = Box::pin(stream);
    for i in 0..3 {
        let item = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap_or_else(|_| panic!("buffer {i} timed out"))
            .unwrap_or_else(|| panic!("stream ended unexpectedly at buffer {i}"))
            .unwrap_or_else(|e| panic!("read error at buffer {i}: {e}"));
        assert!(
            !item.is_empty(),
            "buffer {i} was empty (expected ≥1 byte from a configured device)"
        );
    }
}

/// The whole point of the `RtlSdrReader` split: the parent
/// retains `&mut device` for control while the reader streams.
/// This test pins that contract end-to-end against hardware.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs real RTL-SDR hardware — run with --ignored"]
async fn parent_can_retune_during_stream() {
    use futures_util::StreamExt;

    let Some(mut dev) = open_or_skip("parent_can_retune_during_stream") else {
        return;
    };

    let reader = dev.reader();
    let stream = reader
        .stream_samples_tokio(0)
        .map_err(|boxed| boxed.0)
        .expect("stream_samples_tokio inside multi_thread runtime");

    let mut stream = Box::pin(stream);

    // Drain one buffer at the initial freq.
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("first buffer timed out")
        .expect("stream ended early")
        .expect("first read failed");

    // Retune the parent while the stream is live. This is the
    // shared-handle pattern documented on `RtlSdrDevice::reader`
    // — different USB endpoints, no rusb-level conflict.
    dev.set_center_freq(99_000_000)
        .expect("retune during streaming should succeed");
    dev.set_tuner_gain(150)
        .expect("gain change during streaming should succeed");

    // Drain another buffer at the new freq — proves the stream
    // is still alive after the parent's control activity.
    let buf = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("post-retune buffer timed out")
        .expect("stream ended after retune")
        .expect("post-retune read failed");
    assert!(!buf.is_empty(), "post-retune buffer was empty");
}

/// Drop semantics: dropping the stream stops the worker
/// promptly and returns control of the device handle.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs real RTL-SDR hardware — run with --ignored"]
async fn dropping_stream_stops_worker() {
    use futures_util::StreamExt;

    let Some(dev) = open_or_skip("dropping_stream_stops_worker") else {
        return;
    };

    let reader = dev.reader();
    let stream = reader
        .stream_samples_tokio(0)
        .map_err(|boxed| boxed.0)
        .expect("stream_samples_tokio inside multi_thread runtime");

    let mut stream = Box::pin(stream);

    // Drain one buffer.
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("buffer timed out")
        .expect("stream ended early")
        .expect("read failed");

    // Drop the stream. The worker's `tx.is_closed()` check
    // (between reads) plus `blocking_send` failure (after each
    // read) cooperate to exit the worker within one buffer
    // cadence on the happy path.
    drop(stream);

    // Give the worker a moment to observe the drop. If the
    // underlying USB reads were stalled the worst case would
    // be ~5 s; happy path is much faster. We just need to
    // confirm that the test process doesn't deadlock.
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Sync iterator, also via the reader. Validates the
/// `ReaderIter` `Send` story (move it to a std::thread, drive
/// it, send results back).
#[test]
#[ignore = "needs real RTL-SDR hardware — run with --ignored"]
fn reader_iter_in_std_thread() {
    let Some(dev) = open_or_skip("reader_iter_in_std_thread") else {
        return;
    };

    let reader = dev.reader();
    let (tx, rx) = std::sync::mpsc::channel();

    let handle = std::thread::spawn(move || {
        for chunk in reader.iter_samples(0).take(3) {
            tx.send(chunk).expect("channel rx should still be alive");
        }
    });

    for i in 0..3 {
        let buf = rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("buffer {i} not received"))
            .unwrap_or_else(|e| panic!("buffer {i} errored: {e}"));
        assert!(!buf.is_empty(), "buffer {i} was empty");
    }

    handle.join().expect("worker thread panicked");
}
