#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::needless_range_loop,
    clippy::redundant_closure_for_method_calls,
    clippy::unnecessary_literal_bound,
    clippy::doc_markdown,
    clippy::manual_midpoint,
    clippy::redundant_closure
)]
//! RTL-SDR source module — wraps sdr-rtlsdr for the pipeline.
//!
//! Owns a USB reader thread and lock-free ring buffer. Converts raw
//! uint8 IQ samples from the USB device to f32 Complex samples for
//! the signal processing pipeline.

use sdr_pipeline::source_manager::Source;
use sdr_rtlsdr::RtlSdrDevice;
use sdr_types::{Complex, SourceError};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// IQ sample conversion factor: `(sample - 127.4) / 128.0`
///
/// Matches SDR++ `RTLSDRSourceModule::asyncHandler`.
const IQ_OFFSET: f32 = 127.4;
const IQ_SCALE: f32 = 128.0;

/// Raw USB buffer size in bytes — matches the original librtlsdr
/// async-transfer buffer size. Larger buffers mean fewer bulk
/// transfers per second and less per-transfer overhead. This
/// matters a lot on macOS where IOKit's USB layer has measurably
/// higher per-transfer latency than Linux kernel USB — at the
/// original 32 KB per transfer we were seeing only ~45% of the
/// configured source rate (900 kSps instead of 2 MSps) before the
/// device-side FIFO would drop samples. At 256 KB per transfer the
/// overhead drops enough to sustain the full configured rate.
///
/// Each USB transfer delivers `RAW_BUF_SIZE / 2` IQ pairs (1 byte
/// I + 1 byte Q per pair). The DSP thread consumes them in smaller
/// chunks via `read_samples` — see `RingSlot::consumed`.
const RAW_BUF_SIZE: usize = 262_144;

/// Number of slots in the USB ring buffer.
/// At 2 Msps, each slot is 131072 IQ pairs = ~65 ms. 16 slots =
/// ~1.0 s buffer, plenty of headroom for DSP bursts.
const RING_SLOTS: usize = 16;

/// RTL-SDR USB sample rates (Hz).
pub const SAMPLE_RATES: &[f64] = &[
    250_000.0,
    1_024_000.0,
    1_536_000.0,
    1_792_000.0,
    1_920_000.0,
    2_048_000.0,
    2_160_000.0,
    2_400_000.0,
    2_560_000.0,
    2_880_000.0,
    3_200_000.0,
];

// ---------------------------------------------------------------------------
// Ring buffer — lock-free SPSC for USB bulk read data
// ---------------------------------------------------------------------------

/// A single slot in the ring buffer.
///
/// The `Mutex` is never contended: the atomic `state` flag ensures the
/// writer and reader never access the same slot simultaneously.
struct RingSlot {
    data: Mutex<Vec<u8>>,
    len: AtomicUsize,
    /// Bytes consumed by the reader so far within the current fill.
    /// Touched only by the reader thread (single-consumer) — the
    /// atomic is for memory-visibility / `Sync` rather than for
    /// cross-thread coordination. Reset to 0 when the reader
    /// releases the slot (state → 0).
    consumed: AtomicUsize,
    /// 0 = empty (writer can fill), 1 = full (reader can consume).
    state: AtomicU8,
}

/// Lock-free SPSC ring buffer for USB data blocks.
///
/// The writer (USB reader thread) fills empty slots, the reader (DSP
/// thread via `read_samples`) consumes full slots. No copies, no
/// allocations in steady state.
struct UsbRingBuffer {
    slots: Vec<RingSlot>,
    slot_count: usize,
    write_idx: AtomicUsize,
    read_idx: AtomicUsize,
    /// Set to true by the reader thread on fatal USB error or panic.
    error: AtomicBool,
}

impl UsbRingBuffer {
    fn new(slot_count: usize, slot_size: usize) -> Self {
        let slots = (0..slot_count)
            .map(|_| RingSlot {
                data: Mutex::new(vec![0u8; slot_size]),
                len: AtomicUsize::new(0),
                consumed: AtomicUsize::new(0),
                state: AtomicU8::new(0),
            })
            .collect();
        Self {
            slots,
            slot_count,
            write_idx: AtomicUsize::new(0),
            read_idx: AtomicUsize::new(0),
            error: AtomicBool::new(false),
        }
    }
}

// ---------------------------------------------------------------------------
// RtlSdrSource
// ---------------------------------------------------------------------------

/// RTL-SDR IQ source for the pipeline.
///
/// Ports SDR++ `RTLSDRSourceModule`. Opens the RTL-SDR device,
/// configures it, spawns a USB reader thread, and converts uint8 IQ
/// pairs to f32 Complex samples via `read_samples`.
pub struct RtlSdrSource {
    device: Option<RtlSdrDevice>,
    device_index: u32,
    sample_rate: f64,
    frequency: f64,
    running: Arc<AtomicBool>,
    ring: Option<Arc<UsbRingBuffer>>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
}

