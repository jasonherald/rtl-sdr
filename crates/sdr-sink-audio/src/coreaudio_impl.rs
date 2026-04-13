//! CoreAudio (AUHAL) audio output sink for macOS.
//!
//! Mirrors the [`crate::pw_impl`] PipeWire backend behind the same
//! [`Sink`] trait so [`crate::lib`]'s cfg dispatch can swap one for the
//! other based on `target_os`.
//!
//! ## Architecture
//!
//! ```text
//! write_samples(&[Stereo])  ──interleave──▶  AudioRingBuffer
//!                                                  │
//!                                                  │ (CoreAudio I/O thread)
//!                                                  ▼
//!                                          AURenderCallback
//!                                                  │
//!                                                  ▼
//!                                  Default output device (or `set_target` UID)
//! ```
//!
//! - Uses the **default output unit** (`kAudioUnitSubType_DefaultOutput`) from
//!   the high-level [`coreaudio::audio_unit::AudioUnit`] wrapper.
//! - Format: 48 kHz f32 stereo, **non-interleaved** — the canonical
//!   format for Mac AudioUnits per Apple's *Core Audio Overview*.
//! - The render callback **never allocates**: it uses a stack scratch
//!   buffer sized for `MAX_CB_FRAMES = 4096` and degrades to silence on
//!   overflow rather than falling back to the heap. CoreAudio quanta are
//!   typically 256–512 frames; 4096 is generous headroom for aggregate
//!   or pro-audio devices.
//!
//! ## Public surface parity with `pw_impl`
//!
//! The two backends export the same names — [`AudioSink`], [`AudioDevice`],
//! [`list_audio_sinks`] — and `lib.rs` re-exports whichever one is selected
//! by cfg. `sdr-core::DspState` constructs `AudioSink::new()` without
//! knowing which backend it has.
//!
//! ## v1 vs v2: `node_name` semantics
//!
//! The spec
//! (`docs/superpowers/specs/2026-04-12-coreaudio-sink-design.md`) says
//! `AudioDevice::node_name` should be the CoreAudio device UID
//! (`kAudioDevicePropertyDeviceUID`). `coreaudio-rs 0.14` does not
//! expose a UID helper, so retrieving the UID would require dropping
//! into raw `coreaudio-sys` calls plus a `core-foundation` dep for
//! `CFString → String` conversion plus an `#[allow(unsafe_code)]`
//! override of the workspace-wide deny.
//!
//! For **v1** we use the device *display name* as `node_name` instead.
//! This is a v1-only deviation because:
//!
//! - The MVP UI does not expose an audio device picker (deferred to v2,
//!   issue #237). `set_target` is implemented but never called from any
//!   v1 caller — `node_name` is essentially unused metadata.
//! - Switching to UID is a clean follow-up: one `unsafe` helper that
//!   wraps `AudioObjectGetPropertyData(kAudioDevicePropertyDeviceUID, …)`,
//!   landing alongside the v2 device picker in #237.
//!
//! Documenting this here so the next reviewer doesn't have to re-derive
//! the trade-off from CodeRabbit history.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::macos_helpers::{
    audio_unit_from_device_id, get_audio_device_ids_for_scope, get_audio_device_supports_scope,
    get_default_device_id, get_device_id_from_name, get_device_name,
};
use coreaudio::audio_unit::render_callback::{self, data};
use coreaudio::audio_unit::stream_format::StreamFormat;
use coreaudio::audio_unit::{AudioUnit, Element, SampleFormat, Scope};

use sdr_pipeline::sink_manager::Sink;
use sdr_types::{SinkError, Stereo};

use crate::ring::AudioRingBuffer;

/// Engine-side audio sample rate in Hz. The engine always sees 48 kHz; if
/// the device runs at something else, AUHAL's internal converter handles
/// the resampling for us.
const AUDIO_SAMPLE_RATE: f64 = 48_000.0;

/// Number of audio channels (stereo).
const AUDIO_CHANNELS: u32 = 2;

/// Audio ring buffer capacity in `f32` samples (interleaved stereo).
/// ~1 second at 48 kHz stereo = 96,000 samples.
const RING_CAPACITY: usize = 96_000;

