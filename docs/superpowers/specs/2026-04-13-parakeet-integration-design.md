# Parakeet-TDT Integration Design

**Status:** Design approved 2026-04-13
**Tracking:** #223 (parent: #204 — sherpa-onnx integration epic)
**Branch:** `feature/parakeet-integration`

## Goal

Add NVIDIA Parakeet-TDT-0.6b-v3 as a fourth selectable `SherpaModel` variant. Parakeet is the current top-of-leaderboard ASR model on OpenASR, with 600M parameters and very high accuracy on noisy/spontaneous speech — exactly the radio-chatter use case `sdr-rs` exists for. Users with hardware budget for a 600MB bundle and a chunky decode pass get the best accuracy available; everyone else stays on Zipformer or Moonshine.

## Critical correction to PR 5's memory

Yesterday's `project_current_state.md` claimed Parakeet would slot into the existing `OnlineTransducer` streaming path because Parakeet is "RNNT family". **That was wrong.** sherpa-onnx 1.12 only supports Parakeet through the `OfflineRecognizer` API, not `OnlineRecognizer`. Confirmed by reading `~/.cargo/registry/.../sherpa-onnx-1.12.36/src/offline_asr.rs` — the upstream example `rust-api-examples/examples/nemo_parakeet.rs` uses `OfflineRecognizerConfig` with `OfflineTransducerModelConfig` and `model_type = "nemo_transducer"`.

This means Parakeet slots into the **offline** path PR 5 built (VAD-driven batch decode via `OfflineRecognizer`), not the streaming path. Same UX as Moonshine: offline → no partials → Display mode toggle hides automatically.

## Bundle target

- **Archive**: `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2` from the k2-fsa releases page (~600MB)
- **Inner directory**: `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8`
- **Files**: standard 4-file transducer layout
  - `encoder.int8.onnx`
  - `decoder.int8.onnx`
  - `joiner.int8.onnx`
  - `tokens.txt`
- **Note: v3, not v2.** Issue #223 mentions v2; current k2-fsa releases are on v3. We ship v3.

## Non-goals

