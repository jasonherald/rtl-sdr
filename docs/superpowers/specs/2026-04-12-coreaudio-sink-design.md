---
name: CoreAudio Sink — Design
description: macOS audio output backend for sdr-sink-audio, mirroring the PipeWire implementation behind the same Sink trait
type: spec
---

# CoreAudio Sink — Design

**Status:** Draft
**Date:** 2026-04-12
**Parent epic:** `2026-04-12-swift-ui-macos-epic-design.md`
**Tracking issues:** TBD

---

## Goal

Add a CoreAudio backend to `sdr-sink-audio` so the engine can produce sound on macOS. The CoreAudio impl satisfies the existing `Sink` trait (`crates/sdr-pipeline/src/sink_manager.rs:21`), reuses the same ring-buffer pattern as the PipeWire impl, and is selected automatically on macOS via cfg gating. No changes to the trait, no changes to call sites in `sdr-core`.

Without this, the SwiftUI app cannot produce audio. Everything else in the epic depends on it.

## Non-Goals

- **No new public crate.** This is one new file inside `sdr-sink-audio`, parallel to `pw_impl.rs`.
- **No CoreAudio device routing/picker UI in v1.** The sink defaults to the system output device. Per-device routing exists in the trait surface (`set_target(node_name)`) but the SwiftUI MVP doesn't expose it. v2 adds a device picker.
- **No input capture.** Sink only. Microphone capture is irrelevant — IQ comes from RTL-SDR USB, not the audio system.
- **No format negotiation surfacing to the engine.** If the device wants 44.1 kHz instead of 48 kHz, the sink resamples internally and presents a stable 48 kHz interface upstream. The engine never sees the device rate.
- **No exclusive mode / hog mode / pro audio APIs.** AUHAL default output unit is enough; we're not chasing sub-millisecond latency.

## Background

### Current state of `sdr-sink-audio`

`crates/sdr-sink-audio/src/lib.rs` (39 lines) is a feature-gated facade:

```text
src/
├── lib.rs            — cfg dispatch
├── pw_impl.rs        — PipeWire backend (feature = "pipewire")
└── stub_impl.rs      — no-op stub when no backend feature is on
```

`Cargo.toml` declares only the `pipewire` feature. The crate description says "PipeWire (Linux) / CoreAudio (macOS)" but the CoreAudio path **does not exist** — on macOS today the build falls through to `stub_impl.rs`, which just logs a warning and discards samples.

### Why CoreAudio (and which API)

macOS exposes audio through several layers:

| API | Latency | Complexity | Fits us? |
|-----|---------|------------|----------|
| **AVAudioEngine** (high-level) | ~20 ms | Low | Tempting, but it's tied into the Objective-C runtime, has Swift idioms baked in, and is a poor citizen for a C-Rust process. Hidden allocations on the audio thread. |
| **AudioUnit / AUHAL** (mid-level) | ~3-5 ms | Medium | **Yes.** This is the C API. Stable since 10.0, statically linkable, deterministic callbacks, no hidden runtime. Mirrors how the PipeWire impl works. |
| **AudioQueue** (legacy) | ~30 ms | Low | Deprecated for new code. |
| **CoreHaptics-style HAL** (low-level IOKit) | <1 ms | Very high | Overkill for SDR audio. |

We use **AudioUnit / AUHAL output unit** (`kAudioUnitSubType_DefaultOutput`). It's the same surface SDR++ uses on macOS, the same surface most DAWs use for output, and it's straight C — no Objective-C bridging needed from Rust.

### Crate choice for the binding

Two real options for accessing CoreAudio from Rust:

1. **`coreaudio-rs`** (~0.12, well-maintained, last release 2024) — high-level wrapper around AUHAL. Pros: idiomatic, handles callback registration safely. Cons: another dep, drags in `coreaudio-sys` and bindgen build cost.
2. **`coreaudio-sys`** directly — raw bindgen output. Pros: minimal. Cons: every callback is unsafe and we'd write all the safety wrappers ourselves.