/// Initial capacity for the stereo interleave buffer in `write_samples`.
const INTERLEAVE_BUF_INITIAL_CAP: usize = 1024;

/// Maximum frames per render callback we are willing to service from the
/// stack scratch buffer. CoreAudio quanta are typically 256 or 512 frames;
/// 4096 is a generous ceiling that covers any aggregate device or
/// pro-audio device the user might have configured. The render callback
/// degrades to silence rather than allocating on the RT thread when a
/// quantum exceeds this — see the callback body.
const MAX_CB_FRAMES: usize = 4096;

/// An audio sink device with display name and a caller-opaque identifier.
///
/// On CoreAudio, `node_name` is the device's display name in v1 — see
/// the v1-vs-v2 note in the module-level docs. Empty string means
/// "system default output".
#[derive(Clone, Debug)]
pub struct AudioDevice {
    /// Human-readable name (from `kAudioObjectPropertyName`).
    pub display_name: String,
    /// Caller-opaque device identifier. Empty = "system default output".
    pub node_name: String,
}

/// Audio output sink backed by CoreAudio AUHAL.
pub struct AudioSink {
    sample_rate: f64,
    running: Arc<AtomicBool>,
    ring: Arc<AudioRingBuffer>,
    audio_unit: Option<AudioUnit>,
    /// Target device, set via [`AudioSink::set_target`]. Empty string
    /// means "system default output".
    target_device: String,
    /// Pre-allocated interleave buffer used by `write_samples` so the
    /// hot path never allocates.
    interleave_buf: Vec<f32>,
}

