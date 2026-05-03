# SDR-RS — Project Guide

Rust port of SDR++ — software-defined radio application with GTK4 UI.

## Architecture

23-member workspace (root binary + 22 library crates) with clear dependency boundaries:

```text
sdr-types             → Foundation types, errors, constants (no internal deps)
sdr-dsp               → Pure DSP: math, filters, FFT, demod, resampling, APT decoder (depends on: types)
sdr-config            → JSON configuration persistence + OS keyring (depends on: types)
sdr-pipeline          → Threading, streaming, signal path (depends on: types, dsp, config)
sdr-rtlsdr            → Rust port of librtlsdr over rusb — 5 tuner families (no internal deps)
sdr-rtltcp-discovery  → mDNS browser/responder for `_rtl_tcp._tcp.local.` services
sdr-server-rtltcp     → `rtl_tcp` server — share a local dongle over TCP
sdr-source-rtlsdr     → RTL-SDR source module (depends on: types, pipeline, rtlsdr, config)
sdr-source-network    → TCP/UDP IQ source (depends on: types, pipeline, config)
sdr-source-file       → WAV file playback source (depends on: types, pipeline, config)
sdr-sink-audio        → PipeWire/CoreAudio output (depends on: types, pipeline, config)
sdr-sink-network      → TCP/UDP audio output (depends on: types, pipeline, config)
sdr-radio             → Radio decoder, demod, IF/AF chains, APT image buffer (depends on: types, dsp, pipeline)
sdr-radioreference    → RadioReference.com SOAP client (depends on: types, config)
sdr-sat               → Satellite pass prediction (SGP4) + TLE cache + ground-station catalog
sdr-scanner           → Multi-channel scanner engine — projection, dwell/hang, lockout
sdr-transcription     → Whisper OR Sherpa-onnx backend, Silero VAD, spectral denoiser
sdr-ffi               → C-ABI surface for the future macOS native-app bridge
sdr-core              → Headless cross-platform engine facade (macOS port path)
sdr-splash            → Cross-platform splash subprocess controller (stdin wire protocol)
sdr-splash-gtk        → Linux GTK4 splash window implementation
sdr-ui                → GTK4/libadwaita UI — Linux-only (depends on: all above)
sdr (binary)          → Entry point (depends on: ui, core, splash, transcription, …)
```

## Transcription backend feature mutex

**Whisper and Sherpa-onnx are mutually exclusive cargo features**, and within Sherpa, `sherpa-cpu` and `sherpa-cuda` are also mutually exclusive. The `sdr-transcription` crate has `compile_error!` guards enforcing exactly one. This was originally a PR 2-era workaround for a heap corruption between whisper-rs's libstdc++ state and ONNX Runtime's `ParseSemVerVersion` regex constructors that only manifests when both C++ dep trees are in the same binary; the sherpa-cpu/sherpa-cuda split additionally reflects that the sys crate's CUDA feature rebuilds against a different upstream prebuilt archive.

**Build flavors:**

```bash
# Whisper — multilingual, mature GPU acceleration
make install CARGO_FLAGS="--release"                                # Whisper CPU (default)
make install CARGO_FLAGS="--release --features whisper-cuda"        # User's daily driver (RTX 4080 Super)
make install CARGO_FLAGS="--release --features whisper-hipblas"     # AMD ROCm
make install CARGO_FLAGS="--release --features whisper-vulkan"      # Cross-vendor GPU

# Sherpa-onnx — Zipformer / Moonshine / Parakeet, English-only
make install CARGO_FLAGS="--release --no-default-features --features sherpa-cpu"   # Sherpa CPU
make install CARGO_FLAGS="--release --no-default-features --features sherpa-cuda"  # Sherpa + NVIDIA GPU
```

**`sherpa-cuda` needs CUDA 12.x + cuDNN 9.x runtime libraries** because sherpa-onnx's CUDA prebuilt is pinned to onnxruntime 1.23.2 built against that toolchain. CUDA majors are not ABI-compatible, and Arch ships CUDA 13 today, so we **sideload the exact set of CUDA 12 runtime libs** NVIDIA publishes as a developer redistributable rather than requiring a parallel system install.

