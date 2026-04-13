# Sherpa Model Auto-Download + Splash Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add automatic download + extraction of sherpa-onnx model bundles from k2-fsa GitHub releases, plus a bundled GTK splash window that shows progress during the otherwise-blocking startup phase. PR 2 left download as a manual `wget + tar -xjf` step and the user had no UI feedback during the post-download recognizer init.

**Architecture:** Two new workspace crates: `sdr-splash` (cross-platform controller, zero UI deps, exports `SplashController` that spawns a subprocess and writes to its stdin) and `sdr-splash-gtk` (Linux-only library, GTK4 + libadwaita deps, exports `pub fn run()` that opens a tiny splash window and reads commands from stdin). The main `sdr-rs` binary gets a new `--splash` argv mode that dispatches to `sdr_splash_gtk::run()`. The splash subprocess is the same binary as `sdr-rs` re-exec'd with `--splash`, discovered via `std::env::current_exe()` — no PATH lookup, no second install target. `init_sherpa_host` is restructured to return an `mpsc::Receiver<InitEvent>` so main.rs can drive splash text updates as the worker progresses through download → extract → recognizer creation. The heap-corruption workaround from PR 2 is preserved because the splash subprocess has its own address space — its GTK init can't pollute the parent's pristine state.

**Tech Stack:** Rust 2024, `reqwest` (already in workspace deps), `tar 0.4`, `bzip2 0.4`, `gtk4 0.11`, `libadwaita 0.9`, `mpsc::Receiver` for init event streaming, `std::process::Command` + `ChildStdin` for splash subprocess control.

**Spec:** No separate spec — this plan IS the spec. Behavior was sketched in conversation: Option C (block startup on download with stderr progress) PLUS bundled splash subprocess for visual feedback during the blocking phase.

**Branch:** `feature/sherpa-model-auto-download` (already created, no commits yet)

---

## Background

PR 2 (#249) shipped sherpa-onnx as a streaming transcription backend behind a mutually-exclusive cargo feature. Two pain points:

1. **First-run model setup is manual.** User has to know to run `wget` + `tar -xjf` to populate `~/.local/share/sdr-rs/models/sherpa/streaming-zipformer-en/` before the backend works. The error message tells them exactly what path to populate, but it's a friction point.

2. **No UI feedback during sherpa init.** The host worker thread blocks `main()` for 1-2 seconds (cached path) or 30+ seconds (first-run download path) while it creates the `OnlineRecognizer`. During this time the GUI hasn't loaded yet — the user sees a frozen terminal.

This PR fixes both:

1. The host worker auto-downloads + extracts the bundle on first run, with progress reported via `tracing::info!` to stderr AND via `InitEvent` messages to a channel main.rs reads.

2. main.rs spawns a tiny GTK splash window (as a subprocess of the same binary) that shows "Initializing sherpa-onnx..." / "Downloading sherpa-onnx model... 50%" / "Loading sherpa-onnx recognizer..." while the worker runs. The splash closes when the worker signals `InitEvent::Ready` and main.rs proceeds to `sdr_ui::run()`.

The heap corruption workaround from PR 2 is still load-bearing: `OnlineRecognizer::create` must happen before `sdr_ui::run()` loads GTK. The splash subprocess works around this because it's a separate OS process — its GTK init happens in a different address space and can't pollute the parent's pristine ONNX-Runtime-friendly state.

---

## File structure

**Create:**

- `crates/sdr-splash/Cargo.toml` — cross-platform controller crate, no UI deps
- `crates/sdr-splash/src/lib.rs` — `SplashController` struct + `Drop` impl
- `crates/sdr-splash-gtk/Cargo.toml` — Linux-only GTK splash window crate
- `crates/sdr-splash-gtk/src/lib.rs` — `pub fn run() -> ExitCode` that opens the GTK splash window and reads stdin

**Modify:**

- `Cargo.toml` (root) — add `crates/sdr-splash` and `crates/sdr-splash-gtk` to workspace members + workspace deps; add `sdr-splash` and `sdr-splash-gtk` (Linux-only) to root binary deps
- `crates/sdr-transcription/Cargo.toml` — add `tar` + `bzip2` deps gated on `sherpa` feature
- `crates/sdr-transcription/src/sherpa_model.rs` — add `archive_filename()`, `archive_inner_directory()`, `archive_url()` to `SherpaModel`; add `SherpaModelError`; add `download_sherpa_model()`; add `cleanup_scratch_state()` helper
- `crates/sdr-transcription/src/lib.rs` — add `InitEvent` enum (sherpa-feature-gated); change `init_sherpa_host` signature to return `mpsc::Receiver<InitEvent>`
- `crates/sdr-transcription/src/backends/sherpa.rs` — refactor `run_host_loop` to download (if needed) → extract → create recognizer, emitting `InitEvent`s through the new channel; populate `SHERPA_HOST` from the worker thread; remove the old `init_tx`/`init_rx` blocking handshake
- `src/main.rs` — top-level dispatch on `--splash` argv → `sdr_splash_gtk::run()`; existing main path drives the splash from `init_sherpa_host`'s event channel
- `docs/superpowers/plans/2026-04-12-transcription-sherpa-spike.md` — annotate manual download instructions as superseded

**Untouched (regression surface):**

- `crates/sdr-ui/*` — no UI changes (the splash is separate from the main GTK app)
- `crates/sdr-transcription/src/backend.rs` — no trait or error type changes
- `crates/sdr-transcription/src/util.rs` — unchanged
- All whisper code paths

---

## Conventions for this PR

- **Splash subprocess discovery via `current_exe()`.** `SplashController::try_spawn` calls `std::env::current_exe()` to find the path of the running binary, then spawns it with `--splash` as the first argument. No PATH lookup, no risk of finding a stale binary.
- **Wire protocol on stdin.** Single-line commands:
  - `text:<message>\n` — update the splash window's label text
  - `done\n` — close the window cleanly (or close stdin which produces EOF and triggers the same path)
  - All unrecognized lines are silently ignored (forward compatibility for future commands)
- **Splash falls back to no-op if subprocess can't be spawned.** `SplashController::try_spawn` returns `Self { inner: None }` on any failure (current_exe() unavailable, fork failed, etc.). All methods are no-ops in that state, so `main.rs` doesn't need conditional logic.
- **InitEvent channel is the single source of truth.** main.rs drives both the splash AND the SHERPA_HOST OnceLock population from a single event stream. The worker thread is the producer.
- **Skip the splash on the cached path.** If `sherpa_model::model_exists(model)` returns true, recognizer creation takes ~1-2 seconds and isn't worth a splash window. main.rs only spawns the splash when the model is missing OR a future preload reason exists.
- **Splash binary lives in `sdr-splash-gtk` library, not as a second binary target.** The library exports `pub fn run() -> ExitCode`. `src/main.rs` dispatches to it from the `--splash` argv branch. Single install target.
- **Tests:** unit tests cover URL building, path computation, cleanup helper. Real network downloads NOT tested in CI. The splash UI is similarly NOT tested in CI (no headless GTK in our test env). Manual smoke test covers both.

---

## Sherpa model bundle reference

For the only model in this PR (Streaming Zipformer English):

- **Archive URL:** `https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2`
- **Archive filename:** `sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2`
- **Archive size:** ~256 MB compressed, expands to ~530 MB
- **Top-level directory inside the tarball:** `sherpa-onnx-streaming-zipformer-en-2023-06-26/`
- **Files inside that directory:**
  - `encoder-epoch-99-avg-1-chunk-16-left-128.onnx` (~110 MB)
  - `decoder-epoch-99-avg-1-chunk-16-left-128.onnx` (~3 MB)
  - `joiner-epoch-99-avg-1-chunk-16-left-128.onnx` (~13 MB)
  - `tokens.txt`
  - Several other files (int8 quantized variants, README, test wavs) — extracted but unused
- **Target final directory:** `~/.local/share/sdr-rs/models/sherpa/streaming-zipformer-en/` (renamed from the tarball's top-level directory)

---

## Phase 1: Auto-download (Tasks 1-4)

### Task 1: Add `tar` and `bzip2` deps to `sdr-transcription`

**Files:**
- Modify: `crates/sdr-transcription/Cargo.toml`
- Auto-modified: `Cargo.lock`

- [ ] **Step 1: Add the deps**

In `crates/sdr-transcription/Cargo.toml`, find the existing `[dependencies]` block:

```toml
[dependencies]
whisper-rs = { workspace = true, optional = true }
sherpa-onnx = { workspace = true, optional = true }
reqwest.workspace = true
rustfft.workspace = true
sdr-types.workspace = true
thiserror.workspace = true
tracing.workspace = true
dirs-next.workspace = true
libc.workspace = true
```

Add two new optional deps:

```toml
[dependencies]
whisper-rs = { workspace = true, optional = true }
sherpa-onnx = { workspace = true, optional = true }
reqwest.workspace = true
rustfft.workspace = true
sdr-types.workspace = true
thiserror.workspace = true
tracing.workspace = true
dirs-next.workspace = true
libc.workspace = true
tar = { version = "0.4", optional = true }
bzip2 = { version = "0.4", optional = true }
```

Then update the `sherpa` internal feature in the same file to pull them in. Find:

```toml
sherpa = ["dep:sherpa-onnx"]
```

Replace with:

```toml
sherpa = ["dep:sherpa-onnx", "dep:tar", "dep:bzip2"]
```

- [ ] **Step 2: Verify both build configurations**

```bash
cd /data/source/rtl-sdr
cargo build --workspace 2>&1 | tail -5
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -5
```

Both should be clean. Whisper builds shouldn't pull in tar/bzip2.

```bash
cargo tree --workspace 2>&1 | grep -E "(^| )(tar|bzip2) v" | head -5
cargo tree --workspace --no-default-features --features sherpa-cpu 2>&1 | grep -E "(^| )(tar|bzip2) v" | head -5
```

First command should produce NO matches (no tar/bzip2 in whisper build). Second should show both as deps of `sdr-transcription`.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-transcription/Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
sdr-transcription: add tar + bzip2 deps for sherpa model auto-download

Both gated on the `sherpa` feature so whisper-only builds don't link
them. bzip2 0.4 statically links libbz2 by default — no system dep.

Used by the upcoming download_sherpa_model() function in the next
commit.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Add archive URL helpers to `SherpaModel`

**Files:**
- Modify: `crates/sdr-transcription/src/sherpa_model.rs`

- [ ] **Step 1: Add three new methods**

In `crates/sdr-transcription/src/sherpa_model.rs`, find the `impl SherpaModel` block. After the existing `tokens_filename()` method and BEFORE the `pub const ALL` line, add:

```rust
    /// Filename of the upstream `.tar.bz2` archive on the k2-fsa GitHub
    /// releases page. Used by `download_sherpa_model` to construct the
    /// download URL and to name the local `.part` file during fetch.
    pub fn archive_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => {
                "sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2"
            }
        }
    }

    /// Name of the top-level directory inside the extracted archive.
    /// Sherpa archives unpack to a directory named like
    /// `sherpa-onnx-streaming-zipformer-en-2023-06-26/`. After extraction
    /// we rename it to `dir_name()` so the path layout matches what
    /// `model_directory()` expects.
    pub fn archive_inner_directory(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "sherpa-onnx-streaming-zipformer-en-2023-06-26",
        }
    }

    /// Full HTTPS URL to the upstream `.tar.bz2` archive on the k2-fsa
    /// GitHub releases page.
    pub fn archive_url(self) -> String {
        format!(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/{}",
            self.archive_filename()
        )
    }
```

- [ ] **Step 2: Add unit tests**

In the existing `#[cfg(test)] mod tests` block, add two new tests after the existing `model_file_paths_returns_four_distinct_files`:

```rust
    #[test]
    fn streaming_zipformer_archive_url_is_well_formed() {
        let url = SherpaModel::StreamingZipformerEn.archive_url();
        assert!(url.starts_with("https://github.com/k2-fsa/sherpa-onnx/"));
        assert!(url.ends_with(".tar.bz2"));
        assert!(url.contains("streaming-zipformer-en"));
    }

    #[test]
    fn streaming_zipformer_archive_inner_dir_matches_filename_stem() {
        let model = SherpaModel::StreamingZipformerEn;
        let archive = model.archive_filename();
        let inner = model.archive_inner_directory();
        // Inner directory name should equal the archive filename minus
        // the .tar.bz2 suffix — sanity check that we'll find the right
        // directory after extraction.
        assert_eq!(format!("{inner}.tar.bz2"), archive);
    }
```

- [ ] **Step 3: Run the tests**

```bash
cd /data/source/rtl-sdr
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu sherpa_model 2>&1 | tail -15
```

Expected: 5 tests pass (3 original + 2 new) under `sherpa_model::tests::*`.

```bash
cargo build --workspace 2>&1 | tail -5
```

Expected: clean. Whisper builds don't compile sherpa_model.rs at all.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-transcription/src/sherpa_model.rs
git commit -m "$(cat <<'EOF'
sdr-transcription: add archive URL helpers to SherpaModel

archive_filename(), archive_inner_directory(), and archive_url() build
the upstream k2-fsa GitHub release URL and document the archive's
internal directory layout. Used by the download function in the next
commit.

Two unit tests verify the URL is well-formed and the inner directory
name matches the archive stem (so the post-extract rename will find
the right path).

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Add `SherpaModelError` and `download_sherpa_model()`

**Files:**
- Modify: `crates/sdr-transcription/src/sherpa_model.rs`

- [ ] **Step 1: Add new imports at the top**

In `crates/sdr-transcription/src/sherpa_model.rs`, find the existing `use std::path::PathBuf;` line. Replace with:

```rust
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
```

- [ ] **Step 2: Add the error type**

After the `models_dir()` function (around line 22) and BEFORE the `pub enum SherpaModel` block, add:

```rust
/// Errors from sherpa-onnx model download and extraction.
///
/// Mirrors `crate::model::ModelError` from the Whisper side; we don't
/// share that type because the `model` module is `#[cfg(feature = "whisper")]`
/// gated and `sherpa_model` lives behind `#[cfg(feature = "sherpa")]`.
#[derive(Debug, thiserror::Error)]
pub enum SherpaModelError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("archive extraction failed: {0}")]
    Extract(String),
}
```

- [ ] **Step 3: Add the cleanup helper**

After the error type, before `pub enum SherpaModel`, add:

```rust
/// Remove any leftover scratch files/directories from a previous failed
/// download attempt for `model`. Returns Ok if no scratch existed or
/// cleanup succeeded; Err only if removal failed (e.g. permission denied).
///
/// Idempotent — safe to call when the model has never been downloaded.
fn cleanup_scratch_state(model: SherpaModel) -> Result<(), SherpaModelError> {
    let dir = sherpa_models_dir();
    let archive_part_path = dir.join(format!("{}.part", model.archive_filename()));
    let temp_extract_dir = dir.join(format!("{}.partdir", model.dir_name()));

    if archive_part_path.exists() {
        std::fs::remove_file(&archive_part_path)?;
    }
    if temp_extract_dir.exists() {
        std::fs::remove_dir_all(&temp_extract_dir)?;
    }
    Ok(())
}
```

This needs `SherpaModel` and `sherpa_models_dir` to be in scope, which they will be once added (the function is just placed early but uses items defined later — Rust doesn't care about order at module level).

- [ ] **Step 4: Add `download_sherpa_model`**

After `model_exists()` (around line 109) and BEFORE the `#[cfg(test)]` block, add:

```rust
/// Download and extract a sherpa-onnx model bundle from the k2-fsa
/// GitHub releases page.
///
/// Mirrors [`crate::model::download_model`] for Whisper, but with an
/// extra extraction step because sherpa models ship as `.tar.bz2`
/// bundles containing multiple ONNX files instead of a single GGML
/// blob.
///
/// # Arguments
///
/// * `model` — which sherpa model to download
/// * `progress_tx` — receives integer percent values (0..=100) as the
///   download streams. Only the download phase reports progress —
///   extraction is fast (~1 second on modern disks) and doesn't fire
///   any events.
///
/// # Returns
///
/// On success, the absolute path to the final extracted model directory
/// (the same path that [`model_directory`] returns).
///
/// # Behavior
///
/// 1. Cleans up any leftover `.part` archive or `.partdir` extraction
///    directory from a previous failed attempt.
/// 2. Downloads the `.tar.bz2` to `<archive_filename>.part` in
///    [`sherpa_models_dir`], streaming progress through `progress_tx`.
/// 3. Extracts the archive to `<dir_name>.partdir` (a sibling of the
///    final location).
/// 4. Renames the extracted top-level directory to the final
///    `dir_name()` location atomically.
/// 5. Cleans up the `.part` file and `.partdir` directory.
#[allow(clippy::cast_possible_truncation)]
pub fn download_sherpa_model(
    model: SherpaModel,
    progress_tx: &mpsc::Sender<u8>,
) -> Result<PathBuf, SherpaModelError> {
    let dir = sherpa_models_dir();
    std::fs::create_dir_all(&dir)?;

    let archive_filename = model.archive_filename();
    let archive_part_path = dir.join(format!("{archive_filename}.part"));
    let archive_url = model.archive_url();
    let final_dir = model_directory(model);
    let temp_extract_dir = dir.join(format!("{}.partdir", model.dir_name()));

    // Clean up any leftover state from a previous failed attempt.
    cleanup_scratch_state(model)?;

    tracing::info!(
        url = %archive_url,
        ?archive_part_path,
        "downloading sherpa-onnx model bundle"
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_mins(10))
        .build()?;

    let response = client.get(&archive_url).send()?.error_for_status()?;
    let total_size = response.content_length().unwrap_or(0);

    let mut file = std::fs::File::create(&archive_part_path)?;
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
    tracing::info!(
        bytes = downloaded,
        "sherpa-onnx archive download complete, extracting"
    );

    // Extract via tar + bzip2 into a temp directory adjacent to the
    // final location.
    std::fs::create_dir_all(&temp_extract_dir)?;
    let archive_file = std::fs::File::open(&archive_part_path)?;
    let bz_reader = bzip2::read::BzDecoder::new(archive_file);
    let mut tar_archive = tar::Archive::new(bz_reader);
    tar_archive.unpack(&temp_extract_dir).map_err(|e| {
        SherpaModelError::Extract(format!("tar/bzip2 unpack failed: {e}"))
    })?;

    // The tarball contains a single top-level directory whose name we
    // know via `archive_inner_directory()`. Move it to the final location.
    let extracted_inner = temp_extract_dir.join(model.archive_inner_directory());
    if !extracted_inner.is_dir() {
        return Err(SherpaModelError::Extract(format!(
            "expected directory {extracted_inner:?} not found inside extracted archive"
        )));
    }

    if final_dir.exists() {
        tracing::info!(?final_dir, "removing existing final directory before rename");
        std::fs::remove_dir_all(&final_dir)?;
    }
    std::fs::rename(&extracted_inner, &final_dir)?;

    // Clean up scratch state.
    std::fs::remove_dir_all(&temp_extract_dir)?;
    std::fs::remove_file(&archive_part_path)?;

    tracing::info!(?final_dir, "sherpa-onnx model installed");
    Ok(final_dir)
}
```

- [ ] **Step 5: Add a unit test for the cleanup helper**

In the `#[cfg(test)] mod tests` block, add:

```rust
    #[test]
    fn cleanup_scratch_state_is_idempotent_when_nothing_exists() {
        // Ensures the helper handles the no-leftover case without error.
        // Relies on the developer's environment being in a sane state
        // (no leftover .part files in ~/.local/share/sdr-rs/models/sherpa/).
        // If the dev has scratch lying around, the test still passes —
        // it just removes them, which is the function's job.
        let result = cleanup_scratch_state(SherpaModel::StreamingZipformerEn);
        assert!(
            result.is_ok(),
            "expected Ok on fresh/missing state, got {result:?}"
        );
    }
```

