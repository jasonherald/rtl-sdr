# Parakeet-TDT Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add NVIDIA Parakeet-TDT-0.6b-v3 as a fourth selectable `SherpaModel` variant, slotting into the existing offline (VAD-gated) recognizer path that PR 5 built for Moonshine.

**Architecture:** Parakeet is offline in sherpa-onnx 1.12, not online. Adds one new `ModelKind` variant (`OfflineNemoTransducer`), one new `SherpaModel` variant (`ParakeetTdt06bV3En`), and one new recognizer config builder (`build_nemo_transducer_recognizer_config`) that wraps `OfflineTransducerModelConfig` + `model_type = "nemo_transducer"`. The host `init_offline` function gains a `match model.kind()` dispatch to pick the right config builder. The session loop, VAD, UI, and runtime model reload are all unchanged — they're already generic over offline recognizers.

**Tech Stack:** Rust 2024, sherpa-onnx 1.12.36 (`OfflineRecognizer` + `OfflineTransducerModelConfig` with `nemo_transducer` model type), existing PR 5 offline session loop, existing PR 5 runtime model reload.

---

## File Structure

**Modified files only — no new files:**

- `crates/sdr-transcription/src/sherpa_model.rs` — add `OfflineNemoTransducer` to `ModelKind`, add `ParakeetTdt06bV3En` to `SherpaModel`, extend all match arms (`label`, `dir_name`, `archive_filename`, `archive_inner_directory`, `kind`, `supports_partials`), extend `model_file_paths`, extend `ALL`, update existing `all_contains_three_variants` test → four, add new Parakeet unit tests
- `crates/sdr-transcription/src/backends/sherpa/offline.rs` — add `build_nemo_transducer_recognizer_config` function alongside the existing `build_moonshine_recognizer_config`
- `crates/sdr-transcription/src/backends/sherpa/host.rs` — extend three `match model.kind()` locations (initial dispatch in `run_host_loop`, `ReloadRecognizer` arm, recognizer config selection inside `init_offline`); update `init_offline` doc comment to be generic across offline kinds; update `ModelKind` doc comments to mention the new variant

That's it. No new files, no new modules. PR 5 already built every piece of infrastructure Parakeet needs.

---

## Task 1: Add `OfflineNemoTransducer` to `ModelKind`

**Files:**
- Modify: `crates/sdr-transcription/src/sherpa_model.rs`

This task adds the new ModelKind variant in isolation. Match arms in `kind()`, `supports_partials()`, and the host file all break temporarily because they're exhaustive — Task 2 and Task 4 fix them. We keep the build broken between this and Task 2 because the change is mechanically forced by the compiler and Task 2 lands within minutes.

> **Important:** Tasks 1, 2, 3, and 4 must be executed as a sequential block without spec/quality reviews between them — Task 1 leaves the build broken (compile errors in `kind()`, `supports_partials()`, `model_file_paths()`, and three locations in `host.rs`), and the build only goes green again at the end of Task 4. Run all four tasks, verify the combined build, then do a single review pass covering all four commits.

- [ ] **Step 1: Update the `ModelKind` enum**

In `crates/sdr-transcription/src/sherpa_model.rs`, find:

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

Replace with:

```rust
/// Which sherpa-onnx recognizer family a model belongs to.
///
/// Drives host init branching and session loop dispatch. Online
/// models run through `OnlineRecognizer` + streaming chunks;
/// offline models run through `OfflineRecognizer` + external VAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// Streaming transducer: Zipformer today. Uses `OnlineRecognizer`
    /// + streaming session loop in `backends/sherpa/streaming.rs`.
    OnlineTransducer,
    /// Offline encoder-decoder: Moonshine v1. Requires external VAD
    /// (Silero) to detect utterance boundaries before batch decoding.
    /// Uses `OfflineRecognizer` with `OfflineMoonshineModelConfig`
    /// + the offline session loop in `backends/sherpa/offline.rs`.
    OfflineMoonshine,
    /// Offline transducer-style model from NVIDIA NeMo: Parakeet-TDT
    /// today. Uses `OfflineRecognizer` with `OfflineTransducerModelConfig`
    /// + `model_type = "nemo_transducer"`. Shares the same VAD-gated
    /// offline session loop as `OfflineMoonshine`; only the recognizer
    /// config builder differs.
    OfflineNemoTransducer,
}
```

Note: the previous `OnlineTransducer` doc said "Parakeet-TDT in a future PR" — that was wrong (Parakeet is offline). The corrected doc removes the misleading claim.

- [ ] **Step 2: Commit (build is intentionally broken until Task 4)**

