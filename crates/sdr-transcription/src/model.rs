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
    /// Large v3 turbo — distilled-decoder variant of `LargeV3`.
    /// Roughly half the disk footprint (~1.6 GB vs 3.1 GB) and
    /// ~8× faster inference for near-`LargeV3` English accuracy.
    /// Fills the "medium is too weak, large-v3 is too slow" gap.
    ///
    /// Appended at the end of [`Self::ALL`] rather than inserted
    /// between `MediumEn` and `LargeV3` because the UI persists
    /// the user's model choice as an index into `ALL`; inserting
    /// mid-list would silently upgrade every existing `LargeV3`
    /// user to turbo on next launch. Order follows stability
    /// over aesthetics.
    ///
    /// Naming follows the upstream ggerganov filename
    /// (`ggml-large-v3-turbo.bin`, no `.en` suffix — turbo is
    /// English-focused by design rather than being an `.en`
    /// fine-tune of multilingual weights).
    LargeV3Turbo,
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
            Self::LargeV3Turbo => "ggml-large-v3-turbo.bin",
        }
    }

    /// Download URL for this model variant.
    pub fn url(self) -> String {
        format!("{MODEL_BASE_URL}/{}", self.filename())
    }

    /// Human-readable display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::TinyEn => "Tiny — 75 MB",
            Self::BaseEn => "Base — 142 MB",
            Self::SmallEn => "Small — 466 MB",
            Self::MediumEn => "Medium — 1.5 GB (GPU)",
            Self::LargeV3 => "Large v3 — 3.1 GB (GPU)",
            Self::LargeV3Turbo => "Large v3 Turbo — 1.6 GB (GPU, ~8× faster than Large v3)",
        }
    }

    /// All available variants in order. Append-only by contract —
    /// the UI persists the user's model choice as an index into
    /// this slice, so reordering or inserting mid-list would
    /// silently remap every existing user's selection.
    pub const ALL: &[Self] = &[
        Self::TinyEn,
        Self::BaseEn,
        Self::SmallEn,
        Self::MediumEn,
        Self::LargeV3,
        Self::LargeV3Turbo,
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

    #[test]
    fn large_v3_turbo_filename_matches_upstream() {
        // Upstream ggerganov HF filename pin — deliberate literal
        // test rather than deriving from the enum, so a future
        // edit that accidentally changes the filename fails here
        // rather than silently breaking every user's download.
        assert_eq!(
            WhisperModel::LargeV3Turbo.filename(),
            "ggml-large-v3-turbo.bin"
        );
        assert!(
            WhisperModel::LargeV3Turbo
                .url()
                .ends_with("ggml-large-v3-turbo.bin"),
            "url must resolve to the canonical turbo filename"
        );
    }

    #[test]
    fn all_models_preserves_legacy_indices() {
        // Persistence contract: `ALL` is append-only. The UI
        // stores the user's model choice as an index into this
        // slice, so reordering or inserting mid-list would
        // silently change existing users' selections on next
        // launch. Pinning the leading indices here catches any
        // accidental reordering.
        let models = WhisperModel::ALL;
        assert_eq!(models[0], WhisperModel::TinyEn);
        assert_eq!(models[1], WhisperModel::BaseEn);
        assert_eq!(models[2], WhisperModel::SmallEn);
        assert_eq!(models[3], WhisperModel::MediumEn);
        assert_eq!(models[4], WhisperModel::LargeV3);
        assert_eq!(models[5], WhisperModel::LargeV3Turbo);
    }
}