impl AudioSink {
    /// Create a new sink. Does **not** open the AudioUnit yet — that
    /// happens in [`AudioSink::start`] so the device pick can be retried
    /// if the user changes the system default output between sink
    /// construction and stream start.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sample_rate: AUDIO_SAMPLE_RATE,
            running: Arc::new(AtomicBool::new(false)),
            ring: Arc::new(AudioRingBuffer::new(RING_CAPACITY)),
            audio_unit: None,
            target_device: String::new(),
            interleave_buf: Vec::with_capacity(INTERLEAVE_BUF_INITIAL_CAP),
        }
    }

    /// Set the target output device. Empty string routes to the system
    /// default output.
    ///
    /// In v1 the `node_name` is interpreted as a device **display name**
    /// (matching what [`list_audio_sinks`] returns); v2 will switch to
    /// CoreAudio device UID. See the module docs for the rationale.
    ///
    /// If the sink is already running, it is stopped, reconfigured, and
    /// restarted — same pattern as the PipeWire backend.
    ///
    /// # Errors
    ///
    /// Returns [`SinkError::DeviceNotFound`] (raised by `start` on the
    /// next open attempt) if the new target cannot be located, or any
    /// error from `start` / `stop` if the sink was running.
    pub fn set_target(&mut self, node_name: &str) -> Result<(), SinkError> {
        let was_running = self.audio_unit.is_some();
        if was_running {
            self.stop()?;
        }
        self.target_device.clear();
        self.target_device.push_str(node_name);
        if was_running {
            self.start()?;
        }
        Ok(())
    }

    /// Send stereo audio samples to CoreAudio for playback.
    ///
    /// # Errors
    ///
    /// Returns [`SinkError::NotRunning`] if the sink has not been started.
    pub fn write_samples(&mut self, samples: &[Stereo]) -> Result<(), SinkError> {
        if !self.running.load(Ordering::Acquire) {
            return Err(SinkError::NotRunning);
        }

        // Interleave stereo into the pre-allocated scratch buffer (zero
        // allocation on the hot path) and push it to the ring. The
        // render callback de-interleaves on the audio thread.
        self.interleave_buf.clear();
        for s in samples {
            self.interleave_buf.push(s.l);
            self.interleave_buf.push(s.r);
        }

        self.ring.write(&self.interleave_buf);
        Ok(())
    }

    /// Open the AUHAL output unit, configure format, register the
    /// render callback, and initialize. Called from [`Sink::start`].
    fn open_unit(&mut self) -> Result<AudioUnit, SinkError> {
        // Pick the device. Empty target_device means "system default output".
        let device_id = if self.target_device.is_empty() {
            get_default_device_id(false)
                .ok_or_else(|| SinkError::DeviceNotFound("system default output".to_string()))?
        } else {
            get_device_id_from_name(&self.target_device, false)
                .ok_or_else(|| SinkError::DeviceNotFound(self.target_device.clone()))?
        };

        // Build the AUHAL output unit bound to this device.
        let mut unit = audio_unit_from_device_id(device_id, false)
            .map_err(|e| SinkError::OpenFailed(format!("audio_unit_from_device_id: {e}")))?;

        // Set the input-side stream format on element 0 (output bus). On
        // macOS the canonical AudioUnit format is non-interleaved 32-bit
        // float; AUHAL converts internally to whatever the device wants,
        // including any sample-rate conversion needed when the device
        // runs at a rate other than 48 kHz.
        let format = StreamFormat {
            sample_rate: AUDIO_SAMPLE_RATE,
            sample_format: SampleFormat::F32,
            flags: LinearPcmFlags::IS_FLOAT
                | LinearPcmFlags::IS_PACKED
                | LinearPcmFlags::IS_NON_INTERLEAVED,
            channels: AUDIO_CHANNELS,
        };
        unit.set_stream_format(format, Scope::Input, Element::Output)
            .map_err(|e| SinkError::OpenFailed(format!("set_stream_format: {e}")))?;

        // Register the render callback. The callback captures three
        // pieces of state by move:
        //
        //   1. `ring_for_cb`: Arc clone of the audio ring buffer.
        //      Reads are non-blocking via `try_lock` so contention with
        //      the producer never blocks the audio I/O thread.
        //
        //   2. `scratch_buf`: a heap-allocated de-interleave scratch
        //      buffer, allocated **here** (the cold start path) and
        //      reused for the lifetime of the AudioUnit. The audio I/O
        //      thread never allocates.
        //
        //   3. `overflow_warned`: a one-shot atomic flag used by
        //      `render_callback_body` to throttle the "render quantum
        //      exceeded scratch buffer" warning to exactly **one log
        //      line per sink lifetime**. Without this, a device whose
        //      quantum is consistently above MAX_CB_FRAMES would
        //      generate hundreds of `tracing::warn!` calls per second
        //      from the RT thread — turning the silence fallback into
        //      its own dropout source. The flag resets on every
        //      `start()` call (because a fresh closure is constructed)
        //      so users who fix their device config and restart get a
        //      fresh warning if the issue recurs.
        let ring_for_cb = Arc::clone(&self.ring);
        let mut scratch_buf: Vec<f32> = vec![0.0; MAX_CB_FRAMES * 2];
        let overflow_warned = AtomicBool::new(false);
        unit.set_render_callback(
            move |args: render_callback::Args<data::NonInterleaved<f32>>| {
                render_callback_body(&ring_for_cb, &overflow_warned, &mut scratch_buf, args);
                // The inner function is infallible by construction —
                // every code path in it writes silence on degraded
                // quanta rather than returning an error. The Result
                // wrapping is required by `set_render_callback`'s
                // signature (`FnMut(Args<D>) -> Result<(), ()>`).
                Ok(())
            },
        )
        .map_err(|e| SinkError::OpenFailed(format!("set_render_callback: {e}")))?;

        // Initialize and we're ready to start.
        unit.initialize()
            .map_err(|e| SinkError::OpenFailed(format!("audio_unit.initialize: {e}")))?;

        Ok(unit)
    }
}

impl Default for AudioSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for AudioSink {
    fn name(&self) -> &str {
        "Audio"
    }

