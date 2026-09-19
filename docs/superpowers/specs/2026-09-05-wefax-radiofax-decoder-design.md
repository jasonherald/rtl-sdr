# WEFAX / HF Radiofax Decoder — Design (#877)

**Goal:** Decode HF radiofax (WEFAX) weather-chart broadcasts — the shortwave
cousin of APT — end to end: tune in a dedicated WEFAX demod mode, decode the
FM-subcarrier image on the DSP thread into a live-updating greyscale chart, and
render/export it. No TLEs, no passes: continuous long-form reception.

**Status:** Design approved 2026-09-05. Full end-to-end build (DSP + demod mode
+ shared image handle + controller tap + messages + live viewer + save path) in
one PR.

**Ticket:** #877. Relates to #899 (signal-decode backlog) and #867 (per-mode
surfaces — the eventual home for a dedicated WEFAX activity).

---

## Background — the WEFAX signal

- **Modulation:** an FM audio subcarrier carried in an **USB** voice-bandwidth
  channel. Brightness is encoded as tone *frequency*: **1500 Hz = black,
  2300 Hz = white, 1900 Hz center, ±400 Hz deviation**. (Amplitude is not the
  signal — frequency is — so the decoder is robust to fading.)
- **Line rate:** 120 lines/minute = **2 lines/second** (line period 500 ms).
- **Geometry:** **IOC 576** → pixels/line = π·IOC ≈ **1809 px/line** usable.
- **Chart framing tones (WMO):** start tone ~**300 Hz** (~5 s) identifies
  120 lpm / IOC 576; a **phasing** interval (~20–30 s) of mostly-black lines
  each carrying a short **white pulse** at a fixed position sets the left-edge
  (column-0) alignment; **stop tone ~450 Hz** (~5 s) ends the chart.
- **Tuning convention:** dial frequency = assigned − 1.9 kHz in USB (so the
  1900 Hz center sits where the transmitter intends).
- **Stations (all one decoder):** NMG New Orleans (4317.9 / 8503.9 /
  12789.9 kHz — easy, near-continuous), NMC Pt. Reyes / Boston (US alternates),
  JMH Tokyo (3622.5 / 7795 / 13988.5 kHz — DX).
- **Hardware path:** SpyVerter R2 + Airspy on HF (bias-T powered, −120 MHz
  converter offset). The RTL "V4" direct-sampling path is dead; HF goes through
  the upconverter.

> **Exact-value verification:** the tone frequencies, phasing format, and
> durations above are the **WMO standard** nominal values — that standard is
> the authoritative source, not any reference decoder. The implementation
> confirms each constant against the standard and against **observed behaviour
> in a real reference recording** (does the chart render coherently?) rather
> than trusting them blind. py_wefax is a *troubleshooting sanity reference
> only* — it is not ported and is not treated as a correctness oracle (we do
> not know it is 100% correct).

---

## Architecture

WEFAX is **structurally an APT-style continuous decoder** (running sample
counter, matched-filter/correlation sync, long-form assembly) that writes into
an **LRPT-style shared image handle** (`Arc<Mutex<>>`, one continuous growing
image). It is a clean hybrid of two patterns already in the repo:

```text
USB IF/AF chain (DemodMode::Wefax, 24 kHz AF)
        │  pre_gate_audio → downmix_pre_gate_mono
        ▼
WefaxDecoder (sdr-dsp, pure)         ── discriminator → pixel clock → sync SM → lines
        │  write_line(...)
        ▼
WefaxImageHandle (sdr-radio, Arc<Mutex>)   ── snapshot / take_completed
        │  DspToUi::WefaxLineDecoded / WefaxImageComplete
        ▼
WefaxImageView (sdr-ui viewer)       ── Cairo greyscale surface, Pause / Export PNG
```

The decoder runs on the controller thread only while `current_mode() ==
DemodMode::Wefax` and the UI has set the image handle
(`UiToDsp::SetWefaxImage`), mirroring the SSTV tap's gating discipline.

---

## Components

### 1. `crates/sdr-dsp/src/wefax.rs` — pure DSP decoder

Public surface modeled on `AptDecoder`:

```rust
pub struct WefaxDecoder { /* stateful, no I/O */ }

pub struct WefaxLine {
    pub pixels: [u8; PIXELS_PER_LINE],  // greyscale, fixed 1809 (cheap preallocated out-slice, APT-style)
    pub line_index: u32,                // 0-based within the current chart
    pub state: WefaxState,              // Phasing | Imaging (lines are only emitted once locked)
}
```