**Choice: `coreaudio-rs`.** The audio path is performance-critical but not so unusual that the high-level wrapper gets in the way, and we'd otherwise be re-implementing what `coreaudio-rs` already gets right (non-interleaved buffer handling, format struct construction, error propagation). Both crates are 1st-party from the `RustAudio` org.

## Crate Layout Change

```text
crates/sdr-sink-audio/
├── Cargo.toml                    — adds `coreaudio` feature + macOS target deps
└── src/
    ├── lib.rs                    — cfg dispatch updated
    ├── pw_impl.rs                — unchanged (Linux)
    ├── coreaudio_impl.rs         — NEW (macOS)
    └── stub_impl.rs              — unchanged (fallback)
```

`Cargo.toml`:

```toml
[features]
default = []
pipewire  = ["dep:pipewire"]
coreaudio = ["dep:coreaudio-rs"]

[dependencies]
sdr-types.workspace      = true
sdr-pipeline.workspace   = true
sdr-config.workspace     = true
thiserror.workspace      = true
tracing.workspace        = true
pipewire = { workspace = true, optional = true }

[target.'cfg(target_os = "macos")'.dependencies]
coreaudio-rs = { version = "0.12", optional = true }
```

`lib.rs`:

```rust
#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod pw_impl;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub use pw_impl::{AudioDevice, AudioSink, list_audio_sinks};

#[cfg(all(target_os = "macos", feature = "coreaudio"))]
mod coreaudio_impl;
#[cfg(all(target_os = "macos", feature = "coreaudio"))]
pub use coreaudio_impl::{AudioDevice, AudioSink, list_audio_sinks};

// Fallback for any build without a real backend (used by CI on non-Linux/non-macOS,
// or when both feature flags are disabled).
#[cfg(not(any(
    all(target_os = "linux", feature = "pipewire"),
    all(target_os = "macos", feature = "coreaudio"),
)))]
mod stub_impl;
#[cfg(not(any(
    all(target_os = "linux", feature = "pipewire"),
    all(target_os = "macos", feature = "coreaudio"),
)))]
pub use stub_impl::{AudioDevice, AudioSink, list_audio_sinks};
```

`sdr-core/Cargo.toml` enables the right feature per target:

```toml
[target.'cfg(target_os = "linux")'.dependencies]
sdr-sink-audio = { workspace = true, features = ["pipewire"] }

[target.'cfg(target_os = "macos")'.dependencies]
sdr-sink-audio = { workspace = true, features = ["coreaudio"] }
```

This is the only place the cfg fork lives. All callers see the same `AudioSink` type.

## API Parity with PipeWire Impl

The CoreAudio impl exposes **exactly** the same surface as `pw_impl.rs`:

```rust
pub struct AudioSink { /* CoreAudio-internal state */ }
pub struct AudioDevice {
    pub display_name: String,
    pub node_name: String,         // we re-purpose this for AudioObjectID hex on macOS
}

impl AudioSink {
    pub fn new() -> Self;
    pub fn set_target(&mut self, node_name: &str) -> Result<(), SinkError>;
    pub fn write_samples(&mut self, samples: &[Stereo]) -> Result<(), SinkError>;
}

impl Sink for AudioSink {
    fn name(&self) -> &str;        // returns "Audio"
    fn start(&mut self) -> Result<(), SinkError>;
    fn stop(&mut self) -> Result<(), SinkError>;
    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SinkError>;
    fn sample_rate(&self) -> f64;
}

pub fn list_audio_sinks() -> Vec<AudioDevice>;
```

The DSP controller in `sdr-core` doesn't know which backend it has. `cargo build --target aarch64-apple-darwin` produces one; `cargo build --target x86_64-unknown-linux-gnu` produces the other.

## Internal Architecture

