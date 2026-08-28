#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "f64→u32 Hz casts are bounded: rates come from the firmware table \
              snapshot and frequencies from the tuner's 24 MHz–1.8 GHz range"
)]
//! Airspy R2 / Mini source module — wraps `libairspy-rs` for the
//! pipeline.
//!
//! Unlike the RTL-SDR source, no bespoke USB reader thread is needed:
//! [`libairspy_rs::Device::start_rx`] owns the bulk-transfer reader and
//! consumer threads and delivers converted sample blocks through a
//! callback, and every control request (`set_freq`, gains, bias-T)
//! takes `&self` and is safe mid-stream. The callback here bridges
//! each block into a bounded channel; [`Source::read_samples`] drains
//! it on the DSP thread. When the DSP falls behind, blocks are
//! dropped at the bridge (counted and logged) rather than stalling
//! the driver's consumer thread — the driver additionally reports its
//! own ring drops per transfer via `dropped_samples`.
//!
//! The device streams `Float32Iq`, so a delivered block is already
//! interleaved `[i0, q0, i1, q1, ..]` f32 — [`convert_samples`] is a
//! straight pairing into [`Complex`] with no offset/scale step.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

use libairspy_rs::commands::SampleType;
use libairspy_rs::conversion::Samples;
use libairspy_rs::device::Device;
use sdr_pipeline::source_manager::Source;
use sdr_types::{Complex, SourceError};

/// Composite linearity-gain step count (`airspy_set_linearity_gain`
/// accepts 0–21).
pub const LINEARITY_GAIN_STEPS: u8 = 22;

/// The linearity gain ladder presented through [`Source::gains`].
///
/// The `Source` trait models RTL-style "tenths of dB" gain tables,
/// but Airspy's canonical control is the unitless 0–21 composite
/// linearity index (each step programs a firmware-tuned LNA / mixer /
/// VGA triple; upstream publishes no calibrated dB value per step).
/// Encode step *N* as `N × 10` "tenths" so the existing gain plumbing
/// (dispatch, persistence, `GainList`) round-trips the step exactly
/// and the UI slider shows 0–21 in unit steps. Per issue #848.
pub const LINEARITY_GAIN_TENTHS: [i32; LINEARITY_GAIN_STEPS as usize] = [
    0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160, 170, 180, 190, 200,
    210,
];

/// First-time-user fallback linearity step. Mid-ladder — enough gain
/// to hear broadcast FM on a bare whip without saturating an
/// LNA-equipped chain; the UI's persisted `SetGain` dispatch replaces
/// it within one controller tick (same contract as the RTL source's
/// first-time 29.7 dB default).
pub const FIRST_TIME_LINEARITY_STEP: u8 = 14;

/// Fallback rate list before a device has been opened (matches the
/// driver's own firmware-query fallback for the R2). Replaced by the
/// device-reported list at `start()`.
pub const DEFAULT_SAMPLE_RATES: &[f64] = &[2_500_000.0, 10_000_000.0];

/// Bound on the block channel between the driver's consumer thread
/// and the DSP reader. The driver delivers ~16 blocks/s at 10 Msps
/// (65536-sample transfers), so 16 blocks ≈ 1 s of buffered audio —
/// the same headroom the RTL ring provides.
const BLOCK_CHANNEL_BOUND: usize = 16;

/// Hz → MHz divisor for user-facing frequency text.
const HERTZ_PER_MHZ: f64 = 1_000_000.0;

/// Airspy `Float32Iq` fullscale reciprocal. The driver's float
/// output is `int12 / 4096` (C `SAMPLE_SCALE = 1/(1 << (15 -
/// SAMPLE_SHIFT))`), so a rail-to-rail ADC swing spans ±0.5 —
/// measured empirically against `airspy_rx -t 0` captures during the
/// #848 bring-up. The pipeline's fullscale convention is ±1.0 (RTL's
/// `(u8 − 127.4)/128`), so scale by 2 at conversion; without this the
/// input sits 6 dB low and every display/squelch expectation shifts.
pub const FLOAT32_FULLSCALE_SCALE: f32 = 2.0;