    fn start(&mut self) -> Result<(), SinkError> {
        if self.running.load(Ordering::Acquire) {
            return Err(SinkError::AlreadyRunning);
        }

        // Drain any stale samples from a previous run before opening the
        // unit. This matches the PipeWire impl's ordering.
        self.ring.clear();

        // Set running BEFORE starting the unit so write_samples can
        // queue audio the moment the render callback fires for the
        // first time. (Same ordering as `pw_impl`.)
        self.running.store(true, Ordering::Release);

        let mut unit = match self.open_unit() {
            Ok(u) => u,
            Err(e) => {
                // Roll back the running flag if open failed.
                self.running.store(false, Ordering::Release);
                return Err(e);
            }
        };

        if let Err(e) = unit.start() {
            self.running.store(false, Ordering::Release);
            return Err(SinkError::OpenFailed(format!("audio_unit.start: {e}")));
        }

        self.audio_unit = Some(unit);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        if !self.running.load(Ordering::Acquire) {
            return Err(SinkError::NotRunning);
        }

        if let Some(mut unit) = self.audio_unit.take() {
            // Best-effort: stop the unit. If stop returns an error we
            // still want to drop the unit and clear the running flag,
            // so we log-and-continue rather than propagating up.
            if let Err(e) = unit.stop() {
                tracing::warn!("audio_unit.stop returned error: {e}");
            }
            // Dropping `unit` here uninitializes and disposes the AU.
            drop(unit);
        }

        self.running.store(false, Ordering::Release);
        self.ring.clear();
        Ok(())
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SinkError> {
        // Engine-side rate is fixed at 48 kHz. Anything else is an
        // engine-config bug — the engine should never ask the sink to
        // run at a different rate.
        if !rate.is_finite() || rate <= 0.0 {
            return Err(SinkError::InvalidParameter(format!(
                "sample rate must be positive and finite, got {rate}"
            )));
        }
        if (rate - AUDIO_SAMPLE_RATE).abs() > f64::EPSILON {
            return Err(SinkError::InvalidParameter(format!(
                "CoreAudio sink only supports {AUDIO_SAMPLE_RATE} Hz (got {rate})"
            )));
        }
        self.sample_rate = rate;
        Ok(())
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }
}

/// Render callback body — extracted from the closure so it has a
/// nameable type and is unit-testable in principle.
///
/// **Allocates nothing.** The `scratch` slice is owned by the render
/// closure (allocated once at sink construction) and passed in by
/// `&mut`. The callback uses `scratch.len() / 2` as the per-quantum
/// frame ceiling; if a single quantum exceeds it, the callback writes
/// silence to both channels and emits a `tracing` warning **at most
/// once per sink lifetime** (gated by the `overflow_warned` atomic)
/// rather than growing the scratch on the audio thread. Heap
/// allocation on the CoreAudio render thread can block on the system
/// allocator and miss a deadline; silence is the correct safe
/// degradation. Debug builds also `debug_assert!` so the overflow
/// path fires loudly during dev.
fn render_callback_body(
    ring: &AudioRingBuffer,
    overflow_warned: &AtomicBool,
    scratch: &mut [f32],
    mut args: render_callback::Args<data::NonInterleaved<f32>>,
) {
    let frames = args.num_frames;
    // Scratch is laid out as interleaved stereo, so the per-quantum
    // frame ceiling is half its slice length.
    let max_frames = scratch.len() / 2;

    // Pull out the two channel slices. AUHAL on macOS gives us exactly
    // one buffer per channel; if for some reason there's only one
    // channel (mono device?) we render silence and bail.
    let mut channels = args.data.channels_mut();
    let Some(left) = channels.next() else {
        return;
    };
    let Some(right) = channels.next() else {
        // Mono output — fill the one channel with silence and stop.
        for v in left.iter_mut().take(frames) {
            *v = 0.0;
        }
        return;
    };

    debug_assert!(
        frames <= max_frames,
        "CoreAudio render quantum {frames} exceeded scratch capacity {max_frames}"
    );

    if frames > max_frames {
        // Release-build degradation: write silence to both channels.
        // Emit the warning at most ONCE per sink lifetime via the
        // `overflow_warned` flag — without this gate the warning would
        // fire on every oversized quantum from the RT thread, which
        // would itself become a dropout source (each tracing::warn! is
        // a syscall + format work). Once-per-lifetime means the user
        // sees the issue, observes the silence symptom, and has the
        // info to act; the `start()` path constructs a fresh atomic
        // so a stop-and-restart after fixing the device gets a fresh
        // warning if the issue recurs.
        //
        // `swap(true)` is a single atomic write — Relaxed is fine
        // because we don't need to synchronize anything else.
        if !overflow_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                frames,
                max_frames,
                "CoreAudio render quantum exceeded scratch buffer; rendering silence (further occurrences will be silenced)"
            );
        }
        for v in left.iter_mut().take(frames) {
            *v = 0.0;
        }
        for v in right.iter_mut().take(frames) {
            *v = 0.0;
        }
        return;
    }

    let want_samples = frames * 2;
    let read = ring.read(&mut scratch[..want_samples]);

    // De-interleave into the two channel slices. If the ring didn't
    // have enough samples (underrun, or first-tick before the engine
    // has produced anything), the trailing slots get silence.
    for i in 0..frames {
        let l_idx = i * 2;
        let r_idx = i * 2 + 1;
        if r_idx < read {
            left[i] = scratch[l_idx];
            right[i] = scratch[r_idx];
        } else {
            left[i] = 0.0;
            right[i] = 0.0;
        }
    }
}

