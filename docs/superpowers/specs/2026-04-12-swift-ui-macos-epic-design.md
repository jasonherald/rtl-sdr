---
name: SwiftUI macOS Frontend — Epic Design
description: Top-level design for a native SwiftUI/macOS frontend driving the existing Rust SDR engine via a hand-rolled C ABI
type: spec
---

# SwiftUI macOS Frontend — Epic Design

**Status:** Draft
**Date:** 2026-04-12
**Branch:** `feature/swift-ui-planning`
**Tracking issues:** TBD (epic + child issues to be opened after spec review)

---

## Goal

Ship a native macOS SwiftUI frontend that drives the existing Rust SDR engine and reaches **feature parity** with the GTK4 UI on Linux. The two frontends consume the same headless engine through a single, narrow C ABI; neither one owns engine state.

This is a multi-PR effort. The first deliverable is an **MVP** that runs end-to-end on macOS with RTL-SDR hardware: live spectrum/waterfall, click-to-tune, FM/AM/WFM/NFM demod, and CoreAudio output. Everything else (bookmarks, RadioReference, transcription, IQ recording, network sources, file playback) lands incrementally on top of that foundation behind the same FFI.

## Non-Goals

- **Replacing the GTK UI.** Linux users keep `sdr-ui` exactly as it is today. Both frontends consume the same `sdr-core` facade going forward.
- **Mac App Store distribution.** Initial release is developer-ID signed and notarized for direct download. App Store sandboxing has implications for USB device access that we don't want to fight in v1.
- **iPadOS / iOS support.** Architecturally the FFI would allow it; in practice the rendering, USB access story, and UI idioms differ enough that it's a separate effort.
- **A second cross-platform UI toolkit.** No egui/Slint/Tauri/Iced rewrite. SwiftUI on macOS, GTK on Linux. Two frontends, one engine.
- **Changing the existing message enum names** in `sdr-ui`. The FFI is a *new* surface; the GTK code keeps using its current `UiToDsp`/`DspToUi` channels until the `sdr-core` extraction (PR series 2) migrates it.
- **Replacing `sdr-transcription` in the FFI for v1.** That crate is actively being rebuilt in parallel (sherpa-onnx integration); we keep it out of the FFI surface until it stabilizes.

## Background

### Why a second frontend at all

Today the only way to run this project end-to-end is on Linux through GTK4 + libadwaita + PipeWire. The Rust core is platform-clean — it's the *frontend* that pins us to one OS. A native macOS app is the highest-leverage thing we can ship to widen the audience: macOS users get a real app instead of a Homebrew GTK install, and the project gets a second consumer that pressure-tests how clean the engine boundary really is.

### Why SwiftUI specifically

- **Native feel costs nothing extra.** SwiftUI gives us toolbar, sidebar, settings window, menu bar, sheets, popovers, and a native window chrome with no per-widget effort.
- **`@Observable` (macOS 14+) is a near-perfect fit** for a UI driven by streaming engine state: one observable model, everything binds to it, no Combine boilerplate.
- **Metal is first-class.** The spectrum/waterfall is the perf-critical surface; `MTKView` inside `NSViewRepresentable` gives us 60 fps from a Rust-owned ring buffer with predictable cost.
- **Xcode signing/notarization is the path of least resistance** for shipping a macOS binary that users will actually run.

### Why a hand-rolled C ABI (not swift-bridge / UniFFI)

Rejected during planning. Reasoning:

- The engine surface is **narrow and well-defined**: ~12 events out, ~40 commands in (counting `DspToUi`/`UiToDsp` variants in `crates/sdr-ui/src/messages.rs`), plus FFT frame delivery. That's small enough to maintain by hand without paying for codegen.
- **Zero new dependencies on the Rust side.** No build-script tax, no proc-macro compile time, no version-skew risk between the bridge tool and the Rust toolchain.
- **The C header is the contract.** Both Swift and (eventually) any other consumer link against `sdr_core.h`. The header gets reviewed in PRs like any other interface; nothing is hidden behind generators.
- **Control over threading and lifetime.** We want the engine to drive Swift via a callback on the DSP thread (with explicit "marshal to main" rules), and we want the FFT pull function to be lock-free against the writer. Codegen tools either don't model that or hide it behind opinionated wrappers.

The tradeoff is more boilerplate per command function. We accept that — see `2026-04-12-sdr-ffi-c-abi-design.md` for how the surface is kept manageable.

### What's actively in flight on the engine

