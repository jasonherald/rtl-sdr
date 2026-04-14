# Auto Break Segmentation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a squelch-driven utterance segmentation mode ("Auto Break") for offline sherpa models on NFM demod, mutex with Silero VAD, and make any demod-mode change stop active transcription cleanly with an explainer toast.

**Architecture:** Replace the raw `mpsc::SyncSender<Vec<f32>>` audio-tap contract with a `TranscriptionInput` enum carrying both samples and squelch edge events. The DSP controller emits `SquelchOpened`/`SquelchClosed` edge events from the existing `IfChain::squelch_open()` gate, gated on `current_mode() == DemodMode::Nfm`. The offline sherpa session loop runs either Silero VAD (existing path) or a new Auto Break state machine (`Idle → Recording → HoldingOff`) based on `BackendConfig::segmentation_mode`. On any demod-mode change the transcript panel stops the active session and shows a toast; the setting is persisted so the user can click Start to resume on the new band.

**Tech Stack:** Rust 2024, `mpsc::sync_channel`, `gtk4-rs` 0.11 + `libadwaita`, `sherpa-onnx` 1.12 (fork pinned for sherpa-cuda), `serde_json` config.

**Spec:** [`docs/superpowers/specs/2026-04-13-auto-break-segmentation-design.md`](../specs/2026-04-13-auto-break-segmentation-design.md)

**Pre-plan correction vs. spec:** The spec called out adding a new `IfChain::is_squelch_configured()` accessor as a "small API addition." On reading `crates/sdr-radio/src/if_chain.rs:82`, the existing `pub fn squelch_enabled(&self) -> bool` accessor already returns exactly what we need — it's the flag set by `IfChain::set_squelch_enabled(bool)`, which the UI's `radio_panel.squelch_enabled_row` dispatches through `UiToDsp::SetSquelchEnabled`. The plan uses `squelch_enabled()` directly and adds no new API. The spec's "risk item 2" is retired as a no-op.

---

## File Structure

### Crates touched

| Crate | Files modified | What for |
|---|---|---|
| `sdr-transcription` | `src/backend.rs` | New `TranscriptionInput` enum, new `SegmentationMode` enum, `BackendConfig.segmentation_mode` field, `BackendHandle.audio_tx` type change |
| `sdr-transcription` | `src/lib.rs` | Re-export new public types |
| `sdr-transcription` | `src/backends/whisper.rs` | Pattern-match `TranscriptionInput::Samples`, ignore squelch variants |
| `sdr-transcription` | `src/backends/sherpa/host.rs` | `SessionParams.audio_rx` type change; add `segmentation_mode` to params; reject `AutoBreak` for online models |
| `sdr-transcription` | `src/backends/sherpa/mod.rs` | Pass `segmentation_mode` from `BackendConfig` through `SessionParams` |
| `sdr-transcription` | `src/backends/sherpa/streaming.rs` | Pattern-match `Samples`, ignore edge events |
| `sdr-transcription` | `src/backends/sherpa/offline.rs` | Split `run_session` into `run_session_vad` + `run_session_auto_break`; new state machine; new constants |
| `sdr-core` | `src/messages.rs` | New `DspToUi::DemodModeChanged(DemodMode)` variant; change `UiToDsp::EnableTranscription` payload type |
| `sdr-core` | `src/controller.rs` | `transcription_tx` type change; add `squelch_was_open: bool`; emit edge events on NFM; emit `DemodModeChanged` on demod switch |
| `sdr-ui` | `src/sidebar/transcript_panel.rs` | New `auto_break_row`; visibility rules; persistence key; mutex with VAD slider |
| `sdr-ui` | `src/sidebar/radio_panel.rs` | Subtitle on demod mode selector row |
| `sdr-ui` | `src/window.rs` | Subscribe to `DemodModeChanged`; stop-on-mode-change + toast; squelch-enabled precondition check; pass `segmentation_mode` when building `BackendConfig` |

### New files

None.

### Deleted files

None.

---

## Phase A: Foundation types

Add the new enums and refactor the audio-tap channel type to carry edge events alongside samples. Everything compiles after Phase B; nothing runs correctly yet.

### Task 1: Add `TranscriptionInput` enum and `SegmentationMode` to `sdr-transcription/src/backend.rs`

**Files:**
- Modify: `crates/sdr-transcription/src/backend.rs`
- Modify: `crates/sdr-transcription/src/lib.rs`

- [ ] **Step 1: Add `TranscriptionInput` and `SegmentationMode` to backend.rs**

At `crates/sdr-transcription/src/backend.rs` just above the existing `BackendConfig` struct (line 26):

```rust
/// Frames sent from the DSP controller into a transcription backend.
///
/// Carries both raw audio samples and segmentation-boundary hints. The
/// boundary variants are emitted by `sdr-core::controller` only when the
/// current demod mode is NFM — backends never need to gate on mode
/// themselves.
///
/// Backends that don't care about squelch-based segmentation (Whisper,
/// streaming Zipformer, offline sherpa in `SegmentationMode::Vad`)
/// pattern-match on `Samples` and drop the other variants.
#[derive(Debug, Clone)]
pub enum TranscriptionInput {
    /// Interleaved-stereo f32 PCM at 48 kHz. Always emitted, gap-free.
    Samples(Vec<f32>),

    /// Radio squelch just opened. Edge event, emitted exactly once per
    /// close→open transition. NFM demod only.
    SquelchOpened,

    /// Radio squelch just closed. Edge event, emitted exactly once per
    /// open→close transition. NFM demod only.
    SquelchClosed,
}

/// Which segmentation engine drives utterance boundaries for an offline
/// sherpa transcription session.
///
/// Mutex: exactly one is active per session. Streaming Zipformer always
/// uses `Vad` (its own endpoint detection handles the rest).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SegmentationMode {
    /// Silero VAD drives segmentation. Default for backward compatibility
    /// and the only valid mode for streaming Zipformer.
    #[default]
    Vad,

    /// Auto Break: the radio's squelch gate drives segmentation. Valid
    /// only for offline sherpa models on NFM demod. See the Auto Break
    /// state machine in `backends/sherpa/offline.rs`.
    AutoBreak,
}
```

- [ ] **Step 2: Add `segmentation_mode` field to `BackendConfig`**

Modify the existing `BackendConfig` struct at `crates/sdr-transcription/src/backend.rs:30-41`:

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackendConfig {
    pub model: ModelChoice,
    pub silence_threshold: f32,
    pub noise_gate_ratio: f32,
    /// Silero VAD speech detection threshold (offline models only).
    /// Clamp to `VAD_THRESHOLD_MIN..=VAD_THRESHOLD_MAX`.
    /// Default `VAD_THRESHOLD_DEFAULT`. Lower catches quieter audio
    /// (NFM/scanner); higher is stricter (talk radio). Ignored by
    /// Whisper (no Silero VAD) and ignored when
    /// `segmentation_mode == SegmentationMode::AutoBreak`.
    pub vad_threshold: f32,
    /// How utterance boundaries are detected in an offline sherpa
    /// session. See `SegmentationMode` for valid values. Streaming
    /// Zipformer rejects `AutoBreak` at session start.
    pub segmentation_mode: SegmentationMode,
}
```

- [ ] **Step 3: Change `BackendHandle.audio_tx` type to `SyncSender<TranscriptionInput>`**

Modify `crates/sdr-transcription/src/backend.rs:84-89`:

```rust
pub struct BackendHandle {
    /// Push audio frames + squelch edge events into the backend. See
    /// [`TranscriptionInput`] for the wire format.
    pub audio_tx: mpsc::SyncSender<TranscriptionInput>,
    /// Receive transcription events from the backend.
    pub event_rx: mpsc::Receiver<TranscriptionEvent>,
}
```

- [ ] **Step 4: Re-export the new types from lib.rs**

Modify `crates/sdr-transcription/src/lib.rs` — extend the `pub use backend::{...}` block (around line 45):

```rust
pub use backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, SegmentationMode,
    TranscriptionBackend, TranscriptionEvent, TranscriptionInput,
    VAD_THRESHOLD_DEFAULT, VAD_THRESHOLD_MAX, VAD_THRESHOLD_MIN,
};
```

- [ ] **Step 5: Write the unit test — new types are constructible and `SegmentationMode::default()` is `Vad`**

Append to `crates/sdr-transcription/src/backend.rs` (at the bottom, inside a new `#[cfg(test)] mod tests { ... }` block or an existing one if present):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcription_input_variants_construct() {
        let _samples = TranscriptionInput::Samples(vec![0.0_f32; 16]);
        let _opened = TranscriptionInput::SquelchOpened;
        let _closed = TranscriptionInput::SquelchClosed;
    }

    #[test]
    fn segmentation_mode_default_is_vad() {
        assert_eq!(SegmentationMode::default(), SegmentationMode::Vad);
    }
}
```

- [ ] **Step 6: Run tests**

```bash
cargo test -p sdr-transcription --lib backend::tests 2>&1 | tail -20
```

Expected: 2 tests pass.

The rest of the workspace WILL NOT COMPILE yet (the channel type change breaks every consumer). This is expected; Phase B fixes it.

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-transcription/src/backend.rs crates/sdr-transcription/src/lib.rs
git commit -m "feat(transcription): add TranscriptionInput + SegmentationMode types"
```

