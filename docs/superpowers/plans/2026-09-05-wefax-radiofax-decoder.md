# WEFAX / HF Radiofax Decoder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decode HF radiofax (WEFAX) weather-chart broadcasts end to end — a pure DSP decoder that turns USB-demodulated fax audio into a live greyscale chart, plus a dedicated demod mode, shared image handle, controller tap, live viewer, and PNG save.

**Architecture:** WEFAX is an APT-style continuous decoder (running sample counter, correlation sync) writing into an LRPT-style `Arc<Mutex>` shared image handle. Phase 1 builds the pure DSP unit in `sdr-dsp` plus an offline WAV→PNG harness so a chart renders early; Phase 2 wires it live (`DemodMode::Wefax` → controller tap → viewer → save).

**Tech Stack:** Rust, `sdr-dsp` (pure DSP), `sdr-radio` (image handle + demod), `sdr-core` (controller tap + messages), `sdr-ui` (GTK4/libadwaita viewer), `hound`/existing WAV reader for the example.

**Spec:** `docs/superpowers/specs/2026-09-05-wefax-radiofax-decoder-design.md` (read it alongside this plan — the plan argues from it).

## Global Constraints

- **Ground truth = WMO standard constants + a legible chart from real audio.** py_wefax is a troubleshooting reference ONLY — do not port it, do not treat it or its output as a correctness oracle.
- **Real-data fixtures (NOT committed — too large):** PRIMARY `/home/jherald/Downloads/WEFAX File 1.wav` (44.1 kHz stereo, cleanest, tones on 1500/2300); SECONDARY `/home/jherald/Downloads/48HrSurface_Valid201302011200.wav` (11025 Hz mono — rate-agnostic regression). The example takes a WAV path arg.
- **TDD:** every new function gets a failing test first, verified failing, then minimal code. The whole DSP is unit-tested against SYNTHETIC signals before the real WAV render.
- **Codacy gates (enforced):** function ≤ 50 NLOC (target local ≤ ~44), file ≤ 500 NLOC, function parameters ≤ 8, cover all new pure logic. Split proactively.
- **Library-crate rules:** no `unwrap()`/`panic!()`/`println!()` in `sdr-dsp`/`sdr-radio`/`sdr-core`; `thiserror` (`DspError`) for decoder errors; `tracing` for logs; named constants for all magic numbers; prefer `&str`/`&[T]` params.
- **`sdr-orbcomm` and unrelated crates untouched.**
- **Staging:** NEVER `git add -A`/`.` — the repo permanently carries untracked `build/` + `.vscode/` and modified `.codacy/*`. Stage explicit paths only; verify `git status --short` still shows that drift unstaged before every commit.
- **Commit author:** `Jason Herald <392+jasonherald@users.noreply.github.com>` (via `git -c user.name=... -c user.email=...`). Every commit message ends with:
  ```text
  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
  ```
- **CI parity:** wait for CodeRabbit to review the pushed HEAD before the next push. GTK viewer is user-smoke-tested, never launched by Claude.

### Standard gate block (run per code task, as SEPARATE bare Bash commands, before each commit)

Referenced below as "run the standard gate block against `<files>`". Run these unpiped, one per Bash call, in this order; `fmt` is always LAST:

- `G1`: `cargo test -p <crate> --lib <module>::` (plus `--features whisper-cpu` when the crate is `sdr-ui`)
- `G2`: `uvx lizard <changed .rs files> -T nloc=50` (0 warnings; carve helpers if any function/file exceeds)
- `G3`: `cargo clippy -p <crate> --all-targets -- -D warnings` (add `--features whisper-cpu` for `sdr-ui`)
- `G4`: `cargo test -p <crate>` (full crate, catches integration)
- `G5`: `cargo fmt --all -- --check`  ← LAST; any post-check edit re-runs it

---

## File Structure

**Phase 1 — pure DSP (`sdr-dsp`):**
- `crates/sdr-dsp/src/wefax.rs` — public API: `WefaxDecoder`, `WefaxLine`, `WefaxState`, constants, `process` orchestration. Declares submodules.
- `crates/sdr-dsp/src/wefax/discriminator.rs` — `Discriminator` (FM subcarrier → brightness).
- `crates/sdr-dsp/src/wefax/assembly.rs` — `LineAssembler` (pixel clock, per-column accumulation, column-0 offset, slant).
- `crates/sdr-dsp/src/wefax/tones.rs` — `ToneDetector` (Goertzel start/stop tone detect).
- `crates/sdr-dsp/src/wefax/phasing.rs` — `PhasingTracker` (pulse-column detect + slant estimate).
- `crates/sdr-dsp/src/wefax/sync.rs` — `SyncMachine` (Idle→Phasing→Imaging→Stopped).
- `crates/sdr-dsp/src/wefax/tests.rs` — unit tests (sibling split, DSP-decoder convention).
- `crates/sdr-dsp/examples/wefax_decode_wav.rs` — offline WAV→PNG harness (real-data render gate).
- `crates/sdr-dsp/tests/wefax_integration.rs` — geometry/line-count assertions on a synthetic WAV.
- Modify `crates/sdr-dsp/src/lib.rs` — `pub mod wefax;` + re-exports.

**Phase 2 — live integration:**
- `crates/sdr-radio/src/wefax_image.rs` (+ `wefax_image/tests.rs`) — `WefaxImage`/`WefaxImageHandle` shared handle.
- `crates/sdr-radio/src/demod/wefax.rs` — `WefaxDemodulator` (USB-based, locked passband).
- Modify `crates/sdr-types` — add `DemodMode::Wefax`.
- Modify `crates/sdr-radio/src/demod/mod.rs` — enumerate the new demod.
- Modify `crates/sdr-core/src/messages.rs` — `SetWefaxImage`/`ClearWefaxImage`, `WefaxLineDecoded`/`WefaxImageComplete`/`WefaxState`.
- `crates/sdr-core/src/controller/wefax.rs` — `wefax_decode_tap`; modify `controller.rs` (DspState fields, gated block, reset).
- `crates/sdr-ui/src/wefax_viewer.rs` (+ `wefax_viewer/tests.rs`) — live viewer; modify `state.rs`, window action wiring, `dsp_events`.
- Modify `crates/sdr-ui/src/sidebar/satellites_recorder.rs` — `wefax_output_path`; save wiring in `window.rs`.

---

## Phase 1 — Pure DSP + offline render

### Task 1: WefaxDecoder scaffold + constants + FM discriminator

**Files:**
- Create: `crates/sdr-dsp/src/wefax.rs`, `crates/sdr-dsp/src/wefax/discriminator.rs`, `crates/sdr-dsp/src/wefax/tests.rs`
- Modify: `crates/sdr-dsp/src/lib.rs` (add `pub mod wefax;`)

**Interfaces:**
- Produces: `WefaxDecoder`, `WefaxLine { pixels: [u8; PIXELS_PER_LINE], line_index: u32, state: WefaxState }`, `WefaxState { Idle, Phasing, Imaging, Stopped }`; pub consts `WEFAX_LINES_PER_MIN=120`, `WEFAX_IOC=576`, `PIXELS_PER_LINE=1809`, `SUBCARRIER_BLACK_HZ=1500.0`, `SUBCARRIER_WHITE_HZ=2300.0`, `SUBCARRIER_CENTER_HZ=1900.0`, `SUBCARRIER_DEV_HZ=400.0`, `START_TONE_HZ=300.0`, `STOP_TONE_HZ=450.0`; `pub(crate) struct Discriminator` with `new(sr: f64) -> Self` and `push(&mut self, sample: f64) -> u8`.

