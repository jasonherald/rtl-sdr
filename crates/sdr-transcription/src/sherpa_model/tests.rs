use super::*;

/// Registry size after the #853 final-wave additions — the
/// persisted model index's upper bound.
const SHERPA_MODEL_COUNT: usize = 11;
/// Canary's frozen `ALL` position (wave-2 tail) — a persisted config
/// key like every other index.
const CANARY_MODEL_INDEX: usize = 6;

#[test]
fn all_models_have_unique_directory_names() {
    let names: Vec<_> = SherpaModel::ALL.iter().map(|m| m.dir_name()).collect();
    let unique: std::collections::HashSet<_> = names.iter().collect();
    assert_eq!(names.len(), unique.len());
}

#[test]
fn streaming_zipformer_en_dir_is_under_sherpa() {
    let dir = model_directory(SherpaModel::StreamingZipformerEn);
    assert!(dir.ends_with("sherpa/streaming-zipformer-en"));
}

#[test]
#[allow(clippy::panic)]
fn transducer_model_file_paths_returns_four_distinct_files() {
    let ModelFilePaths::Transducer {
        encoder,
        decoder,
        joiner,
        tokens,
    } = model_file_paths(SherpaModel::StreamingZipformerEn)
    else {
        panic!("StreamingZipformerEn should be a Transducer layout");
    };
    assert_ne!(encoder, decoder);
    assert_ne!(encoder, joiner);
    assert_ne!(encoder, tokens);
    assert_ne!(decoder, joiner);
    assert_ne!(decoder, tokens);
    assert_ne!(joiner, tokens);
}

#[test]
fn streaming_zipformer_archive_url_is_well_formed() {
    let url = SherpaModel::StreamingZipformerEn.archive_url();
    assert!(url.starts_with("https://github.com/k2-fsa/sherpa-onnx/"));
    assert!(url.ends_with(".tar.bz2"));
    assert!(url.contains("streaming-zipformer-en"));
}

#[test]
fn all_archives_have_inner_dir_matching_filename_stem() {
    // Inner directory name should equal the archive filename minus
    // the .tar.bz2 suffix — sanity check that we'll find the right
    // directory after extraction. Loops over every variant in ALL
    // so adding a new model auto-extends this protection.
    for model in SherpaModel::ALL {
        let archive = model.archive_filename();
        let inner = model.archive_inner_directory();
        assert_eq!(
            format!("{inner}.tar.bz2"),
            archive,
            "archive_inner_directory + .tar.bz2 != archive_filename for {model:?}"
        );
    }
}

#[test]
fn zipformer_is_online_transducer() {
    assert_eq!(
        SherpaModel::StreamingZipformerEn.kind(),
        ModelKind::OnlineTransducer
    );
}

#[test]
fn online_transducer_supports_partials() {
    assert!(SherpaModel::StreamingZipformerEn.supports_partials());
}

#[test]
fn supports_partials_is_derived_from_kind() {
    // Sanity check that supports_partials mirrors the kind match —
    // if anyone adds a new ModelKind variant they have to update
    // supports_partials too, and this test locks that relationship.
    for model in SherpaModel::ALL {
        let expected = matches!(model.kind(), ModelKind::OnlineTransducer);
        assert_eq!(
            model.supports_partials(),
            expected,
            "mismatch for {model:?}"
        );
    }
}

// NOTE: there's no unit test for `cleanup_scratch_state` because the
// function resolves paths via `dirs_next::data_dir()` — any test that
// called it would touch the real user's `~/.local/share/sdr-rs/models/sherpa/`
// and could delete in-progress download state. Hermetic coverage
// requires threading a base-dir parameter through `sherpa_models_dir`
// and its callers; that refactor is tracked as part of the hermetic
// testing follow-up mentioned on #255 / discussed in PR #254.

#[test]
fn moonshine_variants_are_offline_moonshine_kind() {
    assert_eq!(
        SherpaModel::MoonshineTinyEn.kind(),
        ModelKind::OfflineMoonshine
    );
    assert_eq!(
        SherpaModel::MoonshineBaseEn.kind(),
        ModelKind::OfflineMoonshine
    );
}