---

## Phase B: Refactor consumers so the workspace compiles again

The `TranscriptionInput` channel type change breaks every consumer. Fix them all in a single cohesive pass and verify the triple-build matrix still compiles before moving on.

### Task 2: Update Whisper backend to consume `TranscriptionInput`

**Files:**
- Modify: `crates/sdr-transcription/src/backends/whisper.rs`

- [ ] **Step 1: Change the channel type in `WhisperBackend::start`**

At `crates/sdr-transcription/src/backends/whisper.rs:79`:

```rust
let (audio_tx, audio_rx) = mpsc::sync_channel::<crate::backend::TranscriptionInput>(AUDIO_CHANNEL_CAPACITY);
```

- [ ] **Step 2: Change the receiver parameter types in `run_worker` and `run_worker_inner`**

At lines 131-138 and 154-161, change both signatures:

```rust
fn run_worker(
    audio_rx: &mpsc::Receiver<crate::backend::TranscriptionInput>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    cancel: &Arc<AtomicBool>,
    model: model::WhisperModel,
    silence_threshold: f32,
    noise_gate_ratio: f32,
) {
```

And identically for `run_worker_inner`.

- [ ] **Step 3: Pattern-match `Samples` in the audio loop, drop edge events**

At `crates/sdr-transcription/src/backends/whisper.rs:216-220`:

```rust
let interleaved = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
    Ok(crate::backend::TranscriptionInput::Samples(data)) => data,
    Ok(crate::backend::TranscriptionInput::SquelchOpened)
    | Ok(crate::backend::TranscriptionInput::SquelchClosed) => {
        // Whisper doesn't use squelch edge events (no Auto Break support
        // in v1). Dropping the frame and continuing the loop returns to
        // the recv on the next iteration.
        continue;
    }
    Err(mpsc::RecvTimeoutError::Timeout) => continue,
    Err(mpsc::RecvTimeoutError::Disconnected) => break,
};
```

Also update the `while let Ok(extra) = audio_rx.try_recv()` drain loop at 226-232 to handle the enum:

```rust
while let Ok(input) = audio_rx.try_recv() {
    if cancel.load(Ordering::Relaxed) {
        tracing::info!("transcription cancelled, worker exiting");
        return Ok(());
    }
    match input {
        crate::backend::TranscriptionInput::Samples(extra) => {
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }
        crate::backend::TranscriptionInput::SquelchOpened
        | crate::backend::TranscriptionInput::SquelchClosed => {
            // no-op: Whisper ignores squelch events
        }
    }
}
```

- [ ] **Step 4: Build check (whisper default)**

```bash
cargo build -p sdr-transcription 2>&1 | tail -10
```

Expected: compiles clean. If any use of `audio_rx` returns `Vec<f32>` directly, it will fail — find and pattern-match all of them.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-transcription/src/backends/whisper.rs
git commit -m "refactor(whisper): consume TranscriptionInput, ignore squelch variants"
```

### Task 3: Update Sherpa session params and host to thread `TranscriptionInput`

**Files:**
- Modify: `crates/sdr-transcription/src/backends/sherpa/host.rs`
- Modify: `crates/sdr-transcription/src/backends/sherpa/mod.rs`

- [ ] **Step 1: Change `SessionParams.audio_rx` type and add `segmentation_mode` field**

At `crates/sdr-transcription/src/backends/sherpa/host.rs:137-146`:

```rust
/// Parameters handed to the host worker for one transcription session.
pub(super) struct SessionParams {
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
    pub audio_rx: mpsc::Receiver<crate::backend::TranscriptionInput>,
    pub event_tx: mpsc::Sender<TranscriptionEvent>,
    pub noise_gate_ratio: f32,
    /// Silero VAD threshold requested for this session (offline VAD mode only).
    /// The worker rebuilds the VAD if this differs from the currently-held
    /// VAD's threshold. Ignored when `segmentation_mode == AutoBreak`.
    pub vad_threshold: f32,
    /// Which segmentation engine drives utterance boundaries. Validated
    /// against the model kind at the top of `run_host_loop`'s `StartSession`
    /// arm — streaming online models reject `AutoBreak`.
    pub segmentation_mode: crate::backend::SegmentationMode,
}
```

- [ ] **Step 2: Thread `segmentation_mode` through `SherpaBackend::start` in sherpa/mod.rs**

At `crates/sdr-transcription/src/backends/sherpa/mod.rs:108` (inside `TranscriptionBackend::start`'s body where `SessionParams { ... }` is constructed), change:

```rust
host.start_session(SessionParams {
    cancel: Arc::clone(&self.cancel),
    audio_rx,
    event_tx,
    noise_gate_ratio: config.noise_gate_ratio,
    vad_threshold: config.vad_threshold,
    segmentation_mode: config.segmentation_mode,
})?;
```

Also change the channel creation above it:

```rust
let (audio_tx, audio_rx) = mpsc::sync_channel::<crate::backend::TranscriptionInput>(AUDIO_CHANNEL_CAPACITY);
```

- [ ] **Step 3: Build check (sherpa-cpu)**

```bash
cargo build -p sdr-transcription --no-default-features --features sherpa-cpu 2>&1 | tail -10
```

Expected: fails in `streaming.rs` and `offline.rs` because those session loops still try to consume `Vec<f32>` directly. Those are Tasks 4 and 5; fix them next.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-transcription/src/backends/sherpa/host.rs \
        crates/sdr-transcription/src/backends/sherpa/mod.rs
git commit -m "refactor(sherpa): SessionParams carries TranscriptionInput + segmentation_mode"
```

### Task 4: Update streaming session loop to consume `TranscriptionInput` and reject `AutoBreak`

**Files:**
- Modify: `crates/sdr-transcription/src/backends/sherpa/streaming.rs`

- [ ] **Step 1: Reject `SegmentationMode::AutoBreak` at session start**

At the top of `run_session` in `crates/sdr-transcription/src/backends/sherpa/streaming.rs:71`, after the `SessionParams` destructure, add:

```rust
pub(super) fn run_session(recognizer: &OnlineRecognizer, params: SessionParams) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
        vad_threshold: _,
        segmentation_mode,
    } = params;

    if segmentation_mode == crate::backend::SegmentationMode::AutoBreak {
        let msg = "streaming Zipformer does not support Auto Break segmentation \
                   — it has its own endpoint detection. Use SegmentationMode::Vad.";
        tracing::error!(%msg);
        let _ = event_tx.send(TranscriptionEvent::Error(msg.to_owned()));
        return;
    }

    let stream = recognizer.create_stream();
    // ... existing code unchanged from here ...
```

- [ ] **Step 2: Pattern-match `Samples` in the recv loop**

At `crates/sdr-transcription/src/backends/sherpa/streaming.rs:96-100`:

```rust
let interleaved = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
    Ok(crate::backend::TranscriptionInput::Samples(data)) => data,
    Ok(crate::backend::TranscriptionInput::SquelchOpened)
    | Ok(crate::backend::TranscriptionInput::SquelchClosed) => continue,
    Err(mpsc::RecvTimeoutError::Timeout) => continue,
    Err(mpsc::RecvTimeoutError::Disconnected) => break,
};
```

And update the `try_recv` drain loop at line 105-111:

```rust
while let Ok(input) = audio_rx.try_recv() {
    if cancel.load(Ordering::Relaxed) {
        finalize_session(recognizer, &stream, &last_partial, &event_tx);
        return;
    }
    match input {
        crate::backend::TranscriptionInput::Samples(extra) => {
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }
        crate::backend::TranscriptionInput::SquelchOpened
        | crate::backend::TranscriptionInput::SquelchClosed => {
            // no-op for streaming — endpoint detection handles boundaries
        }
    }
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-transcription/src/backends/sherpa/streaming.rs
git commit -m "refactor(sherpa-streaming): consume TranscriptionInput, reject AutoBreak mode"
```