```text
write_samples(&[Stereo])  ──▶  interleave to f32 LRLRLR
                              │
                              ▼
              ┌────────────────────────────────┐
              │  AudioRingBuffer (lock-based)  │   reused verbatim from pw_impl
              │  capacity = ~1 sec @ 48 kHz    │   (extracted into a shared module)
              └────────────────────────────────┘
                              │
                              │  AURenderCallback (CoreAudio I/O thread)
                              ▼
              ┌────────────────────────────────┐
              │  ring.read() → AudioBufferList │
              │  (with optional resample)      │
              └────────────────────────────────┘
                              │
                              ▼
                   Default output device
```

**Reuse the ring buffer.** `AudioRingBuffer` in `pw_impl.rs` (lines 36–110) is generic over interleaved f32 — it has nothing PipeWire-specific. We extract it into a private `crates/sdr-sink-audio/src/ring.rs` module shared by both backends. This is a one-line refactor in PR 1 of this work series and lets the CoreAudio impl land in PR 2 with no new ring buffer code.

**One thread, owned by CoreAudio.** Unlike PipeWire (which spawns a `pw-audio` mainloop thread we own), CoreAudio gives us a `RemoteIO`-style render callback fired from its own audio I/O thread. We don't manage the thread. The callback reads from the ring buffer and copies into the AudioUnit's `AudioBufferList`. Underrun = silence. No locks held longer than the memcpy.

**Sample-rate negotiation.** On `start()`:

1. Open the default output unit (`AUHAL`, subType `kAudioUnitSubType_DefaultOutput`).
2. Query the device's nominal sample rate via `AudioObjectGetPropertyData(kAudioDevicePropertyNominalSampleRate)`.
3. If the device rate matches the engine rate (48 kHz), set the AudioUnit's input format to 48 kHz f32 stereo non-interleaved (CoreAudio prefers non-interleaved for AUHAL output) and we're done.
4. If they differ (e.g., user has the device set to 44.1 kHz), set the AudioUnit's input format to **the device rate** and use a `coreaudio::audio_unit::render_callback::ResamplingRenderCallback` (or a hand-rolled linear interpolator — see *Open Questions* below) to convert from 48 kHz on the way in.

The engine never sees the device rate. `Sink::sample_rate()` always returns 48 kHz. This matches what `pw_impl.rs` does (PipeWire negotiates at 48 kHz unconditionally because the daemon does any required SRC).

**Volume.** AudioUnit has its own gain property (`kHALOutputParam_Volume`). We don't use it — volume is applied upstream in `RadioModule` already (via `UiToDsp::SetVolume`), so the sink just renders whatever the engine hands it. Same as the PipeWire impl.

## Public Functions Detail

### `AudioSink::new() -> Self`

- Allocates the ring buffer (96k samples = ~1 sec at 48 kHz stereo).
- Allocates the interleave buffer.
- Does **not** open the AudioUnit. That happens in `start()`.
- Mirrors `pw_impl.rs` line 229.

### `AudioSink::start() -> Result<(), SinkError>`

1. If already running, return `SinkError::AlreadyRunning`.
2. Construct an `AudioUnit` for `kAudioUnitSubType_DefaultOutput` via `coreaudio::audio_unit::AudioUnit::new(IOType::DefaultOutput)`.
3. Determine the format: query device rate, decide whether to enable internal resampling.
4. Set the input format on the unit (LinearPCM, f32, stereo, non-interleaved).
5. Register the render callback (closure capturing `Arc<AudioRingBuffer>`).
6. Initialize the unit, start it.
7. Store the `AudioUnit` and the running flag. Return `Ok`.

The render callback signature is roughly:

```rust
unit.set_render_callback(move |args: render_callback::Args<f32, NonInterleaved>| {
    let frames = args.num_frames;
    // args.data is &mut [&mut [f32]] — one slice per channel
    let mut left  = args.data[0];
    let mut right = args.data[1];

    let mut interleaved = [0.0f32; MAX_CB_FRAMES * 2];
    let to_read = frames * 2;
    let read = ring.read(&mut interleaved[..to_read]);

    for i in 0..frames {
        if i * 2 + 1 < read {
            left[i]  = interleaved[i * 2];
            right[i] = interleaved[i * 2 + 1];
        } else {
            left[i]  = 0.0;
            right[i] = 0.0;
        }
    }
    Ok(())
});
```

(Real code uses a stack-allocated `[f32; N]` sized to a known max and asserts on overflow; CoreAudio quanta are typically 256 or 512 frames.)

### `AudioSink::stop() -> Result<(), SinkError>`

1. If not running, return `SinkError::NotRunning`.
2. Call `audio_unit.stop()`.
3. Drop the `AudioUnit` (uninitializes and disposes).
4. Clear the running flag.
5. Drain the ring buffer.

### `AudioSink::set_target(node_name: &str) -> Result<(), SinkError>`

In v1: parse `node_name` as a hex `AudioObjectID`. If empty, route to the default output device. If non-empty, set `kAudioOutputUnitProperty_CurrentDevice` on the AudioUnit before initialization. Restart the unit if it was already running, just like the PipeWire impl does.

The v1 SwiftUI MVP never calls this — it always uses default output. The implementation is here so the v2 device picker has nothing to add to the sink.

### `list_audio_sinks() -> Vec<AudioDevice>`

Enumerate output devices via:

1. `AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, kAudioHardwarePropertyDevices)` to get the count.
2. `AudioObjectGetPropertyData(...)` to get the `[AudioObjectID]`.
3. For each device, query `kAudioDevicePropertyStreamConfiguration` (output scope) — keep only devices with at least one output channel.
4. For each kept device, query `kAudioObjectPropertyName` for the display name.
5. Always prepend a "Default" entry with empty `node_name`, matching the PipeWire impl's contract.