```bash
git add crates/sdr-transcription/src/sherpa_model.rs
git commit -m "feat(transcription): add OfflineNemoTransducer to ModelKind

Adds the third ModelKind variant for NVIDIA NeMo offline transducer
models (Parakeet-TDT today). Shares the offline session loop with
Moonshine; only the recognizer config builder differs.

Also corrects the OnlineTransducer doc comment which previously
claimed Parakeet was a future streaming addition — Parakeet turned
out to be offline-only in sherpa-onnx 1.12.

Build is intentionally broken at this commit: exhaustive matches on
ModelKind in supports_partials(), the host run_host_loop dispatch,
the ReloadRecognizer dispatch, and init_offline are all missing the
new arm. Tasks 2-4 close the build.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Add `ParakeetTdt06bV3En` to `SherpaModel`

**Files:**
- Modify: `crates/sdr-transcription/src/sherpa_model.rs`

Adds the variant + every match arm + tests. Single task with multiple steps, single commit at the end. After this task the `sherpa_model.rs` file compiles cleanly on its own; `host.rs` still has compile errors waiting for Task 4.

- [ ] **Step 1: Add the variant to `SherpaModel`**

In `crates/sdr-transcription/src/sherpa_model.rs`, find:

```rust
/// Available sherpa-onnx model variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SherpaModel {
    /// Streaming Zipformer English (k2-fsa, 2023-06-26).
    StreamingZipformerEn,
    /// Moonshine Tiny (`UsefulSensors`, English, int8). ~27M params,
    /// ~170MB bundle. Fastest Moonshine variant — best for CPU-only
    /// and low-end hardware. Offline (VAD-gated) decode.
    MoonshineTinyEn,
    /// Moonshine Base (`UsefulSensors`, English, int8). ~61M params,
    /// ~380MB bundle. More accurate than Tiny, higher per-utterance
    /// latency. Offline (VAD-gated) decode.
    MoonshineBaseEn,
}
```

Replace with:

```rust
/// Available sherpa-onnx model variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SherpaModel {
    /// Streaming Zipformer English (k2-fsa, 2023-06-26).
    StreamingZipformerEn,
    /// Moonshine Tiny (`UsefulSensors`, English, int8). ~27M params,
    /// ~170MB bundle. Fastest Moonshine variant — best for CPU-only
    /// and low-end hardware. Offline (VAD-gated) decode.
    MoonshineTinyEn,
    /// Moonshine Base (`UsefulSensors`, English, int8). ~61M params,
    /// ~380MB bundle. More accurate than Tiny, higher per-utterance
    /// latency. Offline (VAD-gated) decode.
    MoonshineBaseEn,
    /// NVIDIA Parakeet-TDT-0.6b v3 (English, int8). ~600M params,
    /// ~600MB bundle. Highest accuracy — currently #1 on the OpenASR
    /// leaderboard. CPU-only today (sherpa-cuda follow-up tracked).
    /// Offline (VAD-gated) batch decode through a NeMo transducer.
    ParakeetTdt06bV3En,
}
```

- [ ] **Step 2: Extend `label()`**

Replace the `label` method body:

```rust
    pub fn label(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "Streaming Zipformer (English)",
            Self::MoonshineTinyEn => "Moonshine Tiny (English)",
            Self::MoonshineBaseEn => "Moonshine Base (English)",
            Self::ParakeetTdt06bV3En => "Parakeet TDT 0.6b v3 (English)",
        }
    }
```

- [ ] **Step 3: Extend `dir_name()`**

Replace the `dir_name` method body:

```rust
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "streaming-zipformer-en",
            Self::MoonshineTinyEn => "moonshine-tiny-en",
            Self::MoonshineBaseEn => "moonshine-base-en",
            Self::ParakeetTdt06bV3En => "parakeet-tdt-0.6b-v3-en",
        }
    }
```

- [ ] **Step 4: Extend `archive_filename()`**

Replace the `archive_filename` method body:

```rust
    pub fn archive_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2",
            Self::MoonshineTinyEn => "sherpa-onnx-moonshine-tiny-en-int8.tar.bz2",
            Self::MoonshineBaseEn => "sherpa-onnx-moonshine-base-en-int8.tar.bz2",
            Self::ParakeetTdt06bV3En => "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        }
    }
```

- [ ] **Step 5: Extend `archive_inner_directory()`**

Replace the `archive_inner_directory` method body:

```rust
    pub fn archive_inner_directory(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "sherpa-onnx-streaming-zipformer-en-2023-06-26",
            Self::MoonshineTinyEn => "sherpa-onnx-moonshine-tiny-en-int8",
            Self::MoonshineBaseEn => "sherpa-onnx-moonshine-base-en-int8",
            Self::ParakeetTdt06bV3En => "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
        }
    }
```

- [ ] **Step 6: Extend `kind()`**

Replace the `kind` method body:

```rust
    pub fn kind(self) -> ModelKind {
        match self {
            Self::StreamingZipformerEn => ModelKind::OnlineTransducer,
            Self::MoonshineTinyEn | Self::MoonshineBaseEn => ModelKind::OfflineMoonshine,
            Self::ParakeetTdt06bV3En => ModelKind::OfflineNemoTransducer,
        }
    }