### Task 5: Update offline session loop to consume `TranscriptionInput` (VAD path only, no Auto Break yet)

**Files:**
- Modify: `crates/sdr-transcription/src/backends/sherpa/offline.rs`

- [ ] **Step 1: Pattern-match in the VAD recv loop**

At `crates/sdr-transcription/src/backends/sherpa/offline.rs:158-164`, destructure `segmentation_mode` (unused in VAD path):

```rust
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
        vad_threshold: _,
        segmentation_mode: _, // handled later in Task 10
    } = params;
```

At lines 182-186:

```rust
let interleaved = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
    Ok(crate::backend::TranscriptionInput::Samples(data)) => data,
    Ok(crate::backend::TranscriptionInput::SquelchOpened)
    | Ok(crate::backend::TranscriptionInput::SquelchClosed) => continue,
    Err(mpsc::RecvTimeoutError::Timeout) => continue,
    Err(mpsc::RecvTimeoutError::Disconnected) => break,
};
```

At lines 192-198:

```rust
while let Ok(input) = audio_rx.try_recv() {
    if cancel.load(Ordering::Relaxed) {
        drain_vad_on_exit(recognizer, vad, &event_tx);
        return;
    }
    match input {
        crate::backend::TranscriptionInput::Samples(extra) => {
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }
        crate::backend::TranscriptionInput::SquelchOpened
        | crate::backend::TranscriptionInput::SquelchClosed => {
            // VAD path ignores squelch events; Task 10 adds the AutoBreak branch
        }
    }
}
```

- [ ] **Step 2: Build check (sherpa-cpu)**

```bash
cargo build -p sdr-transcription --no-default-features --features sherpa-cpu 2>&1 | tail -10
```

Expected: clean compile.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-transcription/src/backends/sherpa/offline.rs
git commit -m "refactor(sherpa-offline): consume TranscriptionInput in VAD path"
```

### Task 6: Update `sdr-core` controller `transcription_tx` type and `UiToDsp::EnableTranscription`

**Files:**
- Modify: `crates/sdr-core/src/messages.rs`
- Modify: `crates/sdr-core/src/controller.rs`

- [ ] **Step 1: Change `UiToDsp::EnableTranscription` payload type**

At `crates/sdr-core/src/messages.rs:128`:

```rust
EnableTranscription(std::sync::mpsc::SyncSender<sdr_transcription::TranscriptionInput>),
```

Also update the test around line 316-318:

```rust
#[test]
fn enable_transcription_message_constructs() {
    let (tx, _rx) = std::sync::mpsc::sync_channel::<sdr_transcription::TranscriptionInput>(1);
    let enable = UiToDsp::EnableTranscription(tx);
    assert!(matches!(enable, UiToDsp::EnableTranscription(_)));
}
```

- [ ] **Step 2: Change `DspState.transcription_tx` type**

At `crates/sdr-core/src/controller.rs:193`:

```rust
transcription_tx: Option<std::sync::mpsc::SyncSender<sdr_transcription::TranscriptionInput>>,
```

- [ ] **Step 3: Wrap sample sends in `TranscriptionInput::Samples`**

At `crates/sdr-core/src/controller.rs:1004-1020`:

```rust
// Send audio copy to transcription worker BEFORE volume
// scaling so recognition isn't affected by the volume knob.
if let Some(ref tx) = state.transcription_tx {
    let mut interleaved = Vec::with_capacity(audio_count * 2);
    for s in &state.audio_buf[..audio_count] {
        interleaved.push(s.l);
        interleaved.push(s.r);
    }
    if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) =
        tx.try_send(sdr_transcription::TranscriptionInput::Samples(interleaved))
    {
        state.transcription_tx = None;
        tracing::info!(
            "transcription receiver disconnected, disabling tap"
        );
    }
}
```

- [ ] **Step 4: Add the `sdr-transcription` dep to `sdr-core` if not already present**

Check `crates/sdr-core/Cargo.toml`:

```bash
grep sdr-transcription crates/sdr-core/Cargo.toml
```

If not present, add under `[dependencies]`:

```toml
sdr-transcription.workspace = true
```

(It's very likely already present since EnableTranscription is already a variant; just verify.)

- [ ] **Step 5: Build check — triple flavor**

```bash
cargo check --workspace 2>&1 | tail -5
cargo check --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -5
cargo check --workspace --no-default-features --features sherpa-cuda 2>&1 | tail -5
```

Expected: all three compile clean.

- [ ] **Step 6: Run existing tests to catch regressions**

```bash
cargo test --workspace 2>&1 | tail -10
```

Expected: all existing tests still pass.

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-core/src/messages.rs crates/sdr-core/src/controller.rs crates/sdr-core/Cargo.toml
git commit -m "refactor(sdr-core): transcription tap carries TranscriptionInput"
```

**Phase B checkpoint: the workspace now compiles on all feature flavors and all pre-existing tests pass, but nothing has NEW behavior yet.**

---

## Phase C: Controller emits squelch edge events and demod-mode-change events

### Task 7: Add `squelch_was_open: bool` to `DspState` and emit edge events on NFM

**Files:**
- Modify: `crates/sdr-core/src/controller.rs`

- [ ] **Step 1: Add the field to `DspState`**

At `crates/sdr-core/src/controller.rs` in the `DspState` struct definition (around line 180-260), add next to `transcription_tx`:

```rust
/// Last known squelch gate state, used to detect open/close edge
/// transitions so we only emit one `SquelchOpened` / `SquelchClosed`
/// event per transition instead of one per audio chunk. Initialized
/// to `false` (matches `IfChain`'s initial closed state).
squelch_was_open: bool,
```

And initialize it in the struct's `::new()` / `::default()` (around line 243):

```rust
squelch_was_open: false,
```

- [ ] **Step 2: Emit edge events on transitions, gated on NFM**

Modify the transcription-tap block at `crates/sdr-core/src/controller.rs:1004-1020`:

```rust
// Send audio copy to transcription worker BEFORE volume
// scaling so recognition isn't affected by the volume knob.
if let Some(ref tx) = state.transcription_tx {
    // Detect squelch edge transitions. We only care about them on NFM,
    // where a closed squelch means "no transmission" and the transition
    // is a meaningful segment boundary for Auto Break.
    let now_open = state.radio.if_chain().squelch_open();
    let mut send_error = false;

    if now_open != state.squelch_was_open
        && state.radio.current_mode() == sdr_types::DemodMode::Nfm
    {
        let edge = if now_open {
            sdr_transcription::TranscriptionInput::SquelchOpened
        } else {
            sdr_transcription::TranscriptionInput::SquelchClosed
        };
        if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) = tx.try_send(edge) {
            send_error = true;
        }
    }
    state.squelch_was_open = now_open;

    if !send_error {
        let mut interleaved = Vec::with_capacity(audio_count * 2);
        for s in &state.audio_buf[..audio_count] {
            interleaved.push(s.l);
            interleaved.push(s.r);
        }
        if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) =
            tx.try_send(sdr_transcription::TranscriptionInput::Samples(interleaved))
        {
            send_error = true;
        }
    }

    if send_error {
        state.transcription_tx = None;
        tracing::info!("transcription receiver disconnected, disabling tap");
    }
}
```

- [ ] **Step 3: Build check**

```bash
cargo check --workspace --no-default-features --features sherpa-cuda 2>&1 | tail -5
```

Expected: clean. If `state.radio.if_chain()` doesn't exist as a borrow, check the existing accessors on `RadioModule` — there may be a pass-through like `state.radio.squelch_open()`. Use whichever exists:

```bash
grep -n "squelch_open\|fn if_chain" crates/sdr-radio/src/lib.rs | head -10
```

Adjust the accessor call to match.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-core/src/controller.rs
git commit -m "feat(sdr-core): emit squelch edge events on NFM transitions"
```

### Task 8: Add `DspToUi::DemodModeChanged` and emit it on mode change

**Files:**
- Modify: `crates/sdr-core/src/messages.rs`
- Modify: `crates/sdr-core/src/controller.rs`

- [ ] **Step 1: Add the variant**

At `crates/sdr-core/src/messages.rs`, inside the `DspToUi` enum (around line 8-33), add:

```rust
    /// Demodulator mode changed. Emitted when `UiToDsp::SetDemodMode`
    /// actually changes the active demod mode. Used by the transcript
    /// panel to stop any active session (band change = session boundary)
    /// and to re-run Auto Break visibility rules on mode transitions.
    DemodModeChanged(DemodMode),