- **Not** adding sherpa-onnx CUDA provider support. Parakeet on CPU at 600M params will be noticeably slower than Moonshine Base — acceptable for users who chose accuracy over speed. CUDA support is a follow-up issue (filed before merge).
- **Not** adding Parakeet v2 (the issue body's reference). v3 is current; we don't want to maintain two versions.
- **Not** generalizing `ModelKind` further than necessary. Three variants (`OnlineTransducer`, `OfflineMoonshine`, `OfflineNemoTransducer`) is the minimum for PR 6; future families add new variants as needed.
- **Not** new UI affordances. Parakeet uses the same dropdown, the same status label, the same runtime reload path that PR 5 shipped. Zero UI code changes.

## Architecture

### Data type changes

**`SherpaModel` adds one variant:**

```rust
pub enum SherpaModel {
    StreamingZipformerEn,    // existing
    MoonshineTinyEn,         // existing
    MoonshineBaseEn,         // existing
    ParakeetTdt06bV3En,      // NEW
}
```

**`ModelKind` adds one variant:**

```rust
pub enum ModelKind {
    OnlineTransducer,        // Zipformer
    OfflineMoonshine,        // Moonshine v1
    OfflineNemoTransducer,   // NEW — Parakeet (and any future NeMo transducer)
}
```

**`ModelFilePaths::Transducer` is reused** — both Zipformer (online) and Parakeet (offline) ship 4 transducer files with the same struct shape (`encoder/decoder/joiner/tokens`). The `ModelKind` discriminant tells the host which recognizer API to feed them into; the `ModelFilePaths` variant only describes the file layout. No new `ModelFilePaths` variant.

**`SherpaModel::ALL` extends to 4 entries** in dropdown order:

```rust
pub const ALL: &[Self] = &[
    Self::StreamingZipformerEn,
    Self::MoonshineTinyEn,
    Self::MoonshineBaseEn,
    Self::ParakeetTdt06bV3En,
];
```

### Method extensions

All match arms in `impl SherpaModel` get a Parakeet branch:

- `label()` → `"Parakeet TDT 0.6b v3 (English)"`
- `dir_name()` → `"parakeet-tdt-0.6b-v3-en"`
- `archive_filename()` → `"sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2"`
- `archive_inner_directory()` → `"sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"`
- `kind()` → `ModelKind::OfflineNemoTransducer`
- `supports_partials()` already delegates to `kind()`, returns `false` for `OfflineNemoTransducer`

`model_file_paths` adds:

```rust
SherpaModel::ParakeetTdt06bV3En => ModelFilePaths::Transducer {
    encoder: dir.join("encoder.int8.onnx"),
    decoder: dir.join("decoder.int8.onnx"),
    joiner: dir.join("joiner.int8.onnx"),
    tokens: dir.join("tokens.txt"),
},
```

`model_exists` is unchanged — it already pattern-matches on `ModelFilePaths::Transducer { encoder, decoder, joiner, tokens }` and checks all four exist.

### Host init branching

`backends/sherpa/host.rs::init_offline` currently hardcodes Moonshine in its Phase 3 (build OfflineRecognizer config). After PR 6 it dispatches based on `model.kind()`:

```rust
fn init_offline(model: SherpaModel, event_tx: &mpsc::Sender<InitEvent>) -> Result<RecognizerState, ()> {
    // Phase 1: Silero VAD (unchanged — both offline kinds need it)
    if !sherpa_model::silero_vad_exists() { /* download */ }

    // Phase 2: model bundle (unchanged — generic per-model)
    if !sherpa_model::model_exists(model) { /* download + extract */ }

    // Phase 3: build OfflineRecognizer config (NEW BRANCH)
    let _ = event_tx.send(InitEvent::CreatingRecognizer);
    let recognizer_config = match model.kind() {
        ModelKind::OfflineMoonshine =>
            super::offline::build_moonshine_recognizer_config(model, "cpu"),
        ModelKind::OfflineNemoTransducer =>
            super::offline::build_nemo_transducer_recognizer_config(model, "cpu"),
        ModelKind::OnlineTransducer => unreachable!("init_offline called with online model"),
    };

    let Some(recognizer) = OfflineRecognizer::create(&recognizer_config) else {
        // existing error handling
    };

    // Phase 4: Silero VAD construction (unchanged)
    let vad = SherpaSileroVad::new(&sherpa_model::silero_vad_path())?;
    Ok(RecognizerState::Offline { recognizer, vad })
}
```

### New recognizer config builder

`backends/sherpa/offline.rs` gains a sibling to `build_moonshine_recognizer_config`:

```rust
/// Build the `OfflineRecognizerConfig` for a NeMo Parakeet-TDT model.
///
/// Uses sherpa-onnx's offline transducer config with the `nemo_transducer`
/// model_type field, matching the upstream `rust-api-examples/examples/
/// nemo_parakeet.rs` pattern.
pub(super) fn build_nemo_transducer_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> OfflineRecognizerConfig {
    let ModelFilePaths::Transducer { encoder, decoder, joiner, tokens } =
        sherpa_model::model_file_paths(model)
    else {
        unreachable!("nemo_transducer config called with non-Transducer layout")
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
        // Required: tells sherpa-onnx to use NeMo's transducer
        // decode loop instead of the generic transducer path.
        // Mirrors the upstream nemo_parakeet.rs example.
        model_type: Some("nemo_transducer".to_owned()),
        ..OfflineModelConfig::default()
    };

    OfflineRecognizerConfig {
        model_config,
        ..OfflineRecognizerConfig::default()
    }
}
```

### Session loop is unchanged

`offline.rs::run_session` already takes a `&OfflineRecognizer` and feeds it audio segments via `SherpaSileroVad`. It doesn't care which decode path the recognizer wraps internally. PR 5's offline loop is now exercising its abstraction for the first time on a second backend, exactly as designed.

### What does NOT change

- `streaming.rs` — Zipformer path untouched
- `silero_vad.rs` — Silero VAD wrapper unchanged
- `vad.rs` — `VoiceActivityDetector` trait unchanged
- `main.rs` — splash driver unchanged (Parakeet's downloads route through the existing `InitEvent::DownloadStart { component }` flow with the new model's label)
- `transcript_panel.rs` — UI dropdown auto-populates from `SherpaModel::ALL`; the `display_mode_row` visibility toggle from PR 5 already calls `supports_partials()` and will hide for Parakeet automatically
- `window.rs` — runtime model reload from PR 5 already handles any new variant for free; row-lock helper from PR 5 is shared
- All download infrastructure — `download_sherpa_archive` and `extract_sherpa_archive` are model-agnostic

## UX notes

1. **Same offline UX as Moonshine**: text appears per-utterance after VAD detects end-of-speech (~100-300ms latency for Moonshine, expected to be ~500ms-1.5s for Parakeet on CPU due to model size). Display mode row hidden. Live captions toggle is meaningless (offline).

2. **First-run download is large**: 600MB Parakeet bundle + 2MB Silero VAD if not already cached. Splash shows component-labeled progress. Users on slow connections will wait.

3. **CPU latency reality check**: Moonshine Base (61M params) was already noticeable per-utterance. Parakeet at 600M is roughly 10x bigger. Real-world latency on RTX 4080 Super (CPU only — no sherpa CUDA yet) needs to be measured during smoke test. If it's unusable, ship anyway and file the CUDA follow-up; users self-select.

4. **Runtime model swap from PR 5 covers Parakeet for free**: switching to/from Parakeet works the same as switching between Zipformer and Moonshine — no restart, no special handling, the existing reload flow rebuilds the recognizer in place.

## Error handling

All error paths reuse PR 5's infrastructure:
- Bundle download failure → `InitEvent::Failed` → splash shows error → user retries by switching models
- Bundle file missing after extract → `OfflineRecognizer::create` returns None → existing error path emits `InitEvent::Failed`
- Model creation failure (e.g., wrong file format) → existing error path
- Mid-session backend death → existing `TranscriptionEvent::Error` arm in `connect_transcript_panel` handles teardown via `unlock_transcription_session_rows`

No new error variants needed.

## Testing strategy

**Unit tests added in `sherpa_model.rs`**:
- `parakeet_is_offline_nemo_transducer_kind` — `kind()` returns `OfflineNemoTransducer`
- `parakeet_does_not_support_partials` — `supports_partials()` returns `false`
- `parakeet_has_transducer_file_layout` — `model_file_paths` returns `ModelFilePaths::Transducer` with the four expected filenames
- `parakeet_archive_url_is_well_formed` — URL contains `parakeet`, ends with `.tar.bz2`, starts with the k2-fsa releases prefix
- `all_contains_four_variants` — `ALL.len() == 4` (existing test renamed/extended from `_three_`)

**Build matrix (dual-build rule)**:
- `cargo build --workspace` (Whisper CUDA default) — must compile clean
- `cargo build --workspace --no-default-features --features sherpa-cpu` — must compile clean
- `cargo clippy --all-targets --workspace -- -D warnings` (both flavors)
- `cargo test --workspace` (both flavors)
- `cargo fmt --all -- --check`

**Manual smoke test (user-driven)**:
1. **Zipformer regression** — confirm streaming captions still work exactly as PR 5 shipped
2. **Moonshine regression** — confirm both Tiny and Base still commit per-utterance
3. **Parakeet clean first-run** — clear `~/.local/share/sdr-rs/models/sherpa/parakeet-tdt-0.6b-v3-en/`, switch model in dropdown, watch splash show "Downloading Parakeet TDT 0.6b v3 (English)..." (and Silero VAD if not cached), then "Extracting...", then "Creating recognizer...", then ready
4. **Parakeet decode** — feed real radio chatter, confirm utterance commits land in the text view, accuracy is noticeably better than Moonshine on a tricky sample (call signs, numbers, etc.)
5. **Latency feel check** — note per-utterance latency on CPU, decide if the CUDA follow-up is "nice to have" or "actually blocking"
6. **Runtime swap exercise** — switch among all four models in succession, confirm reload completes for each, no crashes
7. **Whisper regression** — `make install CARGO_FLAGS="--release --features whisper-cuda"`, confirm whisper-cuda build still works (deferred to next-day check, near-zero risk since all changes are sherpa-gated)

## Follow-up issues to file before merge

**Title**: Add sherpa-onnx CUDA provider support for large offline models

**Body outline**:
- Context: PR 6 ships Parakeet-TDT-0.6b which has 600M parameters and runs on CPU only today.
- Problem: per-utterance latency on CPU is significant for a model this size. Users with CUDA-capable GPUs (like the daily-driver RTX 4080 Super) get no benefit from their hardware.
- Proposed change: add a `sherpa-cuda` cargo feature alongside `sherpa-cpu`. The sherpa-onnx 1.12 crate supports CUDA via its `cuda` feature flag — verify which provider string to pass (`"cuda"` likely) and update the hardcoded `"cpu"` provider in `init_online`/`init_offline` to be feature-driven.
- Acceptance: `make install CARGO_FLAGS="--release --no-default-features --features sherpa-cuda"` builds, sherpa-onnx uses CUDA execution provider, Parakeet decode latency drops to GPU-acceptable levels.
- Caveat: must remain mutually exclusive with whisper features (the feature-mutex pattern from #249).
- Labels: `enhancement`, `transcription`

## Risks / unknowns

1. **Sherpa-onnx CPU latency on a 600M model is unverified**. We're guessing it'll be 500ms-1.5s per utterance. Could be 5+ seconds on weaker CPUs. Mitigation: ship anyway, label clearly as "highest accuracy, slowest", file CUDA follow-up.

2. **The `model_type = "nemo_transducer"` magic string is brittle**. If sherpa-onnx ever renames it (e.g., `"nemo_transducer_v2"`) the recognizer creation silently returns None. Mitigation: tracked by the existing `OfflineRecognizer::create returned None` error message which already points users at "check Moonshine model files" — we should generalize that message in PR 6 to mention model_type misconfigurations too.

3. **Bundle filename drift**. The k2-fsa releases page sometimes renames assets between versions. We hardcode `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2`; if upstream renames it, downloads 404. Mitigation: same as the four other models we already pin.

4. **Whisper code path remains untouched** — the dual-build smoke test is the only safety net against accidental regressions. The existing CI exercises both flavors so this is fine.