- [ ] **Step 6: Verify**

```bash
cd /data/source/rtl-sdr
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo clippy --workspace --no-default-features --features sherpa-cpu --all-targets -- -D warnings 2>&1 | tail -10
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu sherpa_model 2>&1 | tail -15
```

All clean. Test count is 6 (5 from previous + 1 new cleanup test). Whisper build also unaffected:

```bash
cargo build --workspace 2>&1 | tail -5
```

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-transcription/src/sherpa_model.rs
git commit -m "$(cat <<'EOF'
sdr-transcription: add download_sherpa_model() function

Mirrors the Whisper download_model() pattern but with an extra
extraction step because sherpa models ship as .tar.bz2 bundles.

Flow: download .tar.bz2 to <filename>.part with progress reporting
via mpsc::Sender<u8>, extract via bzip2::BzDecoder + tar::Archive
into <dir_name>.partdir, atomic-rename the inner directory to the
final streaming-zipformer-en/ location, cleanup scratch.

Cleanup is factored into cleanup_scratch_state() so it can be tested
in isolation and called at the start of every download attempt to
recover from previous failures.

New SherpaModelError type with Io / Http / Extract variants. Mirrors
crate::model::ModelError but lives separately because the model
module is whisper-feature-gated.

No caller yet — wired into SherpaHost in Task 4.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Refactor `SherpaHost` to emit `InitEvent`s

This task changes `SherpaHost` so the worker thread emits a stream of `InitEvent` messages through a new `mpsc::Receiver<InitEvent>` that `init_sherpa_host` returns. The worker also populates `SHERPA_HOST` directly instead of using the old `init_tx`/`init_rx` blocking handshake. The download phase fires events as it makes progress.

main.rs is unchanged in this task — we wire the splash up in Task 11 after the splash crates exist. For now main.rs just reads the event channel and ignores all events except `Ready`/`Failed`, which preserves the existing blocking semantics.

**Files:**
- Modify: `crates/sdr-transcription/src/lib.rs`
- Modify: `crates/sdr-transcription/src/backends/sherpa.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add the `InitEvent` type to `lib.rs`**

In `crates/sdr-transcription/src/lib.rs`, find the existing re-exports block:

```rust
pub use backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
    TranscriptionEvent,
};

#[cfg(feature = "whisper")]
pub use model::WhisperModel;

#[cfg(feature = "sherpa")]
pub use backends::sherpa::init_sherpa_host;

#[cfg(feature = "sherpa")]
pub use sherpa_model::SherpaModel;
```

Add a new `InitEvent` re-export and a sherpa-gated module declaration. After the existing `pub mod` declarations near the top, add:

```rust
#[cfg(feature = "sherpa")]
pub mod init_event;
```

Then update the re-exports:

```rust
#[cfg(feature = "sherpa")]
pub use backends::sherpa::init_sherpa_host;

#[cfg(feature = "sherpa")]
pub use init_event::InitEvent;

#[cfg(feature = "sherpa")]
pub use sherpa_model::SherpaModel;
```

- [ ] **Step 2: Create `crates/sdr-transcription/src/init_event.rs`**

Write this exact content to `crates/sdr-transcription/src/init_event.rs`:

```rust
//! Sherpa-onnx initialization progress events.
//!
//! Emitted by `init_sherpa_host` through an `mpsc::Receiver<InitEvent>`
//! so callers (currently `src/main.rs`) can render UI feedback while
//! the background worker downloads + extracts + creates the recognizer.
//!
//! The heap-corruption workaround from PR #249 means main() still has
//! to block on this channel until the worker emits Ready or Failed,
//! BEFORE proceeding to `sdr_ui::run()`. The events let main() update
//! a splash window during the wait so the user knows what's happening.

/// Progress events from the sherpa-onnx host worker thread during
/// initialization. The worker emits these in order; the final event
/// is always either `Ready` or `Failed`.
#[derive(Debug, Clone)]
pub enum InitEvent {
    /// The sherpa model bundle is missing locally; download is starting.
    DownloadStart,
    /// Download progress (0..=100). Only fired during the download phase.
    DownloadProgress { pct: u8 },
    /// Download complete; extracting the .tar.bz2 archive.
    Extracting,
    /// Extraction complete; constructing the OnlineRecognizer.
    /// This is the longest step on the cached path (~1-2 seconds).
    CreatingRecognizer,
    /// The host is fully initialized and ready to accept sessions.
    /// SHERPA_HOST has been populated with Ok(host) by the worker.
    Ready,
    /// Initialization failed permanently. SHERPA_HOST has been
    /// populated with Err(error). The error message is intended for
    /// display to the user (e.g. via a status label or toast).
    Failed { message: String },
}
```

- [ ] **Step 3: Refactor `SherpaHost::spawn` and `run_host_loop` in `sherpa.rs`**

Read `crates/sdr-transcription/src/backends/sherpa.rs` first to understand the current state. Then make these changes:

**Edit 3A — Add `use crate::init_event::InitEvent;` near the top.** Find the existing imports block and add:

```rust
use crate::init_event::InitEvent;
```

Place it alongside the other `use crate::*` lines.

**Edit 3B — Replace `SherpaHost::spawn` with the new event-emitting version.**

Find the existing `pub fn spawn(model: SherpaModel) -> Result<Self, BackendError>` function and replace its entire body with:

```rust
    /// Spawn the host worker thread and return immediately.
    ///
    /// Returns a `Receiver<InitEvent>` that streams progress events as
    /// the worker downloads (if needed) + creates the recognizer. The
    /// caller is responsible for draining the receiver until it sees
    /// `InitEvent::Ready` or `InitEvent::Failed` — the worker populates
    /// the global `SHERPA_HOST` `OnceLock` itself before emitting the
    /// final event.
    ///
    /// The signature is intentionally non-`Result` because failures
    /// surface through the event channel as `InitEvent::Failed`. This
    /// keeps the synchronous path (no immediate Result) consistent
    /// with the async event-driven model.
    pub fn spawn(model: SherpaModel) -> std::sync::mpsc::Receiver<InitEvent> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<HostCommand>();
        let (event_tx, event_rx) = mpsc::channel::<InitEvent>();

        std::thread::Builder::new()
            .name("sherpa-host".into())
            .spawn(move || {
                run_host_loop(model, cmd_rx, cmd_tx, event_tx);
            })
            .expect("failed to spawn sherpa-host worker thread");

        event_rx
    }
```

Note the new signature: takes `model`, returns `Receiver<InitEvent>`. The previous return type `Result<Self, BackendError>` is gone — failures are now in the event stream.

**Edit 3C — Update `run_host_loop` signature and body.**

Find the existing `fn run_host_loop` and replace its entire signature + body with:

```rust
/// Worker thread entry point. Owns the recognizer for the entire
/// process lifetime and handles both initialization and command
/// processing.
///
/// Phase 1: download the model bundle if it's missing locally
/// Phase 2: create the OnlineRecognizer
/// Phase 3: store the SherpaHost in SHERPA_HOST and emit Ready
/// Phase 4: process StartSession commands forever
///
/// Failures during phases 1 or 2 store an error in SHERPA_HOST and
/// emit InitEvent::Failed before returning early.
fn run_host_loop(
    model: SherpaModel,
    cmd_rx: mpsc::Receiver<HostCommand>,
    cmd_tx: mpsc::Sender<HostCommand>,
    event_tx: mpsc::Sender<InitEvent>,
) {
    // --- Phase 1: download if needed ---
    if !sherpa_model::model_exists(model) {
        tracing::info!(
            ?model,
            "sherpa model not found locally, downloading bundle (~256 MB)"
        );
        let _ = event_tx.send(InitEvent::DownloadStart);

        let (dl_tx, dl_rx) = mpsc::channel::<u8>();
        let event_tx_dl = event_tx.clone();

        // Forwarder thread translates u8 progress percents into
        // InitEvent::DownloadProgress messages.
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

        let download_result = sherpa_model::download_sherpa_model(model, &dl_tx);

        // Drop the sender so the forwarder thread exits when it drains.
        drop(dl_tx);
        let _ = dl_forwarder.join();

        if let Err(e) = download_result {
            let msg = format!("sherpa model download failed: {e}");
            tracing::error!(%msg);
            store_init_failure(BackendError::Init(msg.clone()));
            let _ = event_tx.send(InitEvent::Failed { message: msg });
            return;
        }

        tracing::info!("sherpa model installed, proceeding to recognizer init");
        // Note: download_sherpa_model emits the Extracting phase
        // implicitly (extraction happens inside the function before
        // it returns). We fire the explicit event here so the splash
        // can update its label, even though by the time it sees this
        // the extraction is already done.
        let _ = event_tx.send(InitEvent::Extracting);
    }

    // --- Phase 2: create the recognizer ---
    let _ = event_tx.send(InitEvent::CreatingRecognizer);
    let recognizer_config = build_recognizer_config(model, "cpu");
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
        // Someone else already set it — shouldn't happen because the
        // worker is the only writer, but be defensive.
        tracing::error!("sherpa host onceLock was already set");
    }
    tracing::info!("sherpa-host ready, signaling Ready event");
    let _ = event_tx.send(InitEvent::Ready);
    drop(event_tx);

    // --- Phase 4: command loop ---
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            HostCommand::StartSession(params) => {
                tracing::info!("sherpa-host: starting session");
                run_session(&recognizer, params);
                tracing::info!("sherpa-host: session ended");
            }
        }
    }
    tracing::info!("sherpa-host worker exiting");
}