```

- [ ] **Step 2: Add a test for the new variant**

In the `#[cfg(test)] mod tests` block at the bottom of `messages.rs`:

```rust
#[test]
fn demod_mode_changed_message_constructs() {
    let m = DspToUi::DemodModeChanged(DemodMode::Nfm);
    assert!(matches!(m, DspToUi::DemodModeChanged(DemodMode::Nfm)));
}
```

- [ ] **Step 3: Emit the event from the controller when `SetDemodMode` actually changes the mode**

Find the `UiToDsp::SetDemodMode` handler in `crates/sdr-core/src/controller.rs`:

```bash
grep -n "SetDemodMode" crates/sdr-core/src/controller.rs
```

In the match arm for `UiToDsp::SetDemodMode(mode)`, after the existing `state.radio.set_mode(mode)` call (or equivalent), add:

```rust
UiToDsp::SetDemodMode(mode) => {
    let old_mode = state.radio.current_mode();
    state.radio.set_mode(mode);
    if old_mode != mode {
        let _ = dsp_tx.send(DspToUi::DemodModeChanged(mode));
    }
}
```

(Read the current handler first — the existing body may already have different logic; preserve it and just add the emission.)

- [ ] **Step 4: Run the new message test**

```bash
cargo test -p sdr-core --lib 2>&1 | tail -10
```

Expected: the new `demod_mode_changed_message_constructs` test passes alongside the existing `messages` tests.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-core/src/messages.rs crates/sdr-core/src/controller.rs
git commit -m "feat(sdr-core): DspToUi::DemodModeChanged event on mode transitions"
```

---

## Phase D: Auto Break state machine in the offline session loop

### Task 9: Add Auto Break constants to `offline.rs`

**Files:**
- Modify: `crates/sdr-transcription/src/backends/sherpa/offline.rs`

- [ ] **Step 1: Add the four constants near the top of the file**

At `crates/sdr-transcription/src/backends/sherpa/offline.rs`, after the existing `SHERPA_NUM_THREADS` / `NEMO_TRANSDUCER_MODEL_TYPE` consts (around line 44):

```rust
/// Squelch openings shorter than this are treated as noise spikes and
/// produce no segment. Chosen to exclude sub-syllable blips while still
/// catching short single-word transmissions ("copy").
const AUTO_BREAK_MIN_OPEN_MS: u32 = 100;

/// Continue buffering audio for this long after the squelch closes, so
/// the last syllable isn't chopped by a tight squelch-close timing.
/// Covers typical PowerSquelch fall time plus ~100 ms of spoken tail.
const AUTO_BREAK_TAIL_MS: u32 = 200;

/// Segments shorter than this are discarded instead of decoded.
/// Moonshine and Parakeet both hallucinate on sub-word fragments, so
/// dropping them is an accuracy improvement, not a loss.
const AUTO_BREAK_MIN_SEGMENT_MS: u32 = 400;

/// Safety cap: if squelch stays open longer than this, flush anyway.
/// Protects against pathological stuck-open situations (bad auto-squelch,
/// carrier jam, band opening) that would otherwise cause unbounded
/// memory growth in the segment buffer.
const AUTO_BREAK_MAX_SEGMENT_MS: u32 = 30_000;
```

- [ ] **Step 2: Commit**

```bash
git add crates/sdr-transcription/src/backends/sherpa/offline.rs
git commit -m "feat(sherpa-offline): add Auto Break timing constants"
```

### Task 10: Split offline `run_session` into VAD + Auto Break dispatch

**Files:**
- Modify: `crates/sdr-transcription/src/backends/sherpa/offline.rs`

- [ ] **Step 1: Rename current `run_session` to `run_session_vad` and add a new `run_session` dispatcher**

At `crates/sdr-transcription/src/backends/sherpa/offline.rs:153`, rename the existing function:

```rust
/// One offline transcription session. Dispatches to the VAD or Auto Break
/// implementation based on `params.segmentation_mode`.
pub(super) fn run_session(
    recognizer: &OfflineRecognizer,
    vad: &mut SherpaSileroVad,
    params: SessionParams,
) {
    match params.segmentation_mode {
        crate::backend::SegmentationMode::Vad => run_session_vad(recognizer, vad, params),
        crate::backend::SegmentationMode::AutoBreak => {
            run_session_auto_break(recognizer, params)
        }
    }
}

/// VAD-driven offline session. Unchanged from the pre-Auto-Break behavior;
/// feeds incoming audio into Silero, batch-decodes each segment Silero
/// emits, and ignores any squelch edge events in the stream.
fn run_session_vad(
    recognizer: &OfflineRecognizer,
    vad: &mut SherpaSileroVad,
    params: SessionParams,
) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
        vad_threshold: _,
        segmentation_mode: _,
    } = params;
    // ... paste the rest of the old run_session body here, unchanged ...
```

- [ ] **Step 2: Build check**

```bash
cargo check -p sdr-transcription --no-default-features --features sherpa-cpu 2>&1 | tail -5
```

Expected: fails — `run_session_auto_break` is not yet defined. Task 11 fixes it.

- [ ] **Step 3: Commit (even though it doesn't build yet — short-lived intermediate state)**

Skip commit here; Task 11 completes the split in a single commit. Do **not** push.

### Task 11: Implement the Auto Break state machine

**Files:**
- Modify: `crates/sdr-transcription/src/backends/sherpa/offline.rs`

- [ ] **Step 1: Write the failing test suite FIRST (TDD)**

At the bottom of `offline.rs`, in the `#[cfg(test)] mod tests` block (create if missing), add the state machine tests. These drive the implementation shape — each test names a state transition we need to handle:

```rust
#[cfg(test)]
mod auto_break_tests {
    use super::*;

    // Simulated ms → sample count at 48 kHz stereo interleaved
    fn samples_for_ms(ms: u32) -> Vec<f32> {
        let frames = (48 * ms) as usize;
        vec![0.5_f32; frames * 2] // stereo
    }

    #[test]
    fn clean_utterance_produces_one_decode() {
        let machine = AutoBreakMachine::new();
        let events = machine
            .feed_sequence(&[
                AutoBreakEvent::Opened,
                AutoBreakEvent::Samples(samples_for_ms(1_000)),
                AutoBreakEvent::Closed,
                AutoBreakEvent::TailTimeout,
            ])
            .expect("state machine should run cleanly");
        assert_eq!(events.decodes_flushed, 1);
        assert_eq!(events.discarded_short, 0);
        assert_eq!(events.discarded_phantom, 0);
    }

    #[test]
    fn hysteresis_blip_single_utterance() {
        let machine = AutoBreakMachine::new();
        let events = machine
            .feed_sequence(&[
                AutoBreakEvent::Opened,
                AutoBreakEvent::Samples(samples_for_ms(500)),
                AutoBreakEvent::Closed,
                AutoBreakEvent::Opened, // blip within hold-off
                AutoBreakEvent::Samples(samples_for_ms(500)),
                AutoBreakEvent::Closed,
                AutoBreakEvent::TailTimeout,
            ])
            .expect("state machine should fold blip into one utterance");
        assert_eq!(events.decodes_flushed, 1);
    }

    #[test]
    fn phantom_open_below_min_open_ms_discarded() {
        let machine = AutoBreakMachine::new();
        let events = machine
            .feed_sequence(&[
                AutoBreakEvent::Opened,
                AutoBreakEvent::Samples(samples_for_ms(50)), // < MIN_OPEN_MS (100)
                AutoBreakEvent::Closed,
                AutoBreakEvent::TailTimeout,
            ])
            .expect("state machine should discard phantom");
        assert_eq!(events.decodes_flushed, 0);
        assert_eq!(events.discarded_phantom, 1);
    }

    #[test]
    fn sub_min_segment_discarded() {
        let machine = AutoBreakMachine::new();
        let events = machine
            .feed_sequence(&[
                AutoBreakEvent::Opened,
                AutoBreakEvent::Samples(samples_for_ms(300)), // > MIN_OPEN but < MIN_SEGMENT (400)
                AutoBreakEvent::Closed,
                AutoBreakEvent::TailTimeout,
            ])
            .expect("state machine should discard sub-min segment");
        assert_eq!(events.decodes_flushed, 0);
        assert_eq!(events.discarded_short, 1);
    }

    #[test]
    fn max_segment_safety_flush() {
        let machine = AutoBreakMachine::new();
        let events = machine
            .feed_sequence(&[
                AutoBreakEvent::Opened,
                AutoBreakEvent::Samples(samples_for_ms(31_000)), // > MAX_SEGMENT_MS (30_000)
                // no Close event — squelch is "stuck open"
            ])
            .expect("state machine should hit max-segment safety cap");
        assert_eq!(events.decodes_flushed, 1);
    }
}
```