```

- [ ] **Step 7: Extend `supports_partials()`**

Replace the `supports_partials` method body:

```rust
    pub fn supports_partials(self) -> bool {
        match self.kind() {
            ModelKind::OnlineTransducer => true,
            ModelKind::OfflineMoonshine | ModelKind::OfflineNemoTransducer => false,
        }
    }
```

- [ ] **Step 8: Extend `ALL`**

Replace the `ALL` constant:

```rust
    pub const ALL: &[Self] = &[
        Self::StreamingZipformerEn,
        Self::MoonshineTinyEn,
        Self::MoonshineBaseEn,
        Self::ParakeetTdt06bV3En,
    ];
```

- [ ] **Step 9: Extend `model_file_paths()`**

Locate the `model_file_paths` function. Add a new arm for Parakeet — it reuses the existing `Transducer` variant (same 4-file shape as Zipformer):

```rust
pub fn model_file_paths(model: SherpaModel) -> ModelFilePaths {
    let dir = model_directory(model);
    match model {
        SherpaModel::StreamingZipformerEn => ModelFilePaths::Transducer {
            encoder: dir.join("encoder-epoch-99-avg-1-chunk-16-left-128.onnx"),
            decoder: dir.join("decoder-epoch-99-avg-1-chunk-16-left-128.onnx"),
            joiner: dir.join("joiner-epoch-99-avg-1-chunk-16-left-128.onnx"),
            tokens: dir.join("tokens.txt"),
        },
        // Moonshine v1 five-file layout (k2-fsa int8 releases): the
        // preprocessor is NOT quantized (`preprocess.onnx`, not `.int8.onnx`).
        SherpaModel::MoonshineTinyEn | SherpaModel::MoonshineBaseEn => ModelFilePaths::Moonshine {
            preprocessor: dir.join("preprocess.onnx"),
            encoder: dir.join("encode.int8.onnx"),
            uncached_decoder: dir.join("uncached_decode.int8.onnx"),
            cached_decoder: dir.join("cached_decode.int8.onnx"),
            tokens: dir.join("tokens.txt"),
        },
        // Parakeet-TDT v3 int8 layout: standard 4-file transducer
        // (encoder + decoder + joiner + tokens), structurally identical
        // to Zipformer. The `Transducer` ModelFilePaths variant is
        // reused — kind() tells the host which recognizer API to feed
        // them into (online for Zipformer vs offline for Parakeet).
        SherpaModel::ParakeetTdt06bV3En => ModelFilePaths::Transducer {
            encoder: dir.join("encoder.int8.onnx"),
            decoder: dir.join("decoder.int8.onnx"),
            joiner: dir.join("joiner.int8.onnx"),
            tokens: dir.join("tokens.txt"),
        },
    }
}
```

- [ ] **Step 10: Update the existing `all_contains_three_variants` test**

Find:

```rust
    #[test]
    fn all_contains_three_variants() {
        assert_eq!(SherpaModel::ALL.len(), 3);
    }
```

Replace with:

```rust
    #[test]
    fn all_contains_four_variants() {
        assert_eq!(SherpaModel::ALL.len(), 4);
    }
```

- [ ] **Step 11: Add new Parakeet unit tests**

In the same `#[cfg(test)] mod tests` block, add these four tests right after `all_contains_four_variants`:

```rust
    #[test]
    fn parakeet_is_offline_nemo_transducer_kind() {
        assert_eq!(
            SherpaModel::ParakeetTdt06bV3En.kind(),
            ModelKind::OfflineNemoTransducer
        );
    }

    #[test]
    fn parakeet_does_not_support_partials() {
        assert!(!SherpaModel::ParakeetTdt06bV3En.supports_partials());
    }

    #[test]
    #[allow(clippy::panic)]
    fn parakeet_has_transducer_file_layout() {
        let paths = model_file_paths(SherpaModel::ParakeetTdt06bV3En);
        let ModelFilePaths::Transducer {
            encoder,
            decoder,
            joiner,
            tokens,
        } = paths
        else {
            panic!("ParakeetTdt06bV3En should be a Transducer layout");
        };
        assert!(encoder.ends_with("encoder.int8.onnx"));
        assert!(decoder.ends_with("decoder.int8.onnx"));
        assert!(joiner.ends_with("joiner.int8.onnx"));
        assert!(tokens.ends_with("tokens.txt"));
        assert_ne!(encoder, decoder);
        assert_ne!(decoder, joiner);
    }

    #[test]
    fn parakeet_archive_url_is_well_formed() {
        let url = SherpaModel::ParakeetTdt06bV3En.archive_url();
        assert!(url.starts_with("https://github.com/k2-fsa/sherpa-onnx/"));
        assert!(url.ends_with(".tar.bz2"));
        assert!(url.contains("parakeet"));
        assert!(url.contains("tdt"));
        assert!(url.contains("v3"));
    }
```

