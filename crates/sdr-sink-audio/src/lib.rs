#![allow(clippy::doc_markdown, clippy::unnecessary_literal_bound)]
//! Audio output sink -- PipeWire (Linux).
//!
//! Ports SDR++ `AudioSinkModule`. Spawns a PipeWire main loop thread,
//! creates a playback stream at 48 kHz stereo f32, and feeds audio
//! from the DSP controller through a bounded channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use sdr_pipeline::sink_manager::Sink;
use sdr_types::{SinkError, Stereo};

/// Audio sample rate in Hz.
const AUDIO_SAMPLE_RATE: u32 = 48_000;

/// Number of audio channels (stereo).
const AUDIO_CHANNELS: u32 = 2;

/// Bounded channel capacity in chunks.
///
/// Each chunk is a `Vec<f32>` of interleaved stereo samples. A small bound
/// keeps latency low while preventing the DSP thread from racing ahead.
const CHANNEL_BOUND: usize = 16;

/// Bytes per sample frame (2 channels x 4 bytes per f32).
const FRAME_SIZE: usize = (AUDIO_CHANNELS as usize) * std::mem::size_of::<f32>();

/// Sentinel message sent via the PipeWire channel to request shutdown.
struct Quit;

/// Audio output sink backed by PipeWire.
///
/// Spawns a dedicated PipeWire main loop thread when started. Audio
/// samples are sent from the DSP thread via a bounded `SyncSender`.
pub struct AudioSink {
    sample_rate: f64,
    running: Arc<AtomicBool>,
    /// Sender half for audio data -- created on `start()`, cleared on `stop()`.
    tx: Option<mpsc::SyncSender<Vec<f32>>>,
    /// Sender for the PipeWire channel -- used to signal the PipeWire loop
    /// to quit cleanly (without unsafe).
    quit_tx: Option<pipewire::channel::Sender<Quit>>,
    /// Join handle for the PipeWire thread.
    pw_thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioSink {
    /// Create a new audio sink (not yet connected to PipeWire).
    pub fn new() -> Self {
        // Initialize PipeWire once (idempotent).
        pipewire::init();

        Self {
            sample_rate: f64::from(AUDIO_SAMPLE_RATE),
            running: Arc::new(AtomicBool::new(false)),
            tx: None,
            quit_tx: None,
            pw_thread: None,
        }
    }

    /// Send stereo audio samples to PipeWire for playback.
    ///
    /// Converts `&[Stereo]` to interleaved `[f32]` and pushes through the
    /// bounded channel. If the channel is full the chunk is dropped rather
    /// than blocking the DSP thread.
    ///
    /// # Errors
    ///
    /// Returns `SinkError::NotRunning` if the sink has not been started.
    pub fn write_samples(&self, samples: &[Stereo]) -> Result<(), SinkError> {
        let tx = self.tx.as_ref().ok_or(SinkError::NotRunning)?;

        // Interleave into a flat f32 vec: [L0, R0, L1, R1, ...]
        let mut buf = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            buf.push(s.l);
            buf.push(s.r);
        }

        // If the channel is full we drop this chunk rather than blocking the
        // DSP thread indefinitely. A brief audio dropout is preferable to
        // stalling the entire pipeline.
        if tx.try_send(buf).is_err() {
            tracing::debug!("audio channel full -- dropping chunk");
        }

        Ok(())
    }
}

impl Default for AudioSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for AudioSink {
    fn name(&self) -> &str {
        "Audio"
    }

    fn start(&mut self) -> Result<(), SinkError> {
        if self.running.load(Ordering::Acquire) {
            return Err(SinkError::AlreadyRunning);
        }

        let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(CHANNEL_BOUND);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Quit>();

        self.tx = Some(tx);
        self.quit_tx = Some(quit_tx);

        let running = Arc::clone(&self.running);
        running.store(true, Ordering::Release);

        let handle = std::thread::Builder::new()
            .name("pw-audio".into())
            .spawn(move || {
                if let Err(e) = pipewire_thread(rx, quit_rx) {
                    tracing::error!("PipeWire thread failed: {e}");
                }
                running.store(false, Ordering::Release);
            })
            .map_err(|e| SinkError::OpenFailed(format!("spawn PipeWire thread: {e}")))?;

        self.pw_thread = Some(handle);
        tracing::info!("audio sink started (PipeWire, {AUDIO_SAMPLE_RATE} Hz stereo f32)");
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        if !self.running.load(Ordering::Acquire) {
            return Err(SinkError::NotRunning);
        }

        // Signal the PipeWire main loop to quit via the PipeWire channel.
        if let Some(quit_tx) = self.quit_tx.take() {
            let _ = quit_tx.send(Quit);
        }

        // Drop the audio sender so the PipeWire thread sees a disconnected channel.
        self.tx = None;

        // Wait for the thread to finish.
        if let Some(handle) = self.pw_thread.take() {
            let _ = handle.join();
        }

        self.running.store(false, Ordering::Release);
        tracing::info!("audio sink stopped");
        Ok(())
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SinkError> {
        if !rate.is_finite() || rate <= 0.0 {
            return Err(SinkError::InvalidParameter(format!(
                "sample rate must be positive and finite, got {rate}"
            )));
        }
        self.sample_rate = rate;
        Ok(())
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }
}

impl Drop for AudioSink {
    fn drop(&mut self) {
        if self.running.load(Ordering::Acquire) {
            if let Some(quit_tx) = self.quit_tx.take() {
                let _ = quit_tx.send(Quit);
            }
            self.tx = None;
            if let Some(handle) = self.pw_thread.take() {
                let _ = handle.join();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PipeWire thread
// ---------------------------------------------------------------------------

/// Entry point for the PipeWire thread. Creates the main loop, context,
/// core, stream, and runs the loop until a `Quit` message arrives.
fn pipewire_thread(
    rx: mpsc::Receiver<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Quit>,
) -> Result<(), SinkError> {
    use pipewire as pw;
    use pw::spa;
    use pw::spa::pod::Pod;

    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| SinkError::OpenFailed(format!("MainLoop::new: {e}")))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| SinkError::OpenFailed(format!("Context::new: {e}")))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| SinkError::OpenFailed(format!("Context::connect: {e}")))?;

    // Attach the quit channel to the main loop. When AudioSink::stop()
    // sends a Quit message, the main loop will invoke this callback.
    let quit_loop = mainloop.clone();
    let _quit_receiver = quit_rx.attach(mainloop.loop_(), move |_: Quit| {
        quit_loop.quit();
    });

    let stream = pw::stream::StreamBox::new(
        &core,
        "sdr-audio",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_NAME => "sdr-rs",
            *pw::keys::APP_NAME => "SDR-RS",
        },
    )
    .map_err(|e| SinkError::OpenFailed(format!("Stream::new: {e}")))?;

