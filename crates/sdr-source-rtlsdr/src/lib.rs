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

use librtlsdr_rs::{RtlSdrDevice, TunerType};
use sdr_pipeline::source_manager::Source;
use sdr_types::{Complex, SourceError};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Lowest frequency the R820T / R828D tuner PLL can reach through
/// the normal tuner path. Below this the VCO (1770–3900 MHz) has
/// no integer divider ≤ 64 that lands on the requested LO, and
/// `librtlsdr-rs` returns `PllProgrammingFailed` rather than the C
/// reference's "PLL not locked" warning + noise. HF reception below
/// this floor needs the RTL2832's direct-sampling path (Q branch on
/// RTL-SDR Blog v3+ dongles), which bypasses the tuner entirely.
/// Matches the 24 MHz lower bound librtlsdr documents for R820T.
pub const R82XX_MIN_TUNER_FREQ_HZ: f64 = 24_000_000.0;

/// Direct-sampling mode value meaning "off — normal tuner path".
const DIRECT_SAMPLING_OFF: i32 = 0;

/// Hz → MHz divisor for user-facing frequency text.
const HERTZ_PER_MHZ: f64 = 1_000_000.0;

/// How many consecutive "soft" bulk-read results (USB timeout or a
/// zero-length transfer) the reader tolerates before declaring the
/// device gone. One read is bounded by the driver's bulk timeout
/// (≈ 5 s), so this is roughly a minute of silence — long enough to
/// ride out host suspend/resume, bus contention from a second dongle
/// or rtl_tcp server, and USB autosuspend blips (the PR #406 "bad state
/// until reseat" case), short enough that a genuinely dead dongle is
/// reported instead of a frozen waterfall. Per #740.
pub const MAX_CONSECUTIVE_SOFT_READ_FAILURES: u32 = 12;

/// Per-reader budget of consecutive soft read failures.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReadRetryBudget {
    consecutive_soft_failures: u32,
}

/// What the reader loop should do with one bulk-read result.
#[derive(Debug)]
pub enum ReadOutcome {
    /// `n` bytes of IQ data (always even — whole pairs) are in the buffer.
    Data(usize),
    /// Transient: nothing usable this time, read again.
    Retry,
    /// Give up and flag the ring; the message is logged.
    Fatal(String),
}

/// Classify a bulk-read result. Pure so the retry policy is unit-testable
/// without hardware.
///
/// * `Ok(n)` with `n >= 2`: data. Odd byte counts are trimmed to whole
///   IQ pairs — a slot whose length is odd could never be fully drained
///   by the pair-wise reader and stalled the ring forever.
/// * `Ok(0)` / `Ok(1)` and USB timeouts: soft — retry up to
///   [`MAX_CONSECUTIVE_SOFT_READ_FAILURES`] in a row.
/// * Anything else (device lost, pipe/overflow, tuner errors): fatal.
pub fn classify_read(
    result: Result<usize, librtlsdr_rs::RtlSdrError>,
    budget: &mut ReadRetryBudget,
) -> ReadOutcome {
    let soft = |budget: &mut ReadRetryBudget, what: &str| {
        budget.consecutive_soft_failures += 1;
        if budget.consecutive_soft_failures > MAX_CONSECUTIVE_SOFT_READ_FAILURES {
            ReadOutcome::Fatal(format!(
                "{what} for {} consecutive bulk reads — giving up",
                budget.consecutive_soft_failures
            ))
        } else {
            ReadOutcome::Retry
        }
    };
    match result {
        Ok(n) if n >= 2 => {
            budget.consecutive_soft_failures = 0;
            ReadOutcome::Data(n & !1)
        }
        Ok(_) => soft(budget, "zero-length USB transfer"),
        Err(librtlsdr_rs::RtlSdrError::Usb(rusb::Error::Timeout)) => {
            soft(budget, "USB read timeout")
        }
        Err(e) => ReadOutcome::Fatal(format!("USB reader error: {e}")),
    }
}

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