/// Convert one interleaved `Float32Iq` block into [`Complex`]
/// samples, rescaled to the pipeline's ±1.0 fullscale convention
/// (see [`FLOAT32_FULLSCALE_SCALE`]). Returns the number of complex
/// samples written (bounded by both the block and `output`).
pub fn convert_samples(raw: &[f32], output: &mut [Complex]) -> usize {
    let count = (raw.len() / 2).min(output.len());
    for (i, out) in output.iter_mut().take(count).enumerate() {
        *out = Complex::new(
            raw[i * 2] * FLOAT32_FULLSCALE_SCALE,
            raw[i * 2 + 1] * FLOAT32_FULLSCALE_SCALE,
        );
    }
    count
}

/// Map a trait-side gain dispatch (tenths, per
/// [`LINEARITY_GAIN_TENTHS`]) back to a linearity step, clamped to
/// the ladder.
pub fn linearity_step_from_tenths(gain_tenths: i32) -> u8 {
    let step = gain_tenths / 10;
    u8::try_from(step.clamp(0, i32::from(LINEARITY_GAIN_STEPS - 1))).unwrap_or(0)
}

/// Pick the supported rate closest to `requested`. The UI's persisted
/// rate may predate the device snapshot (or belong to the RTL rate
/// table until the Source-panel work in issue #848 phase 3 lands), so
/// a mismatch clamps with a warning instead of failing `start()`.
pub fn nearest_supported_rate(supported: &[f64], requested: f64) -> Option<f64> {
    supported
        .iter()
        .copied()
        .min_by(|a, b| {
            (a - requested)
                .abs()
                .partial_cmp(&(b - requested).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .filter(|r| r.is_finite())
}

/// One delivered block plus the DSP-side read offset into it.
struct PendingBlock {
    samples: Vec<f32>,
    /// f32 values (not complex samples) already consumed.
    consumed: usize,
}

/// Airspy IQ source for the pipeline.
pub struct AirspySource {
    device: Option<Device>,
    sample_rate: f64,
    frequency: f64,
    /// Device-reported IQ rates, snapshotted at open (f64 for the
    /// trait). [`DEFAULT_SAMPLE_RATES`] until the first open.
    sample_rates: Vec<f64>,
    /// DSP-side receiver of delivered blocks; `Some` while streaming.
    blocks: Option<Receiver<Vec<f32>>>,
    /// Block currently being drained by `read_samples`.
    current: Option<PendingBlock>,
    /// Set by the bridge callback when the driver reports the stream
    /// ended (device error / unplug) so `read_samples` can surface it.
    stream_dead: Arc<AtomicBool>,
    /// Blocks dropped at the bridge because the DSP fell behind.
    bridge_dropped_blocks: Arc<AtomicU64>,
    /// Most-recent linearity step dispatched via `set_gain` /
    /// `set_gain_by_index`; `None` until the first dispatch so
    /// `start()` can fall back to [`FIRST_TIME_LINEARITY_STEP`].
    /// Remembered across stop/start like the RTL source's
    /// `last_tuner_gain_tenths_db` (#626 regression contract).
    last_linearity_step: Option<u8>,
    /// Most-recent gain mode (`Some(true)` = manual). Manual is the
    /// default: composite linearity gain applied, per-stage AGCs off.
    last_gain_manual: Option<bool>,
    /// Bias-T state to replay on open (powers a `SpyVerter` / external
    /// LNA). Dispatches are remembered even with the device closed.
    bias_tee: bool,
}

impl Default for AirspySource {
    fn default() -> Self {
        Self::new()
    }
}

impl AirspySource {
    /// Create a new Airspy source. Opens the first enumerated device
    /// at `start()` (serial selection is issue #848 follow-up scope).
    #[must_use]
    pub fn new() -> Self {
        Self {
            device: None,
            sample_rate: DEFAULT_SAMPLE_RATES[0],
            frequency: 100_000_000.0,
            sample_rates: DEFAULT_SAMPLE_RATES.to_vec(),
            blocks: None,
            current: None,
            stream_dead: Arc::new(AtomicBool::new(false)),
            bridge_dropped_blocks: Arc::new(AtomicU64::new(0)),
            last_linearity_step: None,
            last_gain_manual: None,
            bias_tee: false,
        }
    }

    /// Open the first enumerated device and program the pre-stream
    /// state: latch `Float32Iq` (before the rate snapshot —
    /// `samplerates()` reports per-sample-type values), snapshot the
    /// firmware rate table, clamp + apply the requested rate, tune,
    /// replay gain state, and apply bias-T (non-fatal). Split out of
    /// `start()` per the 50-NLOC gate on PR #850.
    fn open_and_configure(&mut self) -> Result<Device, SourceError> {
        let mut device = Device::open().map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        // Latch Float32Iq BEFORE the rate snapshot: `samplerates()`
        // reports per-sample-type values (doubled for real types).
        device
            .set_sample_type(SampleType::Float32Iq)
            .map_err(|e| SourceError::OpenFailed(format!("set_sample_type: {e}")))?;
        self.sample_rates = device
            .samplerates()
            .into_iter()
            .map(f64::from)
            .collect::<Vec<_>>();
        tracing::info!(
            rates = ?self.sample_rates,
            "AirspySource::start: device opened, firmware rate table snapshotted"
        );

        // Clamp the requested rate to the firmware table (a persisted
        // RTL-era rate must not fail Play — see `nearest_supported_rate`).
        let rate = nearest_supported_rate(&self.sample_rates, self.sample_rate)
            .ok_or_else(|| SourceError::OpenFailed("empty firmware rate table".into()))?;
        if (rate - self.sample_rate).abs() > f64::EPSILON {
            tracing::warn!(
                requested = self.sample_rate,
                clamped = rate,
                "AirspySource::start: unsupported rate clamped to nearest firmware rate"
            );
            self.sample_rate = rate;
        }
        device
            .set_samplerate(rate as u32)
            .map_err(|e| SourceError::OpenFailed(format!("set_samplerate: {e}")))?;

        device.set_freq(self.frequency as u32).map_err(|e| {
            SourceError::TuneFailed(format!("{:.3} MHz: {e}", self.frequency / HERTZ_PER_MHZ))
        })?;

        self.apply_gain_state(&device)?;
        // Remember the effective state so later mode flips replay the
        // same values (mirrors the RTL source's start() contract).
        self.last_gain_manual = Some(self.last_gain_manual.unwrap_or(true));
        if self.last_linearity_step.is_none() {
            self.last_linearity_step = Some(FIRST_TIME_LINEARITY_STEP);
        }

        if let Err(e) = device.set_rf_bias(self.bias_tee) {
            // Non-fatal: bias-T only matters for externally powered
            // frontends; surface it in the log and stream anyway.
            tracing::warn!(enabled = self.bias_tee, error = %e, "set_rf_bias failed");
        }
        Ok(device)
    }

    /// Apply the remembered gain state to an open device: manual mode
    /// programs the composite linearity gain (which itself disables
    /// both per-stage AGCs); auto mode enables LNA + mixer AGC.
    fn apply_gain_state(&self, device: &Device) -> Result<(), SourceError> {
        let manual = self.last_gain_manual.unwrap_or(true);
        let step = self
            .last_linearity_step
            .unwrap_or(FIRST_TIME_LINEARITY_STEP);
        if manual {
            device
                .set_linearity_gain(step)
                .map_err(|e| SourceError::OpenFailed(format!("set_linearity_gain: {e}")))?;
        } else {
            device
                .set_lna_agc(true)
                .and_then(|()| device.set_mixer_agc(true))
                .map_err(|e| SourceError::OpenFailed(format!("enable AGC: {e}")))?;
        }
        tracing::info!(manual, step, "AirspySource: gain state applied");
        Ok(())
    }
}

/// Bridge one delivered transfer into the block channel. Runs on the
/// driver's consumer thread; must never block, so a full channel
/// drops the block and bumps the drop counter. Returns the
/// keep-streaming flag.
fn bridge_transfer(
    tx: &SyncSender<Vec<f32>>,
    samples: &Samples<'_>,
    driver_dropped: u64,
    bridge_dropped: &AtomicU64,
) -> bool {
    let Samples::Float32(block) = samples else {
        // Unreachable: the sample type is latched to Float32Iq before
        // start_rx. Stop the stream rather than feed garbage.
        tracing::error!("AirspySource: non-Float32 block despite Float32Iq latch");
        return false;
    };
    if driver_dropped > 0 {
        tracing::warn!(driver_dropped, "Airspy driver ring dropped samples");
    }
    match tx.try_send(block.to_vec()) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            let total = bridge_dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if total.is_power_of_two() {
                // Log at 1, 2, 4, 8, ... so a sustained stall is
                // visible without flooding at ~16 blocks/s.
                tracing::warn!(
                    total_dropped_blocks = total,
                    "AirspySource: DSP behind — dropping block at bridge"
                );
            }
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

impl Source for AirspySource {
    fn name(&self) -> &'static str {
        "Airspy"
    }

    fn start(&mut self) -> Result<(), SourceError> {
        tracing::info!(
            sample_rate = self.sample_rate,
            frequency_hz = self.frequency,
            last_step = ?self.last_linearity_step,
            bias_tee = self.bias_tee,
            "AirspySource::start: opening device"
        );
        let mut device = self.open_and_configure()?;

        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(BLOCK_CHANNEL_BOUND);
        self.stream_dead.store(false, Ordering::Release);
        self.bridge_dropped_blocks.store(0, Ordering::Relaxed);
        let stream_dead = Arc::clone(&self.stream_dead);
        let bridge_dropped = Arc::clone(&self.bridge_dropped_blocks);
        device
            .start_rx(move |transfer| {
                let keep = bridge_transfer(
                    &tx,
                    &transfer.samples,
                    transfer.dropped_samples,
                    &bridge_dropped,
                );
                if !keep {
                    stream_dead.store(true, Ordering::Release);
                }
                keep
            })
            .map_err(|e| SourceError::OpenFailed(format!("start_rx: {e}")))?;

        self.blocks = Some(rx);
        self.current = None;
        self.device = Some(device);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        if let Some(mut device) = self.device.take()
            && let Err(e) = device.stop_rx()
        {
            // Drop still closes the USB handle after a failed stop.
            tracing::warn!(error = %e, "AirspySource::stop: stop_rx failed");
        }
        self.blocks = None;
        self.current = None;
        Ok(())
    }

    fn tune(&mut self, frequency_hz: f64) -> Result<(), SourceError> {
        // Control requests are safe mid-stream (`set_freq` is `&self`
        // vendor traffic, independent of the bulk endpoint). Commit
        // only after the driver accepted; with no device open,
        // remember for `start()` (same contract as RTL, #742).
        if let Some(device) = &self.device {
            device.set_freq(frequency_hz as u32).map_err(|e| {
                SourceError::TuneFailed(format!("{:.3} MHz: {e}", frequency_hz / HERTZ_PER_MHZ))
            })?;
        }
        self.frequency = frequency_hz;
        Ok(())
    }

    fn sample_rates(&self) -> &[f64] {
        &self.sample_rates
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SourceError> {
        if let Some(device) = &self.device {
            let rate = nearest_supported_rate(&self.sample_rates, rate)
                .ok_or_else(|| SourceError::InvalidParameter("empty rate table".into()))?;
            device
                .set_samplerate(rate as u32)
                .map_err(|e| SourceError::InvalidParameter(e.to_string()))?;
            self.sample_rate = rate;
        } else {
            // No device to validate against yet — accept verbatim;
            // `start()` clamps to the firmware table.
            self.sample_rate = rate;
        }
        Ok(())
    }

    fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
        let rx = self.blocks.as_ref().ok_or(SourceError::NotRunning)?;
        if self.stream_dead.load(Ordering::Acquire) {
            return Err(SourceError::ReadFailed("Airspy stream ended".to_string()));
        }
        // Refill the working block from the channel when drained.
        if self.current.is_none() {
            match rx.try_recv() {
                Ok(samples) => {
                    self.current = Some(PendingBlock {
                        samples,
                        consumed: 0,
                    });
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(0),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(SourceError::ReadFailed("Airspy stream ended".to_string()));
                }
            }
        }
        let Some(block) = self.current.as_mut() else {
            return Ok(0);
        };
        let count = convert_samples(&block.samples[block.consumed..], output);
        block.consumed += count * 2;
        // A trailing odd f32 can never form an IQ pair; count the
        // block drained once every whole pair is out (consumer-side
        // guard mirroring the RTL slot logic, #742/#785).
        if block.consumed + block.samples.len() % 2 >= block.samples.len() {
            self.current = None;
        }
        Ok(count)
    }

    fn set_gain(&mut self, gain_tenths: i32) -> Result<(), SourceError> {
        let step = linearity_step_from_tenths(gain_tenths);
        let device_open = self.device.is_some();
        tracing::info!(
            gain_tenths,
            step,
            device_open,
            "AirspySource::set_gain dispatch (linearity step)"
        );
        // Remember even when closed so `start()` replays the user's
        // choice instead of the first-time default (#626 contract).
        self.last_linearity_step = Some(step);
        if let Some(device) = &self.device {
            device
                .set_linearity_gain(step)
                .map_err(|e| SourceError::InvalidParameter(e.to_string()))?;
        }
        Ok(())
    }

    fn set_gain_mode(&mut self, manual: bool) -> Result<(), SourceError> {
        let device_open = self.device.is_some();
        tracing::info!(manual, device_open, "AirspySource::set_gain_mode dispatch");
        self.last_gain_manual = Some(manual);
        if let Some(device) = &self.device {
            if manual {
                // Composite gain write disables both AGCs itself.
                let step = self
                    .last_linearity_step
                    .unwrap_or(FIRST_TIME_LINEARITY_STEP);
                device
                    .set_linearity_gain(step)
                    .map_err(|e| SourceError::InvalidParameter(e.to_string()))?;
            } else {
                device
                    .set_lna_agc(true)
                    .and_then(|()| device.set_mixer_agc(true))
                    .map_err(|e| SourceError::InvalidParameter(e.to_string()))?;
            }
        }
        Ok(())
    }

    fn set_gain_by_index(&mut self, index: u32) -> Result<(), SourceError> {
        let Some(&gain_tenths) = usize::try_from(index)
            .ok()
            .and_then(|i| LINEARITY_GAIN_TENTHS.get(i))
        else {
            return Err(SourceError::InvalidParameter(format!(
                "gain index {index} out of range (linearity ladder has {} steps)",
                LINEARITY_GAIN_TENTHS.len()
            )));
        };
        self.set_gain(gain_tenths)
    }

    fn gains(&self) -> &[i32] {
        &LINEARITY_GAIN_TENTHS
    }

    fn set_bias_tee(&mut self, enabled: bool) -> Result<(), SourceError> {
        // Powers a SpyVerter / mast-head LNA over the RF port.
        // Remembered for replay on open, forwarded live otherwise —
        // same contract as the RTL source (#537 / #739 lineage).
        let device_open = self.device.is_some();
        tracing::info!(enabled, device_open, "AirspySource::set_bias_tee dispatch");
        self.bias_tee = enabled;
        if let Some(device) = &self.device {
            device
                .set_rf_bias(enabled)
                .map_err(|e| SourceError::InvalidParameter(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
