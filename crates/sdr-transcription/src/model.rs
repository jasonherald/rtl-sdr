//! Whisper model download and path management.

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;

/// Base URL for Whisper GGML models on `HuggingFace`.
const MODEL_BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// Available Whisper model variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhisperModel {
    TinyEn,
    BaseEn,
    SmallEn,
    MediumEn,
    LargeV3,
}

impl WhisperModel {
    /// GGML filename for this model variant.
    pub fn filename(self) -> &'static str {
        match self {
            Self::TinyEn => "ggml-tiny.en.bin",
            Self::BaseEn => "ggml-base.en.bin",
            Self::SmallEn => "ggml-small.en.bin",
            Self::MediumEn => "ggml-medium.en.bin",
            Self::LargeV3 => "ggml-large-v3.bin",
        }
    }

    /// Download URL for this model variant.
    pub fn url(self) -> String {
        format!("{MODEL_BASE_URL}/{}", self.filename())
    }

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::TinyEn => "Tiny (English, ~75 MB)",
            Self::BaseEn => "Base (English, ~142 MB)",
            Self::SmallEn => "Small (English, ~466 MB)",
            Self::MediumEn => "Medium (English, ~1.5 GB)",
            Self::LargeV3 => "Large v3 (Multilingual, ~3.1 GB)",
        }
    }

    /// All available variants in order.
    pub const ALL: &[Self] = &[
        Self::TinyEn,
        Self::BaseEn,
        Self::SmallEn,
        Self::MediumEn,
        Self::LargeV3,
    ];
}

/// Errors from model download and path management.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
}

/// Returns the directory for storing models (`~/.local/share/sdr-rs/models/`).
pub fn models_dir() -> PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sdr-rs")
        .join("models")
}

/// Returns the full path for a given model variant.
pub fn model_path(model: WhisperModel) -> PathBuf {
    models_dir().join(model.filename())
}

/// Check if a model file exists locally.
pub fn model_exists(model: WhisperModel) -> bool {
    model_path(model).is_file()
}

/// Download a Whisper model, sending progress events.
/// Blocks until download completes.
#[allow(clippy::cast_possible_truncation)]
pub fn download_model(
    model: WhisperModel,
    progress_tx: &mpsc::Sender<u8>,
) -> Result<PathBuf, ModelError> {
    let dir = models_dir();
    std::fs::create_dir_all(&dir)?;

    let filename = model.filename();
    let dest = dir.join(filename);
    let part = dir.join(format!("{filename}.part"));
    tracing::info!(?dest, model = ?model, "downloading Whisper model");

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_mins(2))
        .build()?;

    let response = client.get(model.url()).send()?.error_for_status()?;
    let total_size = response.content_length().unwrap_or(0);

    let mut file = std::fs::File::create(&part)?;
    let mut downloaded: u64 = 0;
    let mut last_pct: u8 = 0;

    let mut reader = response;
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let bytes_read = std::io::Read::read(&mut reader, &mut buf)?;
        if bytes_read == 0 {
            break;
        }
        file.write_all(&buf[..bytes_read])?;
        downloaded += bytes_read as u64;

        if let Some(pct) = (downloaded * 100).checked_div(total_size) {
            let pct = pct.min(100) as u8;
            if pct != last_pct {
                last_pct = pct;
                let _ = progress_tx.send(pct);
            }
        }
    }

    file.flush()?;
    drop(file);
    std::fs::rename(&part, &dest)?;
    tracing::info!(?dest, bytes = downloaded, "model download complete");

    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_dir_ends_with_expected_path() {
        let dir = models_dir();
        assert!(dir.ends_with("sdr-rs/models"));
    }

    #[test]
    fn model_path_includes_filename() {
        let path = model_path(WhisperModel::TinyEn);
        assert_eq!(
            path.file_name().and_then(|f| f.to_str()),
            Some("ggml-tiny.en.bin")
        );
    }

    #[test]
    fn all_models_have_unique_filenames() {
        let filenames: Vec<_> = WhisperModel::ALL.iter().map(|m| m.filename()).collect();
        let unique: std::collections::HashSet<_> = filenames.iter().collect();
        assert_eq!(filenames.len(), unique.len());
    }

    #[test]
    fn model_url_contains_filename() {
        for model in WhisperModel::ALL {
            assert!(model.url().contains(model.filename()));
        }
    }
}
