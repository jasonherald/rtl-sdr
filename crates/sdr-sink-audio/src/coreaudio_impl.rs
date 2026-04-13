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
//!                                  Default output device (or `set_target` AudioDeviceID)
//! ```
//!
//! - Uses the **default output unit** (`kAudioUnitSubType_DefaultOutput`) from
//!   the high-level [`coreaudio::audio_unit::AudioUnit`] wrapper.
//! - Format: 48 kHz f32 stereo, **non-interleaved** — the canonical
//!   format for Mac AudioUnits per Apple's *Core Audio Overview*.
//! - The render callback **never allocates**. The de-interleave scratch
//!   buffer is a single `Vec<f32>` allocated **once** in `open_unit()`
//!   on the cold start path (sized to `MAX_CB_FRAMES * 2` f32s) and
//!   moved into the render closure by value. The callback reuses that
//!   buffer for the lifetime of the AudioUnit and degrades to silence
//!   on quanta larger than `MAX_CB_FRAMES` rather than growing the
//!   buffer on the audio I/O thread. CoreAudio quanta are typically
//!   256–512 frames; 4096 is generous headroom for aggregate or
//!   pro-audio devices.
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
//! For **v1** we use the **`AudioDeviceID` as a decimal string** instead
//! of the UID. The ID is unique within the running session — even when
//! multiple devices share the same display name (e.g., several "USB
//! Audio CODEC" devices, multiple AirPlay endpoints) — so it's the
//! right opaque handle for `set_target` selection. The downside is
//! that AudioDeviceIDs are session-scoped: they don't survive reboots,
//! plug events, or `coreaudiod` restarts. v2 (issue #237) switches to
//! the stable CoreAudio device UID at the same time the device picker
//! lands; until then, the v1 contract works because:
//!
//! - The MVP UI does not expose a persistent device picker (deferred
//!   to v2). v1 only ever uses the empty-string "system default
//!   output" path; `set_target` is implemented for completeness and
//!   for any out-of-tree caller that wants session-scoped routing.
//! - The "Default" entry (empty `node_name`) is always available and
//!   resolves through `get_default_device_id` instead of an ID parse,
//!   so the default-output path is unaffected by the session-scoped
//!   restriction.
//!
//! Earlier drafts of this PR used the device display name as
//! `node_name`, which CodeRabbit caught as non-unique on systems with
//! duplicate names — switched to AudioDeviceID. Documenting both
//! revisions here so the next reviewer doesn't have to re-derive the
//! trade-off from CodeRabbit history.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::macos_helpers::{
    audio_unit_from_device_id, get_audio_device_ids_for_scope, get_audio_device_supports_scope,
    get_default_device_id, get_device_name,
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

/// Capacity for the stereo interleave buffer in `write_samples`.
///
/// Sized to match the ring buffer (the largest write that could ever
/// fit in the ring without dropping data) so the hot path **never**
/// reallocates: the buffer is `clear()`ed and re-pushed each call,
/// but never grows beyond its initial capacity for any input within
/// the ring's natural ceiling. The previous value of 1024 was a
/// historical artifact from `pw_impl.rs` and silently grew the Vec
/// for any write larger than 512 stereo frames — caught by
/// CodeRabbit on PR #253.
const INTERLEAVE_BUF_CAPACITY: usize = RING_CAPACITY;

/// Maximum frames per render callback we are willing to service from
/// the de-interleave scratch buffer. CoreAudio quanta are typically
/// 256 or 512 frames; 4096 is a generous ceiling that covers any
/// aggregate device or pro-audio device the user might have configured.
///
/// The scratch buffer itself is a heap `Vec<f32>` of length
/// `MAX_CB_FRAMES * 2` allocated **once** in `open_unit()` (the cold
/// start path) and moved into the render closure. The audio I/O thread
/// never allocates: when a quantum exceeds `MAX_CB_FRAMES`, the
/// callback writes silence to both channels and emits a one-shot
/// `tracing::warn` rather than growing the buffer.
const MAX_CB_FRAMES: usize = 4096;