/// Helper to store an initialization failure in the global OnceLock.
/// The error gets wrapped in `Arc` to satisfy the `OnceLock<Result<...,
/// Arc<BackendError>>>` type.
fn store_init_failure(err: BackendError) {
    let _ = SHERPA_HOST.set(Err(std::sync::Arc::new(err)));
}
```

**Edit 3D — Update `init_sherpa_host` in lib.rs to use the new signature.**

In `crates/sdr-transcription/src/lib.rs`, find the existing `init_sherpa_host` function (it's a re-export from `backends::sherpa`). The change is in `backends::sherpa` itself — find the existing `pub fn init_sherpa_host` and replace with:

```rust
/// Spawn the global sherpa-onnx host thread and return a channel that
/// streams initialization progress events.
///
/// **MUST be called from `main()` BEFORE GTK is initialized** (before
/// `sdr_ui::run()`). The returned `Receiver<InitEvent>` MUST be drained
/// by the caller until it produces either `InitEvent::Ready` or
/// `InitEvent::Failed` — the worker populates the global `SHERPA_HOST`
/// `OnceLock` itself before emitting the final event, but main() needs
/// to block until that's done so the recognizer creation completes
/// before GTK loads.
///
/// Idempotent — safe to call multiple times; the worker checks
/// `SHERPA_HOST.get()` and exits early if already populated. The
/// returned channel will produce a single `Failed` event if the host
/// was already initialized.
///
/// The previous synchronous variant returned a `Result<(), String>`;
/// the event channel replaces that. Failures route through `InitEvent::Failed`
/// AND through `SHERPA_HOST.get() -> Some(Err(_))`, so the existing
/// `SherpaBackend::start` error path is unchanged.
pub fn init_sherpa_host(model: SherpaModel) -> std::sync::mpsc::Receiver<InitEvent> {
    SherpaHost::spawn(model)
}
```

(This wraps `SherpaHost::spawn` so the public API name stays the same as PR 2 even though the signature changed.)

**Edit 3E — Remove the old `init_tx`/`init_rx` plumbing.**

Search the file for `init_tx` and `init_rx` references. The PR 2 code had a `mpsc::sync_channel::<Result<(), String>>(1)` handshake that no longer exists in the new design. Remove all references — the new code uses the `event_tx` channel for the same purpose.

The old `HOST_INIT_TIMEOUT` constant is no longer used (the worker no longer has a timeout — main.rs drives the loop). Remove it.

**Edit 3F — Update `src/main.rs` to drain the event channel.**

In `src/main.rs`, find:

```rust
    // Initialize the sherpa-onnx host BEFORE GTK is loaded.
    // Only present in builds with the `sherpa` feature.
    #[cfg(feature = "sherpa")]
    sdr_transcription::init_sherpa_host(sdr_transcription::SherpaModel::StreamingZipformerEn);

    sdr_ui::run()
```

Replace with:

```rust
    // Initialize the sherpa-onnx host BEFORE GTK is loaded.
    // Drain the event channel until we see Ready or Failed (or the
    // channel disconnects, which means the worker died unexpectedly).
    // The splash window from sdr-splash will be wired into this loop
    // in a later task — for now we just drain quietly.
    #[cfg(feature = "sherpa")]
    {
        use sdr_transcription::InitEvent;
        let event_rx = sdr_transcription::init_sherpa_host(
            sdr_transcription::SherpaModel::StreamingZipformerEn,
        );
        loop {
            match event_rx.recv() {
                Ok(InitEvent::Ready) => break,
                Ok(InitEvent::Failed { message }) => {
                    tracing::warn!(%message, "sherpa init failed");
                    break;
                }
                Ok(InitEvent::DownloadStart) => {
                    tracing::info!("sherpa download starting");
                }
                Ok(InitEvent::DownloadProgress { pct }) => {
                    tracing::info!(progress_pct = pct, "sherpa download progress");
                }
                Ok(InitEvent::Extracting) => {
                    tracing::info!("sherpa extracting archive");
                }
                Ok(InitEvent::CreatingRecognizer) => {
                    tracing::info!("sherpa creating recognizer");
                }
                Err(_) => {
                    tracing::warn!("sherpa init event channel disconnected");
                    break;
                }
            }
        }
    }

    sdr_ui::run()
```

- [ ] **Step 4: Verify the build**

```bash
cd /data/source/rtl-sdr
cargo build --workspace 2>&1 | tail -10
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10
cargo clippy --workspace --all-targets --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -10
cargo test --workspace 2>&1 | grep "test result" | tail -25
cargo test --workspace --no-default-features --features sherpa-cpu 2>&1 | grep "test result" | tail -25
```

All clean. The integration test `tests/sherpa_uninitialized.rs` may fail if it depends on the old `init_sherpa_host` return type — if so, fix it to use the new `Receiver<InitEvent>` shape (just call init_sherpa_host without consuming the receiver, OR drain it and assert no events arrive because the worker is going to fail with whatever it fails with in test environments).

Actually, that integration test calls `SherpaBackend::start` directly without ever calling `init_sherpa_host`. It tests the path where the global OnceLock is empty. That path is unchanged by this refactor — `SHERPA_HOST.get()` still returns `None` and `start()` still returns `BackendError::Init("not initialized")`. The test should still pass.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-transcription/src/init_event.rs \
    crates/sdr-transcription/src/lib.rs \
    crates/sdr-transcription/src/backends/sherpa.rs \
    src/main.rs
git commit -m "$(cat <<'EOF'
sdr-transcription: refactor SherpaHost to emit InitEvents

The SherpaHost worker thread now downloads the model bundle if it's
missing locally, then proceeds to recognizer creation as before.
Progress is reported via a new InitEvent enum streamed through an
mpsc::Receiver that init_sherpa_host returns.

main.rs drains the receiver until it sees Ready or Failed, blocking
exactly as long as the old init_tx/init_rx handshake did. The splash
window will hook into this same loop in a later task.

The worker also populates SHERPA_HOST itself (instead of returning the
host through init_rx) so the existing SherpaBackend::start error path
sees the same OnceLock state regardless of whether init succeeded
synchronously (cached path) or after a download.

Failures during download or recognizer creation populate SHERPA_HOST
with Err(...) and emit InitEvent::Failed. The user-facing error is
unchanged: SherpaBackend::start reads SHERPA_HOST and surfaces the
error in status_label via the existing PR 2 path.

No splash UI yet — that lands after the sdr-splash and sdr-splash-gtk
crates exist (Tasks 5-11).

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 2: `sdr-splash` controller crate (Tasks 5-7)

### Task 5: Create the `sdr-splash` crate skeleton

**Files:**
- Create: `crates/sdr-splash/Cargo.toml`
- Create: `crates/sdr-splash/src/lib.rs`

- [ ] **Step 1: Create the directory and Cargo.toml**

Run:

```bash
cd /data/source/rtl-sdr
mkdir -p crates/sdr-splash/src
```

Write this exact content to `crates/sdr-splash/Cargo.toml`:

```toml
[package]
name = "sdr-splash"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Cross-platform controller for the sdr-rs splash subprocess"

[dependencies]
tracing.workspace = true

[lints]
workspace = true
```

Note: ZERO UI deps. This crate is just a process spawner + stdin pipe writer. Cross-platform.

- [ ] **Step 2: Create the lib.rs skeleton**

Write this exact content to `crates/sdr-splash/src/lib.rs`:

```rust
//! Cross-platform controller for the sdr-rs splash subprocess.
//!
//! The splash itself is implemented in `sdr-splash-gtk` (Linux) and
//! invoked by re-exec'ing the main `sdr-rs` binary with a `--splash`
//! argv. This crate just spawns that subprocess and writes
//! line-oriented commands to its stdin.
//!
//! ## Wire protocol
//!
//! Single-line commands sent on stdin:
//!
//! - `text:<message>\n` — update the splash window's label text
//! - `done\n` — close the window cleanly
//!
//! All unrecognized lines are silently ignored. Closing stdin (EOF)
//! has the same effect as sending `done`.
//!
//! ## Lifetime
//!
//! `SplashController::try_spawn` returns immediately, with the
//! subprocess running in the background. The controller can be
//! updated via `update_text()`. On `Drop`, the controller closes the
//! subprocess's stdin (which the splash window observes as EOF and
//! exits cleanly) and reaps the child.
//!
//! If the subprocess can't be started for any reason — `current_exe()`
//! unavailable, fork failure, etc. — the controller silently falls
//! back to a no-op state and all methods become no-ops. Callers don't
//! need conditional logic; the splash either appears or it doesn't.

use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};

/// Controller for the sdr-rs splash subprocess.
pub struct SplashController {
    inner: Option<SplashInner>,
}

/// Internal state when the splash subprocess actually started.
struct SplashInner {
    child: Child,
    stdin: ChildStdin,
}

impl SplashController {
    /// Try to spawn the splash subprocess by re-exec'ing the current
    /// binary with `--splash` as argv[1]. Returns an empty controller
    /// (all methods no-op) on any failure.
    ///
    /// `initial_text` is sent to the splash window immediately after
    /// spawn so the user sees it right away.
    pub fn try_spawn(initial_text: &str) -> Self {
        let Ok(exe) = std::env::current_exe() else {
            tracing::warn!("SplashController: current_exe() failed; skipping splash");
            return Self { inner: None };
        };

        let child_result = Command::new(&exe)
            .arg("--splash")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        let mut child = match child_result {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "SplashController: failed to spawn splash subprocess; skipping splash"
                );
                return Self { inner: None };
            }
        };

        let Some(stdin) = child.stdin.take() else {
            tracing::warn!("SplashController: child stdin unavailable; skipping splash");
            // Still need to reap the child even though we won't use it.
            let _ = child.kill();
            let _ = child.wait();
            return Self { inner: None };
        };

        let mut inner = SplashInner { child, stdin };
        // Send the initial text. If this fails the controller stays
        // active but won't render anything until the next update.
        if let Err(e) = writeln!(inner.stdin, "text:{initial_text}") {
            tracing::warn!(error = %e, "SplashController: initial text write failed");
        }
        let _ = inner.stdin.flush();

        Self { inner: Some(inner) }
    }

    /// True if the splash subprocess is running.
    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }

    /// Update the label text on the splash window. No-op if the
    /// controller is inactive (subprocess didn't spawn).
    pub fn update_text(&mut self, text: &str) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        if writeln!(inner.stdin, "text:{text}").is_err() {
            // The subprocess died — drop our state so future calls are no-ops.
            tracing::warn!("SplashController: stdin write failed; subprocess may have died");
            self.inner = None;
            return;
        }
        let _ = inner.stdin.flush();
    }
}

