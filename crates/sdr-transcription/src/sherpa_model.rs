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

/// Which sherpa-onnx recognizer family a model belongs to.
///
/// Drives host init branching and session loop dispatch. Online
/// models run through `OnlineRecognizer` + streaming chunks;
/// offline models run through `OfflineRecognizer` + external VAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// Streaming transducer: Zipformer today. Uses `OnlineRecognizer`
    /// + streaming session loop in `backends/sherpa/streaming.rs`.
    OnlineTransducer,
    /// Offline encoder-decoder: Moonshine v1. Requires external VAD
    /// (Silero) to detect utterance boundaries before batch decoding.
    /// Uses `OfflineRecognizer` with `OfflineMoonshineModelConfig`
    /// + the offline session loop in `backends/sherpa/offline.rs`.
    OfflineMoonshine,
    /// Offline transducer-style model from NVIDIA `NeMo`: Parakeet-TDT
    /// today. Uses `OfflineRecognizer` with `OfflineTransducerModelConfig`
    /// and `model_type = "nemo_transducer"`. Shares the same VAD-gated
    /// offline session loop as `OfflineMoonshine`; only the recognizer
    /// config builder differs.
    OfflineNemoTransducer,
}

/// Available sherpa-onnx model variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SherpaModel {
    /// Streaming Zipformer English (k2-fsa, 2023-06-26).
    StreamingZipformerEn,
    /// Moonshine Tiny (`UsefulSensors`, English, int8). ~27M params,
    /// ~170MB bundle. Fastest Moonshine variant — best for CPU-only
    /// and low-end hardware. Offline (VAD-gated) decode.
    MoonshineTinyEn,
    /// Moonshine Base (`UsefulSensors`, English, int8). ~61M params,
    /// ~380MB bundle. More accurate than Tiny, higher per-utterance
    /// latency. Offline (VAD-gated) decode.
    MoonshineBaseEn,
    /// NVIDIA Parakeet-TDT-0.6b v3 (English, int8). ~600M params,
    /// ~600MB bundle. Highest accuracy — currently #1 on the `OpenASR`
    /// leaderboard. CPU-only today (sherpa-cuda follow-up tracked).
    /// Offline (VAD-gated) batch decode through a `NeMo` transducer.
    ParakeetTdt06bV3En,
}

impl SherpaModel {
    /// Human-readable display label for the model picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "Streaming Zipformer (English)",
            Self::MoonshineTinyEn => "Moonshine Tiny (English)",
            Self::MoonshineBaseEn => "Moonshine Base (English)",
            Self::ParakeetTdt06bV3En => "Parakeet TDT 0.6b v3 (English)",
        }
    }

    /// Directory name (under `models_dir() / sherpa /`) where this model's
    /// files live.
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "streaming-zipformer-en",
            Self::MoonshineTinyEn => "moonshine-tiny-en",
            Self::MoonshineBaseEn => "moonshine-base-en",
            Self::ParakeetTdt06bV3En => "parakeet-tdt-0.6b-v3-en",
        }
    }

    /// Filename of the upstream `.tar.bz2` archive on the k2-fsa GitHub
    /// releases page. Used by `download_sherpa_model` to construct the
    /// download URL and to name the local `.part` file during fetch.
    pub fn archive_filename(self) -> &'static str {
        match self {
            Self::StreamingZipformerEn => "sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2",
            Self::MoonshineTinyEn => "sherpa-onnx-moonshine-tiny-en-int8.tar.bz2",
            Self::MoonshineBaseEn => "sherpa-onnx-moonshine-base-en-int8.tar.bz2",
            Self::ParakeetTdt06bV3En => "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
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
            Self::MoonshineTinyEn => "sherpa-onnx-moonshine-tiny-en-int8",
            Self::MoonshineBaseEn => "sherpa-onnx-moonshine-base-en-int8",
            Self::ParakeetTdt06bV3En => "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
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

    /// Which recognizer family this model uses.
    ///
    /// The host worker branches on this at init time to pick the
    /// right recognizer type and session loop.
    pub fn kind(self) -> ModelKind {
        match self {
            Self::StreamingZipformerEn => ModelKind::OnlineTransducer,
            Self::MoonshineTinyEn | Self::MoonshineBaseEn => ModelKind::OfflineMoonshine,
            Self::ParakeetTdt06bV3En => ModelKind::OfflineNemoTransducer,
        }
    }

    /// True if this model emits intermediate hypothesis updates
    /// (`TranscriptionEvent::Partial`) during speech.
    ///
    /// Drives contextual UI: the "Display mode" (Live/Final) toggle
    /// only appears for models that return `true` here. Offline
    /// models decode once per utterance so partials are not
    /// meaningful.
    pub fn supports_partials(self) -> bool {
        match self.kind() {
            ModelKind::OnlineTransducer => true,
            ModelKind::OfflineMoonshine | ModelKind::OfflineNemoTransducer => false,
        }
    }

    /// All available variants in order — used to populate the UI dropdown.
    pub const ALL: &[Self] = &[
        Self::StreamingZipformerEn,
        Self::MoonshineTinyEn,
        Self::MoonshineBaseEn,
        Self::ParakeetTdt06bV3En,
    ];
}

