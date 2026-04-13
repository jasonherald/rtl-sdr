# Moonshine Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Moonshine Tiny + Moonshine Base models to the Sherpa backend via an offline (VAD-gated, batch-decode) session path, alongside the existing Zipformer streaming path. Introduces a feature-agnostic `VoiceActivityDetector` trait, splits `backends/sherpa.rs` into a focused module, and walks back PR 4's `display_mode_row` mid-session exception in favor of "all settings lock during session."

**Architecture:** `SherpaHost` grows a second recognizer flavor (offline) driven by Silero VAD. Host init branches on `SherpaModel::kind()` at the top, running either the existing streaming path or a new offline path that downloads both the VAD model and the Moonshine bundle before creating an `OfflineRecognizer` + `SherpaSileroVad`. The offline session loop buffers audio, feeds VAD, and batch-decodes each completed speech segment through the `OfflineRecognizer`, emitting only `Text` events (no partials).

**Tech Stack:** Rust 2024, gtk4-rs 0.11, libadwaita, sherpa-onnx 1.12.36 (offline_asr + vad modules), existing `TranscriptionBackend` / `TranscriptionEvent` trait in `sdr-transcription`, `sdr_config::ConfigManager`.

---

## File Structure

**New files:**
- `crates/sdr-transcription/src/vad.rs` — feature-agnostic `VoiceActivityDetector` trait (compiles in whisper builds too, as a pure trait definition with no impls)
- `crates/sdr-transcription/src/backends/sherpa/mod.rs` — `SherpaBackend` facade (struct + `TranscriptionBackend` impl), module-level tests
- `crates/sdr-transcription/src/backends/sherpa/host.rs` — `SherpaHost` + `spawn` + `run_host_loop` + `SHERPA_HOST` `OnceLock` + shared constants + `init_sherpa_host` pub entry
- `crates/sdr-transcription/src/backends/sherpa/streaming.rs` — `run_session` for `OnlineRecognizer` (Zipformer), `build_recognizer_config`, `finalize_session`
- `crates/sdr-transcription/src/backends/sherpa/offline.rs` — `run_session_offline` for `OfflineRecognizer` + VAD batch loop (new)
- `crates/sdr-transcription/src/backends/sherpa/silero_vad.rs` — `SherpaSileroVad` impl of `VoiceActivityDetector` (sherpa-onnx-backed)

**Removed file:**
- `crates/sdr-transcription/src/backends/sherpa.rs` — content split into the new `sherpa/` module

**Modified files:**
- `crates/sdr-transcription/src/lib.rs` — expose `vad` module; no changes to pub-use paths (`pub use backends::sherpa::init_sherpa_host` still resolves via `sherpa/mod.rs`)
- `crates/sdr-transcription/src/sherpa_model.rs` — add `ModelKind` enum, `ModelFilePaths` enum (replaces `(encoder, decoder, joiner, tokens)` tuple), `Moonshine{Tiny,Base}En` variants, `SherpaModel::supports_partials` + `::kind` methods, Silero VAD download helpers
- `crates/sdr-transcription/src/init_event.rs` — `DownloadStart` + `Extracting` carry a `component: &'static str` field
- `src/main.rs` — update splash label mapping to use the new `component` field
- `crates/sdr-ui/src/sidebar/transcript_panel.rs` — contextual `display_mode_row` visibility based on selected model's `supports_partials()`
- `crates/sdr-ui/src/window.rs` — lock `display_mode_row` during transcription alongside other settings rows

---

## Task 1: Feature-agnostic VoiceActivityDetector trait

**Files:**
- Create: `crates/sdr-transcription/src/vad.rs`
- Modify: `crates/sdr-transcription/src/lib.rs`

- [ ] **Step 1: Create `src/vad.rs` with the trait definition**

Write this exact content to `crates/sdr-transcription/src/vad.rs`:

```rust
//! Feature-agnostic voice activity detection trait.
//!
//! This module is deliberately NOT gated on any backend cargo feature —
//! the trait compiles in whisper builds, sherpa builds, and any future
//! combination. The sherpa-onnx-backed impl lives in
//! `backends/sherpa/silero_vad.rs` behind `#[cfg(feature = "sherpa")]`,
//! and a Whisper impl (pure-Rust Silero) is a follow-up PR.
//!
//! Callers feed 16 kHz mono samples via [`VoiceActivityDetector::accept`]
//! and poll [`VoiceActivityDetector::pop_segment`] to pull completed
//! speech segments. Segments are owned `Vec<f32>` so the caller can
//! move them into a batch decoder without re-allocating.

/// Queue-based voice activity detector.
///
/// The implementation is expected to buffer input internally and emit
/// one segment per detected utterance. Segments contain only voiced
/// frames; silence is already trimmed by the detector.
pub trait VoiceActivityDetector {
    /// Feed 16 kHz mono samples. The detector buffers internally and
    /// may emit one or more completed segments after this call.
    fn accept(&mut self, samples: &[f32]);

    /// Pop the next completed speech segment if one is ready.
    /// Returns `None` when the internal queue is empty.
    fn pop_segment(&mut self) -> Option<Vec<f32>>;

    /// Drop all buffered audio and reset detector state.
    /// Called between transcription sessions.
    fn reset(&mut self);
}
```

- [ ] **Step 2: Register the module in `lib.rs`**

In `crates/sdr-transcription/src/lib.rs`, find the existing `pub mod` declarations:

```rust
pub mod backend;
pub mod backends;
pub mod denoise;
pub mod resampler;
pub mod util;
```

Add `pub mod vad;` immediately after `pub mod util;`:

```rust
pub mod backend;
pub mod backends;
pub mod denoise;
pub mod resampler;
pub mod util;
pub mod vad;
```

- [ ] **Step 3: Add a smoke test that the trait is object-safe**

Append this test module to `crates/sdr-transcription/src/vad.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check that the trait is object-safe so it can be
    /// used as `Box<dyn VoiceActivityDetector>` or `&dyn VoiceActivityDetector`.
    /// If someone adds a generic method that breaks object safety this
    /// test will fail to compile.
    #[test]
    fn trait_is_object_safe() {
        fn takes_dyn(_: &mut dyn VoiceActivityDetector) {}
        struct Noop;
        impl VoiceActivityDetector for Noop {
            fn accept(&mut self, _: &[f32]) {}
            fn pop_segment(&mut self) -> Option<Vec<f32>> { None }
            fn reset(&mut self) {}
        }
        let mut noop = Noop;
        takes_dyn(&mut noop);
    }
}
```

- [ ] **Step 4: Verify both builds compile cleanly**

Run:
```bash
cargo build --workspace 2>&1 | tail -10
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo test -p sdr-transcription vad 2>&1 | tail -15
```
Expected: both builds PASS, `trait_is_object_safe` test PASSES.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-transcription/src/vad.rs crates/sdr-transcription/src/lib.rs
git commit -m "feat(transcription): add VoiceActivityDetector trait

Feature-agnostic trait in sdr-transcription/src/vad.rs that compiles
in both whisper and sherpa builds. Sherpa-onnx-backed impl comes in
a later task; Whisper retrofit is a follow-up PR.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: ModelKind enum and SherpaModel methods

**Files:**
- Modify: `crates/sdr-transcription/src/sherpa_model.rs`

- [ ] **Step 1: Add the `ModelKind` enum at the top of `sherpa_model.rs`**

In `crates/sdr-transcription/src/sherpa_model.rs`, find the line:

```rust
/// Available sherpa-onnx model variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SherpaModel {
```

Immediately before that `/// Available sherpa-onnx model variants.` comment, insert:

```rust
/// Which sherpa-onnx recognizer family a model belongs to.
///
/// Drives host init branching and session loop dispatch. Online
/// models run through `OnlineRecognizer` + streaming chunks;
/// offline models run through `OfflineRecognizer` + external VAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// Streaming transducer: Zipformer today, Parakeet-TDT in a future PR.
    /// Uses `OnlineRecognizer` + streaming session loop.
    OnlineTransducer,
    /// Offline encoder-decoder: Moonshine v2. Requires external VAD
    /// to detect utterance boundaries before batch decoding.
    OfflineMoonshine,
}

```

- [ ] **Step 2: Add `kind()` and `supports_partials()` methods to `SherpaModel`**

Find the `impl SherpaModel {` block and locate the existing method `pub fn archive_url(self) -> String`. After that method (and before `pub const ALL:`), insert:

```rust
    /// Which recognizer family this model uses.
    ///
    /// The host worker branches on this at init time to pick the
    /// right recognizer type and session loop.
    pub fn kind(self) -> ModelKind {
        match self {
            Self::StreamingZipformerEn => ModelKind::OnlineTransducer,
        }
    }

    /// True if this model emits intermediate hypothesis updates
    /// (`TranscriptionEvent::Partial`) during speech.
    ///
    /// Drives contextual UI: the "Display mode" (Live/Final) toggle
    /// only appears for models that return `true` here. Offline
    /// models decode once per utterance so partials are not
    /// meaningful.
    pub fn supports_partials(self) -> bool {
        match self.kind() {
            ModelKind::OnlineTransducer => true,
            ModelKind::OfflineMoonshine => false,
        }
    }

```

- [ ] **Step 3: Add unit tests for the new methods**

In the existing `#[cfg(test)] mod tests` block at the bottom of `sherpa_model.rs`, add these three tests inside the block:

```rust
    #[test]
    fn zipformer_is_online_transducer() {
        assert_eq!(
            SherpaModel::StreamingZipformerEn.kind(),
            ModelKind::OnlineTransducer
        );
    }

    #[test]
    fn online_transducer_supports_partials() {
        assert!(SherpaModel::StreamingZipformerEn.supports_partials());
    }

    #[test]
    fn supports_partials_is_derived_from_kind() {
        // Sanity check that supports_partials mirrors the kind match —
        // if anyone adds a new ModelKind variant they have to update
        // supports_partials too, and this test locks that relationship.
        for model in SherpaModel::ALL {
            let expected = matches!(model.kind(), ModelKind::OnlineTransducer);
            assert_eq!(model.supports_partials(), expected, "mismatch for {model:?}");
        }
    }
```

- [ ] **Step 4: Verify sherpa build + tests**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu sherpa_model 2>&1 | tail -20
cargo build --workspace 2>&1 | tail -10
```
Expected: sherpa build PASS, 3 new tests PASS, whisper build PASS (new items are `#[cfg(feature = "sherpa")]` gated via the containing module).

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-transcription/src/sherpa_model.rs
git commit -m "feat(transcription): add ModelKind and SherpaModel methods

Adds ModelKind enum (OnlineTransducer / OfflineMoonshine) and two
SherpaModel methods — kind() and supports_partials() — that let
callers branch on the recognizer family without hardcoding variant
checks. Zipformer returns OnlineTransducer + supports_partials=true.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: ModelFilePaths enum refactor

**Files:**
- Modify: `crates/sdr-transcription/src/sherpa_model.rs`
- Modify: `crates/sdr-transcription/src/backends/sherpa.rs`

- [ ] **Step 1: Add the `ModelFilePaths` enum to `sherpa_model.rs`**

In `crates/sdr-transcription/src/sherpa_model.rs`, locate the existing function:

```rust
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
```

Replace it with this:

```rust
/// Concrete filesystem paths for every file a sherpa model needs on disk.
///
/// Each recognizer family has a different layout. The enum variants
/// match the families in [`ModelKind`]: transducer models (Zipformer,
/// Parakeet-TDT) ship four files, Moonshine v2 ships three.
#[derive(Debug, Clone)]
pub enum ModelFilePaths {
    Transducer {
        encoder: PathBuf,
        decoder: PathBuf,
        joiner: PathBuf,
        tokens: PathBuf,
    },
}

/// Returns the full paths for all files needed by a sherpa model.
///
/// The returned variant matches the model's [`ModelKind`]. The caller
/// is expected to pattern-match on the variant and pass the paths into
/// the right `sherpa_onnx` config (transducer vs moonshine).
pub fn model_file_paths(model: SherpaModel) -> ModelFilePaths {
    match model.kind() {
        ModelKind::OnlineTransducer => {
            let dir = model_directory(model);
            ModelFilePaths::Transducer {
                encoder: dir.join(model.encoder_filename()),
                decoder: dir.join(model.decoder_filename()),
                joiner: dir.join(model.joiner_filename()),
                tokens: dir.join(model.tokens_filename()),
            }
        }
        ModelKind::OfflineMoonshine => {
            // Added in a later task when Moonshine variants exist.
            // This arm is unreachable today because no SherpaModel
            // variant returns ModelKind::OfflineMoonshine yet.
            unreachable!("OfflineMoonshine has no variants yet — see plan Task 7")
        }
    }
}
```

- [ ] **Step 2: Update `model_exists` to use the new enum**

In the same file, locate:

```rust
/// True if all four required files for `model` exist on disk.
pub fn model_exists(model: SherpaModel) -> bool {
    let (e, d, j, t) = model_file_paths(model);
    e.is_file() && d.is_file() && j.is_file() && t.is_file()
}
```

Replace with:

```rust
/// True if every file required by `model` exists on disk.
pub fn model_exists(model: SherpaModel) -> bool {
    match model_file_paths(model) {
        ModelFilePaths::Transducer { encoder, decoder, joiner, tokens } => {
            encoder.is_file() && decoder.is_file() && joiner.is_file() && tokens.is_file()
        }
    }
}
```

- [ ] **Step 3: Update the one caller in `backends/sherpa.rs::build_recognizer_config`**

In `crates/sdr-transcription/src/backends/sherpa.rs`, locate:

```rust
fn build_recognizer_config(model: SherpaModel, provider: &str) -> OnlineRecognizerConfig {
    let (encoder, decoder, joiner, tokens) = sherpa_model::model_file_paths(model);

    let mut config = OnlineRecognizerConfig::default();
    config.model_config.transducer.encoder = Some(encoder.to_string_lossy().into_owned());
    config.model_config.transducer.decoder = Some(decoder.to_string_lossy().into_owned());
    config.model_config.transducer.joiner = Some(joiner.to_string_lossy().into_owned());
    config.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
```

Replace with:

```rust
fn build_recognizer_config(model: SherpaModel, provider: &str) -> OnlineRecognizerConfig {
    let ModelFilePaths::Transducer { encoder, decoder, joiner, tokens } =
        sherpa_model::model_file_paths(model);

    let mut config = OnlineRecognizerConfig::default();
    config.model_config.transducer.encoder = Some(encoder.to_string_lossy().into_owned());
    config.model_config.transducer.decoder = Some(decoder.to_string_lossy().into_owned());
    config.model_config.transducer.joiner = Some(joiner.to_string_lossy().into_owned());
    config.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
```

Note: this uses an irrefutable `let` pattern because there is only one `ModelFilePaths` variant today. The compiler will warn `irrefutable let pattern` — that's expected and OK for now; the warning goes away in Task 7 when we add the `Moonshine` variant. Allow the warning locally for this single let binding:

```rust
fn build_recognizer_config(model: SherpaModel, provider: &str) -> OnlineRecognizerConfig {
    // Irrefutable today — will become refutable when Moonshine variant lands (plan Task 7).
    #[allow(irrefutable_let_patterns)]
    let ModelFilePaths::Transducer { encoder, decoder, joiner, tokens } =
        sherpa_model::model_file_paths(model)
    else {
        unreachable!("StreamingZipformerEn is always a Transducer")
    };
```

Also add the import at the top of `backends/sherpa.rs`:

```rust
use crate::sherpa_model::{self, ModelFilePaths, SherpaModel};
```

(the current import is `use crate::sherpa_model::{self, SherpaModel};` — just add `ModelFilePaths` to that use list).

- [ ] **Step 4: Update the existing `model_file_paths_returns_four_distinct_files` test to match the new shape**

In `crates/sdr-transcription/src/sherpa_model.rs`, find the test:

```rust
    #[test]
    fn model_file_paths_returns_four_distinct_files() {
        let (e, d, j, t) = model_file_paths(SherpaModel::StreamingZipformerEn);
        assert_ne!(e, d);
        ...
```

Replace with:

```rust
    #[test]
    fn transducer_model_file_paths_returns_four_distinct_files() {
        let ModelFilePaths::Transducer { encoder, decoder, joiner, tokens } =
            model_file_paths(SherpaModel::StreamingZipformerEn)
        else {
            panic!("StreamingZipformerEn should be a Transducer layout");
        };
        assert_ne!(encoder, decoder);
        assert_ne!(encoder, joiner);
        assert_ne!(encoder, tokens);
        assert_ne!(decoder, joiner);
        assert_ne!(decoder, tokens);
        assert_ne!(joiner, tokens);
    }
```

- [ ] **Step 5: Verify builds + tests**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu 2>&1 | tail -20
cargo build --workspace 2>&1 | tail -10
```
Expected: sherpa build PASS, tests PASS, whisper build PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-transcription/src/sherpa_model.rs crates/sdr-transcription/src/backends/sherpa.rs
git commit -m "refactor(transcription): model_file_paths returns ModelFilePaths enum

Replaces the (encoder, decoder, joiner, tokens) 4-tuple return with
a ModelFilePaths enum. Currently only the Transducer variant exists
— Moonshine variant lands in a later task. Caller in
build_recognizer_config unpacks via irrefutable let pattern, which
becomes refutable once Moonshine ships.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: InitEvent component labels

**Files:**
- Modify: `crates/sdr-transcription/src/init_event.rs`
- Modify: `crates/sdr-transcription/src/backends/sherpa.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Update `InitEvent` variants to carry component labels**

In `crates/sdr-transcription/src/init_event.rs`, replace the enum definition with:

```rust
/// Progress events from the sherpa-onnx host worker thread during
/// initialization. The worker emits these in order; the final event
/// is always either `Ready` or `Failed`.
///
/// `DownloadStart` and `Extracting` carry a `component` label so the
/// splash window can show which artifact is currently being processed
/// (e.g. "Silero VAD", "Streaming Zipformer (English)", "Moonshine Tiny").
#[derive(Debug, Clone)]
pub enum InitEvent {
    /// A sherpa artifact is missing locally; download is starting.
    /// `component` is a human-readable name rendered on the splash.
    DownloadStart { component: &'static str },
    /// Download progress (0..=100). Only fired during the download phase.
    DownloadProgress { pct: u8 },
    /// Download complete; extracting the archive.
    /// `component` matches the most recent `DownloadStart` payload.
    Extracting { component: &'static str },
    /// Extraction complete; constructing the recognizer.
    /// This is the longest step on the cached path (~1-2 seconds).
    CreatingRecognizer,
    /// The host is fully initialized and ready to accept sessions.
    /// `SHERPA_HOST` has been populated with Ok(host) by the worker.
    Ready,
    /// Initialization failed permanently. `SHERPA_HOST` has been
    /// populated with Err(error). The error message is intended for
    /// display to the user (e.g. via a status label or toast).
    Failed { message: String },
}
```

- [ ] **Step 2: Update the Zipformer emission sites in `backends/sherpa.rs::run_host_loop`**

In `crates/sdr-transcription/src/backends/sherpa.rs`, locate in `run_host_loop`:

```rust
        let _ = event_tx.send(InitEvent::DownloadStart);
```

Replace with:

```rust
        let _ = event_tx.send(InitEvent::DownloadStart {
            component: model.label(),
        });
```

Then locate:

```rust
        tracing::info!("sherpa archive download complete, extracting");
        let _ = event_tx.send(InitEvent::Extracting);
```

Replace with:

```rust
        tracing::info!("sherpa archive download complete, extracting");
        let _ = event_tx.send(InitEvent::Extracting {
            component: model.label(),
        });
```

- [ ] **Step 3: Update the splash driver in `src/main.rs`**

In `src/main.rs`, locate the InitEvent match arms:

```rust
                Ok(InitEvent::DownloadStart) => {
                    tracing::info!("sherpa download starting");
                    splash.update_text("Downloading sherpa-onnx model...");
                }
                Ok(InitEvent::DownloadProgress { pct }) => {
                    tracing::info!(progress_pct = pct, "sherpa download progress");
                    splash.update_text(&format!("Downloading sherpa-onnx model... {pct}%"));
                }
                Ok(InitEvent::Extracting) => {
                    tracing::info!("sherpa extracting archive");
                    splash.update_text("Extracting sherpa-onnx model...");
                }
```

Replace the three arms with:

```rust
                Ok(InitEvent::DownloadStart { component }) => {
                    tracing::info!(%component, "sherpa download starting");
                    current_component = component;
                    splash.update_text(&format!("Downloading {component}..."));
                }
                Ok(InitEvent::DownloadProgress { pct }) => {
                    tracing::info!(progress_pct = pct, "sherpa download progress");
                    splash.update_text(&format!("Downloading {current_component}... {pct}%"));
                }
                Ok(InitEvent::Extracting { component }) => {
                    tracing::info!(%component, "sherpa extracting archive");
                    current_component = component;
                    splash.update_text(&format!("Extracting {component}..."));
                }
```

Then add the `current_component` variable before the loop. Find the line just before `loop {` (around line 69 in the current main.rs — immediately before `match event_rx.recv()`):

```rust
        loop {
            match event_rx.recv() {
```

Change to:

```rust
        let mut current_component: &'static str = "sherpa-onnx";
        loop {
            match event_rx.recv() {
```

The initial value `"sherpa-onnx"` is only a fallback if a `DownloadProgress` somehow arrives before any `DownloadStart` — the worker never does this, but we handle it defensively.

- [ ] **Step 4: Verify both builds + run the binary briefly**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo build --workspace 2>&1 | tail -10
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -10
cargo clippy --all-targets --workspace -- -D warnings 2>&1 | tail -10
```
Expected: all PASS. Whisper build is unaffected because main.rs only consumes `InitEvent` under `#[cfg(feature = "sherpa")]`.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-transcription/src/init_event.rs crates/sdr-transcription/src/backends/sherpa.rs src/main.rs
git commit -m "feat(transcription): InitEvent carries component labels

DownloadStart and Extracting now carry a component: &'static str
field so the splash can show which artifact is processing. Zipformer
init emits the model's label(); future Moonshine init will emit
'Silero VAD' and the Moonshine model label sequentially. Splash
driver in main.rs uses the component to format its label text.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Split `backends/sherpa.rs` into a module

**Files:**
- Remove: `crates/sdr-transcription/src/backends/sherpa.rs`
- Create: `crates/sdr-transcription/src/backends/sherpa/mod.rs`
- Create: `crates/sdr-transcription/src/backends/sherpa/host.rs`
- Create: `crates/sdr-transcription/src/backends/sherpa/streaming.rs`

This is a mechanical refactor with no behavioral changes. The module is re-exported via `pub use backends::sherpa::init_sherpa_host` in `lib.rs` which still resolves through `sherpa/mod.rs`, so no external API changes.

**Symbol destinations:**
- `SherpaBackend` struct + `Default`/`new`/`TranscriptionBackend` impls → `mod.rs`
- Module docs + `pub use` re-exports → `mod.rs`
- Module-level tests (`sherpa_backend_supports_partials`, `sherpa_backend_name_is_stable`, `sherpa_host_spawn_emits_download_start_when_files_missing`) → `mod.rs`
- Shared constants (`AUDIO_CHANNEL_CAPACITY`, `AUDIO_RECV_TIMEOUT`, `SHERPA_SAMPLE_RATE_HZ`) → `host.rs` (top), re-exported as `pub(super)` so siblings can reference them
- `SHERPA_HOST` `OnceLock` + `init_sherpa_host` + `global_sherpa_host` → `host.rs`
- `SessionParams`, `HostCommand`, `SherpaHostState`, `SherpaHost` struct + impl → `host.rs`
- `run_host_loop`, `store_init_failure` → `host.rs`
- `build_recognizer_config` → `streaming.rs`
- `run_session`, `finalize_session` → `streaming.rs`
- `SHERPA_NUM_THREADS`, `RULE1_MIN_TRAILING_SILENCE`, `RULE2_MIN_TRAILING_SILENCE`, `RULE3_MIN_UTTERANCE_LENGTH`, `SESSION_MONO_BUFFER_CAPACITY` → `streaming.rs` (these are streaming-specific)

- [ ] **Step 1: Remove the old file and create the new directory structure**

```bash
git rm crates/sdr-transcription/src/backends/sherpa.rs
mkdir -p crates/sdr-transcription/src/backends/sherpa
```

- [ ] **Step 2: Write `crates/sdr-transcription/src/backends/sherpa/mod.rs`**