- [ ] **Step 1: Write the failing test** (`crates/sdr-dsp/src/wefax/tests.rs`)

```rust
use super::discriminator::Discriminator;
use super::*;

/// Feed a steady tone; the back half (LPF settled) maps to the expected
/// brightness: 1500 Hz→black(≈0), 1900→grey(≈128), 2300→white(≈255).
fn brightness_of_tone(freq_hz: f64, sr: f64) -> f64 {
    let mut d = Discriminator::new(sr);
    let n = sr as usize; // 1 second
    let mut acc = 0.0f64;
    let mut cnt = 0u32;
    for i in 0..n {
        let s = (2.0 * std::f64::consts::PI * freq_hz * i as f64 / sr).sin();
        let b = d.push(s) as f64;
        if i >= n / 2 { acc += b; cnt += 1; } // skip settling transient
    }
    acc / cnt as f64
}

#[test]
fn discriminator_maps_subcarrier_tones_to_brightness() {
    let sr = 44_100.0;
    assert!(brightness_of_tone(SUBCARRIER_BLACK_HZ, sr) < 20.0, "black");
    assert!((brightness_of_tone(SUBCARRIER_CENTER_HZ, sr) - 128.0).abs() < 20.0, "grey");
    assert!(brightness_of_tone(SUBCARRIER_WHITE_HZ, sr) > 235.0, "white");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p sdr-dsp --lib wefax::tests::discriminator_maps_subcarrier_tones_to_brightness`
Expected: FAIL to COMPILE (module/types not defined) — that is the red state.

- [ ] **Step 3: Write `crates/sdr-dsp/src/wefax.rs`** (public shell + constants + module decls)

```rust
//! WEFAX / HF radiofax decoder. Pure DSP: USB-demodulated fax audio →
//! greyscale scanlines. Analytic-signal FM discriminator + IOC-576 line
//! assembly at 120 lpm. No I/O, no threads. See the design spec.

mod discriminator;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

/// Lines per minute (WMO standard fax rate handled here).
pub const WEFAX_LINES_PER_MIN: u32 = 120;
/// Index of Cooperation.
pub const WEFAX_IOC: u32 = 576;
/// Usable pixels per line = round(π · IOC).
pub const PIXELS_PER_LINE: usize = 1809;
/// Black / white / center subcarrier tones and peak deviation (Hz).
pub const SUBCARRIER_BLACK_HZ: f64 = 1500.0;
pub const SUBCARRIER_WHITE_HZ: f64 = 2300.0;
pub const SUBCARRIER_CENTER_HZ: f64 = 1900.0;
pub const SUBCARRIER_DEV_HZ: f64 = 400.0;
/// Chart framing tones (Hz).
pub const START_TONE_HZ: f64 = 300.0;
pub const STOP_TONE_HZ: f64 = 450.0;

/// Where the decoder is in the start-tone / phasing / imaging / stop cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WefaxState {
    Idle,
    Phasing,
    Imaging,
    Stopped,
}

/// One assembled scanline.
#[derive(Debug, Clone)]
pub struct WefaxLine {
    pub pixels: [u8; PIXELS_PER_LINE],
    pub line_index: u32,
    pub state: WefaxState,
}

impl Default for WefaxLine {
    fn default() -> Self {
        Self { pixels: [0; PIXELS_PER_LINE], line_index: 0, state: WefaxState::Imaging }
    }
}
```

- [ ] **Step 4: Write `crates/sdr-dsp/src/wefax/discriminator.rs`**

```rust
//! Analytic-signal FM discriminator: subcarrier audio → 0..=255 brightness.
//! Mix down by the 1900 Hz center, one-pole low-pass, then per-sample
//! instantaneous-frequency (phase difference) = deviation from center.

use std::f64::consts::PI;

use super::{SUBCARRIER_CENTER_HZ, SUBCARRIER_DEV_HZ};

/// One-pole low-pass cutoff (Hz) on the complex baseband — passes the
/// per-line detail (< ~1.8 kHz) while rejecting the sum image at 2·center.
const LPF_CUTOFF_HZ: f64 = 1600.0;

pub(crate) struct Discriminator {
    sr: f64,
    phase: f64,
    phase_inc: f64,
    lpf_i: f64,
    lpf_q: f64,
    lpf_alpha: f64,
    prev_i: f64,
    prev_q: f64,
    have_prev: bool,
}

impl Discriminator {
    pub(crate) fn new(sr: f64) -> Self {
        // one-pole smoothing factor for the chosen cutoff
        let alpha = 1.0 - (-2.0 * PI * LPF_CUTOFF_HZ / sr).exp();
        Self {
            sr,
            phase: 0.0,
            phase_inc: 2.0 * PI * SUBCARRIER_CENTER_HZ / sr,
            lpf_i: 0.0,
            lpf_q: 0.0,
            lpf_alpha: alpha,
            prev_i: 0.0,
            prev_q: 0.0,
            have_prev: false,
        }
    }

    /// Brightness for one real audio sample (0=black … 255=white).
    pub(crate) fn push(&mut self, s: f64) -> u8 {
        let (sn, cs) = self.phase.sin_cos();
        let bi = s * cs; // Re{ s·e^{-jθ} }
        let bq = -s * sn; // Im{ s·e^{-jθ} }
        self.phase += self.phase_inc;
        if self.phase >= 2.0 * PI {
            self.phase -= 2.0 * PI;
        }
        self.lpf_i += self.lpf_alpha * (bi - self.lpf_i);
        self.lpf_q += self.lpf_alpha * (bq - self.lpf_q);
        let (i, q) = (self.lpf_i, self.lpf_q);
        let dev_hz = if self.have_prev {
            // angle( cur · conj(prev) ) · sr / 2π  = deviation from center
            let re = i * self.prev_i + q * self.prev_q;
            let im = q * self.prev_i - i * self.prev_q;
            im.atan2(re) * self.sr / (2.0 * PI)
        } else {
            0.0
        };
        self.prev_i = i;
        self.prev_q = q;
        self.have_prev = true;
        let norm = (dev_hz + SUBCARRIER_DEV_HZ) / (2.0 * SUBCARRIER_DEV_HZ);
        (norm.clamp(0.0, 1.0) * 255.0).round() as u8
    }
}
```

- [ ] **Step 5: Add `pub mod wefax;` to `crates/sdr-dsp/src/lib.rs`** (next to `pub mod apt;`).

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p sdr-dsp --lib wefax::tests::discriminator_maps_subcarrier_tones_to_brightness`
Expected: PASS.

- [ ] **Step 7: Gates + commit.** Run the standard gate block against `crates/sdr-dsp/src/wefax.rs crates/sdr-dsp/src/wefax/discriminator.rs crates/sdr-dsp/src/wefax/tests.rs crates/sdr-dsp/src/lib.rs` (G1 module `wefax`, crate `sdr-dsp`). Then:

```bash
git add crates/sdr-dsp/src/wefax.rs crates/sdr-dsp/src/wefax/discriminator.rs crates/sdr-dsp/src/wefax/tests.rs crates/sdr-dsp/src/lib.rs
git -c user.name="Jason Herald" -c user.email="392+jasonherald@users.noreply.github.com" commit -m "feat(dsp): WEFAX FM subcarrier discriminator (#877)

