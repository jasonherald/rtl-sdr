# Moonshine Integration Design

**Status:** Design approved 2026-04-13
**Tracking:** #224 (parent: #204 — sherpa-onnx integration epic)
**Branch:** `feature/moonshine-integration`

## Goal

Add two new `SherpaModel` variants — `MoonshineTinyEn` and `MoonshineBaseEn` — to the transcription backend. Moonshine is a low-latency, edge-optimized encoder-decoder model from UsefulSensors, distributed through sherpa-onnx as an offline (non-streaming) recognizer. Because it is offline-only, the integration introduces a second session path alongside the existing streaming Zipformer path, gated by a Silero VAD for end-of-speech detection.

This is the first PR that proves the multi-model-kind architecture inside the Sherpa backend; future PRs (Parakeet and beyond) will slot into one of the two paths based on their recognizer kind.

## Motivation

- Zipformer is great but 256MB + a chunky recognizer. Moonshine-tiny (~170MB, ~27M params) gives us a CPU-friendly path for low-end hardware.
- Moonshine is designed for short-utterance, bursty audio — exactly the shape of radio chatter over RTL-SDR. The VAD-gated batch decode pattern matches how scanner audio naturally presents itself.
- Users should pick based on their hardware: tiny for low-end, base (~61M params) for anyone with headroom, Zipformer for true streaming.

## Non-goals

- **Not** retrofitting Whisper to use Silero VAD. Whisper's RMS-gated chunk approach works and is out of scope. A follow-up issue will track that retrofit.
- **Not** shipping Parakeet in this PR. Parakeet is a separate PR, mechanically easier (transducer family → existing streaming loop).
- **Not** exposing VAD hyperparameters (threshold, min silence, etc.) in the UI. Sensible defaults only; tuning is a follow-up if needed.
- **Not** supporting Moonshine v1 (4-file layout). Only Moonshine v2 (2-file: encoder + merged_decoder).
- **Not** generating Partial events for Moonshine. Offline decode fires once per utterance, after VAD says end-of-speech. See "UX decisions" below.

## Architecture

### File structure

```
crates/sdr-transcription/src/
  vad.rs                         # NEW — feature-agnostic VoiceActivityDetector trait
  sherpa_model.rs                # EXTENDED — adds Moonshine variants, ModelFilePaths enum,
                                 #            Silero VAD download helpers, ModelKind enum
  init_event.rs                  # EXTENDED — DownloadStart/Extracting carry component label
  backends/
    sherpa.rs                    # REMOVED (contents split into sherpa/ module)
    sherpa/                      # NEW DIR
      mod.rs                     # SherpaBackend facade (start/stop/shutdown); re-exports init_sherpa_host
      host.rs                    # SherpaHost + spawn + run_host_loop (branches on model kind)
      streaming.rs               # run_session_online — Zipformer (and future Parakeet) loop
      offline.rs                 # run_session_offline — Moonshine + VAD loop
      silero_vad.rs              # SherpaSileroVad: impl VoiceActivityDetector
```

### Data types

**`SherpaModel` variants expand from 1 to 3:**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SherpaModel {
    StreamingZipformerEn,   // existing
    MoonshineTinyEn,        // NEW — ~27M params, fastest, ~170MB bundle
    MoonshineBaseEn,        // NEW — ~61M params, more accurate, ~380MB bundle
}

pub enum ModelKind {
    OnlineTransducer,       // OnlineRecognizer + streaming loop
    OfflineMoonshine,       // OfflineRecognizer + Silero VAD + batch loop
}

impl SherpaModel {
    pub fn label(self) -> &'static str;
    pub fn dir_name(self) -> &'static str;
    pub fn archive_filename(self) -> &'static str;
    pub fn archive_inner_directory(self) -> &'static str;
    pub fn archive_url(self) -> String;

    // NEW
    pub fn kind(self) -> ModelKind;
    pub fn supports_partials(self) -> bool;

