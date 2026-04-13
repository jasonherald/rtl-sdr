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
use std::sync::mpsc;
use std::time::Duration;

/// Returns the base directory for storing models (`~/.local/share/sdr-rs/models/`).
///
/// Duplicated from `model::models_dir` so that `sherpa_model` has no
/// dependency on the `whisper`-gated `model` module.
fn models_dir() -> PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sdr-rs")
        .join("models")
}

/// Errors from sherpa-onnx model download and extraction.
///
/// Mirrors `crate::model::ModelError` from the Whisper side; we don't
/// share that type because the `model` module is `#[cfg(feature = "whisper")]`
/// gated and `sherpa_model` lives behind `#[cfg(feature = "sherpa")]`.
#[derive(Debug, thiserror::Error)]
pub enum SherpaModelError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("archive extraction failed: {0}")]
    Extract(String),
}

/// Remove any leftover scratch files/directories from a previous failed
/// download attempt for `model`. Returns Ok if no scratch existed or
/// cleanup succeeded; Err only if removal failed (e.g. permission denied).
///
/// Idempotent — safe to call when the model has never been downloaded.
fn cleanup_scratch_state(model: SherpaModel) -> Result<(), SherpaModelError> {
    let dir = sherpa_models_dir();
    let archive_part_path = dir.join(format!("{}.part", model.archive_filename()));
    let temp_extract_dir = dir.join(format!("{}.partdir", model.dir_name()));

    if archive_part_path.exists() {
        std::fs::remove_file(&archive_part_path)?;
    }
    if temp_extract_dir.exists() {
        std::fs::remove_dir_all(&temp_extract_dir)?;
    }
    Ok(())
}

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
            Self::StreamingZipformerEn => "encoder-epoch-99-avg-1-chunk-16-left-128.onnx",
        }
    }

    /// Filename of the decoder ONNX file inside the model directory.
    pub fn decoder_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "decoder-epoch-99-avg-1-chunk-16-left-128.onnx",
        }
    }

    /// Filename of the joiner ONNX file inside the model directory.
    pub fn joiner_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "joiner-epoch-99-avg-1-chunk-16-left-128.onnx",
        }
    }

    /// Filename of the tokens file inside the model directory.
    pub fn tokens_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "tokens.txt",
        }
    }

    /// Filename of the upstream `.tar.bz2` archive on the k2-fsa GitHub
    /// releases page. Used by `download_sherpa_model` to construct the
    /// download URL and to name the local `.part` file during fetch.
    pub fn archive_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2",
        }
    }

    /// Name of the top-level directory inside the extracted archive.
    /// Sherpa archives unpack to a directory named like
    /// `sherpa-onnx-streaming-zipformer-en-2023-06-26/`. After extraction
    /// we rename it to `dir_name()` so the path layout matches what
    /// `model_directory()` expects.
    pub fn archive_inner_directory(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "sherpa-onnx-streaming-zipformer-en-2023-06-26",
        }
    }

    /// Full HTTPS URL to the upstream `.tar.bz2` archive on the k2-fsa
    /// GitHub releases page.
    pub fn archive_url(self) -> String {
        format!(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/{}",
            self.archive_filename()
        )
    }

    /// All available variants in order — used to populate the UI dropdown.
    pub const ALL: &[Self] = &[Self::StreamingZipformerEn];
}

/// Returns the sherpa subdirectory under the shared models dir
/// (`~/.local/share/sdr-rs/models/sherpa/`).
pub fn sherpa_models_dir() -> PathBuf {
    models_dir().join("sherpa")
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

/// Download a sherpa-onnx model bundle from the k2-fsa GitHub releases
/// page. Does NOT extract — call [`extract_sherpa_archive`] separately
/// to perform the extraction phase. Splitting download and extract lets
/// the caller (e.g. `SherpaHost::run_host_loop`) emit a separate UI
/// progress event when transitioning into extraction.
///
/// # Arguments
///
/// * `model` — which sherpa model to download
/// * `progress_tx` — receives integer percent values (0..=100) as the
///   download streams.
///
/// # Returns
///
/// On success, the absolute path to the downloaded `.tar.bz2.part` file.
/// Pass this to [`extract_sherpa_archive`] to complete installation.
///
/// # Behavior
///
/// 1. Cleans up any leftover `.part` archive or `.partdir` extraction
///    directory from a previous failed attempt.
/// 2. Downloads the `.tar.bz2` to `<archive_filename>.part` in
///    [`sherpa_models_dir`], streaming progress through `progress_tx`.
#[allow(clippy::cast_possible_truncation)]
pub fn download_sherpa_archive(
    model: SherpaModel,
    progress_tx: &mpsc::Sender<u8>,
) -> Result<PathBuf, SherpaModelError> {
    let dir = sherpa_models_dir();
    std::fs::create_dir_all(&dir)?;

    let archive_filename = model.archive_filename();
    let archive_part_path = dir.join(format!("{archive_filename}.part"));
    let archive_url = model.archive_url();

    // Clean up any leftover state from a previous failed attempt.
    cleanup_scratch_state(model)?;

    tracing::info!(
        url = %archive_url,
        ?archive_part_path,
        "downloading sherpa-onnx model bundle"
    );

    // 30-second connection timeout (fail fast if the server is unreachable),
    // 60-minute total body timeout (256 MB at ~70 KB/s — slow but still
    // legitimate for users on rural broadband or hotel WiFi).
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_mins(60))
        .build()?;

    let response = client.get(&archive_url).send()?.error_for_status()?;
    let total_size = response.content_length().unwrap_or(0);

    // If the server didn't return Content-Length, we can't compute a
    // percent. Send a single 0 sentinel so the caller knows the download
    // has started — without it, the splash label would never update from
    // its initial state until the download finished. GitHub's CDN
    // reliably sets Content-Length so this path is rarely hit in practice.
    if total_size == 0 {
        let _ = progress_tx.send(0);
    }

    let mut file = std::fs::File::create(&archive_part_path)?;
    let mut downloaded: u64 = 0;
    let mut last_pct: u8 = 0;
    let mut reader = response;
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let bytes_read = std::io::Read::read(&mut reader, &mut buf)?;
        if bytes_read == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..bytes_read])?;
        downloaded += bytes_read as u64;

        if let Some(pct) = (downloaded * 100).checked_div(total_size) {
            let pct = pct.min(100) as u8;
            if pct != last_pct {
                last_pct = pct;
                let _ = progress_tx.send(pct);
            }
        }
    }

    std::io::Write::flush(&mut file)?;
    drop(file);
    tracing::info!(bytes = downloaded, "sherpa-onnx archive download complete");

    Ok(archive_part_path)
}

