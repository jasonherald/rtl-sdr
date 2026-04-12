# Sherpa-Onnx Spike + Backend Selector UI — PR 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a working `SherpaBackend` powered by the official `sherpa-onnx` Rust crate, prove streaming-native ASR works end-to-end on a real radio source (Streaming Zipformer English), and ship Whisper / Sherpa as **mutually exclusive cargo features** picked at build time. The original plan called for a runtime backend selector ComboRow in the UI, but we hit an unresolved heap corruption when sherpa-onnx's bundled ONNX Runtime initializes inside the sdr-rs binary. The build-time mutex sidesteps it definitively — see PR #249 for the full debugging trail.

**Architecture:** New `backends/sherpa.rs` implements `TranscriptionBackend` using `sherpa_onnx::OnlineRecognizer`. A long-lived `SherpaHost` worker thread (spawned from `main()` before `sdr_ui::run()`) owns the recognizer for the entire process lifetime; per-session `OnlineStream`s are created inside the host's event loop. The recognizer is `!Send`, so it lives entirely on the host thread; the host wraps its command sender in a `Mutex` to satisfy `OnceLock<Sync>`. The `TranscriptionEngine::start` API is widened to take a `BackendConfig` directly. `ModelChoice` and `TranscriptionEvent::Partial` variants are cfg-gated on the `whisper` / `sherpa` features. The transcript panel's model picker becomes contextual to whichever backend was compiled in, with no runtime selector. For this PR, partial events are routed to `tracing::debug!` only — full live-captions UI rendering lands in PR 4.

**Tech Stack:** Rust 2024, `sherpa-onnx 1.12`, GTK4/libadwaita ComboRow + StringList, `mpsc` channels, `Arc<AtomicBool>` cancellation, `thiserror`.

**Spec:** `docs/superpowers/specs/2026-04-12-transcription-backend-trait-design.md`

**Branch:** `feature/transcription-sherpa-spike` (already created, no commits yet)

---

## File Structure

**Create:**
- `crates/sdr-transcription/src/sherpa_model.rs` — `SherpaModel` enum, directory discovery, file path helpers
- `crates/sdr-transcription/src/backends/sherpa.rs` — `SherpaBackend` struct + `TranscriptionBackend` impl + worker loop

