//! Offline recognizer construction — one `OfflineRecognizerConfig`
//! builder per supported catalog model family (Moonshine v1, `NeMo`
//! Parakeet-TDT, NVIDIA Canary, Cohere Transcribe), plus the
//! sherpa-onnx knobs they share. Split out of `offline.rs` per the
//! file-size pass (issue #820) — construction/model selection on one
//! side, the session/decode loops on the other.

use sherpa_onnx::{
    OfflineCanaryModelConfig, OfflineCohereTranscribeModelConfig, OfflineModelConfig,
    OfflineMoonshineModelConfig, OfflineRecognizerConfig, OfflineTransducerModelConfig,
};

use crate::sherpa_model::{self, ModelFilePaths, SherpaModel};

/// ONNX Runtime threads per recognizer. Sherpa is fast enough on CPU
/// that one thread is sufficient and avoids competing with the audio
/// pipeline.
const SHERPA_NUM_THREADS: i32 = 1;

/// sherpa-onnx `model_type` field value that selects `NeMo`'s Token-and-Duration
/// Transducer decode loop. Without this exact string, sherpa-onnx falls back
/// to the generic transducer decode path which doesn't understand TDT's
/// joiner output shape, and `OfflineRecognizer::create` returns `None` at
/// runtime — silent failure mode.
///
/// Mirrors the upstream `rust-api-examples/examples/nemo_parakeet.rs` example.
const NEMO_TRANSDUCER_MODEL_TYPE: &str = "nemo_transducer";

/// Build the `OfflineRecognizerConfig` for a Moonshine v1 model.
///
/// k2-fsa's int8 Moonshine releases use the v1 layout with five files:
/// preprocessor (not quantized), encoder, uncached decoder, cached
/// decoder, and tokens. The v2 two-file layout (encoder plus merged
/// decoder) exists in `OfflineMoonshineModelConfig` but is not what
/// the releases actually ship.
pub(in crate::backends::sherpa) fn build_moonshine_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> Option<OfflineRecognizerConfig> {
    let ModelFilePaths::Moonshine {
        preprocessor,
        encoder,
        uncached_decoder,
        cached_decoder,
        tokens,
    } = sherpa_model::model_file_paths(model)
    else {
        // Statically tied to the model enum, so this can only fire on
        // a routing bug — but library crates forbid panics, so log
        // and let the caller surface an init failure.
        tracing::error!("build_moonshine_recognizer_config called with non-Moonshine layout");
        return None;
    };

    let moonshine = OfflineMoonshineModelConfig {
        preprocessor: Some(preprocessor.to_string_lossy().into_owned()),
        encoder: Some(encoder.to_string_lossy().into_owned()),
        uncached_decoder: Some(uncached_decoder.to_string_lossy().into_owned()),
        cached_decoder: Some(cached_decoder.to_string_lossy().into_owned()),
        ..OfflineMoonshineModelConfig::default()
    };

    let model_config = OfflineModelConfig {
        moonshine,
        tokens: Some(tokens.to_string_lossy().into_owned()),
        provider: Some(provider.to_owned()),
        num_threads: SHERPA_NUM_THREADS,
        ..OfflineModelConfig::default()
    };

    Some(OfflineRecognizerConfig {
        model_config,
        ..OfflineRecognizerConfig::default()
    })
}

/// Build the `OfflineRecognizerConfig` for a `NeMo` Parakeet-TDT model.
///
/// Uses sherpa-onnx's offline transducer config (4 files: encoder,
/// decoder, joiner, tokens) with `model_type = "nemo_transducer"`.
/// The `model_type` field is required — without it, sherpa-onnx tries
/// to use the generic transducer decode loop which doesn't understand
/// `NeMo`'s TDT (Token-and-Duration Transducer) joiner output shape.
///
/// Mirrors the upstream `rust-api-examples/examples/nemo_parakeet.rs`
/// example.
pub(in crate::backends::sherpa) fn build_nemo_transducer_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> Option<OfflineRecognizerConfig> {
    // `ModelFilePaths::Transducer` also matches `StreamingZipformerEn`
    // (same 4-file layout), so the destructuring alone wouldn't catch
    // a caller that passed the online Zipformer variant by mistake.
    // Guard on kind at the boundary so misuse fails loudly here
    // rather than silently building a NeMo config around Zipformer
    // files at runtime.
    debug_assert_eq!(
        model.kind(),
        crate::sherpa_model::ModelKind::OfflineNemoTransducer,
        "build_nemo_transducer_recognizer_config called with non-OfflineNemoTransducer model"
    );

    let ModelFilePaths::Transducer {
        encoder,
        decoder,
        joiner,
        tokens,
    } = sherpa_model::model_file_paths(model)
    else {
        // See build_moonshine_recognizer_config's else arm.
        tracing::error!(
            "build_nemo_transducer_recognizer_config called with non-Transducer layout"
        );
        return None;
    };

    let transducer = OfflineTransducerModelConfig {
        encoder: Some(encoder.to_string_lossy().into_owned()),
        decoder: Some(decoder.to_string_lossy().into_owned()),
        joiner: Some(joiner.to_string_lossy().into_owned()),
    };

    let model_config = OfflineModelConfig {
        transducer,
        tokens: Some(tokens.to_string_lossy().into_owned()),
        provider: Some(provider.to_owned()),
        num_threads: SHERPA_NUM_THREADS,
        // Required — tells sherpa-onnx to use NeMo's TDT decode loop
        // instead of the generic transducer path.
        model_type: Some(NEMO_TRANSDUCER_MODEL_TYPE.to_owned()),
        ..OfflineModelConfig::default()
    };

    Some(OfflineRecognizerConfig {
        model_config,
        ..OfflineRecognizerConfig::default()
    })
}