A parallel session is rebuilding `sdr-transcription` around a `TranscriptionBackend` trait and adding sherpa-onnx for true streaming live transcription. The trait landed yesterday (PR #226); the backend implementation is still being written.

**Implication for this epic:** transcription is **excluded from the MVP FFI surface** and from the `sdr-core` facade until the backend session declares its public API stable. v2 of the FFI adds it. This is why "Transcript Panel" appears in the v2 backlog below, not the MVP.

## High-Level Architecture

```text
┌─────────────────────────────────────────────────────────────────┐
│  SwiftUI app (apps/macos/SDRMac.app)                            │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │ SwiftUI views (sidebar, toolbar, settings)                │  │
│  │     ↕  @Observable CoreModel                              │  │
│  │ MTKView for spectrum + waterfall                          │  │
│  └─────────────────────────────┬─────────────────────────────┘  │
│                                ↓                                │
│  SdrCoreKit (Swift Package) — wraps the C ABI in Swift types    │
│     • SdrCore actor (lifecycle, command dispatch)               │
│     • AsyncStream<CoreEvent>  (events from Rust → Swift)        │
│     • FftFrame (zero-copy view over Rust-owned buffer)          │
└─────────────────────────────┬───────────────────────────────────┘
                              │ C ABI (include/sdr_core.h)
                              │ • sdr_core_create / sdr_core_destroy
                              │ • typed command fns: sdr_core_tune,
                              │   sdr_core_set_demod_mode,
                              │   sdr_core_set_gain, ... (~25 total)
                              │ • sdr_core_set_event_callback
                              │ • sdr_core_pull_fft
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│  sdr-ffi (new crate, staticlib for v1)                          │
│  Translates C structs ↔ sdr-core Rust types.                    │
│  Owns no state — just a thin shim around Box<sdr_core::Engine>. │
└─────────────────────────────┬───────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│  sdr-core (new crate) — headless facade                         │
│  • Owns DspController + SourceManager + SinkManager             │
│  • Public Rust API: Engine::new, send_command, subscribe,       │
│    pull_fft, shutdown                                           │
│  • Consumed by BOTH sdr-ffi (Swift) AND sdr-ui (GTK)            │
└─────────────────────────────┬───────────────────────────────────┘
                              ↓
            existing crates: sdr-pipeline, sdr-radio, sdr-dsp,
            sdr-types, sdr-config, sdr-source-*, sdr-sink-*
            (+ sdr-sink-audio gains a CoreAudio implementation)
```

The dotted line in the GTK UI's current architecture (`sdr-ui` directly owns `dsp_controller.rs`) goes away. After the `sdr-core` extraction PR series, `sdr-ui` is just another consumer of the facade — the same way Swift will be — and the message enums become `sdr-core`'s public API rather than UI-internal types.

## MVP Scope (v0.1)

Everything below must work on a clean macOS install with an RTL-SDR dongle plugged in. Anything not on this list is v2 or later.

**Sources**
- RTL-SDR USB only

**Demodulation**
- WFM (mono), NFM, AM, USB, LSB, CW, DSB, RAW
- Squelch on/off, threshold slider
- Bandwidth control
- De-emphasis (US75/EU50/None)

**Audio out**
- CoreAudio (new sink impl)
- Default device only — no per-device selection in MVP UI (FFI will support it, UI defers it to v2)
- Volume

**Spectrum / waterfall**
- Live FFT plot with frequency scale
- Scrolling waterfall with palette LUT
- Click-to-tune (sets VFO offset)
- Drag-to-resize VFO bandwidth
- Min/max dB sliders
- FFT size selector (1024/2048/4096/8192)
- FFT window function selector

**Tuner controls**
- Center frequency input (Hz, MHz; arrow keys nudge)
- Gain slider with discrete steps from `gains()`
- AGC toggle
- PPM correction

**App shell**
- Native macOS window with toolbar
- NavigationSplitView with sidebar + main content
- Settings window (`Settings { ... }` SwiftUI scene)
- Standard menu bar (File, View, Window, Help)
- Settings persistence via existing `sdr-config` (read/write through FFI on launch/quit)

**Quality bar**
- Universal binary (arm64 + x86_64)
- Developer-ID signed, hardened runtime, notarized
- Launches and runs from a fresh `.app` drop (no terminal, no Homebrew)

## v2 / Full-Parity Backlog

These exist as GitHub issues from day 1, blocked on the MVP epic, so the parity path is visible:

- **Network IQ source** (TCP/UDP) — already in `sdr-source-network`, just needs FFI surface and a UI panel
- **File playback source** — `sdr-source-file` exists; needs FFI + file picker
- **Audio device picker** — enumerate CoreAudio output devices, route through `SetAudioDevice` command
- **IQ recording to WAV**
- **Audio recording to WAV**
- **Bookmarks panel**
- **RadioReference integration** (depends on existing `sdr-radioreference` crate; keychain credential storage already works on macOS)
- **Transcript panel** — blocked on the parallel sherpa-onnx work declaring its API stable
- **Auto-squelch** — blocked on engine-side noise-floor algorithm settling
- **Display panel extras**: averaging modes (Peak/Avg/Min hold), FPS control
- **Noise blanker, FM IF NR, WFM stereo, notch filter** controls
- **DC blocking, IQ inversion, IQ correction** toggles
- **Decimation factor** selector
- **Network audio sink** — `sdr-sink-network` exists, needs FFI + UI

## Milestones

| #  | Milestone                                  | Status   | Spec doc                                                       |
|----|--------------------------------------------|----------|----------------------------------------------------------------|
| M1 | `sdr-core` facade extraction (no FFI yet)  | Pending  | `2026-04-12-sdr-core-extraction-design.md`                     |
| M2 | `sdr-ffi` crate + `sdr_core.h` + Swift package wrapper | Pending  | `2026-04-12-sdr-ffi-c-abi-design.md`                |
| M3 | CoreAudio sink (`sdr-sink-audio` macOS impl) | Pending  | `2026-04-12-coreaudio-sink-design.md`                          |
| M4 | Metal renderer (spectrum + waterfall)      | Pending  | `2026-04-12-swift-ui-rendering-design.md`                      |
| M5 | SwiftUI app shell + MVP panels + wiring    | Pending  | `2026-04-12-swift-ui-surface-design.md`                        |
| M6 | Packaging: Xcode integration, sign, notarize, CI | Pending  | `2026-04-12-swift-ui-packaging-design.md`                |
| V2 | Full-parity backlog issues (one PR each)   | Backlog  | (per-feature plans created when picked up)                     |

M1 → M2 → M3 are all engine-side and unblock each other. M4 and M5 can run in parallel once M2 lands. M6 is last because it requires a runnable app to sign.

## Success Criteria

- The MVP `.app` launches on a clean macOS 26 machine, finds an RTL-SDR dongle, tunes 100.7 MHz, and produces audible WFM through built-in speakers.
- Click-to-tune on the waterfall moves the VFO and audio follows, end-to-end latency under 200 ms.
- Spectrum + waterfall sustain 60 fps at FFT size 4096 with no main-thread allocations per frame.
- The same `sdr-core` API, on the same commit, still drives the GTK UI on Linux without regressions.
- `cargo test --workspace` and `cargo clippy --all-targets --workspace -- -D warnings` are clean.
- A `.app` produced by CI is signed, notarized, and stapled. `spctl --assess` accepts it.

## Risks & Open Questions

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Engine session destabilizes `sdr-core` API mid-extraction | Medium | High | Land M1 (extraction) **before** the sherpa-onnx work touches the same files; coordinate via PRs not channels |
| USB device access on signed/notarized macOS app surprises us | Medium | High | Validate with a "hello world" libusb-rs sign+notarize spike *before* M5 starts. Hardened runtime + `com.apple.security.device.usb` entitlement should be sufficient (no sandbox). |
| Metal waterfall scroll cost at 8192 FFT × 60 fps higher than expected | Low | Medium | Texture ring buffer with UV scroll, not a memmove; spec'd in rendering doc. Fallback: cap waterfall to 30 fps independently of spectrum line. |
| CoreAudio sample-rate negotiation differs from PipeWire's "give me 48k stereo" assumption | Medium | Medium | CoreAudio sink does its own resampling if device rate ≠ engine rate; spec'd in sink doc. |
| `sdr-config` JSON drifts as both UIs add fields | Low | Low | Single source of truth in `sdr-config`; both frontends read/write through the same struct via FFI |
| GTK UI regressions during `sdr-core` extraction | Medium | High | Extraction PR is *behavior-preserving* — same tests must pass before and after, message enums kept byte-compatible |

**Open questions to resolve before M2 starts:**
- Should the FFI expose an explicit "init logging" call so Swift sees `tracing` output via `os_log`? *(Lean: yes — see FFI spec.)*
- Where does the JSON config file live on macOS? `~/Library/Application Support/SDRMac/config.json`? *(Lean: yes, follow Apple convention; the FFI takes the path as a parameter so the host decides.)*
- Do we ship the FFI as a `cdylib` (linked at runtime, smaller `.app`) or `staticlib` (everything in the binary, simplest signing)? *(Lean: `staticlib` for v1 — one binary to sign, no `@rpath` headaches.)*

## Out of Scope (deferred indefinitely)

- Plugin system. SDR++ has one; we don't, and won't, in v1.
- Multi-VFO. Single VFO is enough for MVP and v2.
- Recording to non-WAV formats.
- Cross-process IPC. The engine runs in-process inside the `.app`.

## References

- `docs/PROJECT.md` — overall project map and C++ → Rust source correspondence
- `crates/sdr-ui/src/messages.rs` — current `UiToDsp`/`DspToUi` enums (the FFI surface follows these closely)
- `crates/sdr-ui/src/dsp_controller.rs` — current DSP thread that `sdr-core` will own
- `crates/sdr-pipeline/src/source_manager.rs` — `Source` trait that survives the extraction unchanged
- `docs/superpowers/specs/2026-04-12-transcription-backend-trait-design.md` — parallel work that this epic deliberately does *not* touch