impl Drop for SplashController {
    fn drop(&mut self) {
        let Some(mut inner) = self.inner.take() else {
            return;
        };
        // Closing stdin signals EOF to the splash subprocess, which
        // observes it and exits cleanly. We then wait for the child
        // to reap; if it doesn't exit within a short window we kill it.
        let _ = writeln!(inner.stdin, "done");
        let _ = inner.stdin.flush();
        drop(inner.stdin);

        // Best-effort wait. We don't want to block the main thread for
        // long during process exit, so kill if it doesn't exit promptly.
        match inner.child.try_wait() {
            Ok(Some(_)) => return,
            _ => {}
        }
        // Give it a brief window to exit on its own.
        std::thread::sleep(std::time::Duration::from_millis(150));
        match inner.child.try_wait() {
            Ok(Some(_)) => return,
            _ => {
                let _ = inner.child.kill();
                let _ = inner.child.wait();
            }
        }
    }
}
```

- [ ] **Step 3: Verify it compiles standalone**

```bash
cd /data/source/rtl-sdr
cargo check -p sdr-splash 2>&1 | tail -10
```

Will fail with "package `sdr-splash` not found" because we haven't added it to the workspace yet — that's Task 7. For now, just verify the file content is syntactically valid:

```bash
rustc --edition 2024 --crate-type lib --emit metadata -o /tmp/sdr-splash-check crates/sdr-splash/src/lib.rs 2>&1 | tail -20
```

Expected: errors about missing tracing crate (since we're compiling outside cargo), but no syntax errors. If you see errors about missing `;` or unresolved syntax, fix them.

- [ ] **Step 4: Don't commit yet**

The crate is incomplete until Task 7 wires it into the workspace. Stage it for the Task 7 commit:

```bash
git status
# Should show two new untracked files:
#   crates/sdr-splash/Cargo.toml
#   crates/sdr-splash/src/lib.rs
```

Move on to Task 6.

---

### Task 6: Add a stub unit test for `SplashController`

**Files:**
- Modify: `crates/sdr-splash/src/lib.rs`

- [ ] **Step 1: Add a `#[cfg(test)] mod tests` block at the bottom of the file**

Add this at the end of `crates/sdr-splash/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_controller_is_inactive() {
        // Construct an empty controller directly (simulating the
        // try_spawn failure path) and verify all methods are no-ops.
        let mut controller = SplashController { inner: None };
        assert!(!controller.is_active());
        // Should not panic.
        controller.update_text("hello");
        // Drop should not panic on an empty controller.
        drop(controller);
    }
}
```

This is a minimal smoke test. We can't easily test the real spawn path in CI because it requires the splash binary to exist, which would be a circular dep. Real verification happens in the manual smoke test.

- [ ] **Step 2: Don't commit yet**

Continue to Task 7.

---

### Task 7: Add `sdr-splash` to the workspace and verify it builds

**Files:**
- Modify: `Cargo.toml` (root)

- [ ] **Step 1: Add to workspace members**

In the root `Cargo.toml`, find the `[workspace]` section and the `members` array:

```toml
[workspace]
resolver = "3"
members = [
    ".",
    "crates/sdr-types",
    "crates/sdr-dsp",
    "crates/sdr-pipeline",
    "crates/sdr-rtlsdr",
    "crates/sdr-source-rtlsdr",
    "crates/sdr-source-network",
    "crates/sdr-source-file",
    "crates/sdr-sink-audio",
    "crates/sdr-sink-network",
    "crates/sdr-radio",
    "crates/sdr-config",
    "crates/sdr-radioreference",
    "crates/sdr-transcription",
    "crates/sdr-core",
    "crates/sdr-ui",
]
```

Add `crates/sdr-splash` (in alphabetical position):

```toml
[workspace]
resolver = "3"
members = [
    ".",
    "crates/sdr-config",
    "crates/sdr-core",
    "crates/sdr-dsp",
    "crates/sdr-pipeline",
    "crates/sdr-radio",
    "crates/sdr-radioreference",
    "crates/sdr-rtlsdr",
    "crates/sdr-sink-audio",
    "crates/sdr-sink-network",
    "crates/sdr-source-file",
    "crates/sdr-source-network",
    "crates/sdr-source-rtlsdr",
    "crates/sdr-splash",
    "crates/sdr-transcription",
    "crates/sdr-types",
    "crates/sdr-ui",
]
```

If the original member ordering wasn't alphabetical, just append `"crates/sdr-splash",` in any reasonable position rather than rewriting the whole list.

- [ ] **Step 2: Add to workspace deps**

Find the `[workspace.dependencies]` section and add:

```toml
sdr-splash = { path = "crates/sdr-splash" }
```

In alphabetical position alongside the other internal crate path deps.

- [ ] **Step 3: Verify the workspace builds**

```bash
cd /data/source/rtl-sdr
cargo build --workspace 2>&1 | tail -10
cargo test -p sdr-splash 2>&1 | tail -10
```

Both clean. The sdr-splash test should pass (1 test: `empty_controller_is_inactive`).

```bash
cargo clippy -p sdr-splash --all-targets -- -D warnings 2>&1 | tail -10
```

Clean.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-splash/Cargo.toml \
    crates/sdr-splash/src/lib.rs \
    Cargo.toml \
    Cargo.lock
git commit -m "$(cat <<'EOF'
sdr-splash: cross-platform controller crate for splash subprocess

New workspace crate that owns the SplashController struct. The
controller spawns the splash subprocess by re-exec'ing the current
binary with `--splash` as argv[1], then writes line-oriented commands
to its stdin (`text:<msg>` to update the label, `done` or stdin close
to exit).

Zero UI deps. The actual splash window implementation lives in
sdr-splash-gtk (next commit). This crate is the glue that lets any
caller (currently src/main.rs) drive the splash without depending on
GTK directly.

Falls back gracefully if subprocess spawn fails (current_exe()
unavailable, fork failed, etc.) — all methods become silent no-ops
and the parent process continues without a splash.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 3: `sdr-splash-gtk` window crate (Tasks 8-10)

### Task 8: Create the `sdr-splash-gtk` crate skeleton

**Files:**
- Create: `crates/sdr-splash-gtk/Cargo.toml`
- Create: `crates/sdr-splash-gtk/src/lib.rs`

- [ ] **Step 1: Create the directory and Cargo.toml**

```bash
cd /data/source/rtl-sdr
mkdir -p crates/sdr-splash-gtk/src
```

Write this exact content to `crates/sdr-splash-gtk/Cargo.toml`:

```toml
[package]
name = "sdr-splash-gtk"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "GTK4 + libadwaita splash window for sdr-rs (Linux)"

[target.'cfg(target_os = "linux")'.dependencies]
gtk4 = { workspace = true }
libadwaita = { workspace = true }

[dependencies]
tracing.workspace = true

[lints]
workspace = true
```

Note: `gtk4` and `libadwaita` are gated to Linux via `[target.'cfg(target_os = "linux")'.dependencies]`. On other platforms, `pub fn run()` falls through to a stub. The crate compiles everywhere but only does anything on Linux.

- [ ] **Step 2: Write the lib.rs**

Write this exact content to `crates/sdr-splash-gtk/src/lib.rs`:

```rust
//! GTK4 + libadwaita splash window for sdr-rs.
//!
//! Linux-only. The cross-platform controller is in the `sdr-splash`
//! crate; this crate is the implementation that opens the actual
//! window. The controller spawns this binary as a subprocess by
//! re-exec'ing the main `sdr-rs` binary with `--splash`; that argv
//! mode dispatches to [`run`] in this crate.
//!
//! ## Wire protocol
//!
//! Reads single-line commands from stdin:
//!
//! - `text:<message>\n` — update the centered label
//! - `done\n` — close the window and exit cleanly
//!
//! All unrecognized lines are silently ignored. EOF on stdin closes
//! the window the same way `done` does.

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::cell::RefCell;
    use std::io::BufRead;
    use std::rc::Rc;

    use gtk4::glib;
    use gtk4::prelude::*;
    use libadwaita::prelude::*;

    /// Run the GTK splash event loop. Returns when the user closes
    /// the window or stdin EOFs.
    pub fn run() -> glib::ExitCode {
        let app = libadwaita::Application::builder()
            .application_id("com.sdr.rs.splash")
            .build();

        // Hold the label widget in a refcell so the stdin reader thread
        // can update it (via the glib main context). The connect_activate
        // closure shares ownership with the activate handler.
        let label_cell: Rc<RefCell<Option<gtk4::Label>>> = Rc::new(RefCell::new(None));

        let label_cell_for_activate = Rc::clone(&label_cell);
        app.connect_activate(move |app| {
            build_window(app, &label_cell_for_activate);
        });

        // Spawn the stdin reader thread. It uses glib::idle_add_local
        // to dispatch updates back to the main thread (where the GTK
        // widgets live and can be touched safely).
        let label_cell_for_reader = Rc::clone(&label_cell);
        glib::MainContext::default().spawn_local(async move {
            let _ = label_cell_for_reader; // keep the Rc alive
        });

        spawn_stdin_reader(label_cell);

        app.run()
    }

    fn build_window(
        app: &libadwaita::Application,
        label_cell: &Rc<RefCell<Option<gtk4::Label>>>,
    ) {
        let label = gtk4::Label::builder()
            .label("Initializing...")
            .wrap(true)
            .justify(gtk4::Justification::Center)
            .css_classes(["title-3"])
            .build();

        let spinner = gtk4::Spinner::builder()
            .spinning(true)
            .width_request(48)
            .height_request(48)
            .build();

        let vbox = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(16)
            .margin_top(32)
            .margin_bottom(32)
            .margin_start(32)
            .margin_end(32)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        vbox.append(&spinner);
        vbox.append(&label);

        let window = libadwaita::ApplicationWindow::builder()
            .application(app)
            .title("SDR-RS")
            .default_width(420)
            .default_height(180)
            .resizable(false)
            .content(&vbox)
            .build();

        // Stash the label in the cell so the stdin reader can update it.
        *label_cell.borrow_mut() = Some(label);

        window.present();
    }

    /// Spawn a background thread that reads stdin lines and dispatches
    /// `text:` updates to the GTK main thread via `glib::idle_add_local_once`.
    /// On `done` or EOF, calls `app.quit()` from the main thread.
    fn spawn_stdin_reader(label_cell: Rc<RefCell<Option<gtk4::Label>>>) {
        // We use a glib MainContext channel to send commands from the
        // stdin thread back to the main thread. The channel handle is
        // Send (the receiver attaches to the main context).
        use glib::MainContext;

        // glib's modern API uses async-channel; for simplicity here we
        // use std::sync::mpsc and poll it from a glib idle callback.
        let (tx, rx) = std::sync::mpsc::channel::<StdinCommand>();

        std::thread::Builder::new()
            .name("sdr-splash-stdin".into())
            .spawn(move || {
                let stdin = std::io::stdin();
                let reader = stdin.lock();
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line == "done" {
                        let _ = tx.send(StdinCommand::Done);
                        break;
                    }
                    if let Some(text) = line.strip_prefix("text:") {
                        if tx.send(StdinCommand::SetText(text.to_owned())).is_err() {
                            break;
                        }
                    }
                    // Unrecognized lines silently ignored.
                }
                // EOF on stdin → tell main thread to quit.
                let _ = tx.send(StdinCommand::Done);
            })
            .expect("failed to spawn sdr-splash-stdin reader thread");

        // Poll the channel from a glib idle source on the main thread.
        let main_context = MainContext::default();
        main_context.spawn_local(async move {
            loop {
                match rx.try_recv() {
                    Ok(StdinCommand::SetText(text)) => {
                        if let Some(label) = label_cell.borrow().as_ref() {
                            label.set_text(&text);
                        }
                    }
                    Ok(StdinCommand::Done) => {
                        if let Some(app) = libadwaita::Application::default() {
                            app.quit();
                        }
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        glib::timeout_future(std::time::Duration::from_millis(50)).await;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        if let Some(app) = libadwaita::Application::default() {
                            app.quit();
                        }
                        break;
                    }
                }
            }
        });
    }

    enum StdinCommand {
        SetText(String),
        Done,
    }
}

/// Run the splash window. On Linux, opens a tiny GTK4 + libadwaita
/// window with a spinner and a label that updates in response to
/// commands read from stdin. On other platforms, prints an error and
/// exits non-zero.
#[cfg(target_os = "linux")]
pub fn run() -> i32 {
    let exit_code = linux_impl::run();
    // glib::ExitCode is a newtype wrapper around c_int. Extract the value.
    let value: i32 = exit_code.value();
    value
}

#[cfg(not(target_os = "linux"))]
pub fn run() -> i32 {
    eprintln!("sdr-splash-gtk: GTK splash window is currently Linux-only");
    1
}
```

**Note on the stdin reader / glib integration:** the code above uses `glib::MainContext::spawn_local` with an async loop that polls the mpsc channel via `try_recv` + `glib::timeout_future` for sleeps. This is a workable pattern but may need adjustment depending on the glib-rs version. If the implementer hits compilation issues, alternatives:

- **Option A (preferred):** use `async-channel` (a workspace dep already used elsewhere?) for a fully async send/recv on the glib main context. Replace the std mpsc with async-channel, write a simple `while let Some(cmd) = rx.recv().await` loop.

- **Option B:** use `glib::source::idle_add_local` (deprecated but still works) to poll the channel periodically. Less idiomatic but simpler.

The implementer should pick whichever compiles cleanly with the workspace's gtk4-rs version. The protocol on stdin is what matters; the internal dispatch mechanism is an implementation detail.

- [ ] **Step 3: Don't commit yet**

Continue to Task 9.

---

### Task 9: Add `sdr-splash-gtk` to the workspace

**Files:**
- Modify: `Cargo.toml` (root)

- [ ] **Step 1: Add to workspace members**

In the root `Cargo.toml`'s `members` array, add `crates/sdr-splash-gtk` next to `crates/sdr-splash`:

```toml
    "crates/sdr-splash",
    "crates/sdr-splash-gtk",
```

- [ ] **Step 2: Add to workspace deps**

In `[workspace.dependencies]`, add:

```toml
sdr-splash-gtk = { path = "crates/sdr-splash-gtk" }
```

- [ ] **Step 3: Verify the crate builds**

```bash
cd /data/source/rtl-sdr
cargo build -p sdr-splash-gtk 2>&1 | tail -10
```

Expected: clean. If you hit errors in the glib integration code (Step 2 of Task 8 noted this), now is the time to fix them. Try Option A (async-channel) first, then Option B (idle_add_local) if that doesn't work.

```bash
cargo clippy -p sdr-splash-gtk --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean. Common lints to expect:
- Unused imports if the implementation simplified
- Missing `#[must_use]` on builder methods — add as needed

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-splash-gtk/Cargo.toml \
    crates/sdr-splash-gtk/src/lib.rs \
    Cargo.toml \
    Cargo.lock
git commit -m "$(cat <<'EOF'
sdr-splash-gtk: GTK4 splash window library

Tiny GTK4 + libadwaita splash window with a centered spinner and
updatable label. Reads single-line commands from stdin (text:<msg>,
done) and updates the label or closes accordingly.

Linux-only (gtk4 + libadwaita are target-conditional deps). On other
platforms `pub fn run()` falls through to a stub that prints an error
and returns non-zero.

The actual `sdr-rs --splash` argv dispatch lives in src/main.rs (next
commit). This crate just exports `pub fn run()`.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 10: Wire `--splash` argv mode into `src/main.rs`

**Files:**
- Modify: `Cargo.toml` (root) — add `sdr-splash-gtk` as a Linux-conditional dep
- Modify: `src/main.rs` — dispatch on `--splash` argv at the top of main

- [ ] **Step 1: Add `sdr-splash-gtk` as a target-conditional dep in the root binary**

In root `Cargo.toml`, find the existing `[target.'cfg(target_os = "linux")'.dependencies]` section:

```toml
[target.'cfg(target_os = "linux")'.dependencies]
sdr-ui = { workspace = true, features = ["pipewire"], optional = true }
gtk4.workspace = true
libadwaita.workspace = true
```

Add `sdr-splash-gtk`:

```toml
[target.'cfg(target_os = "linux")'.dependencies]
sdr-ui = { workspace = true, features = ["pipewire"], optional = true }
sdr-splash-gtk.workspace = true
gtk4.workspace = true
libadwaita.workspace = true
```

`sdr-splash-gtk` is NOT optional and NOT feature-gated — it's a small Linux-only crate that's always linked on Linux. On non-Linux it's not in the dep tree at all.

- [ ] **Step 2: Add `--splash` dispatch at the top of main.rs**

In `src/main.rs`, find the start of the Linux `fn main`:

```rust
#[cfg(all(target_os = "linux", feature = "gtk-frontend"))]
fn main() -> glib::ExitCode {
    // Limit glibc malloc arenas before any threads spawn.
```

Insert a splash dispatch BEFORE the mallopt call. The full updated function should be:

```rust
#[cfg(all(target_os = "linux", feature = "gtk-frontend"))]
fn main() -> glib::ExitCode {
    // Splash subprocess mode. The sdr-splash controller re-execs us
    // with `--splash` as argv[1] to render a tiny GTK splash window
    // during the otherwise-blocking sherpa init phase. Dispatch BEFORE
    // any mallopt or sherpa init — this is a separate process that
    // does its own GTK setup.
    if std::env::args().nth(1).as_deref() == Some("--splash") {
        let exit_code: i32 = sdr_splash_gtk::run();
        return glib::ExitCode::from(u8::try_from(exit_code).unwrap_or(1));
    }

    // Limit glibc malloc arenas before any threads spawn.
    // Without this, glibc creates up to 8*cores arenas that each keep
    // their high-water mark, causing RSS to grow indefinitely with 40+ threads.
    // Uses mallopt() instead of env var — glibc reads MALLOC_ARENA_MAX
    // at allocator init (before main), so set_var is too late.
    #[cfg(target_env = "gnu")]
    #[allow(unsafe_code)]
    let arena_ok = unsafe {
        unsafe extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        const M_ARENA_MAX: i32 = -8;
        mallopt(M_ARENA_MAX, 4) != 0
    };

    tracing_subscriber::fmt::init();
    #[cfg(target_env = "gnu")]
    if !arena_ok {
        tracing::warn!("mallopt(M_ARENA_MAX, 4) failed — arena cap not applied");
    }
    tracing::info!("sdr-rs starting");

    // Initialize the sherpa-onnx host BEFORE GTK is loaded. The splash
    // wiring lands in the next task — for now we just drain the event
    // channel synchronously until Ready or Failed.
    #[cfg(feature = "sherpa")]
    {
        use sdr_transcription::InitEvent;
        let event_rx = sdr_transcription::init_sherpa_host(
            sdr_transcription::SherpaModel::StreamingZipformerEn,
        );
        loop {
            match event_rx.recv() {
                Ok(InitEvent::Ready) => break,
                Ok(InitEvent::Failed { message }) => {
                    tracing::warn!(%message, "sherpa init failed");
                    break;
                }
                Ok(InitEvent::DownloadStart) => {
                    tracing::info!("sherpa download starting");
                }
                Ok(InitEvent::DownloadProgress { pct }) => {
                    tracing::info!(progress_pct = pct, "sherpa download progress");
                }
                Ok(InitEvent::Extracting) => {
                    tracing::info!("sherpa extracting archive");
                }
                Ok(InitEvent::CreatingRecognizer) => {
                    tracing::info!("sherpa creating recognizer");
                }
                Err(_) => {
                    tracing::warn!("sherpa init event channel disconnected");
                    break;
                }
            }
        }
    }

    sdr_ui::run()
}
```

