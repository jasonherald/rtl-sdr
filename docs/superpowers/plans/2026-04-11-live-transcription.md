# Live Transcription Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Real-time speech-to-text for demodulated radio audio, displayed in a scrolling transcript log in the sidebar.

**Architecture:** New `sdr-transcription` crate wraps `whisper-rs` (whisper.cpp bindings) with a background worker thread. Audio is tapped from the DSP controller via a bounded channel, resampled 48 kHz stereo → 16 kHz mono, accumulated in chunks, and fed to Whisper for inference. Results flow back to a new Transcript sidebar panel via an event channel polled on a GTK timer.

**Tech Stack:** `whisper-rs` (whisper.cpp bindings), `reqwest` (model download), GTK4/libadwaita sidebar panel

**Design Spec:** `docs/superpowers/specs/2026-04-11-live-transcription-design.md`

---

## File Structure

### New Files

```text
crates/sdr-transcription/
  Cargo.toml
  src/
    lib.rs              — TranscriptionEngine public API + TranscriptionEvent enum
    worker.rs           — background thread: audio accumulation, silence detection, Whisper inference
    resampler.rs        — 48 kHz stereo → 16 kHz mono conversion
    model.rs            — model download from HuggingFace, path management

crates/sdr-ui/src/sidebar/
  transcript_panel.rs   — Transcript sidebar panel UI
```

### Modified Files

```text
Cargo.toml                                  — workspace members + deps
crates/sdr-ui/Cargo.toml                   — add sdr-transcription dep
crates/sdr-ui/src/sidebar/mod.rs           — add transcript panel to SidebarPanels + build_sidebar
crates/sdr-ui/src/window.rs                — connect transcript panel, wire enable/disable
crates/sdr-ui/src/dsp_controller.rs        — audio tap + UiToDsp messages for transcription
crates/sdr-ui/src/messages.rs              — add transcription-related UiToDsp/DspToUi variants
```

---

## Task 1: sdr-transcription Crate — Resampler (TDD)

**Files:**
- Create: `crates/sdr-transcription/Cargo.toml`
- Create: `crates/sdr-transcription/src/lib.rs`
- Create: `crates/sdr-transcription/src/resampler.rs`
- Modify: `Cargo.toml` (workspace members + deps)

- [ ] **Step 1: Add workspace member and dependencies**

In `Cargo.toml`, add to `[workspace]` members (after `"crates/sdr-radioreference"`):

```toml
"crates/sdr-transcription",
```

Add to `[workspace.dependencies]`:

```toml
sdr-transcription = { path = "crates/sdr-transcription" }
whisper-rs = "0.14"
```

- [ ] **Step 2: Create crate Cargo.toml**

Create `crates/sdr-transcription/Cargo.toml`:

```toml
[package]
name = "sdr-transcription"
version = "0.1.0"
description = "Live audio transcription via Whisper"
edition.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
whisper-rs.workspace = true
reqwest.workspace = true
sdr-types.workspace = true
thiserror.workspace = true
tracing.workspace = true

[lints]
workspace = true
```

- [ ] **Step 3: Create lib.rs stub**

Create `crates/sdr-transcription/src/lib.rs`:

```rust
pub mod resampler;
```

- [ ] **Step 4: Write failing resampler tests**

Create `crates/sdr-transcription/src/resampler.rs`:

```rust
//! Resample 48 kHz interleaved stereo to 16 kHz mono.
//!
//! Simple 3:1 decimation with stereo-to-mono mix. No anti-aliasing
//! filter needed — radio audio bandwidth is well under 8 kHz.

/// Decimation factor: 48000 / 16000 = 3.
const DECIMATION_FACTOR: usize = 3;

/// Convert interleaved stereo f32 at 48 kHz to mono f32 at 16 kHz.
///
/// Input: `[L0, R0, L1, R1, L2, R2, ...]` at 48 kHz
/// Output: `[(L0+R0)/2, (L3+R3)/2, (L6+R6)/2, ...]` at 16 kHz
///
/// Each output sample is the mono mix of every 3rd stereo pair.
pub fn downsample_stereo_to_mono_16k(interleaved_48k: &[f32], output: &mut Vec<f32>) {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_produces_empty_output() {
        let mut out = Vec::new();
        downsample_stereo_to_mono_16k(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn single_stereo_pair_produces_one_sample() {
        // One stereo pair at index 0 → one output sample
        let input = [0.6_f32, 0.4];
        let mut out = Vec::new();
        downsample_stereo_to_mono_16k(&input, &mut out);
        assert_eq!(out.len(), 1);
        assert!((out[0] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn three_pairs_produce_one_sample() {
        // 3 stereo pairs → take only index 0 (decimation factor 3)
        let input = [0.6, 0.4, 0.1, 0.1, 0.2, 0.2];
        let mut out = Vec::new();
        downsample_stereo_to_mono_16k(&input, &mut out);
        assert_eq!(out.len(), 1);
        assert!((out[0] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn six_pairs_produce_two_samples() {
        // 6 stereo pairs → 2 output samples (indices 0 and 3)
        let input = [
            1.0, 0.0, // pair 0: mono = 0.5
            0.0, 0.0, // pair 1: skipped
            0.0, 0.0, // pair 2: skipped
            0.0, 1.0, // pair 3: mono = 0.5
            0.0, 0.0, // pair 4: skipped
            0.0, 0.0, // pair 5: skipped
        ];
        let mut out = Vec::new();
        downsample_stereo_to_mono_16k(&input, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.5).abs() < f32::EPSILON);
        assert!((out[1] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn output_is_appended_not_replaced() {
        let input = [0.8, 0.2];
        let mut out = vec![99.0];
        downsample_stereo_to_mono_16k(&input, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 99.0).abs() < f32::EPSILON);
        assert!((out[1] - 0.5).abs() < f32::EPSILON);
    }
}
```

- [ ] **Step 5: Run tests to verify they fail**

Run: `cargo test -p sdr-transcription`
Expected: FAIL — `todo!()` panics

- [ ] **Step 6: Implement resampler**

Replace `todo!()` in `downsample_stereo_to_mono_16k`:

```rust
pub fn downsample_stereo_to_mono_16k(interleaved_48k: &[f32], output: &mut Vec<f32>) {
    // Each stereo pair is 2 floats. Step by DECIMATION_FACTOR pairs.
    let pair_count = interleaved_48k.len() / 2;
    let mut pair_idx = 0;
    while pair_idx < pair_count {
        let l = interleaved_48k[pair_idx * 2];
        let r = interleaved_48k[pair_idx * 2 + 1];
        output.push((l + r) / 2.0);
        pair_idx += DECIMATION_FACTOR;
    }
}
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p sdr-transcription`
Expected: All 5 tests PASS

- [ ] **Step 8: Clippy**

Run: `cargo clippy -p sdr-transcription -- -D warnings`
Expected: Clean

- [ ] **Step 9: Commit**

```bash
git add crates/sdr-transcription/ Cargo.toml Cargo.lock
git commit -m "add sdr-transcription crate with 48kHz→16kHz resampler"
```

---

## Task 2: Model Download + Path Management

**Files:**
- Create: `crates/sdr-transcription/src/model.rs`
- Modify: `crates/sdr-transcription/src/lib.rs`

- [ ] **Step 1: Create model.rs**

Create `crates/sdr-transcription/src/model.rs`:

```rust
//! Whisper model download and path management.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// Whisper tiny English GGML model URL (from ggerganov/whisper.cpp on HuggingFace).
const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin";

/// Model filename.
const MODEL_FILENAME: &str = "ggml-tiny.en.bin";

/// Returns the directory for storing models.
///
/// Uses `$XDG_DATA_HOME/sdr-rs/models/` (typically `~/.local/share/sdr-rs/models/`).
pub fn models_dir() -> PathBuf {
    let data_dir = dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sdr-rs")
        .join("models");
    data_dir
}

/// Returns the full path to the Whisper tiny English model.
pub fn model_path() -> PathBuf {
    models_dir().join(MODEL_FILENAME)
}

/// Check if the model file exists.
pub fn model_exists() -> bool {
    model_path().is_file()
}

/// Download the Whisper tiny English model, sending progress events.
///
/// Blocks until download completes. Sends `progress_pct` (0..100) to the
/// provided sender.
#[allow(clippy::cast_possible_truncation)]
pub fn download_model(
    progress_tx: &mpsc::Sender<u8>,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let dir = models_dir();
    std::fs::create_dir_all(&dir)?;

    let dest = dir.join(MODEL_FILENAME);
    tracing::info!(?dest, "downloading Whisper model");

    let response = reqwest::blocking::get(MODEL_URL)?;
    let total_size = response.content_length().unwrap_or(0);

    let mut file = std::fs::File::create(&dest)?;
    let mut downloaded: u64 = 0;
    let mut last_pct: u8 = 0;

    let mut reader = response;
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let bytes_read = std::io::Read::read(&mut reader, &mut buf)?;
        if bytes_read == 0 {
            break;
        }
        file.write_all(&buf[..bytes_read])?;
        downloaded += bytes_read as u64;

        if total_size > 0 {
            let pct = ((downloaded * 100) / total_size).min(100) as u8;
            if pct != last_pct {
                last_pct = pct;
                let _ = progress_tx.send(pct);
            }
        }
    }

    file.flush()?;
    tracing::info!(?dest, bytes = downloaded, "model download complete");

    Ok(dest)
}
```

- [ ] **Step 2: Add `dirs-next` dependency**

Add to `Cargo.toml` workspace dependencies:

```toml
dirs-next = "2"
```

Add to `crates/sdr-transcription/Cargo.toml` dependencies:

```toml
dirs-next.workspace = true
```

- [ ] **Step 3: Update lib.rs**

```rust
pub mod model;
pub mod resampler;
```

- [ ] **Step 4: Build and clippy**

Run: `cargo build -p sdr-transcription && cargo clippy -p sdr-transcription -- -D warnings`
Expected: Clean

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-transcription/ Cargo.toml Cargo.lock
git commit -m "add Whisper model download and path management"
```

---

## Task 3: Whisper Worker Thread

**Files:**
- Create: `crates/sdr-transcription/src/worker.rs`
- Modify: `crates/sdr-transcription/src/lib.rs`

- [ ] **Step 1: Create worker.rs**

Create `crates/sdr-transcription/src/worker.rs`:

```rust
//! Background worker thread for audio transcription.
//!
//! Receives interleaved stereo f32 at 48 kHz, resamples to 16 kHz mono,
//! accumulates chunks, and runs Whisper inference when enough audio has
//! accumulated.

use std::sync::mpsc;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::model;
use crate::resampler;

/// How many seconds of 16 kHz audio to accumulate before transcribing.
const CHUNK_SECONDS: usize = 5;

/// 16 kHz * 5 seconds = 80,000 samples per chunk.
const CHUNK_SAMPLES: usize = 16_000 * CHUNK_SECONDS;

/// Minimum RMS energy to consider a chunk as containing speech.
/// Below this threshold the chunk is treated as silence and skipped.
const SILENCE_THRESHOLD: f32 = 0.005;

/// Events sent from the worker to the UI.
#[derive(Debug, Clone)]
pub enum TranscriptionEvent {
    /// Model is being downloaded.
    Downloading { progress_pct: u8 },
    /// Model loaded, worker is listening for speech.
    Ready,
    /// A transcribed text segment.
    Text { timestamp: String, text: String },
    /// An error occurred.
    Error(String),
}

