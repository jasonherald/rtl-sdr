//! Background worker thread for Whisper-based live transcription.
//!
//! Receives interleaved stereo f32 audio at 48 kHz, resamples to 16 kHz mono,
//! accumulates 5-second chunks, and runs Whisper inference on non-silent chunks.

use std::sync::mpsc;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::{denoise, model, resampler};

/// Seconds of audio per transcription chunk.
const CHUNK_SECONDS: usize = 5;

/// Number of 16 kHz mono samples per chunk (16000 * 5 = 80000).
const CHUNK_SAMPLES: usize = 16_000 * CHUNK_SECONDS;

/// RMS threshold below which a chunk is treated as silence and skipped.
/// Measured AFTER the spectral noise gate, so this catches residual noise.
const SILENCE_THRESHOLD: f32 = 0.007;

/// Events emitted by the transcription worker.
#[derive(Debug, Clone)]
pub enum TranscriptionEvent {
    /// Model download in progress.
    Downloading {
        /// 0..=100
        progress_pct: u8,
    },
    /// Model loaded and ready for inference.
    Ready,
    /// Transcribed text from one chunk.
    Text {
        /// Wall-clock timestamp in "HH:MM:SS" format.
        timestamp: String,
        /// Transcribed text (trimmed, non-empty).
        text: String,
    },
    /// Fatal error — worker will exit after sending this.
    Error(String),
}

/// Main worker loop. Blocks the calling thread and exits when `audio_rx` is
/// closed (all senders dropped). Should be spawned on a dedicated thread.
///
/// # Arguments
/// * `audio_rx` — receives interleaved stereo f32 audio at 48 kHz
/// * `event_tx` — sends transcription events to the UI/consumer
pub fn run_worker(
    audio_rx: &mpsc::Receiver<Vec<f32>>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    if let Err(e) = run_worker_inner(audio_rx, event_tx) {
        let _ = event_tx.send(TranscriptionEvent::Error(e));
    }
}

/// Inner implementation that returns errors as strings so the outer function
/// can forward them as `TranscriptionEvent::Error`.
fn run_worker_inner(
    audio_rx: &mpsc::Receiver<Vec<f32>>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) -> Result<(), String> {
    // --- Model download / load ---
    let model_path = if model::model_exists() {
        tracing::info!("whisper model already present");
        model::model_path()
    } else {
        tracing::info!("whisper model not found, downloading");
        let (progress_tx, progress_rx) = mpsc::channel::<u8>();
        let event_tx_dl = event_tx.clone();

        // Forward download progress as TranscriptionEvent::Downloading.
        let progress_thread = std::thread::Builder::new()
            .name("whisper-dl-progress".into())
            .spawn(move || {
                while let Ok(pct) = progress_rx.recv() {
                    let _ = event_tx_dl.send(TranscriptionEvent::Downloading { progress_pct: pct });
                }
            })
            .map_err(|e| format!("failed to spawn progress thread: {e}"))?;

        let path = model::download_model(&progress_tx)
            .map_err(|e| format!("model download failed: {e}"))?;

        // Drop the sender so the progress thread exits.
        drop(progress_tx);
        let _ = progress_thread.join();

        path
    };

    let ctx = WhisperContext::new_with_params(&model_path, WhisperContextParameters::default())
        .map_err(|e| format!("failed to load whisper model: {e}"))?;

    let mut state = ctx
        .create_state()
        .map_err(|e| format!("failed to create whisper state: {e}"))?;

    tracing::info!("whisper model loaded, ready for inference");
    event_tx
        .send(TranscriptionEvent::Ready)
        .map_err(|_| "event channel closed before Ready".to_owned())?;

    // --- Audio loop ---
    let mut mono_buf: Vec<f32> = Vec::with_capacity(CHUNK_SAMPLES * 2);

    while let Ok(interleaved) = audio_rx.recv() {
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        while mono_buf.len() >= CHUNK_SAMPLES {
            let mut chunk: Vec<f32> = mono_buf.drain(..CHUNK_SAMPLES).collect();

            // Spectral noise gate — remove broadband static and hiss
            // before Whisper sees the audio.
            denoise::spectral_denoise(&mut chunk);

            let rms = compute_rms(&chunk);
            if rms < SILENCE_THRESHOLD {
                tracing::debug!(rms, "chunk below silence threshold, skipping");
                continue;
            }

            // Run Whisper inference.
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some("en"));
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            params.set_no_context(true);

            if let Err(e) = state.full(params, &chunk) {
                tracing::warn!("whisper inference failed: {e}");
                continue;
            }

            let n_segments = state.full_n_segments();
            let mut combined = String::new();

            for i in 0..n_segments {
                if let Some(segment) = state.get_segment(i)
                    && let Ok(text) = segment.to_str()
                {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        if !combined.is_empty() {
                            combined.push(' ');
                        }
                        combined.push_str(trimmed);
                    }
                }
            }

            if !combined.is_empty() && !is_hallucination(&combined) {
                let timestamp = chrono_timestamp();
                tracing::debug!(%timestamp, %combined, "transcribed chunk");
                let _ = event_tx.send(TranscriptionEvent::Text {
                    timestamp,
                    text: combined,
                });
            }
        }
    }

    tracing::info!("audio channel closed, worker exiting");
    Ok(())
}