/// How long the USB reader sleeps between checks while the ring is
/// full (DSP behind). Bounded sleep rather than `yield_now()`: a yield
/// returns immediately when nothing else is runnable, so the reader
/// would spin a core and starve the very DSP thread it is waiting on.
/// 100 µs is well under one USB bulk transfer (~65 ms at 2 MSPS).
const RING_FULL_BACKOFF: Duration = Duration::from_micros(100);

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
    /// Most-recent tuner gain mode dispatched (`Some(true)` = manual).
    /// Remembered so it can be re-applied after a direct-sampling
    /// off-transition re-runs the tuner init array (#741) and so a
    /// pre-start dispatch survives into `start()`.
    last_gain_manual: Option<bool>,
    /// RTL2832 digital AGC state dispatched by the controller. Replayed
    /// on every open; previously silently dropped by the trait default
    /// (#739).
    rtl_agc_enabled: bool,
    /// Most-recent direct-sampling mode the controller dispatched
    /// (0 = off, 1 = I branch, 2 = Q branch). Remembered across
    /// stop/start so `start()` can program it *before* the first
    /// `set_center_freq` — the controller's post-`start()` replay
    /// runs too late for HF: an R820T tune below
    /// `R82XX_MIN_TUNER_FREQ_HZ` fails outright, so the replay was
    /// never reached and Q-branch users could not Play below 24 MHz.
    direct_sampling_mode: i32,
}

