# Auto Break Segmentation Design

**Status:** Design approved 2026-04-13
**Tracking:** #265 (from PR 6 follow-up batch)
**Branch:** `feature/auto-break-segmentation` (not yet created)

## Goal

Add a new segmentation mode for offline sherpa transcription (Moonshine Tiny/Base, Parakeet-TDT) that uses the radio's existing squelch gate as the utterance boundary instead of Silero VAD. On NFM scanner audio, the radio's squelch is a cleaner physical-layer answer to "is there a transmission to transcribe right now" than running a speech/non-speech classifier over audio that already went through a signal-detection gate. The user's scanner testing of the sherpa-cuda PR 7 build showed the GPU is effectively free for Parakeet inference and the remaining bottleneck is VAD accuracy on noisy RF input — Auto Break is the feature that takes advantage of that spare compute.

## Motivation

Current behavior on NFM scanner use:

- Silero VAD runs over the AF-chain audio and produces segment boundaries based on generic speech/non-speech classification
- The AF audio always includes squelch-tail hiss and occasional false openings, which Silero sometimes misclassifies as speech
- User-tuned `vad_threshold` (from PR 6) helps but is fighting the underlying problem — Silero was trained on clean speech, not narrow-band RF with squelch-noise contamination

Proposed: use the physical squelch gate, which the radio already tracks, as the ground truth for "a transmission is happening." The radio knows when signal crossed the noise floor; it shouldn't have to ask a machine learning model to also figure that out.

## Non-goals

- **Not** replacing VAD globally — Silero VAD remains the default for offline sherpa models when Auto Break is off, and the only supported segmentation for the streaming Zipformer path. Auto Break is an opt-in alternative, not a replacement.
- **Not** applying to streaming Zipformer. Zipformer's own endpoint detection already handles within-transmission phrase boundaries; Auto Break would introduce races with it. Scoped to offline models only.
- **Not** adding user-tunable sliders for the hold-off timing constants in v1. Ship sensible defaults, file a follow-up issue to expose sliders if and only if real-world testing shows the defaults fail.
- **Not** touching the Whisper backend. `TranscriptionInput`'s new enum variants are passed through but ignored by the Whisper session loop in v1. Whisper retrofit is tracked separately in #259.
- **Not** adding a visual squelch open/close indicator to the UI. Orthogonal improvement, candidate for a separate small PR.
- **Not** changing how auto-squelch works. Auto Break consumes whatever squelch state the user has configured (auto or manual); the transcription setting never reaches over and modifies radio state.

## Critical correction to the PR 6 follow-up-issue notes

Issue #265 was filed under the working title "squelch-gated transcription." The naming during this design session shifted to "Auto Break" because the feature is really about using the natural gaps between transmissions as segmentation markers rather than about gating audio feed to the recognizer. Functionally they're close, but "Auto Break" is the user-facing name going forward — matches scanner radio terminology and is what the UI toggle will read.

## Architecture

### Data flow

```text
RadioModule::if_chain::squelch_open() -> bool   [exists today, unchanged]
    |
    |  (polled once per AF chunk)
    v
sdr-core::controller::DspState
    |
    |  detects edge transitions on squelch_open()
    |  AND gates emission on current_mode == DemodMode::Nfm
    v
mpsc::SyncSender<TranscriptionInput>   [type change, previously SyncSender<Vec<f32>>]
    |
    v
sdr-transcription::backends::sherpa::host::SherpaHost
    |
    v
sdr-transcription::backends::sherpa::offline::run_session
    |
    v
Auto Break state machine  (new, mutex with Silero VAD path)
    |
    v
OfflineRecognizer::decode()   [existing, unchanged]
```

The `sdr-core` controller is the only place that sees both squelch state AND demod mode, so it's the natural producer of squelch events. Keeping emission gated on `demod == DemodMode::Nfm` means downstream backends never have to worry about whether squelch events are meaningful — if they arrive, they are.

### Data type changes

#### `sdr-transcription::backend::TranscriptionInput`

New enum that replaces the current `Vec<f32>` audio-tap contract:

```rust
/// Frames sent from the DSP controller into a transcription backend.
///
/// Carries both raw audio samples and segmentation-boundary hints. The
/// boundary variants are emitted only when the current demod mode is
/// NFM — the controller handles the gating so backends don't have to.
///
/// Backends that don't care about squelch-based segmentation (Whisper,
/// streaming Zipformer, offline sherpa in `SegmentationMode::Vad`)
/// pattern-match on `Samples` and drop the other variants.
#[derive(Debug)]
pub enum TranscriptionInput {
    /// Interleaved-stereo f32 PCM at `SAMPLE_RATE_HZ`. Always emitted,
    /// gap-free, at a cadence determined by the audio pipeline.
    Samples(Vec<f32>),

    /// Radio squelch just opened. Edge event, emitted exactly once per
    /// close→open transition. Only emitted when the current demod mode
    /// is NFM.
    SquelchOpened,

    /// Radio squelch just closed. Edge event, emitted exactly once per
    /// open→close transition. Only emitted when the current demod mode
    /// is NFM.
    SquelchClosed,
}
```

The existing `sdr-core::DspState::transcription_tx: Option<mpsc::SyncSender<Vec<f32>>>` field changes to `Option<mpsc::SyncSender<TranscriptionInput>>`. All producer and consumer sites update together. Keeping the channel bounded (via `SyncSender` rather than unbounded `Sender`) is deliberate: backpressure on the DSP thread prevents unbounded memory growth if the backend stalls, and the `TrySendError::Full` path on squelch edge events drives the retry-next-block logic in the controller that preserves state-machine transition integrity under load.

#### `sdr-transcription::backend::SegmentationMode`

New enum on `BackendConfig` that decides which segmentation engine drives the offline session:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SegmentationMode {
    /// Silero VAD drives segmentation. The only valid mode for streaming
    /// Zipformer, and the default for offline sherpa models unless the
    /// user explicitly opts into Auto Break.
    #[default]
    Vad,

    /// Auto Break: use the radio's squelch gate as the segmentation
    /// boundary. Valid only for offline sherpa models in NFM mode.
    /// See `backends/sherpa/offline.rs` for the state machine that
    /// consumes `SquelchOpened` / `SquelchClosed` events.
    AutoBreak,
}
```

`BackendConfig` gains one field:

```rust
pub struct BackendConfig {
    // ... existing fields ...
    pub vad_threshold: f32,           // existing, only read when mode == Vad
    pub segmentation_mode: SegmentationMode,  // NEW
}
```

### Controller-side changes (sdr-core)

`DspState` grows a `squelch_was_open: bool` field initialized to `false`. On each AF chunk emitted from the radio module:

1. Read `radio.if_chain().squelch_open()` → `now_open: bool`
2. Send `TranscriptionInput::Samples(chunk)` unconditionally (gap-free audio preserved)
3. If `now_open != squelch_was_open` AND `radio.current_mode() == DemodMode::Nfm`:
   - Emit `SquelchOpened` or `SquelchClosed` to the transcription channel
4. Update `squelch_was_open = now_open`

Gating on `current_mode == DemodMode::Nfm` means the DSP controller only emits squelch events on NFM; WFM / AM / other modes never see them. Mid-session mode changes are handled separately by the universal stop-on-mode-change behavior described below — when the user switches band, transcription stops cleanly before any cross-mode sample leakage can reach the session loop.

### Offline session loop state machine

`backends/sherpa/offline.rs::run_session` currently assumes Silero VAD drives segmentation. It gains a branch on `config.segmentation_mode`:

```rust
match config.segmentation_mode {
    SegmentationMode::Vad => run_session_vad(...),       // existing code, renamed
    SegmentationMode::AutoBreak => run_session_auto_break(...),  // NEW
}
```

Hardcoded timing constants at the top of the offline module:

```rust
/// Squelch openings shorter than this are treated as noise spikes and
/// produce no segment. Chosen to exclude sub-syllable blips while still
/// catching short single-word transmissions ("copy").
const AUTO_BREAK_MIN_OPEN_MS: u32 = 100;

/// Continue buffering audio for this long after the squelch closes, so
/// the last syllable of the transmission isn't chopped by a tight
/// squelch-close timing. Chosen empirically from the typical fall time
/// of a PowerSquelch transition plus ~100 ms of spoken-word tail.
const AUTO_BREAK_TAIL_MS: u32 = 200;