... (Co-Authored-By + Claude-Session trailers per Global Constraints)"
```

---

### Task 2: Line assembler (pixel clock, free-running) + WefaxDecoder::process

**Files:**
- Create: `crates/sdr-dsp/src/wefax/assembly.rs`
- Modify: `crates/sdr-dsp/src/wefax.rs` (add `WefaxDecoder`, `process`), `crates/sdr-dsp/src/wefax/tests.rs`

**Interfaces:**
- Consumes: `Discriminator`, `WefaxLine`, `PIXELS_PER_LINE`.
- Produces: `WefaxDecoder::new(input_rate_hz: u32) -> Result<Self, DspError>`; `WefaxDecoder::process(&mut self, input: &[f32], out: &mut [WefaxLine]) -> Result<usize, DspError>`; `pub(crate) struct LineAssembler` with `new(samples_per_line: f64)`, `push(&mut self, brightness: u8) -> Option<[u8; PIXELS_PER_LINE]>`, `set_column_offset(&mut self, cols: i32)`, `set_samples_per_line(&mut self, spl: f64)`. Free-running for now (no sync); emits a line every `samples_per_line` input samples.

- [ ] **Step 1: Write the failing test** (`wefax/tests.rs`)

```rust
use super::assembly::LineAssembler;

/// A ramp of brightness fills exactly one line's worth of samples and
/// emits one line spanning the full 0..255 range across the row.
#[test]
fn line_assembler_emits_one_line_per_period() {
    let spl = 12_000.0; // e.g. 24 kHz / 2 lines-per-sec
    let mut a = LineAssembler::new(spl);
    let mut lines = 0;
    for i in 0..(spl as usize) {
        let b = ((i as f64 / spl) * 255.0) as u8;
        if a.push(b).is_some() { lines += 1; }
    }
    assert_eq!(lines, 1, "exactly one line per samples_per_line window");
}