impl RtlSdrSource {
    /// Create a new RTL-SDR source for the device at the given index.
    pub fn new(device_index: u32) -> Self {
        Self {
            device: None,
            device_index,
            sample_rate: SAMPLE_RATES[7], // 2.4 MHz default
            frequency: 100_000_000.0,     // 100 MHz default
            running: Arc::new(AtomicBool::new(false)),
            ring: None,
            reader_thread: None,
        }
    }

    /// Convert a buffer of raw uint8 IQ pairs to Complex f32 samples.
    ///
    /// Ports the conversion from SDR++ `asyncHandler`:
    /// `re = (buf[i*2] - 127.4) / 128.0; im = (buf[i*2+1] - 127.4) / 128.0`
    pub fn convert_samples(raw: &[u8], output: &mut [Complex]) -> usize {
        let sample_count = raw.len() / 2;
        let count = sample_count.min(output.len());
        for i in 0..count {
            let re = (f32::from(raw[i * 2]) - IQ_OFFSET) / IQ_SCALE;
            let im = (f32::from(raw[i * 2 + 1]) - IQ_OFFSET) / IQ_SCALE;
            output[i] = Complex::new(re, im);
        }
        count
    }
}

impl Source for RtlSdrSource {
    fn name(&self) -> &str {
        "RTL-SDR"
    }

