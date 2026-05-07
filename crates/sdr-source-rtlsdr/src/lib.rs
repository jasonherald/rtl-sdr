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
//! RTL-SDR source module — wraps librtlsdr-rs for the pipeline.
//!
//! Owns a USB reader thread and lock-free ring buffer. Converts raw
//! uint8 IQ samples from the USB device to f32 Complex samples for
//! the signal processing pipeline.

use librtlsdr_rs::RtlSdrDevice;
use sdr_pipeline::source_manager::Source;
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
    /// Most-recent tuner-gain value the controller / UI dispatched
    /// at us, in tenths of dB. `None` means nothing has been
    /// dispatched yet — `start()` falls back to the
    /// out-of-the-box default (`FIRST_TIME_TUNER_GAIN_TENTHS_DB`)
    /// in that case so a fresh user with no persisted gain still
    /// gets signal on first Play. Once the UI dispatches its
    /// persisted value (typically right after the source becomes
    /// available), this transitions to `Some(...)` and `start()`
    /// honours that value forever after — fixes the regression
    /// where source-restart paths (e.g. satellite auto-record
    /// after a stop+start cycle) silently overrode the user's
    /// 0 dB choice with the 29.7 dB default and saturated the
    /// front-end on LNA-equipped chains.
    last_tuner_gain_tenths_db: Option<i32>,
}

/// USB reader-thread main loop.
///
/// Drives [`librtlsdr_rs::RtlSdrReader::iter_samples`] forever
/// (until `cancel` flips false or the iterator yields an error),
/// pushing each owned `Vec<u8>` into the lock-free SPSC ring for
/// the DSP thread to consume. Pulled out of the closure inside
/// `RtlSdrSource::start` so the start path stays under clippy's
/// too-many-lines threshold.
fn run_reader_thread(
    reader: librtlsdr_rs::RtlSdrReader,
    ring_writer: &Arc<UsbRingBuffer>,
    cancel: &Arc<AtomicBool>,
) {
    tracing::info!("USB reader thread started (ring slots={RING_SLOTS})");

    // First-buffer stats: sanity check that real USB data is
    // flowing (not all zeros, not all 127) and what its rough
    // amplitude looks like. Periodic heartbeat: confirms the
    // stream stays alive at the expected throughput.
    let mut buffers_seen = 0u32;
    let mut bytes_total: u64 = 0;
    let mut last_stats_log = std::time::Instant::now();

    // Drive `reader.iter_samples(RAW_BUF_SIZE)` — yields owned
    // `Vec<u8>` per USB bulk transfer. One allocation per yield
    // (~15/sec at 2 Msps × 256 KB), negligible at modern
    // allocator speeds. A zero-alloc
    // `iter_samples_into(&mut Vec<u8>)` variant is a future
    // optimisation if the per-yield allocation ever shows up in
    // profiles.
    for chunk in reader.iter_samples(RAW_BUF_SIZE) {
        if !cancel.load(Ordering::Acquire) {
            break;
        }

        let buf = match chunk {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("USB reader error: {e}");
                ring_writer.error.store(true, Ordering::Release);
                break;
            }
        };
        if buf.is_empty() {
            continue;
        }

        buffers_seen = buffers_seen.saturating_add(1);
        bytes_total = bytes_total.saturating_add(buf.len() as u64);

        if buffers_seen == 1 {
            log_buffer_stats(&buf, "first USB buffer received");
        }
        if last_stats_log.elapsed() >= Duration::from_secs(5) {
            let mb = bytes_total as f64 / 1_048_576.0;
            tracing::debug!(
                buffers_seen,
                mb_total = format!("{mb:.2}"),
                "USB reader thread heartbeat"
            );
            // Amplitude stats every 5 sec (info level so they're
            // visible without bumping log verbosity). Lets us
            // see how the IQ-byte distribution changes after
            // bias-T toggles, gain changes, frequency retunes,
            // and during a satellite pass — captures the
            // saturation / quiet-noise / real-signal shapes
            // that the previous "log only the first buffer"
            // approach missed.
            log_buffer_stats(&buf, "periodic USB buffer stats");
            last_stats_log = std::time::Instant::now();
        }

        // Find an empty slot; yield briefly if the ring is full
        // (DSP can't keep up). The pre-iter-call cancel check
        // above bounds worst-case shutdown latency to one
        // in-flight USB read (~65 ms typical, up to one read
        // timeout on stalled hardware).
        let idx = ring_writer.write_idx.load(Ordering::Relaxed) % ring_writer.slot_count;
        let slot = &ring_writer.slots[idx];

        while slot.state.load(Ordering::Acquire) != 0 {
            if !cancel.load(Ordering::Acquire) {
                tracing::debug!("USB reader thread stopping (ring-full wait)");
                return;
            }
            std::thread::yield_now();
        }

        let Ok(mut data) = slot.data.lock() else {
            tracing::error!("ring slot mutex poisoned");
            ring_writer.error.store(true, Ordering::Release);
            break;
        };

        let n = buf.len();
        data[..n].copy_from_slice(&buf);
        drop(data);
        slot.len.store(n, Ordering::Relaxed);
        slot.state.store(1, Ordering::Release);
        ring_writer.write_idx.fetch_add(1, Ordering::Relaxed);
    }
    tracing::debug!("USB reader thread stopped");
}

