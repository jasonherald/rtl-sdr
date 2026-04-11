//! Whisper model download and path management.

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;

/// Whisper tiny English GGML model URL.
const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin";

/// Model filename.
const MODEL_FILENAME: &str = "ggml-tiny.en.bin";

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

/// Returns the full path to the Whisper tiny English model.
pub fn model_path() -> PathBuf {
    models_dir().join(MODEL_FILENAME)
}

/// Check if the model file exists.
pub fn model_exists() -> bool {
    model_path().is_file()
}

/// Download the Whisper tiny English model, sending progress events.
/// Blocks until download completes.
#[allow(clippy::cast_possible_truncation)]
pub fn download_model(progress_tx: &mpsc::Sender<u8>) -> Result<PathBuf, ModelError> {
    let dir = models_dir();
    std::fs::create_dir_all(&dir)?;

    let dest = dir.join(MODEL_FILENAME);
    tracing::info!(?dest, "downloading Whisper model");

    let response = reqwest::blocking::get(MODEL_URL)?;
    let total_size = response.content_length().unwrap_or(0);

    let mut file = std::fs::File::create(&dest)?;
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
        let path = model_path();
        assert_eq!(path.file_name().and_then(|f| f.to_str()), Some(MODEL_FILENAME));
    }

    #[test]
    fn model_exists_returns_false_when_no_file() {
        // The model file should not exist in the test environment.
        assert!(!model_exists());
    }
}