/// Run the transcription worker loop.
///
/// This function blocks and should be called from a dedicated thread.
/// It exits when the `audio_rx` channel is closed (sender dropped).
#[allow(clippy::too_many_lines)]
pub fn run_worker(
    audio_rx: mpsc::Receiver<Vec<f32>>,
    event_tx: mpsc::Sender<TranscriptionEvent>,
) {
    // ── Model download / load ───────────────────────────────────────
    let model_path = if model::model_exists() {
        model::model_path()
    } else {
        let (progress_tx, progress_rx) = mpsc::channel();

        // Forward progress events
        let event_tx_dl = event_tx.clone();
        let progress_thread = std::thread::spawn(move || {
            while let Ok(pct) = progress_rx.recv() {
                let _ = event_tx_dl.send(TranscriptionEvent::Downloading {
                    progress_pct: pct,
                });
            }
        });

        match model::download_model(&progress_tx) {
            Ok(path) => {
                drop(progress_tx);
                let _ = progress_thread.join();
                path
            }
            Err(e) => {
                let _ = event_tx.send(TranscriptionEvent::Error(format!(
                    "model download failed: {e}"
                )));
                return;
            }
        }
    };

    // Load whisper context
    let ctx = match WhisperContext::new_with_params(
        model_path.to_str().unwrap_or_default(),
        WhisperContextParameters::default(),
    ) {
        Ok(ctx) => ctx,
        Err(e) => {
            let _ = event_tx.send(TranscriptionEvent::Error(format!(
                "failed to load Whisper model: {e}"
            )));
            return;
        }
    };

    let mut state = match ctx.create_state() {
        Ok(s) => s,
        Err(e) => {
            let _ = event_tx.send(TranscriptionEvent::Error(format!(
                "failed to create Whisper state: {e}"
            )));
            return;
        }
    };

    let _ = event_tx.send(TranscriptionEvent::Ready);
    tracing::info!("transcription worker ready");

    // ── Audio accumulation + inference loop ──────────────────────────
    let mut mono_buf: Vec<f32> = Vec::with_capacity(CHUNK_SAMPLES);
    let mut resample_buf: Vec<f32> = Vec::new();

    while let Ok(interleaved) = audio_rx.recv() {
        // Resample 48 kHz stereo → 16 kHz mono, appending to resample_buf
        resample_buf.clear();
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut resample_buf);
        mono_buf.extend_from_slice(&resample_buf);

        // When we have enough audio, transcribe
        if mono_buf.len() >= CHUNK_SAMPLES {
            // Check energy — skip silence
            let rms = compute_rms(&mono_buf[..CHUNK_SAMPLES]);
            if rms < SILENCE_THRESHOLD {
                tracing::trace!(rms, "skipping silent chunk");
                mono_buf.drain(..CHUNK_SAMPLES);
                continue;
            }

            // Run Whisper inference
            let chunk: Vec<f32> = mono_buf.drain(..CHUNK_SAMPLES).collect();
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some("en"));
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            params.set_no_context(true);

            match state.full(params, &chunk) {
                Ok(()) => {
                    let n_segments = state.full_n_segments().unwrap_or(0);
                    let mut full_text = String::new();
                    for i in 0..n_segments {
                        if let Ok(text) = state.full_get_segment_text(i) {
                            let trimmed = text.trim();
                            if !trimmed.is_empty() {
                                if !full_text.is_empty() {
                                    full_text.push(' ');
                                }
                                full_text.push_str(trimmed);
                            }
                        }
                    }
                    if !full_text.is_empty() {
                        let timestamp = chrono_timestamp();
                        let _ = event_tx.send(TranscriptionEvent::Text {
                            timestamp,
                            text: full_text,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!("whisper inference failed: {e}");
                }
            }
        }
    }

    tracing::info!("transcription worker exiting");
}

/// Compute RMS energy of an audio buffer.
fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

/// Format the current wall clock time as HH:MM:SS.
fn chrono_timestamp() -> String {
    let now = std::time::SystemTime::now();
    let since_midnight = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs_today = since_midnight % 86400;
    // Adjust for local timezone offset
    let local_offset = {
        // Use libc localtime to get the real offset
        #[cfg(unix)]
        {
            let t = since_midnight as libc::time_t;
            let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
            // SAFETY: localtime_r is thread-safe and writes into our buffer.
            #[allow(unsafe_code)]
            unsafe {
                libc::localtime_r(&t, tm.as_mut_ptr());
                (*tm.as_ptr()).tm_gmtoff
            }
        }
        #[cfg(not(unix))]
        {
            0i64
        }
    };
    #[allow(clippy::cast_sign_loss)]
    let local_secs = (secs_today as i64 + local_offset).rem_euclid(86400) as u64;
    let h = local_secs / 3600;
    let m = (local_secs % 3600) / 60;
    let s = local_secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_is_zero() {
        assert!((compute_rms(&[0.0; 100]) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rms_of_ones_is_one() {
        assert!((compute_rms(&[1.0; 100]) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rms_of_empty_is_zero() {
        assert!((compute_rms(&[]) - 0.0).abs() < f32::EPSILON);
    }
}
```

- [ ] **Step 2: Add libc dependency**

Add to `Cargo.toml` workspace dependencies:

```toml
libc = "0.2"
```

Add to `crates/sdr-transcription/Cargo.toml` dependencies:

```toml
libc.workspace = true
```

- [ ] **Step 3: Update lib.rs**

```rust
pub mod model;
pub mod resampler;
pub mod worker;

pub use worker::TranscriptionEvent;
```

- [ ] **Step 4: Build and test**

Run: `cargo build -p sdr-transcription && cargo test -p sdr-transcription && cargo clippy -p sdr-transcription -- -D warnings`
Expected: All tests pass, clippy clean

Note: The first build will compile `whisper.cpp` from source via `whisper-rs-sys`. This requires `cmake` and a C++ compiler. If the build fails, install: `sudo pacman -S cmake` (Arch) or `sudo apt install cmake` (Ubuntu).

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-transcription/ Cargo.toml Cargo.lock
git commit -m "add Whisper worker thread with audio accumulation and silence detection"
```

---

## Task 4: TranscriptionEngine Public API

**Files:**
- Modify: `crates/sdr-transcription/src/lib.rs`

- [ ] **Step 1: Implement TranscriptionEngine**

Replace `crates/sdr-transcription/src/lib.rs`:

```rust
//! Live audio transcription via Whisper.
//!
//! Provides `TranscriptionEngine` that runs a background worker thread for
//! speech-to-text. Audio samples are fed from the DSP thread via a bounded
//! channel; transcription results are returned via an event channel.

pub mod model;
pub mod resampler;
pub mod worker;

pub use worker::TranscriptionEvent;

use std::sync::mpsc;

/// Bounded channel capacity for audio buffers from DSP → transcription.
/// Each buffer is ~1024-4096 stereo samples (~20-80ms). 10 buffers gives
/// ~800ms of headroom before drops.
const AUDIO_CHANNEL_CAPACITY: usize = 10;

/// Error type for transcription operations.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptionError {
    #[error("transcription is already running")]
    AlreadyRunning,
    #[error("transcription is not running")]
    NotRunning,
}

/// Live audio transcription engine.
///
/// Create with `new()`, call `start()` to begin, `stop()` to end.
/// Feed audio via `audio_sender()` and receive results from the
/// `Receiver<TranscriptionEvent>` returned by `start()`.
pub struct TranscriptionEngine {
    audio_tx: Option<mpsc::SyncSender<Vec<f32>>>,
    worker_thread: Option<std::thread::JoinHandle<()>>,
}

impl TranscriptionEngine {
    /// Create a new engine. Does not start the worker thread.
    pub fn new() -> Self {
        Self {
            audio_tx: None,
            worker_thread: None,
        }
    }

    /// Start the transcription worker thread.
    ///
    /// Returns a receiver for `TranscriptionEvent`s. The worker will
    /// download the model on first run if needed.
    pub fn start(
        &mut self,
    ) -> Result<mpsc::Receiver<TranscriptionEvent>, TranscriptionError> {
        if self.worker_thread.is_some() {
            return Err(TranscriptionError::AlreadyRunning);
        }

        let (audio_tx, audio_rx) = mpsc::sync_channel(AUDIO_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();

        let handle = std::thread::Builder::new()
            .name("transcription-worker".into())
            .spawn(move || {
                worker::run_worker(audio_rx, event_tx);
            })
            .expect("failed to spawn transcription worker thread");

        self.audio_tx = Some(audio_tx);
        self.worker_thread = Some(handle);

        tracing::info!("transcription engine started");
        Ok(event_rx)
    }

    /// Stop the transcription worker.
    pub fn stop(&mut self) {
        // Drop the audio sender to signal the worker to exit.
        self.audio_tx.take();

        if let Some(handle) = self.worker_thread.take() {
            let _ = handle.join();
        }

        tracing::info!("transcription engine stopped");
    }

    /// Get a clone of the audio sender for feeding samples from the DSP thread.
    ///
    /// Returns `None` if the engine is not running.
    pub fn audio_sender(&self) -> Option<mpsc::SyncSender<Vec<f32>>> {
        self.audio_tx.clone()
    }

    /// Check if the engine is currently running.
    pub fn is_running(&self) -> bool {
        self.worker_thread.is_some()
    }
}

impl Drop for TranscriptionEngine {
    fn drop(&mut self) {
        self.stop();
    }
}
```

- [ ] **Step 2: Build and clippy**

Run: `cargo build -p sdr-transcription && cargo clippy -p sdr-transcription -- -D warnings`
Expected: Clean

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-transcription/src/lib.rs
git commit -m "add TranscriptionEngine public API"
```

---

## Task 5: Audio Tap in DSP Controller

**Files:**
- Modify: `crates/sdr-ui/src/messages.rs` (add UiToDsp variants)
- Modify: `crates/sdr-ui/src/dsp_controller.rs` (add audio tap)
- Modify: `crates/sdr-ui/Cargo.toml` (add sdr-transcription dep)

- [ ] **Step 1: Add sdr-transcription dependency to sdr-ui**

In `crates/sdr-ui/Cargo.toml`, add:

```toml
sdr-transcription.workspace = true
```

- [ ] **Step 2: Add UiToDsp messages**

In `crates/sdr-ui/src/messages.rs`, add these variants to the `UiToDsp` enum:

```rust
/// Start sending audio to the transcription engine.
EnableTranscription(std::sync::mpsc::SyncSender<Vec<f32>>),
/// Stop sending audio to the transcription engine.
DisableTranscription,
```

- [ ] **Step 3: Add transcription sender to DspState**

In `crates/sdr-ui/src/dsp_controller.rs`, add a field to `DspState`:

```rust
/// Transcription audio tap — when Some, audio is copied to this channel.
transcription_tx: Option<std::sync::mpsc::SyncSender<Vec<f32>>>,
```

Initialize it as `None` in `DspState::new()`.

- [ ] **Step 4: Handle UiToDsp messages in the DSP loop**

In the message handling section of the DSP loop (where `UiToDsp` variants are matched), add:

```rust
UiToDsp::EnableTranscription(tx) => {
    state.transcription_tx = Some(tx);
    tracing::info!("transcription audio tap enabled");
}
UiToDsp::DisableTranscription => {
    state.transcription_tx = None;
    tracing::info!("transcription audio tap disabled");
}
```

- [ ] **Step 5: Add audio tap after write_samples**

In `process_iq_block()`, right after the `state.audio_sink.write_samples()` call (around line 1039), add the transcription tap:

```rust
// Send audio copy to transcription worker (non-blocking).
if let Some(ref tx) = state.transcription_tx {
    let _ = tx.try_send(state.interleave_buf.clone());
}
```

Wait — the interleave buffer is inside AudioSink. We need to tap the `audio_buf` (Stereo samples) and interleave ourselves. Let me adjust.

Actually, the simpler approach: tap the audio buffer BEFORE it enters AudioSink and interleave for the transcription channel:

```rust
// Send audio copy to transcription worker (non-blocking).
if let Some(ref tx) = state.transcription_tx {
    let mut interleaved = Vec::with_capacity(audio_count * 2);
    for s in &state.audio_buf[..audio_count] {
        interleaved.push(s.l);
        interleaved.push(s.r);
    }
    let _ = tx.try_send(interleaved);
}
```

Add this BEFORE the `state.audio_sink.write_samples()` call.

- [ ] **Step 6: Build and clippy**

Run: `cargo build --workspace && cargo clippy --all-targets --workspace -- -D warnings`
Expected: Clean

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-ui/Cargo.toml crates/sdr-ui/src/messages.rs crates/sdr-ui/src/dsp_controller.rs
git commit -m "add transcription audio tap to DSP controller"
```

---

## Task 6: Transcript Sidebar Panel

**Files:**
- Create: `crates/sdr-ui/src/sidebar/transcript_panel.rs`
- Modify: `crates/sdr-ui/src/sidebar/mod.rs`

- [ ] **Step 1: Create transcript_panel.rs**

Create `crates/sdr-ui/src/sidebar/transcript_panel.rs`:

```rust
//! Transcript sidebar panel — displays live transcription results.

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

/// Transcript panel UI elements.
pub struct TranscriptPanel {
    /// The top-level widget for the sidebar.
    pub widget: adw::PreferencesGroup,
    /// Toggle to enable/disable transcription.
    pub enable_row: adw::SwitchRow,
    /// Status label ("Listening...", "Downloading...", error).
    pub status_label: gtk4::Label,
    /// Progress bar for model download.
    pub progress_bar: gtk4::ProgressBar,
    /// Scrolled text view for the transcript log.
    pub text_view: gtk4::TextView,
    /// Scroll container for the text view.
    pub scroll: gtk4::ScrolledWindow,
    /// Clear button.
    pub clear_button: gtk4::Button,
}

/// Build the transcript sidebar panel.
pub fn build_transcript_panel() -> TranscriptPanel {
    let group = adw::PreferencesGroup::builder()
        .title("Transcript")
        .description("Live speech-to-text")
        .build();

    // Enable/disable toggle
    let enable_row = adw::SwitchRow::builder()
        .title("Enable Transcription")
        .subtitle("Whisper tiny (English)")
        .build();
    group.add(&enable_row);

    // Status label
    let status_label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .css_classes(["dim-label"])
        .visible(false)
        .margin_start(12)
        .margin_top(4)
        .build();

    // Progress bar (for model download)
    let progress_bar = gtk4::ProgressBar::builder()
        .visible(false)
        .margin_start(12)
        .margin_end(12)
        .margin_top(4)
        .build();

    // Transcript text view
    let text_view = gtk4::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk4::WrapMode::Word)
        .monospace(true)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();

    let scroll = gtk4::ScrolledWindow::builder()
        .child(&text_view)
        .min_content_height(150)
        .max_content_height(300)
        .css_classes(["card"])
        .margin_top(8)
        .build();

    // Clear button
    let clear_button = gtk4::Button::builder()
        .label("Clear")
        .halign(gtk4::Align::Start)
        .margin_top(4)
        .build();

    let text_view_clear = text_view.clone();
    clear_button.connect_clicked(move |_| {
        text_view_clear.buffer().set_text("");
    });

    // Build layout — status, progress, scroll, and clear go into a vertical box
    let content_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(4)
        .build();
    content_box.append(&status_label);
    content_box.append(&progress_bar);
    content_box.append(&scroll);
    content_box.append(&clear_button);
    group.add(&content_box);

    TranscriptPanel {
        widget: group,
        enable_row,
        status_label,
        progress_bar,
        text_view,
        scroll,
        clear_button,
    }
}
```

- [ ] **Step 2: Add to sidebar mod.rs**

In `crates/sdr-ui/src/sidebar/mod.rs`:

1. Add module declaration:

```rust
pub mod transcript_panel;
```

2. Add to exports:

```rust
pub use transcript_panel::{TranscriptPanel, build_transcript_panel};
```

3. Add `transcript: TranscriptPanel` field to `SidebarPanels` struct.

4. In `build_sidebar()`, add:

```rust
let transcript = build_transcript_panel();
```

Append to the sidebar box (after display):

```rust
sidebar_box.append(&transcript.widget);
```

Add to the `SidebarPanels` struct construction:

```rust
transcript,
```

- [ ] **Step 3: Build and clippy**

Run: `cargo build --workspace && cargo clippy --all-targets --workspace -- -D warnings`
Expected: Clean

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/sidebar/transcript_panel.rs crates/sdr-ui/src/sidebar/mod.rs
git commit -m "add Transcript sidebar panel with toggle, log, and clear button"
```

---

## Task 7: Final Wiring — Connect Transcription to UI

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 1: Add connect_transcript_panel function**

In `crates/sdr-ui/src/window.rs`, add a new function after `connect_audio_panel`:

```rust
/// Wire the transcript panel: toggle enables/disables transcription,
/// poll for events to update the transcript log.
fn connect_transcript_panel(panels: &SidebarPanels, state: &Rc<AppState>) {
    use sdr_transcription::{TranscriptionEngine, TranscriptionEvent};

    let engine: Rc<RefCell<TranscriptionEngine>> =
        Rc::new(RefCell::new(TranscriptionEngine::new()));

    let state_clone = Rc::clone(state);
    let engine_clone = Rc::clone(&engine);
    let status_label = panels.transcript.status_label.clone();
    let progress_bar = panels.transcript.progress_bar.clone();
    let text_view = panels.transcript.text_view.clone();
    let scroll = panels.transcript.scroll.clone();

    panels.transcript.enable_row.connect_active_notify(move |row| {
        let mut eng = engine_clone.borrow_mut();

        if row.is_active() {
            // Start transcription
            match eng.start() {
                Ok(event_rx) => {
                    // Send audio sender to DSP thread
                    if let Some(audio_tx) = eng.audio_sender() {
                        state_clone.send_dsp(
                            crate::messages::UiToDsp::EnableTranscription(audio_tx),
                        );
                    }

                    status_label.set_text("Starting...");
                    status_label.set_visible(true);

                    // Poll for transcription events on a 100ms timer
                    let status = status_label.clone();
                    let progress = progress_bar.clone();
                    let tv = text_view.clone();
                    let sc = scroll.clone();

                    glib::timeout_add_local(
                        std::time::Duration::from_millis(100),
                        move || {
                            while let Ok(event) = event_rx.try_recv() {
                                match event {
                                    TranscriptionEvent::Downloading { progress_pct } => {
                                        status.set_text(&format!(
                                            "Downloading model ({progress_pct}%)..."
                                        ));
                                        status.set_visible(true);
                                        progress.set_fraction(f64::from(progress_pct) / 100.0);
                                        progress.set_visible(true);
                                    }
                                    TranscriptionEvent::Ready => {
                                        status.set_text("Listening...");
                                        status.set_css_classes(&["success"]);
                                        progress.set_visible(false);
                                    }
                                    TranscriptionEvent::Text { timestamp, text } => {
                                        let buf = tv.buffer();
                                        let mut end = buf.end_iter();
                                        buf.insert(
                                            &mut end,
                                            &format!("[{timestamp}] {text}\n"),
                                        );
                                        // Auto-scroll to bottom
                                        let mark = buf.create_mark(
                                            None,
                                            &buf.end_iter(),
                                            false,
                                        );
                                        tv.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
                                        buf.delete_mark(&mark);
                                    }
                                    TranscriptionEvent::Error(msg) => {
                                        status.set_text(&msg);
                                        status.set_css_classes(&["error"]);
                                    }
                                }
                            }
                            glib::ControlFlow::Continue
                        },
                    );
                }
                Err(e) => {
                    tracing::warn!("failed to start transcription: {e}");
                    row.set_active(false);
                }
            }
        } else {
            // Stop transcription
            state_clone.send_dsp(crate::messages::UiToDsp::DisableTranscription);
            eng.stop();
            status_label.set_text("");
            status_label.set_visible(false);
            progress_bar.set_visible(false);
        }
    });
}
```

- [ ] **Step 2: Call connect_transcript_panel from connect_sidebar_panels**

In `connect_sidebar_panels()`, add after `connect_audio_panel(panels, state)`:

```rust
connect_transcript_panel(panels, state);
```

- [ ] **Step 3: Build, clippy, test**

Run: `cargo build --workspace && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace`
Expected: All clean

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "wire transcript panel: toggle, event polling, auto-scroll log"
```

---

## Verification Checklist

After all tasks:

1. `cargo build --workspace` compiles (including whisper.cpp from source)
2. `cargo test --workspace` all tests pass
3. `cargo clippy --all-targets --workspace -- -D warnings` clean
4. Transcript panel appears in sidebar with toggle, text view, and clear button
5. Toggle on → status shows "Downloading model..." with progress bar (first run)
6. After download → status shows "Listening..."
7. Audio plays → transcribed text appears as timestamped lines in log
8. Silent periods are skipped (no empty transcriptions)
9. Clear button wipes the log
10. Toggle off → transcription stops, audio tap disconnected
11. No audio dropouts or stuttering when transcription is active
12. Model persists in `~/.local/share/sdr-rs/models/` between sessions