```rust
//! Sherpa-onnx backend — streaming-native ASR via the official k2-fsa
//! `sherpa-onnx` Rust crate.
//!
//! ## Architecture
//!
//! The recognizer is created ONCE per process by [`host::SherpaHost`], a
//! long-lived worker thread spawned from `main()` BEFORE GTK is loaded.
//! This is a workaround for a C++ static-initializer collision between
//! sherpa-onnx's bundled ONNX Runtime and GTK4's transitive C++ deps —
//! creating the recognizer after GTK init causes `free(): invalid pointer`
//! inside `std::regex` constructors called by ONNX Runtime's
//! `ParseSemVerVersion`.
//!
//! [`SherpaBackend`] is a thin facade that asks the global host for a
//! new session. The host creates a fresh stream from the existing
//! recognizer and runs the audio feed loop until the session is
//! cancelled or the audio channel disconnects.
//!
//! ## Submodules
//!
//! - [`host`] owns the `SHERPA_HOST` `OnceLock`, the worker thread,
//!   and the init flow that downloads + creates the recognizer.
//! - [`streaming`] contains the `OnlineRecognizer` session loop used
//!   by Zipformer (and future transducer models like Parakeet).

mod host;
mod streaming;

pub use host::init_sherpa_host;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
};
use host::{AUDIO_CHANNEL_CAPACITY, SessionParams, global_sherpa_host};

/// `TranscriptionBackend` implementation backed by the global sherpa host.
///
/// `SherpaBackend` is stateless apart from a per-session cancellation flag.
/// All actual recognizer state lives on the long-lived host worker thread
/// spawned by [`init_sherpa_host`].
pub struct SherpaBackend {
    cancel: Arc<AtomicBool>,
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
        match config.model {
            ModelChoice::Sherpa(_) => {}
            #[cfg(feature = "whisper")]
            ModelChoice::Whisper(_) => return Err(BackendError::WrongModelKind),
        }

        let host = match global_sherpa_host() {
            Some(Ok(h)) => h,
            Some(Err(stored)) => {
                // Reconstruct a fresh BackendError so callers (and the UI)
                // see the original variant. ModelNotFound is the most
                // important case to preserve — it tells the user exactly
                // where to download the model bundle.
                return Err(match &**stored {
                    BackendError::ModelNotFound { path } => {
                        BackendError::ModelNotFound { path: path.clone() }
                    }
                    BackendError::Init(msg) => {
                        BackendError::Init(format!("sherpa host failed to initialize: {msg}"))
                    }
                    BackendError::Spawn(io_err) => {
                        BackendError::Init(format!(
                            "sherpa host worker thread spawn failed: {io_err}"
                        ))
                    }
                    BackendError::WrongModelKind => BackendError::WrongModelKind,
                });
            }
            None => {
                return Err(BackendError::Init(
                    "sherpa host not initialized — main() must call \
                     sdr_transcription::init_sherpa_host before sdr_ui::run"
                        .to_owned(),
                ));
            }
        };

        self.cancel.store(false, Ordering::Relaxed);

        let (audio_tx, audio_rx) = mpsc::sync_channel(AUDIO_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();

        host.start_session(SessionParams {
            cancel: Arc::clone(&self.cancel),
            audio_rx,
            event_tx,
            noise_gate_ratio: config.noise_gate_ratio,
        })?;

        tracing::info!("sherpa backend session requested");

        Ok(BackendHandle { audio_tx, event_rx })
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        tracing::info!("sherpa backend stopped");
    }

    fn shutdown_nonblocking(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        tracing::info!("sherpa backend shutdown (non-blocking)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sherpa_model::{self, SherpaModel};
    use crate::InitEvent;

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

    /// **Manual-only test.** Kicks off the real worker — which, if the
    /// model files are absent, proceeds past the first `DownloadStart`
    /// event into the full 256 MB download + extract path. See note on
    /// issues #250 and #255 about the hermetic testing follow-up.
    #[test]
    #[ignore = "spawns the real download worker; run manually with --ignored"]
    fn sherpa_host_spawn_emits_download_start_when_files_missing() {
        if sherpa_model::model_exists(SherpaModel::StreamingZipformerEn) {
            eprintln!("skipping test: streaming-zipformer-en model is present locally");
            return;
        }
        let event_rx = host::SherpaHost::spawn(SherpaModel::StreamingZipformerEn);
        let first_event = event_rx
            .recv()
            .expect("worker should send at least one event");
        assert!(
            matches!(first_event, InitEvent::DownloadStart { .. }),
            "expected DownloadStart when model is missing, got {first_event:?}"
        );
        drop(event_rx);
    }
}
```

- [ ] **Step 3: Write `crates/sdr-transcription/src/backends/sherpa/host.rs`**

```rust
//! Sherpa-onnx host worker thread — owns the recognizer for the
//! entire process lifetime.
//!
//! Spawned from `main()` before GTK init via [`init_sherpa_host`].
//! The worker populates the process-wide `SHERPA_HOST` `OnceLock`
//! then sits on a command channel waiting for session requests.

use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use sherpa_onnx::OnlineRecognizer;

use crate::backend::{BackendError, TranscriptionEvent};
use crate::init_event::InitEvent;
use crate::sherpa_model::{self, SherpaModel};

use super::streaming;

/// Bounded channel capacity for audio buffers from DSP → backend.
pub(super) const AUDIO_CHANNEL_CAPACITY: usize = 256;

/// Polling interval for the audio receive loop when checking for cancellation.
pub(super) const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// Sample rate sherpa-onnx expects from `accept_waveform`.
pub(super) const SHERPA_SAMPLE_RATE_HZ: i32 = 16_000;

/// Process-wide singleton for the sherpa-onnx host. Stores either a ready
/// host or the error message from a failed initialization. Set exactly once
/// by [`init_sherpa_host`]; subsequent calls are no-ops.
static SHERPA_HOST: OnceLock<Result<SherpaHost, Arc<BackendError>>> = OnceLock::new();

/// Spawn the global sherpa-onnx host thread and return a channel that
/// streams initialization progress events.
///
/// **MUST be called from `main()` BEFORE GTK is initialized** (before
/// `sdr_ui::run()`). The returned `Receiver<InitEvent>` MUST be drained
/// by the caller until it produces either `InitEvent::Ready` or
/// `InitEvent::Failed` — the worker populates the global `SHERPA_HOST`
/// `OnceLock` itself before emitting the final event, but `main()` needs
/// to block until that's done so the recognizer creation completes
/// before GTK loads.
pub fn init_sherpa_host(model: SherpaModel) -> mpsc::Receiver<InitEvent> {
    SherpaHost::spawn(model)
}

/// Look up the global sherpa host. Returns `None` if `init_sherpa_host` was
/// never called.
pub(super) fn global_sherpa_host() -> Option<&'static Result<SherpaHost, Arc<BackendError>>> {
    SHERPA_HOST.get()
}

/// Parameters handed to the host worker for one transcription session.
pub(super) struct SessionParams {
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
    pub audio_rx: mpsc::Receiver<Vec<f32>>,
    pub event_tx: mpsc::Sender<TranscriptionEvent>,
    pub noise_gate_ratio: f32,
}

/// Commands sent to the host worker thread.
enum HostCommand {
    StartSession(SessionParams),
}

/// Internal state of a sherpa host. Wrapped in a `Mutex` inside `SherpaHost`
/// because `mpsc::Sender` is `!Sync` and we need `SherpaHost: Sync` for
/// `OnceLock` storage.
struct SherpaHostState {
    cmd_tx: mpsc::Sender<HostCommand>,
}

/// Long-lived host for sherpa-onnx transcription. Owns one worker thread
/// that holds the [`OnlineRecognizer`] for the entire process lifetime.
pub(super) struct SherpaHost {
    state: Mutex<SherpaHostState>,
}

impl SherpaHost {
    /// Spawn the host worker thread and return immediately.
    ///
    /// See [`init_sherpa_host`] for details on the draining contract.
    pub(super) fn spawn(model: SherpaModel) -> mpsc::Receiver<InitEvent> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<HostCommand>();
        let (event_tx, event_rx) = mpsc::channel::<InitEvent>();

        std::thread::Builder::new()
            .name("sherpa-host".into())
            .spawn(move || {
                run_host_loop(model, &cmd_rx, cmd_tx, event_tx);
            })
            .expect("failed to spawn sherpa-host worker thread");

        event_rx
    }

    /// Send a `StartSession` command to the host. Returns an error if the
    /// host worker has died.
    pub(super) fn start_session(&self, params: SessionParams) -> Result<(), BackendError> {
        let state = self
            .state
            .lock()
            .map_err(|_| BackendError::Init("sherpa host mutex poisoned".to_owned()))?;
        state
            .cmd_tx
            .send(HostCommand::StartSession(params))
            .map_err(|_| BackendError::Init("sherpa host worker is no longer running".to_owned()))
    }
}

/// Worker thread entry point. Owns the recognizer for the entire
/// process lifetime and handles both initialization and command
/// processing.
///
/// Phase 1: download the model bundle if it's missing locally
/// Phase 2: create the `OnlineRecognizer`
/// Phase 3: store the `SherpaHost` in `SHERPA_HOST` and emit Ready
/// Phase 4: process `StartSession` commands forever
///
/// Failures during phases 1 or 2 store an error in `SHERPA_HOST` and
/// emit `InitEvent::Failed` before returning early.
fn run_host_loop(
    model: SherpaModel,
    cmd_rx: &mpsc::Receiver<HostCommand>,
    cmd_tx: mpsc::Sender<HostCommand>,
    event_tx: mpsc::Sender<InitEvent>,
) {
    // --- Phase 1: download if needed ---
    if !sherpa_model::model_exists(model) {
        tracing::info!(
            ?model,
            "sherpa model not found locally, downloading bundle (~256 MB)"
        );
        let _ = event_tx.send(InitEvent::DownloadStart {
            component: model.label(),
        });

        let (dl_tx, dl_rx) = mpsc::channel::<u8>();
        let event_tx_dl = event_tx.clone();

        let dl_forwarder = match std::thread::Builder::new()
            .name("sherpa-dl-progress".into())
            .spawn(move || {
                while let Ok(pct) = dl_rx.recv() {
                    let _ = event_tx_dl.send(InitEvent::DownloadProgress { pct });
                }
            }) {
            Ok(handle) => handle,
            Err(e) => {
                let msg = format!("failed to spawn sherpa-dl-progress thread: {e}");
                tracing::error!(%msg);
                store_init_failure(BackendError::Init(msg.clone()));
                let _ = event_tx.send(InitEvent::Failed { message: msg });
                return;
            }
        };

        let archive_result = sherpa_model::download_sherpa_archive(model, &dl_tx);

        drop(dl_tx);
        let _ = dl_forwarder.join();

        let archive_path = match archive_result {
            Ok(path) => path,
            Err(e) => {
                let msg = format!("sherpa model download failed: {e}");
                tracing::error!(%msg);
                store_init_failure(BackendError::Init(msg.clone()));
                let _ = event_tx.send(InitEvent::Failed { message: msg });
                return;
            }
        };

        tracing::info!("sherpa archive download complete, extracting");
        let _ = event_tx.send(InitEvent::Extracting {
            component: model.label(),
        });

        if let Err(e) = sherpa_model::extract_sherpa_archive(model, &archive_path) {
            let msg = format!("sherpa model extraction failed: {e}");
            tracing::error!(%msg);
            store_init_failure(BackendError::Init(msg.clone()));
            let _ = event_tx.send(InitEvent::Failed { message: msg });
            return;
        }

        tracing::info!("sherpa model installed, proceeding to recognizer init");
    }

    // --- Phase 2: create the recognizer ---
    let _ = event_tx.send(InitEvent::CreatingRecognizer);
    let recognizer_config = streaming::build_recognizer_config(model, "cpu");
    tracing::info!(?model, "creating sherpa-onnx recognizer (host init)");

    let Some(recognizer) = OnlineRecognizer::create(&recognizer_config) else {
        let msg = "OnlineRecognizer::create returned None — check model file paths".to_owned();
        tracing::error!(%msg);
        store_init_failure(BackendError::Init(msg.clone()));
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return;
    };
    tracing::info!("sherpa-onnx recognizer created successfully");

    // --- Phase 3: build SherpaHost and store in SHERPA_HOST ---
    let host = SherpaHost {
        state: Mutex::new(SherpaHostState { cmd_tx }),
    };
    if SHERPA_HOST.set(Ok(host)).is_err() {
        let msg = "sherpa host OnceLock was already set; this worker is unreachable".to_owned();
        tracing::error!(%msg);
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return;
    }
    tracing::info!("sherpa-host ready, signaling Ready event");
    let _ = event_tx.send(InitEvent::Ready);
    drop(event_tx);

    // --- Phase 4: command loop ---
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            HostCommand::StartSession(params) => {
                tracing::info!("sherpa-host: starting session");
                streaming::run_session(&recognizer, params);
                tracing::info!("sherpa-host: session ended");
            }
        }
    }
    tracing::info!("sherpa-host worker exiting");
}

/// Helper to store an initialization failure in the global `OnceLock`.
fn store_init_failure(err: BackendError) {
    let _ = SHERPA_HOST.set(Err(std::sync::Arc::new(err)));
}
```

- [ ] **Step 4: Write `crates/sdr-transcription/src/backends/sherpa/streaming.rs`**