/// process() over a synthetic white tone yields a bright line.
#[test]
fn process_emits_bright_lines_for_white_tone() {
    let sr = 24_000u32;
    let mut dec = WefaxDecoder::new(sr).unwrap();
    let n = sr as usize; // 1 s → 2 lines
    let audio: Vec<f32> = (0..n)
        .map(|i| (2.0 * std::f64::consts::PI * SUBCARRIER_WHITE_HZ * i as f64 / sr as f64).sin() as f32)
        .collect();
    let mut out = vec![WefaxLine::default(); 8];
    let mut total = 0usize;
    for chunk in audio.chunks(4096) {
        total += dec.process(chunk, &mut out).unwrap();
    }
    assert!(total >= 1, "at least one line from 1 s of audio");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p sdr-dsp --lib wefax::tests::line_assembler_emits_one_line_per_period`
Expected: FAIL to compile (`LineAssembler`, `WefaxDecoder` undefined).

- [ ] **Step 3: Write `crates/sdr-dsp/src/wefax/assembly.rs`**

```rust
//! Pixel clock + scanline accumulation. Maps each brightness sample to a
//! column via a fractional sample-per-line phase, averaging the samples
//! that land in each column, and emits a full line every period.

use super::PIXELS_PER_LINE;

pub(crate) struct LineAssembler {
    samples_per_line: f64,
    pos: f64,               // fractional sample position within the current line
    column_offset: i32,     // phasing left-edge shift, in columns
    bins: [f64; PIXELS_PER_LINE],
    counts: [u32; PIXELS_PER_LINE],
}

impl LineAssembler {
    pub(crate) fn new(samples_per_line: f64) -> Self {
        Self {
            samples_per_line,
            pos: 0.0,
            column_offset: 0,
            bins: [0.0; PIXELS_PER_LINE],
            counts: [0; PIXELS_PER_LINE],
        }
    }

    pub(crate) fn set_column_offset(&mut self, cols: i32) {
        self.column_offset = cols;
    }

    pub(crate) fn set_samples_per_line(&mut self, spl: f64) {
        if spl > 1.0 {
            self.samples_per_line = spl;
        }
    }

    /// Accumulate one brightness sample; return a finished line when the
    /// per-line sample budget is reached.
    pub(crate) fn push(&mut self, brightness: u8) -> Option<[u8; PIXELS_PER_LINE]> {
        let frac = self.pos / self.samples_per_line; // 0..1 across the line
        let raw_col = (frac * PIXELS_PER_LINE as f64) as i32 + self.column_offset;
        let col = raw_col.rem_euclid(PIXELS_PER_LINE as i32) as usize;
        self.bins[col] += brightness as f64;
        self.counts[col] += 1;
        self.pos += 1.0;
        if self.pos >= self.samples_per_line {
            self.pos -= self.samples_per_line;
            Some(self.finish_line())
        } else {
            None
        }
    }

    fn finish_line(&mut self) -> [u8; PIXELS_PER_LINE] {
        let mut line = [0u8; PIXELS_PER_LINE];
        for c in 0..PIXELS_PER_LINE {
            if self.counts[c] > 0 {
                line[c] = (self.bins[c] / self.counts[c] as f64).round() as u8;
            }
            self.bins[c] = 0.0;
            self.counts[c] = 0;
        }
        line
    }
}
```

- [ ] **Step 4: Add `WefaxDecoder` + `process` to `crates/sdr-dsp/src/wefax.rs`**

```rust
mod assembly;

use assembly::LineAssembler;
use discriminator::Discriminator;
use crate::error::DspError; // follow apt.rs's DspError import path

/// Minimum usable input rate (Hz) — Nyquist must clear the 2300 Hz white tone.
const MIN_INPUT_RATE_HZ: u32 = 8_000;

pub struct WefaxDecoder {
    discriminator: Discriminator,
    assembler: LineAssembler,
    line_index: u32,
    state: WefaxState,
}

impl WefaxDecoder {
    /// # Errors
    /// Returns `DspError` when `input_rate_hz` is below [`MIN_INPUT_RATE_HZ`].
    pub fn new(input_rate_hz: u32) -> Result<Self, DspError> {
        if input_rate_hz < MIN_INPUT_RATE_HZ {
            return Err(DspError::InvalidConfig(format!(
                "WEFAX input rate {input_rate_hz} Hz below minimum {MIN_INPUT_RATE_HZ} Hz"
            )));
        }
        let sr = f64::from(input_rate_hz);
        Ok(Self {
            discriminator: Discriminator::new(sr),
            assembler: LineAssembler::new(sr / 2.0), // 120 lpm = 2 lines/s
            line_index: 0,
            state: WefaxState::Imaging, // free-running for Task 2; sync added in Task 6
        })
    }

    pub fn state(&self) -> WefaxState {
        self.state
    }

    /// Feed audio; append completed lines to `out`, returning the count written.
    ///
    /// # Errors
    /// Never fails today; returns `Result` for forward compatibility and to
    /// match the `AptDecoder` contract.
    pub fn process(&mut self, input: &[f32], out: &mut [WefaxLine]) -> Result<usize, DspError> {
        let mut written = 0usize;
        for &s in input {
            let b = self.discriminator.push(f64::from(s));
            if let Some(pixels) = self.assembler.push(b) {
                if written < out.len() {
                    out[written] = WefaxLine { pixels, line_index: self.line_index, state: self.state };
                    written += 1;
                }
                self.line_index = self.line_index.wrapping_add(1);
            }
        }
        Ok(written)
    }
}
```

> **Note on `DspError`:** check `crates/sdr-dsp/src/apt.rs` for the exact error type/variant used (e.g. `DspError::InvalidConfig` or the apt equivalent) and mirror it; if no string-carrying variant exists, add one following the existing `thiserror` pattern.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p sdr-dsp --lib wefax::`
Expected: PASS (both new tests + Task 1's).

- [ ] **Step 6: Gates + commit.** Standard gate block against `crates/sdr-dsp/src/wefax.rs crates/sdr-dsp/src/wefax/assembly.rs crates/sdr-dsp/src/wefax/tests.rs`. Commit (staged paths explicit) with message `feat(dsp): WEFAX pixel clock + free-running line assembly (#877)`.

---

### Task 3: Offline WAV→PNG harness — FIRST RENDER milestone

**Files:**
- Create: `crates/sdr-dsp/examples/wefax_decode_wav.rs`

**Interfaces:**
- Consumes: `WefaxDecoder::new`, `process`, `WefaxLine`, `PIXELS_PER_LINE`.
- Produces: a CLI `wefax_decode_wav <input.wav> <output.png>` that renders a greyscale chart. Mirrors `crates/sdr-dsp/examples/apt_decode_wav.rs` for WAV reading + PNG writing (reuse its WAV/`image`/`png` crate usage).

- [ ] **Step 1: Read the template.** Open `crates/sdr-dsp/examples/apt_decode_wav.rs` and note how it (a) reads WAV samples + rate, (b) constructs the decoder, (c) drains lines in a loop, (d) writes a greyscale PNG. Reuse the same crates/approach.

- [ ] **Step 2: Write `crates/sdr-dsp/examples/wefax_decode_wav.rs`**

```rust
//! Offline WEFAX render harness (real-data gate): decode a WAV of USB fax
//! audio into a greyscale PNG. Stereo is downmixed to mono. Usage:
//!   cargo run -p sdr-dsp --example wefax_decode_wav -- <in.wav> <out.png>

use sdr_dsp::wefax::{WefaxDecoder, WefaxLine, PIXELS_PER_LINE};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args.next().ok_or("usage: wefax_decode_wav <in.wav> <out.png>")?;
    let output = args.next().ok_or("usage: wefax_decode_wav <in.wav> <out.png>")?;

    // Read WAV (reuse the crate used by apt_decode_wav.rs, e.g. hound).
    let mut reader = hound::WavReader::open(&input)?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let rate = spec.sample_rate;
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
    };
    // Downmix to mono.
    let mono: Vec<f32> = if channels > 1 {
        raw.chunks(channels).map(|f| f.iter().copied().sum::<f32>() / channels as f32).collect()
    } else {
        raw
    };

    let mut dec = WefaxDecoder::new(rate)?;
    let mut out = vec![WefaxLine::default(); 64];
    let mut rows: Vec<[u8; PIXELS_PER_LINE]> = Vec::new();
    for chunk in mono.chunks(8192) {
        let n = dec.process(chunk, &mut out)?;
        for line in out.iter().take(n) {
            rows.push(line.pixels);
        }
    }

    let height = rows.len() as u32;
    let width = PIXELS_PER_LINE as u32;
    let mut img = vec![0u8; rows.len() * PIXELS_PER_LINE];
    for (r, row) in rows.iter().enumerate() {
        img[r * PIXELS_PER_LINE..(r + 1) * PIXELS_PER_LINE].copy_from_slice(row);
    }
    image::save_buffer(&output, &img, width, height, image::ColorType::L8)?;
    eprintln!("wrote {output} ({width}x{height})");
    Ok(())
}
```

> If `apt_decode_wav.rs` uses a different WAV/image crate, match it and add any missing `dev-dependencies` to `crates/sdr-dsp/Cargo.toml` (they are dev-only; check what apt already pulls in first to avoid new deps).

- [ ] **Step 3: Render the PRIMARY fixture**

Run: `cargo run -p sdr-dsp --example wefax_decode_wav -- "/home/jherald/Downloads/WEFAX File 1.wav" /tmp/claude-1000/wefax_primary.png`
Expected: writes a PNG. **Milestone check:** open the PNG — even free-running (no sync yet) it should show recognizable chart structure (map outlines / grid / text), likely sheared or horizontally offset. If it is pure noise, STOP and debug the discriminator/rate before proceeding (systematic-debugging).

- [ ] **Step 4: Render the SECONDARY fixture** (rate-agnostic check)

Run: `cargo run -p sdr-dsp --example wefax_decode_wav -- "/home/jherald/Downloads/48HrSurface_Valid201302011200.wav" /tmp/claude-1000/wefax_secondary.png`
Expected: a second PNG with visible chart structure at 11025 Hz.

- [ ] **Step 5: Gates + commit.** `cargo fmt --all -- --check` (examples are formatted too); `cargo clippy -p sdr-dsp --all-targets -- -D warnings`. Commit `crates/sdr-dsp/examples/wefax_decode_wav.rs` (+ `Cargo.toml` if a dev-dep was added) with `feat(dsp): offline WEFAX WAV->PNG render harness (#877)`. The output PNGs are NOT committed.

---

### Task 4: Tone detectors (start 300 Hz / stop 450 Hz)

**Files:**
- Create: `crates/sdr-dsp/src/wefax/tones.rs`
- Modify: `crates/sdr-dsp/src/wefax.rs` (declare `mod tones;`), `wefax/tests.rs`

**Interfaces:**
- Produces: `pub(crate) struct ToneDetector` with `new(sr: f64, target_hz: f64) -> Self` and `push(&mut self, sample: f64) -> bool` (returns `true` while the target tone is sustained-present over the detection window). Goertzel magnitude vs total-energy ratio over a sliding block; a tone is "present" when the ratio exceeds `TONE_PRESENT_RATIO` for `TONE_MIN_BLOCKS` consecutive blocks.

- [ ] **Step 1: Write the failing test** (`wefax/tests.rs`)

```rust
use super::tones::ToneDetector;

fn tone(freq: f64, sr: f64, secs: f64) -> Vec<f64> {
    let n = (sr * secs) as usize;
    (0..n).map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / sr).sin()).collect()
}

#[test]
fn tone_detector_fires_on_target_and_rejects_image_tones() {
    let sr = 44_100.0;
    let mut det = ToneDetector::new(sr, START_TONE_HZ);
    let mut fired = false;
    for s in tone(START_TONE_HZ, sr, 5.0) { fired |= det.push(s); }
    assert!(fired, "300 Hz start tone should fire");

    let mut det2 = ToneDetector::new(sr, START_TONE_HZ);
    let mut fired2 = false;
    for s in tone(SUBCARRIER_WHITE_HZ, sr, 5.0) { fired2 |= det2.push(s); }
    assert!(!fired2, "a 2300 Hz image tone must NOT trigger the 300 Hz detector");
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test -p sdr-dsp --lib wefax::tests::tone_detector_fires_on_target_and_rejects_image_tones` → FAIL to compile.

- [ ] **Step 3: Write `crates/sdr-dsp/src/wefax/tones.rs`**

```rust
//! Goertzel single-frequency detector for the 300 Hz start / 450 Hz stop
//! tones. Reports "present" when the target bin dominates the block energy
//! for several consecutive blocks (rejects the 1500–2300 Hz image band).

/// Samples per Goertzel block (~23 ms at 44.1 kHz).
const BLOCK_LEN: usize = 1024;
/// Target-bin power / total block power to count a block as "on-tone".
const TONE_PRESENT_RATIO: f64 = 0.30;
/// Consecutive on-tone blocks required to declare the tone present.
const TONE_MIN_BLOCKS: u32 = 8;

pub(crate) struct ToneDetector {
    coeff: f64,
    q0: f64,
    q1: f64,
    q2: f64,
    energy: f64,
    n: usize,
    on_blocks: u32,
}

impl ToneDetector {
    pub(crate) fn new(sr: f64, target_hz: f64) -> Self {
        let k = (target_hz / sr * BLOCK_LEN as f64).round();
        let w = 2.0 * std::f64::consts::PI * k / BLOCK_LEN as f64;
        Self { coeff: 2.0 * w.cos(), q0: 0.0, q1: 0.0, q2: 0.0, energy: 0.0, n: 0, on_blocks: 0 }
    }

    /// Feed one sample; returns true once the tone is confirmed present.
    pub(crate) fn push(&mut self, s: f64) -> bool {
        self.q0 = self.coeff * self.q1 - self.q2 + s;
        self.q2 = self.q1;
        self.q1 = self.q0;
        self.energy += s * s;
        self.n += 1;
        if self.n < BLOCK_LEN {
            return false;
        }
        let power = self.q1 * self.q1 + self.q2 * self.q2 - self.coeff * self.q1 * self.q2;
        let ratio = if self.energy > 1e-9 { power / (self.energy * BLOCK_LEN as f64 / 2.0) } else { 0.0 };
        self.q0 = 0.0;
        self.q1 = 0.0;
        self.q2 = 0.0;
        self.energy = 0.0;
        self.n = 0;
        if ratio >= TONE_PRESENT_RATIO {
            self.on_blocks += 1;
        } else {
            self.on_blocks = 0;
        }
        self.on_blocks >= TONE_MIN_BLOCKS
    }
}
```

> The `ratio` normalisation is approximate; during Task 8 (real WAV) confirm `TONE_PRESENT_RATIO`/`TONE_MIN_BLOCKS` actually fire on the fixtures' preamble and tune if needed. Keep the synthetic test as the contract.

- [ ] **Step 4: Add `mod tones;` to `wefax.rs`; run tests** → `cargo test -p sdr-dsp --lib wefax::` → PASS.

- [ ] **Step 5: Gates + commit.** Standard gate block; commit `feat(dsp): WEFAX start/stop tone detectors (#877)`.

---

### Task 5: Phasing tracker (pulse column + slant estimate)

**Files:**
- Create: `crates/sdr-dsp/src/wefax/phasing.rs`
- Modify: `crates/sdr-dsp/src/wefax.rs`, `wefax/tests.rs`

**Interfaces:**
- Produces: `pub(crate) struct PhasingTracker` with `new() -> Self`, `observe_line(&mut self, line: &[u8; PIXELS_PER_LINE])`, `column_offset(&self) -> Option<i32>` (stable pulse column once enough phasing lines seen), `slant_columns_per_line(&self) -> f64` (linear-fit drift). The phasing pulse is the brightest short run in a mostly-dark line; its column sets the left-edge offset, its drift across lines gives the slant.

- [ ] **Step 1: Write the failing test** (`wefax/tests.rs`)

```rust
use super::phasing::PhasingTracker;

fn phasing_line(pulse_col: usize) -> [u8; PIXELS_PER_LINE] {
    let mut l = [10u8; PIXELS_PER_LINE]; // near-black
    for c in pulse_col..(pulse_col + 90).min(PIXELS_PER_LINE) { l[c] = 250; } // ~white pulse
    l
}

#[test]
fn phasing_tracker_recovers_pulse_column() {
    let mut t = PhasingTracker::new();
    for _ in 0..12 { t.observe_line(&phasing_line(300)); }
    let off = t.column_offset().expect("offset locked after enough lines");
    assert!((off - 300).abs() <= 5, "recovered pulse column ~300, got {off}");
}

#[test]
fn phasing_tracker_estimates_slant() {
    let mut t = PhasingTracker::new();
    for k in 0..20 { t.observe_line(&phasing_line(300 + k)); } // +1 col/line drift
    assert!((t.slant_columns_per_line() - 1.0).abs() < 0.3, "≈1 col/line slant");
}
```

- [ ] **Step 2: Run test to verify it fails** → FAIL to compile.

- [ ] **Step 3: Write `crates/sdr-dsp/src/wefax/phasing.rs`**

```rust
//! Phasing-signal tracker: find the recurring white pulse in mostly-dark
//! phasing lines. Its column is the left-edge (column-0) offset; its drift
//! across lines is the slant (TX/RX clock mismatch).

use super::PIXELS_PER_LINE;

/// Minimum phasing lines before the column offset is considered stable.
const MIN_LINES_FOR_LOCK: usize = 6;

pub(crate) struct PhasingTracker {
    pulse_cols: Vec<f64>, // one detected pulse column per observed line
}

impl PhasingTracker {
    pub(crate) fn new() -> Self {
        Self { pulse_cols: Vec::new() }
    }

    pub(crate) fn observe_line(&mut self, line: &[u8; PIXELS_PER_LINE]) {
        self.pulse_cols.push(pulse_column(line));
    }

    /// Median pulse column once enough phasing lines are seen.
    pub(crate) fn column_offset(&self) -> Option<i32> {
        if self.pulse_cols.len() < MIN_LINES_FOR_LOCK {
            return None;
        }
        let mut v = self.pulse_cols.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(v[v.len() / 2].round() as i32)
    }

    /// Least-squares slope of pulse column vs line index (columns/line).
    pub(crate) fn slant_columns_per_line(&self) -> f64 {
        let n = self.pulse_cols.len();
        if n < 2 {
            return 0.0;
        }
        let nf = n as f64;
        let mean_x = (n as f64 - 1.0) / 2.0;
        let mean_y = self.pulse_cols.iter().sum::<f64>() / nf;
        let (mut num, mut den) = (0.0, 0.0);
        for (i, &y) in self.pulse_cols.iter().enumerate() {
            let dx = i as f64 - mean_x;
            num += dx * (y - mean_y);
            den += dx * dx;
        }
        if den.abs() < 1e-9 { 0.0 } else { num / den }
    }
}

/// Centre column of the brightest short run in a line.
fn pulse_column(line: &[u8; PIXELS_PER_LINE]) -> f64 {
    // Weighted centroid of the top-brightness samples is robust to a
    // fixed-width pulse plus noise.
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for (c, &v) in line.iter().enumerate() {
        if v > 180 {
            num += c as f64 * v as f64;
            den += v as f64;
        }
    }
    if den > 0.0 { num / den } else { 0.0 }
}
```

- [ ] **Step 4: Add `mod phasing;`; run tests** → `cargo test -p sdr-dsp --lib wefax::` → PASS.

- [ ] **Step 5: Gates + commit.** Standard gate block; commit `feat(dsp): WEFAX phasing pulse + slant estimation (#877)`.

---

### Task 6: Sync state machine — integrate tones + phasing + slant into process

**Files:**
- Create: `crates/sdr-dsp/src/wefax/sync.rs`
- Modify: `crates/sdr-dsp/src/wefax.rs` (wire the machine into `process`, add `take_chart_complete`), `wefax/tests.rs`

**Interfaces:**
- Consumes: `ToneDetector`, `PhasingTracker`, `LineAssembler`, `WefaxState`.
- Produces: `pub(crate) struct SyncMachine` owning start/stop `ToneDetector`s + `PhasingTracker`, with `new(sr: f64) -> Self`, `on_sample(&mut self, s: f64)`, `on_line(&mut self, line: &[u8; PIXELS_PER_LINE], assembler: &mut LineAssembler) -> LineDisposition`, `state(&self) -> WefaxState`; `enum LineDisposition { Drop, Emit }`. `WefaxDecoder::process` gains: emit lines only when `Emit`, expose `take_chart_complete(&mut self) -> bool` (set when a stop tone finalizes a chart). During `Phasing` the machine feeds `PhasingTracker` and applies `set_column_offset`/`set_samples_per_line` on lock, then transitions to `Imaging`.

- [ ] **Step 1: Write the failing test** (`wefax/tests.rs`) — an end-to-end synthetic chart: start tone → phasing lines → image → stop tone.

```rust
/// Synthesize AF for a mini chart and assert: no lines emitted before lock,
/// lines emitted during imaging, and take_chart_complete() fires after the
/// stop tone.
#[test]
fn sync_gates_emission_and_flags_chart_complete() {
    let sr = 24_000u32;
    let mut dec = WefaxDecoder::new(sr).unwrap();
    let mut out = vec![WefaxLine::default(); 64];

    let push_secs = |dec: &mut WefaxDecoder, gen: &dyn Fn(usize) -> f32, secs: f64, out: &mut [WefaxLine]| -> usize {
        let n = (sr as f64 * secs) as usize;
        let audio: Vec<f32> = (0..n).map(|i| gen(i)).collect();
        let mut t = 0;
        for c in audio.chunks(4096) { t += dec.process(c, out).unwrap(); }
        t
    };
    let tone = |f: f64| move |i: usize| (2.0 * std::f64::consts::PI * f * i as f64 / sr as f64).sin() as f32;

    let before = push_secs(&mut dec, &tone(START_TONE_HZ), 5.0, &mut out);
    assert_eq!(before, 0, "no image lines during start tone");
    // A pure white tone has no phasing pulse, so the machine stays in
    // Phasing and emits nothing — proving emission is gated on a lock.
    let during = push_secs(&mut dec, &tone(SUBCARRIER_WHITE_HZ), 5.0, &mut out);
    assert_eq!(during, 0, "no lines emitted before a phasing lock");
    // Stop tone finalizes the chart and returns to Idle.
    let _stop = push_secs(&mut dec, &tone(STOP_TONE_HZ), 5.0, &mut out);
    assert!(dec.take_chart_complete(), "stop tone flags chart complete");
    assert_eq!(dec.state(), WefaxState::Idle, "resets to Idle after a chart");
}
```

- [ ] **Step 2: Run test to verify it fails** → FAIL (methods/behaviour absent; free-running `process` emits during the start tone).

- [ ] **Step 3: Write `crates/sdr-dsp/src/wefax/sync.rs`** and wire it into `process`.

```rust
//! WEFAX line-sync state machine: start tone → phasing lock → imaging →
//! stop tone. Gates line emission until the left edge is locked and
//! auto-segments charts on the stop tone.

use super::assembly::LineAssembler;
use super::phasing::PhasingTracker;
use super::tones::ToneDetector;
use super::{START_TONE_HZ, STOP_TONE_HZ, WefaxState};

pub(crate) enum LineDisposition {
    Drop,
    Emit,
}

pub(crate) struct SyncMachine {
    sr: f64,
    start: ToneDetector,
    stop: ToneDetector,
    phasing: PhasingTracker,
    state: WefaxState,
    chart_complete: bool,
}

impl SyncMachine {
    pub(crate) fn new(sr: f64) -> Self {
        Self {
            sr,
            start: ToneDetector::new(sr, START_TONE_HZ),
            stop: ToneDetector::new(sr, STOP_TONE_HZ),
            phasing: PhasingTracker::new(),
            state: WefaxState::Idle,
            chart_complete: false,
        }
    }

    pub(crate) fn state(&self) -> WefaxState {
        self.state
    }

    pub(crate) fn take_chart_complete(&mut self) -> bool {
        std::mem::take(&mut self.chart_complete)
    }

    /// Per-sample tone tracking drives Idle→Phasing and *→Stopped.
    pub(crate) fn on_sample(&mut self, s: f64) {
        let start_on = self.start.push(s);
        let stop_on = self.stop.push(s);
        match self.state {
            WefaxState::Idle if start_on => {
                self.state = WefaxState::Phasing;
                self.phasing = PhasingTracker::new();
            }
            WefaxState::Phasing | WefaxState::Imaging if stop_on => {
                self.state = WefaxState::Stopped;
                self.chart_complete = true;
            }
            _ => {}
        }
    }

    /// Per-completed-line handling: build the phasing lock, then emit.
    pub(crate) fn on_line(
        &mut self,
        line: &[u8; super::PIXELS_PER_LINE],
        assembler: &mut LineAssembler,
    ) -> LineDisposition {
        match self.state {
            WefaxState::Phasing => {
                self.phasing.observe_line(line);
                if let Some(off) = self.phasing.column_offset() {
                    assembler.set_column_offset(off);
                    let spl = self.sr / 2.0 - self.phasing.slant_columns_per_line()
                        * (self.sr / 2.0 / super::PIXELS_PER_LINE as f64);
                    assembler.set_samples_per_line(spl);
                    self.state = WefaxState::Imaging;
                }
                LineDisposition::Drop
            }
            WefaxState::Imaging => LineDisposition::Emit,
            WefaxState::Stopped => {
                self.state = WefaxState::Idle;
                LineDisposition::Drop
            }
            WefaxState::Idle => LineDisposition::Drop,
        }
    }
}
```

Wire into `WefaxDecoder`: replace the free-running body with the machine — call `sync.on_sample(sf)` each sample; on a finished line call `sync.on_line(&pixels, &mut assembler)` and only push to `out` when `Emit`, stamping `line.state = sync.state()`; add `pub fn take_chart_complete(&mut self) -> bool { self.sync.take_chart_complete() }`. Reset `line_index` to 0 when a chart completes.

> **Robustness for the offline example:** add a `WefaxDecoder::new_free_running(rate)` constructor (state starts `Imaging`, no sync gating) so the example always renders even if a fixture lacks a clean preamble. The default `new` uses sync. Task 8 chooses per fixture.

- [ ] **Step 4: Add `mod sync;`; run tests** → `cargo test -p sdr-dsp --lib wefax::` → PASS. If the state-machine test needs finer phasing audio, refine the helper (keep the assertions).

- [ ] **Step 5: Gates + commit.** Standard gate block against all changed `wefax/*.rs`. If `wefax.rs` approaches 500 NLOC, move helpers into submodules. Commit `feat(dsp): WEFAX sync state machine — phasing lock + chart segmentation (#877)`.

---

### Task 7: Integration test + real-WAV legibility pass

**Files:**
- Create: `crates/sdr-dsp/tests/wefax_integration.rs`

**Interfaces:**
- Consumes: `WefaxDecoder`, `WefaxLine`, `PIXELS_PER_LINE`.

- [ ] **Step 1: Write the integration test** (synthetic WAV-in-memory → geometry assertions; no external files so CI is hermetic).

```rust
use sdr_dsp::wefax::{WefaxDecoder, WefaxLine, PIXELS_PER_LINE};

/// Free-running decode of N seconds of white tone yields ≈ 2·N lines of
/// full width — locks the pixel-clock geometry against regressions.
#[test]
fn free_running_line_geometry() {
    let sr = 24_000u32;
    let mut dec = WefaxDecoder::new_free_running(sr);
    let secs = 10;
    let n = sr as usize * secs;
    let audio: Vec<f32> = (0..n)
        .map(|i| (2.0 * std::f64::consts::PI * 2300.0 * i as f64 / sr as f64).sin() as f32)
        .collect();
    let mut out = vec![WefaxLine::default(); 64];
    let mut lines = 0usize;
    for c in audio.chunks(8192) { lines += dec.process(c, &mut out).unwrap(); }
    let expected = 2 * secs; // 120 lpm
    assert!((lines as i64 - expected as i64).abs() <= 1, "≈{expected} lines, got {lines}");
    assert_eq!(out[0].pixels.len(), PIXELS_PER_LINE);
}
```

- [ ] **Step 2: Run** `cargo test -p sdr-dsp --test wefax_integration` → PASS.

- [ ] **Step 3: Real-WAV legibility pass (the correctness bar).** Re-render both fixtures with the sync path AND the free-running path:

```bash
cargo run -p sdr-dsp --example wefax_decode_wav -- "/home/jherald/Downloads/WEFAX File 1.wav" /tmp/claude-1000/wefax_primary_sync.png
```

Inspect the PNG: with phasing lock the chart should be **left-aligned and un-sheared** (straight coastlines/grid, readable text). Compare visually against the py_wefax sample images (loose sanity only). If sheared → revisit slant sign/scale; if offset → revisit column-offset convention; if segmented wrong → revisit tone thresholds. Iterate constants (documented in `discriminator.rs`/`tones.rs`) until legible. This is human-judged, per Global Constraints.

- [ ] **Step 4: Gates + commit.** Standard gate block (`sdr-dsp`); commit the integration test + any tuned constants with `test(dsp): WEFAX geometry integration + tuned constants from real fixtures (#877)`.

> **Phase 1 exit:** a legible chart renders from real audio. Phase 2 makes it live.

---

## Phase 2 — Live integration

### Task 8: WefaxImage shared handle

**Files:**
- Create: `crates/sdr-radio/src/wefax_image.rs`, `crates/sdr-radio/src/wefax_image/tests.rs`
- Modify: `crates/sdr-radio/src/lib.rs` (`pub mod wefax_image;`)

**Interfaces:**
- Produces: `WefaxImage` (owner) with `new()`, `handle() -> WefaxImageHandle`, `clear()`; `#[derive(Clone)] WefaxImageHandle` with `write_line(&self, line_index: u32, width: u32, pixels: &[u8])`, `snapshot(&self) -> Option<WefaxSnapshot>`, `take_completed(&self) -> Option<CompletedWefaxImage>`, `clear(&self)`; `CompletedWefaxImage { width: u32, height: u32, pixels: Vec<u8> }` with `to_flat_gray(&self) -> Vec<u8>`. Model on `crates/sdr-radio/src/lrpt_image.rs` (continuous growing image, `Arc<Mutex<Inner>>`, `lock_or_recover()` poison handling).

- [ ] **Step 1: Read `crates/sdr-radio/src/lrpt_image.rs`** and mirror its structure (lock helper, lazy dim init, out-of-order-safe `write_line`, non-destructive `snapshot`, `take_completed` swap+reset).

- [ ] **Step 2: Write the failing test** (`wefax_image/tests.rs`)

```rust
use super::*;

#[test]
fn write_then_snapshot_roundtrips_rows() {
    let img = WefaxImage::new();
    let h = img.handle();
    let row = vec![128u8; 1809];
    h.write_line(0, 1809, &row);
    h.write_line(1, 1809, &row);
    let snap = h.snapshot().expect("snapshot after writes");
    assert_eq!(snap.width, 1809);
    assert!(snap.height >= 2);
}

#[test]
fn take_completed_swaps_and_resets() {
    let img = WefaxImage::new();
    let h = img.handle();
    h.write_line(0, 1809, &vec![200u8; 1809]);
    let done = h.take_completed().expect("completed image");
    assert_eq!(done.width, 1809);
    assert!(h.take_completed().is_none(), "reset after take");
}
```

- [ ] **Step 3: Run to verify fail; implement `wefax_image.rs`; run to pass.** `cargo test -p sdr-radio --lib wefax_image::`.

- [ ] **Step 4: Gates + commit.** Standard gate block (`sdr-radio`, module `wefax_image`); commit `feat(radio): WEFAX shared image handle (#877)`.

---

### Task 9: DemodMode::Wefax + WefaxDemodulator

**Files:**
- Modify: `crates/sdr-types/src/...` (the `DemodMode` enum — add `Wefax`), `crates/sdr-radio/src/demod/mod.rs` (enumerate)
- Create: `crates/sdr-radio/src/demod/wefax.rs`

**Interfaces:**
- Consumes: the `Demodulator`/`DemodConfig` traits used by `crates/sdr-radio/src/demod/usb.rs` and `lrpt.rs`.
- Produces: `WefaxDemodulator` — a USB-based demod (`vfo_reference: Lower`) with `af_sample_rate: 24_000`, `bandwidth_locked: true`, `default_bandwidth ≈ 2_400`, and `DemodMode::Wefax` routed to it.

- [ ] **Step 1: Read `crates/sdr-radio/src/demod/usb.rs` and `lrpt.rs`.** USB gives the config template; LRPT shows the `bandwidth_locked: true` pattern.

- [ ] **Step 2: Add `Wefax` to `DemodMode`** in `sdr-types`. Compile the workspace to enumerate every non-exhaustive `match` on `DemodMode` that now needs a `Wefax` arm; add arms (route like USB where a demod behaviour is needed). Add a failing unit test asserting the mode maps to the WEFAX demod config:

```rust
#[test]
fn wefax_mode_uses_locked_usb_passband() {
    let cfg = demod_config_for(DemodMode::Wefax); // follow the existing lookup fn name
    assert_eq!(cfg.af_sample_rate, 24_000);
    assert!(cfg.bandwidth_locked);
}
```

- [ ] **Step 3: Write `crates/sdr-radio/src/demod/wefax.rs`** mirroring `usb.rs` with the locked config; wire it in `demod/mod.rs`.

- [ ] **Step 4: Run** the new test + `cargo build --workspace` (all `DemodMode` matches covered). Gates + commit `feat(radio): dedicated WEFAX demod mode (#877)`.

> **Scope guard:** touch every `DemodMode` match only enough to compile with sensible USB-like behaviour. Do not restyle unrelated demod code.

---

### Task 10: Controller messages

**Files:**
- Modify: `crates/sdr-core/src/messages.rs`

**Interfaces:**
- Produces: `UiToDsp::SetWefaxImage(sdr_radio::wefax_image::WefaxImageHandle)`, `UiToDsp::ClearWefaxImage`; `DspToUi::WefaxLineDecoded(u32)`, `DspToUi::WefaxImageComplete { width: u32, height: u32, pixels: Vec<u8> }`, `DspToUi::WefaxState(sdr_dsp::wefax::WefaxState)`. Mirror the `SetSstvImage`/`SstvLineDecoded`/`SstvImageComplete` variants.

- [ ] **Step 1: Add the variants** next to the SSTV equivalents (grep `SetSstvImage`, `SstvImageComplete`). Re-export `WefaxState` at `messages.rs` top like `AptLine` is re-exported if a message carries it.
- [ ] **Step 2: `cargo build -p sdr-core`** to confirm the enums compile (handlers added in Task 11). Gates (build + fmt + clippy) + commit `feat(core): WEFAX UiToDsp/DspToUi messages (#877)`.

> Message enums often have exhaustive matches in the UI/controller; expect Task 11/12 to add arms. A bare `cargo build -p sdr-core` may pass now; the workspace build is green only after Tasks 11–12.

---

### Task 11: Controller tap (wefax_decode_tap) + DspState wiring

**Files:**
- Create: `crates/sdr-core/src/controller/wefax.rs`
- Modify: `crates/sdr-core/src/controller.rs` (DspState fields, gated dispatch block, `reset_imaging_decoders`, `SetWefaxImage`/`ClearWefaxImage` handlers)

**Interfaces:**
- Consumes: `WefaxDecoder`, `WefaxImageHandle`, `downmix_pre_gate_mono` (from `controller/audio.rs`), `DemodMode::Wefax`, the new messages.
- Produces: `wefax_decode_tap(state: &mut DspState, dsp_tx: &Sender<DspToUi>, audio_count: usize)`.

- [ ] **Step 1: Read `crates/sdr-core/src/controller/sstv.rs`** (the closest AF-tap template) and the NFM-gated dispatch block in `controller.rs`.

- [ ] **Step 2: Add DspState fields** (mirror the sstv fields): `wefax_decoder: Option<WefaxDecoder>`, `wefax_mono_buf: Vec<f32>`, `wefax_init_failed_at_rate: Option<u32>`, `wefax_image: Option<WefaxImageHandle>`. Add `Set/ClearWefaxImage` handlers setting/clearing `wefax_image`. Reset all four in `reset_imaging_decoders`.

- [ ] **Step 3: Write `wefax_decode_tap`** — lazy-init `WefaxDecoder::new(radio.audio_sample_rate())` (cache failures in `wefax_init_failed_at_rate`, warn once), `downmix_pre_gate_mono`, `process`, `handle.write_line(idx, PIXELS_PER_LINE as u32, &line.pixels)` per line + `dsp_tx.send(DspToUi::WefaxLineDecoded(idx))`; on `take_chart_complete()` → `take_completed()` + `DspToUi::WefaxImageComplete{..}`; emit `DspToUi::WefaxState(..)` on state change. Requires `wefax_image.is_some()` (silent otherwise, like LRPT).

- [ ] **Step 4: Add the gated dispatch** in `controller.rs`: a block that runs `wefax_decode_tap` when `state.radio.current_mode() == DemodMode::Wefax && audio_count > 0` (parallel to the NFM block that runs the APT/SSTV taps).

- [ ] **Step 5:** `cargo test -p sdr-core` + `cargo build --workspace`. Add a controller unit test if the existing `controller/tests/` harness supports a WEFAX-mode tap (mirror `controller/tests/sstv.rs`); otherwise rely on the DSP tests + smoke test. Gates + commit `feat(core): WEFAX decode tap + controller wiring (#877)`.

---

### Task 12: Live viewer (wefax_viewer.rs)

**Files:**
- Create: `crates/sdr-ui/src/wefax_viewer.rs`, `crates/sdr-ui/src/wefax_viewer/tests.rs`
- Modify: `crates/sdr-ui/src/state.rs` (viewer slots), the window action wiring (mirror `connect_sstv_action`), `crates/sdr-ui/src/window/dsp_events.rs` (`WefaxLineDecoded`/`WefaxImageComplete`/`WefaxState` handlers), `crates/sdr-ui/src/lib.rs`/module decl.

**Interfaces:**
- Consumes: `WefaxImageHandle::snapshot`, the new `DspToUi` messages, `UiToDsp::SetWefaxImage`.
- Produces: `WefaxImageView` (Cairo greyscale surface, event-driven `update_from_handle`), `connect_wefax_action(app, parent_provider, state)` registering `app.wefax-open` / accel `<Ctrl><Shift>f`, `open_wefax_viewer_if_needed(...)`. Pause/Resume + Export PNG header bar; a `WefaxState` status label. Auto-open when WEFAX mode is selected (hook in `on_demod_mode_changed`).

- [ ] **Step 1: Read `crates/sdr-ui/src/sstv_viewer.rs`** and mirror it (greyscale L8→ARGB32 where SSTV used RGB; one channel replicated to R=G=B).

- [ ] **Step 2: Unit test the pure render helper** (`wefax_viewer/tests.rs`) — e.g. greyscale→ARGB byte packing (the one pure fn worth testing; GTK widgets are smoke-tested, not unit-tested):

```rust
#[test]
fn gray_to_argb_replicates_channels() {
    let argb = gray_to_argb(&[0u8, 128, 255], 3);
    // little-endian B,G,R,A; grey replicates across B=G=R
    assert_eq!(&argb[4..7], &[128, 128, 128]);
}
```

- [ ] **Step 3: Implement the viewer** (Pause/Resume, Export PNG via Cairo `write_to_png`, status label, `SetWefaxImage` on open). Wire the action + accel + `dsp_events` handlers + the auto-open-on-mode hook.

- [ ] **Step 4:** `cargo test -p sdr-ui --features whisper-cpu --lib wefax_viewer::` + `cargo clippy -p sdr-ui --features whisper-cpu --all-targets -- -D warnings`. Gates + commit `feat(ui): live WEFAX chart viewer (#877)`.

> **User smoke test after this task:** `make install CARGO_FLAGS="--release --no-default-features --features sherpa-cuda"`, then the user tunes a fax freq in WEFAX mode and confirms the viewer paints. Claude does not launch the GTK app.

---

### Task 13: Save path (manual + auto on chart complete)

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/satellites_recorder.rs` (`wefax_output_path`), `crates/sdr-ui/src/window.rs` (auto-save on `WefaxImageComplete`)

**Interfaces:**
- Produces: `wefax_output_path(now: DateTime<Local>) -> PathBuf` → `~/sdr-recordings/wefax-{timestamp}.png` (no satellite slug — WEFAX is not a pass). Reuse `pass_timestamp`.

- [ ] **Step 1: Write the failing test** (`satellites_recorder.rs` inline tests, or its `tests.rs`)

```rust
#[test]
fn wefax_output_path_has_prefix_and_png_ext() {
    let now = /* fixed Local DateTime as in existing recorder tests */;
    let p = wefax_output_path(now);
    let s = p.to_string_lossy();
    assert!(s.contains("sdr-recordings"));
    assert!(s.contains("wefax-"));
    assert!(s.ends_with(".png"));
}
```

- [ ] **Step 2: Implement `wefax_output_path`** next to `png_path_for` (reuse `glib::home_dir().join("sdr-recordings")` + `pass_timestamp(now)`). Run test → PASS.

- [ ] **Step 3: Auto-save wiring** in `window.rs`: on `DspToUi::WefaxImageComplete{width,height,pixels}`, write the greyscale PNG to `wefax_output_path(Local::now())` via the same Cairo/`image` encoder the viewer's Export uses; the viewer's manual Export button covers on-demand saves.

- [ ] **Step 4:** `cargo test -p sdr-ui --features whisper-cpu` + gates. Commit `feat(ui): WEFAX chart auto-save + output path (#877)`.

---

## Final review + PR

- [ ] **Whole-branch review** (subagent-driven-development's final step) on the most capable model.
- [ ] **PR** to `main` titled `feat: WEFAX / HF radiofax decoder (#877)`, body ending with the `🤖 Generated with Claude Code` line + session URL. Note in the PR description that the render gate was validated against two local fixtures (not committed) and that whisper-cuda/sherpa-rocm build flavors were or were not exercised, per the triple-build rule (this feature does not touch `sdr-transcription`, so the mutex flavors are unaffected — state that explicitly).
- [ ] Wait for CodeRabbit + Codacy; address per the usual workflow (0 new Codacy issues; all threads resolved).

---

## Self-Review notes (author)

- **Spec coverage:** discriminator (T1), pixel clock (T2), offline render (T3), tones (T4), phasing+slant (T5), state machine/segmentation (T6), real-data legibility (T7), image handle (T8), demod mode (T9), messages (T10), tap (T11), viewer (T12), save (T13). All spec components mapped.
- **Type consistency:** `WefaxLine.pixels: [u8; PIXELS_PER_LINE]`, `WefaxDecoder::{new,new_free_running,process,state,take_chart_complete}`, `WefaxImageHandle::{write_line,snapshot,take_completed,clear}`, messages `SetWefaxImage/ClearWefaxImage/WefaxLineDecoded/WefaxImageComplete/WefaxState` used consistently across tasks.
- **Known tuning risk:** tone-detector normalisation and phasing/slant sign are the constants most likely to need adjustment against real audio (T4/T5 synthetic tests pin behaviour; T7 tunes values). Flagged in-task.