    fn start(&mut self) -> Result<(), SourceError> {
        let mut device = RtlSdrDevice::open(self.device_index)
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        device
            .set_sample_rate(self.sample_rate as u32)
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        device
            .set_center_freq(self.frequency as u32)
            .map_err(|e| SourceError::TuneFailed(e.to_string()))?;

        device
            .reset_buffer()
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        // Belt-and-suspenders: force auto gain mode so the tuner
        // produces signal regardless of whatever state a prior
        // session's deinit left it in. Matches upstream
        // `rtl_test.c` / `rtl_tcp.c` reference programs, which
        // always call `rtlsdr_set_tuner_gain_mode(dev, 0)`
        // immediately after open. Without this, a dongle that
        // was left in an edge-case state (e.g. an app crash mid-
        // session that didn't run the R820T deinit sequence) can
        // come back with the LNA at a manual zero-gain index and
        // stream nothing until the user physically reseats the
        // USB. The UI's `SetAgc` / `SetGain` message flow
        // re-applies the user's actual preferences immediately
        // after the source becomes visible to the controller;
        // this call just guarantees the first few seconds of the
        // session produce data. Per issue #407 (hit during PR
        // #406 smoke test — reseating the dongle was the only
        // workaround at the time).
        device
            .set_tuner_gain_mode(false)
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        // Set running BEFORE spawning so the reader thread sees it immediately.
        self.running.store(true, Ordering::Release);

        // Create the ring buffer and spawn the USB reader thread.
        let ring = Arc::new(UsbRingBuffer::new(RING_SLOTS, RAW_BUF_SIZE));
        let ring_writer = Arc::clone(&ring);
        let cancel = Arc::clone(&self.running);
        let handle = device.usb_handle();

        let thread = std::thread::Builder::new()
            .name("usb-reader".into())
            .spawn(move || {
                tracing::info!("USB reader thread started (ring slots={RING_SLOTS})");
                let timeout = Duration::from_secs(1);

                while cancel.load(Ordering::Acquire) {
                    let idx =
                        ring_writer.write_idx.load(Ordering::Relaxed) % ring_writer.slot_count;
                    let slot = &ring_writer.slots[idx];

                    // Wait for slot to be empty.
                    if slot.state.load(Ordering::Acquire) != 0 {
                        // Ring full — DSP can't keep up. Yield briefly.
                        std::thread::yield_now();
                        continue;
                    }

                    // Lock the slot's buffer for writing. Never contended because
                    // the state flag ensures reader and writer don't overlap.
                    let Ok(mut data) = slot.data.lock() else {
                        tracing::error!("ring slot mutex poisoned");
                        ring_writer.error.store(true, Ordering::Release);
                        break;
                    };

                    match handle.read_bulk(sdr_rtlsdr::constants::BULK_ENDPOINT, &mut data, timeout)
                    {
                        Ok(n) if n > 0 => {
                            slot.len.store(n, Ordering::Relaxed);
                            slot.state.store(1, Ordering::Release); // mark full
                            ring_writer.write_idx.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(_) | Err(rusb::Error::Timeout) => {}
                        Err(e) => {
                            tracing::warn!("USB reader error: {e}");
                            ring_writer.error.store(true, Ordering::Release);
                            break;
                        }
                    }
                }
                tracing::debug!("USB reader thread stopped");
            })
            .map_err(|e| SourceError::OpenFailed(format!("failed to spawn USB reader: {e}")))?;

        self.ring = Some(ring);
        self.reader_thread = Some(thread);
        self.device = Some(device);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
        self.ring = None;
        self.device = None; // Drop closes the device
        Ok(())
    }

    fn tune(&mut self, frequency_hz: f64) -> Result<(), SourceError> {
        self.frequency = frequency_hz;
        if let Some(device) = &mut self.device {
            device
                .set_center_freq(frequency_hz as u32)
                .map_err(|e| SourceError::TuneFailed(e.to_string()))?;
        }
        Ok(())
    }

    fn sample_rates(&self) -> &[f64] {
        SAMPLE_RATES
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SourceError> {
        self.sample_rate = rate;
        if let Some(device) = &mut self.device {
            device
                .set_sample_rate(rate as u32)
                .map_err(|e| SourceError::OpenFailed(e.to_string()))?;
        }
        Ok(())
    }

    fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
        let ring = self.ring.as_ref().ok_or(SourceError::NotRunning)?;
        // Check if the reader thread died (USB error or mutex poisoned)
        if ring.error.load(Ordering::Acquire) {
            return Err(SourceError::ReadFailed(
                "USB reader thread died".to_string(),
            ));
        }
        let idx = ring.read_idx.load(Ordering::Relaxed) % ring.slot_count;
        let slot = &ring.slots[idx];

        if slot.state.load(Ordering::Acquire) != 1 {
            return Ok(0); // No data available yet
        }

        let len = slot.len.load(Ordering::Relaxed);
        let consumed = slot.consumed.load(Ordering::Relaxed);

        // Convert the next chunk of the slot, up to `output.len()`
        // IQ pairs. A slot holds up to `RAW_BUF_SIZE / 2` = 131072
        // IQ pairs but the DSP typically asks for 16384 at a time,
        // so one USB bulk transfer will typically be drained over
        // several `read_samples` calls.
        let count = {
            let data = slot
                .data
                .lock()
                .map_err(|e| SourceError::ReadFailed(e.to_string()))?;
            Self::convert_samples(&data[consumed..len], output)
        };

        // Each IQ pair = 2 raw bytes. Advance the consumed offset.
        let new_consumed = consumed + count * 2;
        if new_consumed >= len {
            // Slot fully drained — release back to the writer.
            slot.consumed.store(0, Ordering::Relaxed);
            slot.state.store(0, Ordering::Release);
            ring.read_idx.fetch_add(1, Ordering::Relaxed);
        } else {
            // Partial consumption — leave the slot owned by the
            // reader. The writer's `state != 0` check in the ring
            // loop will skip it until we release.
            slot.consumed.store(new_consumed, Ordering::Relaxed);
        }

        Ok(count)
    }

    fn set_gain(&mut self, gain_tenths: i32) -> Result<(), SourceError> {
        if let Some(device) = &mut self.device {
            device
                .set_tuner_gain(gain_tenths)
                .map_err(|e| SourceError::OpenFailed(e.to_string()))?;
        }
        Ok(())
    }

    fn set_gain_mode(&mut self, manual: bool) -> Result<(), SourceError> {
        if let Some(device) = &mut self.device {
            device
                .set_tuner_gain_mode(manual)
                .map_err(|e| SourceError::OpenFailed(e.to_string()))?;
        }
        Ok(())
    }

    fn gains(&self) -> &[i32] {
        if let Some(device) = &self.device {
            device.tuner_gains()
        } else {
            &[]
        }
    }

    fn set_ppm_correction(&mut self, ppm: i32) -> Result<(), SourceError> {
        if let Some(device) = &mut self.device {
            device
                .set_freq_correction(ppm)
                .map_err(|e| SourceError::TuneFailed(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_samples() {
        // 127 should give ~-0.003 (near zero), 255 should give ~0.997
        let raw = [127, 127, 255, 0, 0, 255];
        let mut output = [Complex::default(); 3];
        let count = RtlSdrSource::convert_samples(&raw, &mut output);
        assert_eq!(count, 3);

        // Sample 0: (127 - 127.4) / 128 ≈ -0.003125
        assert!((output[0].re - (-0.003_125)).abs() < 0.001);
        assert!((output[0].im - (-0.003_125)).abs() < 0.001);

        // Sample 1: re = (255 - 127.4) / 128 ≈ 0.997
        assert!((output[1].re - 0.997).abs() < 0.01);
        // im = (0 - 127.4) / 128 ≈ -0.995
        assert!((output[1].im - (-0.995)).abs() < 0.01);
    }

    #[test]
    fn test_sample_rates() {
        assert_eq!(SAMPLE_RATES.len(), 11);
        assert!((SAMPLE_RATES[0] - 250_000.0).abs() < 1.0);
        assert!((SAMPLE_RATES[10] - 3_200_000.0).abs() < 1.0);
    }

    #[test]
    fn test_new() {
        let source = RtlSdrSource::new(0);
        assert_eq!(source.name(), "RTL-SDR");
        assert!((source.sample_rate() - 2_400_000.0).abs() < 1.0);
    }
}