/// Extract a previously-downloaded sherpa-onnx archive into the final
/// model directory.
///
/// # Arguments
///
/// * `model` — which sherpa model the archive is for
/// * `archive_path` — path to the downloaded `.tar.bz2.part` file (the
///   return value of [`download_sherpa_archive`])
///
/// # Returns
///
/// On success, the absolute path to the final extracted model directory
/// (the same path that [`model_directory`] returns).
///
/// # Behavior
///
/// 1. Extracts the archive to `<dir_name>.partdir` (a sibling of the
///    final location).
/// 2. Removes any existing target directory, then renames the extracted
///    top-level directory to the final `dir_name()` location. The rename
///    itself is atomic, but the remove-then-rename sequence is not — if
///    the process is killed between the two syscalls, the model is in
///    "not installed" state and the next launch will trigger a fresh
///    download. Acceptable failure mode.
/// 3. Cleans up the `.part` file and `.partdir` directory.
pub fn extract_sherpa_archive(
    model: SherpaModel,
    archive_path: &std::path::Path,
) -> Result<PathBuf, SherpaModelError> {
    let dir = sherpa_models_dir();
    let final_dir = model_directory(model);
    let temp_extract_dir = dir.join(format!("{}.partdir", model.dir_name()));

    tracing::info!(?archive_path, ?temp_extract_dir, "extracting sherpa-onnx archive");

    // Extract via tar + bzip2 into a temp directory adjacent to the
    // final location.
    std::fs::create_dir_all(&temp_extract_dir)?;
    let archive_file = std::fs::File::open(archive_path)?;
    let bz_reader = bzip2::read::BzDecoder::new(archive_file);
    let mut tar_archive = tar::Archive::new(bz_reader);
    tar_archive
        .unpack(&temp_extract_dir)
        .map_err(|e| SherpaModelError::Extract(format!("tar/bzip2 unpack failed: {e}")))?;

    // The tarball contains a single top-level directory whose name we
    // know via `archive_inner_directory()`. Move it to the final location.
    let extracted_inner = temp_extract_dir.join(model.archive_inner_directory());
    if !extracted_inner.is_dir() {
        return Err(SherpaModelError::Extract(format!(
            "expected directory {} not found inside extracted archive",
            extracted_inner.display()
        )));
    }

    if final_dir.exists() {
        tracing::info!(
            ?final_dir,
            "removing existing final directory before rename"
        );
        std::fs::remove_dir_all(&final_dir)?;
    }
    std::fs::rename(&extracted_inner, &final_dir)?;

    // Clean up scratch state.
    std::fs::remove_dir_all(&temp_extract_dir)?;
    std::fs::remove_file(archive_path)?;

    tracing::info!(?final_dir, "sherpa-onnx model installed");
    Ok(final_dir)
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

    #[test]
    fn streaming_zipformer_archive_url_is_well_formed() {
        let url = SherpaModel::StreamingZipformerEn.archive_url();
        assert!(url.starts_with("https://github.com/k2-fsa/sherpa-onnx/"));
        assert!(url.ends_with(".tar.bz2"));
        assert!(url.contains("streaming-zipformer-en"));
    }

    #[test]
    fn streaming_zipformer_archive_inner_dir_matches_filename_stem() {
        let model = SherpaModel::StreamingZipformerEn;
        let archive = model.archive_filename();
        let inner = model.archive_inner_directory();
        // Inner directory name should equal the archive filename minus
        // the .tar.bz2 suffix — sanity check that we'll find the right
        // directory after extraction.
        assert_eq!(format!("{inner}.tar.bz2"), archive);
    }

    #[test]
    fn cleanup_scratch_state_is_idempotent_when_nothing_exists() {
        // Ensures the helper handles the no-leftover case without error.
        // Relies on the developer's environment being in a sane state
        // (no leftover .part files in ~/.local/share/sdr-rs/models/sherpa/).
        // If the dev has scratch lying around, the test still passes —
        // it just removes them, which is the function's job.
        let result = cleanup_scratch_state(SherpaModel::StreamingZipformerEn);
        assert!(
            result.is_ok(),
            "expected Ok on fresh/missing state, got {result:?}"
        );
    }
}