- [ ] **Step 12: Verify sherpa_model.rs compiles cleanly in isolation**

Run:
```bash
cargo build -p sdr-transcription --no-default-features --features sherpa-cpu 2>&1 | tail -20
```
Expected: FAIL with errors only in `backends/sherpa/host.rs` — specifically the three exhaustive matches on `ModelKind` and the `init_offline` body referencing `build_moonshine_recognizer_config`. NO errors should appear in `sherpa_model.rs` itself. If there are errors in `sherpa_model.rs`, STOP and report BLOCKED.

- [ ] **Step 13: Commit**

```bash
git add crates/sdr-transcription/src/sherpa_model.rs
git commit -m "feat(transcription): add ParakeetTdt06bV3En to SherpaModel

Adds the variant with full metadata (label, dir_name, archive_filename,
archive_inner_directory). kind() returns OfflineNemoTransducer.
supports_partials() returns false (offline model — no partials, same
as Moonshine). model_file_paths reuses ModelFilePaths::Transducer
because Parakeet ships the standard 4-file transducer layout
(encoder/decoder/joiner/tokens) — same shape as Zipformer, only the
recognizer family differs.

ALL extended to four variants. Renames the all_contains_three_variants
test to all_contains_four_variants. Adds four new Parakeet tests:
kind, supports_partials, file layout, archive URL.

Build still broken in host.rs — Task 4 closes it.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Add `build_nemo_transducer_recognizer_config` in `offline.rs`

**Files:**
- Modify: `crates/sdr-transcription/src/backends/sherpa/offline.rs`

Adds the new builder function alongside the existing `build_moonshine_recognizer_config`. After this task the offline.rs file compiles cleanly on its own; host.rs still has compile errors until Task 4.

- [ ] **Step 1: Add `OfflineTransducerModelConfig` to the imports**

In `crates/sdr-transcription/src/backends/sherpa/offline.rs`, find the existing `use sherpa_onnx::` block. It currently looks like:

```rust
use sherpa_onnx::{
    OfflineModelConfig, OfflineMoonshineModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
};
```

Replace with:

```rust
use sherpa_onnx::{
    OfflineModelConfig, OfflineMoonshineModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
    OfflineTransducerModelConfig,
};
```

- [ ] **Step 2: Add the `build_nemo_transducer_recognizer_config` function**

In the same file, find the existing `build_moonshine_recognizer_config` function. Immediately after its closing `}`, insert this new function:

```rust
/// Build the `OfflineRecognizerConfig` for a NeMo Parakeet-TDT model.
///
/// Uses sherpa-onnx's offline transducer config (4 files: encoder,
/// decoder, joiner, tokens) with `model_type = "nemo_transducer"`.
/// The model_type field is required — without it, sherpa-onnx tries
/// to use the generic transducer decode loop which doesn't understand
/// NeMo's TDT (Token-and-Duration Transducer) joiner output shape.
///
/// Mirrors the upstream `rust-api-examples/examples/nemo_parakeet.rs`
/// example.
pub(super) fn build_nemo_transducer_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> OfflineRecognizerConfig {
    let ModelFilePaths::Transducer {
        encoder,
        decoder,
        joiner,
        tokens,
    } = sherpa_model::model_file_paths(model)
    else {
        unreachable!(
            "offline::build_nemo_transducer_recognizer_config called with non-Transducer layout"
        )
    };

    let transducer = OfflineTransducerModelConfig {
        encoder: Some(encoder.to_string_lossy().into_owned()),
        decoder: Some(decoder.to_string_lossy().into_owned()),
        joiner: Some(joiner.to_string_lossy().into_owned()),
    };

    let model_config = OfflineModelConfig {
        transducer,
        tokens: Some(tokens.to_string_lossy().into_owned()),
        provider: Some(provider.to_owned()),
        num_threads: SHERPA_NUM_THREADS,
        // Required — tells sherpa-onnx to use NeMo's TDT decode loop
        // instead of the generic transducer path.
        model_type: Some("nemo_transducer".to_owned()),
        ..OfflineModelConfig::default()
    };

    OfflineRecognizerConfig {
        model_config,
        ..OfflineRecognizerConfig::default()
    }
}
```

- [ ] **Step 3: Verify offline.rs compiles cleanly in isolation**

Run:
```bash
cargo build -p sdr-transcription --no-default-features --features sherpa-cpu 2>&1 | tail -20
```
Expected: still FAILS, but the failure should now be ONLY about `host.rs` exhaustive matches. NO errors should mention `offline.rs`. If there are errors in `offline.rs`, STOP and report BLOCKED.

Common failure modes to watch:
- If `OfflineTransducerModelConfig` has additional fields beyond `encoder`/`decoder`/`joiner` in this version of sherpa-onnx, the compiler will say "missing field". The plan was verified against sherpa-onnx 1.12.36 where the struct is exactly those three `Option<String>` fields. If the version drifted, read the struct definition at `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/sherpa-onnx-1.12.36/src/offline_asr.rs:49` and adjust.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-transcription/src/backends/sherpa/offline.rs
git commit -m "feat(transcription): add Parakeet (nemo_transducer) recognizer builder

New build_nemo_transducer_recognizer_config alongside the existing
build_moonshine_recognizer_config. Wraps OfflineTransducerModelConfig
with model_type = 'nemo_transducer' per the upstream nemo_parakeet.rs
example.

Build still broken in host.rs — Task 4 closes it.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Branch host init on `OfflineNemoTransducer`

**Files:**
- Modify: `crates/sdr-transcription/src/backends/sherpa/host.rs`

Updates the three `match model.kind()` locations in `host.rs` to handle the new variant, and adds the recognizer config dispatch inside `init_offline`. After this task the build is fully green again.

- [ ] **Step 1: Update the initial dispatch in `run_host_loop`**

In `crates/sdr-transcription/src/backends/sherpa/host.rs`, find this block inside `run_host_loop`:

```rust
    use crate::sherpa_model::ModelKind;

    let recognizer_state = match model.kind() {
        ModelKind::OnlineTransducer => match init_online(model, &event_tx) {
            Ok(state) => state,
            Err(()) => return, // init_online already published Failed and stored the error
        },
        ModelKind::OfflineMoonshine => match init_offline(model, &event_tx) {
            Ok(state) => state,
            Err(()) => return,
        },
    };
