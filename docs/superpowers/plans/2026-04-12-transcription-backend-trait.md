# TranscriptionBackend Trait Refactor — PR 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor `sdr-transcription` so the engine delegates to a `TranscriptionBackend` trait, preserving 100% of current Whisper behavior while creating the extension point for sherpa-onnx (PR 2+).

**Architecture:** Pure behavior-preserving refactor. A new `backend.rs` defines the `TranscriptionBackend` trait, `BackendHandle`, `BackendConfig`, and the (unchanged) `TranscriptionEvent` enum. The existing `worker.rs` is moved into `backends/whisper.rs` and wrapped in a `WhisperBackend` struct that implements the trait. `TranscriptionEngine` becomes a thin owner of `Box<dyn TranscriptionBackend>` and delegates lifecycle methods. The public engine API stays unchanged so call sites in `sdr-ui` need zero modifications.

**Tech Stack:** Rust 2024 edition, `whisper-rs`, `mpsc` channels, `Arc<AtomicBool>` cancellation, `thiserror`.

**Spec:** `docs/superpowers/specs/2026-04-12-transcription-backend-trait-design.md`

**Branch:** `feature/transcription-backend-trait` (already created, design doc committed)

---

## File Structure

**Create:**
- `crates/sdr-transcription/src/backend.rs` — trait, `BackendHandle`, `BackendConfig`, `TranscriptionEvent`
- `crates/sdr-transcription/src/backends/mod.rs` — module declarations
- `crates/sdr-transcription/src/backends/whisper.rs` — `WhisperBackend` struct + the worker implementation moved from `worker.rs`
- `crates/sdr-transcription/src/backends/mock.rs` — `MockBackend` for unit-testing the engine (`#[cfg(test)]`)

**Modify:**
- `crates/sdr-transcription/src/lib.rs` — refactor `TranscriptionEngine` to delegate via the trait, swap `pub use worker::*` for `pub use backend::*`

**Delete:**
- `crates/sdr-transcription/src/worker.rs` — content moved to `backends/whisper.rs`

**Untouched (regression surface check):**
- `crates/sdr-transcription/src/denoise.rs` — unchanged
- `crates/sdr-transcription/src/resampler.rs` — unchanged
- `crates/sdr-transcription/src/model.rs` — unchanged
- `crates/sdr-ui/src/window.rs` — must compile with zero edits
- `crates/sdr-ui/src/sidebar/transcript_panel.rs` — must compile with zero edits
- `crates/sdr-ui/src/dsp_controller.rs` — must compile with zero edits

**Conventions for this refactor:**
- Variant names of `TranscriptionEvent` MUST stay byte-identical to today (`Downloading { progress_pct }`, `Ready`, `Text { timestamp, text }`, `Error(String)`). UI code matches on these by name.
- The public engine API (`new`, `start`, `stop`, `shutdown_nonblocking`, `audio_sender`, `is_running`, `Drop`) must keep its current signatures.
- No formatting churn in unrelated files. `cargo fmt` only on what we touch.

**Deviation from spec:** The spec lists "Add `TranscriptionEvent::Partial` variant" as part of PR 1. This plan defers it to PR 2 because adding a new variant would force a non-exhaustive match in `sdr-ui/src/window.rs`, which violates the "zero UI changes" constraint of PR 1. The variant lands in PR 2 alongside the sherpa backend that actually emits it, in the same commit that adds the corresponding window.rs match arm. This is a strict-refactor purity tradeoff and does not change the eventual surface area.

---

## Task 1: Create `backend.rs` with trait and types

**Files:**
- Create: `crates/sdr-transcription/src/backend.rs`

- [ ] **Step 1: Write the new file**