/// Returns the sherpa subdirectory under the shared models dir
/// (`~/.local/share/sdr-rs/models/sherpa/`).
pub fn sherpa_models_dir() -> PathBuf {
    models_dir().join("sherpa")
}

/// Filename of the Silero VAD ONNX model when stored locally.
const SILERO_VAD_FILENAME: &str = "silero_vad.onnx";

/// Directory under `sherpa_models_dir` where the Silero VAD model lives.
const SILERO_VAD_DIR_NAME: &str = "silero-vad";

/// Full HTTPS URL to the Silero VAD ONNX file on the k2-fsa GitHub
/// releases page. Single-file artifact — no tarball, no extraction.
const SILERO_VAD_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";

/// Total-request timeout for the Silero VAD download. The
/// file is ~2 MB but the releases CDN is occasionally slow on
/// cold cache or from specific regions — 5 minutes gives
/// rural broadband / hotel `WiFi` a reasonable envelope without
/// letting a genuinely broken connection hang forever. The
/// separate `connect_timeout(30s)` catches the "can't reach
/// github at all" case fast.
const SILERO_VAD_REQUEST_TIMEOUT_MINS: u64 = 5;

/// Total-request timeout for the sherpa-onnx ASR bundle
/// downloads. These are tarballs in the 250 MB – 1.2 GB range
/// depending on the model (Parakeet is the largest), and
/// unlike the VAD file they can legitimately take a long time
/// over rural broadband. 1 hour is our "give up" threshold —
/// generous enough that a user on a 5 Mbps connection can
/// still download the largest bundle (Parakeet ≈ 30 min at
/// that speed), tight enough that a dead connection eventually
/// surfaces as an error instead of a permanent "Downloading…"
/// spinner.
const SHERPA_ARCHIVE_REQUEST_TIMEOUT_HOURS: u64 = 1;

/// Full path to the Silero VAD ONNX file on disk.
pub fn silero_vad_path() -> PathBuf {
    sherpa_models_dir()
        .join(SILERO_VAD_DIR_NAME)
        .join(SILERO_VAD_FILENAME)
}

/// True if the Silero VAD model exists on disk.
pub fn silero_vad_exists() -> bool {
    silero_vad_path().is_file()
}

/// Download the Silero VAD ONNX model from the k2-fsa releases page.
///
/// # Arguments
///
/// * `progress_tx` — receives integer percent values (0..=100) as the
///   download streams. The file is ~2 MB so this usually only fires
///   a handful of times.
///
/// # Returns
///
/// On success, the absolute path to the downloaded `silero_vad.onnx`.
///
/// # Behavior
///
/// 1. Creates the parent directory if needed.
/// 2. Downloads to `silero_vad.onnx.part` in the same directory.
/// 3. Renames `.part` → final path on successful completion.
///
/// Unlike model bundles, the VAD is a single `.onnx` file — no
/// extraction step. The atomic rename is sufficient to avoid leaving
/// a partially-written model in place if the process dies mid-download.
#[allow(clippy::cast_possible_truncation)]
pub fn download_silero_vad(
    progress_tx: &std::sync::mpsc::Sender<u8>,
) -> Result<PathBuf, SherpaModelError> {
    // Compose the destination directly so there's no `.parent().expect()`
    // dance — library code must not panic on shape assumptions.
    let dir = sherpa_models_dir().join(SILERO_VAD_DIR_NAME);
    let final_path = dir.join(SILERO_VAD_FILENAME);
    std::fs::create_dir_all(&dir)?;

    let part_path = dir.join(format!("{SILERO_VAD_FILENAME}.part"));

    // Clean up leftover scratch from a previous failed attempt.
    if part_path.exists() {
        std::fs::remove_file(&part_path)?;
    }

    tracing::info!(url = %SILERO_VAD_URL, ?part_path, "downloading silero VAD");

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_mins(SILERO_VAD_REQUEST_TIMEOUT_MINS))
        .build()?;

    let response = client.get(SILERO_VAD_URL).send()?.error_for_status()?;
    let total_size = response.content_length().unwrap_or(0);

    if total_size == 0 {
        let _ = progress_tx.send(0);
    }

    let mut file = std::fs::File::create(&part_path)?;
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

    // Atomic rename into place.
    std::fs::rename(&part_path, &final_path)?;

    tracing::info!(
        bytes = downloaded,
        ?final_path,
        "silero VAD download complete"
    );
    Ok(final_path)
}

/// Returns the directory containing all files for a given sherpa model
/// (`~/.local/share/sdr-rs/models/sherpa/<dir_name>/`).
pub fn model_directory(model: SherpaModel) -> PathBuf {
    sherpa_models_dir().join(model.dir_name())
}