```

Replace with:

```rust
    use crate::sherpa_model::ModelKind;

    let recognizer_state = match model.kind() {
        ModelKind::OnlineTransducer => match init_online(model, &event_tx) {
            Ok(state) => state,
            Err(()) => return, // init_online already published Failed and stored the error
        },
        // Both offline kinds share init_offline — only the recognizer
        // config builder differs, and that branching happens inside
        // init_offline based on model.kind() again.
        ModelKind::OfflineMoonshine | ModelKind::OfflineNemoTransducer => {
            match init_offline(model, &event_tx) {
                Ok(state) => state,
                Err(()) => return,
            }
        }
    };
```

- [ ] **Step 2: Update the `ReloadRecognizer` dispatch**

Still in `host.rs`, find this block inside the `HostCommand::ReloadRecognizer` arm:

```rust
                let new_state = match new_model.kind() {
                    crate::sherpa_model::ModelKind::OnlineTransducer => {
                        init_online(new_model, &event_tx)
                    }
                    crate::sherpa_model::ModelKind::OfflineMoonshine => {
                        init_offline(new_model, &event_tx)
                    }
                };
```

Replace with:

```rust
                let new_state = match new_model.kind() {
                    crate::sherpa_model::ModelKind::OnlineTransducer => {
                        init_online(new_model, &event_tx)
                    }
                    crate::sherpa_model::ModelKind::OfflineMoonshine
                    | crate::sherpa_model::ModelKind::OfflineNemoTransducer => {
                        init_offline(new_model, &event_tx)
                    }
                };