    pub const ALL: &[Self] = &[
        Self::StreamingZipformerEn,
        Self::MoonshineTinyEn,
        Self::MoonshineBaseEn,
    ];
}
```

**`ModelFilePaths` replaces the current `(encoder, decoder, joiner, tokens)` 4-tuple:**

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

pub fn model_file_paths(model: SherpaModel) -> ModelFilePaths;
pub fn model_exists(model: SherpaModel) -> bool;  // matches on kind internally
```

**`VoiceActivityDetector` trait in `src/vad.rs` — feature-agnostic**:

```rust
/// Feed 16kHz mono samples. Internally buffers and runs Silero detection.
/// Segments are emitted via `pop_segment` once VAD sees a complete utterance.
pub trait VoiceActivityDetector {
    fn accept(&mut self, samples: &[f32]);
    fn pop_segment(&mut self) -> Option<Vec<f32>>;
    fn reset(&mut self);
}
```

The trait lives in `src/vad.rs` (NOT `backends/sherpa/vad.rs`) so the Whisper retrofit PR can reference it without going through a sherpa-gated module path. The sherpa-onnx-backed impl lives in `backends/sherpa/silero_vad.rs` behind `#[cfg(feature = "sherpa")]`. The trait definition itself compiles into both feature flavors.

**`InitEvent` gains component labels:**

```rust
pub enum InitEvent {
    DownloadStart { component: &'static str },      // was: unit variant
    DownloadProgress { pct: u8 },                   // unchanged
    Extracting { component: &'static str },         // was: unit variant
    CreatingRecognizer,
    Ready,
    Failed { message: String },
}
```

- Zipformer init emits `component: "Streaming Zipformer (English)"` — minor UX win, splash now says the actual model name.
- Moonshine init emits twice: first `component: "Silero VAD"`, then `component: "Moonshine Tiny"` (or "Moonshine Base").
- Splash label mapping in `main.rs` (or wherever `SplashController::update_text` is driven) uses `format!("Downloading {component}...")`.

**Silero VAD model path helpers in `sherpa_model.rs`:**

```rust
pub fn silero_vad_path() -> PathBuf;          // ~/.local/share/sdr-rs/models/sherpa/silero-vad/silero_vad.onnx
pub fn silero_vad_exists() -> bool;
pub fn download_silero_vad(progress_tx: &Sender<u8>) -> Result<PathBuf, SherpaModelError>;
```

Silero VAD is a single ~2MB `.onnx` file from the sherpa-onnx releases page — no tarball, no extraction. The download function writes to `silero_vad.onnx.part` then renames to `silero_vad.onnx` on success (atomic).

### Host init flow

`backends/sherpa/host.rs::run_host_loop` branches on `model.kind()` at the top:

```
if model_kind == OnlineTransducer:
    [unchanged from current behavior — Zipformer path]
    1. if !model_exists(model): DownloadStart + progress + Extracting, download+extract
    2. emit CreatingRecognizer
    3. create OnlineRecognizer
    4. store host, emit Ready
    5. streaming command loop → run_session_online

if model_kind == OfflineMoonshine:
    1. if !silero_vad_exists():
         emit DownloadStart { "Silero VAD" }
         download silero_vad.onnx with progress events
    2. if !model_exists(model):
         emit DownloadStart { model.label() }
         download bundle with progress events
         emit Extracting { model.label() }
         extract
    3. emit CreatingRecognizer
    4. create OfflineRecognizer (OfflineMoonshineModelConfig)
    5. create SherpaSileroVad from silero_vad_path()
    6. store host (now owns BOTH recognizer AND vad), emit Ready
    7. offline command loop → run_session_offline
```

The host struct `SherpaHost` holds either the online recognizer or the (offline recognizer + VAD) pair — represented as an enum internally:

```rust
enum RecognizerState {
    Online(OnlineRecognizer),
    Offline {
        recognizer: OfflineRecognizer,
        vad: SherpaSileroVad,
    },
}
```