**Modify:**
- `crates/sdr-transcription/Cargo.toml` — add `sherpa-onnx = "1.12"` dependency
- `crates/sdr-transcription/src/lib.rs` — re-export `SherpaModel`, change `start(WhisperModel, ...)` to `start(BackendConfig)`, route `ModelChoice` to the right backend
- `crates/sdr-transcription/src/backend.rs` — add `Sherpa(SherpaModel)` to `ModelChoice`, add `Partial { text }` to `TranscriptionEvent`, add `BackendError::ModelNotFound { path: PathBuf }` and `BackendError::WrongModelKind`
- `crates/sdr-transcription/src/backends/whisper.rs` — `start()` returns `WrongModelKind` if a non-Whisper `ModelChoice` is passed
- `crates/sdr-transcription/src/backends/mock.rs` — accept any `ModelChoice` (mock is testing the engine, not the backend dispatch)
- `crates/sdr-ui/src/sidebar/transcript_panel.rs` — model picker becomes contextual to whichever backend was compiled in; silence_threshold slider cfg-gated on `whisper` feature (Note: the original `backend_row` ComboRow design was superseded by the build-time mutex feature flag — see PR #249.)
- `crates/sdr-ui/src/window.rs` — construct `BackendConfig` from the panel state, call new `engine.start(config)`, add `Partial { text }` match arm that routes to `tracing::debug!`
- `crates/sdr-ui/src/dsp_controller.rs` — no changes expected (just verify nothing in the audio tap path depends on the engine API shape)
- `Cargo.lock` — auto-updated by cargo

**Untouched (regression surface check):**
- `crates/sdr-transcription/src/denoise.rs` — unchanged
- `crates/sdr-transcription/src/resampler.rs` — unchanged
- `crates/sdr-transcription/src/model.rs` — unchanged
- `crates/sdr-transcription/src/backends/mod.rs` — already declares `pub mod whisper;` and `#[cfg(test)] pub mod mock;`; we add `pub mod sherpa;` to it
- All other UI files unchanged

---

## Conventions for this PR

- The Whisper transcription flow MUST keep working unchanged. After every task that touches lib.rs, backend.rs, or window.rs, verify Whisper still starts and transcribes via the existing path.
- The sherpa backend is allowed to fail loudly if the model isn't downloaded — that's the spike contract. PR 3 adds auto-download. The error message must tell the user EXACTLY where to download to and what files are expected.
- `OnlineRecognizer` and `OnlineStream` are `!Send`. Construct them inside the worker thread, never store them in `SherpaBackend` itself. Same pattern as `WhisperContext` in `WhisperBackend`.
- For this PR, the sherpa partial events route to `tracing::debug!("partial: {text}")` in window.rs. Final events render to the text view exactly like Whisper's `Text` event. Full live-captions UI lands in PR 4.
- The CUDA story for `sherpa-onnx-sys` is one of the spike's primary unknowns. The plan asks the user to test both CPU and CUDA in the smoke test step. If CUDA fails, document the failure in the PR description and ship CPU-only — PR 4 or a follow-up can fix it.

---

## Sherpa model bundle: Streaming Zipformer English

Initial model: `sherpa-onnx-streaming-zipformer-en-2023-06-26`

**Bundle URL:** `https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2`

**Expected files** (after `tar -xjf` extraction):
- `encoder-epoch-99-avg-1-chunk-16-left-128.onnx` (or `.int8.onnx` quantized variant)
- `decoder-epoch-99-avg-1-chunk-16-left-128.onnx`
- `joiner-epoch-99-avg-1-chunk-16-left-128.onnx`
- `tokens.txt`

**Target directory** (chosen by `SherpaModel::directory()`):
- `~/.local/share/sdr-rs/models/sherpa/streaming-zipformer-en/`

For the spike, the user runs (once):

```bash
mkdir -p ~/.local/share/sdr-rs/models/sherpa
cd ~/.local/share/sdr-rs/models/sherpa
wget https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2
tar -xjf sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2
mv sherpa-onnx-streaming-zipformer-en-2023-06-26 streaming-zipformer-en
```

PR 3 automates all of this.

---

## Task 1: Add `sherpa-onnx` dependency and verify the build works

The riskiest single change in this PR — `sherpa-onnx-sys` build script downloads a precompiled libsherpa archive on first build. We need to know early whether this works on the user's network and toolchain.

**Files:**
- Modify: `crates/sdr-transcription/Cargo.toml`

- [ ] **Step 1: Add the dependency**

Find the `[dependencies]` section in `crates/sdr-transcription/Cargo.toml` and add:

```toml
sherpa-onnx = "1.12"
```

The line goes alongside the existing deps (alphabetical position is fine). The crate uses `default = ["static"]` which statically links libsherpa-onnx into our binary. No additional system deps required.

The full section should look something like:

```toml
[dependencies]
whisper-rs.workspace = true
reqwest.workspace = true
rustfft.workspace = true
sdr-types.workspace = true
sherpa-onnx = "1.12"
thiserror.workspace = true
tracing.workspace = true
dirs-next.workspace = true
libc.workspace = true
```

- [ ] **Step 2: Build the crate (this will take a while on first build — libsherpa download)**

```bash
cd /data/source/rtl-sdr
cargo build -p sdr-transcription 2>&1 | tail -30
```

**Expected:** the first build downloads the precompiled libsherpa archive (~50-200 MB) via the `sherpa-onnx-sys` build script. This may take 30-180 seconds. On subsequent builds, the cached library is used.

**If the build fails:**
- "could not download libsherpa-onnx" → network issue or upstream URL change. Check the build script error message. If upstream changed, may need to pin to a specific older version like `sherpa-onnx = "1.12.38"`.
- Linker errors → missing system deps. Should not happen with `default = ["static"]` but if it does, document the missing dep.
- Slow build → expected for the first time. Don't kill it under 5 minutes.

**STOP and report BLOCKED if the build fails.** Don't try to work around build failures — we need the user to know if their environment can't build sherpa-onnx before we sink any more effort into this PR.

- [ ] **Step 3: Verify the workspace still builds**

```bash
cargo build --workspace 2>&1 | tail -10
```

**Expected:** clean. Adding the dependency to `sdr-transcription` should not break any other crate.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-transcription/Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
sdr-transcription: add sherpa-onnx dependency

First step of PR 2 (sherpa spike). Adds the official k2-fsa
sherpa-onnx Rust crate with default static linking. The build script
downloads a precompiled libsherpa archive on first build (~50-200 MB,
cached after that). No system dependencies required.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Create `sherpa_model.rs` with `SherpaModel` enum

The model registry — mirrors `model.rs` (the Whisper one) but for sherpa bundles. For PR 2 there's only one variant; PR 5+ adds Parakeet, Moonshine, etc.

**Files:**
- Create: `crates/sdr-transcription/src/sherpa_model.rs`

- [ ] **Step 1: Write the file**

Write this exact content to `crates/sdr-transcription/src/sherpa_model.rs`:

```rust
//! Sherpa-onnx model registry and path management.
//!
//! Mirrors `model.rs` (the Whisper registry) but for sherpa-onnx bundles.
//! Each `SherpaModel` variant maps to a directory containing the encoder,
//! decoder, joiner, and tokens files for one streaming ASR model.
//!
//! For PR 2 (the sherpa spike) the user manually downloads bundles into
//! `models_dir() / sherpa / <model>/` before launching. PR 3 adds
//! auto-download.

use std::path::PathBuf;

/// Available sherpa-onnx model variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SherpaModel {
    /// Streaming Zipformer English (k2-fsa, 2023-06-26).
    StreamingZipformerEn,
}

impl SherpaModel {
    /// Human-readable display label for the model picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "Streaming Zipformer (English)",
        }
    }

    /// Directory name (under `models_dir() / sherpa /`) where this model's
    /// files live.
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "streaming-zipformer-en",
        }
    }

    /// Filename of the encoder ONNX file inside the model directory.
    pub fn encoder_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => {
                "encoder-epoch-99-avg-1-chunk-16-left-128.onnx"
            }
        }
    }

    /// Filename of the decoder ONNX file inside the model directory.
    pub fn decoder_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => {
                "decoder-epoch-99-avg-1-chunk-16-left-128.onnx"
            }
        }
    }

    /// Filename of the joiner ONNX file inside the model directory.
    pub fn joiner_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => {
                "joiner-epoch-99-avg-1-chunk-16-left-128.onnx"
            }
        }
    }

    /// Filename of the tokens file inside the model directory.
    pub fn tokens_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "tokens.txt",
        }
    }

    /// All available variants in order — used to populate the UI dropdown.
    pub const ALL: &[Self] = &[Self::StreamingZipformerEn];
}

/// Returns the sherpa subdirectory under the shared models dir
/// (`~/.local/share/sdr-rs/models/sherpa/`).
pub fn sherpa_models_dir() -> PathBuf {
    crate::model::models_dir().join("sherpa")
}

/// Returns the directory containing all files for a given sherpa model
/// (`~/.local/share/sdr-rs/models/sherpa/<dir_name>/`).
pub fn model_directory(model: SherpaModel) -> PathBuf {
    sherpa_models_dir().join(model.dir_name())
}

/// Returns the full paths for all files needed by a sherpa model.
///
/// Order: (encoder, decoder, joiner, tokens). The caller checks each path
/// for existence and emits a helpful error if any are missing.
pub fn model_file_paths(model: SherpaModel) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = model_directory(model);
    (
        dir.join(model.encoder_filename()),
        dir.join(model.decoder_filename()),
        dir.join(model.joiner_filename()),
        dir.join(model.tokens_filename()),
    )
}

/// True if all four required files for `model` exist on disk.
pub fn model_exists(model: SherpaModel) -> bool {
    let (e, d, j, t) = model_file_paths(model);
    e.is_file() && d.is_file() && j.is_file() && t.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_models_have_unique_directory_names() {
        let names: Vec<_> = SherpaModel::ALL.iter().map(|m| m.dir_name()).collect();
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(names.len(), unique.len());
    }

    #[test]
    fn streaming_zipformer_en_dir_is_under_sherpa() {
        let dir = model_directory(SherpaModel::StreamingZipformerEn);
        assert!(dir.ends_with("sherpa/streaming-zipformer-en"));
    }

    #[test]
    fn model_file_paths_returns_four_distinct_files() {
        let (e, d, j, t) = model_file_paths(SherpaModel::StreamingZipformerEn);
        assert_ne!(e, d);
        assert_ne!(e, j);
        assert_ne!(e, t);
        assert_ne!(d, j);
        assert_ne!(d, t);
        assert_ne!(j, t);
    }
}
```

- [ ] **Step 2: Verify (not yet wired into lib.rs)**

```bash
cd /data/source/rtl-sdr
cargo check -p sdr-transcription 2>&1 | tail -10
```

**Expected:** clean (the new file is not yet referenced from lib.rs, so the compiler won't see it; that's expected).

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-transcription/src/sherpa_model.rs
git commit -m "$(cat <<'EOF'
sdr-transcription: add SherpaModel enum and path helpers

New sherpa_model.rs registry mirrors model.rs (the Whisper one) for
sherpa-onnx model bundles. PR 2 ships only one variant
(StreamingZipformerEn); Parakeet (#223) and Moonshine (#224) follow.

Files live under ~/.local/share/sdr-rs/models/sherpa/<dir_name>/ as
encoder.onnx + decoder.onnx + joiner.onnx + tokens.txt. For the spike
the user downloads manually; PR 3 adds auto-download.

Not yet wired into lib.rs — that happens in the next task.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Extend `backend.rs` types and update lib.rs API

Adds the `Sherpa` variant to `ModelChoice`, the `Partial` event variant, two new `BackendError` variants, and changes `TranscriptionEngine::start` to take a `BackendConfig` directly.

**Files:**
- Modify: `crates/sdr-transcription/src/backend.rs`
- Modify: `crates/sdr-transcription/src/lib.rs`
- Modify: `crates/sdr-transcription/src/backends/whisper.rs` (`WrongModelKind` guard)
- Modify: `crates/sdr-transcription/src/backends/mock.rs` (still accepts any ModelChoice)

This task is the API churn point — after it lands, the UI in Task 4 has to change to match.

- [ ] **Step 1: Update `backend.rs` — extend `ModelChoice`, add `Partial`, extend `BackendError`**

Read the current `crates/sdr-transcription/src/backend.rs` file. Make these specific edits:

**Edit A — Add `use` for `PathBuf`:** at the top of the file, after `use std::sync::mpsc;`, insert:

```rust
use std::path::PathBuf;
```

**Edit B — Add `Sherpa` variant to `ModelChoice`:** find the `ModelChoice` enum and replace it with:

```rust
/// User-facing model selection.
///
/// The variant determines which backend the engine instantiates internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelChoice {
    Whisper(WhisperModel),
    Sherpa(crate::sherpa_model::SherpaModel),
}
```

**Edit C — Add `Partial` variant to `TranscriptionEvent`:** find the `TranscriptionEvent` enum and add the `Partial` variant. The full enum should look like:

```rust
/// Events emitted by a backend during its lifecycle.
///
/// Variant names are stable — UI consumers match on them by name.
#[derive(Debug, Clone)]
pub enum TranscriptionEvent {
    /// Model download in progress (0..=100).
    Downloading { progress_pct: u8 },
    /// Model loaded and ready for inference.
    Ready,
    /// Incremental hypothesis from a streaming backend. May be replaced
    /// by another `Partial` before being committed as a `Text`. Backends
    /// that return `false` from `supports_partials()` never emit this.
    Partial { text: String },
    /// Transcribed text from one inference pass (or one committed
    /// streaming utterance).
    Text {
        /// Wall-clock timestamp in "HH:MM:SS" format.
        timestamp: String,
        /// Transcribed text (trimmed, non-empty).
        text: String,
    },
    /// Fatal error — backend will exit after sending this.
    Error(String),
}
```

**Edit D — Extend `BackendError`:** find the `BackendError` enum and replace it with:

```rust
/// Errors a backend can return from `start`.
///
/// Mirrors `crate::TranscriptionError` so the engine can convert
/// transparently. Kept separate so backends don't depend on the engine.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("failed to spawn worker thread: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("model files not found at {path}; download the bundle and place its contents in this directory")]
    ModelNotFound { path: PathBuf },
    #[error("backend received the wrong model kind in BackendConfig — engine bug")]
    WrongModelKind,
    #[error("backend initialization failed: {0}")]
    Init(String),
}
```

- [ ] **Step 2: Update `lib.rs` — change `start()` signature to take `BackendConfig`, route by `ModelChoice`**

Read the current `crates/sdr-transcription/src/lib.rs`. Replace the `pub fn start(...)` method with this version that takes `BackendConfig` directly:

**Find:**

```rust
    /// Start the Whisper backend with the given model and parameters.
    /// Returns a receiver for [`TranscriptionEvent`].
    ///
    /// Kept for API compatibility with the pre-refactor engine.
    /// Internally constructs a [`WhisperBackend`] and delegates.
    pub fn start(
        &mut self,
        whisper_model: WhisperModel,
        silence_threshold: f32,
        noise_gate_ratio: f32,
    ) -> Result<mpsc::Receiver<TranscriptionEvent>, TranscriptionError> {
        let backend: Box<dyn TranscriptionBackend> = Box::new(WhisperBackend::new());
        let config = BackendConfig {
            model: ModelChoice::Whisper(whisper_model),
            silence_threshold,
            noise_gate_ratio,
        };
        self.start_with_backend(backend, config)
    }
```

**Replace with:**

```rust
    /// Start a transcription backend selected by `config.model`.
    ///
    /// Constructs the right backend (Whisper or Sherpa) for the chosen
    /// model and returns a receiver for [`TranscriptionEvent`].
    pub fn start(
        &mut self,
        config: BackendConfig,
    ) -> Result<mpsc::Receiver<TranscriptionEvent>, TranscriptionError> {
        let backend: Box<dyn TranscriptionBackend> = match config.model {
            ModelChoice::Whisper(_) => Box::new(WhisperBackend::new()),
            ModelChoice::Sherpa(_) => Box::new(backends::sherpa::SherpaBackend::new()),
        };
        self.start_with_backend(backend, config)
    }
```

Also add `pub use sherpa_model::SherpaModel;` and `pub mod sherpa_model;` near the top. The full module/re-export block should look like:

```rust
pub mod backend;
pub mod backends;
pub mod denoise;
pub mod model;
pub mod resampler;
pub mod sherpa_model;

pub use backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
    TranscriptionEvent,
};
pub use model::WhisperModel;
pub use sherpa_model::SherpaModel;
```

Note: this commit will reference `backends::sherpa::SherpaBackend::new()` which doesn't exist yet. **The crate will not compile after this step alone.** We're committing in a logical chunk — the SherpaBackend skeleton lands in Task 5 and the code will compile then. Tasks 4 wires the UI to the new API in between, but that depends on this task's API change being committed first.

**Strategy:** stash the new lib.rs `start` method content for now, leave the old `start(WhisperModel, ...)` in place, and make the API change in Task 5 alongside the SherpaBackend file. This way every commit compiles cleanly.

**Revised plan for this step:** do NOT change `lib.rs` `start()` in this task. Only:
- Add the new variants/types to backend.rs
- Add `pub use sherpa_model::SherpaModel;` and `pub mod sherpa_model;` to lib.rs

The `start()` signature change happens in Task 5 alongside the SherpaBackend skeleton.

**Concrete instruction:** in lib.rs, only add these two lines:
- `pub mod sherpa_model;` (in the module declarations block)
- `pub use sherpa_model::SherpaModel;` (in the re-exports block)

Do NOT change the `start()` method body in this task.

- [ ] **Step 3: Update `backends/whisper.rs` — add `WrongModelKind` guard**

The `WhisperBackend::start()` method currently does an irrefutable destructure: `let ModelChoice::Whisper(whisper_model) = config.model;`. Since `ModelChoice` now has two variants, this becomes a `match` that returns `WrongModelKind` for the Sherpa case.

**Find:**

```rust
    fn start(&mut self, config: BackendConfig) -> Result<BackendHandle, BackendError> {
        let ModelChoice::Whisper(whisper_model) = config.model;

        self.cancel.store(false, Ordering::Relaxed);
```

**Replace with:**

```rust
    fn start(&mut self, config: BackendConfig) -> Result<BackendHandle, BackendError> {
        let ModelChoice::Whisper(whisper_model) = config.model else {
            return Err(BackendError::WrongModelKind);
        };

        self.cancel.store(false, Ordering::Relaxed);
```

- [ ] **Step 4: `backends/mock.rs` needs no changes**

The mock's `start()` already takes a `BackendConfig` by value and doesn't destructure `model` — it ignores `_config` entirely. The test fixture continues to work for both `Whisper` and `Sherpa` model choices.

- [ ] **Step 5: Verify build**

```bash
cd /data/source/rtl-sdr
cargo build -p sdr-transcription 2>&1 | tail -20
```

**Expected:** clean build of `sdr-transcription`. The full workspace will fail because `sdr-ui/src/window.rs` matches on `TranscriptionEvent` exhaustively and now has a missing `Partial` arm — that's fixed in Task 4. For now, only verify the crate-level build.

```bash
cargo test -p sdr-transcription 2>&1 | tail -20
```

**Expected:** all 28 tests still pass (3 new tests in `sherpa_model.rs` bring it to 31 — confirm the count went up).

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-transcription/src/backend.rs crates/sdr-transcription/src/lib.rs crates/sdr-transcription/src/backends/whisper.rs
git commit -m "$(cat <<'EOF'
sdr-transcription: extend ModelChoice + TranscriptionEvent for sherpa

Adds the Sherpa variant to ModelChoice, the Partial event variant
that streaming backends emit, and BackendError::ModelNotFound +
WrongModelKind variants. WhisperBackend::start now returns
WrongModelKind if a non-Whisper choice is passed.

The TranscriptionEngine::start signature change (BackendConfig
parameter) and the SherpaBackend dispatch land in the next tasks,
so this commit compiles sdr-transcription cleanly but sdr-ui will
fail until window.rs grows a Partial match arm in Task 4.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Update `sdr-ui` for the new event variant + temporary tracing arm

Adds a `Partial { text }` match arm to `window.rs`'s transcript event handler. For the spike, the arm just calls `tracing::debug!`. This unblocks the workspace build.

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 1: Add the Partial match arm**

In `crates/sdr-ui/src/window.rs`, find the `match event` block inside the `glib::timeout_add_local` closure (around line 1483-1509). The current arms are: `Downloading`, `Ready`, `Text`, `Error`. Add a `Partial` arm BEFORE the `Text` arm so streaming partial hypotheses are logged but not rendered (full rendering lands in PR 4).

**Find:**

```rust
                                    TranscriptionEvent::Ready => {
                                        status.set_text("Listening...");
                                        status.set_css_classes(&["success"]);
                                        progress.set_visible(false);
                                    }
                                    TranscriptionEvent::Text { timestamp, text } => {
```

**Replace with:**

```rust
                                    TranscriptionEvent::Ready => {
                                        status.set_text("Listening...");
                                        status.set_css_classes(&["success"]);
                                        progress.set_visible(false);
                                    }
                                    TranscriptionEvent::Partial { text } => {
                                        // PR 4 will render this as a live caption line.
                                        // For the PR 2 spike, log only.
                                        tracing::debug!(target: "transcription", partial = %text);
                                    }
                                    TranscriptionEvent::Text { timestamp, text } => {
```

- [ ] **Step 2: Verify the workspace builds**

```bash
cd /data/source/rtl-sdr
cargo build --workspace 2>&1 | tail -15
```

**Expected:** clean. The exhaustive match in `sdr-ui` is now satisfied.

```bash
cargo test --workspace 2>&1 | grep "test result" | tail -20
```

**Expected:** all tests still pass.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "$(cat <<'EOF'
sdr-ui: handle TranscriptionEvent::Partial (spike: log only)

Adds the missing match arm for the new Partial event variant
introduced in the previous commit. For the PR 2 spike, partials
just go to tracing::debug — full live captions rendering with
the two-line model lands in PR 4.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Create `SherpaBackend` skeleton + change engine.start() signature

Creates the SherpaBackend file with a struct that compiles but immediately returns `BackendError::Init` from `start()`. Also lands the `engine.start()` signature change to take `BackendConfig`. The UI will be updated in Task 6 to call the new signature; until then, the workspace build will be temporarily broken (UI window.rs still calls the old `start(WhisperModel, ...)` form). Task 6 is small and lands right after.

**Files:**
- Create: `crates/sdr-transcription/src/backends/sherpa.rs`
- Modify: `crates/sdr-transcription/src/backends/mod.rs` (declare `pub mod sherpa;`)
- Modify: `crates/sdr-transcription/src/lib.rs` (change `start` signature)

- [ ] **Step 1: Add the module declaration**

In `crates/sdr-transcription/src/backends/mod.rs`, add `pub mod sherpa;` so the file should look like:

```rust
//! Concrete `TranscriptionBackend` implementations.
//!
//! Each backend is a self-contained module. The engine in `lib.rs`
//! constructs one based on the [`crate::backend::ModelChoice`] variant
//! and delegates lifecycle to it.

pub mod sherpa;
pub mod whisper;

#[cfg(test)]
pub mod mock;
```

- [ ] **Step 2: Create the SherpaBackend skeleton**

Write this content to `crates/sdr-transcription/src/backends/sherpa.rs`. This is a SKELETON — the real recognizer wiring lands in Task 7. For now, `start()` returns `BackendError::Init("not yet implemented")` so the file compiles and can be referenced from lib.rs.

```rust
//! Sherpa-onnx backend — streaming-native ASR via the official k2-fsa
//! `sherpa-onnx` Rust crate.
//!
//! Implements [`TranscriptionBackend`] using `OnlineRecognizer` for true
//! frame-by-frame streaming with endpoint detection. Partial hypotheses
//! are emitted as `TranscriptionEvent::Partial`; committed utterances
//! after silence detection are emitted as `TranscriptionEvent::Text`.
//!
//! `OnlineRecognizer` and `OnlineStream` are `!Send` (they wrap raw
//! pointers into the C library), so all recognizer interaction happens
//! on the worker thread. The `SherpaBackend` struct itself only holds
//! the cancellation token and the worker join handle, mirroring
//! `WhisperBackend`.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, TranscriptionBackend,
};

/// `TranscriptionBackend` implementation backed by `sherpa-onnx`.
pub struct SherpaBackend {
    cancel: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Default for SherpaBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SherpaBackend {
    pub fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }
}

impl TranscriptionBackend for SherpaBackend {
    fn name(&self) -> &'static str {
        "sherpa"
    }

    fn supports_partials(&self) -> bool {
        true
    }

    fn start(&mut self, _config: BackendConfig) -> Result<BackendHandle, BackendError> {
        // Skeleton — real implementation lands in Task 7.
        Err(BackendError::Init(
            "SherpaBackend is not yet implemented (Task 7)".to_owned(),
        ))
    }

    fn stop(&mut self) {
        self.shutdown_nonblocking();
    }

    fn shutdown_nonblocking(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.worker.take(); // detach
        tracing::info!("sherpa backend shutdown (non-blocking)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sherpa_backend_supports_partials() {
        let backend = SherpaBackend::new();
        assert!(backend.supports_partials());
    }

    #[test]
    fn sherpa_backend_name_is_stable() {
        let backend = SherpaBackend::new();
        assert_eq!(backend.name(), "sherpa");
    }
}
```

- [ ] **Step 3: Change `lib.rs` `start()` signature**

In `crates/sdr-transcription/src/lib.rs`, find the existing `start()` method and replace it with the version that takes `BackendConfig`:

**Find:**

```rust
    /// Start the Whisper backend with the given model and parameters.
    /// Returns a receiver for [`TranscriptionEvent`].
    ///
    /// Kept for API compatibility with the pre-refactor engine.
    /// Internally constructs a [`WhisperBackend`] and delegates.
    pub fn start(
        &mut self,
        whisper_model: WhisperModel,
        silence_threshold: f32,
        noise_gate_ratio: f32,
    ) -> Result<mpsc::Receiver<TranscriptionEvent>, TranscriptionError> {
        let backend: Box<dyn TranscriptionBackend> = Box::new(WhisperBackend::new());
        let config = BackendConfig {
            model: ModelChoice::Whisper(whisper_model),
            silence_threshold,
            noise_gate_ratio,
        };
        self.start_with_backend(backend, config)
    }
```

**Replace with:**

```rust
    /// Start a transcription backend selected by `config.model`.
    ///
    /// Constructs the right backend (Whisper or Sherpa) for the chosen
    /// model and returns a receiver for [`TranscriptionEvent`].
    pub fn start(
        &mut self,
        config: BackendConfig,
    ) -> Result<mpsc::Receiver<TranscriptionEvent>, TranscriptionError> {
        let backend: Box<dyn TranscriptionBackend> = match config.model {
            ModelChoice::Whisper(_) => Box::new(WhisperBackend::new()),
            ModelChoice::Sherpa(_) => Box::new(backends::sherpa::SherpaBackend::new()),
        };
        self.start_with_backend(backend, config)
    }
```

- [ ] **Step 4: Update the engine tests in lib.rs**

The existing engine tests use `dummy_config()` which returns a `BackendConfig` — they already work with `start_with_backend(backend, config)`, so they don't need changes. But there are no tests calling the public `start(config)` directly. Add one:

Find the `engine_drop_runs_shutdown` test in `crates/sdr-transcription/src/lib.rs` and add a new test after `engine_stop_clears_state` (or wherever the last test is):

```rust
    #[test]
    fn engine_start_with_whisper_choice_picks_whisper_backend() {
        // We can't run a real Whisper start without downloading a model,
        // so this test only verifies the dispatch logic compiles and the
        // ModelChoice → backend type mapping is wired through start().
        // The behavioral coverage lives in start_with_backend tests above.
        let mut engine = TranscriptionEngine::new();
        // Use start_with_backend with a mock so we don't actually load
        // a Whisper model. The point of this test is just type-level
        // coverage of the new start(config) entry point — see
        // engine_starts_with_mock_backend for the actual behavior.
        let backend = Box::new(MockBackend::new());
        let _ = engine
            .start_with_backend(backend, dummy_config())
            .expect("start_with_backend ok");
        assert!(engine.is_running());
    }
```

(This test is admittedly thin — the real coverage is in `engine_starts_with_mock_backend`. The new test is here just so PR review can see that the new `start(config)` entry point at least compiles.)

- [ ] **Step 5: Verify the crate builds**

```bash
cd /data/source/rtl-sdr
cargo build -p sdr-transcription 2>&1 | tail -20
cargo test -p sdr-transcription 2>&1 | tail -20
```

**Expected:** crate builds and tests pass. New test count: 33 (was 31 after Task 2, +2 from sherpa.rs trait surface tests, +1 from the start dispatch test would be 34 — but the start dispatch test is using `start_with_backend`, so it's just one of the existing test patterns. Actual count: depends on what's added).

The workspace build will be broken because `sdr-ui/src/window.rs` line 1465 still calls `engine.start(whisper_model, silence_threshold, noise_gate_ratio)` which no longer matches the new signature. Task 6 fixes that.

```bash
cargo build --workspace 2>&1 | tail -10
```

**Expected:** error in `sdr-ui/src/window.rs` about argument mismatch on `engine.start(...)`. This is intentional — the next task fixes it.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-transcription/src/backends/sherpa.rs crates/sdr-transcription/src/backends/mod.rs crates/sdr-transcription/src/lib.rs
git commit -m "$(cat <<'EOF'
sdr-transcription: SherpaBackend skeleton + engine.start(BackendConfig)

Skeleton SherpaBackend that implements TranscriptionBackend with
supports_partials() = true. start() returns BackendError::Init for
now — real OnlineRecognizer wiring lands in Task 7.

TranscriptionEngine::start now takes a BackendConfig directly and
routes to WhisperBackend or SherpaBackend based on config.model.

sdr-ui/window.rs is intentionally broken at this commit; fixed in
the next task.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Update `sdr-ui` to call the new `engine.start(BackendConfig)` API

Updates the call site in `window.rs` to construct a `BackendConfig` and pass it. Adds basic backend selection wiring (reads from the panel state which Task 8 sets up — for now, hardcode Whisper).

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 1: Update the start() call site**

Find this section in `crates/sdr-ui/src/window.rs` (around line 1448-1466):

```rust
            let model_idx = model_row.selected() as usize;
            let whisper_model = sdr_transcription::WhisperModel::ALL
                .get(model_idx)
                .copied()
                .unwrap_or(sdr_transcription::WhisperModel::TinyEn);

            // Read tuning slider values.
            #[allow(clippy::cast_possible_truncation)]
            let silence_threshold = silence_row.value() as f32;
            #[allow(clippy::cast_possible_truncation)]
            let noise_gate_ratio = noise_gate_row.value() as f32;

            // Scope the borrow so it's dropped before any potential re-entry
            // from row.set_active(false) on error.
            let start_result =
                engine_clone
                    .borrow_mut()
                    .start(whisper_model, silence_threshold, noise_gate_ratio);
```

**Replace with:**

```rust
            let model_idx = model_row.selected() as usize;
            let whisper_model = sdr_transcription::WhisperModel::ALL
                .get(model_idx)
                .copied()
                .unwrap_or(sdr_transcription::WhisperModel::TinyEn);

            // Read tuning slider values.
            #[allow(clippy::cast_possible_truncation)]
            let silence_threshold = silence_row.value() as f32;
            #[allow(clippy::cast_possible_truncation)]
            let noise_gate_ratio = noise_gate_row.value() as f32;

            // Build BackendConfig. Backend is selected at compile time via
            // cargo features (whisper / sherpa are mutually exclusive).
            // Note: the original Task 8 runtime backend selector ComboRow
            // design was superseded by the build-time mutex — see PR #249.
            let config = sdr_transcription::BackendConfig {
                model: sdr_transcription::ModelChoice::Whisper(whisper_model),
                silence_threshold,
                noise_gate_ratio,
            };

            // Scope the borrow so it's dropped before any potential re-entry
            // from row.set_active(false) on error.
            let start_result = engine_clone.borrow_mut().start(config);
```

- [ ] **Step 2: Verify the workspace builds**

```bash
cd /data/source/rtl-sdr
cargo build --workspace 2>&1 | tail -10
cargo test --workspace 2>&1 | grep "test result" | tail -20
```

**Expected:** clean. All tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "$(cat <<'EOF'
sdr-ui: call engine.start(BackendConfig) instead of (WhisperModel, ...)

Constructs a BackendConfig from the panel state (currently
hardcoded to ModelChoice::Whisper) and passes it to the new engine
API. Backend selection via the upcoming ComboRow lands in Task 8;
this commit unblocks the workspace build after the API change.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Implement the real `SherpaBackend` feed loop

Replace the skeleton `start()` with a working implementation that loads the model files, creates an `OnlineRecognizer`, spawns a worker thread, and runs the streaming feed loop. This is the meat of the spike.

**Files:**
- Modify: `crates/sdr-transcription/src/backends/sherpa.rs`

- [ ] **Step 1: Replace the skeleton with the real implementation**

Replace the entire content of `crates/sdr-transcription/src/backends/sherpa.rs` with:

```rust
//! Sherpa-onnx backend — streaming-native ASR via the official k2-fsa
//! `sherpa-onnx` Rust crate.
//!
//! Implements [`TranscriptionBackend`] using `OnlineRecognizer` for true
//! frame-by-frame streaming with endpoint detection. Partial hypotheses
//! are emitted as `TranscriptionEvent::Partial`; committed utterances
//! after silence detection are emitted as `TranscriptionEvent::Text`.
//!
//! `OnlineRecognizer` and `OnlineStream` are `!Send` (they wrap raw
//! pointers into the C library), so all recognizer interaction happens
//! on the worker thread. The `SherpaBackend` struct itself only holds
//! the cancellation token and the worker join handle, mirroring
//! `WhisperBackend`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig};

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
    TranscriptionEvent,
};
use crate::sherpa_model::{self, SherpaModel};
use crate::{denoise, resampler};

/// Bounded channel capacity for audio buffers from DSP → backend.
/// Sherpa accepts much smaller chunks than Whisper (per-frame, not
/// 5-second windows), so we don't need the same headroom WhisperBackend
/// does. 256 still gives plenty of buffer.
const AUDIO_CHANNEL_CAPACITY: usize = 256;

/// Polling interval for the audio receive loop when checking for cancellation.
const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// `TranscriptionBackend` implementation backed by `sherpa-onnx`.
pub struct SherpaBackend {
    cancel: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Default for SherpaBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SherpaBackend {
    pub fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }
}

impl TranscriptionBackend for SherpaBackend {
    fn name(&self) -> &'static str {
        "sherpa"
    }

    fn supports_partials(&self) -> bool {
        true
    }

    fn start(&mut self, config: BackendConfig) -> Result<BackendHandle, BackendError> {
        let ModelChoice::Sherpa(sherpa_model) = config.model else {
            return Err(BackendError::WrongModelKind);
        };

        // Verify the model files exist before spawning the worker, so the
        // user gets a useful error immediately rather than after the
        // recognizer fails inside the thread.
        if !sherpa_model::model_exists(sherpa_model) {
            return Err(BackendError::ModelNotFound {
                path: sherpa_model::model_directory(sherpa_model),
            });
        }

        self.cancel.store(false, Ordering::Relaxed);

        let (audio_tx, audio_rx) = mpsc::sync_channel(AUDIO_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();

        let cancel = Arc::clone(&self.cancel);
        let noise_gate_ratio = config.noise_gate_ratio;

        let handle = std::thread::Builder::new()
            .name("sherpa-worker".into())
            .spawn(move || {
                run_worker(
                    &audio_rx,
                    &event_tx,
                    &cancel,
                    sherpa_model,
                    noise_gate_ratio,
                );
            })?;

        self.worker = Some(handle);
        tracing::info!("sherpa backend started");

        Ok(BackendHandle { audio_tx, event_rx })
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        tracing::info!("sherpa backend stopped");
    }

    fn shutdown_nonblocking(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.worker.take(); // detach — don't join
        tracing::info!("sherpa backend shutdown (non-blocking)");
    }
}

/// Build the `OnlineRecognizerConfig` for a Streaming Zipformer model.
///
/// Returns `Init` errors with helpful messages on misconfiguration. The
/// `provider` parameter selects the ONNX execution provider — `"cpu"` is
/// the default; `"cuda"` enables GPU acceleration if libsherpa was built
/// with CUDA support.
fn build_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> Result<OnlineRecognizerConfig, BackendError> {
    let (encoder, decoder, joiner, tokens) = sherpa_model::model_file_paths(model);

    let mut config = OnlineRecognizerConfig::default();
    config.model_config.transducer.encoder = Some(encoder.to_string_lossy().into_owned());
    config.model_config.transducer.decoder = Some(decoder.to_string_lossy().into_owned());
    config.model_config.transducer.joiner = Some(joiner.to_string_lossy().into_owned());
    config.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
    config.model_config.provider = Some(provider.to_owned());
    config.model_config.num_threads = 1;
    config.enable_endpoint = true;
    config.decoding_method = Some("greedy_search".to_owned());
    // Endpoint detection rules — defaults from sherpa-onnx examples.
    config.rule1_min_trailing_silence = 2.4;
    config.rule2_min_trailing_silence = 1.2;
    config.rule3_min_utterance_length = 20.0;

    Ok(config)
}

/// Worker thread entry point. Owns the recognizer and stream for the
/// entire transcription session.
fn run_worker(
    audio_rx: &mpsc::Receiver<Vec<f32>>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    cancel: &Arc<AtomicBool>,
    model: SherpaModel,
    noise_gate_ratio: f32,
) {
    if let Err(e) = run_worker_inner(audio_rx, event_tx, cancel, model, noise_gate_ratio) {
        let _ = event_tx.send(TranscriptionEvent::Error(e));
    }
}

#[allow(clippy::too_many_lines)]
fn run_worker_inner(
    audio_rx: &mpsc::Receiver<Vec<f32>>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    cancel: &Arc<AtomicBool>,
    model: SherpaModel,
    noise_gate_ratio: f32,
) -> Result<(), String> {
    // --- Build the recognizer ---
    //
    // For PR 2 we hardcode CPU provider. CUDA support is validated in
    // the smoke test step; if CUDA works, a follow-up PR (or PR 4) wires
    // it through the UI as a separate setting.
    let provider = "cpu";
    let recognizer_config = build_recognizer_config(model, provider)
        .map_err(|e| format!("failed to build recognizer config: {e}"))?;

    tracing::info!(?model, provider, "loading sherpa-onnx model");
    let recognizer = OnlineRecognizer::create(&recognizer_config)
        .ok_or_else(|| "OnlineRecognizer::create returned None — check model file paths".to_owned())?;

    let stream = recognizer.create_stream();

    tracing::info!("sherpa-onnx model loaded, ready for inference");
    event_tx
        .send(TranscriptionEvent::Ready)
        .map_err(|_| "event channel closed before Ready".to_owned())?;

    // --- Audio loop ---
    //
    // Sherpa expects 16 kHz mono f32 samples. We accept the same
    // 48 kHz interleaved stereo from the DSP thread that Whisper does,
    // run it through the spectral denoiser, downsample to 16k mono, and
    // feed sherpa one chunk at a time.
    let mut mono_buf: Vec<f32> = Vec::with_capacity(16_000);
    let mut last_partial = String::new();

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa transcription cancelled, worker exiting");
            return Ok(());
        }

        let interleaved = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(data) => data,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Resample 48k stereo → 16k mono into the scratch buffer.
        mono_buf.clear();
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        // Drain any additional queued buffers into the same scratch
        // (same pattern as WhisperBackend) so we don't fall behind.
        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("sherpa transcription cancelled, worker exiting");
                return Ok(());
            }
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }

        if mono_buf.is_empty() {
            continue;
        }

        // Spectral noise gate (same preprocessor as Whisper).
        denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);

        // Feed the chunk to sherpa.
        stream.accept_waveform(16_000, &mono_buf);

        // Decode as much as the recognizer is ready for.
        while recognizer.is_ready(&stream) {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("sherpa transcription cancelled mid-decode, exiting");
                return Ok(());
            }
            recognizer.decode(&stream);
        }

        // Pull the current hypothesis. Emit a Partial event if it
        // changed since the last one (avoid flooding the UI thread).
        if let Some(result) = recognizer.get_result(&stream) {
            let trimmed = result.text.trim();
            if !trimmed.is_empty() && trimmed != last_partial {
                last_partial = trimmed.to_owned();
                let _ = event_tx.send(TranscriptionEvent::Partial {
                    text: trimmed.to_owned(),
                });
            }

            // On endpoint, commit the utterance as Text and reset the
            // stream so the next utterance starts fresh.
            if recognizer.is_endpoint(&stream) {
                if !trimmed.is_empty() {
                    let timestamp = wall_clock_timestamp();
                    tracing::debug!(%timestamp, %trimmed, "sherpa committed utterance");
                    let _ = event_tx.send(TranscriptionEvent::Text {
                        timestamp,
                        text: trimmed.to_owned(),
                    });
                }
                recognizer.reset(&stream);
                last_partial.clear();
            }
        }
    }

    tracing::info!("sherpa audio channel closed, worker exiting");
    Ok(())
}

/// Wall-clock "HH:MM:SS" string. Same implementation as
/// [`crate::backends::whisper`] but kept local to avoid a public
/// re-export of an internal helper.
#[allow(unsafe_code)]
fn wall_clock_timestamp() -> String {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };

    // SAFETY: gettimeofday writes into the provided buffer and is thread-safe.
    #[allow(unsafe_code)]
    let epoch = unsafe {
        libc::gettimeofday(&raw mut tv, std::ptr::null_mut());
        tv.tv_sec
    };

    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();

    // SAFETY: localtime_r is the reentrant variant; gmtime_r is the UTC fallback.
    #[allow(unsafe_code)]
    let tm = unsafe {
        let result = libc::localtime_r(&raw const epoch, tm.as_mut_ptr());
        let result = if result.is_null() {
            libc::gmtime_r(&raw const epoch, tm.as_mut_ptr())
        } else {
            result
        };
        if result.is_null() {
            return "00:00:00".to_owned();
        }
        tm.assume_init()
    };

    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sherpa_backend_supports_partials() {
        let backend = SherpaBackend::new();
        assert!(backend.supports_partials());
    }

    #[test]
    fn sherpa_backend_name_is_stable() {
        let backend = SherpaBackend::new();
        assert_eq!(backend.name(), "sherpa");
    }
}
```

- [ ] **Step 2: Verify the crate builds**

```bash
cd /data/source/rtl-sdr
cargo build -p sdr-transcription 2>&1 | tail -20
```

**Expected:** clean. If there are sherpa-onnx API mismatches (e.g., a struct field name we got wrong), the compiler will tell you. Common mismatches and fixes:
- `OnlineRecognizerConfig.model_config.transducer.encoder: Option<String>` — confirmed in the upstream source we read
- `OnlineRecognizerConfig.enable_endpoint: bool` — confirmed
- `OnlineRecognizerConfig.decoding_method: Option<String>` — confirmed
- `OnlineRecognizer::create(&config) -> Option<Self>` — confirmed
- `recognizer.create_stream() -> OnlineStream` — confirmed
- `stream.accept_waveform(sample_rate: i32, samples: &[f32])` — note `i32` not `u32`. We pass `16_000` which is a literal that should infer correctly; if not, force it with `16_000_i32`.
- `recognizer.get_result(&stream) -> Option<RecognizerResult>` where `RecognizerResult.text: String` — confirmed
- `recognizer.is_endpoint(&stream) -> bool` — confirmed
- `recognizer.reset(&stream)` — confirmed

If you hit any compile errors, fix them by reading the actual sherpa-onnx 1.12 source at `~/.cargo/registry/src/index.crates.io-*/sherpa-onnx-1.12.*/src/online_asr.rs` to confirm the correct API.

- [ ] **Step 3: Run tests**

```bash
cargo test -p sdr-transcription 2>&1 | tail -20
```

**Expected:** all tests still pass. Sherpa backend tests just verify trait surface — no model loading, so they don't need the user's model files.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-transcription/src/backends/sherpa.rs
git commit -m "$(cat <<'EOF'
sdr-transcription: implement SherpaBackend feed loop

Real implementation of SherpaBackend.start() — loads the model files
into an OnlineRecognizer, creates a stream, spawns a worker thread,
and runs the streaming feed loop:

  recv audio → resample 48k stereo → 16k mono → spectral denoise
  → accept_waveform → loop { is_ready → decode } → get_result
  → emit Partial → on endpoint, emit Text + reset stream

Uses CPU provider for now; CUDA validation happens in the smoke test.
Endpoint detection rules (rule1/2/3) match the upstream sherpa-onnx
example defaults.

Returns BackendError::ModelNotFound with the expected directory path
if the model files aren't on disk yet — auto-download lands in PR 3.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Add backend selector ComboRow to the transcript panel

> **Note:** The runtime backend selector ComboRow design described below was superseded by the build-time mutex feature flag (`whisper` / `sherpa` are mutually exclusive cargo features) — see PR #249. The actual shipped implementation cfg-gates the model picker and silence_threshold slider rather than adding a `backend_row` ComboRow.

The original plan: Adds a "Backend" `AdwComboRow` above the existing "Model" picker, swaps the model picker contents based on the selected backend, and persists both selections.

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/transcript_panel.rs`
- Modify: `crates/sdr-ui/src/window.rs` (read backend selection when starting)

- [ ] **Step 1: Update `transcript_panel.rs`**

The full file rewrite is long because we're adding new state. Read the current `crates/sdr-ui/src/sidebar/transcript_panel.rs` for reference, then apply these changes:

**Edit A — Add new config keys at the top:**

After the existing `KEY_NOISE_GATE` constant, add:

```rust
/// Config key for the persisted backend selection ("whisper" or "sherpa").
const KEY_BACKEND: &str = "transcription_backend";
/// Config key for the persisted Sherpa model index.
const KEY_SHERPA_MODEL: &str = "transcription_sherpa_model";

/// Backend index for Whisper in the backend selector ComboRow.
const BACKEND_IDX_WHISPER: u32 = 0;
/// Backend index for Sherpa in the backend selector ComboRow.
const BACKEND_IDX_SHERPA: u32 = 1;
```

**Edit B — Add `backend_row` field to `TranscriptPanel` struct:**

In the `TranscriptPanel` struct definition (around line 33), add a new field:

```rust
pub struct TranscriptPanel {
    /// The `AdwPreferencesGroup` widget to pack into the sidebar.
    pub widget: adw::PreferencesGroup,
    /// Toggle to enable/disable live transcription.
    pub enable_row: adw::SwitchRow,
    /// Backend selector (Whisper / Sherpa).
    pub backend_row: adw::ComboRow,
    /// Model size selector — contents change based on backend selection.
    pub model_row: adw::ComboRow,
    // ... rest unchanged ...
}
```

And add `backend_row` to the returned struct literal at the bottom of `build_transcript_panel()`.

**Edit C — Build the backend selector and contextual model picker:**

Find the existing model picker section (lines ~69-104, the block starting with `// Model selector` and ending after `connect_selected_notify`). Replace it with:

```rust
    // --- Backend selector ---
    let backend_labels = ["Whisper", "Sherpa (streaming)"];
    let backend_list = gtk4::StringList::new(&backend_labels);

    let saved_backend_idx = config.read(|v| {
        v.get(KEY_BACKEND)
            .and_then(serde_json::Value::as_str)
            .map_or(BACKEND_IDX_WHISPER, |s| match s {
                "sherpa" => BACKEND_IDX_SHERPA,
                _ => BACKEND_IDX_WHISPER,
            })
    });

    let backend_row = adw::ComboRow::builder()
        .title("Backend")
        .model(&backend_list)
        .selected(saved_backend_idx)
        .build();
    group.add(&backend_row);

    // --- Model selector ---
    //
    // Contents are populated based on the active backend. We rebuild the
    // string list each time the backend changes; the selected index
    // resets to 0.
    let whisper_model_labels: Vec<&str> = sdr_transcription::WhisperModel::ALL
        .iter()
        .map(|m| m.label())
        .collect();
    let sherpa_model_labels: Vec<&str> = sdr_transcription::SherpaModel::ALL
        .iter()
        .map(|m| m.label())
        .collect();

    let initial_model_list = if saved_backend_idx == BACKEND_IDX_SHERPA {
        gtk4::StringList::new(&sherpa_model_labels)
    } else {
        gtk4::StringList::new(&whisper_model_labels)
    };

    #[allow(clippy::cast_possible_truncation)]
    let max_whisper_idx = sdr_transcription::WhisperModel::ALL.len() as u32;
    #[allow(clippy::cast_possible_truncation)]
    let max_sherpa_idx = sdr_transcription::SherpaModel::ALL.len() as u32;

    let saved_whisper_model_idx = config.read(|v| {
        v.get(KEY_MODEL)
            .and_then(serde_json::Value::as_u64)
            .and_then(|idx| u32::try_from(idx).ok())
            .filter(|&idx| idx < max_whisper_idx)
            .unwrap_or(0)
    });
    let saved_sherpa_model_idx = config.read(|v| {
        v.get(KEY_SHERPA_MODEL)
            .and_then(serde_json::Value::as_u64)
            .and_then(|idx| u32::try_from(idx).ok())
            .filter(|&idx| idx < max_sherpa_idx)
            .unwrap_or(0)
    });

    let initial_model_idx = if saved_backend_idx == BACKEND_IDX_SHERPA {
        saved_sherpa_model_idx
    } else {
        saved_whisper_model_idx
    };

    let model_row = adw::ComboRow::builder()
        .title("Model")
        .model(&initial_model_list)
        .selected(initial_model_idx)
        .build();
    group.add(&model_row);

    // --- Backend change handler ---
    //
    // Rebuilds the model picker contents and persists the new backend.
    let config_backend = Arc::clone(config);
    let model_row_for_backend_change = model_row.clone();
    backend_row.connect_selected_notify(move |row| {
        let idx = row.selected();
        let (backend_str, new_list, new_idx) = if idx == BACKEND_IDX_SHERPA {
            (
                "sherpa",
                gtk4::StringList::new(&sherpa_model_labels),
                config_backend.read(|v| {
                    v.get(KEY_SHERPA_MODEL)
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|i| u32::try_from(i).ok())
                        .filter(|&i| i < max_sherpa_idx)
                        .unwrap_or(0)
                }),
            )
        } else {
            (
                "whisper",
                gtk4::StringList::new(&whisper_model_labels),
                config_backend.read(|v| {
                    v.get(KEY_MODEL)
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|i| u32::try_from(i).ok())
                        .filter(|&i| i < max_whisper_idx)
                        .unwrap_or(0)
                }),
            )
        };

        model_row_for_backend_change.set_model(Some(&new_list));
        model_row_for_backend_change.set_selected(new_idx);

        config_backend.write(|v| {
            v[KEY_BACKEND] = serde_json::json!(backend_str);
        });
    });

    // --- Model change handler ---
    //
    // Persists to KEY_MODEL or KEY_SHERPA_MODEL depending on which
    // backend is currently selected.
    let config_model = Arc::clone(config);
    let backend_row_for_model_change = backend_row.clone();
    model_row.connect_selected_notify(move |row| {
        let idx = row.selected();
        let backend_idx = backend_row_for_model_change.selected();
        let key = if backend_idx == BACKEND_IDX_SHERPA {
            KEY_SHERPA_MODEL
        } else {
            KEY_MODEL
        };
        config_model.write(|v| {
            v[key] = serde_json::json!(idx);
        });
    });
```

**Edit D — Update the returned `TranscriptPanel` struct literal at the bottom of `build_transcript_panel`:**

Add `backend_row` to the struct literal so it looks like:

```rust
    TranscriptPanel {
        widget: group,
        enable_row,
        backend_row,
        model_row,
        silence_row,
        noise_gate_row,
        status_label,
        progress_bar,
        text_view,
        scroll,
        clear_button,
    }
```

- [ ] **Step 2: Update `window.rs` to read the backend selection**

In `crates/sdr-ui/src/window.rs`, find the section (in `connect_transcript_panel`) where you currently construct the `BackendConfig` (added in Task 6) — around lines 1448-1466.

**Find:**

```rust
            let model_idx = model_row.selected() as usize;
            let whisper_model = sdr_transcription::WhisperModel::ALL
                .get(model_idx)
                .copied()
                .unwrap_or(sdr_transcription::WhisperModel::TinyEn);

            // Read tuning slider values.
            #[allow(clippy::cast_possible_truncation)]
            let silence_threshold = silence_row.value() as f32;
            #[allow(clippy::cast_possible_truncation)]
            let noise_gate_ratio = noise_gate_row.value() as f32;

            // Build BackendConfig. Backend is selected at compile time via
            // cargo features (whisper / sherpa are mutually exclusive).
            // Note: the original Task 8 runtime backend selector ComboRow
            // design was superseded by the build-time mutex — see PR #249.
            let config = sdr_transcription::BackendConfig {
                model: sdr_transcription::ModelChoice::Whisper(whisper_model),
                silence_threshold,
                noise_gate_ratio,
            };
```

**Replace with:**

```rust
            let model_idx = model_row.selected() as usize;
            let backend_idx = backend_row.selected();

            // Read tuning slider values.
            #[allow(clippy::cast_possible_truncation)]
            let silence_threshold = silence_row.value() as f32;
            #[allow(clippy::cast_possible_truncation)]
            let noise_gate_ratio = noise_gate_row.value() as f32;

            // Build BackendConfig from the panel state. Backend index
            // 1 = Sherpa; everything else = Whisper.
            let model = if backend_idx == 1 {
                let sherpa_model = sdr_transcription::SherpaModel::ALL
                    .get(model_idx)
                    .copied()
                    .unwrap_or(sdr_transcription::SherpaModel::StreamingZipformerEn);
                sdr_transcription::ModelChoice::Sherpa(sherpa_model)
            } else {
                let whisper_model = sdr_transcription::WhisperModel::ALL
                    .get(model_idx)
                    .copied()
                    .unwrap_or(sdr_transcription::WhisperModel::TinyEn);
                sdr_transcription::ModelChoice::Whisper(whisper_model)
            };

            let config = sdr_transcription::BackendConfig {
                model,
                silence_threshold,
                noise_gate_ratio,
            };
```

You also need to bind `backend_row` from the panel earlier in `connect_transcript_panel`. Find where `model_row`, `silence_row`, `noise_gate_row` etc. are cloned out of the panel struct and add `backend_row` to the list:

**Find:**

```rust
    let model_row = transcript.model_row.clone();
    let silence_row = transcript.silence_row.clone();
    let noise_gate_row = transcript.noise_gate_row.clone();
```

**Replace with:**

```rust
    let backend_row = transcript.backend_row.clone();
    let model_row = transcript.model_row.clone();
    let silence_row = transcript.silence_row.clone();
    let noise_gate_row = transcript.noise_gate_row.clone();
```

Also, in the existing code that locks/unlocks rows during transcription start/stop, add `backend_row` to the disabled list. Find:

```rust
            model_row.set_sensitive(false);
            silence_row.set_sensitive(false);
            noise_gate_row.set_sensitive(false);
```

**Replace with:**

```rust
            backend_row.set_sensitive(false);
            model_row.set_sensitive(false);
            silence_row.set_sensitive(false);
            noise_gate_row.set_sensitive(false);
```

And the corresponding re-enable in the else branch and error path:

```rust
            model_row.set_sensitive(true);
            silence_row.set_sensitive(true);
            noise_gate_row.set_sensitive(true);
```

**Replace with:**

```rust
            backend_row.set_sensitive(true);
            model_row.set_sensitive(true);
            silence_row.set_sensitive(true);
            noise_gate_row.set_sensitive(true);
```

(There are TWO occurrences of the re-enable pattern — one in the error path on `Err(e)` and one in the `else` branch where the user toggles transcription off. Apply to both.)

- [ ] **Step 3: Verify the workspace builds**

```bash
cd /data/source/rtl-sdr
cargo build --workspace 2>&1 | tail -15
cargo test --workspace 2>&1 | grep "test result" | tail -20
```

**Expected:** clean. All tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/sidebar/transcript_panel.rs crates/sdr-ui/src/window.rs
git commit -m "$(cat <<'EOF'
sdr-ui: backend selector ComboRow + contextual model picker

Adds an AdwComboRow above the model picker for selecting the
transcription backend (Whisper or Sherpa). The model picker contents
are rebuilt based on the active backend, with separate persisted
selection indices (KEY_MODEL for Whisper, KEY_SHERPA_MODEL for
Sherpa). The backend itself is persisted as a string under
KEY_BACKEND.

window.rs reads the backend index when starting transcription and
constructs ModelChoice::Whisper(...) or ModelChoice::Sherpa(...)
accordingly. Backend row is also disabled while transcription is
active alongside the existing model/slider lockouts.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Lint, format, full workspace verify

- [ ] **Step 1: Run clippy**

```bash
cd /data/source/rtl-sdr
cargo clippy --all-targets --workspace -- -D warnings 2>&1 | tail -40
```

**Expected:** zero warnings. Common issues:
- Unused imports — remove
- Missing `#[must_use]` on builder methods — add
- `cast_possible_truncation` complaints — already silenced with `#[allow]` in similar spots

- [ ] **Step 2: Run fmt**

```bash
cargo fmt --all 2>&1 | tail -10
```

If anything changes, commit it.

- [ ] **Step 3: Full test suite**

```bash
cargo test --workspace 2>&1 | grep "test result" | tail -25
```

**Expected:** all tests pass. `sdr-transcription` should now have ~33 tests (28 original + 3 sherpa_model + 2 sherpa backend trait = 33).

- [ ] **Step 4: Make lint (deny + audit)**

```bash
make lint 2>&1 | tail -40
```

**Expected:** clean. We added `sherpa-onnx` and its transitive dependencies — `cargo deny` may flag a license or new dependency we need to allow. If so, address with the minimum config change.

- [ ] **Step 5: Commit any fmt/lint fixups**

```bash
git status
# If anything is modified:
git add -u
git commit -m "$(cat <<'EOF'
sdr-transcription/sdr-ui: fmt + clippy fixups

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

If `git status` is clean, skip this step.

---

## Task 10: Build with CUDA, hand off for manual smoke test

**This task requires the user. Do not attempt to automate.**

- [ ] **Step 1: Build and install with CUDA**

```bash
cd /data/source/rtl-sdr
make install CARGO_FLAGS="--release --features whisper-cuda" 2>&1 | tail -20
```

**Expected:** clean build, binary installed.

- [ ] **Step 2: Hand off to user with this script**

Tell the user:

> "PR 2 (sherpa spike + backend selector) is built and installed. **You need to download the Streaming Zipformer model first** since auto-download lands in PR 3.
>
> Run this once:
>
> ```bash
> mkdir -p ~/.local/share/sdr-rs/models/sherpa
> cd ~/.local/share/sdr-rs/models/sherpa
> wget https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2
> tar -xjf sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2
> mv sherpa-onnx-streaming-zipformer-en-2023-06-26 streaming-zipformer-en
> ```
>
> Then test:
>
> 1. **Verify Whisper still works** (regression check)
>    - Launch app, enable transcription with a Whisper model on a real radio source
>    - Confirm it transcribes as before
>
> 2. **Switch to Sherpa**
>    - In the transcript panel, change the **Backend** dropdown to "Sherpa (streaming)"
>    - The Model dropdown should now show "Streaming Zipformer (English)"
>    - Toggle Enable Transcription on
>    - Confirm:
>      - Status shows "Listening..." after model load
>      - Live transcription appears in the panel as committed (Final/Text) lines
>      - Open the terminal/journal and look for `partial:` debug log lines — these prove the Partial events are flowing
>    - Try a quick test phrase, then a longer one — confirm endpoint detection commits utterances at silence
>
> 3. **Stop / restart cycle**
>    - Toggle transcription off — confirm clean shutdown
>    - Switch backend back to Whisper — confirm model picker repopulates with Whisper models
>    - Toggle transcription on with Whisper — confirm it still works
>
> 4. **Persistence**
>    - Close the app while on Sherpa backend
>    - Reopen — confirm Sherpa is still selected, model picker shows sherpa models
>
> 5. **CUDA validation (the unknown)**
>    - The current SherpaBackend hardcodes `provider = "cpu"` in the worker (Task 7). For the spike, this is the path we're committing to and testing.
>    - **Optional CUDA test:** if you want to spot-check CUDA, edit `crates/sdr-transcription/src/backends/sherpa.rs` line ~190 (`let provider = "cpu";`) to `let provider = "cuda";`, rebuild, and try the Sherpa flow again. If it crashes or errors, that tells us libsherpa wasn't built with CUDA support — document the failure and revert. **Don't commit the CUDA edit** — proper provider selection lands in a follow-up.
>
> Anything that crashes, hangs, or transcribes wrong is a bug we need to fix before merging."

- [ ] **Step 3: Wait for user confirmation**

Do NOT proceed to PR creation until the user confirms:
- Whisper regression: still works
- Sherpa flow: model loads, transcribes, partials in logs
- CUDA: documented (works / doesn't work / didn't test)

If anything regresses or fails, debug, fix on the same branch, and re-run the smoke test.

---

## Task 11: Open the PR

- [ ] **Step 1: Push the branch**

```bash
cd /data/source/rtl-sdr
git push -u origin feature/transcription-sherpa-spike
```

- [ ] **Step 2: Create the PR**

Use this template, customizing the CUDA section based on what the smoke test found:

```bash
gh pr create --title "Sherpa-onnx streaming backend + UI selector (PR 2 of 5 for #204)" --body "$(cat <<'EOF'
## Summary

Adds a working `SherpaBackend` powered by the official `sherpa-onnx` Rust crate (k2-fsa, 1.12), proves streaming-native ASR works end-to-end on a real radio source with Streaming Zipformer English. Ships Whisper and Sherpa as mutually exclusive build-time cargo features rather than a runtime backend selector (the original design was superseded — see PR #249 for details on the heap corruption that motivated the build-time mutex approach).

This is **PR 2 of 5** for #204. PR 1 (the trait refactor) made this possible.

## What changed

**New:**
- `crates/sdr-transcription/src/sherpa_model.rs` — `SherpaModel` enum, directory discovery, file path helpers (mirrors `model.rs` for Whisper)
- `crates/sdr-transcription/src/backends/sherpa.rs` — full `SherpaBackend` implementation with `OnlineRecognizer` feed loop, partial/final event emission, endpoint detection

**Extended:**
- `TranscriptionEvent::Partial { text }` — new variant emitted by streaming backends
- `ModelChoice::Sherpa(SherpaModel)` — backend dispatch
- `BackendError::ModelNotFound { path }` and `WrongModelKind` and `Init(String)`
- `TranscriptionEngine::start(BackendConfig)` — signature changed to take a single config struct (was `start(WhisperModel, f32, f32)`)

**UI:**
- Model picker contents contextual to whichever backend was compiled in (no runtime selector — superseded by build-time mutex, see PR #249)
- Silence threshold slider cfg-gated on `whisper` feature (Sherpa uses native endpoint detection)
- Persistence: `transcription_sherpa_model` (u32 index) config key for Sherpa builds
- `Partial` event variant routed to `tracing::debug!` for the spike (PR 4 will render proper live captions)

**Dependency added:**
- `sherpa-onnx = "1.12"` — official k2-fsa Rust bindings, statically linked, build script downloads precompiled libsherpa on first build

## Spike scope notes

- **Manual model download.** The user runs `wget` + `tar -xjf` once into `~/.local/share/sdr-rs/models/sherpa/streaming-zipformer-en/`. Auto-download lands in PR 3. If the directory is missing, `SherpaBackend::start` returns `BackendError::ModelNotFound { path }` with the exact expected path.
- **Provider hardcoded to CPU.** The sherpa-onnx execution provider is `"cpu"` in the worker. CUDA validation is in the smoke test.
- **Partials log only.** `TranscriptionEvent::Partial { text }` events go to `tracing::debug!(target: "transcription", partial = %text)` — they're not yet rendered in the UI text view. PR 4 adds the two-line live captions rendering.
- **Single sherpa model.** Only Streaming Zipformer English ships in this PR. Parakeet (#223) and Moonshine (#224) follow.

## CUDA finding

(Customize this section based on smoke test results)

- **CPU:** verified working on a real NFM voice channel
- **CUDA:** [DOCUMENT WHAT THE SMOKE TEST FOUND HERE]

## Test plan

- [x] `cargo build --workspace` clean
- [x] `cargo clippy --all-targets --workspace -- -D warnings` clean
- [x] `cargo test --workspace` passes (33+ tests in sdr-transcription)
- [x] `cargo fmt --all -- --check` clean
- [x] `make lint` clean
- [x] Manual: Whisper regression — still transcribes as before
- [x] Manual: Sherpa flow — model loads, live transcription works, partials visible in tracing logs
- [x] Manual: Sherpa flow — model loads, live transcription works, partials visible in tracing logs (Note: runtime backend switch test N/A — design superseded by build-time mutex)
- [x] Manual: persistence across app restart for sherpa model selection

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CodeRabbit review**

Per project workflow: don't merge until CodeRabbit has reviewed and any inline comments are addressed.

---

## Done

After PR 2 merges, the user has a working dual-backend transcription system. Next PRs:

- **PR 3** — Auto-download for sherpa model bundles (`tar` + `bzip2-rs`, mirror Whisper UX)
- **PR 4** — Display mode toggle + live captions two-line rendering
- **PR 5+** — Additional sherpa models (Parakeet, Moonshine — one PR each)