```

- [ ] **Step 3: Update `init_offline` to dispatch on model kind**

Still in `host.rs`, find the `init_offline` function. Locate this part of its body (the existing `let recognizer_config = ... build_moonshine_recognizer_config ...` line):

```rust
    let _ = event_tx.send(InitEvent::CreatingRecognizer);
    let recognizer_config = super::offline::build_moonshine_recognizer_config(model, "cpu");
    tracing::info!(?model, "creating sherpa-onnx OfflineRecognizer (Moonshine)");

    let Some(recognizer) = OfflineRecognizer::create(&recognizer_config) else {
        let msg = "OfflineRecognizer::create returned None — check Moonshine model files".to_owned();
```

Replace with:

```rust
    let _ = event_tx.send(InitEvent::CreatingRecognizer);
    // Both offline kinds use OfflineRecognizer but with different config
    // builders. Branch here so init_offline's Phase 1-2 (download VAD +
    // bundle) stays generic across all offline models.
    let recognizer_config = match model.kind() {
        crate::sherpa_model::ModelKind::OfflineMoonshine => {
            super::offline::build_moonshine_recognizer_config(model, "cpu")
        }
        crate::sherpa_model::ModelKind::OfflineNemoTransducer => {
            super::offline::build_nemo_transducer_recognizer_config(model, "cpu")
        }
        crate::sherpa_model::ModelKind::OnlineTransducer => {
            unreachable!("init_offline called with an online model")
        }
    };
    tracing::info!(?model, "creating sherpa-onnx OfflineRecognizer");

    let Some(recognizer) = OfflineRecognizer::create(&recognizer_config) else {
        let msg = format!(
            "OfflineRecognizer::create returned None for {} — check model files and model_type",
            model.label()
        );
```

Note two adjustments:
- The `tracing::info!` message drops the "(Moonshine)" parenthetical because it now applies to both offline kinds.
- The error message becomes generic across both offline kinds AND mentions `model_type` since that's a common Parakeet-specific failure mode.

The next line in the existing code is `tracing::error!(%msg);`. Since the `msg` is now constructed via `format!` instead of `to_owned()`, no other changes needed — the rest of the error-reporting flow uses `msg.clone()` and is unaffected.

- [ ] **Step 4: Update `init_offline`'s doc comment to be generic**

Still in `host.rs`, find the doc comment immediately above `fn init_offline`:

```rust
/// Phase 1-2 for the `OfflineMoonshine` path: download the Silero VAD
/// model if missing, download the Moonshine bundle if missing, then
/// create the `OfflineRecognizer` + `SherpaSileroVad`. Returns `Err(())`
/// on any failure — the error has already been stored in `SHERPA_HOST`
/// and emitted as `InitEvent::Failed`.
fn init_offline(
```

Replace with:

```rust
/// Phase 1-2 for any offline model (`OfflineMoonshine` or
/// `OfflineNemoTransducer`): download the Silero VAD if missing,
/// download the model bundle if missing, then build the right
/// `OfflineRecognizerConfig` for the model's kind and create the
/// `OfflineRecognizer` + `SherpaSileroVad`.
///
/// The recognizer config builder is selected via `model.kind()` so
/// callers don't need to know which offline family they're using.
/// Returns `Err(())` on any failure — the error has already been
/// stored in `SHERPA_HOST` and emitted as `InitEvent::Failed`.
fn init_offline(
```

- [ ] **Step 5: Verify both builds compile + clippy clean**

Run:
```bash
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo build --workspace 2>&1 | tail -10
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -10
cargo clippy --all-targets --workspace -- -D warnings 2>&1 | tail -10
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu 2>&1 | tail -25
```
Expected: all PASS. The build was broken from Task 1 through Task 3; this is the commit that closes it.

If clippy complains about `match self.kind() { ... | ... => false, _ => true }` style issues in `supports_partials` from Task 2, normalize using whatever pattern clippy prefers and report the adjustment.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-transcription/src/backends/sherpa/host.rs
git commit -m "feat(transcription): branch host init on OfflineNemoTransducer

Updates all three match locations in host.rs to handle the new
ModelKind::OfflineNemoTransducer variant:

1. run_host_loop initial dispatch — both offline kinds share
   init_offline, only the recognizer config builder differs
2. ReloadRecognizer arm — same pattern
3. init_offline body — branches on model.kind() to call either
   build_moonshine_recognizer_config or
   build_nemo_transducer_recognizer_config

init_offline doc comment generalized to mention both offline
families. Recognizer creation error message now mentions model_type
since that's a common Parakeet-specific misconfiguration mode.

Closes the build that was intentionally broken from Task 1 onward.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Full lint + fmt + test sweep both flavors

**Files:** none (verification only)

- [ ] **Step 1: cargo fmt check**

Run: `cargo fmt --all -- --check`
Expected: PASS with no output. If it reports differences, run `cargo fmt --all` and commit the result with `chore: cargo fmt`.

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

Run: `cargo test --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -25`
Expected: all passing, including the four new Parakeet tests from Task 2 Step 11 and the renamed `all_contains_four_variants` test.

- [ ] **Step 6: Commit any fmt cleanup**

If fmt produced fixes, commit them with `chore: cargo fmt`. Otherwise skip.

---

## Task 6: File the sherpa-cuda follow-up issue

**Files:** none (GitHub API action)

- [ ] **Step 1: Create the issue via gh CLI**

```bash
gh issue create --title "Add sherpa-onnx CUDA provider support for large offline models" --body "$(cat <<'EOF'
## Context

PR 6 (#TBD) ships NVIDIA Parakeet-TDT-0.6b-v3 as a fourth selectable Sherpa model. Parakeet has 600M parameters — roughly 10x the size of Moonshine Base — and runs through sherpa-onnx's CPU execution provider today.

## Problem

Per-utterance latency on CPU is significant for a model this size. Users with CUDA-capable GPUs (the maintainer's daily-driver RTX 4080 Super, for instance) get no benefit from their hardware.

## Proposed change

Add a \`sherpa-cuda\` cargo feature alongside the existing \`sherpa-cpu\` feature in \`crates/sdr-transcription/Cargo.toml\`:

1. Investigate which sherpa-onnx 1.12 cargo feature flag enables the CUDA execution provider (likely \`cuda\` or \`cuda-provider\` — verify at https://crates.io/crates/sherpa-onnx).
2. Add \`sherpa-cuda = [\"dep:sherpa-onnx\", \"sherpa-onnx/cuda\", \"dep:tar\", \"dep:bzip2\"]\` (adjust the inner feature name to match upstream).
3. Update the hardcoded \`\"cpu\"\` provider strings in \`crates/sdr-transcription/src/backends/sherpa/host.rs::init_online\` and \`init_offline\` to be selected via \`#[cfg]\`:
   \`\`\`rust
   #[cfg(feature = \"sherpa-cuda\")]
   const SHERPA_PROVIDER: &str = \"cuda\";
   #[cfg(not(feature = \"sherpa-cuda\"))]
   const SHERPA_PROVIDER: &str = \"cpu\";
   \`\`\`
4. Pass \`SHERPA_PROVIDER\` to both \`build_recognizer_config\` (streaming) and the offline config builders (\`build_moonshine_recognizer_config\`, \`build_nemo_transducer_recognizer_config\`).
5. Update the install commands documented in CLAUDE.md and \`project_current_state.md\` memory to include the new flag:
   \`\`\`
   make install CARGO_FLAGS=\"--release --no-default-features --features sherpa-cuda\"
   \`\`\`

## Constraints

- **Must remain mutually exclusive with whisper features** — extend the existing \`compile_error!\` guards in \`crates/sdr-transcription/src/lib.rs\` to forbid \`whisper-*\` + \`sherpa-cuda\` combinations.
- **Must not break the dual-build CI smoke test** — CI continues to exercise \`whisper-cuda\` (default) and \`sherpa-cpu\`. The new \`sherpa-cuda\` build is opt-in for users with the right hardware.
- Initial PR can leave \`sherpa-cuda\` out of CI to avoid the GPU-runner cost. Add it to a separate runner later if it proves important.

## Acceptance criteria

- \`make install CARGO_FLAGS=\"--release --no-default-features --features sherpa-cuda\"\` builds on a machine with CUDA installed
- sherpa-onnx logs confirm the CUDA execution provider is in use at recognizer creation
- Parakeet decode latency drops to GPU-acceptable levels (target: <100ms per utterance for typical radio chatter on RTX 4080-class hardware)
- Whisper, Zipformer, and Moonshine still work in their respective builds
- Dual-build smoke test (\`whisper-cuda\` + \`sherpa-cpu\`) still passes

## Labels

\`enhancement\`, \`transcription\`
EOF
)" --label enhancement 2>&1 | tail -3
```

Note the issue number it returns. It will be referenced in the PR description (Task 8).

---

## Task 7: Manual smoke test (user-executed)

**Files:** none (manual verification)

This task is for the human reviewer. The subagent running this plan should stop before this task, report "Ready for manual smoke test", and hand off.

- [ ] **Step 1: Install the sherpa-cpu build**

```bash
make install CARGO_FLAGS="--release --no-default-features --features sherpa-cpu"
```

- [ ] **Step 2: Zipformer regression**

- Launch `sdr-rs`. Whatever model is currently persisted in config loads at startup as usual.
- In the transcript panel, switch model to "Streaming Zipformer (English)" if not already selected.
- Display mode row should be VISIBLE (Zipformer supports partials).
- Enable transcription on a known audio source (talk radio, RadioReference stream, etc.).
- Verify live captions stream in place exactly as PR 5 shipped. No regression.

- [ ] **Step 3: Moonshine regression**

- Switch to "Moonshine Tiny (English)". Display mode row should HIDE.
- Enable transcription, verify per-utterance commits work and accuracy is what you remember from PR 5.
- Switch to "Moonshine Base (English)". Same checks.

- [ ] **Step 4: Parakeet clean first-run**

- Confirm Parakeet bundle is NOT yet downloaded:
  ```bash
  ls ~/.local/share/sdr-rs/models/sherpa/parakeet-tdt-0.6b-v3-en/ 2>&1
  ```
  Should report "No such file or directory".
- Switch model to "Parakeet TDT 0.6b v3 (English)" in the dropdown.
- Status label should appear: "Reloading Parakeet TDT 0.6b v3 (English)..."
- Splash-style progress should show:
  - "Downloading Parakeet TDT 0.6b v3 (English)..." with percent (Silero VAD already cached from PR 5 testing — won't re-download)
  - "Extracting Parakeet TDT 0.6b v3 (English)..."
  - "Creating recognizer..." (this is the moment of truth — Parakeet's first-ever creation in this binary)
  - Status clears
- Display mode row should HIDE (Parakeet is offline).
- Enable transcription, feed real radio chatter.
- Verify text commits land in the text view per utterance. Note the latency.

- [ ] **Step 5: Parakeet accuracy spot-check**

- Find a radio source with tricky content (call signs, numbers, accented speakers).
- Compare Parakeet's commits against your memory of Moonshine Base on similar audio.
- Subjective expectation: Parakeet should be noticeably more accurate, especially on numbers and proper nouns.
- Note: latency may be uncomfortable on CPU. That's expected and the sherpa-cuda issue you just filed is the answer.

- [ ] **Step 6: Runtime swap stress test**

- With transcription stopped, switch among all four models in succession: Zipformer → Moonshine Tiny → Moonshine Base → Parakeet → Zipformer.
- Each switch should reload cleanly, no crashes, status label flashes "Reloading..." then clears.
- Re-enable transcription on each to confirm the recognizer is actually live for each switch.

- [ ] **Step 7: Whisper regression (deferred OK)**

- Skip unless you want to verify before the PR opens. Risk is near-zero since all changes are sherpa-gated.
- If running:
  ```bash
  make install CARGO_FLAGS="--release --features whisper-cuda"
  ```
- Launch, enable transcription on the existing Whisper model. Confirm Display mode row is NOT present, no Parakeet in the dropdown, existing Whisper behavior unchanged.

- [ ] **Step 8: Report outcome**

Report: all smoke-test steps passed (or list any failures). Include rough Parakeet latency observation so the sherpa-cuda issue can be appropriately prioritized.

---

## Task 8: Push and open PR

**Files:** none

- [ ] **Step 1: Push the branch**

```bash
git push -u origin feature/parakeet-integration
```

- [ ] **Step 2: Create the PR**

Replace `<CUDA_ISSUE_NUMBER>` with the issue number from Task 6.

```bash
gh pr create --title "feat(transcription): NVIDIA Parakeet-TDT 0.6b v3 via offline path (#223)" --body "$(cat <<'EOF'
## Summary

Adds NVIDIA Parakeet-TDT-0.6b-v3 as a fourth selectable Sherpa model — currently #1 on the OpenASR leaderboard and the highest-accuracy ASR option in the dropdown. Slots into the offline (VAD-gated) recognizer path PR 5 built for Moonshine; no new infrastructure, just data + one new recognizer config builder + one new ModelKind dispatch arm.

Closes #223. Part of #204 (sherpa-onnx integration epic) — this is PR 6 of the roadmap.

## Critical correction to PR 5's memory

Yesterday's notes claimed Parakeet would slot into the existing `OnlineTransducer` streaming path because Parakeet is "RNNT family". That was wrong — sherpa-onnx 1.12 only exposes Parakeet through `OfflineRecognizer` (verified against the upstream `rust-api-examples/examples/nemo_parakeet.rs` example). Parakeet shares the offline UX with Moonshine: per-utterance commits, no live captions, Display mode toggle hidden.

## What's new

- **`SherpaModel::ParakeetTdt06bV3En`** — fourth dropdown entry, ~600MB int8 bundle, ~600M parameters.
- **`ModelKind::OfflineNemoTransducer`** — third recognizer family, distinguishes Parakeet from Moonshine for config-builder dispatch.
- **`build_nemo_transducer_recognizer_config`** in `backends/sherpa/offline.rs` — wraps `OfflineTransducerModelConfig` with `model_type = "nemo_transducer"` per the upstream example.
- **`init_offline` dispatches on `model.kind()`** to pick the right recognizer config builder. Phase 1-2 (download VAD + model bundle) stays generic across all offline kinds.
- **`ModelFilePaths::Transducer` reused** — Parakeet ships the standard 4-file transducer layout (encoder/decoder/joiner/tokens), structurally identical to Zipformer. The variant is overloaded by intent; `kind()` discriminates the recognizer family.
- **Doc-comment cleanup** — corrected the `OnlineTransducer` doc that previously claimed Parakeet was a future streaming addition.

## What's NOT in this PR

- `streaming.rs`, `silero_vad.rs`, `vad.rs` — unchanged
- UI code (`transcript_panel.rs`, `window.rs`) — unchanged. PR 5's display-mode-row visibility logic and runtime model reload already handle Parakeet without modification because they delegate to `supports_partials()` and `SherpaModel::ALL`.
- `main.rs` — splash driver unchanged. Parakeet downloads route through PR 5's component-labeled `InitEvent::DownloadStart` flow.

## Follow-up

Filed #<CUDA_ISSUE_NUMBER> — add sherpa-onnx CUDA provider support for large offline models. Parakeet's 600M parameters are CPU-acceptable but not snappy; users with GPU horsepower (RTX 4080 Super daily driver) deserve to use it. CUDA is a separate PR because it touches Cargo.toml feature gates and the dual-build CI matrix.

## Test plan

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --all-targets --workspace -- -D warnings` (Whisper)
- [x] `cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings` (Sherpa)
- [x] `cargo test --workspace` (both flavors)
- [x] Manual (sherpa-cpu): Zipformer regression, both Moonshines regression, Parakeet clean first-run via runtime model swap, Parakeet decode on real radio chatter, runtime swap stress test across all four models
- [ ] Manual (whisper-cuda): deferred to next-day regression check — near-zero risk since all changes are `#[cfg(feature = "sherpa")]`-gated

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Return the PR URL**

Report the PR URL so the user can follow CodeRabbit review.