    // -- Register the process callback --------------------------------
    let _listener = stream
        .add_local_listener_with_user_data(AudioCallbackData::new(rx))
        .process(process_callback)
        .register()
        .map_err(|e| SinkError::OpenFailed(format!("stream listener: {e}")))?;

    // -- Build the SPA audio format pod --------------------------------
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(AUDIO_SAMPLE_RATE);
    audio_info.set_channels(AUDIO_CHANNELS);

    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .map_err(|e| SinkError::OpenFailed(format!("pod serialize: {e:?}")))?
    .0
    .into_inner();

    let mut params = [Pod::from_bytes(&values)
        .ok_or_else(|| SinkError::OpenFailed("invalid format pod".into()))?];

    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| SinkError::OpenFailed(format!("stream connect: {e}")))?;

    tracing::info!("PipeWire stream connected");

    // Block in the PipeWire main loop until quit is signalled.
    mainloop.run();

    // Stream, listener, and quit receiver are dropped here.
    tracing::info!("PipeWire thread exiting");
    Ok(())
}

/// Per-callback state kept as stream user data.
struct AudioCallbackData {
    rx: mpsc::Receiver<Vec<f32>>,
    /// Leftover samples from the previous callback that did not fit in the
    /// PipeWire buffer.
    remainder: Vec<f32>,
}

impl AudioCallbackData {
    fn new(rx: mpsc::Receiver<Vec<f32>>) -> Self {
        Self {
            rx,
            remainder: Vec::new(),
        }
    }
}

/// PipeWire process callback -- runs on the PipeWire thread at real-time
/// priority. Must not block.
fn process_callback(stream: &pipewire::stream::Stream, data: &mut AudioCallbackData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };

    let datas = buffer.datas_mut();
    if datas.is_empty() {
        return;
    }

    let buf_data = &mut datas[0];
    let Some(slice) = buf_data.data() else {
        return;
    };

    let n_frames = slice.len() / FRAME_SIZE;
    let n_samples = n_frames * (AUDIO_CHANNELS as usize);

    // Drain all available chunks from the channel (non-blocking).
    while let Ok(chunk) = data.rx.try_recv() {
        data.remainder.extend_from_slice(&chunk);
    }

    // Copy samples into the PipeWire buffer.
    let available = data.remainder.len().min(n_samples);
    let (to_copy, leftover) = data.remainder.split_at(available);

    // Write interleaved f32 samples as bytes.
    for (i, &sample) in to_copy.iter().enumerate() {
        let offset = i * std::mem::size_of::<f32>();
        let end = offset + std::mem::size_of::<f32>();
        if end <= slice.len() {
            slice[offset..end].copy_from_slice(&sample.to_le_bytes());
        }
    }

    // Zero-fill the rest (silence) if we don't have enough data.
    let written_bytes = available * std::mem::size_of::<f32>();
    let total_bytes = n_frames * FRAME_SIZE;
    if written_bytes < total_bytes {
        for byte in &mut slice[written_bytes..total_bytes] {
            *byte = 0;
        }
    }

    // Keep leftover for the next callback.
    data.remainder = leftover.to_vec();

    // Update the chunk metadata.
    let chunk = buf_data.chunk_mut();
    *chunk.offset_mut() = 0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    {
        *chunk.stride_mut() = FRAME_SIZE as i32;
        *chunk.size_mut() = total_bytes as u32;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_new_does_not_panic() {
        let sink = AudioSink::new();
        assert_eq!(sink.name(), "Audio");
        assert!((sink.sample_rate() - f64::from(AUDIO_SAMPLE_RATE)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_write_before_start_returns_not_running() {
        let sink = AudioSink::new();
        let samples = [Stereo::new(0.0, 0.0)];
        assert!(
            matches!(sink.write_samples(&samples), Err(SinkError::NotRunning)),
            "write_samples should fail before start"
        );
    }

    #[test]
    fn test_set_sample_rate_validation() {
        let mut sink = AudioSink::new();
        assert!(sink.set_sample_rate(44100.0).is_ok());
        assert!(sink.set_sample_rate(-1.0).is_err());
        assert!(sink.set_sample_rate(f64::NAN).is_err());
        assert!(sink.set_sample_rate(f64::INFINITY).is_err());
    }

    #[test]
    fn test_stop_before_start_returns_not_running() {
        let mut sink = AudioSink::new();
        assert!(matches!(sink.stop(), Err(SinkError::NotRunning)));
    }
}