```rust
//! Backend abstraction for the transcription engine.
//!
//! `TranscriptionBackend` is the trait every ASR implementation must satisfy.
//! The engine owns one backend at a time and delegates lifecycle to it.
//! This file defines the trait, the handle returned by `start`, the config
//! passed in, and the event type emitted to consumers.

use std::sync::mpsc;

use crate::model::WhisperModel;

/// Configuration handed to a backend at `start` time.
///
/// `model` selects which ASR model the backend should load. Additional
/// fields are preprocessing parameters shared across all backends.
#[derive(Debug, Clone, Copy)]
pub struct BackendConfig {
    pub model: ModelChoice,
    pub silence_threshold: f32,
    pub noise_gate_ratio: f32,
}

/// User-facing model selection.
///
/// The variant determines which backend the engine instantiates internally.
/// Currently only `Whisper` is implemented; `Sherpa` lands in PR 2.
#[derive(Debug, Clone, Copy)]
pub enum ModelChoice {
    Whisper(WhisperModel),
}

/// Events emitted by a backend during its lifecycle.
///
/// Variant names are stable — UI consumers match on them by name.
#[derive(Debug, Clone)]
pub enum TranscriptionEvent {
    /// Model download in progress (0..=100).
    Downloading { progress_pct: u8 },
    /// Model loaded and ready for inference.
    Ready,
    /// Transcribed text from one inference pass.
    Text {
        /// Wall-clock timestamp in "HH:MM:SS" format.
        timestamp: String,
        /// Transcribed text (trimmed, non-empty).
        text: String,
    },
    /// Fatal error — backend will exit after sending this.
    Error(String),
}

/// Returned by [`TranscriptionBackend::start`]. Carries the channels the
/// engine wires through to its caller.
pub struct BackendHandle {
    /// Push 48 kHz interleaved stereo f32 samples into the backend.
    pub audio_tx: mpsc::SyncSender<Vec<f32>>,
    /// Receive transcription events from the backend.
    pub event_rx: mpsc::Receiver<TranscriptionEvent>,
}

/// Errors a backend can return from `start`.
///
/// Mirrors `crate::TranscriptionError` so the engine can convert
/// transparently. Kept separate so backends don't depend on the engine.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("failed to spawn worker thread: {0}")]
    Spawn(#[from] std::io::Error),
}

/// Trait every transcription backend must implement.
///
/// Backends own their own worker threads. The engine just holds a
/// `Box<dyn TranscriptionBackend>` and delegates lifecycle calls.
pub trait TranscriptionBackend: Send {
    /// Human-readable backend name (used for tracing/logging).
    fn name(&self) -> &'static str;

    /// True if this backend can emit incremental partial hypotheses.
    /// Whisper returns `false`; streaming backends return `true`.
    /// Used by the UI to enable/disable the "live captions" toggle.
    fn supports_partials(&self) -> bool;

    /// Spawn worker thread(s) and return channels for audio in / events out.
    ///
    /// Must emit [`TranscriptionEvent::Ready`] once the model is loaded.
    fn start(&mut self, config: BackendConfig) -> Result<BackendHandle, BackendError>;

    /// Signal the backend to stop without waiting for it to finish.
    ///
    /// Drops the audio sender so the worker exits after its current
    /// inference completes; detaches the thread so the caller never blocks.
    fn shutdown_nonblocking(&mut self);
}
```

- [ ] **Step 2: Verify it compiles in isolation**

The file references `WhisperModel` from `crate::model`. Run:

```bash
cd /data/source/rtl-sdr
cargo check -p sdr-transcription 2>&1 | tail -30
```