#[test]
fn moonshine_variants_do_not_support_partials() {
    assert!(!SherpaModel::MoonshineTinyEn.supports_partials());
    assert!(!SherpaModel::MoonshineBaseEn.supports_partials());
}

#[test]
#[allow(clippy::panic)]
fn moonshine_tiny_has_five_file_layout() {
    let paths = model_file_paths(SherpaModel::MoonshineTinyEn);
    let ModelFilePaths::Moonshine {
        preprocessor,
        encoder,
        uncached_decoder,
        cached_decoder,
        tokens,
    } = paths
    else {
        panic!("MoonshineTinyEn should be a Moonshine layout");
    };
    assert!(preprocessor.ends_with("preprocess.onnx"));
    assert!(encoder.ends_with("encode.int8.onnx"));
    assert!(uncached_decoder.ends_with("uncached_decode.int8.onnx"));
    assert!(cached_decoder.ends_with("cached_decode.int8.onnx"));
    assert!(tokens.ends_with("tokens.txt"));
    assert_ne!(encoder, uncached_decoder);
    assert_ne!(uncached_decoder, cached_decoder);
}

#[test]
fn moonshine_archive_urls_are_well_formed() {
    for model in [SherpaModel::MoonshineTinyEn, SherpaModel::MoonshineBaseEn] {
        let url = model.archive_url();
        assert!(url.starts_with("https://github.com/k2-fsa/sherpa-onnx/"));
        assert!(url.ends_with(".tar.bz2"));
        assert!(url.contains("moonshine"));
    }
}

#[test]
fn all_contains_six_variants() {
    assert_eq!(SherpaModel::ALL.len(), SHERPA_MODEL_COUNT);
}

#[test]
fn parakeet_is_offline_nemo_transducer_kind() {
    assert_eq!(
        SherpaModel::ParakeetTdt06bV3En.kind(),
        ModelKind::OfflineNemoTransducer
    );
}

#[test]
fn parakeet_does_not_support_partials() {
    assert!(!SherpaModel::ParakeetTdt06bV3En.supports_partials());
}

#[test]
#[allow(clippy::panic)]
fn parakeet_has_transducer_file_layout() {
    let paths = model_file_paths(SherpaModel::ParakeetTdt06bV3En);
    let ModelFilePaths::Transducer {
        encoder,
        decoder,
        joiner,
        tokens,
    } = paths
    else {
        panic!("ParakeetTdt06bV3En should be a Transducer layout");
    };
    assert!(encoder.ends_with("encoder.int8.onnx"));
    assert!(decoder.ends_with("decoder.int8.onnx"));
    assert!(joiner.ends_with("joiner.int8.onnx"));
    assert!(tokens.ends_with("tokens.txt"));
    assert_ne!(encoder, decoder);
    assert_ne!(decoder, joiner);
}

#[test]
fn parakeet_archive_url_is_well_formed() {
    let url = SherpaModel::ParakeetTdt06bV3En.archive_url();
    assert!(url.starts_with("https://github.com/k2-fsa/sherpa-onnx/"));
    assert!(url.ends_with(".tar.bz2"));
    assert!(url.contains("parakeet"));
    assert!(url.contains("tdt"));
    assert!(url.contains("v3"));
}

#[test]
fn parakeet_v3_filename_and_label_match_upstream() {
    // Upstream k2-fsa release filename + display label pin —
    // deliberate literal test rather than deriving from the enum,
    // so a future edit that accidentally changes either one fails
    // here rather than silently breaking every user's download or
    // their persisted UI selection. Mirrors the Whisper-side
    // `large_v3_turbo_filename_matches_upstream` pattern.
    assert_eq!(
        SherpaModel::ParakeetTdt06bV3En.archive_filename(),
        "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2"
    );
    assert_eq!(
        SherpaModel::ParakeetTdt06bV3En.label(),
        "Parakeet TDT 0.6b v3 (English)"
    );
    assert!(
        SherpaModel::ParakeetTdt06bV3En
            .archive_url()
            .ends_with("sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2"),
        "archive_url must resolve to the canonical upstream filename"
    );
}

