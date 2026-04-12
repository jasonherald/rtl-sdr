//! Headless SDR engine — shared by every frontend.
//!
//! `sdr-core` owns the DSP controller thread, the source/sink instantiation
//! logic, the message enums (`UiToDsp` / `DspToUi`), the shared FFT buffer
//! primitive, and the WAV recording helpers. It exposes a single facade —
//! [`Engine`] — that both the GTK4 frontend (`sdr-ui`) and the planned
//! macOS `SwiftUI` frontend (via `sdr-ffi`, M2) consume.
//!
//! Crate layout:
//!
//! ```text
//! sdr-core/
//! ├── engine.rs         — public Engine facade (commands, events, FFT pull)
//! ├── controller.rs     — DSP controller thread (moved from sdr-ui)
//! ├── messages.rs       — UiToDsp / DspToUi enums (moved from sdr-ui)
//! ├── fft_buffer.rs     — SharedFftBuffer (extracted from controller)
//! └── wav_writer.rs     — WAV file writer (moved from sdr-ui)
//! ```
//!
//! Spec: `docs/superpowers/specs/2026-04-12-sdr-core-extraction-design.md`.
//!
//! ## What this crate is **not**
//!
//! - It is not a UI library — there is no GTK, no `SwiftUI`, no rendering.
//! - It is not an FFI surface — that lives in the future `sdr-ffi` crate
//!   (M2). `sdr-core` is plain Rust; the FFI crate wraps it for C/Swift
//!   consumers.
//! - It does not own a tokio runtime. The DSP thread is a raw OS thread;
//!   commands cross via `mpsc::Sender<UiToDsp>`. Async consumers (the
//!   eventual `SwiftUI` app) wrap their side; the engine stays sync.

pub mod controller;
pub mod engine;
pub mod fft_buffer;
pub mod messages;
pub mod wav_writer;

pub use engine::{Engine, EngineError};
pub use fft_buffer::SharedFftBuffer;
pub use messages::{DspToUi, SourceType, UiToDsp};
pub use wav_writer::WavWriter;