Pixels reach the UI through the **shared image handle** (`write_line` on the
controller thread), not through the message channel — `WefaxLineDecoded(u32)`
carries only the line index as a repaint nudge, and the viewer reads pixels via
`snapshot()`. So no large per-line buffer is cloned across the DSP→UI boundary
(contrast `AptLine`, which does carry its pixels and is therefore boxed).

```rust

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WefaxState { Idle, Phasing, Imaging, Stopped }

impl WefaxDecoder {
    pub fn new(input_rate_hz: u32) -> Result<Self, DspError>;
    /// Feed AF samples; append completed lines to `out`. Returns count written.
    pub fn process(&mut self, input: &[f32], out: &mut [WefaxLine])
        -> Result<usize, DspError>;
    pub fn state(&self) -> WefaxState;
    /// True on the sample that a stop tone finalized a chart (drives ImageComplete).
    pub fn take_chart_complete(&mut self) -> bool;
}
```

**Algorithm:**

1. **FM subcarrier discriminator (tone → brightness).** Analytic-signal
   instantaneous frequency — the standard FM-discriminator method (py_wefax
   happens to use the same, but that is incidental, not the reason): form the
   analytic signal (Hilbert FIR, or equivalently mix down by 1900 Hz +
   low-pass), then `inst_freq = angle(conj(prev) · analytic) · sr / (2π)`.
   Low-pass the demodulated brightness (~cutoff around the pixel bandwidth).
   Map `inst_freq`: 1500 Hz → 0 (black), 2300 Hz → 255 (white), linear, clamped.
   Greyscale (proportional) mapping is primary so halftone/satellite insets
   survive; a nearest-of-two-carriers binary threshold is the documented
   fallback.
2. **Pixel clock.** `samples_per_line = round(sr / 2)` (2 lines/s). Each pixel
   averages the discriminator output over its `samples_per_line / 1809 ≈
   6.63`-sample window, indexed off the running sample counter.
3. **Line-sync state machine** (`Idle → Phasing → Imaging → Stopped`):
   - **Start tone** (~300 Hz keying, ~5 s) → enter `Phasing`.
   - **Phasing** → detect the recurring ~25 ms white pulse per line by
     correlating a median-filtered line against a pulse kernel (py_wefax's
     approach); its position sets the **column-0 offset**. Cross-correlate
     across several phasing lines for a stable lock, then enter `Imaging`.
   - **Imaging** → assemble and emit lines (aligned to the locked offset) until…
   - **Stop tone** (~450 Hz, ~5 s) → `Stopped`, signal chart-complete, reset to
     `Idle` for the next chart.
4. **Slant correction.** TX/RX sample-clock mismatch skews long charts
   diagonally. Estimate slant from phasing-pulse drift across the phasing
   interval (and/or a coarse search) and fold it into the effective
   `samples_per_line` (`sr/2 + slant`, per py_wefax) so full charts render
   straight.

Constants (named, no magic numbers): `WEFAX_LINES_PER_MIN = 120`,
`WEFAX_IOC = 576`, `PIXELS_PER_LINE = 1809`, `SUBCARRIER_BLACK_HZ = 1500.0`,
`SUBCARRIER_WHITE_HZ = 2300.0`, `SUBCARRIER_CENTER_HZ = 1900.0`,
`START_TONE_HZ = 300.0`, `STOP_TONE_HZ = 450.0`, tone-detect and phasing window
lengths pinned during implementation.

Tests live in `crates/sdr-dsp/src/wefax/tests.rs` (sibling split, per the
DSP-decoder convention).

### 2. `crates/sdr-radio/src/wefax_image.rs` — shared image handle

Mirror `lrpt_image.rs` (the continuous-image analog):

```rust
pub struct WefaxImage { handle: WefaxImageHandle }         // owner
#[derive(Clone)] pub struct WefaxImageHandle { inner: Arc<Mutex<Inner>> }

impl WefaxImageHandle {
    pub fn write_line(&self, line_index: u32, width: u32, pixels: &[u8]);
    pub fn snapshot(&self) -> Option<WefaxSnapshot>;        // non-destructive, for viewer
    pub fn take_completed(&self) -> Option<CompletedWefaxImage>; // swap+reset on stop tone
    pub fn clear(&self);
}
```