/// Concrete filesystem paths for every file a sherpa model needs on disk.
///
/// Each recognizer family has a different layout. The enum variants
/// match the families in [`ModelKind`]: transducer models (Zipformer,
/// Parakeet-TDT) ship four files; Moonshine v1 ships five (preprocessor,
/// encoder, uncached decoder, cached decoder, tokens). The k2-fsa int8
/// release bundles use the v1 layout despite v2 being supported upstream.
#[derive(Debug, Clone)]
pub enum ModelFilePaths {
    Transducer {
        encoder: PathBuf,
        decoder: PathBuf,
        joiner: PathBuf,
        tokens: PathBuf,
    },
    Moonshine {
        preprocessor: PathBuf,
        encoder: PathBuf,
        uncached_decoder: PathBuf,
        cached_decoder: PathBuf,
        tokens: PathBuf,
    },
}

/// Returns the full paths for all files needed by a sherpa model.
///
/// The returned variant matches the model's [`ModelKind`]. The caller
/// is expected to pattern-match on the variant and pass the paths into
/// the right `sherpa_onnx` config (transducer vs moonshine).
///
/// All filename literals live in this single function so that adding a
/// new model variant means updating exactly one match — no per-file
/// helpers to forget.
pub fn model_file_paths(model: SherpaModel) -> ModelFilePaths {
    let dir = model_directory(model);
    match model {
        SherpaModel::StreamingZipformerEn => ModelFilePaths::Transducer {
            encoder: dir.join("encoder-epoch-99-avg-1-chunk-16-left-128.onnx"),
            decoder: dir.join("decoder-epoch-99-avg-1-chunk-16-left-128.onnx"),
            joiner: dir.join("joiner-epoch-99-avg-1-chunk-16-left-128.onnx"),
            tokens: dir.join("tokens.txt"),
        },
        // Moonshine v1 five-file layout (k2-fsa int8 releases): the
        // preprocessor is NOT quantized (`preprocess.onnx`, not `.int8.onnx`).
        SherpaModel::MoonshineTinyEn | SherpaModel::MoonshineBaseEn => ModelFilePaths::Moonshine {
            preprocessor: dir.join("preprocess.onnx"),
            encoder: dir.join("encode.int8.onnx"),
            uncached_decoder: dir.join("uncached_decode.int8.onnx"),
            cached_decoder: dir.join("cached_decode.int8.onnx"),
            tokens: dir.join("tokens.txt"),
        },
        // Parakeet-TDT v3 int8 layout: standard 4-file transducer
        // (encoder + decoder + joiner + tokens), structurally identical
        // to Zipformer. The `Transducer` ModelFilePaths variant is
        // reused — kind() tells the host which recognizer API to feed
        // them into (online for Zipformer vs offline for Parakeet).
        SherpaModel::ParakeetTdt06bV3En => ModelFilePaths::Transducer {
            encoder: dir.join("encoder.int8.onnx"),
            decoder: dir.join("decoder.int8.onnx"),
            joiner: dir.join("joiner.int8.onnx"),
            tokens: dir.join("tokens.txt"),
        },
    }
}

/// True if every file required by `model` exists on disk.
pub fn model_exists(model: SherpaModel) -> bool {
    match model_file_paths(model) {
        ModelFilePaths::Transducer {
            encoder,
            decoder,
            joiner,
            tokens,
        } => encoder.is_file() && decoder.is_file() && joiner.is_file() && tokens.is_file(),
        ModelFilePaths::Moonshine {
            preprocessor,
            encoder,
            uncached_decoder,
            cached_decoder,
            tokens,
        } => {
            preprocessor.is_file()
                && encoder.is_file()
                && uncached_decoder.is_file()
                && cached_decoder.is_file()
                && tokens.is_file()
        }
    }
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
///
/// # Concurrent instances (known limitation)
///
/// This function does not take a per-model filesystem lock. If two
/// `sdr-rs` instances start simultaneously on a first-run machine, they
/// can race on the scratch `.part` and `.partdir` paths and leave the
/// install corrupted. In practice this is a rare edge case — `sdr-rs`
/// is a personal-use app with one user, and the model is cached after
/// the first successful download, so subsequent launches skip this
/// function entirely. A proper fix (flock on a sentinel file via
/// `fs2` or `fslock`) is tracked in
/// <https://github.com/jasonherald/rtl-sdr/issues/255>.
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
        .timeout(Duration::from_hours(SHERPA_ARCHIVE_REQUEST_TIMEOUT_HOURS))
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

    tracing::info!(
        ?archive_path,
        ?temp_extract_dir,
        "extracting sherpa-onnx archive"
    );

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

    // Post-install scratch cleanup. If these fail AFTER the model is
    // already renamed into place, we log but don't downgrade a
    // successful install into Err — the model is installed, the
    // scratch state is recoverable by cleanup_scratch_state on next
    // launch.
    if let Err(e) = std::fs::remove_dir_all(&temp_extract_dir) {
        tracing::warn!(
            error = %e,
            ?temp_extract_dir,
            "failed to remove sherpa scratch dir (install succeeded)"
        );
    }
    if let Err(e) = std::fs::remove_file(archive_path) {
        tracing::warn!(
            error = %e,
            ?archive_path,
            "failed to remove downloaded sherpa archive (install succeeded)"
        );
    }

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
    fn silero_vad_path_is_under_sherpa_models_dir() {
        let path = silero_vad_path();
        assert!(path.ends_with("silero-vad/silero_vad.onnx"));
    }
}
