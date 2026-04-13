# SDR-RS — Project Guide

Rust port of SDR++ — software-defined radio application with GTK4 UI.

## Architecture

18-member workspace (root binary + 17 library crates) with clear dependency boundaries:

```text
sdr-types           → Foundation types, errors, constants (no internal deps)
sdr-dsp             → Pure DSP: math, filters, FFT, demod, resampling (depends on: types)
sdr-config          → JSON configuration persistence + OS keyring (depends on: types)
sdr-pipeline        → Threading, streaming, signal path (depends on: types, dsp, config)
sdr-rtlsdr          → Rust port of librtlsdr over rusb — 5 tuner families (no internal deps)
sdr-source-rtlsdr   → RTL-SDR source module (depends on: types, pipeline, rtlsdr, config)
sdr-source-network  → TCP/UDP IQ source (depends on: types, pipeline, config)
sdr-source-file     → WAV file playback source (depends on: types, pipeline, config)
sdr-sink-audio      → PipeWire/CoreAudio output (depends on: types, pipeline, config)
sdr-sink-network    → TCP/UDP audio output (depends on: types, pipeline, config)
sdr-radio           → Radio decoder, demod, IF/AF chains (depends on: types, dsp, pipeline)
sdr-radioreference  → RadioReference.com SOAP client (depends on: types, config)
sdr-transcription   → Whisper OR Sherpa-onnx backend, Silero VAD, spectral denoiser
sdr-core            → Headless cross-platform engine facade (macOS port path)
sdr-splash          → Cross-platform splash subprocess controller (stdin wire protocol)
sdr-splash-gtk      → Linux GTK4 splash window implementation
sdr-ui              → GTK4/libadwaita UI — Linux-only (depends on: all above)
sdr (binary)        → Entry point (depends on: ui, core, splash, transcription, …)
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

**`sherpa-cuda` requires CUDA 12.x + cuDNN 9.x installed system-wide** (onnxruntime dlopens `libcudnn.so.9` / `libcublas.so.12` at startup). Linux x86_64 only. The first build downloads a ~235 MB CUDA prebuilt archive from k2-fsa releases. During the PR window the sherpa-onnx dep is pinned to the [jasonherald/sherpa-onnx](https://github.com/jasonherald/sherpa-onnx) fork on branch `feat/rust-sys-cuda-support`; swap back to a crates.io version pin once the upstream PR merges and releases.

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

## C++ Reference

Original SDR++ source is in `original/SDRPlusPlus/` (gitignored).
Original librtlsdr source is in `original/librtlsdr/` (gitignored).
See `docs/PROJECT.md` for the complete C++ source map.
