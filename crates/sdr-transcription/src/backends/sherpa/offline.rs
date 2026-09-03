//! Offline session loop for Moonshine (and future offline recognizers).
//!
//! Runs on the sherpa-host worker thread. Uses Silero VAD to detect
//! utterance boundaries in the incoming audio stream, then batch-decodes
//! each completed segment through the `OfflineRecognizer`.
//!
//! Unlike the streaming loop, this path emits NO `TranscriptionEvent::Partial`
//! events. Moonshine is offline — partials aren't meaningful. The UI hides
//! the Live/Final display-mode toggle when a Moonshine model is selected
//! (see `SherpaModel::supports_partials`).
//!
//! Split per the file-size pass (issue #820): recognizer
//! construction lives in [`build`], the shared decode service in
//! [`decode`], and the two segmentation-mode session loops in
//! [`vad`] and [`auto_break`]. This root keeps the entry-point
//! dispatcher and the re-exports `host.rs` / `mod.rs` consume.

mod auto_break;
mod build;
mod decode;
mod vad;

pub(super) use build::{
    build_canary_recognizer_config, build_cohere_recognizer_config,
    build_moonshine_recognizer_config, build_nemo_transducer_recognizer_config,
};

use super::host::SessionParams;
use sherpa_onnx::OfflineRecognizer;

/// One offline transcription session. Dispatches to the VAD or Auto Break
/// implementation based on `params.segmentation_mode`.
///
/// Runs on the sherpa-host worker thread. The session spawns a second
/// "session I/O" thread that owns the audio channel, the state machine,
/// and (for VAD mode) a freshly-constructed Silero. The host thread then
/// drains a decode-request channel and runs `OfflineRecognizer::decode`
/// on each segment the I/O thread forwards. This decouples inference
/// latency from audio intake so a slow decode never backpressures the
/// DSP → transcription channel (issue #275).
pub(super) fn run_session(recognizer: &OfflineRecognizer, params: SessionParams) {
    match params.segmentation_mode {
        crate::backend::SegmentationMode::Vad => {
            vad::run_session_vad(recognizer, params);
        }
        crate::backend::SegmentationMode::AutoBreak => {
            auto_break::run_session_auto_break(recognizer, params);
        }
    }
}

// Re-imported so `auto_break_tests`' `use super::*` glob keeps
// resolving the machine surface it exercises — the test file stays
// declared here (child files untouched, per the split convention;
// PR #880's `server/tests/mod.rs` pattern).
#[cfg(test)]
use crate::backend::{TranscriptionEvent, TranscriptionInput};
#[cfg(test)]
use crate::denoise;
#[cfg(test)]
use auto_break::*;
#[cfg(test)]
use decode::*;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use std::sync::mpsc;

#[cfg(test)]
mod auto_break_tests;