/// Segments shorter than this are discarded instead of being fed to
/// the recognizer. Moonshine and Parakeet both hallucinate badly on
/// sub-word fragments, so dropping them is an accuracy improvement,
/// not a loss.
const AUTO_BREAK_MIN_SEGMENT_MS: u32 = 400;

/// Safety cap: if squelch stays open longer than this, flush the
/// buffer anyway. Protects against pathological stuck-open situations
/// (bad auto-squelch, carrier jam, band opening) that would otherwise
/// cause unbounded memory growth.
const AUTO_BREAK_MAX_SEGMENT_MS: u32 = 30_000;
```

State machine — implemented as a Rust enum + `recv_timeout` loop, no extra threads:

```text
┌─────────────────────────────┐
│ Idle                        │
│  - buffer: empty             │
│  - waiting for SquelchOpened │
└─────────────────────────────┘
    │
    │ SquelchOpened
    │ (record open_time, clear buffer)
    v
┌─────────────────────────────┐
│ Recording(open_time)        │
│  - buffer: growing           │
│  - append incoming Samples   │
└─────────────────────────────┘
    │
    │ SquelchClosed
    v
┌─────────────────────────────┐
│ HoldingOff(close_time)      │
│  - buffer: still growing     │
│    (capture trailing tail)  │
│  - recv_timeout = TAIL_MS    │
└─────────────────────────────┘
    │
    ├─── SquelchOpened ─────────┐
    │    (cancel hold-off,       │
    │     back to Recording,     │
    │     same buffer — this is │
    │     a hysteresis blip)     │
    │                            │
    │ ◄──────────────────────────┘
    │
    │ timeout(TAIL_MS)
    v
