# SDR++ Rust Port - Project Document

## Overview

This project is a Rust port of [SDR++](https://github.com/AlexandreRouma/SDRPlusPlus), a C++ software-defined radio application. The port targets a reduced scope: Linux and macOS only, RTL-SDR USB hardware + TCP/UDP network I/O, and a GTK4 UI with Paper design principles. All features are built natively -- no plugin/module system.

The original C++ project includes `librtlsdr` (the USB driver library), which is also being ported to pure Rust using the `rusb` crate.

---

## Architecture

### Workspace Crates

```
crates/
  sdr-types/           # Foundation: Complex, Stereo, SampleRate, Frequency, error types, constants, enums
  sdr-dsp/             # Pure DSP: math, taps, FFT, filters, multirate, demod, convert, correction, AGC/PLL, noise
  sdr-pipeline/        # Threading & streaming: Stream<T>, Block trait, Chain, Splitter, IqFrontend, managers
  sdr-rtlsdr/          # Pure Rust port of librtlsdr (USB via rusb, tuner drivers, RTL2832 control)
  sdr-source-rtlsdr/   # RTL-SDR source module using sdr-rtlsdr
  sdr-source-network/  # TCP client + UDP receiver for IQ input
  sdr-source-file/     # WAV file IQ playback (testing/replay)
  sdr-sink-audio/      # PipeWire (Linux) + CoreAudio (macOS)
  sdr-sink-network/    # TCP/UDP audio output
  sdr-radio/           # Radio decoder: demod selection, IF/AF chains, mode switching
  sdr-config/          # JSON config persistence (serde_json)
  sdr-ui/              # GTK4 + libadwaita: waterfall, FFT plot, VFO overlay, controls
  sdr-app/             # Binary entry point
```

### Dependency Graph

```
sdr-app
  └── sdr-ui
        ├── sdr-pipeline
        │     ├── sdr-dsp
        │     │     └── sdr-types
        │     └── sdr-config
        │           └── sdr-types
        ├── sdr-radio
        │     └── sdr-dsp
        ├── sdr-source-rtlsdr
        │     ├── sdr-rtlsdr
        │     ├── sdr-pipeline
        │     └── sdr-types
        ├── sdr-source-network
        │     ├── sdr-pipeline
        │     └── sdr-types
        ├── sdr-source-file
        │     ├── sdr-pipeline
        │     └── sdr-types
        ├── sdr-sink-audio
        │     ├── sdr-pipeline
        │     └── sdr-types
        └── sdr-sink-network
              ├── sdr-pipeline
              └── sdr-types
```

---

## C++ Source Map

### Core DSP

Translating from `original/SDRPlusPlus/core/src/dsp/`.

| C++ File | Rust Target | Purpose |
|---|---|---|
| `types.h` | `sdr-types/src/lib.rs` | `Complex` (f32 re/im), `Stereo` (f32 l/r) with arithmetic ops |
| `stream.h` | `sdr-pipeline/src/stream.rs` | Double-buffer swap channel (1M sample buffers, mutex + condvar) |
| `block.h` | `sdr-pipeline/src/block.rs` | Threaded processing block base with start/stop lifecycle |
| `chain.h` | `sdr-pipeline/src/chain.rs` | Ordered processor chain with enable/disable/rewire |
| `buffer/buffer.h` | `sdr-types/src/buffer.rs` | Aligned memory allocation (replaces `volk_malloc`) |
| `buffer/packer.h` | `sdr-pipeline/src/packer.rs` | Sample frame packing for fixed-size output blocks |
| `convert/complex_to_*`, `stereo_to_mono` | `sdr-dsp/src/convert.rs` | Complex-to-real, mono-to-stereo, stereo-to-mono, L/R split |
| `filter/fir.h`, `deemphasis.h` | `sdr-dsp/src/filter.rs` | FIR filter, decimating FIR, deemphasis filter |
| `multirate/polyphase_resampler.h`, `power_decimator.h` | `sdr-dsp/src/multirate.rs` | Polyphase resampler, power decimator, rational resampler |
| `demod/quadrature.h`, `am.h`, `fm.h`, `ssb.h` | `sdr-dsp/src/demod.rs` | Quadrature, AM, FM, broadcast FM, SSB, CW demodulators |
| `channel/rx_vfo.h`, `frequency_xlator.h` | `sdr-dsp/src/channel.rs` | VFO: frequency translation + resampling + filtering |
| `math/` | `sdr-dsp/src/math.rs` | Window functions (Blackman, Nuttall, Hann), fast_atan2, sinc |
| `taps/` | `sdr-dsp/src/taps.rs` | FIR tap generation (lowpass, bandpass, highpass, windowed sinc) |
| `loop/agc.h`, `pll.h` | `sdr-dsp/src/loops.rs` | AGC, PLL, phase control loop |
| `correction/dc_blocker.h` | `sdr-dsp/src/correction.rs` | DC blocking filter |
| `noise_reduction/` | `sdr-dsp/src/noise.rs` | Noise blanker, power squelch, FM IF noise reduction |

### Signal Path

Translating from `original/SDRPlusPlus/core/src/signal_path/`.

| C++ File | Rust Target | Purpose |
|---|---|---|
| `iq_frontend.cpp/h` | `sdr-pipeline/src/iq_frontend.rs` | Decimation + DC blocking + IQ conjugate + FFT computation + VFO fan-out |
| `source.cpp/h` | `sdr-pipeline/src/source_manager.rs` | Source registration, selection, start/stop/tune lifecycle |
| `sink.cpp/h` | `sdr-pipeline/src/sink_manager.rs` | Sink provider registration, stream routing, volume control |
| `vfo_manager.cpp/h` | `sdr-pipeline/src/vfo_manager.rs` | Multi-VFO creation/deletion/parameter management |

### librtlsdr (Pure Rust Port)

Translating from `original/librtlsdr/`. Replaces the C library entirely using `rusb` for USB.

| C File | Lines | Rust Target | Purpose |
|---|---|---|---|
| `src/librtlsdr.c` | 2,052 | `sdr-rtlsdr/src/device.rs` | Core: USB control, RTL2832 demod chip, async bulk transfers |
| `src/tuner_r82xx.c` | 1,366 | `sdr-rtlsdr/src/tuner/r82xx.rs` | R820T/R828D tuner driver (most common in consumer dongles) |
| `src/tuner_e4k.c` | 1,000 | `sdr-rtlsdr/src/tuner/e4k.rs` | Elonics E4000 tuner driver |
| `src/tuner_fc2580.c` | 494 | `sdr-rtlsdr/src/tuner/fc2580.rs` | Fitipower FC2580 tuner driver |
| `src/tuner_fc0013.c` | 500 | `sdr-rtlsdr/src/tuner/fc0013.rs` | Fitipower FC0013 tuner driver |
| `src/tuner_fc0012.c` | 333 | `sdr-rtlsdr/src/tuner/fc0012.rs` | Fitipower FC0012 tuner driver |
| `include/rtl-sdr.h` | 407 | `sdr-rtlsdr/src/lib.rs` | Public API (37 functions) |

**Total**: ~5,745 lines of C. Tuner drivers are I2C register manipulation -- maps cleanly to Rust. CLI utilities (`rtl_fm`, `rtl_tcp`, etc.) are NOT ported.

### Sources

| C++ File | Rust Target | Purpose |
|---|---|---|
| `source_modules/rtl_sdr_source/src/main.cpp` | `sdr-source-rtlsdr/src/lib.rs` | Source module using `sdr-rtlsdr`, uint8-to-f32 IQ conversion |
| `source_modules/network_source/src/main.cpp` | `sdr-source-network/src/lib.rs` | TCP client + UDP receiver, int8/16/32/f32 format conversion |
| `source_modules/file_source/src/main.cpp` | `sdr-source-file/src/lib.rs` | WAV file IQ playback for testing and replay |

### Sinks

| C++ File | Rust Target | Purpose |
|---|---|---|
| `sink_modules/audio_sink/src/main.cpp` | `sdr-sink-audio/src/lib.rs` | RtAudio replaced by PipeWire (Linux) / CoreAudio (macOS) |
| `sink_modules/network_sink/src/main.cpp` | `sdr-sink-network/src/lib.rs` | TCP/UDP int16 audio output, mono/stereo modes |

### Radio Decoder

| C++ File | Rust Target | Purpose |
|---|---|---|
| `decoder_modules/radio/src/radio_module.h` | `sdr-radio/src/lib.rs` | Demod selection, IF/AF processing chains, mode switching |
| `decoder_modules/radio/src/demod/*.h` | `sdr-radio/src/demod/` | WFM, NFM, AM, USB, LSB, DSB, CW, RAW demodulators |

### UI (Complete Rewrite: ImGui to GTK4)

| C++ File | Rust Target | Purpose |
|---|---|---|
| `core/src/gui/main_window.cpp` | `sdr-ui/src/window.rs` | Main application window layout |
| `core/src/gui/widgets/waterfall.cpp` (1,426 LOC) | `sdr-ui/src/waterfall.rs` | GtkGLArea waterfall + FFT spectrum plot |
| `core/src/gui/widgets/frequency_select.cpp` | `sdr-ui/src/frequency_select.rs` | Digit-scroll frequency entry widget |
| `core/src/gui/menus/source.cpp` | `sdr-ui/src/panels/source.rs` | Source selection and configuration panel |
| `core/src/gui/menus/sink.cpp` | `sdr-ui/src/panels/sink.rs` | Sink selection and configuration panel |
| `core/src/gui/menus/display.cpp` | `sdr-ui/src/panels/display.rs` | FFT/waterfall display settings |

### Config

| C++ File | Rust Target | Purpose |
|---|---|---|
| `core/src/config.cpp/h` | `sdr-config/src/lib.rs` | JSON load/save/auto-save with `serde_json` |

---

## Key Architecture Decisions

### 1. No Plugin System

All sources, sinks, and decoders are compiled natively into the binary. Extensibility is via Rust traits (`Source`, `Sink`, `Demodulator`), not dynamic `.so` loading.

### 2. Pure Rust FFT (`rustfft`)

Default FFT engine is `rustfft` (pure Rust, no system dependencies). Optional `fftw` feature gate for users who want maximum performance. An `FftEngine` trait abstracts the implementation.

### 3. GtkGLArea Waterfall

The waterfall display uses `GtkGLArea` with OpenGL texture scrolling, matching the C++ approach. FFT data arrives via `glib::Sender` channel from DSP threads. VFO overlays are drawn as semi-transparent quads with GTK4 gesture controllers for click-to-tune and drag-to-resize.

### 4. Double-Buffer Swap Streaming

Faithful port of the C++ `dsp::stream<T>` -- two pre-allocated 1M-sample buffers swapped atomically under mutex + condvar. Zero allocation in the hot path, backpressure when consumer is slow.

### 5. VOLK Replacement

C++ uses VOLK (SIMD library) for vectorized math. Rust approach: write scalar loops and rely on compiler auto-vectorization with `-C target-cpu=native`. Hand-optimize only measured bottlenecks (FIR dot products, frequency rotator) using `std::simd` or target feature intrinsics.

### 6. Native Platform Audio

- **Linux**: `pipewire-rs` (native PipeWire bindings)
- **macOS**: `coreaudio-rs`
- Selected at compile time via `#[cfg(target_os)]`

### 7. Custom Complex Type

`sdr-types::Complex` with `f32 re, im` fields -- matches SDR++ memory layout exactly. Includes SDR-specific methods (`fast_phase`, `fast_amplitude`, `conj`). Avoids pulling in the `num` ecosystem.

### 8. Pure Rust librtlsdr Port

The `sdr-rtlsdr` crate replaces C `librtlsdr` entirely. Uses `rusb` for USB communication. ~5,745 LOC of C ported to safe Rust with a trait-based tuner abstraction. Eliminates the C dependency chain.

---

## Signal Processing Pipeline

```
┌─────────────────────────────────────────────────┐
│ SOURCE (RTL-SDR USB, Network TCP/UDP, WAV File) │
│ Output: Stream<Complex>                         │
└──────────────────┬──────────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────────┐
│ IQ FRONTEND                                     │
│ ├─ Input Buffering                              │
│ ├─ Power Decimation Chain                       │
│ ├─ IQ Conjugate (inversion correction)          │
│ └─ DC Blocker                                   │
└──────────────────┬──────────────────────────────┘
                   │
          ┌────────┴────────┐
          │                 │
    [FFT Path]        [VFO Path]
          │                 │
          ▼                 ▼
   ┌──────────┐    ┌──────────────────┐
   │ Splitter │    │ Splitter         │
   └────┬─────┘    └────────┬─────────┘
        │                   │
        ▼                   ▼
 ┌──────────────┐   ┌──────────────────┐
 │ FFT Engine   │   │ RX VFO (per VFO) │
 │ (rustfft)    │   │ ├─ Freq Xlation  │
 │ + Windowing  │   │ ├─ FIR Filter    │
 └──────┬───────┘   │ └─ Resampler     │
        │           └────────┬─────────┘
        ▼                    │
 [Waterfall/FFT UI]          ▼
                    ┌──────────────────┐
                    │ RADIO DECODER    │
                    │ IF: NB → SQ → NR│
                    │ DEMOD: FM/AM/SSB │
                    │ AF: Deemph → Res │
                    └────────┬─────────┘
                             │
                    ┌────────┴────────┐
                    │                 │
                    ▼                 ▼
             ┌──────────┐    ┌──────────────┐
             │ Audio    │    │ Network Sink │
             │ Sink     │    │ (TCP/UDP)    │
             │ (PipeWire│    └──────────────┘
             │/CoreAudio│
             └──────────┘
```

---

## GTK4 UI Layout

```
+------------------------------------------------------------+
| AdwHeaderBar (title, play/stop, frequency display)         |
+----------+-------------------------------------------------+
| Sidebar  | Main Content Area                               |
| (AdwFlap)|                                                 |
|          | +---------------------------------------------+ |
| Source   | | FFT Spectrum Plot (GtkGLArea)               | |
| panel    | +---------------------------------------------+ |
|          | | Frequency Scale + VFO markers               | |
| Sink     | +---------------------------------------------+ |
| panel    | | Waterfall Display (GtkGLArea)               | |
|          | +---------------------------------------------+ |
| Demod    | | Status bar (SNR, sample rate, buffer)       | |
| panel    | +---------------------------------------------+ |
|          |                                                 |
| Display  |                                                 |
| panel    |                                                 |
+----------+-------------------------------------------------+
```

### Widget Mapping

| SDR++ (ImGui) | Rust (GTK4/libadwaita) |
|---|---|
| Waterfall texture (OpenGL) | `GtkGLArea` with custom GL rendering |
| FFT plot (ImGui drawlist) | `GtkGLArea` shared with waterfall |
| VFO overlay rectangles | Custom drawing on GL area overlay |
| Frequency selector digits | Custom `GtkWidget` subclass with scroll-per-digit |
| Side menu accordion | `AdwPreferencesGroup` in `AdwFlap` sidebar |
| Source/sink combo | `AdwComboRow` |
| Gain slider | `GtkScale` or `AdwSpinRow` |
| Play/Stop button | `GtkToggleButton` with media icons |

### Threading Model

- **DSP threads**: Worker threads per processing block
- **DSP → UI**: `glib::Sender<Message>` for FFT data, SNR, status
- **UI → DSP**: `std::sync::mpsc` or `crossbeam::channel` for commands (tune, change mode, etc.)
- GTK4 widgets are only accessed from the main thread

---

## External Dependencies

| Purpose | Crate | Notes |
|---|---|---|
| FFT | `rustfft` | Pure Rust; optional `fftw` feature gate |
| GTK4 UI | `gtk4`, `libadwaita` | UI framework |
| OpenGL | `glow` | Safe OpenGL wrapper for GtkGLArea |
| JSON config | `serde`, `serde_json` | Config persistence |
| USB | `rusb` | Pure Rust libusb wrapper (for sdr-rtlsdr) |
| Audio (Linux) | `pipewire-rs` | Native PipeWire |
| Audio (macOS) | `coreaudio-rs` | CoreAudio |
| WAV files | `hound` | WAV reading for file source |
| Networking | `std::net` | TCP/UDP (no async needed) |
| Error handling | `thiserror` (libs), `anyhow` (bin) | Per project convention |
| Logging | `tracing`, `tracing-subscriber` | Structured logging |
| License audit | `cargo-deny` | Advisory + license checks |

---

## Phased Implementation Plan

### Phase 1: Foundation

**Branch**: `feature/phase-1-foundation`

**Scope**: `sdr-types` + `sdr-dsp` + `sdr-config` + workspace scaffolding

**Deliverables**:
- Workspace `Cargo.toml` with centralized dependencies
- `CLAUDE.md`, `Makefile`, `deny.toml`, `.coderabbit.yaml`
- GitHub Actions CI (fmt, clippy, test, deny)
- `sdr-types`: Complex, Stereo, newtypes, error types, constants, enums (DemodMode, SampleFormat, Protocol)
- `sdr-dsp`: All DSP modules (math, taps, fft, filter, multirate, convert, correction, loops, demod, noise, channel)
- `sdr-config`: ConfigManager with JSON load/save/auto-save
- Comprehensive unit tests for every DSP function

**Milestone**: `cargo test` passes for all DSP functions with numerical accuracy within 1e-5 of C++ reference values.

### Phase 2: Pipeline

**Branch**: `feature/phase-2-pipeline`

**Scope**: `sdr-pipeline` -- threading, streaming, signal routing

**Deliverables**:
- `Stream<T>` double-buffer swap channel with writer/reader stop semantics
- `Block` trait hierarchy (Block, Processor, Source, Sink wrappers)
- `Chain<T>` with enable/disable/rewire
- `Splitter<T>` for fan-out
- `SourceManager`, `SinkManager`, `VfoManager`
- `IqFrontend`: decimation + DC blocker + IQ conjugate + FFT + splitter + VFO management
- Integration test: synthetic sine → IqFrontend → VFO → verify frequency-translated output

**Milestone**: End-to-end test with synthetic signal flowing through the full pipeline.

### Phase 3a: RTL-SDR Driver

**Branch**: `feature/phase-3a-rtlsdr-driver`

**Scope**: `sdr-rtlsdr` -- pure Rust port of librtlsdr

**Deliverables**:
- `rusb` integration for USB bulk transfers and control transfers
- RTL2832 demodulator chip control (registers, sample rate, frequency, AGC, FIR coefficients)
- `Tuner` trait with register read/write abstraction
- R820T/R828D tuner driver (highest priority -- most common in consumer dongles)
- E4000, FC0012, FC0013, FC2580 tuner drivers
- Async bulk USB transfer engine
- Device enumeration, open/close, bias-T, direct sampling, offset tuning, PPM correction
- Public API covering librtlsdr's 37 functions
- Integration test with real hardware

**Milestone**: Device opens, tunes to a frequency, and streams raw IQ uint8 data.

### Phase 3b: I/O Modules

**Branch**: `feature/phase-3b-io`

**Scope**: Source and sink modules

**Deliverables**:
- `sdr-source-rtlsdr`: Source module wrapping `sdr-rtlsdr` with uint8→f32 conversion
- `sdr-source-network`: TCP client and UDP receiver with int8/16/32/f32 format conversion
- `sdr-source-file`: WAV IQ playback using `hound` crate
- `sdr-sink-audio`: PipeWire (Linux) / CoreAudio (macOS) with device enumeration and sample rate selection
- `sdr-sink-network`: TCP/UDP int16 audio output, mono/stereo modes
- Network loopback integration test

**Milestone**: CLI test program: RTL-SDR → FM demod → audio output.

### Phase 4: Radio Decoder

**Branch**: `feature/phase-4-radio`

**Scope**: `sdr-radio` -- demodulation and audio processing

**Deliverables**:
- `Demodulator` trait
- All 8 demodulator implementations: WFM, NFM, AM, USB, LSB, DSB, CW, RAW
- IF chain: noise blanker → squelch → FM IF noise reduction
- AF chain: deemphasis → audio resampler
- Mode switching (stop demod, reconfigure VFO bandwidth, start new demod)

**Milestone**: Full signal processing from RTL-SDR to audio, selectable by demod mode.

### Phase 5: GTK4 UI

**Branch**: `feature/phase-5-ui`

**Scope**: `sdr-ui` + `sdr-app`

**Sub-phases**:
1. **5a**: Window skeleton -- AdwApplication, header bar, sidebar flap, empty main area
2. **5b**: Waterfall + FFT -- GtkGLArea, colormap, FFT line plot, waterfall texture scrolling, frequency scale
3. **5c**: VFO overlay -- click-to-tune, drag VFO, bandwidth handles, frequency display
4. **5d**: Frequency selector -- custom digit-scroll widget
5. **5e**: Source/sink panels -- device selection, sample rate, gain controls in sidebar
6. **5f**: Demod panel -- mode selector, bandwidth, squelch, deemphasis, AGC settings
7. **5g**: Transport + status -- play/stop controls, SNR meter, status bar

**Milestone**: Fully functional GUI SDR application.

### Phase 6: Polish

**Branch**: `feature/phase-6-polish`

**Deliverables**:
- Linux `.desktop` file and application icon
- macOS `.app` bundle via `cargo-bundle`
- Makefile `install` target
- Performance profiling and hot-path optimization
- README and architecture documentation
- CI/CD: build + test on Linux and macOS

**Milestone**: Release-ready application.
