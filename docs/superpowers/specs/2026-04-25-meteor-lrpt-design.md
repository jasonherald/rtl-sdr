# Meteor-M LRPT Reception — Design

**Status:** Design-complete, implementation not started
**Author:** jasonherald (with Claude)
**Last updated:** 2026-04-25
**Epic:** [#469 — Meteor-M LRPT digital weather satellites (137 MHz)](https://github.com/jasonherald/rtl-sdr/issues/469)
**Related:** epic [#468](https://github.com/jasonherald/rtl-sdr/issues/468) (NOAA APT — sets the precedent for this design), epic [#520](https://github.com/jasonherald/rtl-sdr/issues/520) (LRPT post-MVP enhancements — deferred work)

---

## 1. Goal & success criteria

Receive Meteor-M LRPT downlinks end-to-end as a pure-Rust port: tune the radio at AOS, demodulate the QPSK signal, decode the FEC chain (Viterbi + Reed-Solomon), parse CCSDS frames, reassemble Meteor's reduced-JPEG imagery, and save a multi-channel PNG set on LOS. Same unattended-receive UX as the just-shipped NOAA APT auto-record (#468), but for digital satellites.

**Success looks like:** the user toggles auto-record, walks away, and 12 minutes later finds in `~/sdr-recordings/`:

- `lrpt-METEOR-M-2-3-{timestamp}/composite-rgb.png` — default false-color RGB composite (channels 1+2+3 typical)
- `lrpt-METEOR-M-2-3-{timestamp}/ch{N}.png` — one PNG per imaging channel actually transmitted on this pass

A live LRPT viewer shows the image building up during the pass with channel picker + RGB composite picker.

### In scope (this epic)

- All four pipeline stages: QPSK demod → Viterbi+RS FEC → CCSDS framing → JPEG image assembly
- Catalog (as of April 2026): Meteor-M 2 (137.100 MHz, degraded but operational) and Meteor-M 2-3 (137.900 MHz, operational). Meteor-M 2-4 stays deferred per #506 until Celestrak publishes a TLE.
- Per-channel + composite PNG export at LOS
- Auto-record integration via the existing recorder, generalized for non-APT satellites (closes [#514](https://github.com/jasonherald/rtl-sdr/issues/514))
- Live LRPT viewer with channel/composite picker
- "Receive your first Meteor LRPT pass" walkthrough doc

### Out of scope (deferred — see epic #520)

- Doppler correction ([#521](https://github.com/jasonherald/rtl-sdr/issues/521)) — 38 kHz channel filter absorbs the ±3.5 kHz shift; cross-protocol feature
- Map projection / georeferencing ([#522](https://github.com/jasonherald/rtl-sdr/issues/522)) — downstream polish; cross-protocol
- Meteor spacecraft telemetry decode ([#523](https://github.com/jasonherald/rtl-sdr/issues/523)) — non-imaging VCs dropped in v1
- Archive raw CCSDS frames to disk ([#524](https://github.com/jasonherald/rtl-sdr/issues/524)) — power-user / decoder-developer feature
- macOS native-app integration. `sdr-lrpt` lives under the GTK4-only UI surface in v1, same as the APT decoder (`sdr-radio::apt_image` is consumed by `sdr-ui` but not exposed via `sdr-ffi`). Future macOS LRPT support would route through `sdr-ffi` like other cross-platform features; out of scope for #469.

---

## 2. Architecture & data flow

```text
[I/Q samples from RTL-SDR]
        ↓
   sdr-pipeline                       (existing — same path APT uses)
        ↓
[I/Q at baseband sample rate]
        ↓
┌─────────────────────────────────────────────────────────────┐
│ STAGE 1 — Demod                          sdr-dsp::lrpt      │
│   • Costas loop (QPSK carrier recovery, ±half-symbol)       │
│   • Root-raised-cosine matched filter (β=0.6, span 31)      │
│   • Symbol timing recovery (Gardner)                        │
│   • Hard slice → soft symbols (i8, ±127)                    │
└─────────────────────────────────────────────────────────────┘
        ↓
[soft symbols, 72 ksym/s × 2 (I+Q) = 144k i8/s]
        ↓
┌─────────────────────────────────────────────────────────────┐
│ STAGE 2 — FEC                            sdr-lrpt::fec      │
│   • Viterbi rate-1/2 K=7 convolutional decoder              │
│   • Frame sync (32-bit ASM 0x1ACFFC1D, sliding correlator)  │
│   • De-randomize (PN sequence per CCSDS spec)               │
│   • Reed-Solomon (255, 223), CCSDS dual-basis               │
└─────────────────────────────────────────────────────────────┘
        ↓
[CCSDS Virtual-Channel Data Units (VCDUs), 1024 bytes each]
        ↓
┌─────────────────────────────────────────────────────────────┐
│ STAGE 3 — Frame                          sdr-lrpt::ccsds    │
│   • CCSDS Channel Access Data Unit (CADU) parsing           │
│   • Multiplex demux by VC ID (imaging VCs only — others     │
│     dropped in v1, see #523 for telemetry follow-up)        │
│   • CCSDS packet (M_PDU) reassembly across CADU boundaries  │
└─────────────────────────────────────────────────────────────┘
        ↓
[image packets, one per AVHRR-channel scan-line group]
        ↓
┌─────────────────────────────────────────────────────────────┐
│ STAGE 4 — Image                          sdr-lrpt::image    │
│   • JPEG-DCT-block decode (Meteor's reduced JPEG variant)   │
│   • Per-channel 2D image buffer accumulation                │
│   • False-color RGB compositor (per user-selectable triple) │
│   • PNG export (per channel + composite)                    │
└─────────────────────────────────────────────────────────────┘
        ↓
[per-pass subdirectory of PNGs]    ~/sdr-recordings/lrpt-...
```

### Crate placement rationale

- **Stage 1 lives in `sdr-dsp`** because it's pure DSP (filters, PLL, slicer). Same crate as `sdr-dsp::apt`. Pure functions, no I/O.
- **Stage 2 lives in `sdr-lrpt::fec`**, *not* `sdr-dsp`, because Viterbi + RS aren't really DSP — they're protocol-layer error correction. Future cross-protocol use can lift `sdr-lrpt::fec` into a generic `sdr-fec` crate; YAGNI until that second consumer arrives.
- **Stages 2–4 are protocol logic** that stay together in `sdr-lrpt`. They share types (Frame, VCDU, M_PDU, ImagePacket) that don't need to be public outside the crate.
- **Each stage's interface is a streaming function** — `process(input: &[T_in], output: &mut [T_out]) -> usize` — matching the project-wide DSP convention.

### Threading model

Same as APT: the entire 4-stage pipeline runs on the existing audio-thread tap that APT already uses. LRPT-at-72-ksym/s is well within budget for a single-thread pipeline; no need for stage-level threading.

### Dependency graph for the new code

```text
sdr-dsp           (already exists; gains lrpt:: submodule for stage 1)
   ↑
sdr-lrpt          (new; depends on sdr-dsp, sdr-types)
   ↑
sdr-radio         (already exists; gains lrpt_image::* glue, like apt_image)
   ↑
sdr-ui            (already exists; gains lrpt_viewer.rs + sat-recorder generalization)
```

`sdr-lrpt` itself has no DSP or GTK dependencies — it's a pure-data crate.

---

## 3. File layout + reference codebase mapping

### `crates/sdr-dsp/src/lrpt/` (extends existing `sdr-dsp`)

```text
lrpt/
  mod.rs          // re-exports + LrptDemod top-level processor
  costas.rs       // QPSK Costas loop (carrier recovery)
  rrc_filter.rs   // root-raised-cosine matched filter (β=0.6, span 31)
  timing.rs       // Gardner symbol-timing recovery
  slicer.rs       // hard slice → soft symbols (i8 ±127)
```

Reference: **SDR++'s `decoder_modules/meteor_demodulator/src/`** — `meteor_costas.h` + `meteor_demod.h` are the source-of-truth. ~550 LoC C++, idiomatic translation.

### `crates/sdr-lrpt/src/` (new crate)

```text
src/
  lib.rs          // public API: LrptPipeline, decode_pass(...) entry point
  fec/
    mod.rs        // ConvViterbi + RsBlock chain
    viterbi.rs    // rate-1/2 K=7 convolutional Viterbi (G1=0o171, G2=0o133)
    reed_solomon.rs  // RS(255, 223) over GF(256), CCSDS dual-basis
    sync.rs       // 32-bit ASM correlator (0x1ACFFC1D)
    derand.rs     // CCSDS PN sequence de-randomizer
  ccsds/
    mod.rs        // VCDU / CADU / M_PDU types + parser
    vcdu.rs       // Virtual Channel Data Unit framing
    mpdu.rs       // Multiplexed Protocol Data Unit reassembly
    demux.rs      // VC-ID router (imaging only in v1)
  image/
    mod.rs        // ImageAssembler + ChannelBuffer
    jpeg.rs       // Meteor's reduced-JPEG decoder (DCT block per scan group)
    composite.rs  // false-color RGB compositor
    png_export.rs // multi-PNG writer
  bin/
    replay.rs     // sdr-lrpt-replay CLI binary (PR 5) — fixture file → PNGs
```

### Reference codebases per layer

| Layer | Primary reference | Secondary / cross-check |
|---|---|---|
| FEC Viterbi | `medet/viterbi.c` | `libcorrect` (validation only — we don't link) |
| FEC RS | `medet/correlator.c` + `rs.c` | CCSDS Blue Book 101.0-B-3 (the spec itself) |
| FEC sync | `medet/correlator.c` | `meteor_demod`'s sync (different signal point) |
| CCSDS VCDU | `meteordemod` (digitalvoid7) | CCSDS Blue Book 132.0-B-1 |
| CCSDS M_PDU | `meteordemod` | CCSDS Blue Book 133.0-B-1 |
| Meteor JPEG | `medet/met_jpg.c` | SatDump's `meteor_decoder_module.cpp` |
| Image assembly | `medet/met_to_data.c` | SatDump's image module |

`medet` is the foundational Russian decoder (~2014, C). Everyone else forked from it. Where `medet` is unclear we cross-check against `meteordemod` (modern C++, cleaner) and SatDump (most polished, but wrapped in their plugin framework).

Sub-`mod.rs` boundaries are deliberate. Each file ≤300 lines after the port (matches our file-size convention). Each module's public surface is a small struct + a `process()` method; internals stay private.

### `crates/sdr-radio/src/lrpt_image.rs` (new file in existing crate)

Mirrors `apt_image.rs` exactly — buffers per-channel scan lines, exposes a streaming push API, owns the live image surface that `sdr-ui` reads. ~150 LoC, structurally identical to APT.

---

## 4. UI integration + auto-record generalization

### `crates/sdr-ui/src/lrpt_viewer.rs` (new file)

Fresh from scratch, structurally parallel to `apt_viewer.rs` but rendering very different content:

```text
LrptImageView
  • per-channel buffers (up to 6 AVHRR channels — only those
    actually transmitted on this pass populate)
  • header bar:
      [Channel ▾]    [R: ch1 ▾  G: ch2 ▾  B: ch3 ▾]    [Pause/Resume]    [Export PNG]
  • main view: GtkDrawingArea showing either a single channel
    (greyscale) or the live RGB composite, user-toggleable
  • "Composite" view defaults to the satellite's typical
    visible / near-IR / mid-IR triple (Meteor convention),
    user can remap via the three RGB dropdowns
  • per-channel preview tabs (small thumbnails) so the user can
    confirm which channels are arriving without leaving live view
```

Implements the same `ChannelDecoderConsumer` shape as `AptImageView` (push-line API + clear + export_png) so the recorder can drive both with minimal branching at the call site.

### Satellites panel changes

Minimal:

- Toggle copy: `"Auto-record APT passes"` → `"Auto-record satellite passes"` (now covers Meteor-M too)
- Subtitle copy: `"Tune to the satellite, start the decoder, save the image at LOS."` — unchanged (already protocol-neutral)
- The pass-list rows and tune-button play action are already protocol-neutral; nothing else to touch

### Auto-record generalization (closes #514)

Today the recorder has `is_apt_capable(satellite_name)` hardcoded to NOAA 15/18/19. For LRPT we replace this with catalog-driven dispatch:

```rust
// in sdr-sat::KnownSatellite
pub struct KnownSatellite {
    pub name: &'static str,
    pub norad_id: u32,
    pub downlink_hz: u64,
    pub demod_mode: DemodMode,
    pub bandwidth_hz: u32,
    pub imaging_protocol: Option<ImagingProtocol>,   // NEW
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagingProtocol {
    Apt,    // NOAA 15 / 18 / 19
    Lrpt,   // Meteor-M 2 / 2-3
    // Sstv,  -- added in #472
}
```

The recorder's `is_apt_capable` filter becomes `imaging_protocol.is_some()` (catalog-driven, no string matching), and the existing `Action::StartAutoRecord` grows a `protocol: ImagingProtocol` field. The `interpret_action` wiring in `window.rs::connect_satellites_panel` becomes:

```rust
match protocol {
    ImagingProtocol::Apt => crate::apt_viewer::open_apt_viewer_if_needed(&parent_provider_a, &state_a),
    ImagingProtocol::Lrpt => crate::lrpt_viewer::open_lrpt_viewer_if_needed(&parent_provider_a, &state_a),
}
```

`SavedTune` stays exactly as-is (frequency / VFO / mode / bandwidth / playback / scanner state are all protocol-agnostic). The whole tune-snapshot-and-restore round trip works unchanged.

### Output paths

APT keeps its current single-file shape. LRPT goes into a per-pass subdirectory because there can be up to 7 artefacts per pass (1 composite + up to 6 channels):

```text
~/sdr-recordings/
  apt-NOAA-19-2026-04-25-143022.png            (existing — unchanged)
  lrpt-METEOR-M-2-3-2026-04-25-143022/         (new pattern)
    composite-rgb.png
    ch1.png  ch2.png  ch3.png  (only channels actually transmitted)
```

The save toast at LOS shows the directory path, not individual files. Future Doppler / map-projection work (#521 / #522) lands additional artefacts in the same directory.

---

## 5. Sub-ticket decomposition

Eight sub-tickets, mirroring APT's count. Bottom-up: each PR is independently testable and leaves the project in a working state. PRs 1–5 are pure-Rust unit-tested ports against synthetic test vectors. PR 6 generalizes the recorder framework. PR 7 is the e2e finishing PR that flips Meteor on. PR 8 is docs.

| # | PR scope | Crate(s) touched | Closes |
|---|---|---|---|
| 1 | **Stage 1: QPSK demod.** Costas loop + RRC matched filter + Gardner symbol-timing + slicer. Tests with synthetic IQ → known soft-symbol fixtures. | `sdr-dsp::lrpt` (new submodule) | new |
| 2 | **Stage 2a: Viterbi + frame sync + derand.** Rate-1/2 K=7 Viterbi, 32-bit ASM correlator (`0x1ACFFC1D`), CCSDS PN de-randomizer. Tests with bit-vectors encoded by a known reference. | `sdr-lrpt::fec` (new crate, partial) | new |
| 3 | **Stage 2b: Reed-Solomon (255, 223).** GF(256) arithmetic, CCSDS dual-basis representation, RS decoder. Tests against CCSDS Blue Book test vectors. | `sdr-lrpt::fec` (completes the FEC chain) | new |
| 4 | **Stage 3: CCSDS framing.** VCDU / CADU parser, M_PDU reassembly across CADU boundaries, virtual-channel demux (image VCs only — non-imaging routed to `Discard` per the #523 deferral). | `sdr-lrpt::ccsds` (new submodule) | new |
| 5 | **Stage 4: Image assembly + Meteor JPEG.** Meteor's reduced-JPEG decoder (DCT-block per scan-line group), per-channel image buffer, false-color RGB compositor. Includes a small CLI binary `sdr-lrpt-replay` that decodes a fixture frame file → PNGs, end-to-end test of stages 1-4. | `sdr-lrpt::image`, `sdr-radio::lrpt_image` | new |
| 6 | **Auto-record generalization** (closes [#514](https://github.com/jasonherald/rtl-sdr/issues/514)). Add `ImagingProtocol` enum to `sdr-sat`, `imaging_protocol` field to `KnownSatellite`, recorder filter from `is_apt_capable` → `imaging_protocol.is_some()`, `protocol` field on `Action::StartAutoRecord`, branching dispatch in `interpret_action`. Meteor catalog entries stay `None` for now — generalization lands without changing user-visible behaviour. | `sdr-sat`, `sdr-ui::sidebar::satellites_recorder`, `sdr-ui::window` | #514 |
| 7 | **End-to-end LRPT integration.** New `sdr-ui::lrpt_viewer.rs` (channel picker + RGB composite + pause/export). New `sdr-radio::lrpt_decoder` driver that wires the 4-stage pipeline to the existing audio-thread tap. Flip Meteor-M 2 / Meteor-M 2-3 catalog entries to `imaging_protocol = Some(Lrpt)`. Hook `interpret_action`'s LRPT branch. Update Satellites panel toggle copy. Per-pass subdirectory output paths. | `sdr-ui`, `sdr-radio`, `sdr-sat` | — |
| 8 | **Docs walkthrough.** `docs/guides/lrpt-reception.md` — antenna requirements (same V-dipole, more SNR-sensitive), "your first Meteor LRPT pass" UI flow, troubleshooting (sync loss / FEC dropouts / missing channels / unfamiliar composite colours). Update `CLAUDE.md` + `README.md`. | `docs/`, `CLAUDE.md`, `README.md` | #469 |

### Sequencing notes

- PRs 1–5 produce no user-visible change; they're pure additions, fully unit-tested, easy CR review.
- The CLI replay binary in PR 5 is the first end-to-end visual smoke test — fixture file in, PNGs out. Lets us validate the decoder against captured pass data (which we'll harvest from a real Meteor pass overnight or use SatDump to record a known-good IQ capture for fixturing).
- PR 6 closes #514 and lands the framework, but Meteor stays `None` until PR 7 — keeps each PR's diff strictly additive.
- PR 7 is the big finishing PR. It's larger than the others (viewer + driver + catalog + wiring), but each piece needs the others to actually ship value.

### Where the cross-cutting test additions live

The four comprehensive-testing additions from §6 don't get their own sub-tickets — they're folded into the stages they protect:

- **Property-based FEC tests** ship inside PRs 2 and 3 (whichever stage the property test covers).
- **Golden-output regression infrastructure** is set up in PR 4 (CCSDS — first stage where we have realistic frame outputs to compare); PRs 5 and 7 extend the golden corpus.
- **Coverage gate** is wired into CI in PR 1 (smallest landing surface for the CI plumbing change).
- **Criterion benches** are added per-stage alongside the stage PR that introduces the code (PR 1 for demod, PRs 2–3 for FEC, PR 4 for framing, PR 5 for image).

### Closes-which-ticket convention

Per repo convention, the `closes #469` keyword goes on the docs PR (PR 8) — that's the final piece that finishes the epic. PR 7 finishes the *functional* receive loop but doesn't close the epic until docs land.

---

## 6. Test strategy

The biggest risk in this epic is the FEC math (Viterbi + RS) — bugs there don't crash, they produce subtly-corrupted images that look "almost right" until you compare side-by-side with a known-good decoder. Test discipline is what separates a port that ships from one that limps. The strategy varies by stage.

### Per-stage testing

**Stage 1 (DSP — demod).** Synthetic test vectors: generate a known QPSK pattern in Rust, run it through the demod, assert the output soft-symbol stream matches expected within a tolerance (the timing-recovery loop converges asymptotically, so exact-match isn't realistic — bit-error-rate tolerance threshold is the right shape).

**Stage 2 (FEC — Viterbi + RS).** Both layers have published test vectors:
- **Viterbi (CCSDS 131.0-B-3, K=7, G1=0o171, G2=0o133):** the standard's appendix has known input/output bit pairs we copy verbatim into a `#[test]` module.
- **Reed-Solomon (255, 223 dual-basis, CCSDS 101.0-B-3):** standard's appendix has reference codewords. Plus `medet` ships its own self-test fixtures.

These give us bit-exact validation without needing any captured satellite data. If our output diverges from the spec's reference vectors by a single bit, the test fails. This is the strongest correctness guarantee in the entire epic.

**Stage 3 (CCSDS framing).** No "official spec test vector" for VCDU/M_PDU reassembly across boundaries — the spec defines the format, not specific instances. Strategy:
- Hand-craft synthetic CADU streams with known M_PDU contents that exercise edge cases (M_PDU spanning two CADUs, M_PDU exactly aligned to a CADU boundary, missing/corrupt CADU mid-stream)
- Cross-check our output against `meteordemod`'s output on the same input via the **golden-output regression pattern** (see comprehensive testing item 2 below) — no runtime C dependency, only one-time fixture generation

**Stage 4 (image — JPEG + composite).** Validation strategy splits:
- **Meteor JPEG decoder:** known DCT-block layouts. Decode a hand-crafted block + assert pixel values match.
- **Channel buffer / composite:** structural tests (push N lines, assert image dimensions are right, assert RGB composite samples three channels at the same row index).
- **Real-pass validation:** PR 5's `sdr-lrpt-replay` CLI tool gives us the full chain. Capture an IQ recording of a real Meteor pass (or borrow a known-good one from SatDump's example archive), run it through our decoder, visually compare the output PNG against SatDump's output PNG of the same recording. Integration-level smoke test.

### Comprehensive testing additions

Beyond per-stage unit tests, four cross-cutting test types add high-value coverage at the highest-bug-density layers:

**1. Property-based testing for FEC.** Use `proptest` to round-trip random valid inputs through encoder + decoder for `sdr-lrpt::fec`. Generate a random bit stream → encode through Viterbi+RS → corrupt a controlled number of bits → decode → assert recovery succeeds within the FEC's correction budget. The CCSDS spec vectors prove we handle one specific input correctly; property tests prove we handle the generative space correctly. Particularly high-value for FEC math.

**2. Golden-output regression.** Run a known IQ recording through a reference decoder (`medet` or SatDump) one time at fixture-creation time; commit the post-FEC frame stream + final per-channel PNGs as golden files in `crates/sdr-lrpt/tests/fixtures/golden/`. Tests run our decoder on the same IQ and assert byte-equality on frames + structural-similarity (SSIM > 0.99) on PNGs. Differential-testing strength against an established reference without dragging C build dependencies into our test environment. The one-time generation step lives in `crates/sdr-lrpt/tests/fixtures/REGENERATE_GOLDENS.md` for when goldens need refreshing.

**3. 90% coverage gate on `sdr-lrpt`.** `cargo-llvm-cov` integrated into CI for the new crate. Pure-data crate with no GTK or async surface — high coverage is achievable. CI fails if coverage drops below threshold on a PR. Stops the "test suite passes but a whole module is untested" failure mode.

**4. Criterion benchmarks per stage.** `criterion` benches in `crates/sdr-lrpt/benches/` measuring throughput of each stage on a representative input size. Establishes a per-stage performance floor (recorded baselines committed to the repo); catches accidental O(n²) refactors that pass functional tests but tank real-pass throughput. Also gives us hard data for the "Rust port is faster than the C original" claim.

### Test fixtures committed to the repo

- `crates/sdr-lrpt/tests/fixtures/ccsds_131_viterbi_vectors.txt` — copied from the spec
- `crates/sdr-lrpt/tests/fixtures/ccsds_101_rs_vectors.txt` — copied from the spec
- `crates/sdr-lrpt/tests/fixtures/synthetic_cadu_stream.bin` — small (a few KB), hand-built, exercises framing edge cases
- `crates/sdr-lrpt/tests/fixtures/golden/` — reference frame stream + PNG outputs from medet/SatDump on a known IQ recording

A real-pass IQ recording (30–50 MB) is **not** committed. Kept under `~/sdr-recordings/` on the dev machine, referenced by an `#[ignore]`-gated integration test that runs on demand (`cargo test -- --ignored real_pass`). Same convention as APT's overnight smoke tests.

### Intentionally out of scope for testing

- Fuzz testing (`cargo-fuzz`) — overkill for v1; the framer's input is already trusted (post-RS-FEC bytes), and we're not exposing this to untrusted network input.
- BER-vs-SNR theoretical curve sweeps — research-grade signal-analysis work that's interesting but doesn't catch correctness bugs the spec vectors + property tests don't already catch.

### Triple-build burden

LRPT doesn't touch `sdr-transcription`, so no triple-build burden — single `--release` build per PR is fine.

---

## 7. Open questions for plan phase

These are intentionally deferred to the implementation plan rather than this design:

- **Costas loop bandwidth values** — port from SDR++'s tuned constants directly, validate empirically against captured passes.
- **RRC filter rolloff factor** — Meteor uses β=0.6 per `meteor_demod`; carry forward unless capture data suggests otherwise.
- **Default RGB composite triple** — Meteor's typical visible/near-IR/mid-IR (channels 1/2/3 by convention). Per-pass overrides via the viewer's RGB dropdowns.
- **JPEG quality table** — Meteor's reduced-JPEG uses a fixed quantization table; copy verbatim from `medet/met_jpg.c`.
- **Concrete `imaging_protocol` field default** — `None` for any catalog entry that doesn't have a working decoder yet (forward-compat for SSTV in #472).

---

## 8. Verification

Epic complete when:

1. `cargo build --workspace` compiles cleanly with `sdr-lrpt` added.
2. `cargo test --workspace` — all per-stage + property + golden tests pass.
3. CI coverage gate reports ≥90% on `sdr-lrpt`.
4. `sdr-lrpt-replay <fixture.iq>` produces PNGs that match the golden references (SSIM > 0.99).
5. Live overnight smoke test against a real Meteor-M 2-3 pass produces a recognizable Earth image with at least three channels populated.
6. Auto-record toggle (renamed) auto-records both NOAA and Meteor passes correctly, with full tune restore at LOS.
7. `docs/guides/lrpt-reception.md` walks a first-timer through the receive flow.
8. `CLAUDE.md` updated: `sdr-lrpt` added to the workspace roster, "Satellite reception" subsection extended.
9. `README.md` updated: Weather-satellites section mentions LRPT, architecture diagram lists `sdr-lrpt`.
10. Epic [#469](https://github.com/jasonherald/rtl-sdr/issues/469) closed with a wrap-up summary listing the 8 shipped sub-tickets.