Height grows as lines arrive (charts are variable-length); `lock_or_recover()`
poison handling copied from the SSTV/LRPT handles. `CompletedWefaxImage`
exposes `to_flat_gray() -> Vec<u8>` for PNG.

### 3. `crates/sdr-radio/src/demod/wefax.rs` + `DemodMode::Wefax`

A USB-based demod with a **locked** passband so the user can't detune the fax
band. Mirror `demod/usb.rs` config but `bandwidth_locked: true`,
`default_bandwidth ≈ 2_400` Hz, `af_sample_rate: 24_000`, `vfo_reference:
Lower`. Add the `Wefax` variant to `DemodMode` (`sdr-types`) and the
`demod/mod.rs` enumeration; add the mode-selector entry in the UI. No IQ-rate
pin needed (unlike LRPT) — 24 kHz AF is ample for a 2300 Hz-max subcarrier.

### 4. `crates/sdr-core/src/controller/wefax.rs` — the decode tap

`wefax_decode_tap(state, dsp_tx, audio_count)`: new gated block in
`controller.rs` for `current_mode() == DemodMode::Wefax`. Lazy-init
`WefaxDecoder::new(radio.audio_sample_rate())` (cache failures in
`wefax_init_failed_at_rate`), `downmix_pre_gate_mono` the AF, `process(...)`,
`handle.write_line(...)` per line, emit `DspToUi::WefaxLineDecoded(idx)`; on
`take_chart_complete()`, `take_completed()` + `DspToUi::WefaxImageComplete{…}`.
New `DspState` fields (`wefax_decoder`, `wefax_mono_buf`,
`wefax_init_failed_at_rate`, `wefax_image`) reset in `reset_imaging_decoders`.

### 5. `crates/sdr-core/src/messages.rs` — messages

- `UiToDsp::SetWefaxImage(WefaxImageHandle)` / `UiToDsp::ClearWefaxImage`.
- `DspToUi::WefaxLineDecoded(u32)`,
  `DspToUi::WefaxImageComplete { width: u32, height: u32, pixels: Vec<u8> }`,
  `DspToUi::WefaxState(WefaxState)` for a status pill (Idle/Phasing/Imaging).

### 6. `crates/sdr-ui/src/wefax_viewer.rs` — live viewer

Event-driven, mirroring `sstv_viewer.rs`: a persistent Cairo greyscale
(ARGB32, R=G=B) surface updated on `WefaxLineDecoded` via `snapshot()` +
`queue_draw` (buffered while paused). Header bar: **Pause/Resume** and **Export
PNG**. A small status label reflects `WefaxState`. Action `app.wefax-open`,
accel **`Ctrl+Shift+F`**; `open_wefax_viewer_if_needed(...)` creates the window,
sets the handle, and sends `SetWefaxImage`. Auto-suggested when the user selects
WEFAX mode (via `on_demod_mode_changed`).

### 7. Save path

- **Manual Export PNG** from the viewer (Cairo `write_to_png`).
- **Auto-save on chart complete:** on `WefaxImageComplete`, write
  `~/sdr-recordings/wefax-{local-timestamp}.png` (no satellite slug — WEFAX is
  not a pass). A single `wefax_output_path(now)` helper is the source of truth,
  alongside the existing `png_path_for` family in `satellites_recorder.rs`.
  No auto-record state machine tie-in.

### 8. Offline render harness (real-data gate)

`crates/sdr-dsp/examples/wefax_decode_wav.rs` (mirror `apt_decode_wav.rs`):
reads a WAV of USB AF, runs `WefaxDecoder`, writes a PNG. This is the
**rendered-image deliverable** and the real-data gate, run against a fetched
public NMG/DWD reference recording.

---

## Data flow (live)

1. User selects **WEFAX** mode → USB-based demod delivers 24 kHz AF; UI
   auto-opens the viewer, which sends `SetWefaxImage(handle)`.
2. Controller's NFM-style block additionally runs `wefax_decode_tap` while in
   WEFAX mode: AF → discriminator → sync SM → lines into the handle.
3. Viewer repaints on each `WefaxLineDecoded`; status pill tracks
   Phasing/Imaging.
4. Stop tone → `WefaxImageComplete` → viewer finalizes + auto-save PNG; decoder
   resets for the next chart.

---

## Error handling

- **Decoder init failure** (bad rate): cache in `wefax_init_failed_at_rate`,
  warn once, drop audio (no spam) — same discipline as APT/SSTV.
