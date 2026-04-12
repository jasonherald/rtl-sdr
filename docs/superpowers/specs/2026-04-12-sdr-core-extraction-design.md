---
name: sdr-core Facade Extraction — Design
description: Carve a headless `sdr-core` crate that owns the DSP controller and exposes a single Rust API consumed by both GTK and SwiftUI frontends
type: spec
---

# `sdr-core` Facade Extraction — Design

**Status:** Draft
**Date:** 2026-04-12
**Parent epic:** `2026-04-12-swift-ui-macos-epic-design.md`
**Tracking issues:** TBD

---

## Goal

Extract the engine state and lifecycle currently owned by `crates/sdr-ui/src/dsp_controller.rs` into a new headless crate, **`sdr-core`**, with a clean Rust API. After this refactor:

- `sdr-core` owns the DSP thread, the source manager, the sink manager, the IQ frontend, the radio module, and the message channels.
- `sdr-ui` (GTK) becomes a *consumer* of `sdr-core` instead of a containing host. Its `dsp_controller.rs` shrinks to a thin GTK glue layer that subscribes to events on the GTK main loop.
- A second consumer (`sdr-ffi` for SwiftUI) can be added in a follow-up PR series **without touching `sdr-ui`** because both frontends sit on the same crate.

This is a **behavior-preserving refactor**. No new features, no observable behavior change for GTK users. `cargo test --workspace` and clippy stay green throughout.

## Non-Goals

- **No FFI in this PR series.** The C ABI lives in a separate `sdr-ffi` crate, designed in `2026-04-12-sdr-ffi-c-abi-design.md`. `sdr-core` is pure Rust.
- **No message enum redesign.** Variant names, fields, and semantics stay byte-identical to today's `UiToDsp`/`DspToUi`. The enums physically move from `sdr-ui` to `sdr-core` and re-export to `sdr-ui` so its existing match arms compile unchanged.
- **No new crate boundaries below `sdr-core`.** `sdr-pipeline`, `sdr-radio`, `sdr-dsp`, etc. are unchanged. We're moving one file's worth of orchestration up into a new crate, not reshuffling the workspace.
- **No transcription in `sdr-core`.** The transcription backend trait is being rebuilt in a parallel session. We leave transcription wired through `sdr-ui` exactly as it is today (UI-side `EnableTranscription(SyncSender<Vec<f32>>)` channel still works) and revisit after that session declares stability. See *Risks* below.
- **No async runtime.** Engine remains thread-based with `mpsc` channels internally. The FFI layer will translate to async on the Swift side; `sdr-core` itself stays sync to keep both consumers honest.

## Background

### Where the orchestration lives today

`crates/sdr-ui/src/dsp_controller.rs` (1230 lines) is the de-facto engine entry point. It:

1. Spawns the `dsp-controller` background thread (`spawn_dsp_thread`).
2. Owns `RtlSdrSource`, `IqFrontend`, `RxVfo`, `RadioModule`, `AudioSink`, `WavWriter`s, the active source enum, all DSP config state, and the FFT shared buffer.
3. Polls `ui_rx: mpsc::Receiver<UiToDsp>` for commands, reads IQ samples, processes the signal path, writes audio, and pushes FFT frames into `SharedFftBuffer`.
4. Sends status events back over `dsp_tx: mpsc::Sender<DspToUi>`.

The GTK side (`crates/sdr-ui/src/window.rs`) creates the channels, calls `spawn_dsp_thread`, then registers a GLib timeout that reads `dsp_rx` and pumps events into widgets.

### Why this is the wrong place for it

- **It's not GTK-specific.** Nothing in `dsp_controller.rs` touches GTK. The only reason it lives in `sdr-ui` is historical — that's where the channels were first created.
- **A second frontend can't import it.** A SwiftUI host would need to either depend on `sdr-ui` (which pulls in GTK4 + libadwaita), or duplicate the orchestration. Both are wrong.
- **The test surface is mixed.** Engine logic and UI glue share a crate, so engine tests pull `gtk4` into `dev-dependencies`.

### Why a *facade* crate, not a feature flag on `sdr-pipeline`

Considered: add a `controller` module to `sdr-pipeline` and gate it behind a feature. Rejected because:
- `sdr-pipeline` deliberately stays at "pipeline primitives" — `Source`/`Sink` traits, `IqFrontend`, `RxVfo`, channel manager. It doesn't own a thread, and we like that.
- The orchestration crate also needs to depend on `sdr-radio`, `sdr-source-rtlsdr`, `sdr-source-network`, `sdr-source-file`, `sdr-sink-audio`, and `sdr-sink-network`. Putting that dependency tree on `sdr-pipeline` would invert the layering.
- A separate `sdr-core` keeps `sdr-pipeline` clean for any future tooling that wants to consume primitives without the whole engine.