┌─────────────────────────────┐
│ Evaluate buffer:             │
│                              │
│ if duration < MIN_OPEN_MS:   │
│   discard (phantom open)     │
│                              │
│ elif duration < MIN_SEGMENT: │
│   discard (too short for     │
│   recognizer accuracy)       │
│                              │
│ else:                        │
│   OfflineRecognizer::decode()│
│   emit Text event            │
│                              │
│ → Idle                       │
└─────────────────────────────┘
```

**Max-segment safety**: in the `Recording` state, if the current buffer duration exceeds `AUTO_BREAK_MAX_SEGMENT_MS`, the state machine force-transitions to an evaluation step as if `HoldingOff` had timed out. This catches stuck-open squelch situations without letting the buffer grow unbounded.

**recv_timeout plumbing**: the session loop already uses an mpsc receiver. Switch to `recv_timeout` in the `HoldingOff` state with the remaining hold-off duration. `Recording` state uses a blocking `recv` (no timer needed). `Idle` state uses blocking `recv`. Single-threaded, no extra coordination primitives.

### Mode-change behavior — universal transcription stop

**Demod mode is a top-level boundary. Any change to `current_mode` while transcription is active stops the session, regardless of which segmentation mode is configured (VAD or Auto Break).** This is a behavior change from PRs 1–6 — today, mid-session mode changes continue transcription in whatever state the recognizer happens to be in. The new behavior treats the demod mode as part of the session's conceptual identity, so changing it requires an explicit restart.

**Rationale**:

- "Changing band" (in user language) is the top-level "I'm listening to something different now" signal. Tying transcription to it matches scanner/radio user intuition.
- Eliminates the WFM fallback question entirely for Auto Break — no state-machine edge cases around Samples-flowing-without-events, no "Auto Break is configured but inert" mental-model trap.
- Applies uniformly to VAD mode too, which benefits — Whisper's RMS gating in particular drifts badly across band changes because different bands have different noise characteristics, so a clean restart is an accuracy improvement.
- Simpler state model: no mid-session dual-mode handoff logic anywhere.

**Implementation**:

- A new `DspToUi::DemodModeChanged(DemodMode)` event is emitted by the controller whenever `current_mode` changes. This event already exists in the spec for UI visibility-rule updates on the Auto Break toggle; we reuse it here.
- The transcript panel subscribes to `DemodModeChanged`. If transcription is currently active when the event fires, it calls `backend.stop()` via the existing stop path and shows a toast (see below).
- Backend config (including `segmentation_mode`, `vad_threshold`, model selection) is preserved across the stop — the user's configuration is intact, they just need to click Start to resume on the new band.
- Config persistence is unchanged — config keys remain valid across the stop/start cycle.

**User-facing explainer**:

- **Proactive**: the demod mode selector (a `gtk4::DropDown` in the header bar — not an `AdwComboRow`, so it has no subtitle slot) gains an extended `tooltip_text` reading *"Demodulation mode — changing modes stops active transcription"*. Hover-visible warning before the user triggers the stop. This was a late adjustment from an earlier spec draft that assumed the selector was an `AdwComboRow` with a subtitle slot; the tooltip is the closest equivalent on the actual header `DropDown` widget.
- **Reactive**: when transcription actually stops due to a mode change, the transcript panel (or the main window's toast overlay) shows an `AdwToast` titled *"Transcription stopped — demod mode changed. Press Start to resume."* — 65 characters, one line even in a narrow window.

**What this replaces**: the earlier draft of this spec proposed fallback semantics for Auto Break on non-NFM modes (state machine stays idle indefinitely, transcription silently goes quiet). That approach is now dropped in favor of the universal stop, which produces equivalent behavior with a much clearer user signal (explicit toast vs silent quiet).

### UI — transcript panel

One new `AdwSwitchRow` added to the transcript panel below the VAD threshold SpinRow:

```text
┌──────────────────────────────────────────────┐
│ Transcription                                │
│                                              │
│ [Model       ] [Parakeet TDT 0.6b      ▾]    │
│ [Enable      ] [ ○ ]                         │
│                                              │
│ [VAD threshold]  ├─────●──────┤   0.30       │  ← hidden when Auto Break ON
│                                              │
│ [Auto Break  ] [ ● ]                         │  ← NEW, NFM + offline only
│   Use the radio's squelch as the             │
│   transcription boundary instead of VAD.     │
│   NFM only.                                  │
│                                              │
│ ┌────────────────── transcript ─────────┐    │
│ │                                        │    │
│ └────────────────────────────────────────┘    │
└──────────────────────────────────────────────┘
```

**Visibility rules**: the Auto Break row is visible when ALL of:

1. The selected sherpa model is offline (`!SherpaModel::supports_partials()`)
2. The current demod mode is `DemodMode::Nfm`
3. The `sherpa` cargo feature is active (automatically true if this panel is rendering)

**Mutex visibility with VAD threshold slider**: when the Auto Break toggle is ON, the VAD threshold SpinRow hides. When the Auto Break toggle is OFF, the VAD threshold SpinRow shows as today. Both rows share the offline-model visibility check; the mutex is a second layer on top.

**Subtitle copy**: `"Use the radio's squelch as the transcription boundary instead of VAD. NFM only."` — matches AdwPreferencesRow subtitle slot, serves as inline explainer for operators who don't yet understand the segmentation system.

**Demod mode reactivity**: transcript panel currently has no `mode-changed` signal input from the engine. We add one — a new `DspToUi::DemodModeChanged(DemodMode)` event emitted from the controller whenever `current_mode` changes, subscribed to by the transcript panel to re-run visibility checks. Small plumbing addition, reusable later for other mode-gated UI.

**Persistence**: new config key `transcription_auto_break_enabled: bool`, default `false`. Session-locked during active transcription per the PR 5 pattern — the Auto Break toggle and the VAD threshold slider both become insensitive when a session is running.

**Precondition check at session start**: if the user has Auto Break enabled AND the radio has squelch fully disabled (not auto-squelch, not manual threshold — literally off), session start is blocked with a toast:

```text
Auto Break needs squelch enabled to detect transmission boundaries.
Enable squelch in the radio panel, or turn off Auto Break to use VAD.
```

The user clears the error by either enabling squelch or turning off Auto Break, then re-clicks Start. The precondition check reads a new public accessor `IfChain::is_squelch_configured() -> bool` which returns `true` when either auto-squelch is enabled OR a manual threshold is set, and `false` only when squelch is fully disabled. `sdr-radio` currently exposes `squelch_open() -> bool` (runtime gate state) but not the configuration state, so this is a small API addition: two lines in `PowerSquelch` to track the "enabled" flag and one forwarding method on `IfChain`.

### Backend selection guard

`backends/sherpa/offline.rs::run_session` validates at entry:

```rust
if config.segmentation_mode == SegmentationMode::AutoBreak
    && !matches!(model.kind(), ModelKind::OfflineMoonshine | ModelKind::OfflineNemoTransducer)
{
    return Err(BackendError::Init(
        "Auto Break is only supported for offline sherpa models \
         (Moonshine, Parakeet). Streaming Zipformer must use Vad.".to_owned()
    ));
}
```

Streaming Zipformer's session loop asserts the same precondition but in `run_session_online` — rejects `SegmentationMode::AutoBreak` with a clear error. Should never fire because UI prevents the combination, but defensive guard protects against config file corruption or API misuse.

## Error handling

All error paths reuse the existing sherpa backend infrastructure:

- Squelch-disabled precondition failure → toast + session start blocked, no new error variant
- Auto Break + streaming Zipformer combination → rejected at session start with `BackendError::Init`, caught by the existing session-init error path in `window.rs`
- Max-segment safety flush → emits a tracing `warn!` log line ("Auto Break buffer exceeded 30s — forcing flush, consider checking squelch configuration") and proceeds to normal decode
- Segment discarded due to MIN_OPEN or MIN_SEGMENT filter → tracing `debug!` line, no user-visible event (these are expected in normal operation)
- Recognizer decode failure inside `OfflineRecognizer::decode()` → existing error path emits `TranscriptionEvent::Error`

No new `BackendError` variants needed.

## Testing strategy

**Unit tests in `backends/sherpa/offline.rs`**:

- `auto_break_state_machine_clean_utterance` — simulate `SquelchOpened → Samples × N → SquelchClosed → timeout`, assert single decode call with the buffered samples
- `auto_break_hysteresis_blip_single_utterance` — simulate `Open → Samples → Close → Samples → Open → Samples → Close → timeout`, assert single decode call containing all samples (blip ignored)
- `auto_break_phantom_open_discarded` — simulate `Open → Close` within MIN_OPEN_MS, assert zero decode calls
- `auto_break_sub_min_segment_discarded` — simulate `Open → 300 ms of samples → Close`, assert zero decode calls (below MIN_SEGMENT_MS)
- `auto_break_max_segment_safety_flush` — simulate `Open → 31 s of samples`, assert decode call triggered by the safety cap, state returns to Recording-equivalent for the remaining stream
- `auto_break_rejects_streaming_model` — `BackendConfig { segmentation_mode: AutoBreak, model: StreamingZipformerEn }` at session start returns `BackendError::Init` with the expected message
- `auto_break_vad_mutex_default_is_vad` — `BackendConfig::default().segmentation_mode == SegmentationMode::Vad`

**Unit tests for the mode-change stop behavior** (in `sdr-ui::sidebar::transcript_panel` or equivalent):

- `mode_change_stops_active_transcription_vad_mode` — given an active session in `SegmentationMode::Vad`, simulate a `DemodModeChanged` event, assert the backend stop path is invoked and the toast is shown with the expected copy
- `mode_change_stops_active_transcription_auto_break_mode` — same scenario but with `SegmentationMode::AutoBreak`, assert equivalent behavior (universal, not feature-gated)
- `mode_change_preserves_config` — after the stop triggered by mode change, assert that `BackendConfig` (VAD threshold, segmentation mode, model selection) is unchanged in the panel state
- `mode_change_without_active_session_is_noop` — simulate `DemodModeChanged` when transcription is not running, assert no toast, no spurious stop calls, config unchanged

**Integration smoke test (user-driven)**:

1. **Clean regression pass** — with Auto Break OFF, all three sherpa models still work per PR 7 baseline
2. **Auto Break with Parakeet on NFM scanner** — enable Auto Break, scanner for 10 minutes on a real NFM frequency, verify transcript shows one entry per transmission with no mid-transmission splits and no squelch-tail garbage
3. **Auto Break with Moonshine Base on NFM scanner** — same test as above, Moonshine should produce shorter transcripts faster
4. **Hysteresis blip test** — find a weak station that drops below squelch briefly mid-transmission, verify the transmission is captured as one segment not two
5. **Phantom open test** — tune to a dead channel with auto-squelch, verify no spurious transcript entries from noise-spike openings
6. **Mode switch with Auto Break on** — switch NFM → WFM mid-session, verify transcription stops cleanly, verify the toast appears reading *"Transcription stopped — demod mode changed. Press Start to resume."*, switch back to NFM, click Start, verify session resumes with Auto Break configuration intact
7. **Mode switch with VAD mode on (behavior change regression)** — same as #6 but with Auto Break OFF (plain VAD mode). Verify transcription ALSO stops on mode change and shows the same toast. This is a behavior change from PR 1–6 and needs an explicit confirm-this-is-what-we-want check.
8. **Squelch-disabled precondition** — disable squelch entirely in the radio panel, try to start transcription with Auto Break on, verify toast appears and session does not start
9. **VAD regression (within a session)** — disable Auto Break, verify VAD threshold slider reappears and existing PR 6 VAD behavior within a single-mode session is unchanged
10. **Demod selector tooltip** — hover over the demod mode `DropDown` in the header bar and verify the tooltip reads *"Demodulation mode — changing modes stops active transcription"*

**Pre-PR smoke test checklist (user-driven, runs before the PR is opened)**:

This is the structured regression pass that catches anything the unit tests and CI don't cover — end-to-end user flows on real RF, touching both the new Auto Break feature and everything it might accidentally break via the `TranscriptionInput` channel refactor. Every item is a concrete observable behavior; check each box only after verifying it on the running binary.

**Build + launch sanity (covers the channel refactor on non-sherpa paths)**:

- [ ] `make install CARGO_FLAGS="--release"` (default whisper-cpu) builds and `sdr-rs` launches without error
- [ ] `make install CARGO_FLAGS="--release --no-default-features --features sherpa-cuda"` builds and `sdr-rs` launches without error
- [ ] Whisper default: start transcription on a live NFM signal, confirm RMS-gated commits still appear in the transcript (if the channel refactor broke the Whisper consumer, this is where it surfaces)

**Sherpa model regression pass (Auto Break OFF, VAD mode)**:

- [ ] Zipformer streaming: live caption line renders partials, commits on endpoint detection, no crashes on mode switch within NFM (e.g. freq change, same demod)
- [ ] Moonshine Base: VAD-gated offline commits appear per utterance on an NFM scanner recording, VAD threshold slider visible and responsive
- [ ] Parakeet-TDT: VAD-gated offline commits appear per utterance on an NFM scanner recording, GPU allocation visible in `nvidia-smi` during inference

**VAD threshold slider regression**:

- [ ] VAD threshold SpinRow visible when an offline model is selected AND Auto Break is OFF
- [ ] VAD threshold SpinRow hidden when Zipformer is selected (streaming has its own endpoint detection)
- [ ] VAD threshold value persists across app restart (existing PR 6 behavior)

**Auto Break — new feature**:

- [ ] Auto Break toggle visible when `(offline model)` AND `(current demod mode == NFM)`
- [ ] Auto Break toggle hidden when current demod mode is WFM / AM / other
- [ ] Auto Break toggle hidden when Zipformer is selected, even on NFM
- [ ] Auto Break ON → VAD threshold SpinRow hides (mutex visibility)
- [ ] Auto Break OFF → VAD threshold SpinRow reappears
- [ ] Auto Break toggle is disabled during an active transcription session (session lock)
- [ ] Auto Break setting persists across app restart (`transcription_auto_break_enabled` config key)
- [ ] Auto Break ON + Moonshine Base + real NFM scanner audio: one clean transcript entry per transmission, no squelch-tail garbage, no mid-utterance splits on brief dips
- [ ] Auto Break ON + Parakeet + real NFM scanner audio: same as above, with noticeably higher recognition accuracy
- [ ] Auto Break ON + Parakeet + dead channel (auto-squelch, no signal): zero spurious transcript entries from noise-spike openings
- [ ] Auto Break ON + squelch disabled in the radio panel + click Start: precondition toast appears, session does NOT start
- [ ] Auto Break ON + squelch re-enabled + click Start: session starts normally

**Universal mode-change stop — new behavior**:

- [ ] Proactive tooltip *"Demodulation mode — changing modes stops active transcription"* visible on hover over the header-bar demod mode `DropDown`
- [ ] VAD mode active + switch NFM → WFM: transcription stops, toast appears with exact text *"Transcription stopped — demod mode changed. Press Start to resume."*
- [ ] Auto Break mode active + switch NFM → WFM: same stop + same toast
- [ ] After mode-change stop, click Start: transcription resumes on the new band with configuration preserved (model, VAD threshold, Auto Break setting if applicable)
- [ ] Mode change while transcription is NOT running: no toast, no error, no state change — the demod mode just switches silently as it does today

**Whisper regression (feature mutex is intact)**:

- [ ] `make install CARGO_FLAGS="--release --features whisper-cuda"` builds and `sdr-rs` launches — the `TranscriptionInput` channel refactor MUST NOT break the Whisper feature path; Whisper pattern-matches on `Samples` only and ignores the squelch variants

**Deliverable of this checklist**: a paste of the filled-in list in the PR description, so CodeRabbit and future readers can see exactly what was validated before merge.

**Triple-build verification (per transcription-feature protocol)**:

- `cargo check --workspace` (default whisper-cpu)
- `cargo check --workspace --features whisper-cuda`
- `cargo check --workspace --no-default-features --features sherpa-cpu`
- `cargo check --workspace --no-default-features --features sherpa-cuda`
- `cargo clippy --all-targets --workspace -- -D warnings` (default + sherpa-cpu + sherpa-cuda)
- `cargo test --workspace` (default)
- `cargo fmt --all -- --check`
- `cargo deny check sources` (default + sherpa-cuda graph)

## Risks and unknowns

1. **Hardcoded timing constants may be wrong for real-world NFM.** Mitigation: ship with the user's local scanner testing as the validation signal; if defaults are wrong, we have the follow-up issue ready to add sliders. The constants are one-line changes if tuning is needed before ship.

2. **Squelch configuration accessor doesn't exist yet.** `IfChain` exposes `squelch_open() -> bool` but not the user's configured level or the enable flag. We need to add one new accessor so the precondition check can tell "squelch is off" from "squelch is set but not currently open." Small public API addition to `sdr-radio`; risk is low.

3. **Channel type change to `TranscriptionInput`.** Breaking change to the internal audio-tap contract, touches at least: `sdr-core::controller`, `sdr-transcription::backend`, `sdr-transcription::backends::sherpa::host`, `sdr-transcription::backends::sherpa::offline`, `sdr-transcription::backends::sherpa::streaming`, `sdr-transcription::backends::whisper::*`. All edits are mechanical (wrap `Vec<f32>` in `TranscriptionInput::Samples(...)` at the producer, pattern-match at consumers) but the patch is wide. Mitigation: test each transcription flavor before committing.

4. **Demod mode change plumbing.** We need a `DspToUi::DemodModeChanged` event so the transcript panel can re-run visibility checks AND trigger the stop-on-mode-change behavior. This is net-new message-enum work — small, but the messages crate is shared between several consumers and needs a clean add. Risk: low, but the touch surface grows slightly.

5. **Behavior change vs PRs 1–6.** PRs 1–6 let VAD mode transcription continue across demod mode changes; this PR changes that to a universal stop. The project has one user today (the owner), who confirmed during design that the change is fine and actively preferable — the old behavior was quietly lossy (cross-band noise characteristics break recognition quality) and the new behavior gives a clear explicit signal via toast. No soft-launch / opt-out gate is included. The PR description will still call the change out prominently for CodeRabbit and future readers, and the proactive tooltip on the demod selector acts as a passive explainer.

6. **Auto Break follow-up issue for sliders.** User agreed to ship hardcoded constants with a follow-up issue to add sliders once real-world testing is in. The follow-up issue needs to be filed before merge so we don't forget.

## Follow-up issues to file before merge

**Title**: Expose Auto Break timing parameters as user-tunable sliders

**Body outline**:

- Context: PR for #265 ships Auto Break with hardcoded `MIN_OPEN_MS`, `TAIL_MS`, `MIN_SEGMENT_MS` constants in `backends/sherpa/offline.rs`.
- Problem: no one-size-fits-all for real-world NFM — different bands, repeaters, mobile signals, and scanner use cases have different optimal hold-off values. The VAD threshold slider in PR 6 is precedent for exactly this kind of tuning surface.
- Proposed: add three new SpinRows to the transcript panel under the Auto Break toggle (visible only when Auto Break is on). Persist as `transcription_auto_break_min_open_ms` / `_tail_ms` / `_min_segment_ms` config keys. Defaults match the v1 hardcoded values.
- Acceptance: real-world scanner testing with the slider values tuned produces cleaner segment boundaries than the defaults on at least one representative recording.
- Labels: `enhancement`, `transcription`, `ui`

## Open questions

None. All design decisions confirmed with user during brainstorming on 2026-04-13.