/// Source/target language for Canary's task tokens. ASR (not
/// translation) means src == tgt; this app targets English scanner
/// audio. Per issue #853 wave 2.
const CANARY_LANGUAGE: &str = "en";

/// Build the `OfflineRecognizerConfig` for NVIDIA Canary 180M Flash.
///
/// Encoder + decoder + tokens through `OfflineCanaryModelConfig`,
/// with src == tgt == "en" (plain ASR, no translation) and
/// punctuation-and-capitalization enabled — same transcript-quality
/// rationale as the Cohere builder's punct/ITN flags.
pub(in crate::backends::sherpa) fn build_canary_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> Option<OfflineRecognizerConfig> {
    debug_assert_eq!(
        model.kind(),
        crate::sherpa_model::ModelKind::OfflineCanary,
        "build_canary_recognizer_config called with non-OfflineCanary model"
    );

    let ModelFilePaths::Canary {
        encoder,
        decoder,
        tokens,
    } = sherpa_model::model_file_paths(model)
    else {
        // See build_moonshine_recognizer_config's else arm.
        tracing::error!("build_canary_recognizer_config called with non-Canary layout");
        return None;
    };

    let canary = OfflineCanaryModelConfig {
        encoder: Some(encoder.to_string_lossy().into_owned()),
        decoder: Some(decoder.to_string_lossy().into_owned()),
        src_lang: Some(CANARY_LANGUAGE.to_owned()),
        tgt_lang: Some(CANARY_LANGUAGE.to_owned()),
        use_pnc: true,
    };

    let model_config = OfflineModelConfig {
        canary,
        tokens: Some(tokens.to_string_lossy().into_owned()),
        provider: Some(provider.to_owned()),
        num_threads: SHERPA_NUM_THREADS,
        ..OfflineModelConfig::default()
    };

    Some(OfflineRecognizerConfig {
        model_config,
        ..OfflineRecognizerConfig::default()
    })
}

/// Language code Cohere Transcribe decodes with. The model is
/// multilingual; this app targets English scanner audio, so the
/// language is fixed rather than user-selectable (revisit if a
/// multilingual use case appears). Per issue #853.
const COHERE_TRANSCRIBE_LANGUAGE: &str = "en";

/// Build the `OfflineRecognizerConfig` for Cohere Transcribe.
///
/// Encoder + decoder + tokens through
/// `OfflineCohereTranscribeModelConfig`, with punctuation and inverse
/// text normalization enabled — radio transcripts read better with
/// "123" than "one two three", and the transcript panel does no
/// post-processing of its own.
pub(in crate::backends::sherpa) fn build_cohere_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> Option<OfflineRecognizerConfig> {
    debug_assert_eq!(
        model.kind(),
        crate::sherpa_model::ModelKind::OfflineCohereTranscribe,
        "build_cohere_recognizer_config called with non-OfflineCohereTranscribe model"
    );

    let ModelFilePaths::CohereTranscribe {
        encoder,
        // Resolved by onnxruntime relative to `encoder`; only
        // existence validation consumes the path.
        encoder_data: _,
        decoder,
        tokens,
    } = sherpa_model::model_file_paths(model)
    else {
        // See build_moonshine_recognizer_config's else arm.
        tracing::error!("build_cohere_recognizer_config called with non-CohereTranscribe layout");
        return None;
    };

    let cohere_transcribe = OfflineCohereTranscribeModelConfig {
        encoder: Some(encoder.to_string_lossy().into_owned()),
        decoder: Some(decoder.to_string_lossy().into_owned()),
        language: Some(COHERE_TRANSCRIBE_LANGUAGE.to_owned()),
        use_punct: true,
        use_itn: true,
    };

    let model_config = OfflineModelConfig {
        cohere_transcribe,
        tokens: Some(tokens.to_string_lossy().into_owned()),
        provider: Some(provider.to_owned()),
        num_threads: SHERPA_NUM_THREADS,
        ..OfflineModelConfig::default()
    };

    Some(OfflineRecognizerConfig {
        model_config,
        ..OfflineRecognizerConfig::default()
    })
}