## Crate Layout

```text
crates/sdr-core/
├── Cargo.toml
└── src/
    ├── lib.rs              — public API surface, re-exports
    ├── engine.rs           — Engine struct: lifecycle (new, start, stop, send_command, subscribe)
    ├── controller.rs       — DSP thread loop (moved verbatim from sdr-ui/src/dsp_controller.rs)
    ├── messages.rs         — UiToDsp / DspToUi enums (moved from sdr-ui)
    ├── fft_buffer.rs       — SharedFftBuffer (moved from sdr-ui)
    ├── wav_writer.rs       — WavWriter (moved from sdr-ui — used by controller for recordings)
    ├── source_factory.rs   — registers RTL-SDR / network / file sources into SourceManager
    └── sink_factory.rs     — registers audio + network sinks into SinkManager
```

**`crates/sdr-core/Cargo.toml` dependencies:**

```toml
[dependencies]
sdr-types.workspace = true
sdr-config.workspace = true
sdr-dsp.workspace = true
sdr-pipeline.workspace = true
sdr-radio.workspace = true
sdr-rtlsdr.workspace = true
sdr-source-rtlsdr.workspace = true
sdr-source-network.workspace = true
sdr-source-file.workspace = true
sdr-sink-audio.workspace = true
sdr-sink-network.workspace = true
thiserror.workspace = true
tracing.workspace = true
hound.workspace = true
```

**`crates/sdr-ui/Cargo.toml` after the move:**

```toml
[dependencies]
sdr-core = { path = "../sdr-core" }   # NEW
# removed: direct deps on sdr-pipeline, sdr-radio, sdr-source-rtlsdr,
#          sdr-source-network, sdr-source-file, sdr-sink-audio, sdr-sink-network,
#          hound (those are now transitive via sdr-core)
# kept: sdr-types, sdr-config, sdr-dsp (still touched by GTK plotting code)
gtk4 = "..."
adw = "..."
# ...rest of GTK stack unchanged
```

## Public API

The `Engine` struct is the entire public surface. It is `Send + Sync` so consumers can hold it in an `Arc` and call commands from any thread.

`std::sync::mpsc::Receiver<T>` is `Send` but **not** `Sync`, so the receiver can't sit naked in a field on a `Sync` type. We wrap it in `Mutex<Option<...>>`: the option lets `subscribe` *take* the receiver out (it's a one-shot — only one consumer drains it), and the mutex satisfies `Sync` while the option is still occupied. After `subscribe` returns the receiver to the caller, all subsequent calls return `None`.

```rust
// crates/sdr-core/src/engine.rs

use std::sync::{Arc, Mutex, mpsc};
use crate::fft_buffer::SharedFftBuffer;
use crate::messages::{DspToUi, UiToDsp};

/// The headless SDR engine. One instance per app. Send + Sync — consumers
/// can hold it in an Arc and dispatch commands from any thread.
pub struct Engine {
    cmd_tx: mpsc::Sender<UiToDsp>,
    /// One-shot subscription slot. `subscribe()` takes the receiver out.
    /// Wrapped in Mutex<Option<...>> because mpsc::Receiver is !Sync.
    evt_rx: Mutex<Option<mpsc::Receiver<DspToUi>>>,
    fft: Arc<SharedFftBuffer>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Engine {
    /// Build, register sources/sinks, spawn the DSP thread, return.
    /// Does NOT start the source — call `send_command(UiToDsp::Start)` for that.
    ///
    /// `config_path` is where settings are loaded from at boot and persisted to.
    /// On macOS this is typically `~/Library/Application Support/SDRMac/config.json`;
    /// on Linux it's `$XDG_CONFIG_HOME/sdr-rs/config.json`. The host decides.
    pub fn new(config_path: std::path::PathBuf) -> Result<Self, EngineError> { ... }

    /// Send a command to the DSP thread. Non-blocking. Safe from any thread.
    pub fn send_command(&self, cmd: UiToDsp) -> Result<(), EngineError> { ... }

    /// Take the event receiver. Returns `Some(_)` exactly once per Engine;
    /// every subsequent call returns `None`. The caller chooses how to drain
    /// it (GTK timeout, FFI dispatcher thread, async task, …).
    pub fn subscribe(&self) -> Option<mpsc::Receiver<DspToUi>> {
        self.evt_rx.lock().ok()?.take()
    }

    /// Pull a snapshot of the latest FFT frame, if a new one is ready since
    /// the last call. Lock-free check; locks only when reading the buffer.
    /// Returns false and doesn't call `f` if no new frame.
    pub fn pull_fft<F: FnOnce(&[f32])>(&self, f: F) -> bool {
        self.fft.take_if_ready(f)
    }

    /// Block-and-stop. Idempotent. Joins the DSP thread; safe to call from
    /// any thread, but NOT from within the event-receiver loop (would deadlock).
    pub fn shutdown(&self) -> Result<(), EngineError> { ... }
}

// SAFETY: All fields are Send + Sync:
//  - mpsc::Sender<T> is Send + Sync
//  - Mutex<Option<...>> is Send + Sync when T: Send
//  - Arc<SharedFftBuffer> is Send + Sync (SharedFftBuffer uses internal Mutex+Atomic)
// So Engine: Send + Sync is auto-derived; this comment exists to justify the
// public claim if a future field threatens it.
```

