use super::*;

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
fn all_contains_four_variants() {
    assert_eq!(SherpaModel::ALL.len(), 4);
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