```rust
//! Streaming session loop for Zipformer (and future Parakeet-TDT).
//!
//! Runs on the sherpa-host worker thread. Owns nothing — all state
//! lives in the caller-provided `OnlineRecognizer` reference and a
//! per-session `OnlineStream`.

use std::sync::atomic::Ordering;
use std::sync::mpsc;

use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig, OnlineStream};

use crate::backend::TranscriptionEvent;
use crate::sherpa_model::{self, ModelFilePaths, SherpaModel};
use crate::{denoise, resampler};

use super::host::{AUDIO_RECV_TIMEOUT, SHERPA_SAMPLE_RATE_HZ, SessionParams};

/// Endpoint detection rule defaults — match upstream sherpa-onnx examples.
const RULE1_MIN_TRAILING_SILENCE: f32 = 2.4;
const RULE2_MIN_TRAILING_SILENCE: f32 = 1.2;
const RULE3_MIN_UTTERANCE_LENGTH: f32 = 20.0;

/// Initial capacity for the per-session resampled-mono scratch buffer.
const SESSION_MONO_BUFFER_CAPACITY: usize = 16_000;

/// ONNX Runtime threads per recognizer. Sherpa is fast enough on CPU
/// that one thread is sufficient and avoids competing with the audio
/// pipeline.
const SHERPA_NUM_THREADS: i32 = 1;

/// Build the `OnlineRecognizerConfig` for a streaming transducer model.
///
/// Note: `BackendConfig::silence_threshold` is intentionally NOT honored here
/// because sherpa-onnx's `OnlineRecognizer` has native endpoint detection
/// (via `rule1`/`rule2`/`rule3_min_trailing_silence`) that handles silence
/// at the model level. Adding an RMS-based pre-gate would mask short pauses
/// inside utterances and confuse the streaming decoder. The Whisper backend
/// uses `silence_threshold` because Whisper has no built-in VAD.
pub(super) fn build_recognizer_config(model: SherpaModel, provider: &str) -> OnlineRecognizerConfig {
    // Irrefutable today — will become refutable when Moonshine variant lands.
    #[allow(irrefutable_let_patterns)]
    let ModelFilePaths::Transducer { encoder, decoder, joiner, tokens } =
        sherpa_model::model_file_paths(model)
    else {
        unreachable!("StreamingZipformerEn is always a Transducer")
    };

    let mut config = OnlineRecognizerConfig::default();
    config.model_config.transducer.encoder = Some(encoder.to_string_lossy().into_owned());
    config.model_config.transducer.decoder = Some(decoder.to_string_lossy().into_owned());
    config.model_config.transducer.joiner = Some(joiner.to_string_lossy().into_owned());
    config.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
    config.model_config.provider = Some(provider.to_owned());
    config.model_config.num_threads = SHERPA_NUM_THREADS;
    config.enable_endpoint = true;
    config.decoding_method = Some("greedy_search".to_owned());
    config.rule1_min_trailing_silence = RULE1_MIN_TRAILING_SILENCE;
    config.rule2_min_trailing_silence = RULE2_MIN_TRAILING_SILENCE;
    config.rule3_min_utterance_length = RULE3_MIN_UTTERANCE_LENGTH;

    config
}

/// One transcription session. Creates a fresh stream from `recognizer`,
/// runs the feed loop until cancelled or the audio channel disconnects.
pub(super) fn run_session(recognizer: &OnlineRecognizer, params: SessionParams) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
    } = params;

    let stream = recognizer.create_stream();

    if event_tx.send(TranscriptionEvent::Ready).is_err() {
        return;
    }

    let mut mono_buf: Vec<f32> = Vec::with_capacity(SESSION_MONO_BUFFER_CAPACITY);
    let mut last_partial = String::new();

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa session cancelled");
            finalize_session(recognizer, &stream, &last_partial, &event_tx);
            return;
        }

        let interleaved = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(d) => d,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        mono_buf.clear();
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                finalize_session(recognizer, &stream, &last_partial, &event_tx);
                return;
            }
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }

        if mono_buf.is_empty() {
            continue;
        }

        denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);

        stream.accept_waveform(SHERPA_SAMPLE_RATE_HZ, &mono_buf);

        while recognizer.is_ready(&stream) {
            if cancel.load(Ordering::Relaxed) {
                finalize_session(recognizer, &stream, &last_partial, &event_tx);
                return;
            }
            recognizer.decode(&stream);
        }

        let current_text = if let Some(result) = recognizer.get_result(&stream) {
            let trimmed = result.text.trim().to_owned();
            if !trimmed.is_empty() && trimmed != last_partial {
                last_partial.clone_from(&trimmed);
                let _ = event_tx.send(TranscriptionEvent::Partial {
                    text: trimmed.clone(),
                });
            }
            trimmed
        } else {
            String::new()
        };

        if recognizer.is_endpoint(&stream) {
            let committed_text = if current_text.is_empty() {
                last_partial.clone()
            } else {
                current_text
            };
            if !committed_text.is_empty() {
                let timestamp = crate::util::wall_clock_timestamp();
                tracing::debug!(%timestamp, text = %committed_text, "sherpa committed utterance");
                let _ = event_tx.send(TranscriptionEvent::Text {
                    timestamp,
                    text: committed_text,
                });
            }
            recognizer.reset(&stream);
            last_partial.clear();
        }
    }

    finalize_session(recognizer, &stream, &last_partial, &event_tx);
    tracing::info!("sherpa session ended (audio channel disconnected)");
}

/// Commit any in-flight partial hypothesis as a final `Text` event before
/// the session ends. Called from both the cancel and disconnect exit paths.
fn finalize_session(
    recognizer: &OnlineRecognizer,
    stream: &OnlineStream,
    last_partial: &str,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    let final_text = recognizer
        .get_result(stream)
        .map(|r| r.text.trim().to_owned())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| last_partial.to_owned());

    if !final_text.is_empty() {
        let timestamp = crate::util::wall_clock_timestamp();
        tracing::debug!(%timestamp, text = %final_text, "sherpa finalizing on session end");
        let _ = event_tx.send(TranscriptionEvent::Text {
            timestamp,
            text: final_text,
        });
    }
}
```

- [ ] **Step 5: Verify builds + all existing tests + clippy**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo build --workspace 2>&1 | tail -10
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -10
cargo clippy --all-targets --workspace -- -D warnings 2>&1 | tail -10
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu 2>&1 | tail -20
```
Expected: all PASS. The refactor is purely mechanical — no behavior change, all existing tests still pass.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-transcription/src/backends/
git commit -m "refactor(transcription): split backends/sherpa.rs into module

Mechanical split with no behavior changes:
- mod.rs: SherpaBackend facade + TranscriptionBackend impl + tests
- host.rs: SHERPA_HOST, spawn, run_host_loop, shared constants
- streaming.rs: build_recognizer_config, run_session, finalize_session

Sets up the Moonshine integration by giving us a clean place for
offline.rs and silero_vad.rs without pushing sherpa.rs over 1000 lines.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Add Moonshine variants to SherpaModel

**Files:**
- Modify: `crates/sdr-transcription/src/sherpa_model.rs`

- [ ] **Step 1: Add the two new enum variants**

In `crates/sdr-transcription/src/sherpa_model.rs`, find:

```rust
pub enum SherpaModel {
    /// Streaming Zipformer English (k2-fsa, 2023-06-26).
    StreamingZipformerEn,
}
```

Replace with:

```rust
pub enum SherpaModel {
    /// Streaming Zipformer English (k2-fsa, 2023-06-26).
    StreamingZipformerEn,
    /// Moonshine Tiny (UsefulSensors, English, int8). ~27M params,
    /// ~170MB bundle. Fastest Moonshine variant — best for CPU-only
    /// and low-end hardware. Offline (VAD-gated) decode.
    MoonshineTinyEn,
    /// Moonshine Base (UsefulSensors, English, int8). ~61M params,
    /// ~380MB bundle. More accurate than Tiny, higher per-utterance
    /// latency. Offline (VAD-gated) decode.
    MoonshineBaseEn,
}
```

- [ ] **Step 2: Extend the `label`, `dir_name`, and `archive_*` methods**

Find each of these methods in `impl SherpaModel` and add match arms for the new variants.

`label`:
```rust
    pub fn label(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "Streaming Zipformer (English)",
            Self::MoonshineTinyEn => "Moonshine Tiny (English)",
            Self::MoonshineBaseEn => "Moonshine Base (English)",
        }
    }
```

`dir_name`:
```rust
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "streaming-zipformer-en",
            Self::MoonshineTinyEn => "moonshine-tiny-en",
            Self::MoonshineBaseEn => "moonshine-base-en",
        }
    }
```

`archive_filename`:
```rust
    pub fn archive_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2",
            Self::MoonshineTinyEn => "sherpa-onnx-moonshine-tiny-en-int8.tar.bz2",
            Self::MoonshineBaseEn => "sherpa-onnx-moonshine-base-en-int8.tar.bz2",
        }
    }
```

`archive_inner_directory`:
```rust
    pub fn archive_inner_directory(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "sherpa-onnx-streaming-zipformer-en-2023-06-26",
            Self::MoonshineTinyEn => "sherpa-onnx-moonshine-tiny-en-int8",
            Self::MoonshineBaseEn => "sherpa-onnx-moonshine-base-en-int8",
        }
    }
```

- [ ] **Step 3: Extend `kind()` and update `ALL`**

`kind()`:
```rust
    pub fn kind(self) -> ModelKind {
        match self {
            Self::StreamingZipformerEn => ModelKind::OnlineTransducer,
            Self::MoonshineTinyEn | Self::MoonshineBaseEn => ModelKind::OfflineMoonshine,
        }
    }
```

`ALL` constant:
```rust
    pub const ALL: &[Self] = &[
        Self::StreamingZipformerEn,
        Self::MoonshineTinyEn,
        Self::MoonshineBaseEn,
    ];
```

- [ ] **Step 4: Deal with the Moonshine-specific filename methods**

The existing `encoder_filename`, `decoder_filename`, `joiner_filename`, `tokens_filename` methods are Transducer-specific. For Moonshine we introduce two new methods that return the Moonshine-layout filenames. Add these inside `impl SherpaModel` next to the existing filename methods:

```rust
    /// Filename of Moonshine's encoder ONNX file inside the model
    /// directory. Panics if called on a non-Moonshine variant — callers
    /// should match on [`SherpaModel::kind`] first.
    pub fn moonshine_encoder_filename(self) -> &'static str {
        match self {
            Self::MoonshineTinyEn => "encode.int8.onnx",
            Self::MoonshineBaseEn => "encode.int8.onnx",
            Self::StreamingZipformerEn => unreachable!(
                "moonshine_encoder_filename called on non-Moonshine variant"
            ),
        }
    }

    /// Filename of Moonshine v2's merged-decoder ONNX file.
    pub fn moonshine_merged_decoder_filename(self) -> &'static str {
        match self {
            Self::MoonshineTinyEn => "decode.int8.onnx",
            Self::MoonshineBaseEn => "decode.int8.onnx",
            Self::StreamingZipformerEn => unreachable!(
                "moonshine_merged_decoder_filename called on non-Moonshine variant"
            ),
        }
    }

    /// Filename of Moonshine's tokens file.
    pub fn moonshine_tokens_filename(self) -> &'static str {
        match self {
            Self::MoonshineTinyEn | Self::MoonshineBaseEn => "tokens.txt",
            Self::StreamingZipformerEn => unreachable!(
                "moonshine_tokens_filename called on non-Moonshine variant"
            ),
        }
    }
```

Note: the filenames `encode.int8.onnx` and `decode.int8.onnx` match the sherpa-onnx moonshine v2 bundle layout as released on the k2-fsa releases page. If the implementer discovers during testing that the actual extracted filenames differ, they should update these literals and report the adjustment.

- [ ] **Step 5: Wire Moonshine into `model_file_paths`**

Locate the `model_file_paths` function and replace the `unreachable!` branch:

```rust
pub fn model_file_paths(model: SherpaModel) -> ModelFilePaths {
    match model.kind() {
        ModelKind::OnlineTransducer => {
            let dir = model_directory(model);
            ModelFilePaths::Transducer {
                encoder: dir.join(model.encoder_filename()),
                decoder: dir.join(model.decoder_filename()),
                joiner: dir.join(model.joiner_filename()),
                tokens: dir.join(model.tokens_filename()),
            }
        }
        ModelKind::OfflineMoonshine => {
            let dir = model_directory(model);
            ModelFilePaths::Moonshine {
                encoder: dir.join(model.moonshine_encoder_filename()),
                merged_decoder: dir.join(model.moonshine_merged_decoder_filename()),
                tokens: dir.join(model.moonshine_tokens_filename()),
            }
        }
    }
}
```

- [ ] **Step 6: Add the `Moonshine` variant to `ModelFilePaths`**

Locate the `ModelFilePaths` enum and add the `Moonshine` variant:

```rust
pub enum ModelFilePaths {
    Transducer {
        encoder: PathBuf,
        decoder: PathBuf,
        joiner: PathBuf,
        tokens: PathBuf,
    },
    Moonshine {
        encoder: PathBuf,
        merged_decoder: PathBuf,
        tokens: PathBuf,
    },
}
```

- [ ] **Step 7: Update `model_exists` to handle the `Moonshine` variant**

```rust
pub fn model_exists(model: SherpaModel) -> bool {
    match model_file_paths(model) {
        ModelFilePaths::Transducer { encoder, decoder, joiner, tokens } => {
            encoder.is_file() && decoder.is_file() && joiner.is_file() && tokens.is_file()
        }
        ModelFilePaths::Moonshine { encoder, merged_decoder, tokens } => {
            encoder.is_file() && merged_decoder.is_file() && tokens.is_file()
        }
    }
}
```

- [ ] **Step 8: Remove the `#[allow(irrefutable_let_patterns)]` attribute in `streaming.rs`**

In `crates/sdr-transcription/src/backends/sherpa/streaming.rs`, find:

```rust
    // Irrefutable today — will become refutable when Moonshine variant lands.
    #[allow(irrefutable_let_patterns)]
    let ModelFilePaths::Transducer { encoder, decoder, joiner, tokens } =
        sherpa_model::model_file_paths(model)
    else {
        unreachable!("StreamingZipformerEn is always a Transducer")
    };
```

Replace with:

```rust
    let ModelFilePaths::Transducer { encoder, decoder, joiner, tokens } =
        sherpa_model::model_file_paths(model)
    else {
        unreachable!("streaming::build_recognizer_config called with non-Transducer model")
    };
```

The pattern is now refutable (because `ModelFilePaths::Moonshine` exists) so the allow attribute is unnecessary.

- [ ] **Step 9: Add unit tests for Moonshine variants**

In the `#[cfg(test)] mod tests` block at the bottom of `sherpa_model.rs`, add:

```rust
    #[test]
    fn moonshine_variants_are_offline_moonshine_kind() {
        assert_eq!(SherpaModel::MoonshineTinyEn.kind(), ModelKind::OfflineMoonshine);
        assert_eq!(SherpaModel::MoonshineBaseEn.kind(), ModelKind::OfflineMoonshine);
    }

    #[test]
    fn moonshine_variants_do_not_support_partials() {
        assert!(!SherpaModel::MoonshineTinyEn.supports_partials());
        assert!(!SherpaModel::MoonshineBaseEn.supports_partials());
    }

    #[test]
    fn moonshine_tiny_has_three_file_layout() {
        let paths = model_file_paths(SherpaModel::MoonshineTinyEn);
        let ModelFilePaths::Moonshine { encoder, merged_decoder, tokens } = paths else {
            panic!("MoonshineTinyEn should be a Moonshine layout");
        };
        assert!(encoder.ends_with("encode.int8.onnx"));
        assert!(merged_decoder.ends_with("decode.int8.onnx"));
        assert!(tokens.ends_with("tokens.txt"));
        assert_ne!(encoder, merged_decoder);
    }

    #[test]
    fn moonshine_archive_urls_are_well_formed() {
        for model in [SherpaModel::MoonshineTinyEn, SherpaModel::MoonshineBaseEn] {
            let url = model.archive_url();
            assert!(url.starts_with("https://github.com/k2-fsa/sherpa-onnx/"));
            assert!(url.ends_with(".tar.bz2"));
            assert!(url.contains("moonshine"));
        }
    }

    #[test]
    fn all_contains_three_variants() {
        assert_eq!(SherpaModel::ALL.len(), 3);
    }
```

- [ ] **Step 10: Verify builds + tests**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu sherpa_model 2>&1 | tail -20
cargo build --workspace 2>&1 | tail -10
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -10
```
Expected: all PASS, new tests PASS.

Note: the host init flow in `host.rs` still hardcodes `OnlineRecognizer::create`. If a user selects `MoonshineTinyEn` or `MoonshineBaseEn` in the UI at this point, the backend will fail to initialize because the Moonshine model files won't exist after download (the download code still works — the directory + archive download is model-agnostic — but `OnlineRecognizer` doesn't understand Moonshine bundles). Task 10 adds the host branching to fix this. For Task 6's verification, we only compile and test, not run.

- [ ] **Step 11: Commit**

```bash
git add crates/sdr-transcription/src/sherpa_model.rs crates/sdr-transcription/src/backends/sherpa/streaming.rs
git commit -m "feat(transcription): add Moonshine variants to SherpaModel

Adds MoonshineTinyEn and MoonshineBaseEn variants with full metadata
(label, dir_name, archive_filename, archive_inner_directory, kind).
Both return ModelKind::OfflineMoonshine and supports_partials=false.
ModelFilePaths gains a Moonshine variant with the v2 (encoder +
merged_decoder + tokens) three-file layout. model_file_paths and
model_exists branch on kind. Host init loop still hardcodes
OnlineRecognizer::create — will branch in a later task.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Silero VAD download helpers

**Files:**
- Modify: `crates/sdr-transcription/src/sherpa_model.rs`

- [ ] **Step 1: Add the silero VAD constants and helper functions**

In `crates/sdr-transcription/src/sherpa_model.rs`, immediately after the existing `sherpa_models_dir()` function (around line 148), add:

```rust
/// Filename of the Silero VAD ONNX model when stored locally.
const SILERO_VAD_FILENAME: &str = "silero_vad.onnx";

/// Directory under `sherpa_models_dir` where the Silero VAD model lives.
const SILERO_VAD_DIR_NAME: &str = "silero-vad";

/// Full HTTPS URL to the Silero VAD ONNX file on the k2-fsa GitHub
/// releases page. Single-file artifact — no tarball, no extraction.
const SILERO_VAD_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";

/// Full path to the Silero VAD ONNX file on disk.
pub fn silero_vad_path() -> PathBuf {
    sherpa_models_dir()
        .join(SILERO_VAD_DIR_NAME)
        .join(SILERO_VAD_FILENAME)
}

/// True if the Silero VAD model exists on disk.
pub fn silero_vad_exists() -> bool {
    silero_vad_path().is_file()
}

/// Download the Silero VAD ONNX model from the k2-fsa releases page.
///
/// # Arguments
///
/// * `progress_tx` — receives integer percent values (0..=100) as the
///   download streams. The file is ~2 MB so this usually only fires
///   a handful of times.
///
/// # Returns
///
/// On success, the absolute path to the downloaded `silero_vad.onnx`.
///
/// # Behavior
///
/// 1. Creates the parent directory if needed.
/// 2. Downloads to `silero_vad.onnx.part` in the same directory.
/// 3. Renames `.part` → final path on successful completion.
///
/// Unlike model bundles, the VAD is a single `.onnx` file — no
/// extraction step. The atomic rename is sufficient to avoid leaving
/// a partially-written model in place if the process dies mid-download.
#[allow(clippy::cast_possible_truncation)]
pub fn download_silero_vad(
    progress_tx: &std::sync::mpsc::Sender<u8>,
) -> Result<PathBuf, SherpaModelError> {
    let final_path = silero_vad_path();
    let dir = final_path
        .parent()
        .expect("silero_vad_path always has a parent")
        .to_path_buf();
    std::fs::create_dir_all(&dir)?;

    let part_path = dir.join(format!("{SILERO_VAD_FILENAME}.part"));

    // Clean up leftover scratch from a previous failed attempt.
    if part_path.exists() {
        std::fs::remove_file(&part_path)?;
    }

    tracing::info!(url = %SILERO_VAD_URL, ?part_path, "downloading silero VAD");

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_mins(5))
        .build()?;

    let response = client.get(SILERO_VAD_URL).send()?.error_for_status()?;
    let total_size = response.content_length().unwrap_or(0);

    if total_size == 0 {
        let _ = progress_tx.send(0);
    }

    let mut file = std::fs::File::create(&part_path)?;
    let mut downloaded: u64 = 0;
    let mut last_pct: u8 = 0;
    let mut reader = response;
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let bytes_read = std::io::Read::read(&mut reader, &mut buf)?;
        if bytes_read == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..bytes_read])?;
        downloaded += bytes_read as u64;

        if let Some(pct) = (downloaded * 100).checked_div(total_size) {
            let pct = pct.min(100) as u8;
            if pct != last_pct {
                last_pct = pct;
                let _ = progress_tx.send(pct);
            }
        }
    }

    std::io::Write::flush(&mut file)?;
    drop(file);

    // Atomic rename into place. Any existing final file (rare — it
    // would only happen if silero_vad_exists() returned false but the
    // file then appeared, i.e. a concurrent instance) is replaced.
    std::fs::rename(&part_path, &final_path)?;

    tracing::info!(bytes = downloaded, ?final_path, "silero VAD download complete");
    Ok(final_path)
}
```

- [ ] **Step 2: Add unit tests for the path helpers**

In the `#[cfg(test)] mod tests` block, add:

```rust
    #[test]
    fn silero_vad_path_is_under_sherpa_models_dir() {
        let path = silero_vad_path();
        assert!(path.ends_with("silero-vad/silero_vad.onnx"));
    }
```

Note: we deliberately do NOT test `silero_vad_exists` or `download_silero_vad` — both touch the real user filesystem via `dirs_next::data_dir()` and match the hermetic-testing caveat already documented on issue #255.

- [ ] **Step 3: Verify builds + tests**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu sherpa_model::tests::silero 2>&1 | tail -10
cargo build --workspace 2>&1 | tail -10
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -10
```
Expected: all PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-transcription/src/sherpa_model.rs
git commit -m "feat(transcription): add Silero VAD download helpers

Adds silero_vad_path, silero_vad_exists, and download_silero_vad
to sherpa_model. Single-file artifact (~2MB) hosted on the same
k2-fsa releases page as model bundles. No extraction needed — just
download to .part then atomic rename.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: SherpaSileroVad impl

**Files:**
- Create: `crates/sdr-transcription/src/backends/sherpa/silero_vad.rs`

- [ ] **Step 1: Create the SherpaSileroVad adapter**

Write this exact content to `crates/sdr-transcription/src/backends/sherpa/silero_vad.rs`:

```rust
//! Sherpa-onnx-backed Silero VAD wrapper implementing the
//! feature-agnostic [`VoiceActivityDetector`] trait.
//!
//! The underlying `sherpa_onnx::VoiceActivityDetector` is a queue-based
//! detector: you feed audio via `accept_waveform`, it buffers internally
//! and queues completed speech segments, and you pull them via
//! `front`/`pop` until `is_empty` returns true.
//!
//! This adapter flattens that queue-based API into the trait's
//! `accept` + `pop_segment` pattern so callers can write a simple
//! `while let Some(segment) = vad.pop_segment() { decode(segment) }`
//! loop.

use std::path::Path;

use sherpa_onnx::{
    SileroVadModelConfig, VadModelConfig, VoiceActivityDetector as SherpaVad,
};

use crate::backend::BackendError;
use crate::vad::VoiceActivityDetector;

use super::host::SHERPA_SAMPLE_RATE_HZ;

/// Silero VAD default hyperparameters. These match the sherpa-onnx
/// upstream `moonshine_v2.rs` example and are appropriate for radio
/// audio (short bursts, occasional long silences).
const SILERO_THRESHOLD: f32 = 0.5;
const SILERO_MIN_SILENCE_DURATION: f32 = 0.25;
const SILERO_MIN_SPEECH_DURATION: f32 = 0.25;
const SILERO_MAX_SPEECH_DURATION: f32 = 20.0;
const SILERO_WINDOW_SIZE: i32 = 512;

/// Internal buffer size for the detector, in seconds of audio.
/// 30 seconds is well above `SILERO_MAX_SPEECH_DURATION`, giving
/// the detector plenty of headroom even on the longest permitted
/// utterance.
const VAD_BUFFER_SIZE_SECONDS: f32 = 30.0;

/// Sherpa-onnx-backed Silero VAD.
pub struct SherpaSileroVad {
    inner: SherpaVad,
}

impl SherpaSileroVad {
    /// Create a new Silero VAD using the ONNX file at `model_path`.
    /// The file is typically installed by [`crate::sherpa_model::download_silero_vad`].
    pub fn new(model_path: &Path) -> Result<Self, BackendError> {
        let silero_config = SileroVadModelConfig {
            model: model_path.to_string_lossy().into_owned(),
            threshold: SILERO_THRESHOLD,
            min_silence_duration: SILERO_MIN_SILENCE_DURATION,
            min_speech_duration: SILERO_MIN_SPEECH_DURATION,
            max_speech_duration: SILERO_MAX_SPEECH_DURATION,
            window_size: SILERO_WINDOW_SIZE,
        };

        let vad_config = VadModelConfig {
            silero_vad: silero_config,
            sample_rate: SHERPA_SAMPLE_RATE_HZ,
            num_threads: 1,
            provider: "cpu".to_owned(),
            debug: false,
        };

        let inner = SherpaVad::new(&vad_config, VAD_BUFFER_SIZE_SECONDS).ok_or_else(|| {
            BackendError::Init(format!(
                "Silero VAD creation failed — check model at {}",
                model_path.display()
            ))
        })?;

        Ok(Self { inner })
    }
}

impl VoiceActivityDetector for SherpaSileroVad {
    fn accept(&mut self, samples: &[f32]) {
        self.inner.accept_waveform(samples);
    }