**Event flow remains channel-based.** `subscribe()` hands the receiver to whoever wants it: GTK polls it from a `glib::timeout_add_local`; the FFI crate spawns its own dispatcher thread that calls a C callback. Neither model is hard-coded into `sdr-core`.

**FFT delivery is pull-based.** The DSP thread writes into `SharedFftBuffer` and sets a flag; the consumer drains via `pull_fft`. This is exactly how `dsp_controller.rs` works today (lines 77–111), just relocated. It avoids the per-frame `Vec<f32>` allocation that the `DspToUi::FftData` channel variant would otherwise cause.

> **Note on `DspToUi::FftData`:** the variant stays in the enum for backward compatibility but `sdr-core` *does not emit it*. FFT data goes through `pull_fft` exclusively. This is consistent with what `dsp_controller.rs` already does (the channel-based path was deprecated in favor of `SharedFftBuffer`). The variant gets removed in a follow-up after both consumers confirm they don't need it.

## Migration Plan

The extraction is done in **three sequential PRs** so each one is small and reviewable. CodeRabbit will see narrow diffs.

### PR 1 — Create `sdr-core` skeleton (no behavior change yet)

- Add `crates/sdr-core/` to the workspace.
- Move `crates/sdr-ui/src/messages.rs` → `crates/sdr-core/src/messages.rs`. Re-export from `sdr-ui` so `crate::messages::UiToDsp` still resolves.
- Move `crates/sdr-ui/src/wav_writer.rs` → `crates/sdr-core/src/wav_writer.rs`. Same re-export.
- Move `SharedFftBuffer` from `dsp_controller.rs` → `crates/sdr-core/src/fft_buffer.rs`. Re-export.
- `sdr-core` exposes nothing else yet.
- `sdr-ui` adds `sdr-core` as a dependency.
- **Acceptance:** `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --all-targets --workspace -- -D warnings` all pass with zero source changes outside the moved files and the new re-exports.

### PR 2 — Move the controller into `sdr-core`

- Move `crates/sdr-ui/src/dsp_controller.rs` → `crates/sdr-core/src/controller.rs`.
- Add `crates/sdr-core/src/engine.rs` with the `Engine` struct as specified above. `Engine::new` does what `Window::new` does today: creates channels, builds the `SharedFftBuffer`, registers sources via the source factory, registers sinks via the sink factory, calls `spawn_dsp_thread`.
- Add `crates/sdr-core/src/source_factory.rs` and `sink_factory.rs` — pure functions that build a `SourceManager`/`SinkManager` populated with the project's stock sources and sinks.
- `sdr-ui/src/dsp_controller.rs` becomes a stub that re-exports `sdr_core::Engine` and adds the GTK-specific event-pump glue (the `glib::timeout_add_local` loop that drains the event channel and calls into `Window`).
- `sdr-ui/src/window.rs` constructs an `Engine` instead of calling `spawn_dsp_thread` directly. The `cmd_tx`/`evt_rx`/`fft_shared` fields are replaced by an `Arc<Engine>`.
- **Acceptance:** the GTK app launches, tunes a station, demods audio, FFT updates render, recording start/stop works, source switching works, and `make lint` is clean.

### PR 3 — Carve out transcription glue