#[test]
fn all_models_preserves_legacy_indices() {
    // Persistence contract: `ALL` is append-only. The UI stores
    // the user's model choice as an index into this slice, so
    // reordering or inserting mid-list would silently change
    // existing users' selections on next launch. Pinning the
    // leading indices here catches any accidental reordering.
    // Mirrors the Whisper-side `all_models_preserves_legacy_indices`
    // pattern.
    let models = SherpaModel::ALL;
    assert_eq!(models[0], SherpaModel::StreamingZipformerEn);
    assert_eq!(models[1], SherpaModel::MoonshineTinyEn);
    assert_eq!(models[2], SherpaModel::MoonshineBaseEn);
    assert_eq!(models[3], SherpaModel::ParakeetTdt06bV3En);
}

#[test]
fn silero_vad_path_is_under_sherpa_models_dir() {
    let path = silero_vad_path();
    assert!(path.ends_with("silero-vad/silero_vad.onnx"));
}

// ── #853: Nemotron streaming + Cohere Transcribe ──────────────────

#[test]
fn nemotron_is_online_transducer_with_128_feature_dim() {
    let m = SherpaModel::NemotronStreamingEn;
    assert_eq!(m.kind(), ModelKind::OnlineTransducer);
    assert!(m.supports_partials());
    // NVIDIA's cache-aware streaming export uses 128-dim features —
    // Zipformer's 80 would silently produce garbage decodes.
    assert_eq!(m.feature_dim(), 128);
    assert_eq!(SherpaModel::StreamingZipformerEn.feature_dim(), 80);
}

#[test]
fn nemotron_file_paths_are_int8_transducer_layout() {
    let ModelFilePaths::Transducer {
        encoder,
        decoder,
        joiner,
        tokens,
    } = model_file_paths(SherpaModel::NemotronStreamingEn)
    else {
        panic!("nemotron must be a Transducer layout");
    };
    assert!(encoder.ends_with("nemotron-streaming-en/encoder.int8.onnx"));
    assert!(decoder.ends_with("nemotron-streaming-en/decoder.int8.onnx"));
    assert!(joiner.ends_with("nemotron-streaming-en/joiner.int8.onnx"));
    assert!(tokens.ends_with("nemotron-streaming-en/tokens.txt"));
}

#[test]
fn cohere_is_offline_with_encoder_decoder_layout() {
    let m = SherpaModel::CohereTranscribe14Lang;
    assert_eq!(m.kind(), ModelKind::OfflineCohereTranscribe);
    assert!(!m.supports_partials());
    let ModelFilePaths::CohereTranscribe {
        encoder,
        encoder_data,
        decoder,
        tokens,
    } = model_file_paths(m)
    else {
        panic!("cohere must be a CohereTranscribe layout");
    };
    assert!(encoder.ends_with("cohere-transcribe-14-lang/encoder.int8.onnx"));
    // The 2B encoder ships as a ~3 MB ONNX graph plus a ~2.7 GB
    // external-data sidecar — the sidecar IS the weights, and
    // model_exists must demand it (CR round 2 on PR #857: a missing
    // sidecar would skip the download and init an empty encoder).
    assert!(encoder_data.ends_with("cohere-transcribe-14-lang/encoder.int8.onnx.data"));
    assert!(decoder.ends_with("cohere-transcribe-14-lang/decoder.int8.onnx"));
    assert!(tokens.ends_with("cohere-transcribe-14-lang/tokens.txt"));
}