(The Task 4 sherpa-init loop is preserved unchanged. Task 11 wraps it with the splash.)

- [ ] **Step 3: Verify**

```bash
cd /data/source/rtl-sdr
cargo build --workspace 2>&1 | tail -10
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10
```

Both clean. Test the splash dispatch directly by running the binary with `--splash`:

```bash
echo "text:Test splash from CLI" | cargo run --release --features whisper-cuda -- --splash
```

Expected: a tiny GTK window appears with "Test splash from CLI" as the label, with a spinner. EOF on stdin (when `echo` exits) closes the window. You should see the window briefly then it disappears.

If the window doesn't appear, debug the GTK integration in `sdr-splash-gtk/src/lib.rs`.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs
git commit -m "$(cat <<'EOF'
sdr-rs: dispatch --splash argv mode to sdr_splash_gtk::run

Adds sdr-splash-gtk as a target-conditional dep (Linux-only) and a
top-of-main argv check that re-routes `sdr-rs --splash` to the splash
window implementation. The splash subprocess is the same binary as
sdr-rs re-exec'd by SplashController; the --splash dispatch runs
before any mallopt or sherpa init code so it's a clean separate
process that does its own GTK setup without polluting the parent's
ONNX-Runtime-friendly state.

Verified by running `echo "text:..." | sdr-rs --splash` directly —
GTK window appears, label shows the text, EOF closes it.

The actual SplashController integration with the sherpa init flow
lands in the next commit.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 4: Wire splash into sherpa init flow (Task 11)

### Task 11: SplashController in main.rs sherpa init loop

**Files:**
- Modify: `Cargo.toml` (root) — add `sdr-splash` as a normal dep
- Modify: `src/main.rs` — wrap the sherpa init loop with a SplashController

- [ ] **Step 1: Add `sdr-splash` as a regular dep**

In root `Cargo.toml`'s `[dependencies]` section, find:

```toml
[dependencies]
sdr-pipeline.workspace = true
...
```

Add `sdr-splash` (alphabetically):

```toml
[dependencies]
sdr-pipeline.workspace = true
sdr-radio.workspace = true
sdr-radioreference.workspace = true
sdr-rtlsdr.workspace = true
sdr-source-rtlsdr.workspace = true
sdr-source-network.workspace = true
sdr-source-file.workspace = true
sdr-sink-audio.workspace = true
sdr-sink-network.workspace = true
sdr-splash.workspace = true
sdr-transcription.workspace = true
tracing.workspace = true
tracing-subscriber = { workspace = true, features = ["env-filter"] }
anyhow.workspace = true
```

(Insert in whatever position matches the existing ordering — alphabetical or by category.)

`sdr-splash` is cross-platform (no UI deps) so it goes in the regular `[dependencies]` block, NOT under `[target.'cfg(target_os = "linux")']`.

- [ ] **Step 2: Wrap the sherpa init loop with `SplashController`**

In `src/main.rs`, find the existing sherpa init loop:

```rust
    #[cfg(feature = "sherpa")]
    {
        use sdr_transcription::InitEvent;
        let event_rx = sdr_transcription::init_sherpa_host(
            sdr_transcription::SherpaModel::StreamingZipformerEn,
        );
        loop {
            match event_rx.recv() {
                Ok(InitEvent::Ready) => break,
                ...
            }
        }
    }
```

Replace with:

```rust
    #[cfg(feature = "sherpa")]
    {
        use sdr_splash::SplashController;
        use sdr_transcription::InitEvent;

        // Spawn the splash subprocess BEFORE init_sherpa_host. If the
        // model is already cached, the recognizer creation takes ~1-2
        // seconds and the splash flickers briefly; if it has to
        // download (~30 seconds for the 256 MB bundle), the splash
        // shows progress for the duration. Falls back to a no-op
        // controller if the subprocess can't spawn — see
        // SplashController::try_spawn for the failure modes.
        let mut splash = SplashController::try_spawn("Initializing sherpa-onnx...");

        let event_rx = sdr_transcription::init_sherpa_host(
            sdr_transcription::SherpaModel::StreamingZipformerEn,
        );

        loop {
            match event_rx.recv() {
                Ok(InitEvent::DownloadStart) => {
                    tracing::info!("sherpa download starting");
                    splash.update_text("Downloading sherpa-onnx model...");
                }
                Ok(InitEvent::DownloadProgress { pct }) => {
                    tracing::info!(progress_pct = pct, "sherpa download progress");
                    splash.update_text(&format!(
                        "Downloading sherpa-onnx model... {pct}%"
                    ));
                }
                Ok(InitEvent::Extracting) => {
                    tracing::info!("sherpa extracting archive");
                    splash.update_text("Extracting sherpa-onnx model...");
                }
                Ok(InitEvent::CreatingRecognizer) => {
                    tracing::info!("sherpa creating recognizer");
                    splash.update_text("Loading sherpa-onnx recognizer...");
                }
                Ok(InitEvent::Ready) => {
                    tracing::info!("sherpa ready");
                    break;
                }
                Ok(InitEvent::Failed { message }) => {
                    tracing::warn!(%message, "sherpa init failed");
                    // Don't update splash text — we're about to drop it.
                    // The error will surface in status_label when the
                    // user toggles Sherpa transcription.
                    break;
                }
                Err(_) => {
                    tracing::warn!("sherpa init event channel disconnected");
                    break;
                }
            }
        }

        // Drop the splash controller — closes the subprocess.
        drop(splash);
    }

    sdr_ui::run()
```

- [ ] **Step 3: Verify**

```bash
cd /data/source/rtl-sdr
cargo build --workspace 2>&1 | tail -10
cargo build --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -10
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10
cargo clippy --workspace --all-targets --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -10
```

All clean. The splash is now wired into both the cached and download paths for sherpa builds.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs
git commit -m "$(cat <<'EOF'
sdr-rs: wire SplashController into sherpa init loop

main.rs now spawns a SplashController before init_sherpa_host and
updates its label as InitEvents stream in from the worker. The splash
shows the user what's happening during the otherwise-blocking startup
phase:

  1. "Initializing sherpa-onnx..." (initial state)
  2. "Downloading sherpa-onnx model..." → "...50%" → "...100%"
  3. "Extracting sherpa-onnx model..."
  4. "Loading sherpa-onnx recognizer..."
  5. (splash drops, GTK app launches)

On the cached path the splash flickers briefly (~1-2 seconds) while
the recognizer loads. On first-run download it shows live progress
for the ~30 second duration of the download.

If the splash subprocess can't spawn, the controller silently falls
back to no-op state and the user sees the existing stderr-only
behavior.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 5: Verification (Tasks 12-13)

### Task 12: Lint, fmt, full workspace verify

- [ ] **Step 1: Run all lints**

```bash
cd /data/source/rtl-sdr
cargo clippy --all-targets --workspace -- -D warnings 2>&1 | tail -15
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -15
cargo fmt --all 2>&1 | tail -3
cargo test --workspace 2>&1 | grep "test result" | tail -25
cargo test --workspace --no-default-features --features sherpa-cpu 2>&1 | grep "test result" | tail -25
make lint 2>&1 | tail -40
```

All clean. New deps (`tar`, `bzip2`) may trigger cargo-deny license warnings — address with the minimum config change.

Test count: should be UP from PR 2 by +3 (sherpa_model URL test, archive inner test, cleanup test) +1 (sdr-splash empty controller test) = +4 total.

- [ ] **Step 2: Commit any fmt/lint fixups**