- [ ] **Step 2: Run tests — expect FAIL with "AutoBreakMachine not defined"**

```bash
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu --lib auto_break_tests 2>&1 | tail -20
```

Expected: compile errors referencing `AutoBreakMachine`, `AutoBreakEvent`.

- [ ] **Step 3: Implement `AutoBreakEvent` and `AutoBreakMachine`**

In `offline.rs`, above the `#[cfg(test)] mod auto_break_tests`, add:

```rust
/// Events consumed by the Auto Break state machine. The real session
/// loop translates incoming `TranscriptionInput` variants + a
/// `recv_timeout` timer into this shape; the state machine itself is
/// pure so it can be unit-tested without a running recognizer.
#[cfg_attr(not(test), allow(dead_code))]
enum AutoBreakEvent {
    Samples(Vec<f32>),
    Opened,
    Closed,
    /// Hold-off timer expired (hit `AUTO_BREAK_TAIL_MS` with no new events).
    TailTimeout,
}

/// Counters returned by a test-only simulation of the state machine.
/// Only exists to give tests something observable — the real session
/// loop wires the same transitions into `decode_segment` / `tracing`.
#[cfg(test)]
#[derive(Debug, Default)]
struct AutoBreakFlushCounts {
    decodes_flushed: u32,
    discarded_short: u32,
    discarded_phantom: u32,
}

/// Pure state machine for Auto Break segmentation. Holds no I/O handles
/// so it can be unit-tested. The real session loop owns one and drives
/// it from the channel recv loop + hold-off timer.
struct AutoBreakMachine {
    state: AutoBreakState,
    /// Accumulated stereo interleaved samples for the current segment.
    buffer: Vec<f32>,
}

enum AutoBreakState {
    Idle,
    Recording,
    HoldingOff,
}

impl AutoBreakMachine {
    fn new() -> Self {
        Self {
            state: AutoBreakState::Idle,
            buffer: Vec::new(),
        }
    }

    /// Duration in ms currently held in the buffer, assuming 48 kHz
    /// stereo interleaved input.
    fn buffer_duration_ms(&self) -> u32 {
        let frames = self.buffer.len() / 2;
        #[allow(clippy::cast_possible_truncation)]
        let ms = (frames as u64 * 1000 / 48_000) as u32;
        ms
    }

    /// Test-only: drive the machine through a scripted sequence of
    /// events and return the flush-decision counters. A real session
    /// loop would call `decode_segment` instead of incrementing a counter.
    #[cfg(test)]
    fn feed_sequence(
        mut self,
        events: &[AutoBreakEvent],
    ) -> Result<AutoBreakFlushCounts, String> {
        let mut counts = AutoBreakFlushCounts::default();
        for ev in events {
            match ev {
                AutoBreakEvent::Samples(s) => self.on_samples(s.clone()),
                AutoBreakEvent::Opened => self.on_squelch_opened(),
                AutoBreakEvent::Closed => self.on_squelch_closed(),
                AutoBreakEvent::TailTimeout => {
                    if let Some(decision) = self.on_tail_timeout() {
                        match decision {
                            FlushDecision::Decode => counts.decodes_flushed += 1,
                            FlushDecision::DiscardPhantom => counts.discarded_phantom += 1,
                            FlushDecision::DiscardShort => counts.discarded_short += 1,
                        }
                    }
                }
            }
            // Max-segment safety check runs after every event that grows
            // the buffer.
            if matches!(self.state, AutoBreakState::Recording | AutoBreakState::HoldingOff)
                && self.buffer_duration_ms() >= AUTO_BREAK_MAX_SEGMENT_MS
            {
                // Force-flush as if the tail timer had fired, but without
                // the phantom check — the buffer is way past MIN_SEGMENT by
                // definition, so it's always a real utterance.
                counts.decodes_flushed += 1;
                self.state = AutoBreakState::Idle;
                self.buffer.clear();
            }
        }
        Ok(counts)
    }

    fn on_samples(&mut self, samples: Vec<f32>) {
        if matches!(self.state, AutoBreakState::Recording | AutoBreakState::HoldingOff) {
            self.buffer.extend_from_slice(&samples);
        }
        // Idle: discard — no transmission is happening
    }

    fn on_squelch_opened(&mut self) {
        match self.state {
            AutoBreakState::Idle => {
                self.buffer.clear();
                self.state = AutoBreakState::Recording;
            }
            AutoBreakState::HoldingOff => {
                // Hysteresis blip — cancel the deferred flush, stay with
                // the same buffer.
                self.state = AutoBreakState::Recording;
            }
            AutoBreakState::Recording => {
                // Already recording (redundant event); ignore.
            }
        }
    }

    fn on_squelch_closed(&mut self) {
        if matches!(self.state, AutoBreakState::Recording) {
            self.state = AutoBreakState::HoldingOff;
        }
    }

    fn on_tail_timeout(&mut self) -> Option<FlushDecision> {
        if !matches!(self.state, AutoBreakState::HoldingOff) {
            return None;
        }
        let duration = self.buffer_duration_ms();
        let decision = if duration < AUTO_BREAK_MIN_OPEN_MS {
            FlushDecision::DiscardPhantom
        } else if duration < AUTO_BREAK_MIN_SEGMENT_MS {
            FlushDecision::DiscardShort
        } else {
            FlushDecision::Decode
        };
        self.state = AutoBreakState::Idle;
        self.buffer.clear();
        Some(decision)
    }

    /// Take the accumulated buffer for decoding. Used by the real session
    /// loop (NOT the test feed_sequence path) to hand samples off without
    /// re-allocating.
    #[allow(dead_code)]
    fn take_buffer(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.buffer)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
enum FlushDecision {
    Decode,
    DiscardShort,
    DiscardPhantom,
}
```

- [ ] **Step 4: Run tests — expect PASS**

```bash
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu --lib auto_break_tests 2>&1 | tail -20
```

Expected: 5 tests pass (`clean_utterance_produces_one_decode`, `hysteresis_blip_single_utterance`, `phantom_open_below_min_open_ms_discarded`, `sub_min_segment_discarded`, `max_segment_safety_flush`).

- [ ] **Step 5: Implement `run_session_auto_break` driving the state machine from the real channel**

Below `run_session_vad` in `offline.rs`, add:

```rust
/// Auto Break offline session. Drives an `AutoBreakMachine` from the
/// transcription input channel + a hold-off timer implemented via
/// `recv_timeout`. Buffers stereo 48 kHz interleaved samples during
/// Recording / HoldingOff states; on flush, resamples to 16 kHz mono,
/// applies the spectral denoiser, and decodes through the recognizer.
fn run_session_auto_break(
    recognizer: &OfflineRecognizer,
    params: SessionParams,
) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
        vad_threshold: _,
        segmentation_mode: _,
    } = params;

    if event_tx.send(TranscriptionEvent::Ready).is_err() {
        return;
    }

    let tail_duration = std::time::Duration::from_millis(u64::from(AUTO_BREAK_TAIL_MS));
    let mut machine = AutoBreakMachine::new();
    // `pending_flush_deadline` is Some only while we're in HoldingOff.
    // When it expires we synthesize a TailTimeout event for the machine.
    let mut pending_flush_deadline: Option<std::time::Instant> = None;

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa Auto Break session cancelled");
            return;
        }

        // Choose timeout: if we're waiting for a tail flush, recv_timeout
        // at the remaining deadline; otherwise just block on the audio
        // polling interval so we can check `cancel`.
        let timeout = match pending_flush_deadline {
            Some(deadline) => deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_else(|| std::time::Duration::from_millis(0)),
            None => AUDIO_RECV_TIMEOUT,
        };

        let recv = audio_rx.recv_timeout(timeout);
        match recv {
            Ok(crate::backend::TranscriptionInput::Samples(samples)) => {
                machine.on_samples(samples);
                // Max-segment safety cap
                if machine.buffer_duration_ms() >= AUTO_BREAK_MAX_SEGMENT_MS {
                    tracing::warn!(
                        "Auto Break buffer exceeded {}ms — forcing flush, check squelch config",
                        AUTO_BREAK_MAX_SEGMENT_MS
                    );
                    flush_auto_break_buffer(
                        recognizer,
                        &mut machine,
                        &event_tx,
                        noise_gate_ratio,
                    );
                    pending_flush_deadline = None;
                }
            }
            Ok(crate::backend::TranscriptionInput::SquelchOpened) => {
                machine.on_squelch_opened();
                pending_flush_deadline = None;
            }
            Ok(crate::backend::TranscriptionInput::SquelchClosed) => {
                machine.on_squelch_closed();
                pending_flush_deadline =
                    Some(std::time::Instant::now() + tail_duration);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(deadline) = pending_flush_deadline
                    && std::time::Instant::now() >= deadline
                {
                    if let Some(decision) = machine.on_tail_timeout() {
                        match decision {
                            FlushDecision::Decode => flush_auto_break_buffer(
                                recognizer,
                                &mut machine,
                                &event_tx,
                                noise_gate_ratio,
                            ),
                            FlushDecision::DiscardPhantom => {
                                tracing::debug!("Auto Break: discarded phantom open");
                            }
                            FlushDecision::DiscardShort => {
                                tracing::debug!("Auto Break: discarded sub-min segment");
                            }
                        }
                    }
                    pending_flush_deadline = None;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::info!("sherpa Auto Break session ended (channel disconnected)");
                return;
            }
        }
    }
}

/// Resample + denoise + decode the current Auto Break buffer, emit a
/// `Text` event if the recognizer produced non-empty output. Resets the
/// machine's internal buffer after decoding.
fn flush_auto_break_buffer(
    recognizer: &OfflineRecognizer,
    machine: &mut AutoBreakMachine,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    noise_gate_ratio: f32,
) {
    let stereo_buf = machine.take_buffer();
    if stereo_buf.is_empty() {
        return;
    }
    let mut mono_buf: Vec<f32> = Vec::with_capacity(stereo_buf.len() / 6); // rough: /2 for mono, /3 for 48k→16k
    resampler::downsample_stereo_to_mono_16k(&stereo_buf, &mut mono_buf);
    denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);
    decode_segment(recognizer, &mono_buf, event_tx);
}
```

- [ ] **Step 6: Build + test**

```bash
cargo test -p sdr-transcription --no-default-features --features sherpa-cpu --lib 2>&1 | tail -15
```

Expected: all existing tests + the 5 new Auto Break state machine tests pass.

- [ ] **Step 7: Build check on sherpa-cuda**

```bash
cargo check --workspace --no-default-features --features sherpa-cuda 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 8: Clippy + fmt**

```bash
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -10
cargo fmt --all
```

Expected: clippy clean, fmt makes no changes (or minor whitespace).

- [ ] **Step 9: Commit**

```bash
git add crates/sdr-transcription/src/backends/sherpa/offline.rs
git commit -m "feat(sherpa-offline): Auto Break state machine + session dispatch"
```

---

## Phase E: UI wiring

### Task 12: Add the Auto Break toggle row + persistence in `transcript_panel.rs`

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/transcript_panel.rs`

- [ ] **Step 1: Add a config key constant near the top of the file**

Near the existing `KEY_SHERPA_VAD_THRESHOLD` const in `transcript_panel.rs` (search with `grep -n KEY_SHERPA transcript_panel.rs`):

```rust
/// Config key for persisting the Auto Break segmentation preference.
/// When true, offline sherpa sessions use squelch edges as utterance
/// boundaries instead of Silero VAD. Default false (preserve existing
/// behavior for existing config files).
#[cfg(feature = "sherpa")]
pub(crate) const KEY_AUTO_BREAK_ENABLED: &str = "transcription_auto_break_enabled";
```

- [ ] **Step 2: Build the `auto_break_row` widget and add it to the preferences group**

In `build_transcript_panel()`, after the `vad_threshold_row` block (around line 332 in current source):

```rust
#[cfg(feature = "sherpa")]
let auto_break_row = {
    let saved = config.read(|v| {
        v.get(KEY_AUTO_BREAK_ENABLED)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    });

    let row = adw::SwitchRow::builder()
        .title("Auto Break")
        .subtitle("Use the radio's squelch as the transcription boundary instead of VAD. NFM only.")
        .active(saved)
        .build();
    group.add(&row);

    let config_ab = Arc::clone(config);
    row.connect_active_notify(move |r| {
        let active = r.is_active();
        config_ab.write(|v| {
            v[KEY_AUTO_BREAK_ENABLED] = serde_json::json!(active);
        });
    });

    row
};
```

- [ ] **Step 3: Add the row to the `TranscriptPanel` struct and return it**

Find the `TranscriptPanel` struct definition (search `grep -n "pub struct TranscriptPanel" transcript_panel.rs`). Add:

```rust
#[cfg(feature = "sherpa")]
pub auto_break_row: adw::SwitchRow,
```

And at the bottom of `build_transcript_panel()` in the returned struct literal, add:

```rust
#[cfg(feature = "sherpa")]
auto_break_row,
```

- [ ] **Step 4: Build check**

```bash
cargo check -p sdr-ui --no-default-features --features sherpa-cpu 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-ui/src/sidebar/transcript_panel.rs
git commit -m "feat(ui): add Auto Break toggle row to transcript panel"
```

### Task 13: Wire visibility rules for the Auto Break toggle (offline model + NFM gate + mutex with VAD slider)

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/transcript_panel.rs`

- [ ] **Step 1: Initial visibility based on saved model + assume NFM at startup**

The existing visibility block at lines 385-405 sets `vad_threshold_row` visibility based on `supports_partials`. Extend it to also handle the new `auto_break_row`:

```rust
#[cfg(feature = "sherpa")]
{
    let initial_supports_partials = sdr_transcription::SherpaModel::ALL
        .get(saved_model_idx as usize)
        .copied()
        .is_some_and(sdr_transcription::SherpaModel::supports_partials);
    let initial_is_offline = !initial_supports_partials;
    let initial_auto_break_active = auto_break_row.is_active();

    display_mode_row.set_visible(initial_supports_partials);
    // VAD slider visible only when offline model AND Auto Break is OFF
    vad_threshold_row.set_visible(initial_is_offline && !initial_auto_break_active);
    // Auto Break toggle visible only when offline model (NFM gate added
    // by DemodModeChanged handler in window.rs; initial state assumes NFM
    // because that's the most common startup mode for transcription use).
    auto_break_row.set_visible(initial_is_offline);

    let display_mode_row_for_visibility = display_mode_row.clone();
    let vad_threshold_row_for_visibility = vad_threshold_row.clone();
    let auto_break_row_for_visibility = auto_break_row.clone();
    model_row.connect_selected_notify(move |r| {
        let idx = r.selected() as usize;
        let supports_partials = sdr_transcription::SherpaModel::ALL
            .get(idx)
            .copied()
            .is_some_and(sdr_transcription::SherpaModel::supports_partials);
        let is_offline = !supports_partials;
        let ab_active = auto_break_row_for_visibility.is_active();

        display_mode_row_for_visibility.set_visible(supports_partials);
        vad_threshold_row_for_visibility.set_visible(is_offline && !ab_active);
        auto_break_row_for_visibility.set_visible(is_offline);
    });

    // Mutex: toggling Auto Break hides/shows the VAD threshold slider.
    let vad_threshold_row_for_mutex = vad_threshold_row.clone();
    let auto_break_row_clone = auto_break_row.clone();
    auto_break_row.connect_active_notify(move |r| {
        // Only re-evaluate visibility if Auto Break is currently visible
        // at all (i.e., we're on an offline model). If the row is hidden
        // it means a streaming model is selected and the mutex doesn't apply.
        if auto_break_row_clone.is_visible() {
            vad_threshold_row_for_mutex.set_visible(!r.is_active());
        }
    });
}
```

- [ ] **Step 2: Build check**

```bash
cargo check -p sdr-ui --no-default-features --features sherpa-cpu 2>&1 | tail -5
cargo check -p sdr-ui --no-default-features --features sherpa-cuda 2>&1 | tail -5
```

Expected: both clean.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/sidebar/transcript_panel.rs
git commit -m "feat(ui): Auto Break visibility rules + VAD slider mutex"
```

