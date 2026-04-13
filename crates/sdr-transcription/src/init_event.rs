//! Sherpa-onnx initialization progress events.
//!
//! Emitted by `init_sherpa_host` through an `mpsc::Receiver<InitEvent>`
//! so callers (currently `src/main.rs`) can render UI feedback while
//! the background worker downloads + extracts + creates the recognizer.
//!
//! The heap-corruption workaround from PR #249 means `main()` still has
//! to block on this channel until the worker emits Ready or Failed,
//! BEFORE proceeding to `sdr_ui::run()`. The events let `main()` update
//! a splash window during the wait so the user knows what's happening.

/// Progress events from the sherpa-onnx host worker thread during
/// initialization. The worker emits these in order; the final event
/// is always either `Ready` or `Failed`.
#[derive(Debug, Clone)]
pub enum InitEvent {
    /// The sherpa model bundle is missing locally; download is starting.
    DownloadStart,
    /// Download progress (0..=100). Only fired during the download phase.
    DownloadProgress { pct: u8 },
    /// Download complete; extracting the .tar.bz2 archive.
    Extracting,
    /// Extraction complete; constructing the `OnlineRecognizer`.
    /// This is the longest step on the cached path (~1-2 seconds).
    CreatingRecognizer,
    /// The host is fully initialized and ready to accept sessions.
    /// `SHERPA_HOST` has been populated with Ok(host) by the worker.
    Ready,
    /// Initialization failed permanently. `SHERPA_HOST` has been
    /// populated with Err(error). The error message is intended for
    /// display to the user (e.g. via a status label or toast).
    Failed { message: String },
}