/// An audio sink device with display name and a caller-opaque identifier.
///
/// On CoreAudio, `node_name` is the device's `AudioDeviceID` formatted
/// as a decimal `u32` string. This is **session-scoped** — it stays
/// stable for the lifetime of the running process but is not
/// guaranteed to persist across reboots, plug events, or `coreaudiod`
/// restarts. v2 (issue #237) will switch this to the device's
/// CoreAudio UID (`kAudioDevicePropertyDeviceUID`) once we can drop
/// the unsafe-code helper that currently blocks the upgrade.
///
/// **Why not the display name?** Display names are not unique on
/// CoreAudio: a system can have two "USB Audio CODEC" devices, two
/// "AirPlay" endpoints, two virtual aggregates with the same label,
/// etc. Using the name as the caller-opaque handle would make
/// `set_target` selection non-deterministic across duplicates and
/// could route audio to the wrong device. AudioDeviceID is unique by
/// definition within a session, so it's the right opaque handle even
/// before we can store a fully-stable UID. CodeRabbit caught this on
/// PR #253.
///
/// Empty string means "system default output" on every platform.
#[derive(Clone, Debug)]
pub struct AudioDevice {
    /// Human-readable name (from `kAudioObjectPropertyName`).
    pub display_name: String,
    /// Caller-opaque device identifier. On macOS this is the
    /// `AudioDeviceID` as a decimal string in v1; in v2 it becomes the
    /// CoreAudio device UID. Empty = "system default output".
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
            interleave_buf: Vec::with_capacity(INTERLEAVE_BUF_CAPACITY),
        }
    }

    /// Set the target output device. Empty string routes to the system
    /// default output.
    ///
    /// `node_name` is interpreted as a CoreAudio `AudioDeviceID`
    /// formatted as a decimal string (matching what
    /// [`list_audio_sinks`] returns). The format is **pre-validated
    /// up front** before touching the running AudioUnit, so a
    /// completely invalid string returns [`SinkError::InvalidParameter`]
    /// without disturbing audio playback. In v2 (issue #237)
    /// `node_name` will become the device's CoreAudio UID instead,
    /// but the pre-validate-then-swap contract stays the same.
    ///
    /// If the sink is already running, the swap is **transactional**:
    /// the old `target_device` is preserved, the unit is stopped, the
    /// new target is installed, and `start()` is called. If the start
    /// fails (e.g., the new ID is stale after a plug/unplug, or the
    /// device disappeared between `list_audio_sinks` and now), the old
    /// `target_device` is restored and `start()` is called a second
    /// time to bring the previous working route back. Only if both
    /// the swap **and** the rollback fail does the sink end up
    /// stopped — and in that case the function returns the original
    /// swap error so the caller knows what was attempted, with a
    /// `tracing::error` covering the rollback failure.
    ///
    /// # Errors
    ///
    /// Returns [`SinkError::InvalidParameter`] if `node_name` cannot
    /// be parsed as a `u32`, [`SinkError::DeviceNotFound`] if the
    /// parsed ID does not name a valid output device when `start()`
    /// runs, or any error from `stop` / `start`. On failure to swap
    /// AND failure to roll back, the original swap error is returned.
    pub fn set_target(&mut self, node_name: &str) -> Result<(), SinkError> {
        // Pre-validate the new target *before* touching the running
        // unit. Catches the easy class of failures (typos, garbage,
        // wrong-type values) without any teardown.
        parse_target_device(node_name)?;

        // Idempotent fast path: if the requested target matches the
        // current one, do nothing. Avoids a stop/start cycle (and the
        // audible glitch + failure surface that comes with it) when
        // callers re-set the same device, e.g., after a settings
        // window confirms its current selection.
        if self.target_device == node_name {
            return Ok(());
        }

        let was_running = self.audio_unit.is_some();

        if !was_running {
            // Idle sink: store and return. The next `start()` call
            // will run open_unit and surface any device-resolution
            // failure.
            self.target_device.clear();
            self.target_device.push_str(node_name);
            return Ok(());
        }

        // Running sink: transactional swap with rollback on failure.
        let old_target = std::mem::take(&mut self.target_device);

        if let Err(stop_err) = self.stop() {
            // Could not even stop the old unit cleanly. Put the old
            // target back so subsequent calls see consistent state and
            // bail; the sink may or may not still be running depending
            // on what stop() actually did.
            self.target_device = old_target;
            return Err(stop_err);
        }

        self.target_device.clear();
        self.target_device.push_str(node_name);

        match self.start() {
            Ok(()) => Ok(()),
            Err(swap_err) => {
                // Rollback path: the new target failed to start.
                // Restore the old target_device and try to bring the
                // previous working route back so audio playback
                // resumes instead of staying dead.
                tracing::warn!(
                    error = %swap_err,
                    new_target = node_name,
                    old_target = old_target.as_str(),
                    "set_target failed; rolling back to previous device"
                );
                self.target_device.clear();
                self.target_device.push_str(&old_target);

                if let Err(rollback_err) = self.start() {
                    // Both the swap and the rollback failed. The sink
                    // is now stopped. Surface the original error so
                    // the caller sees what they tried to do; the
                    // rollback error gets a tracing::error of its own.
                    tracing::error!(
                        swap_error = %swap_err,
                        rollback_error = %rollback_err,
                        "set_target rollback also failed; sink is now stopped"
                    );
                }
                Err(swap_err)
            }
        }
    }

    /// Send stereo audio samples to CoreAudio for playback.
    ///
    /// The interleave buffer is pre-sized to [`INTERLEAVE_BUF_CAPACITY`]
    /// (= [`RING_CAPACITY`] f32s = ~48,000 stereo frames). For any input
    /// up to that ceiling the call is **allocation-free** — the buffer
    /// is `clear()`ed (which only resets `len`, leaving `capacity`
    /// alone) and then `push()`ed back up to the new size without ever
    /// calling the allocator. Inputs larger than that ceiling would
    /// also overflow the ring buffer itself, so they're a contract
    /// violation; debug builds `debug_assert!` to catch this loudly,
    /// release builds let `Vec::push` reallocate once and then proceed
    /// with the larger capacity (a one-time cost we accept as a graceful
    /// degradation rather than dropping samples).
    ///
    /// # Errors
    ///
    /// Returns [`SinkError::NotRunning`] if the sink has not been started.
    pub fn write_samples(&mut self, samples: &[Stereo]) -> Result<(), SinkError> {
        if !self.running.load(Ordering::Acquire) {
            return Err(SinkError::NotRunning);
        }

        debug_assert!(
            samples.len() * 2 <= INTERLEAVE_BUF_CAPACITY,
            "write_samples called with {} stereo frames, exceeds interleave buffer capacity {} (would overflow the ring buffer too)",
            samples.len(),
            INTERLEAVE_BUF_CAPACITY / 2
        );

        // Interleave stereo into the pre-allocated scratch buffer (zero
        // allocation on the hot path under normal sizing) and push it
        // to the ring. The render callback de-interleaves on the audio
        // thread.
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
        // Pick the device. Empty target_device means "system default
        // output"; any non-empty value is parsed as a decimal
        // AudioDeviceID (the same format `list_audio_sinks` emits).
        let device_id = match parse_target_device(&self.target_device)? {
            Some(id) => id,
            None => get_default_device_id(false)
                .ok_or_else(|| SinkError::DeviceNotFound("system default output".to_string()))?,
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

/// Parse a `target_device` string into an [`AudioDeviceID`] selection.
///
/// - Empty string → `Ok(None)` (caller resolves the system default).
/// - Decimal `u32` string → `Ok(Some(id))`.
/// - Anything else → `Err(SinkError::InvalidParameter)`.
///
/// Extracted from [`AudioSink::open_unit`] so it can be unit-tested in
/// isolation and so [`AudioSink::set_target`] can pre-validate a new
/// target before tearing down the running AudioUnit (avoiding the
/// "stop-and-fail leaves the sink dead" hazard CodeRabbit caught on
/// PR #253).
fn parse_target_device(target_device: &str) -> Result<Option<u32>, SinkError> {
    if target_device.is_empty() {
        return Ok(None);
    }
    target_device.parse::<u32>().map(Some).map_err(|_| {
        SinkError::InvalidParameter(format!(
            "CoreAudio target_device must be a decimal AudioDeviceID, got {target_device:?}"
        ))
    })
}

/// Render callback body — extracted from the closure so it has a
/// nameable type and is unit-testable in principle.
///
/// **Allocates nothing.** The `scratch` slice is a heap-allocated
/// `Vec<f32>` owned by the render closure: it is allocated **once**
/// in `open_unit()` (the cold start path) and reused for the lifetime
/// of the AudioUnit. The audio I/O thread never sees an allocator
/// call. The callback uses `scratch.len() / 2` as the per-quantum
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
/// can route to the system default without knowing its identifier.
/// Devices without any output channels (e.g., mic-only inputs) are
/// filtered out. The system default device is deduplicated against
/// the enumerated list so it doesn't appear twice in pickers.
///
/// `node_name` is the device's [`AudioDeviceID`] formatted as a
/// decimal `u32` string — uniquely identifies a device within the
/// running session, even when multiple devices share a display name
/// (e.g., several "USB Audio CODEC" devices). [`set_target`] parses
/// this string back into the integer ID directly. v2 (issue #237)
/// switches `node_name` to a stable CoreAudio device UID; v1 uses
/// the session-scoped ID because it's the only unique handle the
/// `coreaudio-rs 0.14` API exposes without dropping into raw
/// `coreaudio-sys` calls.
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

        let display_name =
            get_device_name(device_id).unwrap_or_else(|_| format!("Device {device_id}"));

        sinks.push(AudioDevice {
            display_name,
            // Decimal AudioDeviceID. Unique within the running
            // session even when display names collide. See the
            // function docs and the v1-vs-v2 note in the module
            // docs for the v2 path (#237 → CoreAudio device UID).
            node_name: device_id.to_string(),
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

    #[test]
    fn enumerated_node_names_parse_as_audio_device_ids() {
        // Every non-default entry's node_name must be a decimal u32
        // (the AudioDeviceID round-trip contract). This is what
        // `set_target` will parse it as.
        let devices = list_audio_sinks();
        for dev in devices.iter().skip(1) {
            assert!(
                !dev.node_name.is_empty(),
                "non-default device has empty node_name: {dev:?}"
            );
            let parsed = dev.node_name.parse::<u32>();
            assert!(
                parsed.is_ok(),
                "node_name {:?} for device {:?} is not a decimal u32",
                dev.node_name,
                dev.display_name,
            );
        }
    }

    #[test]
    fn set_target_stores_valid_id_when_idle() {
        // On an idle sink (audio_unit = None), set_target pre-validates
        // the format and then stores the string. No AudioUnit work
        // happens because there's nothing to swap; the next start()
        // will call open_unit and surface any device-resolution
        // failure (stale ID, etc.).
        let mut sink = AudioSink::new();
        sink.set_target("42")
            .expect("set_target with a valid id should succeed on an idle sink");
        assert_eq!(sink.target_device, "42");
    }

    #[test]
    fn set_target_pre_validation_rejects_garbage_without_disturbing_state() {
        // The pre-validation step in set_target catches malformed
        // strings BEFORE touching the running AudioUnit (or, on an
        // idle sink, before mutating target_device). This is the
        // "doesn't take down a working sink for a typo" guarantee
        // CodeRabbit caught on PR #253.
        let mut sink = AudioSink::new();

        // Establish a known target so we can prove it survives the
        // failed call.
        sink.set_target("7").expect("baseline set_target");
        assert_eq!(sink.target_device, "7");

        let err = sink
            .set_target("not-a-number")
            .expect_err("set_target with garbage should fail pre-validation");
        assert!(
            matches!(err, SinkError::InvalidParameter(_)),
            "expected InvalidParameter, got {err:?}",
        );

        // target_device must NOT have been touched.
        assert_eq!(
            sink.target_device, "7",
            "failed pre-validation must not disturb the previous target"
        );
    }

    #[test]
    fn set_target_is_idempotent_for_unchanged_target() {
        // Re-setting the same target should be a no-op fast path —
        // no stop/start cycle, no audible glitch, no failure surface
        // expansion. We can't observe the lack of a stop/start
        // directly here (no real AudioUnit involved on an idle
        // sink), but we can prove that the call returns Ok and
        // leaves target_device exactly equal to what was already
        // there.
        let mut sink = AudioSink::new();
        sink.set_target("42").expect("baseline");
        sink.set_target("42")
            .expect("re-setting the same target should succeed as a no-op");
        assert_eq!(sink.target_device, "42");

        // Same for the empty-string ("default device") case.
        sink.set_target("").expect("switch to default");
        sink.set_target("")
            .expect("re-setting empty should succeed as a no-op");
        assert!(sink.target_device.is_empty());
    }

    #[test]
    fn set_target_empty_string_clears_to_default() {
        // Empty string = "system default output". The pre-validation
        // path treats it as Ok(None), so set_target accepts it and
        // clears the stored target.
        let mut sink = AudioSink::new();
        sink.set_target("42").expect("baseline");
        sink.set_target("")
            .expect("empty string should resolve to default device");
        assert!(sink.target_device.is_empty());
    }

    #[test]
    fn parse_target_device_empty_means_default() {
        assert_eq!(parse_target_device("").unwrap(), None);
    }

    #[test]
    fn parse_target_device_decimal_id_round_trips() {
        assert_eq!(parse_target_device("0").unwrap(), Some(0));
        assert_eq!(parse_target_device("42").unwrap(), Some(42));
        assert_eq!(
            parse_target_device(&u32::MAX.to_string()).unwrap(),
            Some(u32::MAX)
        );
    }

    #[test]
    fn parse_target_device_rejects_garbage() {
        // Anything that isn't an empty string and isn't a decimal u32
        // surfaces InvalidParameter — both via the helper directly and
        // via open_unit when start() runs.
        for bad in ["not-a-number", "1.5", "0x42", "-1", "  ", "42abc"] {
            let err = parse_target_device(bad)
                .expect_err(&format!("expected parse_target_device({bad:?}) to fail"));
            assert!(
                matches!(err, SinkError::InvalidParameter(_)),
                "expected InvalidParameter for {bad:?}, got {err:?}"
            );
        }
    }
}