/// Common hallucination phrases Whisper produces on silence/noise.
const HALLUCINATIONS: &[&str] = &[
    "thank you",
    "thanks for watching",
    "subscribe",
    "like and subscribe",
    "see you next time",
    "bye",
    "you",
    "the end",
];

/// Check if Whisper output is a known hallucination pattern.
///
/// Whisper tends to produce these when fed non-speech audio (radio static,
/// tones, data bursts). We filter them out to keep the transcript clean.
fn is_hallucination(text: &str) -> bool {
    let lower = text.to_lowercase();

    // Bracketed/parenthesized annotations Whisper generates for non-speech.
    if (lower.starts_with('[') && lower.ends_with(']'))
        || (lower.starts_with('(') && lower.ends_with(')'))
    {
        return true;
    }

    HALLUCINATIONS
        .iter()
        .any(|h| lower.trim().eq_ignore_ascii_case(h))
}

/// Compute the root-mean-square of a sample buffer.
pub fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum_sq / samples.len() as f32;
    mean.sqrt()
}

/// Return the current wall-clock time formatted as "HH:MM:SS" in local time.
///
/// Uses `libc::localtime_r` for timezone-aware formatting without pulling in
/// the `chrono` crate.
#[allow(unsafe_code)]
fn chrono_timestamp() -> String {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };

    // SAFETY: `gettimeofday` writes into the provided buffer and is
    // thread-safe. We pass null for the timezone (deprecated parameter).
    #[allow(unsafe_code)]
    let epoch = unsafe {
        libc::gettimeofday(&raw mut tv, std::ptr::null_mut());
        tv.tv_sec
    };

    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();

    // SAFETY: `localtime_r` is the reentrant (thread-safe) variant.
    // We provide a valid `time_t` and a valid output buffer.
    // Returns null on failure, in which case we fall back to UTC via `gmtime_r`.
    #[allow(unsafe_code)]
    let tm = unsafe {
        let result = libc::localtime_r(&raw const epoch, tm.as_mut_ptr());
        if result.is_null() {
            // localtime_r failed — fall back to UTC.
            libc::gmtime_r(&raw const epoch, tm.as_mut_ptr());
        }
        tm.assume_init()
    };

    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_is_zero() {
        let silence = vec![0.0_f32; 1024];
        let rms = compute_rms(&silence);
        assert!((rms - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rms_of_ones_is_one() {
        let ones = vec![1.0_f32; 1024];
        let rms = compute_rms(&ones);
        assert!((rms - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rms_of_empty_is_zero() {
        let rms = compute_rms(&[]);
        assert!((rms - 0.0).abs() < f32::EPSILON);
    }
}
