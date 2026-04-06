# SDR-RS

Software-defined radio application in Rust -- a port of [SDR++](https://github.com/AlexandreRouma/SDRPlusPlus) with a GTK4/libadwaita UI.

<!-- Screenshot: add a screenshot here once the UI is stable -->

## Features

- 8 demodulation modes: NFM, WFM, AM, DSB, USB, LSB, CW, RAW
- RTL-SDR hardware support (pure Rust USB driver, no C library needed)
- TCP/UDP network IQ source and sink
- WAV file IQ playback with looping
- PipeWire audio output (Linux), CoreAudio planned (macOS)
- GTK4/libadwaita UI with waterfall, FFT, and spectrum displays
- Configurable DSP pipeline: decimation, resampling, filtering
- JSON-based configuration persistence

## Build

### Dependencies

- **Rust** 1.85+ (2024 edition)
- **GTK 4.10+** and **libadwaita 1.5+**
- **PipeWire** development libraries (Linux audio)
- **libusb** (for RTL-SDR USB access)
- **libclang** (build-time, for bindgen if needed)

#### Arch Linux

```bash
sudo pacman -S gtk4 libadwaita pipewire libusb clang
```

#### Ubuntu / Debian

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev libpipewire-0.3-dev libusb-1.0-0-dev libclang-dev
```

#### macOS

```bash
brew install gtk4 libadwaita libusb llvm
```

### Compile

```bash
cargo build --release
```

### Run tests

```bash
cargo test --workspace
```

### Lint

```bash
cargo clippy --all-targets --workspace -- -D warnings
cargo fmt --all -- --check
```

## Usage

```bash
cargo run --release
```

1. Select a source (RTL-SDR device, network, or file)
2. Set center frequency
3. Choose demodulation mode (NFM, WFM, AM, USB, LSB, etc.)
4. Press **Play**

## Architecture

13-crate workspace with clear dependency boundaries:

```
sdr (binary)            Entry point
sdr-ui                  GTK4/libadwaita UI
sdr-radio               Radio decoder, demod, IF/AF chains
sdr-pipeline            Threading, streaming, signal path
sdr-dsp                 Pure DSP: math, filters, FFT, demod, resampling
sdr-types               Foundation types, errors, constants
sdr-config              JSON configuration persistence
sdr-rtlsdr              Pure Rust RTL-SDR USB driver
sdr-source-rtlsdr       RTL-SDR source module
sdr-source-network      TCP/UDP IQ source
sdr-source-file         WAV file playback source
sdr-sink-audio          PipeWire/CoreAudio audio output
sdr-sink-network        TCP/UDP audio output
```

**Signal chain:** Source -> Decimation -> Channel filter -> Demodulator -> AF filter -> Audio sink

DSP functions are pure (no threading, no I/O). Threading and streaming live in `sdr-pipeline`.

## License

MIT

## Credits

- [SDR++](https://github.com/AlexandreRouma/SDRPlusPlus) by Alexandre Rouma -- the original C++ application this project ports
- [RTL-SDR](https://osmocom.org/projects/rtl-sdr/wiki) -- the original C library for RTL2832U devices
