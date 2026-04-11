# Live Transcription — Design Spec

## Overview

Real-time speech-to-text for demodulated radio audio using Whisper (via `whisper-rs`). Audio is tapped from the audio sink, resampled, and fed to a background transcription worker. Results appear in a new Transcript sidebar panel as timestamped log entries.

## Architecture

```text
sdr-transcription    — NEW crate: Whisper inference, audio buffering, VAD
sdr-sink-audio       — Audio tap (copy of samples to transcription channel)
sdr-ui               — Transcript sidebar panel
```

### Data Flow

```text
AudioSink::write_samples()
  ├→ PipeWire ring buffer (audio playback, unchanged)
  └→ try_send to transcription channel (non-blocking, drops if full)

Transcription worker thread:
  ← Receives interleaved f32 at 48 kHz stereo
  → Resample to 16 kHz mono (average L+R, decimate 3:1)
  → VAD accumulates until speech pause
  → Whisper inference on accumulated segment
  → Send TranscriptionEvent::Text to UI

UI thread:
  ← Receives TranscriptionEvent
  → Appends timestamped line to transcript panel
```

### Threading Model

| Thread | Role | Blocking allowed? |
|--------|------|--------------------|
| DSP | Copies audio to channel via `try_send` | No |
| Transcription worker | Accumulates audio, runs Whisper | Yes (dedicated thread) |
| UI | Displays transcript updates | No (GTK main loop) |

Audio playback is never affected by transcription — the tap is fire-and-forget.

## New Crate: `sdr-transcription`

### File Structure

```text
crates/sdr-transcription/src/
  lib.rs           — TranscriptionEngine public API
  worker.rs        — background thread: audio accumulation, VAD, inference
  resampler.rs     — 48 kHz stereo → 16 kHz mono conversion
  model.rs         — model download, path management, loading
```

### Dependencies

```toml
whisper-rs = "0.14"
reqwest = { workspace = true }         # model download (blocking)
sdr-types.workspace = true
tracing.workspace = true
thiserror.workspace = true
```

### Public API

```rust
pub struct TranscriptionEngine { ... }

impl TranscriptionEngine {
    /// Create a new engine (does not start worker or load model).
    pub fn new() -> Self;

    /// Start the transcription worker. Downloads model if needed.
    /// Returns a receiver for transcription events.
    pub fn start(&mut self) -> Result<std::sync::mpsc::Receiver<TranscriptionEvent>, TranscriptionError>;

    /// Stop the worker and release resources.
    pub fn stop(&mut self);

    /// Get the sender for feeding audio from the DSP thread.
    /// Returns None if not started.
    pub fn audio_sender(&self) -> Option<std::sync::mpsc::SyncSender<Vec<f32>>>;
}
```

### TranscriptionEvent

```rust
pub enum TranscriptionEvent {
    /// Model is being downloaded.
    Downloading { progress_pct: u8 },
    /// Model loaded, listening for speech.
    Ready,
    /// A transcribed utterance.
    Text { timestamp: String, text: String },
    /// An error occurred.
    Error(String),
}
```

### Worker Loop

1. Receive interleaved stereo f32 samples at 48 kHz from `SyncSender` channel
2. Resample to 16 kHz mono:
   - Average L and R channels: `(left + right) / 2.0`
   - Decimate 3:1 (48000 / 16000 = 3)
3. Feed to `whisper-rs` with VAD enabled — accumulates until speech pause
4. On speech end: run Whisper inference on the accumulated segment
5. Format timestamp from wall clock
6. Send `TranscriptionEvent::Text { timestamp, text }` to UI

### Resampler

Simple 3:1 decimation with averaging:
- Input: interleaved `[L, R, L, R, ...]` at 48 kHz
- Step 1: mono mix `(L + R) / 2.0` → mono at 48 kHz
- Step 2: take every 3rd sample → mono at 16 kHz

No anti-aliasing filter needed — Whisper's mel-spectrogram computation
handles the frequency content, and radio audio bandwidth is typically
well under 8 kHz (Nyquist at 16 kHz).

### Model Management

- **Storage:** `~/.local/share/sdr-rs/models/`
- **Model file:** `ggml-tiny.en.bin` (~75 MB)
- **Source:** `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin`
- **Download:** `reqwest::blocking::get()` with progress tracking via `Content-Length` header
- **First-run flow:** `start()` → check file exists → download if missing → load → `Ready`

## Audio Tap (`sdr-sink-audio`)

### Changes to AudioSink

Add an `Option<SyncSender<Vec<f32>>>` field to `AudioSink`. When set:
- After writing to the PipeWire ring buffer, clone the interleaved buffer
- `try_send` the clone to the transcription channel
- If channel is full, silently drop (no audio impact)

When `None` (transcription disabled): zero overhead, no allocation.

### Integration Point

`AudioSink::write_samples()` in `crates/sdr-sink-audio/src/pw_impl.rs` —
after the existing `ring.write(&self.interleave_buf)` call.

## Transcript Panel (`sdr-ui`)

### File

```text
crates/sdr-ui/src/sidebar/transcript_panel.rs
```

### Layout

```text
┌─ Transcript ─────────────────────┐
│ [Toggle: Enable Transcription]   │
│ Status: Listening...             │
│ ┌──────────────────────────────┐ │
│ │ [14:32:05] Officer respond...│ │
│ │ [14:32:12] Copy that, en ... │ │
│ │ [14:32:28] 10-4 received ... │ │
│ │                              │ │
│ └──────────────────────────────┘ │
│ [Clear]                          │
└──────────────────────────────────┘
```

### Components

- **Toggle switch** (`adw::SwitchRow`): Enable/Disable transcription
  - On enable: create `TranscriptionEngine`, call `start()`, hook audio tap
  - On disable: call `stop()`, disconnect audio tap
- **Status label**: Shows "Downloading model (42%)...", "Listening...", or error
- **Progress bar** (`gtk4::ProgressBar`): visible only during model download
- **Transcript log** (`gtk4::TextView`): read-only, monospace, auto-scroll to bottom
- **Clear button**: wipes the text buffer

### Event Handling

Poll `TranscriptionEvent` receiver on a GTK timer (100ms interval):
- `Downloading { pct }` → update progress bar + status
- `Ready` → hide progress bar, show "Listening..."
- `Text { timestamp, text }` → append `[{timestamp}] {text}\n` to text view, auto-scroll
- `Error(msg)` → show in status label with error styling

## V1 Scope

- English only, Whisper tiny model
- Auto-download on first enable
- No model picker, no language selector
- No 10-code interpretation
- No transcript export

## Dependencies (Workspace)

| Crate | Version | Used By | Purpose |
|-------|---------|---------|---------|
| whisper-rs | 0.14 | sdr-transcription | Whisper inference + VAD |

Note: `whisper-rs` statically links `whisper.cpp` (C++). Requires `cmake` and
a C++ compiler at build time (already available for GTK).
