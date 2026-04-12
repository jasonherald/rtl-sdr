//! Sherpa-onnx model registry and path management.
//!
//! Mirrors `model.rs` (the Whisper registry) but for sherpa-onnx bundles.
//! Each `SherpaModel` variant maps to a directory containing the encoder,
//! decoder, joiner, and tokens files for one streaming ASR model.
//!
//! For PR 2 (the sherpa spike) the user manually downloads bundles into
//! `models_dir() / sherpa / <model>/` before launching. PR 3 adds
//! auto-download.

use std::path::PathBuf;

/// Available sherpa-onnx model variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SherpaModel {
    /// Streaming Zipformer English (k2-fsa, 2023-06-26).
    StreamingZipformerEn,
}

impl SherpaModel {
    /// Human-readable display label for the model picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "Streaming Zipformer (English)",
        }
    }

    /// Directory name (under `models_dir() / sherpa /`) where this model's
    /// files live.
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "streaming-zipformer-en",
        }
    }

    /// Filename of the encoder ONNX file inside the model directory.
    pub fn encoder_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => {
                "encoder-epoch-99-avg-1-chunk-16-left-128.onnx"
            }
        }
    }

    /// Filename of the decoder ONNX file inside the model directory.
    pub fn decoder_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => {
                "decoder-epoch-99-avg-1-chunk-16-left-128.onnx"
            }
        }
    }

    /// Filename of the joiner ONNX file inside the model directory.
    pub fn joiner_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => {
                "joiner-epoch-99-avg-1-chunk-16-left-128.onnx"
            }
        }
    }

    /// Filename of the tokens file inside the model directory.
    pub fn tokens_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "tokens.txt",
        }
    }

    /// All available variants in order — used to populate the UI dropdown.
    pub const ALL: &[Self] = &[Self::StreamingZipformerEn];
}

/// Returns the sherpa subdirectory under the shared models dir
/// (`~/.local/share/sdr-rs/models/sherpa/`).
pub fn sherpa_models_dir() -> PathBuf {
    crate::model::models_dir().join("sherpa")
}

/// Returns the directory containing all files for a given sherpa model
/// (`~/.local/share/sdr-rs/models/sherpa/<dir_name>/`).
pub fn model_directory(model: SherpaModel) -> PathBuf {
    sherpa_models_dir().join(model.dir_name())
}

/// Returns the full paths for all files needed by a sherpa model.
///
/// Order: (encoder, decoder, joiner, tokens). The caller checks each path
/// for existence and emits a helpful error if any are missing.
pub fn model_file_paths(model: SherpaModel) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = model_directory(model);
    (
        dir.join(model.encoder_filename()),
        dir.join(model.decoder_filename()),
        dir.join(model.joiner_filename()),
        dir.join(model.tokens_filename()),
    )
}

/// True if all four required files for `model` exist on disk.
pub fn model_exists(model: SherpaModel) -> bool {
    let (e, d, j, t) = model_file_paths(model);
    e.is_file() && d.is_file() && j.is_file() && t.is_file()
}

#[cfg(test)]
mod tests {
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
    fn model_file_paths_returns_four_distinct_files() {
        let (e, d, j, t) = model_file_paths(SherpaModel::StreamingZipformerEn);
        assert_ne!(e, d);
        assert_ne!(e, j);
        assert_ne!(e, t);
        assert_ne!(d, j);
        assert_ne!(d, t);
        assert_ne!(j, t);
    }
}