Expected: error about `unresolved module 'backend'` from `lib.rs` (we haven't wired it up yet) but the file itself parses cleanly. If you see errors *inside* `backend.rs`, fix them before moving on.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-transcription/src/backend.rs
git commit -m "$(cat <<'EOF'
sdr-transcription: add TranscriptionBackend trait + types

New backend.rs defines the trait, BackendHandle, BackendConfig,
ModelChoice, BackendError, and (unchanged) TranscriptionEvent. Not
wired into lib.rs yet — that happens in the next task.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Create `backends/` module skeleton

**Files:**
- Create: `crates/sdr-transcription/src/backends/mod.rs`

- [ ] **Step 1: Write the module file**

```rust
//! Concrete `TranscriptionBackend` implementations.
//!
//! Each backend is a self-contained module. The engine in `lib.rs`
//! constructs one based on the [`crate::backend::ModelChoice`] variant
//! and delegates lifecycle to it.

pub mod whisper;

#[cfg(test)]
pub mod mock;
```

- [ ] **Step 2: Verify it parses**

```bash
cd /data/source/rtl-sdr
cargo check -p sdr-transcription 2>&1 | tail -30
```

Expected: errors about `unresolved module 'whisper'` and `unresolved module 'mock'` — this is fine, we create them in the next tasks.

- [ ] **Step 3: Commit (as part of next task)**

Don't commit yet — fold into Task 3 commit so the module is non-empty when we add it.

---

## Task 3: Move worker.rs into `backends/whisper.rs` as a `WhisperBackend` struct

**Files:**
- Create: `crates/sdr-transcription/src/backends/whisper.rs`
- Delete: `crates/sdr-transcription/src/worker.rs` (after content move)

This is the largest task. We're moving the existing worker to a new location and wrapping it in a struct that implements the trait. The internal worker function stays nearly identical — only the event type import path changes.

- [ ] **Step 1: Create `backends/whisper.rs` with the moved content**

Write this complete file. It is the existing `worker.rs` content with the following changes only:

1. `use crate::backend::{BackendConfig, BackendError, BackendHandle, TranscriptionBackend, TranscriptionEvent};` instead of defining `TranscriptionEvent` locally
2. The `TranscriptionEvent` enum definition is **deleted** from this file (it lives in `backend.rs` now)
3. `pub fn run_worker(...)` becomes `fn run_worker(...)` (private — only `WhisperBackend` calls it)
4. New `WhisperBackend` struct + `impl TranscriptionBackend` block at the top
5. `compute_rms` stays `pub(crate)` because tests live in this file but no other module needs it
6. Tests stay at the bottom of the file unchanged

Full file content:

```rust
//! Whisper backend — `whisper-rs` powered transcription.
//!
//! Implements [`TranscriptionBackend`] for the [`crate::model::WhisperModel`]
//! family. Receives interleaved stereo f32 audio at 48 kHz, resamples to
//! 16 kHz mono, accumulates 5-second chunks, and runs Whisper inference on
//! non-silent chunks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
    TranscriptionEvent,
};
use crate::{denoise, model, resampler};

/// Bounded channel capacity for audio buffers from DSP → backend.
/// Each buffer is ~1024-4096 stereo samples (~20-80 ms). At 48 kHz with
/// 5-second inference chunks, we need ~250 buffers to avoid drops during
/// a single inference pass. 512 gives comfortable headroom.
const AUDIO_CHANNEL_CAPACITY: usize = 512;

/// Seconds of audio per transcription chunk.
const CHUNK_SECONDS: usize = 5;

/// Number of 16 kHz mono samples per chunk (16000 * 5 = 80000).
const CHUNK_SAMPLES: usize = 16_000 * CHUNK_SECONDS;

/// Polling interval for the audio receive loop when checking for cancellation.
const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// `TranscriptionBackend` implementation backed by `whisper-rs`.
pub struct WhisperBackend {
    cancel: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Default for WhisperBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WhisperBackend {
    pub fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }
}

impl TranscriptionBackend for WhisperBackend {
    fn name(&self) -> &'static str {
        "whisper"
    }

    fn supports_partials(&self) -> bool {
        false
    }

    fn start(&mut self, config: BackendConfig) -> Result<BackendHandle, BackendError> {
        let ModelChoice::Whisper(whisper_model) = config.model;

        self.cancel.store(false, Ordering::Relaxed);

        let (audio_tx, audio_rx) = mpsc::sync_channel(AUDIO_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();

        let cancel = Arc::clone(&self.cancel);
        let silence_threshold = config.silence_threshold;
        let noise_gate_ratio = config.noise_gate_ratio;

        let handle = std::thread::Builder::new()
            .name("whisper-worker".into())
            .spawn(move || {
                run_worker(
                    &audio_rx,
                    &event_tx,
                    &cancel,
                    whisper_model,
                    silence_threshold,
                    noise_gate_ratio,
                );
            })?;

        self.worker = Some(handle);
        tracing::info!("whisper backend started");

        Ok(BackendHandle { audio_tx, event_rx })
    }

    fn shutdown_nonblocking(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.worker.take(); // detach — don't join
        tracing::info!("whisper backend shutdown (non-blocking)");
    }
}

/// Main worker loop. Blocks the calling thread and exits when `audio_rx` is
/// closed (all senders dropped) or the cancellation token is set.
fn run_worker(
    audio_rx: &mpsc::Receiver<Vec<f32>>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    cancel: &Arc<AtomicBool>,
    model: model::WhisperModel,
    silence_threshold: f32,
    noise_gate_ratio: f32,
) {
    if let Err(e) = run_worker_inner(
        audio_rx,
        event_tx,
        cancel,
        model,
        silence_threshold,
        noise_gate_ratio,
    ) {
        let _ = event_tx.send(TranscriptionEvent::Error(e));
    }
}

#[allow(clippy::too_many_lines)]
fn run_worker_inner(
    audio_rx: &mpsc::Receiver<Vec<f32>>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    cancel: &Arc<AtomicBool>,
    model: model::WhisperModel,
    silence_threshold: f32,
    noise_gate_ratio: f32,
) -> Result<(), String> {
    // --- Model download / load ---
    let model_path = if model::model_exists(model) {
        tracing::info!(?model, "whisper model already present");
        model::model_path(model)
    } else {
        tracing::info!("whisper model not found, downloading");
        let (progress_tx, progress_rx) = mpsc::channel::<u8>();
        let event_tx_dl = event_tx.clone();

        let progress_thread = std::thread::Builder::new()
            .name("whisper-dl-progress".into())
            .spawn(move || {
                while let Ok(pct) = progress_rx.recv() {
                    let _ = event_tx_dl.send(TranscriptionEvent::Downloading { progress_pct: pct });
                }
            })
            .map_err(|e| format!("failed to spawn progress thread: {e}"))?;

        let path = model::download_model(model, &progress_tx)
            .map_err(|e| format!("model download failed: {e}"))?;

        drop(progress_tx);
        let _ = progress_thread.join();

        path
    };

    tracing::info!(?model_path, "loading Whisper model");
    let ctx = WhisperContext::new_with_params(&model_path, WhisperContextParameters::default())
        .map_err(|e| {
            format!(
                "Failed to load model: {e}. If using a GPU, try a smaller model — \
                 the selected model may exceed available VRAM."
            )
        })?;

    let mut state = ctx
        .create_state()
        .map_err(|e| format!("failed to create whisper state: {e}"))?;

    tracing::info!("whisper model loaded, ready for inference");
    event_tx
        .send(TranscriptionEvent::Ready)
        .map_err(|_| "event channel closed before Ready".to_owned())?;

    // --- Audio loop ---
    let mut mono_buf: Vec<f32> = Vec::with_capacity(CHUNK_SAMPLES * 2);

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("transcription cancelled, worker exiting");
            return Ok(());
        }

        let interleaved = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(data) => data,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("transcription cancelled, worker exiting");
                return Ok(());
            }
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }

        while mono_buf.len() >= CHUNK_SAMPLES {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("transcription cancelled, worker exiting");
                return Ok(());
            }

            let mut chunk: Vec<f32> = mono_buf.drain(..CHUNK_SAMPLES).collect();

            denoise::spectral_denoise(&mut chunk, noise_gate_ratio);

            let rms = compute_rms(&chunk);
            if rms < silence_threshold {
                tracing::debug!(rms, "chunk below silence threshold, skipping");
                continue;
            }

            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some("en"));
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            params.set_no_context(true);

            if let Err(e) = state.full(params, &chunk) {
                tracing::warn!("whisper inference failed: {e}");
                continue;
            }

            let n_segments = state.full_n_segments();
            let mut combined = String::new();

            for i in 0..n_segments {
                if let Some(segment) = state.get_segment(i)
                    && let Ok(text) = segment.to_str()
                {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        if !combined.is_empty() {
                            combined.push(' ');
                        }
                        combined.push_str(trimmed);
                    }
                }
            }

            if !combined.is_empty() && !is_hallucination(&combined) {
                let timestamp = chrono_timestamp();
                tracing::debug!(%timestamp, %combined, "transcribed chunk");
                let _ = event_tx.send(TranscriptionEvent::Text {
                    timestamp,
                    text: combined,
                });
            }
        }
    }

    tracing::info!("audio channel closed, worker exiting");
    Ok(())
}

/// Common hallucination phrases Whisper produces on silence/noise.
const HALLUCINATIONS: &[&str] = &[
    "thank you",
    "thanks for watching",
    "subscribe",
    "like and subscribe",
    "see you next time",
    "bye",
    "you",
    "the end",
];

/// Check if Whisper output is a known hallucination pattern.
///
/// Whisper tends to produce these when fed non-speech audio (radio static,
/// tones, data bursts). We filter them out to keep the transcript clean.
fn is_hallucination(text: &str) -> bool {
    let lower = text.to_lowercase();

    if (lower.starts_with('[') && lower.ends_with(']'))
        || (lower.starts_with('(') && lower.ends_with(')'))
    {
        return true;
    }

    HALLUCINATIONS
        .iter()
        .any(|h| lower.trim().eq_ignore_ascii_case(h))
}

/// Compute the root-mean-square of a sample buffer.
pub(crate) fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum_sq / samples.len() as f32;
    mean.sqrt()
}

/// Return the current wall-clock time formatted as "HH:MM:SS" in local time.
#[allow(unsafe_code)]
fn chrono_timestamp() -> String {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };

    // SAFETY: `gettimeofday` writes into the provided buffer and is
    // thread-safe. We pass null for the timezone (deprecated parameter).
    #[allow(unsafe_code)]
    let epoch = unsafe {
        libc::gettimeofday(&raw mut tv, std::ptr::null_mut());
        tv.tv_sec
    };

    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();

    // SAFETY: `localtime_r` is the reentrant (thread-safe) variant.
    // We provide a valid `time_t` and a valid output buffer.
    // Returns null on failure, in which case we fall back to UTC via `gmtime_r`.
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
    fn rms_of_silence_is_zero() {
        let silence = vec![0.0_f32; 1024];
        let rms = compute_rms(&silence);
        assert!((rms - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rms_of_ones_is_one() {
        let ones = vec![1.0_f32; 1024];
        let rms = compute_rms(&ones);
        assert!((rms - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rms_of_empty_is_zero() {
        let rms = compute_rms(&[]);
        assert!((rms - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn whisper_backend_does_not_support_partials() {
        let backend = WhisperBackend::new();
        assert!(!backend.supports_partials());
    }

    #[test]
    fn whisper_backend_name_is_stable() {
        let backend = WhisperBackend::new();
        assert_eq!(backend.name(), "whisper");
    }
}
```

- [ ] **Step 2: Delete `worker.rs`**

```bash
cd /data/source/rtl-sdr
rm crates/sdr-transcription/src/worker.rs
```

- [ ] **Step 3: Verify (lib.rs is still wrong, expected)**

```bash
cargo check -p sdr-transcription 2>&1 | tail -40
```

Expected: errors in `lib.rs` about `mod worker` not found and `worker::run_worker` not in scope. The new files compile fine — the errors are *only* in `lib.rs`. Fix `lib.rs` in Task 5. If you see errors inside `backends/whisper.rs` itself, fix them before moving on.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-transcription/src/backends/mod.rs crates/sdr-transcription/src/backends/whisper.rs
git rm crates/sdr-transcription/src/worker.rs
git commit -m "$(cat <<'EOF'
sdr-transcription: move worker into backends/whisper.rs

Wraps the existing worker loop in a WhisperBackend struct that
implements TranscriptionBackend. Worker function is now private
to the module. Behavior unchanged — same channels, same threading,
same hallucination filter, same chunk handling. Two new tests verify
the trait surface (name + supports_partials).

lib.rs is intentionally broken at this commit; fixed in the next task.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Add `MockBackend` for engine tests

**Files:**
- Create: `crates/sdr-transcription/src/backends/mock.rs`

The mock lets us unit-test `TranscriptionEngine` lifecycle without loading a real Whisper model. It records `start` / `shutdown_nonblocking` calls and lets tests inject events.

- [ ] **Step 1: Write the mock**

```rust
//! Mock backend for unit-testing the transcription engine.
//!
//! Records lifecycle calls and lets tests push events into the channel
//! the engine hands out to its consumer. Construct via `MockBackend::new`,
//! optionally configure `supports_partials` for testing UI gating logic.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, TranscriptionBackend, TranscriptionEvent,
};

/// Records what the engine did to a backend. Cloneable handle so tests
/// can inspect state after the engine drops the backend.
#[derive(Clone, Default)]
pub struct MockState {
    pub start_count: Arc<AtomicUsize>,
    pub shutdown_count: Arc<AtomicUsize>,
    pub last_event_tx: Arc<Mutex<Option<mpsc::Sender<TranscriptionEvent>>>>,
    pub supports_partials_value: Arc<AtomicBool>,
}

pub struct MockBackend {
    state: MockState,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            state: MockState::default(),
        }
    }

    /// Get a clone of the state for inspection in tests.
    pub fn state(&self) -> MockState {
        self.state.clone()
    }

    /// Configure what `supports_partials` returns.
    pub fn with_supports_partials(self, value: bool) -> Self {
        self.state.supports_partials_value.store(value, Ordering::Relaxed);
        self
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptionBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn supports_partials(&self) -> bool {
        self.state.supports_partials_value.load(Ordering::Relaxed)
    }

    fn start(&mut self, _config: BackendConfig) -> Result<BackendHandle, BackendError> {
        self.state.start_count.fetch_add(1, Ordering::Relaxed);

        let (audio_tx, _audio_rx) = mpsc::sync_channel(8);
        let (event_tx, event_rx) = mpsc::channel();

        // Stash the event_tx so tests can push events through it.
        *self.state.last_event_tx.lock().expect("mock state poisoned") = Some(event_tx);

        Ok(BackendHandle { audio_tx, event_rx })
    }

    fn shutdown_nonblocking(&mut self) {
        self.state.shutdown_count.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ModelChoice;
    use crate::model::WhisperModel;

    fn dummy_config() -> BackendConfig {
        BackendConfig {
            model: ModelChoice::Whisper(WhisperModel::TinyEn),
            silence_threshold: 0.007,
            noise_gate_ratio: 3.0,
        }
    }

    #[test]
    fn mock_records_start_and_shutdown() {
        let mut backend = MockBackend::new();
        let state = backend.state();

        assert_eq!(state.start_count.load(Ordering::Relaxed), 0);
        assert_eq!(state.shutdown_count.load(Ordering::Relaxed), 0);

        let _handle = backend.start(dummy_config()).expect("start should succeed");
        assert_eq!(state.start_count.load(Ordering::Relaxed), 1);

        backend.shutdown_nonblocking();
        assert_eq!(state.shutdown_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn mock_partials_default_false() {
        let backend = MockBackend::new();
        assert!(!backend.supports_partials());
    }

    #[test]
    fn mock_partials_can_be_configured() {
        let backend = MockBackend::new().with_supports_partials(true);
        assert!(backend.supports_partials());
    }

    #[test]
    fn mock_can_push_events_through_handle() {
        let mut backend = MockBackend::new();
        let state = backend.state();

        let handle = backend.start(dummy_config()).expect("start should succeed");

        // Test pushes an event through the stashed sender; the handle's
        // receiver should see it.
        let tx = state
            .last_event_tx
            .lock()
            .expect("mock state poisoned")
            .clone()
            .expect("event_tx should be stashed after start");
        tx.send(TranscriptionEvent::Ready).expect("send Ready");

        let received = handle.event_rx.recv().expect("recv Ready");
        assert!(matches!(received, TranscriptionEvent::Ready));
    }
}
```

- [ ] **Step 2: Verify it compiles (still expecting lib.rs errors)**

```bash
cd /data/source/rtl-sdr
cargo check -p sdr-transcription 2>&1 | tail -40
```

Expected: same `lib.rs` errors as before. The mock file itself should be clean. If you see errors inside `mock.rs`, fix them.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-transcription/src/backends/mock.rs
git commit -m "$(cat <<'EOF'
sdr-transcription: add MockBackend for engine unit tests

Test-only backend that records start/shutdown calls and exposes the
event sender so tests can push events through the handle the engine
hands out. Has its own unit tests verifying the recording surface.
Used by lib.rs tests in the next task.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Refactor `lib.rs` `TranscriptionEngine` to delegate via the trait

**Files:**
- Modify: `crates/sdr-transcription/src/lib.rs`

This is the heart of the refactor. The `TranscriptionEngine` keeps its current public API but internally holds `Box<dyn TranscriptionBackend>` and delegates lifecycle calls. We add a `pub(crate)` `start_with_backend` for tests so unit tests can inject a `MockBackend`.

- [ ] **Step 1: Replace the file content**

```rust
//! Live audio transcription.
//!
//! Provides [`TranscriptionEngine`] — a backend-agnostic façade over
//! [`backend::TranscriptionBackend`] implementations. The engine owns
//! one backend at a time and delegates lifecycle to it.
//!
//! Currently only the Whisper backend is implemented; sherpa-onnx
//! lands in PR 2.

pub mod backend;
pub mod backends;
pub mod denoise;
pub mod model;
pub mod resampler;

pub use backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
    TranscriptionEvent,
};
pub use model::WhisperModel;

use std::sync::mpsc;

use crate::backends::whisper::WhisperBackend;

/// Error type for engine-level operations.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptionError {
    #[error("transcription is already running")]
    AlreadyRunning,
    #[error("transcription is not running")]
    NotRunning,
    #[error(transparent)]
    Backend(#[from] BackendError),
}

/// Backend-agnostic live transcription engine.
///
/// Holds one [`TranscriptionBackend`] at a time. The public API matches
/// the pre-refactor `TranscriptionEngine` so existing call sites in
/// `sdr-ui` need no changes.
pub struct TranscriptionEngine {
    backend: Option<Box<dyn TranscriptionBackend>>,
    audio_tx: Option<mpsc::SyncSender<Vec<f32>>>,
}

impl Default for TranscriptionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptionEngine {
    pub fn new() -> Self {
        Self {
            backend: None,
            audio_tx: None,
        }
    }

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

    /// Start the engine with a caller-provided backend.
    ///
    /// Used internally by [`Self::start`] and by unit tests that want to
    /// inject a mock backend. Will become `pub` in PR 2 once the UI can
    /// pick a backend.
    pub(crate) fn start_with_backend(
        &mut self,
        mut backend: Box<dyn TranscriptionBackend>,
        config: BackendConfig,
    ) -> Result<mpsc::Receiver<TranscriptionEvent>, TranscriptionError> {
        if self.backend.is_some() {
            return Err(TranscriptionError::AlreadyRunning);
        }

        let BackendHandle { audio_tx, event_rx } = backend.start(config)?;
        self.audio_tx = Some(audio_tx);
        self.backend = Some(backend);

        tracing::info!("transcription engine started");
        Ok(event_rx)
    }

    /// Stop the engine, blocking until the backend's worker has finished.
    ///
    /// May block for the duration of one inference pass. Use
    /// [`Self::shutdown_nonblocking`] from the UI thread or during app exit.
    pub fn stop(&mut self) {
        self.audio_tx.take();
        if let Some(mut backend) = self.backend.take() {
            backend.stop();
            tracing::info!("transcription engine stopped");
        }
    }

    /// Signal the backend to shut down without waiting.
    ///
    /// Drops the audio sender so the worker exits after its current
    /// inference completes; detaches the thread so the caller never blocks.
    pub fn shutdown_nonblocking(&mut self) {
        self.audio_tx.take();
        if let Some(mut backend) = self.backend.take() {
            backend.shutdown_nonblocking();
        }
        tracing::info!("transcription engine stopped");
    }

    /// Get a clone of the audio sender for feeding samples from the DSP thread.
    pub fn audio_sender(&self) -> Option<mpsc::SyncSender<Vec<f32>>> {
        self.audio_tx.clone()
    }

    /// True if the engine has an active backend.
    pub fn is_running(&self) -> bool {
        self.backend.is_some()
    }

    /// True if the active backend can emit partial hypotheses.
    /// Returns `false` if no backend is running.
    pub fn supports_partials(&self) -> bool {
        self.backend
            .as_ref()
            .is_some_and(|b| b.supports_partials())
    }
}

impl Drop for TranscriptionEngine {
    fn drop(&mut self) {
        self.shutdown_nonblocking();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::mock::MockBackend;
    use std::sync::atomic::Ordering;

    fn dummy_config() -> BackendConfig {
        BackendConfig {
            model: ModelChoice::Whisper(WhisperModel::TinyEn),
            silence_threshold: 0.007,
            noise_gate_ratio: 3.0,
        }
    }

    #[test]
    fn engine_new_is_not_running() {
        let engine = TranscriptionEngine::new();
        assert!(!engine.is_running());
        assert!(engine.audio_sender().is_none());
        assert!(!engine.supports_partials());
    }

    #[test]
    fn engine_starts_with_mock_backend() {
        let mut engine = TranscriptionEngine::new();
        let backend = Box::new(MockBackend::new());
        let state = backend.state();

        let _event_rx = engine
            .start_with_backend(backend, dummy_config())
            .expect("start should succeed");

        assert!(engine.is_running());
        assert!(engine.audio_sender().is_some());
        assert_eq!(state.start_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn engine_double_start_returns_already_running() {
        let mut engine = TranscriptionEngine::new();
        let backend1 = Box::new(MockBackend::new());
        engine
            .start_with_backend(backend1, dummy_config())
            .expect("first start ok");

        let backend2 = Box::new(MockBackend::new());
        let err = engine
            .start_with_backend(backend2, dummy_config())
            .expect_err("second start should fail");
        assert!(matches!(err, TranscriptionError::AlreadyRunning));
    }

    #[test]
    fn engine_shutdown_clears_state() {
        let mut engine = TranscriptionEngine::new();
        let backend = Box::new(MockBackend::new());
        let state = backend.state();

        engine
            .start_with_backend(backend, dummy_config())
            .expect("start ok");

        engine.shutdown_nonblocking();

        assert!(!engine.is_running());
        assert!(engine.audio_sender().is_none());
        assert_eq!(state.shutdown_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn engine_supports_partials_reflects_backend() {
        let mut engine = TranscriptionEngine::new();
        let backend = Box::new(MockBackend::new().with_supports_partials(true));

        engine
            .start_with_backend(backend, dummy_config())
            .expect("start ok");

        assert!(engine.supports_partials());
    }

    #[test]
    fn engine_drop_runs_shutdown() {
        let backend = Box::new(MockBackend::new());
        let state = backend.state();
        {
            let mut engine = TranscriptionEngine::new();
            engine
                .start_with_backend(backend, dummy_config())
                .expect("start ok");
        }
        // Engine dropped here.
        assert_eq!(state.shutdown_count.load(Ordering::Relaxed), 1);
    }
}
```

- [ ] **Step 2: Build the crate**

```bash
cd /data/source/rtl-sdr
cargo build -p sdr-transcription 2>&1 | tail -30
```

Expected: clean build, no errors. If you see errors, read them carefully — most likely cause is a typo in a re-export or a missing import.

- [ ] **Step 3: Run the crate's tests**

```bash
cargo test -p sdr-transcription 2>&1 | tail -40
```

Expected: all tests pass. You should see (at minimum) the three RMS tests, the two `WhisperBackend` trait tests, the four mock tests, and the six engine tests = 15 tests passing in `sdr-transcription`. Plus whatever exists in `denoise.rs` and `model.rs`.

If a test fails, fix it before continuing — do not move on with red tests.

- [ ] **Step 4: Build the whole workspace to confirm `sdr-ui` still compiles**

```bash
cargo build --workspace 2>&1 | tail -30
```

Expected: clean build. The whole point of keeping the public engine API stable is that `sdr-ui` should compile with zero edits. If `sdr-ui` has errors, the refactor broke the API contract — fix `lib.rs` to restore compatibility, do not touch `sdr-ui`.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-transcription/src/lib.rs
git commit -m "$(cat <<'EOF'
sdr-transcription: refactor TranscriptionEngine to delegate via trait

Engine now holds Box<dyn TranscriptionBackend> and delegates start /
shutdown / audio_sender / is_running to the backend. Public start()
method preserved for API compatibility — internally constructs a
WhisperBackend and calls start_with_backend(). Adds supports_partials()
for the upcoming UI live-captions toggle gating.

Six new engine unit tests cover lifecycle, double-start, shutdown,
partials reflection, and Drop behavior — all using MockBackend so
they run without loading a real Whisper model.

sdr-ui call sites unchanged.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Lint, format, and full workspace verification

- [ ] **Step 1: Run clippy on the workspace**

```bash
cd /data/source/rtl-sdr
cargo clippy --all-targets --workspace -- -D warnings 2>&1 | tail -40
```

Expected: zero warnings. Common issues to expect and how to fix:
- "unused import" — remove the import
- "method is never used" — add `#[cfg(test)]` if it's only used in tests, or `#[allow(dead_code)]` only if you have a strong reason
- "module has the same name as its containing module" — false positive if `backends/mod.rs` declares `pub mod whisper`. Should not occur with the layout above, but if it does, the fix is to move declarations into `lib.rs` directly. Don't change the layout — investigate first.

If clippy flags real issues, fix them and re-run until clean. Do not silence warnings to make clippy pass.

- [ ] **Step 2: Run fmt check**

```bash
cargo fmt --all -- --check 2>&1 | tail -20
```

Expected: no output (clean). If anything prints, run `cargo fmt --all` to auto-fix and commit the result.

- [ ] **Step 3: Run the full test suite**

```bash
cargo test --workspace 2>&1 | tail -60
```

Expected: all tests pass. Pay particular attention to the `sdr-transcription` test count — should be at least 15 (3 RMS + 2 backend + 4 mock + 6 engine). If the count is lower, a test got dropped during the move.

- [ ] **Step 4: Run cargo deny**

```bash
make lint 2>&1 | tail -40
```

Expected: clean. We didn't add any new dependencies in this PR so this should be a no-op, but verify.

- [ ] **Step 5: Commit any fmt/lint fixes if needed**

```bash
git status
# If anything is modified:
git add -u
git commit -m "$(cat <<'EOF'
sdr-transcription: fmt + clippy fixes

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

If `git status` is clean, skip this step.

---

## Task 7: Manual end-to-end smoke test

**This task requires the user. Do not attempt to automate.**

The whole point of this refactor is that nothing the user notices has changed. Verify by running through the existing transcription flow.

- [ ] **Step 1: Build and install with CUDA**

```bash
cd /data/source/rtl-sdr
make install CARGO_FLAGS="--release --features cuda" 2>&1 | tail -20
```

Expected: clean build, binary installed.

- [ ] **Step 2: Hand off to user with this script**

Tell the user:

> "PR 1 (trait refactor) is built and installed. This is a pure refactor — nothing should look or feel different. Please verify:
>
> 1. Launch the app
> 2. Open the transcript panel (header bar button)
> 3. Pick a Whisper model (try Base or Small)
> 4. Tune to a NFM voice channel and hit Play
> 5. Toggle 'Enable transcription' on
> 6. Confirm:
>    - Model loads (toast or log shows 'Ready')
>    - Live transcription appears in the panel as today
>    - Stopping the radio stops transcription
>    - Toggling transcription off clears state
>    - Closing the app doesn't hang
> 7. Switch models mid-session — confirm the new model loads
>
> Anything different from current behavior is a regression and needs fixing before we move on."

- [ ] **Step 3: Wait for user confirmation**

Do not proceed to PR creation until the user reports the smoke test passing. If they hit a regression, debug, fix on the same branch, and re-run the smoke test.

---

## Task 8: Open the PR

- [ ] **Step 1: Push the branch**

```bash
cd /data/source/rtl-sdr
git push -u origin feature/transcription-backend-trait
```

- [ ] **Step 2: Create the PR**

```bash
gh pr create --title "Refactor transcription engine behind TranscriptionBackend trait" --body "$(cat <<'EOF'
## Summary

Pure behavior-preserving refactor of `sdr-transcription`. Introduces a `TranscriptionBackend` trait, moves the existing Whisper worker behind it, and refactors `TranscriptionEngine` to delegate lifecycle via `Box<dyn TranscriptionBackend>`.

This is **PR 1 of 5** for #204. It creates the extension point for the sherpa-onnx backend (PR 2) and the model auto-download / UI work that follows. No new dependencies, no UI changes, no behavioral changes — `sdr-ui` call sites are untouched.

See `docs/superpowers/specs/2026-04-12-transcription-backend-trait-design.md` for the full design.

## What changed

- New `backend.rs` defines `TranscriptionBackend`, `BackendHandle`, `BackendConfig`, `ModelChoice`, `BackendError`
- `TranscriptionEvent` moved into `backend.rs` (variant names unchanged)
- New `backends/whisper.rs` houses `WhisperBackend` and the worker function (moved from `worker.rs`)
- New `backends/mock.rs` provides `MockBackend` for engine unit tests
- `lib.rs` `TranscriptionEngine` refactored to delegate via the trait
- Added `supports_partials()` to engine API (returns false for Whisper) — used by future UI live-captions toggle gating
- 11 new unit tests covering the trait surface, mock recording, and engine lifecycle
- `worker.rs` deleted

## Test plan

- [ ] `cargo build --workspace` clean
- [ ] `cargo clippy --all-targets --workspace -- -D warnings` clean
- [ ] `cargo test --workspace` passes (15+ tests in sdr-transcription)
- [ ] `cargo fmt --all -- --check` clean
- [ ] `make lint` clean (no new deps, sanity check)
- [ ] Manual: launch app, enable transcription with a Whisper model, verify it transcribes a real radio source (CUDA build)
- [ ] Manual: switch models mid-session, verify reload works
- [ ] Manual: stop radio, verify transcription stops
- [ ] Manual: close app while transcription running, verify no hang

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CodeRabbit review**

Do not merge until CodeRabbit has reviewed and any inline comments are addressed. Reply to every CR inline comment per project workflow.

---

## Done

After PR 1 merges, PR 2 (sherpa spike) is unblocked and can begin in a fresh branch. The sherpa work will:

1. Add `sherpa-onnx = "1.12"` to the crate
2. Create `backends/sherpa.rs` with a real `SherpaBackend` implementing the trait
3. Investigate the CUDA story (open question in the spec)
4. Stand up Streaming Zipformer end-to-end with manual model download for the spike

That's PR 2's plan, not this one.