The command loop is the same shape for both — `while let Ok(cmd) = cmd_rx.recv()` with `HostCommand::StartSession(params)` dispatching to either `streaming::run_session` or `offline::run_session` based on `RecognizerState`.

### Offline session loop

`backends/sherpa/offline.rs::run_session`:

```
accept SessionParams { cancel, audio_rx, event_tx, noise_gate_ratio }
event_tx.send(Ready)
vad.reset()  // clear any residual state from a previous session

loop {
    if cancel: break
    match audio_rx.recv_timeout(100ms):
        Ok(interleaved):
            mono_buf.clear()
            downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf)
            // drain extra queued buffers into same scratch
            while let Ok(extra) = audio_rx.try_recv() {
                if cancel: break out
                downsample_stereo_to_mono_16k(&extra, &mut mono_buf)
            }
            if mono_buf.is_empty(): continue
            spectral_denoise(&mut mono_buf, noise_gate_ratio)
            vad.accept(&mono_buf)

            while let Some(segment) = vad.pop_segment() {
                if cancel: break out
                // Offline decode shape mirrors Online but without streaming:
                // one stream per segment, batch decode, pull result, drop stream.
                let stream = recognizer.create_stream()
                stream.accept_waveform(SHERPA_SAMPLE_RATE_HZ, &segment)
                recognizer.decode(&stream)
                let result = stream.get_result()
                let text = result.text.trim()
                if !text.is_empty() {
                    let timestamp = wall_clock_timestamp()
                    event_tx.send(TranscriptionEvent::Text { timestamp, text })
                }
            }

        Err(Timeout): continue
        Err(Disconnected): break
}

// finalize: flush VAD to pop any in-flight segment, decode + emit as final Text
vad.reset()
```

**Key differences from the online loop:**

- No `recognizer.is_ready()` / `is_endpoint()` / `reset()` — VAD owns those semantics.
- No `Partial` event emission — Moonshine is offline, partials aren't meaningful. The `live_line_label` in the UI stays empty for Moonshine sessions.
- `recognizer.decode_offline(&segment)` is a blocking batch call per detected segment. Moonshine-tiny is fast (~50-100ms per segment on CPU); Moonshine-base is ~150-300ms. Well within the audio recv_timeout budget.
- `spectral_denoise` is still applied — same as the Zipformer path — because RTL-SDR squelch tails confuse Silero VAD just as much as they confuse decoders.

### UI changes

**`sherpa_model.rs` `supports_partials` drives contextual visibility of `display_mode_row`:**

`transcript_panel.rs::build_transcript_panel` extends the existing `model_row.connect_selected_notify` closure (which currently persists the index) with a second handler that toggles `display_mode_row.set_visible()`:

```rust
#[cfg(feature = "sherpa")]
{
    let display_mode_row_for_visibility = display_mode_row.clone();
    model_row.connect_selected_notify(move |r| {
        let idx = r.selected() as usize;
        if let Some(model) = sdr_transcription::SherpaModel::ALL.get(idx).copied() {
            display_mode_row_for_visibility.set_visible(model.supports_partials());
        }
    });
}
```

Initial visibility (at panel build) is set from `saved_model_idx` lookup.