- **No lock / noisy band:** decoder stays in `Phasing`/`Idle` and emits nothing
  rather than garbage lines; the status pill tells the user.
- **Mutex poison:** `lock_or_recover()` warns and continues.
- **Library-crate rules:** no `unwrap`/`panic!`/`println!` in `sdr-dsp` /
  `sdr-radio` / `sdr-core`; `thiserror` (`DspError`) for the decoder;
  `tracing` for logs.

---

## Testing strategy

**TDD on the pure DSP** (`wefax/tests.rs`):
- Discriminator maps synthetic 1500 / 1900 / 2300 Hz tones → 0 / ~128 / 255.
- `samples_per_line` / pixel-window math for representative rates.
- Phasing correlation recovers a known injected pulse offset (column-0 lock).
- Start/stop tone detectors fire on synthetic 300 / 450 Hz bursts and reject
  in-band image tones.
- State-machine transitions (Idle→Phasing→Imaging→Stopped→Idle), including no
  line emission before lock.
- Slant estimation recovers a known injected skew.

**Real-data gate (the actual correctness bar):** `examples/wefax_decode_wav.rs`
renders a **coherent, legible weather chart** from a real reference WAV — this
human-judged "does it look like a chart" outcome is the correctness bar, since
no bit-exact oracle exists. py_wefax may be run on the same input as a loose
sanity check when a bug is being chased, but disagreement with it is not by
itself a failure (it is not known-correct either).

**Integration:** `crates/sdr-dsp/tests/wefax_integration.rs` (WAV fixture →
line count / geometry assertions), mirroring `apt_integration.rs`.

**UI:** viewer is user-smoke-tested (GTK), not launched by Claude, per the
standing workflow.

---

## Codacy quality gates (enforced — bake into every task)

- **Function/method ≤ 50 NLOC** (target local ≤ ~44; Codacy counts ~4–7
  stricter than `uvx lizard`). Carve helpers proactively.
- **File ≤ 500 NLOC** — split tests into sibling `tests.rs`; split the decoder
  into focused submodules (discriminator / sync / assembly) if it approaches
  the limit.
- **Function parameters ≤ 8** — bundle into structs (e.g. a decoder-config
  struct) rather than long signatures.
- **Diff coverage stays green:** unit-test all new pure logic (discriminator,
  pixel clock, phasing, tone detect, state machine, slant, path helper,
  image-handle round-trip).
- Each code task runs `uvx lizard <changed .rs> -T nloc=50` alongside the cargo
  gates, `cargo fmt --all -- --check` last before any push.
- Docs (this spec, the plan) pass markdownlint: fenced blocks carry a language;
  no duplicate sibling headings.

---

## Scope

**In scope (v1):** IOC 576 / 120 lpm / greyscale; USB reception via a dedicated
`DemodMode::Wefax`; discriminator; pixel clock; phasing/column-0 alignment;
start–stop chart segmentation; slant correction; live viewer + manual/auto PNG
save; offline WAV→PNG harness.

**Deferred (non-goals):** IOC 288 & 240 lpm; a WEFAX station catalog +
scheduled/auto-capture; binary/halftone rendering toggle; dynamic carrier
auto-detect as the primary (starts as a fallback behind hardcoded tones); the
#867 dedicated per-mode activity surface. `sdr-orbcomm` and unrelated crates are
untouched.

---

## References

- **py_wefax** (`mryndzionek/py_wefax`, `decode.py`): **informational
  troubleshooting reference only — not ported, not a correctness oracle.**
  Useful ideas we independently adopt: analytic-signal (`hilbert`) FM demod
  `angle(conj(prev)·analytic)·ref·sr`; line length `sr/2 + slant` (dynamic
  slant); phasing via correlation of a ~25 ms pulse kernel against a
  median-filtered (`medfilt 201`) line; dynamic two-carrier peak detection. Its
  output PNGs (real JMH / Pinneberg charts) are a loose *visual* comparison
  target, but they are themselves unverified.
- **APT decoder** (`crates/sdr-dsp/src/apt.rs`) — the architectural template for
  the pure DSP unit and its `process(&[f32], &mut [Line]) -> Result<usize>`
  shape.
- **LRPT image handle** (`crates/sdr-radio/src/lrpt_image.rs`) — the
  continuous shared-image template.
- **SSTV viewer** (`crates/sdr-ui/src/sstv_viewer.rs`) — the event-driven
  viewer template.