#[test]
fn new_models_are_appended_after_existing_indices() {
    // The UI persists the selection as an index into ALL — existing
    // entries' positions are config keys and must not move.
    /// Number of models shipped before #853 — their `ALL` positions
    /// are frozen by persisted configs.
    const PRE_853_MODEL_COUNT: usize = 4;

    assert_eq!(
        SherpaModel::ALL[..PRE_853_MODEL_COUNT],
        [
            SherpaModel::StreamingZipformerEn,
            SherpaModel::MoonshineTinyEn,
            SherpaModel::MoonshineBaseEn,
            SherpaModel::ParakeetTdt06bV3En,
        ]
    );
    assert_eq!(SherpaModel::ALL.len(), SHERPA_MODEL_COUNT);
    assert_eq!(
        SherpaModel::ALL[PRE_853_MODEL_COUNT],
        SherpaModel::NemotronStreamingEn
    );
    assert_eq!(
        SherpaModel::ALL[PRE_853_MODEL_COUNT + 1],
        SherpaModel::CohereTranscribe14Lang
    );
}

// ── #853 wave 2: Canary 180M Flash ────────────────────────────────

#[test]
fn canary_is_offline_with_encoder_decoder_layout() {
    let m = SherpaModel::Canary180mFlash;
    assert_eq!(m.kind(), ModelKind::OfflineCanary);
    assert!(!m.supports_partials());
    assert_eq!(m.feature_dim(), 80);
    let ModelFilePaths::Canary {
        encoder,
        decoder,
        tokens,
    } = model_file_paths(m)
    else {
        panic!("canary must be a Canary layout");
    };
    assert!(encoder.ends_with("canary-180m-flash/encoder.int8.onnx"));
    assert!(decoder.ends_with("canary-180m-flash/decoder.int8.onnx"));
    assert!(tokens.ends_with("canary-180m-flash/tokens.txt"));
}

#[test]
fn final_wave_is_appended_after_canary() {
    assert_eq!(SherpaModel::ALL.len(), SHERPA_MODEL_COUNT);
    // Wave-2 tail position is frozen (persisted index), final wave
    // appends after it.
    assert_eq!(
        SherpaModel::ALL[CANARY_MODEL_INDEX],
        SherpaModel::Canary180mFlash
    );
    assert_eq!(
        SherpaModel::ALL[SHERPA_MODEL_COUNT - 1],
        SherpaModel::ParakeetUnifiedEn06b
    );
}

// ── #853 final wave: Nemotron lookahead variants ──────────────────

#[test]
fn nemotron_variants_share_the_streaming_contract() {
    for m in [
        SherpaModel::NemotronStreamingEn80ms,
        SherpaModel::NemotronStreamingEn160ms,
        SherpaModel::NemotronStreamingEn1120ms,
    ] {
        assert_eq!(m.kind(), ModelKind::OnlineTransducer, "{m:?}");
        assert!(m.supports_partials(), "{m:?}");
        assert_eq!(m.feature_dim(), 128, "{m:?}");
        let ModelFilePaths::Transducer { encoder, .. } = model_file_paths(m) else {
            panic!("{m:?} must be a Transducer layout");
        };
        assert!(encoder.ends_with("encoder.int8.onnx"), "{m:?}");
    }
    // Distinct storage dirs — variants must not clobber each other.
    let dirs: std::collections::HashSet<_> = [
        SherpaModel::NemotronStreamingEn,
        SherpaModel::NemotronStreamingEn80ms,
        SherpaModel::NemotronStreamingEn160ms,
        SherpaModel::NemotronStreamingEn1120ms,
    ]
    .iter()
    .map(|m| m.dir_name())
    .collect();
    assert_eq!(dirs.len(), 4);
}

// ── #858: per-backend model support ───────────────────────────────

#[cfg(not(feature = "sherpa-rocm"))]
#[test]
fn all_models_supported_on_non_rocm_backends() {
    assert!(SherpaModel::ALL.iter().all(|m| m.supported_on_backend()));
}

#[cfg(feature = "sherpa-rocm")]
#[test]
fn rocm_backend_allows_only_cohere() {
    // The 780M bring-up matrix (issue #858): Cohere was the one
    // model MIGraphX ran correctly.
    for m in SherpaModel::ALL {
        assert_eq!(
            m.supported_on_backend(),
            *m == SherpaModel::CohereTranscribe14Lang,
            "{m:?}"
        );
    }
}