**All settings lock during transcription (walking back PR 4's `display_mode_row` exception):**

The `enable_row.connect_active_notify` "on" branch adds:
```rust
#[cfg(feature = "sherpa")]
display_mode_row.set_sensitive(false);
```

The "off" branch, sync start-error path, and async Error-arm teardown each mirror `set_sensitive(true)`. The PR 4 inline comment explaining "display_mode_row is intentionally NOT locked" gets replaced with one explaining the new rule: all transcription settings lock during a session for mid-session fault tolerance.

Rationale for the reversal:
1. Simpler mental model — one rule, no exceptions.
2. Eliminates model-switch-during-session edge cases for the display_mode_row visibility toggle (model_row is already locked during session, so visibility updates only happen while stopped — clean).
3. Consistency with model_row, silence_row, noise_gate_row which already lock.
4. PR 4's "flip mid-session to see immediate effect" value prop was nice-to-have, not essential. Users can stop → change → start, same end result.

**No changes to `window.rs` event handlers.** The `Partial` arm already guards on `live_line_weak.upgrade()` and conditionally writes; Moonshine simply never emits partials, so the arm is a no-op for Moonshine sessions. The `Text` arm (which clears the live line) still fires — Moonshine emits Text via VAD-triggered decode — and clears an already-empty live line as a harmless no-op.

### Main.rs / splash driver changes

The splash label mapper (wherever `SplashController::update_text` is currently driven by `InitEvent`s) changes from hardcoded strings to `format!("Downloading {component}...")` / `format!("Extracting {component}...")`. One string update path for both Zipformer and Moonshine paths.

## UX decisions

1. **Live captions toggle is hidden for Moonshine models.** Because Moonshine emits no partials, the Live/Final distinction is meaningless for Moonshine sessions. The toggle only appears when `supports_partials() == true` (Zipformer currently, Parakeet later).

2. **Committed text with Moonshine appears ~100-300ms after speech ends.** User experience is "I stop talking, the text pops up a moment later" rather than "I speak, text streams as I go". Document this in the PR description so users set expectations.

3. **Model switching must be done with transcription stopped.** Per the "all settings lock during session" rule. The model picker re-enables on stop; user switches; user re-enables. Visibility of `display_mode_row` follows the new model's `supports_partials()` immediately.

4. **Splash sequential downloads for Moonshine first-run.** The splash shows VAD download → Moonshine bundle download → extract → create recognizer → ready. Two download progress phases in sequence rather than one. Acceptable UX because VAD is tiny (~2MB, ~1-2 seconds on any connection).

## Error handling

**Silero VAD download failure:** Treated identically to Moonshine bundle download failure — `store_init_failure(BackendError::Init(...))` + `InitEvent::Failed { message }`. User sees a clear error on the splash and the backend reports it via `start()`.

**OfflineRecognizer creation failure:** Same pattern as OnlineRecognizer — `store_init_failure` + `InitEvent::Failed`.

**VAD emits no segments for a long period:** Not an error condition. The session loop just keeps accumulating audio and waiting for VAD to fire. If the user was truly silent, that's correct behavior.

**Per-model filesystem lock:** Same limitation as current Zipformer download (documented on issue #255). Two concurrent `sdr-rs` instances on first run can race on scratch `.part` paths. The Moonshine + VAD download paths inherit this limitation without making it worse.

## Testing strategy

**Unit tests (added):**
- `SherpaModel::supports_partials` returns `true` for Zipformer, `false` for both Moonshine variants
- `SherpaModel::kind` returns the correct `ModelKind` per variant
- `ModelFilePaths` pattern matching: Transducer variant has 4 fields, Moonshine variant has 3 fields
- `SherpaModel::ALL` has exactly 3 entries and all are distinct

**Unit tests (NOT added — same rationale as PR 3/4):**
- Anything that touches `dirs_next::data_dir()` — see the NOTE comment in `sherpa_model.rs` tests, tracked via a future hermetic-testing refactor
- `SherpaSileroVad` internals — thin FFI wrapper, tested via integration smoke

**Build matrix (dual-build rule):**
- `cargo build --workspace` (Whisper CUDA default) — must compile clean with zero warnings
- `cargo build --workspace --no-default-features --features sherpa-cpu` — must compile clean
- `cargo clippy --all-targets --workspace -- -D warnings` (Whisper)
- `cargo clippy --all-targets --workspace --no-default-features --features sherpa-cpu -- -D warnings`
- `cargo test --workspace` (both flavors)
- `cargo fmt --all -- --check`

**Manual smoke test (user-driven):**

1. **Clean first-run for each Moonshine variant** — delete `~/.local/share/sdr-rs/models/sherpa/silero-vad/` + `moonshine-tiny-en/`. Launch. Verify splash shows "Downloading Silero VAD..." → "Downloading Moonshine Tiny..." → "Extracting Moonshine Tiny..." → "Creating recognizer..." → window opens. Repeat for Base.

2. **Zipformer regression** — select Streaming Zipformer, enable transcription, verify live captions still stream in place exactly as PR 4 shipped. Display mode row visible. Mode toggle works. Clear button clears both. This must not regress.

3. **Moonshine Tiny happy path** — select Moonshine Tiny, confirm Display mode row is hidden, enable transcription, feed known audio. Verify text appears in text view on utterance boundaries (after ~100ms of silence). Verify no live line ever shows. Verify Text commit still adds timestamped lines.

4. **Moonshine Base happy path** — same as Tiny but with Base selected. Latency will be noticeably higher per utterance; should still feel responsive.

5. **Model switching (stopped)** — with transcription stopped, switch between Zipformer / Moonshine Tiny / Moonshine Base. Verify Display mode row toggles visibility based on selection. Verify no crashes / no stale state.

6. **Settings lock during session** — enable transcription on any model. Try to interact with Model picker, Noise gate slider, Display mode row (where applicable). Confirm all are disabled. Toggle transcription off. Confirm all re-enable.

7. **Whisper regression (deferred to tomorrow morning per user preference, like PR 4)** — whisper-cuda build must compile and behave exactly as shipped. This is near-zero-risk because all Moonshine code is `#[cfg(feature = "sherpa")]`.

## Follow-up issue (file before merge)

**Title:** Retrofit Whisper backend to use VoiceActivityDetector trait

**Body outline:**
- Context: PR for Moonshine (#TBD) introduced a feature-agnostic `VoiceActivityDetector` trait in `sdr-transcription/src/vad.rs` plus a sherpa-onnx-backed `SherpaSileroVad` impl.
- Current state: Whisper backend uses RMS-gated chunk detection (crude, splits utterances on RTL-SDR squelch tails).
- Goal: Add a second `VoiceActivityDetector` impl for Whisper builds using a pure-Rust Silero crate (no sherpa-onnx dep — preserves the feature mutex). Candidates: `voice_activity_detector`, `silero-rs`, or vendoring Silero VAD directly. Wire Whisper's session loop to use the trait instead of RMS.
- Acceptance: Whisper transcripts stop splitting mid-utterance on squelch tails. Whisper builds still compile without any sherpa-onnx deps.
- Labels: `enhancement`, `transcription`

## Open questions

None. All design decisions above have been ratified.

## Risks / unknowns

1. **Moonshine decoder latency on CPU** — documented in the Moonshine paper as sub-100ms per utterance, but we haven't benchmarked it on our specific session loop. If Base is too slow on the user's RTX 4080 Super (unlikely, GPU isn't used by sherpa-onnx by default), we may want to restrict Base to CPU-only or surface a warning. Mitigated by keeping Tiny as the recommended default.

2. **Silero VAD false negatives on radio audio** — Silero is trained on clean speech. RTL-SDR audio is noisy, compressed, band-limited. If VAD misses end-of-speech on real radio chatter, Moonshine will accumulate infinitely. Mitigated by `spectral_denoise` before VAD (same pre-processing as Zipformer), and by VAD's internal `max_speech_duration` (20s default) forcing a segment break. Worst case: user switches to a different model.

3. **`OfflineRecognizer` + `VoiceActivityDetector` simultaneous ownership on one host thread** — sherpa-onnx types are generally `!Send`. If either type holds interior state that conflicts with cross-call access from the same thread, we'd see undefined behavior. Sherpa upstream examples use them together in this exact pattern, so this risk is low but worth naming. Mitigated by the existing host-thread pattern — both live in `RecognizerState::Offline` on the same worker thread, accessed sequentially in the session loop.

4. **PR 4 reversal risk** — CodeRabbit might flag the `display_mode_row` lock as a regression from PR 4's "intentionally NOT locked" comment. Mitigated by the new inline comment explaining the rule change and the rationale in this spec.