- Transcription is currently wired through `UiToDsp::EnableTranscription(SyncSender<Vec<f32>>)`. The DSP controller forwards demod samples into that sender. The sender lifecycle is currently owned by `sdr-ui/src/window.rs`.
- Because the parallel session is reshaping `sdr-transcription`, we don't refactor it into `sdr-core` here. We just confirm the existing variant continues to work after PRs 1+2: GTK creates the `SyncSender`, sends it through the engine's command channel, the controller forwards audio to it. **Zero functional change.**
- **Acceptance:** transcript panel still emits text on the GTK side. Add a regression test that constructs an engine, sends `EnableTranscription` with a test sender, and asserts samples arrive.

After these three PRs land, `sdr-ui` is structurally a "consumer" and the `sdr-ffi` PR series can begin in parallel without touching GTK code.

## Threading Model (unchanged)

Identical to today, just owned by a different crate:

```text
                        ┌─────────────────┐
   host UI thread ─────▶│  Engine.send_   │──▶ mpsc ──▶ ┌──────────────┐
   (GTK / SwiftUI)      │  command()      │            │  DSP thread  │
                        └─────────────────┘            │              │
                                                       │ • SourceMgr  │
                        ┌─────────────────┐            │ • IqFrontend │
   host UI thread ◀────│ subscribed       │◀── mpsc ──│ • RxVfo      │
                        │ DspToUi receiver│            │ • RadioModule│
                        └─────────────────┘            │ • AudioSink  │
                                                       │ • WavWriters │
                        ┌─────────────────┐            └──────┬───────┘
   host render tick ───▶│ Engine.pull_fft │────────────────────┘
   (GTK / MTKView)      │                 │   (SharedFftBuffer)
                        └─────────────────┘
```

The DSP thread is the sole writer of engine state. All hosts go through the command channel; no host ever holds a reference to the controller. This is what makes the FFI safe later — there is exactly one mutex-free path into the engine.

## Test Strategy

- **Unit tests for `Engine`:** construct an engine with a `MockSource` (already exists in `source_manager.rs` tests), drive it with a sequence of commands, assert events arrive on the receiver. These tests do *not* touch GTK or any UI code.
- **Regression tests:** every test currently in `sdr-ui/src/dsp_controller.rs`'s `#[cfg(test)] mod tests` moves to `sdr-core/src/controller.rs` and must continue to pass without modification.
- **Behavior-preservation gate for PR 2:** before merging, manually launch the GTK app and walk through this checklist (the same set as the transcription-trait refactor):
  - Tune to FM 100.7 → audio
  - Switch to network source → no panic, error surfaces in UI
  - Start IQ recording → file written, stop → finalized
  - Click on the waterfall → VFO moves, audio retunes
  - Quit cleanly (no thread leaks visible in `Activity Monitor`)

## Risks

| Risk | Mitigation |
|------|------------|
| Parallel sherpa-onnx work edits `dsp_controller.rs` mid-extraction | Land PR 1 within a day of starting. Coordinate via GitHub: open the extraction PRs first, then ask the other session to rebase. The transcription work touches `sdr-transcription` primarily, so the merge surface in `dsp_controller.rs` is small. |
| Re-export gymnastics confuse `cargo doc` / IDE go-to-def | Re-exports are one line each; we accept the tradeoff for the one-PR window where both old and new paths resolve. After PR 2, GTK callers update to the canonical `sdr_core::messages::*` paths and re-exports go away. |
| `Engine::subscribe` returning `Option<Receiver>` is awkward for callers that want a "broadcast" model later | Acceptable for v1: GTK is a single subscriber, FFI is a single subscriber, and they live in different processes/builds. If we ever need multi-subscriber, replace `mpsc::Receiver` with a `tokio::sync::broadcast` or hand-rolled fan-out. Out of scope here. |
| `sdr-core` accidentally pulls in `pipewire` on macOS through `sdr-sink-audio` | `sdr-sink-audio` is feature-gated (`pipewire` feature off by default). The CoreAudio sink design (`2026-04-12-coreaudio-sink-design.md`) keeps this gate intact and adds a `coreaudio` feature on the same crate. `sdr-core` enables one or the other via target_os cfg. |

## References

- `crates/sdr-ui/src/dsp_controller.rs` — current orchestration to be moved
- `crates/sdr-ui/src/messages.rs` — message enums to be moved
- `crates/sdr-ui/src/window.rs:1-200` — current channel construction site to be replaced with `Engine::new`
- `crates/sdr-pipeline/src/source_manager.rs` — `Source` trait, unchanged
- `crates/sdr-pipeline/src/sink_manager.rs` — `Sink` trait, unchanged
- `docs/superpowers/specs/2026-04-12-sdr-ffi-c-abi-design.md` — what consumes `sdr-core` next
