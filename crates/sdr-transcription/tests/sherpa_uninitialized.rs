//! Integration test verifying that `SherpaBackend::start` returns a clear
//! `BackendError::Init` when `init_sherpa_host` was never called.
//!
//! This MUST be an integration test (separate test binary, separate process)
//! because it depends on the process-wide `OnceLock<SHERPA_HOST>` being empty.
//! Unit tests share a process and can't reliably guarantee that.

#![cfg(feature = "sherpa")]

use sdr_transcription::backends::sherpa::SherpaBackend;
use sdr_transcription::{
    BackendConfig, BackendError, ModelChoice, SherpaModel, TranscriptionBackend,
};

#[test]
#[allow(clippy::panic)]
fn sherpa_backend_start_returns_init_error_when_host_not_initialized() {
    // The integration test process never calls init_sherpa_host, so the
    // global SHERPA_HOST OnceLock is empty and start() must return
    // BackendError::Init with a clear "not initialized" message.
    let mut backend = SherpaBackend::new();
    let config = BackendConfig {
        model: ModelChoice::Sherpa(SherpaModel::StreamingZipformerEn),
        silence_threshold: 0.007,
        noise_gate_ratio: 3.0,
        vad_threshold: sdr_transcription::VAD_THRESHOLD_DEFAULT,
        segmentation_mode: sdr_transcription::SegmentationMode::Vad,
        auto_break_min_open_ms: sdr_transcription::AUTO_BREAK_MIN_OPEN_MS_DEFAULT,
        auto_break_tail_ms: sdr_transcription::AUTO_BREAK_TAIL_MS_DEFAULT,
        auto_break_min_segment_ms: sdr_transcription::AUTO_BREAK_MIN_SEGMENT_MS_DEFAULT,
        audio_enhancement: sdr_transcription::denoise::AudioEnhancement::default(),
    };
    let result = backend.start(config);
    match result {
        Err(BackendError::Init(msg)) => {
            assert!(
                msg.contains("not initialized") || msg.contains("failed to initialize"),
                "expected init error mentioning initialization, got: {msg}"
            );
        }
        Err(e) => panic!("expected Init error, got: {e:?}"),
        Ok(_) => panic!("expected Init error because init_sherpa_host was never called, got Ok"),
    }
}
