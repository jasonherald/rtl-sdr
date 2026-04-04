# SDR-RS — Project Guide

Rust port of SDR++ — software-defined radio application with GTK4 UI.

## Architecture

13-crate workspace with clear dependency boundaries:

```text
sdr-types           → Foundation types, errors, constants (no internal deps)
sdr-dsp             → Pure DSP: math, filters, FFT, demod, resampling (depends on: types)
sdr-config          → JSON configuration persistence (depends on: types)
sdr-pipeline        → Threading, streaming, signal path (depends on: types, dsp, config)
sdr-rtlsdr          → Pure Rust librtlsdr port — USB, tuner drivers (no internal deps)
sdr-source-rtlsdr   → RTL-SDR source module (depends on: types, pipeline, rtlsdr, config)
sdr-source-network  → TCP/UDP IQ source (depends on: types, pipeline, config)
sdr-source-file     → WAV file playback source (depends on: types, pipeline, config)
sdr-sink-audio      → PipeWire/CoreAudio output (depends on: types, pipeline, config)
sdr-sink-network    → TCP/UDP audio output (depends on: types, pipeline, config)
sdr-radio           → Radio decoder, demod, IF/AF chains (depends on: types, dsp, pipeline)
sdr-ui              → GTK4/libadwaita UI (depends on: all above)
sdr (binary)        → Entry point (depends on: ui, pipeline, config, sources, sinks)
```

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