This is one short function (~60 lines). It runs synchronously on the calling thread; CoreAudio property queries are cheap (no main loop needed, unlike PipeWire's enumerate-and-quit dance).

### `Sink::write_samples(&[Stereo]) -> Result<(), SinkError>`

Identical to `pw_impl.rs` line 268: interleave into the pre-allocated buffer, write to the ring. No CoreAudio call here.

## Test Strategy

CoreAudio is hardware-touching, so the test surface is split:

**Pure unit tests** (run on every CI, no audio device required):
- `AudioRingBuffer` round-trip (already covered in `pw_impl.rs` tests; the extracted module brings them along).
- `list_audio_sinks` returns at least one entry on a macOS CI runner. The runner has a "Null Audio Output Device" by default, so this is safe.
- Format-struct construction: build the `AudioStreamBasicDescription` for 48 kHz f32 stereo and verify field-by-field. No hardware needed.

**Hardware-touching tests** (run only when `RTLSDR_TEST_HARDWARE=1` and on macOS):
- `start()` opens the default output, writes 1 second of a 1 kHz sine, `stop()` cleanly. Audible verification by the developer; CI just asserts no error returned and no panic.
- Sample-rate mismatch: temporarily switch the device to 44.1 kHz (via `audiodevice` CLI in the test helper), start the sink, verify the resampling path runs without underruns over 5 seconds.

**Behavior parity test** (cross-platform, runs on every CI):
- Build a small test harness that uses `Sink` trait methods only and runs against whichever backend the platform compiles. Verifies `start → write 5s → stop` returns no error and `sample_rate()` reports 48 kHz before and after. Run on Linux with `--features pipewire` and on macOS with `--features coreaudio`. This locks in trait-level parity.

## Risks

| Risk | Mitigation |
|------|------------|
| `coreaudio-rs` adds noticeable build time via `bindgen` | Accepted. It's a one-time cost; the alternative (raw `coreaudio-sys` + hand-rolled wrappers) is more code to own. |
| Default output device changes mid-stream (user plugs in headphones) | `kAudioHardwarePropertyDefaultOutputDevice` listener triggers a stop+restart on the sink thread. Same UX as every other macOS app. v2-quality polish — for v1 the user reselects manually if they care. |
| AURenderCallback fires before `running` flag is set | Set `running = true` *before* `audio_unit.start()`. Same ordering rule as the PipeWire impl. |
| Stack-allocated render scratch buffer overflows for unusual quanta | Assert `args.num_frames * 2 <= MAX_CB_FRAMES` in debug, fall back to a heap allocation in release. CoreAudio default output rarely exceeds 1024 frames. |
| Linking against `AudioUnit.framework` and `CoreAudio.framework` requires special build flags | `coreaudio-rs` declares them via `#[link(name = "AudioUnit", kind = "framework")]`. Verified in PR 1 spike before this design is committed. |
| Universal binary build (arm64 + x86_64) — `coreaudio-rs` cross-compiles cleanly? | Yes, the bindgen step takes the target triple from cargo. CI matrix builds both. |

## Open Questions

- **Resampler choice when device rate ≠ 48 kHz:** `coreaudio-rs` exposes `audio_unit::render_callback::Resampler`, but it's `kAudioUnitType_FormatConverter` — adds another AudioUnit in the chain. Alternative: use `sdr-dsp`'s existing rational resampler upstream, in the sink, before writing to the ring. **Lean: use `sdr-dsp` resampler** because it's already in the dependency tree, well-tested, and we control its quality settings. The format converter AU adds an opaque box.
- **What does `node_name` look like for CoreAudio devices?** Hex AudioObjectID ("0x42"), or device UID string ("BuiltInSpeakerDevice")? The UID is more stable across reboots and device-add/remove. **Lean: device UID via `kAudioDevicePropertyDeviceUID`.** AudioDevice's `node_name` field becomes the UID on macOS, the PipeWire node name on Linux. Both are caller-opaque strings.
- **Should `list_audio_sinks` include "Default" twice if the user's actual default device shows up in the enumeration?** The PipeWire impl deduplicates by node name. We do the same on macOS by checking the default device's UID against the list and skipping it from the enumerated entries (it's already represented by the "Default" entry).

## Implementation Sequencing

This work happens as **two PRs** that can land before, alongside, or after the `sdr-core` extraction (no hard ordering dependency):

### PR A — Extract shared `AudioRingBuffer` into `crates/sdr-sink-audio/src/ring.rs`

- Move the type from `pw_impl.rs` to `ring.rs`.
- Update `pw_impl.rs` to import from `crate::ring`.
- All existing tests still pass, no behavior change.
- Diff: tiny. Makes PR B reviewable in isolation.

### PR B — `coreaudio_impl.rs`

- New file, ~400 lines.
- New `coreaudio` feature + `coreaudio-rs` target dep.
- `lib.rs` cfg dispatch updated.
- Hardware-gated tests added.
- CI matrix gains a `macos-26` job that builds with `--features coreaudio` (build-only, no run, since GitHub macOS runners don't have a real audio device but the build has to succeed).

Once both land, `sdr-core` enables the right feature automatically per target_os and the rest of the epic can assume audio works on macOS.

## References

- `crates/sdr-sink-audio/src/pw_impl.rs:114-289` — the shape we're mirroring
- `crates/sdr-pipeline/src/sink_manager.rs:21-36` — `Sink` trait, unchanged
- [`coreaudio-rs` examples](https://github.com/RustAudio/coreaudio-rs/tree/master/examples) — sine-wave output, render callback patterns
- [Apple AUHAL docs](https://developer.apple.com/library/archive/technotes/tn2091/_index.html) — older but still authoritative
- `2026-04-12-sdr-core-extraction-design.md` — consumer of this sink
