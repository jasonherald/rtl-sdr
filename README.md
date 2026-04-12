# SDR-RS

Software-defined radio application in Rust -- a port of [SDR++](https://github.com/AlexandreRouma/SDRPlusPlus) with a GTK4/libadwaita UI.

![SDR-RS](screenshots/sdr-rs.png)

## Features

### Radio

- 8 demodulation modes: WFM, NFM, AM, DSB, USB, LSB, CW, RAW
- RTL-SDR hardware support (pure Rust USB driver, no C library needed)
- TCP/UDP network IQ source and sink
- WAV file IQ playback with looping
- Audio notch filter (biquad IIR, 20-20,000 Hz)
- Bookmark tuning profiles with full state capture/restore

### Display

- Cairo-rendered FFT spectrum plot and scrolling waterfall
- Frequency axis with smart Hz/kHz/MHz/GHz labels
- Spectrum zoom (scroll to zoom, clamped to FFT bandwidth)
- VFO overlay with drag-to-tune and bandwidth handles
- Configurable FFT size, window function, colormap, and dB range

### Recording

- Audio WAV recording (48 kHz stereo, IEEE float 32-bit)
- IQ WAV recording (raw pre-decimation samples)
- Waterfall PNG export with desktop notification and click-to-open

### Transcription

- Live speech-to-text via Whisper — 5 model sizes from tiny (75 MB) to large-v3 (3.1 GB)
- Optional GPU acceleration: CUDA (NVIDIA), ROCm/HIP (AMD), Vulkan, Metal (macOS)
- Slide-out transcript panel with timestamped log and model selector
- FFT-based spectral noise gate preprocessor for cleaner recognition
- Auto-downloads selected model on first use
- Volume-independent audio tap (transcription unaffected by volume knob)

### Integration

- [RadioReference.com](https://www.radioreference.com) frequency database browser — search by ZIP code, browse by category/agency, import as bookmarks (requires RadioReference premium account)
- Secure credential storage via OS keyring (GNOME Keyring / macOS Keychain)
- Preferences window with directory settings and account management
- PipeWire audio output (Linux), CoreAudio planned (macOS)
- Desktop notifications (GNotification) with click-to-open

### Under the Hood

- 15-crate workspace with clear dependency boundaries
- Pure DSP functions (no threading, no I/O, no side effects)
- Zero per-frame heap allocations on hot paths
- Lock-based SPSC audio ring buffer between DSP and audio threads
- `mallopt(M_ARENA_MAX)` + periodic `malloc_trim` for glibc arena management
- JSON-based configuration with auto-save

## Build

### Dependencies

- **Rust** 1.85+ (2024 edition)
- **GTK 4.10+** and **libadwaita 1.5+**
- **PipeWire** development libraries (Linux audio)
- **libusb** (for RTL-SDR USB access)
- **libdbus** (for secure credential storage)
- **cmake** + **C++ compiler** (build-time, for whisper.cpp)
- **libclang** (build-time, for bindgen if needed)

#### Arch Linux

```bash
sudo pacman -S gtk4 libadwaita pipewire libusb dbus clang cmake
```

#### Ubuntu / Debian

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev libpipewire-0.3-dev \
  libusb-1.0-0-dev libdbus-1-dev libclang-dev cmake g++
```

#### macOS

```bash
brew install gtk4 libadwaita libusb llvm cmake
```

### Compile

```bash
cargo build --release
```

### Install

```bash
make install
```

Installs the binary, desktop entry, and icon for app launcher integration.

### Transcription backend (optional)

```bash
make install CARGO_FLAGS="--release --features whisper-cuda"      # NVIDIA
make install CARGO_FLAGS="--release --features whisper-hipblas"   # AMD ROCm
make install CARGO_FLAGS="--release --features whisper-vulkan"    # Cross-vendor
make install CARGO_FLAGS="--release"                               # Whisper CPU (default)
make install CARGO_FLAGS="--release --no-default-features --features sherpa-cpu"  # Sherpa CPU
```

Requires the corresponding GPU toolkit (CUDA toolkit, ROCm, Vulkan SDK).

### Run tests

```bash
cargo test --workspace
```

### Lint

```bash
make lint
```

Runs `cargo fmt --check`, `cargo clippy`, `cargo test`, `cargo deny`, and `cargo audit`.

## Usage

```bash
sdr-rs
```

1. Select a source (RTL-SDR device, network, or file)
2. Set center frequency using the digit selector (scroll or click digits)
3. Choose demodulation mode (WFM, NFM, AM, USB, LSB, etc.)
4. Press **Play**

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| Space | Play / Stop |
| M | Cycle demod mode |
| F9 | Toggle sidebar |
| Ctrl+, | Preferences |
| Ctrl+/ | Keyboard shortcuts |

## Architecture

15-crate workspace with clear dependency boundaries:

```text
sdr (binary)              Entry point
sdr-ui                    GTK4/libadwaita UI
sdr-radio                 Radio decoder, demod, IF/AF chains
sdr-pipeline              Threading, streaming, signal path
sdr-dsp                   Pure DSP: math, filters, FFT, demod, resampling
sdr-types                 Foundation types, errors, constants
sdr-config                JSON config persistence + OS keyring access
sdr-rtlsdr                Pure Rust RTL-SDR USB driver (5 tuner chips)
sdr-radioreference        RadioReference.com SOAP API client
sdr-transcription         Live speech-to-text via Whisper + spectral denoiser
sdr-source-rtlsdr         RTL-SDR source module
sdr-source-network        TCP/UDP IQ source
sdr-source-file           WAV file playback source
sdr-sink-audio            PipeWire/CoreAudio audio output
sdr-sink-network          TCP/UDP audio output
```

**Signal chain:** Source -> Decimation -> Channel filter -> Demodulator -> AF filter -> Audio sink

DSP functions are pure (no threading, no I/O). Threading and streaming live in `sdr-pipeline`.

## RadioReference Integration

SDR-RS can browse and import frequencies from [RadioReference.com](https://www.radioreference.com), the largest radio communications reference source in the US.

**Setup:** Open Preferences (Ctrl+,) > Accounts > enter your RadioReference credentials > Test & Save. A [premium account](https://www.radioreference.com/premium/) is required for API access.

**Usage:** Click the antenna icon in the header bar > enter a US ZIP code > browse frequencies by category and agency > check the ones you want > Import. Frequencies are saved as bookmarks with auto-mapped demod mode and bandwidth.

Your credentials are stored in your system keyring (GNOME Keyring / macOS Keychain) and are only sent to RadioReference.com.

## Responsible Use

> **Disclaimer:** This section provides general information and is **not legal advice**. Consult a qualified attorney for guidance specific to your situation and jurisdiction.

SDR-RS is a personal listening tool for unencrypted radio transmissions. Listening to public safety, amateur, and commercial radio is legal in most jurisdictions, but please use it responsibly:

**It's legal to:**
- Listen to unencrypted radio for personal interest, education, or amateur radio activities
- Take notes for your own reference
- Use the transcription feature to convert audio to text on your local machine

**It's not OK to:**
- Publish or share transcripts of intercepted public safety communications (US Communications Act §605 prohibits divulging or publishing intercepted communications)
- Aggregate or sell information overheard on the radio
- Use information obtained from radio listening to identify, track, harass, or harm individuals
- Decrypt encrypted transmissions
- Listen to cellular phone communications (illegal under ECPA)

**Privacy considerations:**

Public safety broadcasts may include personally identifiable information — names, addresses, license plates, phone numbers. SDR-RS keeps everything local:

- Transcripts live in memory only and are cleared when the app closes
- No data is uploaded anywhere
- The Whisper model runs entirely on your machine
- Audio recordings are saved locally only when you explicitly enable them

If you're using SDR-RS in a shared space, be mindful that others may see the transcript on your screen. Future versions may add automatic redaction of PII patterns and a "lock transcript" mode (see [#219](https://github.com/jasonherald/rtl-sdr/issues/219), [#220](https://github.com/jasonherald/rtl-sdr/issues/220), [#221](https://github.com/jasonherald/rtl-sdr/issues/221)).

**Know your local laws:** Scanner laws vary by jurisdiction. Some US states restrict scanner use in vehicles. Some countries prohibit listening to certain frequencies entirely. It's your responsibility to know and follow the laws where you live.

## Security

See [SECURITY.md](SECURITY.md) for vulnerability reporting and security scanning details.

## License

MIT

## Credits

- [SDR++](https://github.com/AlexandreRouma/SDRPlusPlus) by Alexandre Rouma -- the original C++ application this project ports
- [RTL-SDR](https://osmocom.org/projects/rtl-sdr/wiki) -- the original C library for RTL2832U devices