`make install CARGO_FLAGS="... --features sherpa-cuda"` automatically runs `scripts/fetch-cuda-redist.sh`, which downloads ~1.83 GB of NVIDIA tarballs (cudart, cublas, cufft, curand, cudnn) into `$HOME/.cache/sdr-rs/cuda-redist/`, verifies SHA-256, extracts the runtime `.so` files while preserving the symlink chain, and installs them into `$(BINDIR)/sdr-rs-libs/` alongside the binary. The binary's `DT_RPATH` — forced to old-style via `-Wl,--disable-new-dtags` in `build.rs` so it cascades to libraries loaded via `dlopen` — resolves them at runtime.

The only system-level requirement is a working NVIDIA kernel driver (`libcuda.so.1`, packaged with `nvidia`/`nvidia-utils` on Arch); everything in userspace is self-contained. Linux x86_64 only. The first build also downloads a ~235 MB sherpa-onnx CUDA prebuilt from k2-fsa. During the PR window the sherpa-onnx dep is pinned to the [jasonherald/sherpa-onnx](https://github.com/jasonherald/sherpa-onnx) fork on branch `feat/rust-sys-cuda-support`; swap back to a crates.io version pin once the upstream PR merges and releases.

**Triple-build testing rule:** any change to `sdr-transcription` or its callers MUST be tested with `whisper-cuda`, `sherpa-cpu`, AND `sherpa-cuda` builds sequentially before pushing. The cfg gates make it easy to break one without noticing. CI runs `cargo check` on sherpa-cpu and sherpa-cuda (adding CUDA toolkit to CI runners is not worth the minutes, so whisper-cuda is user-verified locally).

Runtime model selection for Sherpa builds is in-place (drop-old-recognizer-build-new) and does NOT require a restart — PR 5 architecture. The PR 2 "pre-GTK init" half of the heap-corruption workaround turned out to be unnecessary once the feature mutex was in place; only the first recognizer needs pre-GTK creation, and subsequent swaps post-GTK work fine.

## Build & Test

```bash
cargo build --workspace          # Build all crates
cargo test --workspace           # Run all tests
cargo clippy --all-targets --workspace -- -D warnings  # Lint
cargo fmt --all -- --check       # Format check
make lint                        # All of the above + cargo deny + cargo audit
```

## Workflow

- **Always work on feature branches**, never commit directly to main
- **All changes go through PRs** reviewed by CodeRabbit
- Branch naming: `feature/description`, `fix/description`, `chore/description`
- CI runs on every PR: clippy, fmt, test, cargo-deny

## Key Conventions

### Code style

- No `unwrap()` or `panic!()` in library crates
- Library crates use `thiserror` for error types (never `anyhow` — it erases types)
- `anyhow` is only allowed in the `sdr` binary (`src/main.rs`)
- No `println!()` — use `tracing` macros (`info!`, `debug!`, `warn!`, `error!`)
- Prefer `&str` over `String` in function parameters
- Workspace lints: clippy pedantic enabled (with select allows)
- Tests inline at file bottom in `#[cfg(test)] mod tests`

### DSP conventions

- DSP functions in `sdr-dsp` are pure: no threading, no I/O, no side effects
- All DSP processors: `process(input: &[T], output: &mut [T]) -> usize`
- Threading and streaming live in `sdr-pipeline`, never in `sdr-dsp`
- Use named constants for all magic numbers (sample rates, buffer sizes, thresholds)

### Dependencies

- Shared/version-pinned dependencies go in `[workspace.dependencies]`
- All crates use `[lints] workspace = true`
- `unsafe_code` is denied workspace-wide

### Sidebar architecture

The GTK4 UI uses a VS Code-style activity-bar pattern (epic #420): a narrow icon strip on each window edge switches a `GtkStack` of panels between "activities". Left bar hosts General / Radio / Audio / Display / Scanner / Share / Satellites; right bar hosts Transcript / Bookmarks. See `docs/design/sidebar-activity-bar-redesign.md` for the full rationale.

**Key files:**

- `crates/sdr-ui/src/sidebar/activity_bar.rs` — the `ActivityBarEntry` struct, the canonical `LEFT_ACTIVITIES` / `RIGHT_ACTIVITIES` slices (single source of truth for icon + shortcut + display name + config-persistence name), `build_activity_bar` widget builder, and `SidebarSession` persistence.
- `crates/sdr-ui/src/window.rs::build_layout` — nests two `AdwOverlaySplitView`s inside a horizontal `GtkBox` alongside the two activity bars.
- `crates/sdr-ui/src/window.rs::wire_activity_bar_clicks` — the click-handler semantics: different-button swaps stack + opens panel; same-button toggles panel while keeping the icon selected.
- `crates/sdr-ui/src/window.rs::build_resize_handle` — custom `GtkGestureDrag` on an invisible 6 px strip at each panel's inner edge, because `AdwOverlaySplitView` has no built-in draggable divider in libadwaita 1.9.

**Adding a new activity:**

1. Append an `ActivityBarEntry` to `LEFT_ACTIVITIES` or `RIGHT_ACTIVITIES` in `activity_bar.rs`. Keep existing entries' order + names stable — they're config keys. Shortcut accelerator uses `<Ctrl>N` / `<Ctrl><Shift>N` per side.
2. In `window.rs::build_layout`, add a stack child under the new name: `left_stack.add_named(&panel_widget, Some("your-name"))` (or `right_stack`).
3. Keyboard shortcut registration and help-dialog rows auto-derive from the slice — no wiring changes needed.
4. If the activity hosts a new panel with its own DSP controls, add a `connect_your_panel(panels, state)` call in `connect_sidebar_panels` matching the pattern of existing panels.

**Panel layout convention:**

Every activity panel is an `AdwPreferencesPage` of flat `AdwPreferencesGroup`s with `title` + short plain-English `description`. We deliberately avoid `AdwExpanderRow` — the expander-row inset stacked on top of the group's own inset read cluttered (verified across General / Radio / Audio / Display / Scanner). "Expanded by default, collapsible" became "always visible, scrollable" and the app looks cleaner for it.

**Session persistence:**

Three config keys per side (six total) live in `activity_bar.rs`: `ui_sidebar_{left,right}_{selected,open,width_px}`. Load at launch via `load_session(config)`, apply before wiring handlers (seed-then-wire prevents the initial `set_active` / `set_show_sidebar` calls from writing back). On change, persist via `save_*` helpers. Pixel widths are converted to `AdwOverlaySplitView`'s `[0, 1]` fraction via `apply_sidebar_width`'s one-shot `notify::width` handler once the split view has its first real allocation.

**macOS counterpart:** the same activity-bar pattern is implemented in SwiftUI on the Mac side, sharing the same six `ui_sidebar_*` config keys via the engine's config FFI (ABI 0.21+). Contributor docs for the SwiftUI version live in `apps/macos/README.md` → "Sidebar architecture (macOS)".

### Satellite reception

NOAA APT (epic #468), Meteor-M LRPT (epic #469), and ISS SSTV (epic #472) are all shipped end-to-end.

**Key files:**

- `crates/sdr-sat/src/lib.rs` — `KNOWN_SATELLITES` is the canonical catalog (NOAA / Meteor-M / ISS) with downlink frequency, demod mode, and channel bandwidth per entry. New satellites get added here, not at call sites.
- `crates/sdr-sat/src/passes.rs` — `upcoming_passes(...)` enumerates SGP4-propagated passes for a `GroundStation`. Pure function over time + TLEs; no I/O.
- `crates/sdr-sat/src/tle_cache.rs` — Celestrak `gp.php?CATNR=…` per-NORAD fetcher with 24 h cache under `~/.cache/sdr-rs/tle/`. Background `gio::spawn_blocking`; never call from the GTK main loop.
- `crates/sdr-dsp/src/apt.rs` — APT decoder (FM demod → 11025 Hz resample → 2400 Hz AM envelope → sync detect → scan-line assembly). Pure DSP, no GTK awareness.
- `crates/sdr-radio/src/apt_image.rs` + `apt_telemetry.rs` — 2D image buffer + telemetry-wedge decode for AVHRR channel ID and brightness calibration; consumed by the live viewer.
- `crates/sdr-ui/src/sidebar/satellites_panel.rs` — Satellites activity panel: ground-station entry, TLE refresh, upcoming passes list, auto-record toggle.
- `crates/sdr-ui/src/sidebar/satellites_recorder.rs` — Auto-record state machine (Idle → BeforePass → Recording → Finalizing). **Pure** — `tick()` returns `Vec<Action>`, the wiring layer in `window.rs::connect_satellites_panel` interprets each. Keep state-machine logic in this module so it stays unit-testable without a GTK harness.
- `crates/sdr-ui/src/apt_viewer.rs` — Live image viewer window with Pause/Resume + Export PNG header buttons.
- `crates/sdr-radio/src/lrpt_decoder.rs` — LRPT counterpart of `apt_decode_tap`: bridges the post-VFO IQ buffer to `LrptDemod` (QPSK) → `LrptPipeline` (Viterbi → ASM sync → derand → RS → demux → JPEG) → `LrptImage` shared assembler. Per-APID line watermark prevents double-push.
- `crates/sdr-radio/src/demod/lrpt.rs` — `LrptDemodulator`: silent-passthrough demod that pins `RadioModule`'s IF rate to 144 ksps (`sdr_dsp::lrpt::SAMPLE_RATE_HZ`) so the controller's `lrpt_decode_tap` reads `radio_input` at the rate the QPSK demod expects.
- `crates/sdr-ui/src/lrpt_viewer.rs` — Multi-channel live LRPT viewer with per-APID Cairo surfaces, dynamic channel dropdown, Pause/Resume + Export PNG header buttons. Polls the shared `LrptImage` at 4 Hz; PNG export uses `gio::spawn_blocking` so encoding doesn't freeze the GTK main loop.
- `crates/sdr-radio/src/sstv_image.rs` — Shared SSTV image handle (`Arc<Mutex<Inner>>`). DSP tap writes via `write_line`; UI reads via `snapshot()`; `take_completed()` drains the finished frame and resets for the next VIS detection. `SstvImage::clear()` convenience wrapper delegates to the handle.
- `crates/sdr-ui/src/sstv_viewer.rs` — Live SSTV viewer. `SstvImageRenderer` owns the Cairo ARGB32 surface; `SstvImageView` is the GTK widget (cloneable via Rc). `open_sstv_viewer_if_needed` opens the window and wires `UiToDsp::SetSstvImage`. `write_sstv_rgb_png` is the Cairo-based PNG encoder used by both manual Export and the auto-record LOS save. Wired via `connect_sstv_action` (`Ctrl+Shift+V`).

**User-facing walkthroughs:** `docs/guides/apt-reception.md`, `docs/guides/lrpt-reception.md`, `docs/guides/sstv-reception.md`. The two onboarding entry points are `docs/guides/getting-started.md` (first-FM-station tour) and `docs/guides/sdr-concepts.md` (IQ / sample rate / decimation / FFT / bandwidth, against this app's UI).

**Output paths** (all under `~/sdr-recordings/`):
- APT: `apt-{satellite-slug}-{local-timestamp}.png` (single PNG per pass).
- LRPT: `lrpt-{satellite-slug}-{local-timestamp}/apid{N}.png` (per-pass directory holding one PNG per AVHRR APID — LRPT is multispectral). Audio recording is suppressed for LRPT regardless of the user toggle: the demod is silent and ~170 MB of stereo silence per pass would be wasteful.
- SSTV: `sstv-{satellite-slug}-{local-timestamp}/img{N}.png` (per-pass directory, one PNG per completed image frame — ARISS events typically send ~12 per pass). Audio recording is NOT suppressed for SSTV: ISS SSTV is audible NFM and the audio recording captures the raw signal.

`png_path_for`, `lrpt_dir_for`, and `sstv_dir_for` in `satellites_recorder.rs` are the single sources of truth for those paths; the recorder's `PassOutput` enum dispatches APT to `Action::SavePng`, LRPT to `Action::SaveLrptPass`, and SSTV to `Action::SaveSstvPass`.

## C++ Reference

Original SDR++ source is in `original/SDRPlusPlus/` (gitignored).
Original librtlsdr source is in `original/librtlsdr/` (gitignored).
See `docs/PROJECT.md` for the complete C++ source map.