### Task 14: Pass `segmentation_mode` into `BackendConfig` in window.rs

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 1: Read `auto_break_row.is_active()` when building the config**

At `crates/sdr-ui/src/window.rs:1774-1779`, the `BackendConfig { ... }` construction:

```rust
#[cfg(feature = "sherpa")]
let segmentation_mode = if auto_break_row.is_active() {
    sdr_transcription::SegmentationMode::AutoBreak
} else {
    sdr_transcription::SegmentationMode::Vad
};
#[cfg(feature = "whisper")]
let segmentation_mode = sdr_transcription::SegmentationMode::Vad;

let config = sdr_transcription::BackendConfig {
    model,
    silence_threshold,
    noise_gate_ratio,
    vad_threshold,
    segmentation_mode,
};
```

The `auto_break_row` reference needs to be cloned into the closure at the top of `connect_transcript_panel`. Find the existing `let vad_threshold_row = ...` clone block earlier in the file and add an analogous `let auto_break_row = transcript.auto_break_row.clone();` (gated on `#[cfg(feature = "sherpa")]`).

- [ ] **Step 2: Lock `auto_break_row` during active session**

In the same connect handler where `vad_threshold_row.set_sensitive(false)` is called (line 1733 in current source):

```rust
#[cfg(feature = "sherpa")]
vad_threshold_row.set_sensitive(false);
#[cfg(feature = "sherpa")]
auto_break_row.set_sensitive(false);
```

And where session-end re-enables rows (search for `set_sensitive(true)` in the same function):

```rust
#[cfg(feature = "sherpa")]
auto_break_row.set_sensitive(true);
```

Mirror every existing `vad_threshold_row.set_sensitive(...)` line with an `auto_break_row.set_sensitive(...)` counterpart.

- [ ] **Step 3: Build check**

```bash
cargo check -p sdr-ui --no-default-features --features sherpa-cpu 2>&1 | tail -5
cargo check --workspace 2>&1 | tail -5
```

Expected: both clean.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "feat(ui): pass Auto Break selection into BackendConfig + session lock"
```

### Task 15: Add the proactive demod mode selector subtitle

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/radio_panel.rs`

- [ ] **Step 1: Find the demod mode selector ComboRow**

```bash
grep -n "demod\|DemodMode\|ComboRow" crates/sdr-ui/src/sidebar/radio_panel.rs | head -20
```

Locate the line where the demod mode row is built (e.g., `let demod_row = adw::ComboRow::builder()...`).

- [ ] **Step 2: Add the subtitle**

Add `.subtitle("Changing band stops active transcription")` to the builder chain. Example (actual field names will match whatever is in the source):

```rust
let demod_row = adw::ComboRow::builder()
    .title("Demodulator")
    .subtitle("Changing band stops active transcription")
    .model(&demod_list)
    .build();
```

- [ ] **Step 3: Build check**

```bash
cargo check -p sdr-ui 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/sidebar/radio_panel.rs
git commit -m "feat(ui): add proactive subtitle to demod mode selector"
```

### Task 16: Subscribe to `DemodModeChanged` in window.rs — stop active transcription + toast

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 1: Find the existing `DspToUi` event router**

```bash
grep -n "DspToUi::\|dsp_rx\|glib::MainContext" crates/sdr-ui/src/window.rs | head -30
```

The project uses a `glib::MainContext` channel with a `attach`/`while let` pattern to route `DspToUi` events into UI updates. Find the big `match` on `DspToUi` variants.

- [ ] **Step 2: Add a `DspToUi::DemodModeChanged` arm that stops transcription + shows a toast**

In the `match` arm, add:

```rust
crate::messages::DspToUi::DemodModeChanged(new_mode) => {
    tracing::info!(?new_mode, "demod mode changed");

    // Update Auto Break toggle visibility based on new mode. The row
    // is only visible when the current mode is NFM AND the selected
    // model is offline — offline check is handled by the existing
    // model-change handler in transcript_panel, so we just gate on
    // mode here.
    #[cfg(feature = "sherpa")]
    {
        let is_nfm = new_mode == sdr_types::DemodMode::Nfm;
        // If not NFM, force the Auto Break row hidden (overrides the
        // offline-model check). If NFM, restore the offline-model
        // visibility rule.
        let selected_is_offline = {
            let idx = panels.transcript.model_row.selected() as usize;
            sdr_transcription::SherpaModel::ALL
                .get(idx)
                .copied()
                .is_some_and(|m| !m.supports_partials())
        };
        panels
            .transcript
            .auto_break_row
            .set_visible(is_nfm && selected_is_offline);
    }

    // If a transcription session is currently active, stop it and
    // toast the user. The session's BackendConfig and row settings
    // are preserved — the user clicks Start to resume on the new band.
    if panels.transcript.enable_row.is_active() {
        tracing::info!("stopping active transcription due to demod mode change");
        // Toggling enable_row off triggers the existing stop path
        // (connect_active_notify handler at around window.rs:1719).
        panels.transcript.enable_row.set_active(false);

        let toast = adw::Toast::new(
            "Transcription stopped — demod mode changed. Press Start to resume.",
        );
        toast_overlay.add_toast(toast);
    }
}
```

Note: `toast_overlay` is the existing `AdwToastOverlay` reference already bound in scope; `panels` is the existing panels aggregate. If the exact binding names differ, match whatever is in use.

- [ ] **Step 3: Build check**

```bash
cargo check -p sdr-ui 2>&1 | tail -10
cargo check --workspace 2>&1 | tail -5
cargo check --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -5
cargo check --workspace --no-default-features --features sherpa-cuda 2>&1 | tail -5
```

Expected: all four clean.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "feat(ui): stop transcription on demod mode change + toast"
```

### Task 17: Squelch-enabled precondition check on session start

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 1: Check `radio.squelch_enabled_row.is_active()` before starting with Auto Break**

In the `transcript.enable_row.connect_active_notify` handler at `crates/sdr-ui/src/window.rs:1719`, at the very start of the `if row.is_active()` branch (before any config building):

```rust
#[cfg(feature = "sherpa")]
{
    // Auto Break precondition: squelch must be enabled so the radio
    // actually produces open/close transitions. If squelch is off, the
    // session would run forever in Idle with no segments.
    if auto_break_row.is_active() && !radio_panel.squelch_enabled_row.is_active() {
        let toast = adw::Toast::new(
            "Auto Break needs squelch enabled to detect transmission boundaries. \
             Enable squelch in the radio panel, or turn off Auto Break to use VAD.",
        );
        toast_overlay.add_toast(toast);
        // Revert the toggle — user has to take action first.
        row.set_active(false);
        return;
    }
}
```

`radio_panel.squelch_enabled_row` needs to be cloned into the closure. Find the existing panel reference clones at the top of `connect_transcript_panel` and add a clone for `radio_panel.squelch_enabled_row`.

- [ ] **Step 2: Build check**

```bash
cargo check -p sdr-ui --no-default-features --features sherpa-cuda 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "feat(ui): Auto Break precondition — require squelch enabled"
```

---

## Phase F: Verification and close-out

### Task 18: Full triple-build + clippy verification

- [ ] **Step 1: Triple-flavor cargo check**

```bash
cargo check --workspace 2>&1 | tail -5
cargo check --workspace --features whisper-cuda 2>&1 | tail -5
cargo check --workspace --no-default-features --features sherpa-cpu 2>&1 | tail -5
cargo check --workspace --no-default-features --features sherpa-cuda 2>&1 | tail -5
```

Expected: all four clean.

- [ ] **Step 2: Clippy on each flavor**

```bash
cargo clippy --all-targets --workspace -- -D warnings 2>&1 | tail -15
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings 2>&1 | tail -15
cargo clippy --all-targets --workspace --no-default-features --features sherpa-cuda -- -D warnings 2>&1 | tail -15
```

Expected: clean on all three.

- [ ] **Step 3: Test workspace**

```bash
cargo test --workspace 2>&1 | grep -E "test result|FAILED" | tail -20
```

Expected: all pre-existing + new tests pass, zero failures.

- [ ] **Step 4: Format check**

```bash
cargo fmt --all -- --check 2>&1 | tail -5
```

Expected: no output.

- [ ] **Step 5: Deny check for both graphs**

```bash
cargo deny check 2>&1 | tail -10
cargo deny --no-default-features --features sherpa-cuda check sources 2>&1 | tail -5
```

Expected: both pass.

- [ ] **Step 6: If anything fails, fix and re-run; if all pass, commit any fmt adjustments**

```bash
git status
# If files were modified by cargo fmt:
git add -u
git commit -m "chore: cargo fmt"
```

### Task 19: User-driven pre-PR smoke test

This is the checklist from the spec's "Pre-PR smoke test checklist" section. The user runs through it manually on a running `sdr-rs` binary.

- [ ] **Step 1: Install the sherpa-cuda build**

```bash
make install CARGO_FLAGS="--release --no-default-features --features sherpa-cuda"
```

Expected: clean build, install succeeds.

- [ ] **Step 2: Run the full spec checklist**

Work through every item in `docs/superpowers/specs/2026-04-13-auto-break-segmentation-design.md` under "Pre-PR smoke test checklist (user-driven, runs before the PR is opened)". Items are grouped by category:

- Build + launch sanity (3 items)
- Sherpa model regression with Auto Break OFF (3 items)
- VAD threshold slider regression (3 items)
- Auto Break new feature (12 items)
- Universal mode-change stop (5 items)
- Whisper regression (1 item)

Each checkbox is a concrete observable behavior. Check it only after verifying on the running binary.

- [ ] **Step 3: Record the filled-in checklist**

Copy the completed checklist into a scratch note for use as the PR body's test plan.

### Task 20: File follow-up issue for slider tuning

- [ ] **Step 1: Create the GitHub issue**

```bash
gh issue create --title "Expose Auto Break timing parameters as user-tunable sliders" --body "Context: PR for #265 ships Auto Break with hardcoded constants in \`crates/sdr-transcription/src/backends/sherpa/offline.rs\`:

- \`AUTO_BREAK_MIN_OPEN_MS = 100\`
- \`AUTO_BREAK_TAIL_MS = 200\`
- \`AUTO_BREAK_MIN_SEGMENT_MS = 400\`
- \`AUTO_BREAK_MAX_SEGMENT_MS = 30_000\`

Problem: there is no one-size-fits-all for real-world NFM — different bands, repeaters, mobile signals, and scanner use cases have different optimal hold-off values. The VAD threshold slider in PR 6 is precedent for exactly this kind of tuning surface.

Proposed: add three new SpinRows to the transcript panel under the Auto Break toggle (visible only when Auto Break is on). Persist as \`transcription_auto_break_min_open_ms\` / \`_tail_ms\` / \`_min_segment_ms\` config keys. Defaults match the v1 hardcoded values.

Acceptance: real-world scanner testing with the slider values tuned produces cleaner segment boundaries than the defaults on at least one representative recording.

Labels: enhancement, transcription, ui" --label enhancement
```

- [ ] **Step 2: Record the issue number for the PR description**

### Task 21: Commit, push, open PR

- [ ] **Step 1: Final status check**

```bash
git status
git log --oneline feature/auto-break-segmentation ^main
```

Expected: clean working tree, a sequence of feature commits on the branch.

- [ ] **Step 2: Push the branch**

```bash
git push -u origin feature/auto-break-segmentation
```

- [ ] **Step 3: Open the PR**

```bash
gh pr create --title "feat(transcription): Auto Break segmentation + universal stop-on-mode-change" --body "$(cat <<'EOF'
## Summary
- Adds a new `sherpa-cuda`-and-`sherpa-cpu`-compatible `Auto Break` segmentation mode for offline sherpa models (Moonshine, Parakeet) that uses the radio's squelch gate as the utterance boundary instead of Silero VAD. NFM demod only; mutex with VAD per session.
- Introduces a universal behavior change: any demod mode change while transcription is active now stops the session cleanly with an explainer toast. Applies to BOTH Auto Break and VAD modes — this is a deliberate UX simplification.
- Refactors the internal audio-tap channel from `mpsc::SyncSender<Vec<f32>>` to `mpsc::SyncSender<TranscriptionInput>` so squelch edge events can flow alongside samples. Whisper pattern-matches on `Samples` and ignores the edge variants.

## Test plan
(Paste the filled-in pre-PR smoke test checklist from the spec here.)

## Design docs
- Spec: `docs/superpowers/specs/2026-04-13-auto-break-segmentation-design.md`
- Plan: `docs/superpowers/plans/2026-04-13-auto-break-segmentation.md`

## Behavior change for existing VAD users
PRs 1–6 let VAD mode transcription continue across demod mode changes. This PR changes that to a universal stop. The project has one user today (the owner), who confirmed during design that the change is actively preferable — the old behavior was quietly lossy across band changes because different bands have different noise characteristics.

## Follow-up
- #XXX (filed during this PR) — expose Auto Break timing parameters as user-tunable sliders after real-world testing

Closes #265.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)" 2>&1 | tail -3
```

Replace `#XXX` with the actual issue number from Task 20.

- [ ] **Step 4: Record the PR URL**

---

## Self-review

### Spec coverage

Walking through each spec section:

- **Goal / Motivation / Non-goals** — captured in plan header, no tasks needed
- **Architecture / Data flow** — Tasks 1-8 (channel refactor + edge-event emission)
- **`TranscriptionInput` enum** — Task 1 Step 1
- **`SegmentationMode` enum + `BackendConfig` field** — Task 1 Steps 1-2
- **Controller-side changes (squelch_was_open, edge events, NFM gate)** — Task 7
- **Offline session loop state machine** — Tasks 9-11
- **`run_session` dispatch** — Task 10
- **Hardcoded timing constants** — Task 9
- **Hysteresis blip behavior** — covered by `hysteresis_blip_single_utterance` test in Task 11 Step 1
- **Max-segment safety cap** — covered by `max_segment_safety_flush` test + implementation in Task 11
- **Mode-change universal stop** — Tasks 8 (emit event) + 16 (UI subscriber)
- **UI transcript panel row + visibility** — Tasks 12-14
- **Demod selector subtitle** — Task 15
- **Toast on mode change** — Task 16 Step 2
- **Precondition check** — Task 17
- **Streaming rejection of AutoBreak** — Task 4 Step 1
- **Whisper compatibility (ignore edge events)** — Task 2
- **Config key persistence** — Task 12 Steps 1-2
- **Session lock** — Task 14 Step 2
- **Testing strategy unit tests** — Task 11 Step 1 (5 Auto Break state machine tests)
- **Mode-change stop unit tests** — NOT covered by a unit test task; gap noted below
- **Pre-PR smoke test checklist** — Task 19
- **Follow-up issue for sliders** — Task 20
- **Triple-build verification** — Task 18

**Gaps found:**

1. **Mode-change stop unit tests** — the spec lists 4 unit tests (`mode_change_stops_active_transcription_vad_mode`, `_auto_break_mode`, `mode_change_preserves_config`, `mode_change_without_active_session_is_noop`) but these test UI-layer behavior that requires a running GTK context. Testing them properly needs either a headless GTK harness (not in scope today) or a refactor that extracts the decision logic into a pure function. **Accepting the gap**: the smoke test checklist Task 19 covers all four scenarios as user-driven verification. Noted in the plan's Task 19 so it's not forgotten.

### Placeholder scan

Ran: `grep -ni "tbd\|todo\|fill in\|handle the case" docs/superpowers/plans/2026-04-13-auto-break-segmentation.md`

No hits on "TBD", "TODO", or "fill in". Some tasks reference "the existing handler at line N" — that's a pointer into specific code locations, not a placeholder.

### Type consistency

- `TranscriptionInput::{Samples, SquelchOpened, SquelchClosed}` — used consistently across Tasks 1, 2, 3, 4, 5, 6, 7, 11. No drift.
- `SegmentationMode::{Vad, AutoBreak}` — consistent across Tasks 1, 3, 4, 10, 14.
- `AutoBreakMachine::{new, on_samples, on_squelch_opened, on_squelch_closed, on_tail_timeout, buffer_duration_ms, take_buffer, feed_sequence}` — method names defined in Task 11 Step 3 and used in Task 11 Steps 1 (test) and 5 (session loop). Consistent.
- `KEY_AUTO_BREAK_ENABLED` — defined in Task 12 Step 1, used in Task 12 Steps 1-2 only (not referenced from window.rs; state lives in the row widget).
- `auto_break_row` — field name consistent across Tasks 12, 13, 14, 16, 17.
- `DemodMode::Nfm` — used consistently, correct enum path via `sdr_types::DemodMode`.
- `DspToUi::DemodModeChanged(DemodMode)` — defined in Task 8 Step 1, consumed in Task 16 Step 2. Consistent.

No type drift found.
