# Transcription Backend Trait + sherpa-onnx Integration — Design

**Status:** Draft
**Date:** 2026-04-12
**Tracking issues:** [#204](https://github.com/jasonherald/rtl-sdr/issues/204) (parent), [#223](https://github.com/jasonherald/rtl-sdr/issues/223) (Parakeet), [#224](https://github.com/jasonherald/rtl-sdr/issues/224) (Moonshine), [#225](https://github.com/jasonherald/rtl-sdr/issues/225) (Zipformer)

## Goal

Refactor `sdr-transcription` so the transcription engine is no longer hardwired to Whisper. Introduce a `TranscriptionBackend` trait that abstracts the underlying ASR implementation, then add a second backend powered by `sherpa-onnx` to enable genuinely streaming live transcription.

This sets the project up so that adding a new ASR model — or a future cloud API — is a drop-in module, not an architectural change.

## Non-Goals

- Replacing Whisper. The existing `whisper-rs` backend stays. Users keep their downloaded models and existing workflow.
- Police 10-code interpretation, NAED codes, phonetic decoding — separate future issues.
- Multi-language support. English-only for now.
- Cloud transcription backends (OpenAI, AssemblyAI, Deepgram). The trait makes them trivial to add later.
- Replacing the FFT-based spectral denoiser with sherpa's VAD as part of *this* work. Worth evaluating later (sherpa ships a `VoiceActivityDetector`), but out of scope for this design.

## Background

### Why we dropped Cohere Transcribe

The earlier plan called for integrating `cohere_transcribe_rs` as a second backend. On investigation it turned out to be a CLI/server crate, not a library — `transcribe()` takes raw `tch::Tensor` plus separate encoder/decoder/tokenizer objects, with no high-level API. It hard-depends on libtorch (~2 GB), is CUDA-only practically, and the model is gated on HuggingFace requiring manual login. Most importantly, **Cohere Transcribe is a chunked file processor like Whisper**. Integrating it would solve none of the live-streaming problems we actually have. See the rewritten parent issue #204 for the full reasoning.

### Why sherpa-onnx

`sherpa-onnx` (k2-fsa / Next-Gen Kaldi) is a runtime that wraps many modern ASR models behind one C API: streaming Zipformer, streaming Paraformer, Moonshine, NVIDIA Parakeet, Whisper, SenseVoice, and more. The official Rust crate `sherpa-onnx` (1.12.38, first-party from k2-fsa) exposes:

- `online_asr` — streaming ASR with proper endpoint detection (frame-by-frame partial hypotheses, silence-based commit)
- `offline_asr` — chunked ASR (Whisper-style)
- `vad`, `resampler`, `wave` — utility modules we can adopt incrementally
- Future hooks: `kws` (keyword spotting — radio call signs), `speaker_embedding`, `audio_tagging`

The crate links sherpa-onnx statically by default and the underlying `sherpa-onnx-sys` build script handles libsherpa archive download (bzip2 + tar + ureq). This means **our build doesn't need to drag in libtorch or any system-wide native deps** the way Cohere Transcribe would.

## High-Level Architecture

```text
sdr-transcription/
├── Cargo.toml                — adds `sherpa-onnx` dep (mandatory, no feature flag)
├── src/
│   ├── lib.rs                — TranscriptionEngine, public API
│   ├── backend.rs            — TranscriptionBackend trait, BackendConfig, events
│   ├── backends/
│   │   ├── mod.rs            — pub use whisper, sherpa
│   │   ├── whisper.rs        — current worker.rs refactored behind trait
│   │   └── sherpa.rs         — new sherpa-onnx implementation
│   ├── denoise.rs            — unchanged (shared preprocessor)
│   ├── resampler.rs          — unchanged
│   └── model.rs              — extended: WhisperModel + SherpaModel enums + BackendKind
```

The `TranscriptionEngine` becomes a thin owner of one active backend at a time. When the user switches backends (or models, or restarts), the engine drops the current backend instance and constructs a new one. This keeps lifecycle simple and avoids any "hot swap" complexity.

### Threading model

Each backend owns its own internal worker thread(s). The engine does not spawn threads — it delegates start/stop to the backend. The trait surface gives the engine an audio sender and an event receiver; it doesn't care how the backend uses them internally.

```text
DSP thread  ──audio_tx──> Backend internal worker(s)  ──event_tx──> UI thread
                          (owned by Backend impl)
```

This is important because the two backend families have very different threading needs:

- **Whisper backend** accumulates audio in a ring buffer, periodically pulls 5-second chunks, runs blocking `whisper.cpp` inference. One worker thread is enough.
- **Sherpa backend** has its own internal feed/poll loop driven by the `online_asr::OnlineRecognizer` API. Audio is fed frame-by-frame; partial hypotheses come back from the recognizer state, not from a separate worker callback. Likely one feeder thread + one event-emitter loop.

By making each backend own its threads, we don't try to force both into one shape.

## Trait Design

```rust
// crates/sdr-transcription/src/backend.rs

use std::sync::mpsc;

/// Configuration handed to a backend at construction time.
/// Backend-specific fields are gated by which variant is in use.
pub struct BackendConfig {
    pub model: ModelChoice,
    pub silence_threshold: f32,
    pub noise_gate_ratio: f32,
}

/// User-facing model selection. The variant determines which backend
/// the engine will instantiate.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum ModelChoice {
    Whisper(WhisperModel),
    Sherpa(SherpaModel),
}

/// Transcription events emitted by a backend.
#[derive(Debug, Clone)]
pub enum TranscriptionEvent {
    /// In-progress hypothesis. May be replaced by another `Partial`
    /// before being promoted to `Final`. Streaming backends only.
    Partial { text: String },

    /// Committed transcription. Whisper emits these directly per chunk.
    /// Streaming backends emit these on endpoint detection.
    Final {
        text: String,
        timestamp: String, // HH:MM:SS local time
    },

    /// Backend ready to receive audio (after model load).
    Ready,

    /// Backend hit a fatal error and shut down.
    Error { message: String },
}

/// A handle returned by `Backend::start` that the engine wires through to the UI.
pub struct BackendHandle {
    pub audio_tx: mpsc::SyncSender<Vec<f32>>,
    pub event_rx: mpsc::Receiver<TranscriptionEvent>,
}

pub trait TranscriptionBackend: Send {
    /// Human-readable backend name for the UI.
    fn name(&self) -> &'static str;

    /// True if this backend can emit `Partial` events.
    /// Used by the UI to enable/disable the live captions toggle.
    fn supports_partials(&self) -> bool;

    /// Spawn worker threads and begin accepting audio.
    /// Must emit `TranscriptionEvent::Ready` once the model is loaded.
    fn start(&mut self, config: BackendConfig) -> Result<BackendHandle, TranscriptionError>;

    /// Signal the worker to stop without blocking.
    /// Drops the audio sender and detaches threads.
    fn shutdown_nonblocking(&mut self);
}
```

### Why a trait, not an enum

Could go either way. Trait wins because:

1. The two backends have radically different internal state — `WhisperContext` vs `OnlineRecognizer`. Wrapping them in an enum would either expose both lifetimes or require boxing anyway.
2. Backends are constructed once, used for the lifetime of one transcription session, and dropped. There's no hot dispatch pressure where dynamic dispatch would matter.
3. Adding a third backend (cloud API, etc.) requires touching only the new file plus a one-line registration. With an enum we'd touch the enum definition, every match site, and the error type.

### Engine becomes a thin owner

```rust
// crates/sdr-transcription/src/lib.rs

pub struct TranscriptionEngine {
    backend: Option<Box<dyn TranscriptionBackend>>,
    audio_tx: Option<mpsc::SyncSender<Vec<f32>>>,
}

impl TranscriptionEngine {
    pub fn start(&mut self, config: BackendConfig) -> Result<mpsc::Receiver<TranscriptionEvent>, TranscriptionError> {
        if self.backend.is_some() {
            return Err(TranscriptionError::AlreadyRunning);
        }
        let mut backend: Box<dyn TranscriptionBackend> = match config.model {
            ModelChoice::Whisper(_) => Box::new(backends::whisper::WhisperBackend::new()),
            ModelChoice::Sherpa(_) => Box::new(backends::sherpa::SherpaBackend::new()),
        };
        let BackendHandle { audio_tx, event_rx } = backend.start(config)?;
        self.audio_tx = Some(audio_tx);
        self.backend = Some(backend);
        Ok(event_rx)
    }

    pub fn audio_sender(&self) -> Option<mpsc::SyncSender<Vec<f32>>> {
        self.audio_tx.clone()
    }

    pub fn shutdown_nonblocking(&mut self) {
        self.audio_tx.take();
        if let Some(mut backend) = self.backend.take() {
            backend.shutdown_nonblocking();
        }
    }

    pub fn supports_partials(&self) -> bool {
        self.backend.as_ref().is_some_and(|b| b.supports_partials())
    }
}
```

The engine's public API is **almost identical to today's** — `start`, `stop`, `audio_sender`, `is_running`. The only new method is `supports_partials()`, used by the UI to grey out the display-mode toggle when Whisper is active. **The DSP controller and UI window code do not need any changes** beyond the new toggle.

## Backend Implementations

### `backends/whisper.rs` — refactor of current worker

Direct port of `worker.rs` behind the trait. No behavior changes. `WhisperBackend::start()` spawns the existing worker thread, returns a `BackendHandle` carrying the existing channels.

`supports_partials()` returns `false`. Whisper only emits `Final` events.

The existing `is_hallucination()` filter and `[Music]`/`thank you` skip stay in place, scoped to this backend.

### `backends/sherpa.rs` — new streaming backend

Constructs an `OnlineRecognizer` from the selected `SherpaModel`. Runs a feed loop:

```text
loop {
    sample_chunk = audio_rx.recv_timeout(...)?;
    if cancelled { break; }

    // resample 48k stereo → 16k mono (or use sherpa's resampler)
    stream.accept_waveform(16000, &mono_16k);

    while recognizer.is_ready(&stream) {
        recognizer.decode_stream(&stream);
    }

    let partial = recognizer.get_result(&stream);
    if !partial.is_empty() {
        event_tx.send(TranscriptionEvent::Partial { text: partial })?;
    }

    if recognizer.is_endpoint(&stream) {
        let final_text = recognizer.get_result(&stream);
        event_tx.send(TranscriptionEvent::Final { text: final_text, timestamp })?;
        recognizer.reset(&stream);
    }
}
```

(Pseudocode — actual API names will be confirmed during the spike step.)

`supports_partials()` returns `true`.

## Model Auto-Download

Sherpa models are multi-file bundles distributed as `.tar.bz2` archives on the [k2-fsa sherpa-onnx model release pages](https://github.com/k2-fsa/sherpa-onnx/releases). Each bundle typically contains `encoder.onnx`, `decoder.onnx`, `joiner.onnx`, and `tokens.txt`.

We mirror the existing Whisper auto-download UX:

1. User picks a Sherpa model from the dropdown
2. Engine checks `~/.local/share/sdr-rs/models/sherpa/<model_name>/` for an `encoder.onnx` (sentinel file)
3. If absent: download `<model_url>` to `<model_name>.tar.bz2.part`, extract to a temp dir, atomic rename to final location
4. Toast progress via the existing notification system

New deps: `tar` + `bzip2-rs` (both pure Rust, tiny).

The existing `model.rs` extends with a `SherpaModel` enum mirroring the `WhisperModel` shape:

```rust
pub enum SherpaModel {
    StreamingZipformerEn,
    ParakeetTdt06bV2,
    MoonshineTiny,
    MoonshineBase,
}

impl SherpaModel {
    pub fn label(&self) -> &'static str { ... }
    pub fn archive_url(&self) -> &'static str { ... }
    pub fn directory_name(&self) -> &'static str { ... }
}
```

Initial implementation lands `StreamingZipformerEn` only (#225 — proven, lowest risk). Parakeet (#223) and Moonshine (#224) follow as separate PRs.

## UI Changes

Two new controls in the transcript panel, persisted in config alongside the existing model and slider settings:

1. **Backend selector** (`AdwComboRow`): "Whisper" / "Sherpa (streaming)"
2. **Display mode toggle** (`AdwSwitchRow`): "Live captions" (default for Sherpa) / "High accuracy" (default for Whisper)
   - Greyed out when `engine.supports_partials() == false`
   - Tooltip explains: "Whisper only emits final transcriptions"

Existing model picker becomes contextual: shows `WhisperModel::ALL` when Whisper backend is selected, `SherpaModel::ALL` when Sherpa is selected.

### Live captions rendering

When display mode is "Live captions", the transcript panel splits into:

- **Committed history** — append-only `GtkTextView`, scrollback log of all `Final` events, exactly like today
- **Live line** — single `GtkLabel` below the history that updates in place from `Partial` events. Cleared on `Final`. Italicized + dimmed to visually distinguish "in-progress" from "committed."

When display mode is "High accuracy", the live line is hidden, only history is shown. `Partial` events are dropped on the floor in the UI handler. Final-only behavior is identical to today's Whisper experience.

### Config persistence

New keys in `ConfigManager`:

- `transcription.backend` — `"whisper"` or `"sherpa"`
- `transcription.sherpa_model` — variant name
- `transcription.display_mode` — `"live"` or `"final"`

Existing keys (`transcription.model`, `transcription.silence_threshold`, `transcription.noise_gate`) remain.

## Implementation Phases

Each phase is its own PR. Each gets manual user testing before merge per project workflow.

### PR 1 — Trait + Whisper refactor (`feature/transcription-backend-trait`)

**Scope:** Pure refactor. No new behavior, no UI changes.

- Add `backend.rs` with trait and event types
- Move `worker.rs` to `backends/whisper.rs`, refactor behind trait
- Update `lib.rs` `TranscriptionEngine` to delegate to a `Box<dyn TranscriptionBackend>`
- Add `supports_partials()` to engine API (returns `false` from Whisper)
- Verify existing flow still works: model picker, transcription start/stop, sliders, persistence

**Acceptance:** end-to-end Whisper transcription works on a real radio source, identical to current behavior.

### PR 2 — Sherpa spike (`feature/transcription-sherpa-spike`)

**Scope:** Wire up `sherpa-onnx` crate, prove the streaming flow works end-to-end with one model.

- Add `sherpa-onnx = "1.12"` to `sdr-transcription` Cargo.toml
- Create `backends/sherpa.rs` skeleton
- Stand up `SherpaBackend` with hardcoded Streaming Zipformer model path (manual download for now)
- Wire feed loop, partial/final event emission
- Add temporary "Sherpa" backend option to UI for testing
- Validate on real radio audio (CPU first, then CUDA if the build allows)
- **Document CUDA story:** investigate during this PR whether the official `sherpa-onnx` crate supports CUDA via shared linking, env vars, or whether we need to bring back a feature flag

**Acceptance:** can flip a radio on, switch to Sherpa backend, and see live partials + final lines stream into the panel.

### PR 3 — Model auto-download for Sherpa bundles

**Scope:** Match Whisper's "click model, get model" UX.

- Add `tar` + `bzip2-rs` deps
- Implement archive download → extract → atomic rename in `model.rs`
- Hook download progress into existing toast notification system
- Add `SherpaModel::StreamingZipformerEn` as the only initial variant

**Acceptance:** fresh install → pick "Streaming Zipformer" → bundle downloads → transcription works without manual intervention.

### PR 4 — UI polish: backend selector + display mode toggle

**Scope:** Finalize the UI surface.

- Backend selector ComboRow
- Display mode toggle (greyed out for Whisper)
- Live captions two-line rendering when display mode is "live"
- Config persistence for backend + display mode + sherpa model
- README transcription section update

**Acceptance:** Whisper users see no behavioral change. Sherpa users see live partials when display mode is "live", clean final-only when "high accuracy". All settings persist across restarts.

### PR 5+ — Additional Sherpa models

One PR per model, tracked in #223 (Parakeet), #224 (Moonshine). Each adds a `SherpaModel` enum variant and the download URL. Should be near-trivial after PR 3 lands the download infrastructure.

## Open Questions

1. **CUDA acceleration for sherpa-onnx.** The deprecated `sherpa-rs` crate exposed a `cuda` feature flag, but the official `sherpa-onnx` crate's Cargo.toml only shows `static`/`shared`. Need to investigate during PR 2:
   - Does the upstream sherpa-onnx C library auto-detect CUDA at build time?
   - Is shared linking against a system-installed `libsherpa-onnx-cuda` the path?
   - Do we need an env var like `SHERPA_ONNX_USE_CUDA=1` passed through to the cmake build?

   This is the only blocking unknown. If CUDA turns out to require a custom build path, we may need to add a `sherpa-cuda` feature flag to `sdr-transcription` mirroring the existing Whisper GPU flags.

2. **Sherpa VAD vs our spectral denoiser.** Sherpa ships a `VoiceActivityDetector`. Worth A/B testing once the basic backend is working — could replace our FFT-based gate. Not in scope for these PRs but flagged for future consideration.

3. **Resampler choice.** Sherpa has its own `Resampler` module. Our existing `resampler.rs` works; we should pick one and stick with it for consistency. Lean: keep ours, since it's already in the audio path.

## Risks

- **Build complexity:** the `sherpa-onnx-sys` build script downloads a precompiled libsherpa archive on first build. CI build times go up; first-time clones get a one-time delay. Mitigation: document in README, maybe add a build cache hint.
- **Binary size:** static linking sherpa-onnx adds ~50 MB to the binary. User has explicitly accepted this tradeoff (compile both backends, simpler beats smaller).
- **Whisper backend regression:** PR 1 is a pure refactor but moves a lot of code. Mitigation: keep PR 1 strictly behavior-preserving, no opportunistic cleanups, manual end-to-end test before merge.
- **Sherpa model download URLs may change.** Mitigation: pin to specific release tags, not "latest" links.

## Verification Plan

After each PR:

- `cargo build --workspace` clean
- `cargo clippy --all-targets --workspace -- -D warnings` clean
- `cargo test --workspace` passes (existing test coverage)
- `cargo fmt --all -- --check` clean
- Manual end-to-end test on real radio source
- CodeRabbit review, all inline comments addressed

After PR 4 (full integration):

- Whisper-only flow unchanged (regression check)
- Sherpa flow with Streaming Zipformer works on CPU and CUDA (if PR 2 confirmed CUDA path)
- Backend switching mid-session works without crashes
- Config persistence verified across app restart for: backend, both model picks, display mode, sliders
- Live captions visually behave as expected (partials update in place, final line promotes to history)
- High accuracy mode shows no live line, identical scrollback to Whisper

## Memory + Documentation Updates

After PR 4 lands:

- Update `project_transcription.md` memory file
- Update `project_current_state.md` memory file
- README transcription section: replace "Live speech-to-text via Whisper" with the new dual-backend description, mention live captions feature