/// Enumerate available CoreAudio output devices.
///
/// Always prepends a "Default" entry with `node_name = ""` so callers
/// can route to the system default without knowing its name. Devices
/// without any output channels (e.g., mic-only inputs) are filtered out.
/// The system default device is deduplicated against the enumerated
/// list so it doesn't appear twice in pickers.
#[must_use]
pub fn list_audio_sinks() -> Vec<AudioDevice> {
    let mut sinks = vec![AudioDevice {
        display_name: "Default".to_string(),
        node_name: String::new(),
    }];

    let default_id = get_default_device_id(false);

    let Ok(all_ids) = get_audio_device_ids_for_scope(Scope::Global) else {
        // Could not enumerate — return just the default entry. The
        // caller can still route audio.
        return sinks;
    };

    for device_id in all_ids {
        // Skip devices without output channels.
        match get_audio_device_supports_scope(device_id, Scope::Output) {
            Ok(true) => {}
            Ok(false) | Err(_) => continue,
        }

        // Skip the system default — it's already represented by the
        // "Default" entry above.
        if Some(device_id) == default_id {
            continue;
        }

        let Ok(name) = get_device_name(device_id) else {
            continue;
        };

        sinks.push(AudioDevice {
            display_name: name.clone(),
            // v1: see the v1-vs-v2 note in the module docs. node_name
            // is the display name; v2 switches to CoreAudio device UID
            // alongside the device picker (#237).
            node_name: name,
        });
    }

    sinks
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn new_does_not_panic() {
        let sink = AudioSink::new();
        assert_eq!(sink.name(), "Audio");
        assert!((sink.sample_rate() - AUDIO_SAMPLE_RATE).abs() < f64::EPSILON);
    }

    #[test]
    fn write_before_start_returns_not_running() {
        let mut sink = AudioSink::new();
        let samples = [Stereo::new(0.0, 0.0)];
        assert!(
            matches!(sink.write_samples(&samples), Err(SinkError::NotRunning)),
            "write_samples should fail before start"
        );
    }

    #[test]
    fn stop_before_start_returns_not_running() {
        let mut sink = AudioSink::new();
        assert!(matches!(sink.stop(), Err(SinkError::NotRunning)));
    }

    #[test]
    fn set_sample_rate_validation() {
        let mut sink = AudioSink::new();
        assert!(sink.set_sample_rate(AUDIO_SAMPLE_RATE).is_ok());
        assert!(sink.set_sample_rate(44100.0).is_err());
        assert!(sink.set_sample_rate(-1.0).is_err());
        assert!(sink.set_sample_rate(f64::NAN).is_err());
        assert!(sink.set_sample_rate(f64::INFINITY).is_err());
    }

    #[test]
    fn list_audio_sinks_includes_default() {
        let devices = list_audio_sinks();
        assert!(!devices.is_empty(), "list must always include 'Default'");
        let default = &devices[0];
        assert_eq!(default.display_name, "Default");
        assert!(default.node_name.is_empty());
    }
}