/// Histogram-style amplitude stats for one USB buffer.
///
/// The reader-thread `log_buffer_stats` calls below are the
/// diagnostic backbone for LNA / saturation / signal-level
/// debugging — three info-level lines per source-start (first
/// buffer + every 5 sec) is the right cadence to spot
/// regressions without spamming the log. Examples of what these
/// stats catch:
///
/// - `mean` significantly off from 127.5 → tuner DC offset (rare)
/// - `frac_at_0` or `frac_at_255` > 1% → ADC clipping / front-
///   end saturation (gain too high)
/// - `std_dev` < 1 → near-zero signal at the antenna (LNA dead,
///   antenna disconnected, SAW filter blocking the band)
/// - `std_dev` 3-10 → healthy noise floor with proper LNA gain
/// - `std_dev` > 30 → strong in-band signal OR full clipping
///
/// Stats are computed in a single pass and returned so the
/// caller can format the log line with a context-specific
/// event name. Per the #626 RtlSdrReader-split smoke test,
/// where the periodic `log_buffer_stats` lines were the
/// definitive proof that bias-T + LNA + 0 dB tuner gain was
/// producing healthy noise (std_dev 4.65, no rail clipping)
/// rather than the saturation we'd suspected from waterfall
/// appearance alone.
struct BufferStats {
    len: usize,
    min: u8,
    max: u8,
    mean: f64,
    std_dev: f64,
    frac_at_0: f64,
    frac_at_255: f64,
}