```bash
git status
git add -u
git commit -m "$(cat <<'EOF'
fmt + clippy fixups

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

If `git status` is clean, skip.

---

### Task 13: Manual smoke test (user-driven)

**This task requires the user.** Multiple test scenarios because there are several code paths to validate.

- [ ] **Step 1: Build and install with sherpa-cpu**

```bash
cd /data/source/rtl-sdr
make install CARGO_FLAGS="--release --no-default-features --features sherpa-cpu" 2>&1 | tail -10
```

- [ ] **Step 2: Pre-test setup**

Tell the user:

> "PR 3 (sherpa auto-download + splash) is built. To test the auto-download path, you need to remove the existing model first:
>
> ```bash
> mv ~/.local/share/sdr-rs/models/sherpa/streaming-zipformer-en ~/sherpa-zipformer-backup
> ```
>
> Then run sdr-rs from a terminal so you can see both the GTK splash AND stderr."

- [ ] **Step 3: Hand off the test plan**

Tell the user:

> "Sherpa CPU build installed. Run through these tests in order:
>
> **Test A: First-run download with splash**
> 1. Launch from terminal: `sdr-rs`
> 2. Within ~1 second, a small GTK window should appear with the SDR-RS title, a spinning spinner, and the label "Initializing sherpa-onnx..."
> 3. The label should update through the phases:
>    - "Downloading sherpa-onnx model..." (briefly)
>    - "Downloading sherpa-onnx model... 1%", 2%, ... 100%
>    - "Extracting sherpa-onnx model..."
>    - "Loading sherpa-onnx recognizer..."
> 4. After ~30 seconds (depending on network) the splash window closes
> 5. The main GTK app appears
> 6. Toggle Enable Transcription, verify sherpa transcription works
>
> **Test B: Cached path (fast startup)**
> 1. Close sdr-rs cleanly
> 2. Re-launch: `sdr-rs`
> 3. Splash should appear briefly (~1-2 seconds) showing only "Initializing sherpa-onnx..." then "Loading sherpa-onnx recognizer..."
> 4. Main app appears almost immediately
> 5. No download messages — model is cached
>
> **Test C: Download failure path**
> 1. Disconnect your network briefly OR move the model dir aside again
> 2. If you went the network route: also delete the model so it tries to download
>    ```bash
>    rm -rf ~/.local/share/sdr-rs/models/sherpa/streaming-zipformer-en
>    ```
> 3. Launch `sdr-rs`
> 4. Splash appears, shows "Downloading sherpa-onnx model...", then... what happens? Either:
>    - It eventually fails with an HTTP error (visible in stderr)
>    - The splash closes
>    - The main app opens anyway (failure is stashed)
>    - When you toggle Sherpa transcription, the error appears in red in the transcript panel
> 5. Reconnect network, launch again, verify the auto-retry works (next launch detects the model is missing and downloads)
>
> **Test D: Splash subprocess discovery**
> 1. Verify the `--splash` argv mode works directly:
>    ```bash
>    echo "text:Hello from CLI" | sdr-rs --splash
>    ```
> 2. A small GTK window appears with "Hello from CLI" as the label, a spinner, and closes when echo exits (EOF on stdin)
>
> **Test E: Cleanup**
> ```bash
> # Either keep the freshly-downloaded model:
> rm -rf ~/sherpa-zipformer-backup
>
> # OR restore the backup if you don't trust the new download:
> rm -rf ~/.local/share/sdr-rs/models/sherpa/streaming-zipformer-en
> mv ~/sherpa-zipformer-backup ~/.local/share/sdr-rs/models/sherpa/streaming-zipformer-en
> ```
>
> Report back which tests passed/failed."

- [ ] **Step 4: Wait for confirmation**

Don't proceed to PR creation until tests A, B, and D pass at minimum. Test C (failure path) is nice to have but harder to set up reliably.

If anything fails, debug, fix on the same branch, and re-run the smoke test.

- [ ] **Step 5: Whisper regression check**

```bash
make install CARGO_FLAGS="--release --features whisper-cuda" 2>&1 | tail -5
```

Tell the user:

> "Whisper CUDA reinstalled. Quick regression check: launch sdr-rs, verify Whisper still works exactly as before. No splash should appear — splash only fires when sherpa is enabled. Whisper transcription should be unchanged."

Wait for confirmation.

---

## Phase 6: PR (Task 14)

### Task 14: Open the PR

- [ ] **Step 1: Push the branch**

```bash
cd /data/source/rtl-sdr
git push -u origin feature/sherpa-model-auto-download
```

- [ ] **Step 2: Create the PR**

```bash
gh pr create --title "Sherpa model auto-download + GTK splash window (PR 3 of 5 for #204)" --body "$(cat <<'EOF'
## Summary

Two related features in one PR:

1. **Auto-download of sherpa-onnx model bundles.** PR 2 left this as a manual `wget + tar -xjf` step. The host worker now downloads + extracts the bundle on first run, atomic-renames into place, and falls through to recognizer creation as before. Cached path is unchanged.

2. **Bundled GTK splash window** that shows progress during the otherwise-blocking sherpa init phase. New \`sdr-splash\` (controller, no UI deps) and \`sdr-splash-gtk\` (Linux GTK4 + libadwaita window) crates. The splash subprocess is the same \`sdr-rs\` binary re-exec'd with \`--splash\` argv — single install target. The splash subprocess has its own address space so it can load GTK without polluting the parent's pristine ONNX-Runtime-friendly state (the heap-corruption workaround from PR #249 is preserved).

This is **PR 3 of 5** for #204.

## What changed

**New crates:**

- \`crates/sdr-splash\` — cross-platform controller, zero UI deps. \`SplashController::try_spawn(initial_text)\` re-execs \`current_exe()\` with \`--splash\`, exposes \`update_text(text)\`, \`Drop\` closes the subprocess. Falls back gracefully to a no-op state if subprocess spawn fails.
- \`crates/sdr-splash-gtk\` — Linux-only library, GTK4 + libadwaita deps. \`pub fn run()\` opens a tiny window with a spinner and a label, reads single-line commands from stdin (\`text:<msg>\` to update label, \`done\` or EOF to exit).

**New code in \`crates/sdr-transcription/src/sherpa_model.rs\`:**

- \`SherpaModelError\` enum (\`Io\` / \`Http\` / \`Extract\`)
- \`SherpaModel::archive_filename()\`, \`archive_inner_directory()\`, \`archive_url()\`
- \`download_sherpa_model(model, progress_tx)\` — downloads .tar.bz2 to \`<filename>.part\`, extracts via bzip2 + tar, atomic-renames, cleans up scratch
- \`cleanup_scratch_state()\` helper for recovering from previous failed attempts

**New code in \`crates/sdr-transcription/src/init_event.rs\`:**

- \`InitEvent\` enum: \`DownloadStart\`, \`DownloadProgress { pct }\`, \`Extracting\`, \`CreatingRecognizer\`, \`Ready\`, \`Failed { message }\`

**Modified \`crates/sdr-transcription/src/backends/sherpa.rs\`:**

- \`SherpaHost::spawn\` now returns \`mpsc::Receiver<InitEvent>\` instead of \`Result<Self, BackendError>\`
- The worker thread populates \`SHERPA_HOST\` directly (the old \`init_tx\`/\`init_rx\` handshake is gone)
- \`run_host_loop\` downloads if needed, emits InitEvents, builds SherpaHost on success or stores Err in OnceLock on failure

**Modified \`src/main.rs\`:**

- New top-level \`--splash\` argv dispatch — re-execs into \`sdr_splash_gtk::run()\` for the splash subprocess
- New sherpa init event loop that drives \`SplashController\` from \`InitEvent\` messages

**New deps (sherpa-feature-gated):**

- \`tar = \"0.4\"\`
- \`bzip2 = \"0.4\"\` (statically links libbz2)
- \`sdr-splash\` (workspace dep, cross-platform)
- \`sdr-splash-gtk\` (workspace dep, Linux target-conditional)

## What didn't change

- \`crates/sdr-ui/*\` — no UI changes to the main app
- \`crates/sdr-transcription/src/backend.rs\` — no trait or error type changes
- All whisper code paths
- Single install target — the splash binary is the same \`sdr-rs\` binary in a different argv mode

## First-run UX

\`\`\`
$ sdr-rs
[GTK splash window appears immediately with "Initializing sherpa-onnx..."]
[Label updates: "Downloading sherpa-onnx model... 1%", "...50%", "...100%"]
[Label updates: "Extracting sherpa-onnx model..."]
[Label updates: "Loading sherpa-onnx recognizer..."]
[Splash window closes]
[Main GTK app appears, transcription works immediately]
\`\`\`

Subsequent runs see \`model_exists() == true\`, splash flickers briefly with "Initializing sherpa-onnx..." → "Loading sherpa-onnx recognizer..." for ~1-2 seconds, then the main app launches.

## Heap corruption workaround preserved

PR 2 spent days debugging a heap corruption that occurs when sherpa-onnx's bundled ONNX Runtime initializes inside a process that has GTK4 loaded. The fix was to create the OnlineRecognizer BEFORE \`sdr_ui::run()\` loads GTK. PR 3 preserves this constraint:

- The splash window runs in a separate subprocess (\`sdr-rs --splash\`) with its own address space — its GTK init can't pollute the parent
- The parent process (\`sdr-rs\` main mode) still does NOT load GTK until after the sherpa worker emits \`InitEvent::Ready\`
- main.rs blocks on the event channel exactly as long as it used to block on \`init_rx\` in PR 2

## Why bundled splash instead of zenity / notify-send

We considered three options:
- **Zenity progress dialog (subprocess).** Adds an undeclared runtime dep (zenity is not always installed on Debian/server installs). Less control over the UI.
- **Desktop notification (notify-send).** Lightweight but not a window with a spinner — just toasts.
- **Bundled Rust GTK splash.** Total control over UI/text, no runtime dep, can be extended for future preload tasks (the user mentioned "preload setup" as a future requirement).

The bundled approach won. It's also small (~150 lines of GTK code in sdr-splash-gtk plus ~80 lines of controller in sdr-splash) and the architecture cleanly accommodates future variants (\`sdr-splash-cocoa\` or similar for the macOS port).

## Test plan

- [x] \`cargo build --workspace\` clean (default whisper-cpu)
- [x] \`cargo build --workspace --no-default-features --features sherpa-cpu\` clean
- [x] \`cargo clippy --all-targets --workspace -- -D warnings\` clean (both configurations)
- [x] \`cargo test --workspace\` passes in both configurations
- [x] \`cargo fmt --all -- --check\` clean
- [x] \`make lint\` clean
- [x] **Test A (Manual):** first-run download triggers splash, label updates through all phases, sherpa transcription works after auto-download
- [x] **Test B (Manual):** cached path shows brief splash and starts in <2 seconds
- [x] **Test C (Manual):** download failure surfaces error in transcript panel via the existing PR 2 path
- [x] **Test D (Manual):** \`echo \"text:test\" | sdr-rs --splash\` opens a window directly
- [x] **Test E (Manual):** whisper-cuda regression check — Whisper still works unchanged

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CodeRabbit review**

Per project workflow.

---

## Done

After PR 3 merges:

- ✅ **PR 1** — TranscriptionBackend trait + Whisper refactor
- ✅ **PR 2** — Sherpa spike + mutex feature flags
- ✅ **PR 3** — This PR
- ⬜ **PR 4** — Display mode toggle + live captions two-line rendering for partial events
- ⬜ **PR 5+** — Parakeet (#223), Moonshine (#224)

Optional follow-up spike: parallel-init for SherpaHost so download progress could surface in the UI directly (no splash needed). Low priority — the splash is good enough.