/// USB reader-thread main loop.
///
/// Issues one [`librtlsdr_rs::RtlSdrReader::read_sync`] per ring slot,
/// straight into the slot's buffer (no per-transfer allocation or
/// copy), until `cancel` flips false or [`classify_read`] reports a fatal
/// result. Transient timeouts / empty transfers are retried within
/// [`MAX_CONSECUTIVE_SOFT_READ_FAILURES`]; the old iterator fused on the
/// first `Ok(0)` or `Err` and left the ring reporting "no data" forever
/// (#740). Pulled out of the closure inside `RtlSdrSource::start` so the
/// start path stays under clippy's too-many-lines threshold.
fn run_reader_thread(
    reader: &librtlsdr_rs::RtlSdrReader,
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
    let mut retry_budget = ReadRetryBudget::default();

    while cancel.load(Ordering::Acquire) {
        // Find an empty slot; back off briefly if the ring is full
        // (DSP can't keep up). Checking cancel here bounds
        // worst-case shutdown latency to one in-flight USB read
        // (~65 ms typical, up to one read timeout on stalled
        // hardware).
        let idx = ring_writer.write_idx.load(Ordering::Relaxed) % ring_writer.slot_count;
        let slot = &ring_writer.slots[idx];

        while slot.state.load(Ordering::Acquire) != 0 {
            if !cancel.load(Ordering::Acquire) {
                tracing::debug!("USB reader thread stopping (ring-full wait)");
                return;
            }
            std::thread::sleep(RING_FULL_BACKOFF);
        }

        let Ok(mut data) = slot.data.lock() else {
            tracing::error!("ring slot mutex poisoned");
            ring_writer.error.store(true, Ordering::Release);
            break;
        };

        // Read directly into the slot.
        let n = match classify_read(
            reader.read_sync(&mut data[..RAW_BUF_SIZE]),
            &mut retry_budget,
        ) {
            ReadOutcome::Data(n) => n,
            ReadOutcome::Retry => {
                drop(data);
                continue;
            }
            ReadOutcome::Fatal(msg) => {
                tracing::warn!("{msg}");
                ring_writer.error.store(true, Ordering::Release);
                break;
            }
        };

        buffers_seen = buffers_seen.saturating_add(1);
        bytes_total = bytes_total.saturating_add(n as u64);
        if buffers_seen == 1 {
            log_buffer_stats(&data[..n], "first USB buffer received");
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
            log_buffer_stats(&data[..n], "periodic USB buffer stats");
            last_stats_log = std::time::Instant::now();
        }

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

/// Apply a bias-T toggle to a dongle that isn't currently being
/// streamed by an active [`RtlSdrSource`]. Briefly opens the device
/// at `device_index`, calls `set_bias_tee(enabled)`, and drops the
/// handle — releasing it back to other consumers (or to the next
/// `start()` call).
///
/// **When to call this.** The streaming path on a live
/// [`RtlSdrSource`] is `RtlSdrSource::set_bias_tee`; this helper
/// covers the **paused / pre-play** case where there is no source
/// instance and therefore no held device handle. Without it, a user
/// flipping bias-T off between sessions would leave their SAWbird+
/// (or other LNA powered by the bias-T supply) in whatever state the
/// last play session set it to — see #652.
///
/// **Bias-T state is sticky across device close.** RTL-SDR Blog v3
/// dongles latch the GPIO output until the dongle is power-cycled
/// or the GPIO is explicitly toggled. So setting the line here
/// persists past the device drop — the SAWbird+ stays on (or off)
/// after this returns. The next `RtlSdrSource::start()` will
/// re-apply the same value via `rtl_sdr_replay_persisted_settings`,
/// keeping behavior consistent across the pause boundary.
///
/// # Errors
///
/// Returns [`SourceError::TuneFailed`] if the device can't be
/// opened (busy / not present / older V3 clone without bias-T
/// circuitry — driver returns Err on those, same as the live
/// path).
pub fn apply_bias_tee_idle(device_index: u32, enabled: bool) -> Result<(), SourceError> {
    tracing::info!(
        device_index,
        enabled,
        "apply_bias_tee_idle: brief open + set_bias_tee + drop"
    );
    let device = RtlSdrDevice::open(device_index)
        .map_err(|e| SourceError::TuneFailed(format!("open for bias-T: {e}")))?;
    device
        .set_bias_tee(enabled)
        .map_err(|e| SourceError::TuneFailed(format!("set_bias_tee: {e}")))?;
    // Device drops at end of scope, releasing the USB handle.
    Ok(())
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
            last_gain_manual: None,
            rtl_agc_enabled: false,
            direct_sampling_mode: DIRECT_SAMPLING_OFF,
        }
    }

    /// Build the user-facing `TuneFailed` message for a failed
    /// `set_center_freq`.
    ///
    /// Pure so it is unit-testable without hardware. When the
    /// request is below the R82xx tuner floor and direct sampling
    /// is off, the raw driver error ("PLL programming failed … no
    /// valid VCO divider") is replaced with an actionable hint
    /// pointing at the Source panel's Direct Sampling combo.
    /// Otherwise the driver text passes through unchanged.
    fn tune_failure_message(
        tuner: TunerType,
        direct_sampling_mode: i32,
        frequency_hz: f64,
        driver_error: &str,
    ) -> String {
        let r82xx = matches!(tuner, TunerType::R820T | TunerType::R828D);
        if r82xx
            && direct_sampling_mode == DIRECT_SAMPLING_OFF
            && frequency_hz < R82XX_MIN_TUNER_FREQ_HZ
        {
            format!(
                "{:.3} MHz is below the {tuner:?} tuner's {:.0} MHz floor. \
                 For HF, set Direct Sampling to \"Q branch\" in the Source panel \
                 (driver: {driver_error})",
                frequency_hz / HERTZ_PER_MHZ,
                R82XX_MIN_TUNER_FREQ_HZ / HERTZ_PER_MHZ,
            )
        } else {
            driver_error.to_string()
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

        // Program direct sampling BEFORE the first tune, mirroring
        // rtl_fm's call order. `rtlsdr_set_center_freq` branches on
        // the direct-sampling flag, so an HF frequency with Q branch
        // selected is only tunable once the mode is in place. The
        // controller's post-`start()` replay still runs (harmless
        // no-op re-write) but is too late to save this first tune.
        if self.direct_sampling_mode != DIRECT_SAMPLING_OFF {
            device
                .set_direct_sampling(self.direct_sampling_mode)
                .map_err(|e| SourceError::OpenFailed(e.to_string()))?;
        }

        let tuner = device.tuner_type();
        let direct_sampling_mode = self.direct_sampling_mode;
        let frequency_hz = self.frequency;
        device.set_center_freq(self.frequency as u32).map_err(|e| {
            SourceError::TuneFailed(Self::tune_failure_message(
                tuner,
                direct_sampling_mode,
                frequency_hz,
                &e.to_string(),
            ))
        })?;

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
        // Gain mode follows the last dispatch when there was one
        // (pre-start dispatch or a previous open); manual otherwise.
        device
            .set_tuner_gain_mode(self.last_gain_manual.unwrap_or(true))
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;
        // Replay both states: the RTL2832 keeps digital AGC across
        // handles, so a previous session leaving it on must be undone.
        if let Err(e) = device.set_agc_mode(self.rtl_agc_enabled) {
            tracing::warn!(
                enabled = self.rtl_agc_enabled,
                error = %e,
                "RTL AGC restore on open failed"
            );
        }
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
            .spawn(move || run_reader_thread(&reader, &ring_writer, &cancel))
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
            let tuner = device.tuner_type();
            let direct_sampling_mode = self.direct_sampling_mode;
            device.set_center_freq(frequency_hz as u32).map_err(|e| {
                SourceError::TuneFailed(Self::tune_failure_message(
                    tuner,
                    direct_sampling_mode,
                    frequency_hz,
                    &e.to_string(),
                ))
            })?;
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
                .map_err(|e| SourceError::InvalidParameter(e.to_string()))?;
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
        self.last_gain_manual = Some(manual);
        if let Some(device) = &mut self.device {
            device
                .set_tuner_gain_mode(manual)
                .map_err(|e| SourceError::InvalidParameter(e.to_string()))?;
        }
        Ok(())
    }

    fn set_rtl_agc(&mut self, enabled: bool) -> Result<(), SourceError> {
        // RTL2832 digital AGC (distinct from the tuner's LNA/mixer/VGA
        // AGC). Remembered for replay on open; forwarded live. Was a
        // trait-default no-op on the USB source while the UI, the
        // controller replay and the FFI all dispatched it (#739).
        let device_open = self.device.is_some();
        tracing::info!(enabled, device_open, "RtlSdrSource::set_rtl_agc dispatch");
        self.rtl_agc_enabled = enabled;
        if let Some(device) = &self.device {
            device
                .set_agc_mode(enabled)
                .map_err(|e| SourceError::InvalidParameter(e.to_string()))?;
        }
        Ok(())
    }

    fn set_gain_by_index(&mut self, index: u32) -> Result<(), SourceError> {
        // Resolve the index against the tuner's gain table and route
        // through `set_gain` so the value is remembered like any other
        // gain dispatch. Was a trait-default no-op on the USB source
        // (#739).
        let table = self.gains();
        let Some(&gain_tenths) = usize::try_from(index).ok().and_then(|i| table.get(i)) else {
            return Err(SourceError::InvalidParameter(format!(
                "gain index {index} out of range (table has {} entries)",
                table.len()
            )));
        };
        self.set_gain(gain_tenths)
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
        let was_off = self.direct_sampling_mode == DIRECT_SAMPLING_OFF;
        let Some(device) = &mut self.device else {
            // Remember even with no open device so the next `start()`
            // programs it ahead of the first tune.
            self.direct_sampling_mode = mode;
            return Ok(());
        };
        device
            .set_direct_sampling(mode)
            .map_err(|e| SourceError::TuneFailed(e.to_string()))?;
        // Leaving direct sampling re-runs the tuner init array
        // (librtlsdr `set_direct_sampling(0)` → `tuner.init()`),
        // which overwrites the LNA/mixer/VGA gain registers the
        // user's gain mode / gain had programmed. Re-apply them
        // so a 0 dB + LNA chain doesn't come back saturated (#741).
        if mode == DIRECT_SAMPLING_OFF && !was_off {
            if let Some(manual) = self.last_gain_manual {
                device
                    .set_tuner_gain_mode(manual)
                    .map_err(|e| SourceError::InvalidParameter(e.to_string()))?;
            }
            if let Some(gain) = self.last_tuner_gain_tenths_db {
                device
                    .set_tuner_gain(gain)
                    .map_err(|e| SourceError::InvalidParameter(e.to_string()))?;
            }
        }
        // Persist only after the device accepted the mode AND the gain
        // restoration completed, so a later `start()` / `tune()` never
        // replays a rejected setting and a failed restoration stays
        // retryable (`was_off` is still false on the next mode-0 call).
        self.direct_sampling_mode = mode;
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
        assert_eq!(source.direct_sampling_mode, DIRECT_SAMPLING_OFF);
    }

    #[test]
    fn set_direct_sampling_is_remembered_without_open_device() {
        let mut source = RtlSdrSource::new(0);
        source.set_direct_sampling(2).expect("valid mode");
        assert_eq!(source.direct_sampling_mode, 2);
        assert!(source.set_direct_sampling(3).is_err());
        assert_eq!(
            source.direct_sampling_mode, 2,
            "invalid mode must not overwrite"
        );
    }

    /// #739 — RTL AGC and gain-by-index are real operations on the USB
    /// dongle, not trait-default no-ops. Without a device they are
    /// remembered / validated; with one they reach the hardware.
    #[test]
    fn set_rtl_agc_is_remembered_without_a_device() {
        let mut source = RtlSdrSource::new(0);
        assert!(!source.rtl_agc_enabled);
        source.set_rtl_agc(true).expect("stored");
        assert!(source.rtl_agc_enabled);
    }

    #[test]
    fn set_gain_by_index_rejects_out_of_range() {
        let mut source = RtlSdrSource::new(0);
        // No device → no gain table → any index is out of range.
        assert!(matches!(
            source.set_gain_by_index(0),
            Err(SourceError::InvalidParameter(_))
        ));
    }

    #[test]
    fn set_gain_mode_is_remembered_without_a_device() {
        let mut source = RtlSdrSource::new(0);
        assert_eq!(source.last_gain_manual, None);
        source.set_gain_mode(false).expect("stored");
        assert_eq!(source.last_gain_manual, Some(false));
    }

    /// #740 — the reader must tolerate a bounded run of timeouts /
    /// zero-length transfers (host suspend, bus contention, USB
    /// autosuspend) and only give up on real device loss.
    #[test]
    fn classify_read_tolerates_bounded_timeouts_and_empty_reads() {
        let mut retries = ReadRetryBudget::default();
        let timeout = || Err::<usize, _>(librtlsdr_rs::RtlSdrError::Usb(rusb::Error::Timeout));
        for _ in 0..MAX_CONSECUTIVE_SOFT_READ_FAILURES {
            assert!(matches!(
                classify_read(timeout(), &mut retries),
                ReadOutcome::Retry
            ));
        }
        assert!(matches!(
            classify_read(timeout(), &mut retries),
            ReadOutcome::Fatal(_)
        ));

        let mut retries = ReadRetryBudget::default();
        for _ in 0..MAX_CONSECUTIVE_SOFT_READ_FAILURES {
            assert!(matches!(
                classify_read(Ok(0), &mut retries),
                ReadOutcome::Retry
            ));
        }
        assert!(matches!(
            classify_read(Ok(0), &mut retries),
            ReadOutcome::Fatal(_)
        ));
    }

    #[test]
    fn classify_read_data_resets_the_retry_budget_and_masks_odd_lengths() {
        let mut retries = ReadRetryBudget::default();
        let timeout = || Err::<usize, _>(librtlsdr_rs::RtlSdrError::Usb(rusb::Error::Timeout));
        for _ in 0..MAX_CONSECUTIVE_SOFT_READ_FAILURES {
            classify_read(timeout(), &mut retries);
        }
        // A successful transfer clears the budget…
        assert!(matches!(
            classify_read(Ok(4096), &mut retries),
            ReadOutcome::Data(4096)
        ));
        assert!(matches!(
            classify_read(timeout(), &mut retries),
            ReadOutcome::Retry
        ));
        // …and an odd byte count is trimmed to whole IQ pairs so the ring
        // slot can always be fully drained.
        assert!(matches!(
            classify_read(Ok(4097), &mut retries),
            ReadOutcome::Data(4096)
        ));
        // A lone odd byte is not data.
        assert!(matches!(
            classify_read(Ok(1), &mut retries),
            ReadOutcome::Retry
        ));
    }

    #[test]
    fn classify_read_device_loss_is_fatal_immediately() {
        let mut retries = ReadRetryBudget::default();
        assert!(matches!(
            classify_read(
                Err(librtlsdr_rs::RtlSdrError::Usb(rusb::Error::NoDevice)),
                &mut retries
            ),
            ReadOutcome::Fatal(_)
        ));
    }

    #[test]
    fn hf_tune_failure_on_r820t_hints_at_direct_sampling() {
        let msg = RtlSdrSource::tune_failure_message(
            TunerType::R820T,
            DIRECT_SAMPLING_OFF,
            4_800_000.0,
            "R82xx: PLL programming failed for 6425000 Hz (no valid VCO divider)",
        );
        assert!(msg.contains("4.800 MHz"), "{msg}");
        assert!(msg.contains("24 MHz floor"), "{msg}");
        assert!(msg.contains("Direct Sampling"), "{msg}");
        assert!(
            msg.contains("no valid VCO divider"),
            "driver detail kept: {msg}"
        );
    }

    #[test]
    fn tune_failure_passthrough_when_hint_does_not_apply() {
        let raw = "some driver error";
        // Already in direct sampling — tuner floor is irrelevant.
        assert_eq!(
            RtlSdrSource::tune_failure_message(TunerType::R820T, 2, 4_800_000.0, raw),
            raw
        );
        // Above the floor — a different failure, don't mislead.
        assert_eq!(
            RtlSdrSource::tune_failure_message(TunerType::R820T, 0, 100_000_000.0, raw),
            raw
        );
        // Non-R82xx tuner — floor constant doesn't apply.
        assert_eq!(
            RtlSdrSource::tune_failure_message(TunerType::E4000, 0, 4_800_000.0, raw),
            raw
        );
    }
}