fn compute_buffer_stats(buf: &[u8]) -> Option<BufferStats> {
    let len = buf.len();
    if len == 0 {
        return None;
    }
    let mut min = 255u8;
    let mut max = 0u8;
    let mut sum: u64 = 0;
    let mut zeros: u64 = 0;
    let mut peaks: u64 = 0;
    for &b in buf {
        if b < min {
            min = b;
        }
        if b > max {
            max = b;
        }
        sum += b as u64;
        if b == 0 {
            zeros += 1;
        }
        if b == 255 {
            peaks += 1;
        }
    }
    let mean = sum as f64 / len as f64;
    let var: f64 = buf
        .iter()
        .map(|&b| {
            let d = b as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / len as f64;
    Some(BufferStats {
        len,
        min,
        max,
        mean,
        std_dev: var.sqrt(),
        frac_at_0: zeros as f64 / len as f64,
        frac_at_255: peaks as f64 / len as f64,
    })
}

/// Log the buffer-stats summary at info level with a caller-
/// supplied event message. Used both for the one-time first-
/// buffer log AND the periodic post-toggle heartbeat so we can
/// see how the IQ amplitude shifts after gain / bias-T /
/// frequency changes. Per the bias-T-saturation diagnosis
/// during the #626 RtlSdrReader-split smoke test.
fn log_buffer_stats(buf: &[u8], event: &'static str) {
    let Some(stats) = compute_buffer_stats(buf) else {
        return;
    };
    tracing::info!(
        len = stats.len,
        min = stats.min,
        max = stats.max,
        mean = format!("{:.2}", stats.mean),
        std_dev = format!("{:.2}", stats.std_dev),
        frac_at_0 = format!("{:.4}", stats.frac_at_0),
        frac_at_255 = format!("{:.4}", stats.frac_at_255),
        event,
    );
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
            last_tuner_gain_tenths_db: None,
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
        // First-time-user fallback gain. R820T supports 29.7 dB
        // exactly (gain-table index 17) — picked as a mid-range
        // value that produces audible signal on broadcast FM
        // without amplifier saturation for the bare-dongle (no
        // LNA) case. Used only when the controller / UI hasn't
        // dispatched a gain yet (`last_tuner_gain_tenths_db ==
        // None`) — once the user's persisted setting flows in,
        // `start()` honours that instead so the LNA-equipped
        // setup the user explicitly configured (e.g. 0 dB tuner
        // + SAW LNA = ~28 dB total) survives source restarts.
        // Per issue #407 + PR #418 smoke test feedback
        // ("AGC off by default") + the LNA-saturation bug found
        // during the #626 RtlSdrReader-split smoke test.
        const FIRST_TIME_TUNER_GAIN_TENTHS_DB: i32 = 297;
        let initial_gain_tenths_db = self
            .last_tuner_gain_tenths_db
            .unwrap_or(FIRST_TIME_TUNER_GAIN_TENTHS_DB);

        // Per-start diagnostic: this single log line lets us reconstruct
        // the source's intent on every fresh open from a session log.
        // Most LNA-related issues we've debugged (saturation, silence,
        // wrong-band noise) come down to a mismatch between what the
        // user thinks the gain / mode is and what the source actually
        // applied. Per #626 RtlSdrReader-split smoke test debugging.
        tracing::info!(
            device_index = self.device_index,
            sample_rate = self.sample_rate,
            frequency_hz = self.frequency,
            initial_gain_tenths_db,
            initial_gain_db = initial_gain_tenths_db as f64 / 10.0,
            last_dispatched_gain = ?self.last_tuner_gain_tenths_db,
            ring_slots = RING_SLOTS,
            buffer_bytes = RAW_BUF_SIZE,
            "RtlSdrSource::start: opening device with config"
        );

        let mut device = RtlSdrDevice::open(self.device_index)
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        // Capture device identity + tuner gain ladder right after
        // open. Logging the gain table tells us which tuner family
        // was probed (R820T vs E4000 vs FC0012/13/2580 vs FC2580
        // each have different step counts), and the USB strings
        // confirm which physical dongle the workflow opened —
        // important when more than one is plugged in or after a
        // hot-plug. Per #626 RtlSdrReader-split smoke test
        // debugging.
        tracing::info!(
            tuner_type = ?device.tuner_type(),
            manufacturer = device.manufacturer(),
            product = device.product(),
            serial = device.serial(),
            gain_table_tenths_db = ?device.tuner_gains(),
            "RtlSdrSource::start: device opened"
        );

        device
            .set_sample_rate(self.sample_rate as u32)
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        device
            .set_center_freq(self.frequency as u32)
            .map_err(|e| SourceError::TuneFailed(e.to_string()))?;

        device
            .reset_buffer()
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        // Belt-and-suspenders: explicitly put the tuner into a
        // known manual-gain state so the first Play produces
        // signal regardless of whatever state a prior session
        // left the device in. Pre-#407 no post-open gain setup
        // ran at all, which let a USB-reseat-needing edge case
        // slip through (dongle left in a bad state streamed
        // zero bytes until physically reseated — seen during
        // the PR #406 smoke test).
        //
        // **Gain mode: manual (AGC off) by default.** User
        // preference is AGC off — mirrors SDR++ / GQRX's
        // default for scanner / FM reception where a fixed gain
        // is easier to reason about than an auto-ranging loop.
        // The UI's `SetAgc(true)` dispatch re-enables auto mode
        // immediately after the source is visible to the
        // controller, so users who save "AGC on" still get
        // their saved preference within one controller tick.
        //
        // **Gain value: mid-range default.** `set_gain_mode(true)`
        // writes LNA-auto-off + mixer-auto-off + VGA 16.3 dB to
        // the R820T regs, leaving the LNA and mixer at whatever
        // index the `R82XX_INIT_ARRAY` post-init sequence left
        // behind (LNA index 3 is common — low but non-zero).
        // Explicitly set a mid-range tuner gain (29.7 dB, index
        // 17 of 29 for R820T) on top of that so fresh-install
        // users hear signal on the first Play without having to
        // touch the gain slider. UI `SetGain` dispatch overrides
        // this with the saved preference a moment later.
        //
        // Per issue #407 + user feedback on PR #418 smoke test
        // ("AGC should default to off").
        device
            .set_tuner_gain_mode(true)
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;
        if let Err(e) = device.set_tuner_gain(initial_gain_tenths_db) {
            // Non-fatal: the gain-mode write above already put
            // the tuner in a valid manual state. If the
            // mid-range default fails (unexpected tuner
            // variant / I2C flake), log and carry on — the UI's
            // `SetGain` dispatch takes over on the next
            // controller tick.
            tracing::warn!(
                error = %e,
                "RtlSdrSource::start: post-open set_tuner_gain default failed (non-fatal)"
            );
        }

        // Set running BEFORE spawning so the reader thread sees it immediately.
        self.running.store(true, Ordering::Release);

        // Create the ring buffer and spawn the USB reader thread.
        // The reader uses sdr-rtlsdr's `RtlSdrReader` —
        // a streaming-focused handle acquired cheaply from the
        // device, holding its own `Arc<DeviceHandle>` clone — so
        // the parent thread retains `self.device = Some(device)`
        // for control methods (`set_center_freq`, etc.) that the
        // satellite auto-record + UI tune both call mid-stream
        // without restarting the source. Per #626 round 4
        // (RtlSdrReader split).
        let ring = Arc::new(UsbRingBuffer::new(RING_SLOTS, RAW_BUF_SIZE));
        let ring_writer = Arc::clone(&ring);
        let cancel = Arc::clone(&self.running);
        let reader = device.reader();

        let thread = std::thread::Builder::new()
            .name("usb-reader".into())
            .spawn(move || run_reader_thread(reader, &ring_writer, &cancel))
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
        // Remember the dispatched value EVEN IF the device isn't
        // currently open, so a later `start()` call (e.g. user
        // clicked Play after dispatching gain at app launch, or
        // satellite auto-record restarted the source) reapplies
        // the user's choice rather than the first-time default.
        // Per the regression fix in the #626 RtlSdrReader-split
        // smoke test where a 0 dB user setting was silently
        // overridden by 29.7 dB on every start, saturating the
        // front-end on LNA-equipped chains.
        // Diagnostic info-level log: gain dispatches are
        // user-paced (UI slider drag, persisted-settings replay
        // on source open, satellite auto-record paths) so
        // logging each one at info doesn't add meaningful noise
        // and is invaluable when debugging
        // saturation / silent-recording issues. The
        // `device_open` field disambiguates "dispatched and
        // applied to hardware" from "dispatched but stored in
        // `last_tuner_gain_tenths_db` for the next open" —
        // critical for the LNA-saturation-debug workflow that
        // motivated this log. Per #626 RtlSdrReader-split smoke
        // test debugging.
        let device_open = self.device.is_some();
        tracing::info!(
            gain_tenths_db = gain_tenths,
            gain_db = gain_tenths as f64 / 10.0,
            device_open,
            "RtlSdrSource::set_gain dispatch"
        );
        self.last_tuner_gain_tenths_db = Some(gain_tenths);
        if let Some(device) = &mut self.device {
            device
                .set_tuner_gain(gain_tenths)
                .map_err(|e| SourceError::OpenFailed(e.to_string()))?;
        }
        Ok(())
    }

    fn set_gain_mode(&mut self, manual: bool) -> Result<(), SourceError> {
        // Diagnostic info-level log — user-paced (fires only on
        // a UI AGC-toggle flip or persisted-settings replay),
        // so logging each one at info doesn't add meaningful
        // noise. Pairs with `set_gain dispatch`: when AGC is
        // on the manual gain is silently ignored by librtlsdr,
        // which is a known class of bug — having both events
        // on the same log timeline makes that diagnosis
        // straightforward. Per #626 smoke test.
        let device_open = self.device.is_some();
        tracing::info!(manual, device_open, "RtlSdrSource::set_gain_mode dispatch");
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

    fn set_bias_tee(&mut self, enabled: bool) -> Result<(), SourceError> {
        // Routes through `rtlsdr_set_bias_tee` (GPIO 0). Older
        // V3-clone dongles lack the bias-T circuit entirely; the
        // driver returns Err on those — surfaced as a
        // `TuneFailed` toast rather than crashing. Per issue
        // #537. The Source-trait default is a silent no-op so
        // every other source type (file, network) ignores the
        // command — only the live RTL-SDR USB path actually
        // toggles hardware.
        // Diagnostic info-level log — user-paced (UI bias-T
        // checkbox flip), so one info line per toggle is fine.
        // Critical for LNA-debug workflows where the
        // observable state of the dongle (waterfall noise floor,
        // periodic-buffer-stats std_dev) only makes sense in the
        // context of the bias-T timeline. Per #626 smoke test
        // where bias-T off → std_dev 0.48, bias-T on → std_dev
        // 4.65 was THE smoking-gun confirmation that the LNA was
        // wired correctly.
        let device_open = self.device.is_some();
        tracing::info!(enabled, device_open, "RtlSdrSource::set_bias_tee dispatch");
        if let Some(device) = &mut self.device {
            device
                .set_bias_tee(enabled)
                .map_err(|e| SourceError::TuneFailed(e.to_string()))?;
        }
        Ok(())
    }

    fn set_direct_sampling(&mut self, mode: i32) -> Result<(), SourceError> {
        // Routes through `rtlsdr_set_direct_sampling`. Mode 0
        // disables direct sampling (normal tuner path); 1 selects
        // the I branch and 2 selects the Q branch — both bypass
        // the tuner entirely and feed the ADC straight from the
        // antenna input, which is how RTL-SDR Blog v3+ dongles
        // tune below 28 MHz (the R820T tuner cuts off there).
        // Most users want Q branch on a v3 dongle. Per issue
        // #538.
        //
        // Defense-in-depth boundary check: the UI handler in
        // `connect_source_panel` already validates the combo
        // index against `DIRECT_SAMPLING_MAX_IDX` and the
        // persistence loader range-clamps before dispatch, but
        // any future caller (FFI consumer, scripted DSP test,
        // etc.) could still wire a malformed `mode` here. Reject
        // out-of-range values with a clear error rather than
        // forwarding to the driver, which would either silently
        // misbehave or surface a confusing low-level error. Per
        // `CodeRabbit` round 1 on PR #559.
        if !(0..=2).contains(&mode) {
            return Err(SourceError::TuneFailed(format!(
                "invalid direct sampling mode: {mode} (expected 0..=2)"
            )));
        }
        if let Some(device) = &mut self.device {
            device
                .set_direct_sampling(mode)
                .map_err(|e| SourceError::TuneFailed(e.to_string()))?;
        }
        Ok(())
    }

    fn set_offset_tuning(&mut self, enabled: bool) -> Result<(), SourceError> {
        // Routes through `rtlsdr_set_offset_tuning`. Pushes the
        // local oscillator off the tuned frequency so the DC
        // spike that lives at the LO doesn't sit on top of the
        // signal of interest. Most relevant on E4000 tuners; on
        // R820T / R828D the driver in
        // `crates/sdr-rtlsdr/src/device/frequency.rs` returns
        // `InvalidParameter` ("offset tuning not supported for
        // R82XX tuners"), and the call is also rejected while
        // direct sampling is enabled. We surface either rejection
        // as a `TuneFailed` toast rather than crashing — the user
        // sees a clear "your tuner doesn't support this" message
        // instead of a no-op. Per issue #539 + `CodeRabbit`
        // round 1 on PR #559.
        if let Some(device) = &mut self.device {
            device
                .set_offset_tuning(enabled)
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