    fn pop_segment(&mut self) -> Option<Vec<f32>> {
        if self.inner.is_empty() {
            return None;
        }
        // front() borrows from the detector's internal queue; clone
        // the samples so the returned segment is owned. Then pop to
        // advance the queue.
        let front = self.inner.front();
        let samples = front.samples.to_vec();
        self.inner.pop();
        Some(samples)
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}
```

- [ ] **Step 2: Register the submodule in `backends/sherpa/mod.rs`**

In `crates/sdr-transcription/src/backends/sherpa/mod.rs`, find:

```rust
mod host;
mod streaming;
```

Replace with:

```rust
mod host;
mod silero_vad;
mod streaming;
```

- [ ] **Step 3: Verify builds**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -20
```

Expected: PASS. If the sherpa-onnx API surface for `SileroVadModelConfig`, `VadModelConfig`, or `VoiceActivityDetector::new/front/pop/is_empty/reset/accept_waveform` differs from what's written above, report BLOCKED with the exact compile error and the actual method signatures from the compiled rustdoc. The plan assumes sherpa-onnx 1.12.36 signatures as read from `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/sherpa-onnx-1.12.36/src/vad.rs`.

Run also:
```bash
cargo build --workspace 2>&1 | tail -10
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -10
```
Expected: both PASS. The new file is entirely inside the sherpa module, so whisper builds are unaffected.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-transcription/src/backends/sherpa/silero_vad.rs crates/sdr-transcription/src/backends/sherpa/mod.rs
git commit -m "feat(transcription): add SherpaSileroVad adapter

Implements VoiceActivityDetector trait on top of sherpa-onnx's queue-based
SileroVad. Flattens the is_empty/front/pop pattern into accept + pop_segment
so callers can write a simple while-let loop. Hyperparameters match the
upstream moonshine_v2 example: 0.5 threshold, 0.25s min silence/speech,
20s max utterance.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Host init branching on model kind

> **Important:** Tasks 9 and 10 MUST be executed as a pair without an interruption. Task 9 leaves the build intentionally broken (references `super::offline::run_session` which doesn't exist until Task 10). Do NOT run spec or code-quality review between them — run both tasks, verify the combined build compiles, then do a single review pass covering both commits. The controller should batch them.

**Files:**
- Modify: `crates/sdr-transcription/src/backends/sherpa/host.rs`

- [ ] **Step 1: Update imports at the top of `host.rs`**

In `crates/sdr-transcription/src/backends/sherpa/host.rs`, find the existing import:

```rust
use sherpa_onnx::OnlineRecognizer;
```

Replace with:

```rust
use sherpa_onnx::{OfflineRecognizer, OnlineRecognizer};
```

Then find the existing `use super::streaming;` line and add a companion import for silero_vad directly after it:

```rust
use super::streaming;
use super::silero_vad::SherpaSileroVad;
```

- [ ] **Step 2: Add the `RecognizerState` enum**

Still in `host.rs`, locate the existing struct `SherpaHost` (near the middle of the file). Immediately BEFORE the `struct SherpaHostState` declaration, insert:

```rust
/// Which recognizer (and optional VAD) the host worker owns.
///
/// Set once in `run_host_loop` based on `SherpaModel::kind()`. The
/// command loop pattern-matches on this to dispatch to the right
/// session runner.
pub(super) enum RecognizerState {
    Online(OnlineRecognizer),
    Offline {
        recognizer: OfflineRecognizer,
        vad: SherpaSileroVad,
    },
}
```

- [ ] **Step 3: Update `run_host_loop` to branch on `model.kind()`**

Replace the entire body of `run_host_loop` with the following, which handles both the existing Zipformer path and the new Moonshine path:

```rust
fn run_host_loop(
    model: SherpaModel,
    cmd_rx: &mpsc::Receiver<HostCommand>,
    cmd_tx: mpsc::Sender<HostCommand>,
    event_tx: mpsc::Sender<InitEvent>,
) {
    use crate::sherpa_model::ModelKind;

    let recognizer_state = match model.kind() {
        ModelKind::OnlineTransducer => {
            match init_online(model, &event_tx) {
                Ok(state) => state,
                Err(()) => return, // init_online already published Failed and stored the error
            }
        }
        ModelKind::OfflineMoonshine => {
            match init_offline(model, &event_tx) {
                Ok(state) => state,
                Err(()) => return,
            }
        }
    };

    // --- Phase 3: build SherpaHost and store in SHERPA_HOST ---
    let host = SherpaHost {
        state: Mutex::new(SherpaHostState { cmd_tx }),
    };
    if SHERPA_HOST.set(Ok(host)).is_err() {
        let msg = "sherpa host OnceLock was already set; this worker is unreachable".to_owned();
        tracing::error!(%msg);
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return;
    }
    tracing::info!("sherpa-host ready, signaling Ready event");
    let _ = event_tx.send(InitEvent::Ready);
    drop(event_tx);

    // --- Phase 4: command loop ---
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            HostCommand::StartSession(params) => {
                tracing::info!("sherpa-host: starting session");
                match &recognizer_state {
                    RecognizerState::Online(recognizer) => {
                        super::streaming::run_session(recognizer, params);
                    }
                    RecognizerState::Offline { recognizer, vad } => {
                        // Clone the VAD state handle into a mutable borrow
                        // so the offline loop can reset/accept/pop on it.
                        // We can't actually clone the inner sherpa VAD, so
                        // the offline loop takes &mut through a helper
                        // that holds RefCell or similar — see offline.rs.
                        //
                        // The simplest approach: move the session-local
                        // mutability down into run_session_offline by
                        // passing the VAD by &mut through a local unsafe
                        // re-borrow. We use a local RefCell instead to keep
                        // the code safe — see offline::run_session.
                        super::offline::run_session(recognizer, vad, params);
                    }
                }
                tracing::info!("sherpa-host: session ended");
            }
        }
    }
    tracing::info!("sherpa-host worker exiting");
}
```

Note: the comment about "can't actually clone the inner sherpa VAD" is a placeholder — the real answer is that `SherpaSileroVad` holds `&mut self` methods, so the session runner takes `&mut SherpaSileroVad`. The match arm borrows the VAD mutably from the enum. Rust allows that because the match on `&recognizer_state` gives back `&RecognizerState::Offline { recognizer, vad }` — but `vad` there is `&SherpaSileroVad`, not `&mut`. To get `&mut`, we need to either:

1. Hold the VAD inside a `RefCell<SherpaSileroVad>` inside `RecognizerState::Offline` so the session runner can `.borrow_mut()`, or
2. Match on `&mut recognizer_state` and bind `vad` as `&mut SherpaSileroVad`.

Option 2 is simpler — the command loop owns `recognizer_state` and takes `&mut` on match. Replace the command loop block above with this version that uses `&mut`:

```rust
    // --- Phase 4: command loop ---
    let mut recognizer_state = recognizer_state;
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            HostCommand::StartSession(params) => {
                tracing::info!("sherpa-host: starting session");
                match &mut recognizer_state {
                    RecognizerState::Online(recognizer) => {
                        super::streaming::run_session(recognizer, params);
                    }
                    RecognizerState::Offline { recognizer, vad } => {
                        super::offline::run_session(recognizer, vad, params);
                    }
                }
                tracing::info!("sherpa-host: session ended");
            }
        }
    }
```

And remove the earlier code-block comment about RefCell — it's not needed.

- [ ] **Step 4: Add the `init_online` helper**

Add this function to `host.rs` after the existing `store_init_failure` helper:

```rust
/// Phase 1-2 for the OnlineTransducer path: download the bundle if
/// needed, then create the `OnlineRecognizer`. Returns `Err(())` on
/// any failure — the error has already been stored in SHERPA_HOST and
/// emitted as `InitEvent::Failed`.
fn init_online(
    model: SherpaModel,
    event_tx: &mpsc::Sender<InitEvent>,
) -> Result<RecognizerState, ()> {
    if !sherpa_model::model_exists(model) {
        if !download_and_extract_bundle(model, event_tx, model.label()) {
            return Err(());
        }
    }

    let _ = event_tx.send(InitEvent::CreatingRecognizer);
    let recognizer_config = super::streaming::build_recognizer_config(model, "cpu");
    tracing::info!(?model, "creating sherpa-onnx OnlineRecognizer");

    let Some(recognizer) = OnlineRecognizer::create(&recognizer_config) else {
        let msg = "OnlineRecognizer::create returned None — check model file paths".to_owned();
        tracing::error!(%msg);
        store_init_failure(BackendError::Init(msg.clone()));
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return Err(());
    };
    tracing::info!("OnlineRecognizer created successfully");
    Ok(RecognizerState::Online(recognizer))
}
```

- [ ] **Step 5: Add the `init_offline` helper**

```rust
/// Phase 1-2 for the OfflineMoonshine path: download the Silero VAD
/// model if missing, download the Moonshine bundle if missing, then
/// create the `OfflineRecognizer` + `SherpaSileroVad`. Returns `Err(())`
/// on any failure — the error has already been stored in SHERPA_HOST
/// and emitted as `InitEvent::Failed`.
fn init_offline(
    model: SherpaModel,
    event_tx: &mpsc::Sender<InitEvent>,
) -> Result<RecognizerState, ()> {
    // --- Silero VAD ---
    if !sherpa_model::silero_vad_exists() {
        tracing::info!("silero VAD not found locally, downloading");
        if !download_silero_vad_with_progress(event_tx) {
            return Err(());
        }
    }

    // --- Moonshine model bundle ---
    if !sherpa_model::model_exists(model) {
        if !download_and_extract_bundle(model, event_tx, model.label()) {
            return Err(());
        }
    }

    // --- Build OfflineRecognizer ---
    let _ = event_tx.send(InitEvent::CreatingRecognizer);
    let recognizer_config = super::offline::build_moonshine_recognizer_config(model, "cpu");
    tracing::info!(?model, "creating sherpa-onnx OfflineRecognizer (Moonshine)");

    let Some(recognizer) = OfflineRecognizer::create(&recognizer_config) else {
        let msg = "OfflineRecognizer::create returned None — check Moonshine model files".to_owned();
        tracing::error!(%msg);
        store_init_failure(BackendError::Init(msg.clone()));
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return Err(());
    };
    tracing::info!("OfflineRecognizer created successfully");

    // --- Build SherpaSileroVad ---
    let vad_path = sherpa_model::silero_vad_path();
    let vad = match SherpaSileroVad::new(&vad_path) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("Silero VAD creation failed: {e}");
            tracing::error!(%msg);
            store_init_failure(BackendError::Init(msg.clone()));
            let _ = event_tx.send(InitEvent::Failed { message: msg });
            return Err(());
        }
    };
    tracing::info!("SherpaSileroVad created successfully");

    Ok(RecognizerState::Offline { recognizer, vad })
}
```

- [ ] **Step 6: Add the two shared download helpers**

Both init paths share archive download + extract and VAD download. Extract them as helpers at the bottom of `host.rs`:

```rust
/// Download + extract a sherpa model bundle. Returns `false` on any
/// failure (error already stored + InitEvent::Failed emitted).
fn download_and_extract_bundle(
    model: SherpaModel,
    event_tx: &mpsc::Sender<InitEvent>,
    component: &'static str,
) -> bool {
    tracing::info!(?model, "sherpa model bundle not found locally, downloading");
    let _ = event_tx.send(InitEvent::DownloadStart { component });

    let (dl_tx, dl_rx) = mpsc::channel::<u8>();
    let event_tx_dl = event_tx.clone();

    let dl_forwarder = match std::thread::Builder::new()
        .name("sherpa-dl-progress".into())
        .spawn(move || {
            while let Ok(pct) = dl_rx.recv() {
                let _ = event_tx_dl.send(InitEvent::DownloadProgress { pct });
            }
        }) {
        Ok(handle) => handle,
        Err(e) => {
            let msg = format!("failed to spawn sherpa-dl-progress thread: {e}");
            tracing::error!(%msg);
            store_init_failure(BackendError::Init(msg.clone()));
            let _ = event_tx.send(InitEvent::Failed { message: msg });
            return false;
        }
    };

    let archive_result = sherpa_model::download_sherpa_archive(model, &dl_tx);
    drop(dl_tx);
    let _ = dl_forwarder.join();

    let archive_path = match archive_result {
        Ok(path) => path,
        Err(e) => {
            let msg = format!("sherpa model download failed: {e}");
            tracing::error!(%msg);
            store_init_failure(BackendError::Init(msg.clone()));
            let _ = event_tx.send(InitEvent::Failed { message: msg });
            return false;
        }
    };

    tracing::info!("sherpa archive download complete, extracting");
    let _ = event_tx.send(InitEvent::Extracting { component });

    if let Err(e) = sherpa_model::extract_sherpa_archive(model, &archive_path) {
        let msg = format!("sherpa model extraction failed: {e}");
        tracing::error!(%msg);
        store_init_failure(BackendError::Init(msg.clone()));
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return false;
    }

    tracing::info!("sherpa model bundle installed");
    true
}

/// Download the Silero VAD ONNX file. Returns `false` on any failure
/// (error already stored + InitEvent::Failed emitted).
fn download_silero_vad_with_progress(event_tx: &mpsc::Sender<InitEvent>) -> bool {
    const VAD_COMPONENT: &str = "Silero VAD";
    let _ = event_tx.send(InitEvent::DownloadStart {
        component: VAD_COMPONENT,
    });

    let (dl_tx, dl_rx) = mpsc::channel::<u8>();
    let event_tx_dl = event_tx.clone();

    let dl_forwarder = match std::thread::Builder::new()
        .name("sherpa-vad-dl-progress".into())
        .spawn(move || {
            while let Ok(pct) = dl_rx.recv() {
                let _ = event_tx_dl.send(InitEvent::DownloadProgress { pct });
            }
        }) {
        Ok(handle) => handle,
        Err(e) => {
            let msg = format!("failed to spawn sherpa-vad-dl-progress thread: {e}");
            tracing::error!(%msg);
            store_init_failure(BackendError::Init(msg.clone()));
            let _ = event_tx.send(InitEvent::Failed { message: msg });
            return false;
        }
    };

    let result = sherpa_model::download_silero_vad(&dl_tx);
    drop(dl_tx);
    let _ = dl_forwarder.join();

    if let Err(e) = result {
        let msg = format!("silero VAD download failed: {e}");
        tracing::error!(%msg);
        store_init_failure(BackendError::Init(msg.clone()));
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return false;
    }

    tracing::info!("silero VAD download complete");
    true
}
```

- [ ] **Step 7: Remove the old inline download/extract code**

The old `run_host_loop` body (Phase 1 download code) is replaced by the call to `download_and_extract_bundle` via `init_online`. After Step 2's replacement of `run_host_loop`, make sure the old inline Phase 1 code is gone — it should be, because Step 2 replaced the entire function body.

- [ ] **Step 8: Make `SHERPA_SAMPLE_RATE_HZ` accessible to `silero_vad.rs`**

`silero_vad.rs` (from Task 8) imports `SHERPA_SAMPLE_RATE_HZ` via `use super::host::SHERPA_SAMPLE_RATE_HZ;`. That import already works because `SHERPA_SAMPLE_RATE_HZ` is declared `pub(super)` in host.rs. No action needed; just verify when building.

- [ ] **Step 9: Verify sherpa build compiles**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -30
```

Expected: this will fail with one specific error — `super::offline::run_session` and `super::offline::build_moonshine_recognizer_config` don't exist yet (Task 10 adds them). The error message should reference only `offline::run_session` and `offline::build_moonshine_recognizer_config`. If there are any OTHER errors (type mismatches, missing fields, wrong import paths, etc.), STOP and report BLOCKED.

- [ ] **Step 10: Commit**

Because the build is intentionally broken (missing `offline.rs`), Task 9 ends with a commit that Task 10 will complete. This is acceptable because the broken state is localized to a missing module — the rest of the code compiles.

```bash
git add crates/sdr-transcription/src/backends/sherpa/host.rs
git commit -m "feat(transcription): branch host init on model kind

run_host_loop now splits into init_online (current Zipformer path)
and init_offline (new Moonshine path with VAD download + OfflineRecognizer
creation). Adds RecognizerState enum to hold either the online or
offline recognizer plus VAD. Command loop dispatches to streaming or
offline session runner based on RecognizerState.

Build is intentionally broken at this commit: references
super::offline::run_session which lands in the next task.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Offline session loop

**Files:**
- Create: `crates/sdr-transcription/src/backends/sherpa/offline.rs`
- Modify: `crates/sdr-transcription/src/backends/sherpa/mod.rs`

- [ ] **Step 1: Register the `offline` submodule**

In `crates/sdr-transcription/src/backends/sherpa/mod.rs`, find:

```rust
mod host;
mod silero_vad;
mod streaming;
```

Replace with:

```rust
mod host;
mod offline;
mod silero_vad;
mod streaming;
```

- [ ] **Step 2: Write `crates/sdr-transcription/src/backends/sherpa/offline.rs`**

```rust
//! Offline session loop for Moonshine (and future offline recognizers).
//!
//! Runs on the sherpa-host worker thread. Uses Silero VAD to detect
//! utterance boundaries in the incoming audio stream, then batch-decodes
//! each completed segment through the `OfflineRecognizer`.
//!
//! Unlike the streaming loop, this path emits NO `TranscriptionEvent::Partial`
//! events. Moonshine is offline — partials aren't meaningful. The UI hides
//! the Live/Final display-mode toggle when a Moonshine model is selected
//! (see `SherpaModel::supports_partials`).

use std::sync::atomic::Ordering;
use std::sync::mpsc;

use sherpa_onnx::{
    OfflineMoonshineModelConfig, OfflineModelConfig, OfflineRecognizer,
    OfflineRecognizerConfig,
};

use crate::backend::TranscriptionEvent;
use crate::sherpa_model::{self, ModelFilePaths, SherpaModel};
use crate::vad::VoiceActivityDetector;
use crate::{denoise, resampler};

use super::host::{AUDIO_RECV_TIMEOUT, SHERPA_SAMPLE_RATE_HZ, SessionParams};
use super::silero_vad::SherpaSileroVad;

/// Initial capacity for the per-session resampled-mono scratch buffer.
const SESSION_MONO_BUFFER_CAPACITY: usize = 16_000;

/// ONNX Runtime threads per recognizer. Sherpa is fast enough on CPU
/// that one thread is sufficient and avoids competing with the audio
/// pipeline.
const SHERPA_NUM_THREADS: i32 = 1;

/// Build the `OfflineRecognizerConfig` for a Moonshine model.
pub(super) fn build_moonshine_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> OfflineRecognizerConfig {
    let ModelFilePaths::Moonshine { encoder, merged_decoder, tokens } =
        sherpa_model::model_file_paths(model)
    else {
        unreachable!("offline::build_moonshine_recognizer_config called with non-Moonshine model")
    };

    let moonshine = OfflineMoonshineModelConfig {
        encoder: Some(encoder.to_string_lossy().into_owned()),
        merged_decoder: Some(merged_decoder.to_string_lossy().into_owned()),
        ..OfflineMoonshineModelConfig::default()
    };

    let model_config = OfflineModelConfig {
        moonshine,
        tokens: Some(tokens.to_string_lossy().into_owned()),
        provider: Some(provider.to_owned()),
        num_threads: SHERPA_NUM_THREADS,
        ..OfflineModelConfig::default()
    };

    OfflineRecognizerConfig {
        model_config,
        ..OfflineRecognizerConfig::default()
    }
}

/// One offline transcription session. Feeds audio through the VAD,
/// batch-decodes each detected speech segment, and emits `Text` events.
/// Never emits `Partial`.
pub(super) fn run_session(
    recognizer: &OfflineRecognizer,
    vad: &mut SherpaSileroVad,
    params: SessionParams,
) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
    } = params;

    // Clear any residual state from a previous session.
    vad.reset();

    if event_tx.send(TranscriptionEvent::Ready).is_err() {
        return;
    }

    let mut mono_buf: Vec<f32> = Vec::with_capacity(SESSION_MONO_BUFFER_CAPACITY);

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa offline session cancelled");
            drain_vad_on_exit(recognizer, vad, &event_tx);
            return;
        }

        let interleaved = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(d) => d,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Resample 48 kHz stereo → 16 kHz mono.
        mono_buf.clear();
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                drain_vad_on_exit(recognizer, vad, &event_tx);
                return;
            }
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }

        if mono_buf.is_empty() {
            continue;
        }

        // Spectral denoise BEFORE VAD — RTL-SDR squelch tails confuse
        // Silero just as much as they confuse decoders.
        denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);

        vad.accept(&mono_buf);

        while let Some(segment) = vad.pop_segment() {
            if cancel.load(Ordering::Relaxed) {
                drain_vad_on_exit(recognizer, vad, &event_tx);
                return;
            }
            decode_segment(recognizer, &segment, &event_tx);
        }
    }

    // Audio channel disconnected — flush any in-flight segment.
    drain_vad_on_exit(recognizer, vad, &event_tx);
    tracing::info!("sherpa offline session ended (audio channel disconnected)");
}

/// Batch-decode a single speech segment and emit a `Text` event if
/// the recognizer produced any text.
fn decode_segment(
    recognizer: &OfflineRecognizer,
    segment: &[f32],
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    let stream = recognizer.create_stream();
    stream.accept_waveform(SHERPA_SAMPLE_RATE_HZ, segment);
    recognizer.decode(&stream);
    let result = stream.get_result();
    let text = result.text.trim().to_owned();
    if !text.is_empty() {
        let timestamp = crate::util::wall_clock_timestamp();
        tracing::debug!(%timestamp, %text, "moonshine committed utterance");
        let _ = event_tx.send(TranscriptionEvent::Text { timestamp, text });
    }
}

/// Flush the VAD on session exit — any completed segment in the queue
/// gets decoded and emitted as a final `Text`. Reset the VAD afterward
/// so the next session starts clean.
fn drain_vad_on_exit(
    recognizer: &OfflineRecognizer,
    vad: &mut SherpaSileroVad,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    while let Some(segment) = vad.pop_segment() {
        decode_segment(recognizer, &segment, event_tx);
    }
    vad.reset();
}
```

- [ ] **Step 3: Verify both builds compile**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -20
cargo build --workspace 2>&1 | tail -10
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -15
cargo clippy --all-targets --workspace -- -D warnings 2>&1 | tail -10
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu 2>&1 | tail -20
```

Expected: all PASS. If the sherpa-onnx `OfflineMoonshineModelConfig`, `OfflineModelConfig`, `OfflineRecognizerConfig`, or `OfflineStream` APIs differ from what's written above (struct field names, method signatures, default implementations), report BLOCKED with the exact compile error and the actual signature from the crate source. The plan assumes sherpa-onnx 1.12.36 as read from `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/sherpa-onnx-1.12.36/src/offline_asr.rs`.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-transcription/src/backends/sherpa/offline.rs crates/sdr-transcription/src/backends/sherpa/mod.rs
git commit -m "feat(transcription): offline Moonshine session loop

VAD-driven batch decode path for Moonshine. Feeds audio through
Silero VAD, pops completed speech segments, batch-decodes each via
OfflineRecognizer, emits TranscriptionEvent::Text. Never emits
Partial. Drains VAD on session exit so the last utterance isn't
dropped.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: UI — contextual display_mode_row visibility

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/transcript_panel.rs`

- [ ] **Step 1: Add a second handler on `model_row.connect_selected_notify` that toggles display_mode_row visibility**

In `crates/sdr-ui/src/sidebar/transcript_panel.rs`, locate the existing block:

```rust
    // Persist model selection on change.
    let config_model = Arc::clone(config);
    model_row.connect_selected_notify(move |row| {
        let idx = row.selected();
        if idx < max_model_idx {
            config_model.write(|v| {
                v[key_for_persistence] = serde_json::json!(idx);
            });
        }
    });
```

Immediately after this block (so it runs AFTER display_mode_row is built later in the function — wait, it runs BEFORE. We need to add the new handler later in the function, after display_mode_row is constructed).

Actually locate the block at the end of the display-mode-row setup (the block added in PR 4) which ends with:

```rust
        row
    };
```

(This is the `let display_mode_row = { ... };` block that returns `row`.)

Immediately after that `};`, add:

```rust
    // Toggle display_mode_row visibility based on whether the selected
    // model emits partial hypotheses. Models like Moonshine are offline
    // and decode once per utterance — the Live/Final distinction is
    // meaningless and the row is hidden entirely. Initial visibility
    // is set here based on the currently-saved model index.
    #[cfg(feature = "sherpa")]
    {
        let initial_visible = sdr_transcription::SherpaModel::ALL
            .get(saved_model_idx as usize)
            .copied()
            .is_some_and(|m| m.supports_partials());
        display_mode_row.set_visible(initial_visible);

        let display_mode_row_for_visibility = display_mode_row.clone();
        model_row.connect_selected_notify(move |r| {
            let idx = r.selected() as usize;
            let visible = sdr_transcription::SherpaModel::ALL
                .get(idx)
                .copied()
                .is_some_and(|m| m.supports_partials());
            display_mode_row_for_visibility.set_visible(visible);
        });
    }
```

The PR 4 decision to chain GLib handlers (both handlers fire on every change) applies here too — the first handler persists the model index, the second toggles visibility.

- [ ] **Step 2: Verify both builds + clippy**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo build --workspace 2>&1 | tail -10
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -10
cargo clippy --all-targets --workspace -- -D warnings 2>&1 | tail -10
```

Expected: all PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/sidebar/transcript_panel.rs
git commit -m "feat(ui): hide display_mode_row for offline Moonshine models

Adds a second GLib-chained handler on model_row.connect_selected_notify
that toggles display_mode_row visibility based on the selected model's
supports_partials(). Moonshine variants hide the Live/Final toggle
because offline decode emits no partials. Initial visibility is set
at panel build from the saved model index.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: UI — lock display_mode_row during transcription

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 1: Add `display_mode_row.set_sensitive(false)` to the "on" branch**

In `crates/sdr-ui/src/window.rs`, locate this block:

```rust
            model_row.set_sensitive(false);
            #[cfg(feature = "whisper")]
            silence_row.set_sensitive(false);
            noise_gate_row.set_sensitive(false);
            // display_mode_row is intentionally NOT locked — the Partial
            // handler re-reads it on every event, so flipping it mid-session
            // is safe and desirable (user sees effect immediately).
```

Replace with:

```rust
            model_row.set_sensitive(false);
            #[cfg(feature = "whisper")]
            silence_row.set_sensitive(false);
            noise_gate_row.set_sensitive(false);
            // All settings lock during a session for mid-session fault
            // tolerance — walks back PR 4's earlier display_mode_row
            // exception. User stops, changes, starts.
            #[cfg(feature = "sherpa")]
            display_mode_row.set_sensitive(false);
```

- [ ] **Step 2: Add `display_mode_row.set_sensitive(true)` to the sync start-error path**

In the same function, locate the sync start-error block (inside `match start_result { ... Err(e) => { ... } }`):

```rust
                Err(e) => {
                    tracing::warn!("failed to start transcription: {e}");
                    model_row.set_sensitive(true);
                    #[cfg(feature = "whisper")]
                    silence_row.set_sensitive(true);
                    noise_gate_row.set_sensitive(true);
```

Add `display_mode_row.set_sensitive(true);` right after `noise_gate_row.set_sensitive(true);`:

```rust
                Err(e) => {
                    tracing::warn!("failed to start transcription: {e}");
                    model_row.set_sensitive(true);
                    #[cfg(feature = "whisper")]
                    silence_row.set_sensitive(true);
                    noise_gate_row.set_sensitive(true);
                    #[cfg(feature = "sherpa")]
                    display_mode_row.set_sensitive(true);
```

- [ ] **Step 3: Add `display_mode_row.set_sensitive(true)` to the async Error-arm teardown**

Locate the `TranscriptionEvent::Error(msg)` arm inside the timeout closure, which contains:

```rust
                                    TranscriptionEvent::Error(msg) => {
                                        if let Some(model) = model_row_weak.upgrade() {
                                            model.set_sensitive(true);
                                        }
                                        #[cfg(feature = "whisper")]
                                        if let Some(silence) = silence_row_weak.upgrade() {
                                            silence.set_sensitive(true);
                                        }
                                        if let Some(noise) = noise_gate_row_weak.upgrade() {
                                            noise.set_sensitive(true);
                                        }
```

After the `noise_gate_row_weak` block, add:

```rust
                                        #[cfg(feature = "sherpa")]
                                        if let Some(display) = display_mode_row_weak.upgrade() {
                                            display.set_sensitive(true);
                                        }
```

- [ ] **Step 4: Add `display_mode_row.set_sensitive(true)` to the `else` (stop) branch**

Locate the else branch of `enable_row.connect_active_notify`:

```rust
        } else {
            model_row.set_sensitive(true);
            #[cfg(feature = "whisper")]
            silence_row.set_sensitive(true);
            noise_gate_row.set_sensitive(true);
            state_clone.send_dsp(crate::messages::UiToDsp::DisableTranscription);
            engine_clone.borrow_mut().shutdown_nonblocking();
```

Add `display_mode_row.set_sensitive(true);` after `noise_gate_row.set_sensitive(true);`:

```rust
        } else {
            model_row.set_sensitive(true);
            #[cfg(feature = "whisper")]
            silence_row.set_sensitive(true);
            noise_gate_row.set_sensitive(true);
            #[cfg(feature = "sherpa")]
            display_mode_row.set_sensitive(true);
            state_clone.send_dsp(crate::messages::UiToDsp::DisableTranscription);
            engine_clone.borrow_mut().shutdown_nonblocking();
```

- [ ] **Step 5: Verify both builds + clippy**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo build --workspace 2>&1 | tail -10
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -10
cargo clippy --all-targets --workspace -- -D warnings 2>&1 | tail -10
```

Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "feat(ui): lock display_mode_row during transcription

Walks back PR 4's 'intentionally NOT locked' exception. All
transcription settings now lock uniformly during an active session —
simpler mental model, eliminates model-switch-during-session edge
cases. Covers on-branch, off-branch, sync start-error, and async
Error teardown paths.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 13: Full workspace lint + fmt + test sweep

**Files:** none (verification only)

- [ ] **Step 1: cargo fmt check**

Run: `cargo fmt --all -- --check`
Expected: PASS with no output. If it reports differences, run `cargo fmt --all` and commit the result with message `chore: cargo fmt`.

- [ ] **Step 2: Whisper clippy**

Run: `cargo clippy --all-targets --workspace -- -D warnings`
Expected: PASS.

- [ ] **Step 3: Whisper tests**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: all passing.

- [ ] **Step 4: Sherpa clippy**

Run: `cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Sherpa tests**

Run: `cargo test --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -20`
Expected: all passing.

- [ ] **Step 6: Commit any fmt cleanup if needed**

If fmt produced fixes, commit them with `chore: cargo fmt`. Otherwise skip.

---

## Task 14: File the Whisper VAD retrofit follow-up issue

**Files:** none (GitHub API action)

- [ ] **Step 1: Use `gh` to create the issue**

```bash
gh issue create --title "Retrofit Whisper backend to use VoiceActivityDetector trait" --body "$(cat <<'EOF'
## Context

The Moonshine integration PR introduced a feature-agnostic \`VoiceActivityDetector\` trait in \`crates/sdr-transcription/src/vad.rs\` along with a sherpa-onnx-backed implementation (\`SherpaSileroVad\`) used by the offline Moonshine session loop.

The trait is deliberately feature-agnostic: it compiles in whisper builds too, with no sherpa-onnx dependency. That was done specifically to make this retrofit straightforward.

## Current Whisper behavior

\`backends/whisper.rs\` uses a naive RMS gate (\`silence_threshold\` slider in the UI) to detect silence between utterances. On RTL-SDR audio, this frequently false-triggers on squelch tails and splits utterances across multiple transcription chunks, which makes the committed text noisy and occasionally cuts words in half.

## Proposed change

1. Add a second \`VoiceActivityDetector\` impl for whisper builds using a pure-Rust Silero crate. Candidates:
   - \`voice_activity_detector\` on crates.io (actively maintained, bundles silero ONNX)
   - \`silero-rs\` (if it exists and is workable)
   - Vendor Silero VAD directly (pull the ONNX file from sherpa-onnx's releases, write a thin inference loop using \`ort\` or \`tract-onnx\`)

2. Wire whisper.rs to use the trait instead of RMS. Retire the \`silence_threshold\` slider (or keep it as a fallback when VAD fails to initialize).

3. Must not add a sherpa-onnx dependency to whisper builds — the feature mutex is non-negotiable.

## Acceptance criteria

- Whisper transcripts stop splitting mid-utterance on squelch tails.
- Whisper builds still compile without any sherpa-onnx deps.
- \`VoiceActivityDetector\` trait shape is unchanged (Moonshine's impl should not need to move).
- Dual-build smoke test (whisper-cuda + sherpa-cpu) still passes.

## Labels

\`enhancement\`, \`transcription\`
EOF
)" --label enhancement 2>&1 | tail -3
```

Note the returned issue number. It will be referenced in the PR description.

---

## Task 15: Manual smoke test (user-executed)

**Files:** none (manual verification)

This task is for the human reviewer. The subagent running this plan should stop before this task, report "Ready for manual smoke test", and hand off.

- [ ] **Step 1: Install the sherpa-cpu build**

```bash
make install CARGO_FLAGS="--release --no-default-features --features sherpa-cpu"
```

- [ ] **Step 2: Zipformer regression (must not break)**

- Delete `~/.local/share/sdr-rs/models/sherpa/silero-vad/` if present, but keep Zipformer installed
- Launch `sdr-rs`
- Transcript panel → Model = "Streaming Zipformer (English)"
- Verify: Display mode row is VISIBLE (Zipformer supports partials)
- Enable transcription → verify live captions still stream in place as before
- Verify Display mode toggle still works (Live captions vs Final only)
- Verify Clear button clears both live line and text view
- Verify: all settings rows (Model, Noise gate, Display mode) are LOCKED during transcription
- Disable transcription → verify all rows unlock

- [ ] **Step 3: Moonshine Tiny clean first-run**

- Delete `~/.local/share/sdr-rs/models/sherpa/silero-vad/` AND `~/.local/share/sdr-rs/models/sherpa/moonshine-tiny-en/`
- Launch `sdr-rs` with Model dropdown set to Moonshine Tiny BEFORE transcription is enabled
- Actually the splash shows while the app is starting — model selection is UI-time, not init-time. So: launch, wait for window, set Model = Moonshine Tiny, enable transcription
- First enable triggers the download flow IF the host was initialized with a different model at startup. Wait — actually the host is initialized once at startup with whatever model was persisted. Switching models after startup does NOT re-init the host in the current architecture.
- **Open question during smoke test**: does changing the model in the UI dropdown and enabling transcription actually swap the recognizer? Looking at the architecture, the `TranscriptionEngine::start` call passes a `ModelChoice::Sherpa(model)` but the SherpaBackend forwards all sessions to the single `global_sherpa_host()` which has ONE recognizer locked in at spawn time. If this is true, the currently-installed host locks the user into whatever model was selected on startup.
- **Actual test**: close sdr-rs. Edit `~/.config/sdr-rs/config.json` (or wherever config lives) to set `transcription_sherpa_model: 1` (MoonshineTinyEn index). Launch. Splash should show "Downloading Silero VAD..." → "Downloading Moonshine Tiny (English)..." → "Extracting Moonshine Tiny (English)..." → "Loading sherpa-onnx recognizer..." → window opens.
- Verify: Display mode row is HIDDEN (Moonshine doesn't support partials)
- Enable transcription → feed known audio
- Verify: text appears in text view on utterance boundaries (~100-300ms after speech ends)
- Verify: no live line ever appears
- Verify: Clear button still works on the text view

- [ ] **Step 4: Moonshine Base clean first-run**

- Close sdr-rs
- Delete `~/.local/share/sdr-rs/models/sherpa/moonshine-base-en/` (leave VAD alone — already downloaded in Step 3)
- Edit config to select Moonshine Base (index 2)
- Launch — splash should NOT show VAD download (already present) but SHOULD show "Downloading Moonshine Base (English)..." then extract then create
- Enable transcription, verify text commits with slightly higher latency than Tiny

- [ ] **Step 5: Settings lock during session**

- With any model active and transcription enabled, try clicking each settings row
- Verify: Model, Noise gate, Display mode (where applicable) are all disabled (greyed out)
- Toggle transcription off → verify all rows re-enable

- [ ] **Step 6: Whisper regression (deferred to tomorrow, near-zero risk)**

- Skip this unless time permits or the user asks for it
- If running: `make install CARGO_FLAGS="--release --features whisper-cuda"`, launch, enable transcription, verify no Display mode row present, verify no live line, verify existing Whisper behavior unchanged

- [ ] **Step 7: Report outcome**

Report to reviewer: all smoke-test steps passed (or list any failures).

---

## Task 16: Open PR

**Files:** none

- [ ] **Step 1: Push the branch**

```bash
git push -u origin feature/moonshine-integration
```

- [ ] **Step 2: Open PR via gh CLI**

Replace `<WHISPER_ISSUE_NUMBER>` in the body below with the issue number from Task 14.

```bash
gh pr create --title "feat(transcription): Moonshine tiny + base via VAD-gated offline path (#224)" --body "$(cat <<'EOF'
## Summary

Adds NVIDIA UsefulSensors Moonshine (tiny and base) as selectable sherpa-onnx models, alongside the existing Streaming Zipformer. Moonshine is offline-only, so this PR introduces a second session path inside the Sherpa backend that uses Silero VAD to detect utterance boundaries before batch decoding each segment through \`OfflineRecognizer\`.

Part of #204 (sherpa-onnx integration epic) — this is PR 5 of the roadmap and proves the multi-model-kind architecture that future PRs (Parakeet, etc.) will slot into.

## What's new

- **Two new model variants:** \`SherpaModel::MoonshineTinyEn\` (~27M params, ~170MB bundle) and \`SherpaModel::MoonshineBaseEn\` (~61M params, ~380MB bundle). User picks based on hardware in the existing model dropdown.
- **Feature-agnostic \`VoiceActivityDetector\` trait** (\`sdr-transcription/src/vad.rs\`) — compiles in whisper builds too for the follow-up retrofit.
- **\`SherpaSileroVad\`** sherpa-onnx-backed trait impl, used by the new offline session loop.
- **Silero VAD auto-download** — ~2MB, downloaded to \`~/.local/share/sdr-rs/models/sherpa/silero-vad/silero_vad.onnx\` on Moonshine first-run.
- **InitEvent component labels** — splash now shows \"Downloading Silero VAD...\", \"Downloading Moonshine Tiny...\", \"Extracting Streaming Zipformer (English)...\" etc. with the actual artifact name.
- **\`backends/sherpa.rs\` split** into a \`sherpa/\` module (\`mod.rs\`, \`host.rs\`, \`streaming.rs\`, \`offline.rs\`, \`silero_vad.rs\`) for clarity.
- **\`display_mode_row\` contextually hidden** for Moonshine models (offline, no partials) and now **locked during transcription** — walks back PR 4's mid-session exception for a simpler \"all settings lock during session\" rule.

## Follow-up

Filed #<WHISPER_ISSUE_NUMBER> — retrofit Whisper to use the \`VoiceActivityDetector\` trait. Whisper currently uses RMS gating which false-triggers on RTL-SDR squelch tails and splits utterances. This PR intentionally leaves Whisper alone.

## Test plan

- [x] \`cargo fmt --all -- --check\`
- [x] \`cargo clippy --all-targets --workspace -- -D warnings\` (Whisper)
- [x] \`cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings\` (Sherpa)
- [x] \`cargo test --workspace\` (both flavors)
- [x] Manual (sherpa-cpu): Zipformer regression, Moonshine Tiny clean first-run with VAD+bundle download, Moonshine Base clean first-run with bundle download only, all settings lock during session, Clear button wipes live line AND text view
- [ ] Manual (whisper-cuda): deferred to next-day regression check — near-zero risk since all Moonshine code is \`#[cfg(feature = \"sherpa\")]\`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Return the PR URL to the user**
