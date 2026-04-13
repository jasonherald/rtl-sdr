//! PipeWire audio output sink implementation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sdr_pipeline::sink_manager::Sink;
use sdr_types::{SinkError, Stereo};

use crate::ring::AudioRingBuffer;

/// Audio sample rate in Hz.
const AUDIO_SAMPLE_RATE: u32 = 48_000;

/// Number of audio channels (stereo).
const AUDIO_CHANNELS: u32 = 2;

/// Audio ring buffer capacity in f32 samples (interleaved stereo).
/// ~1 second at 48 kHz stereo = 96,000 samples.
const RING_CAPACITY: usize = 96_000;

/// Capacity for the stereo interleave buffer in `write_samples`.
///
/// Sized to match the ring buffer (the largest write that could ever
/// fit in the ring without dropping data) so the hot path **never**
/// reallocates: the buffer is `clear()`ed and re-pushed each call but
/// never grows beyond its initial capacity for any input within the
/// ring's natural ceiling. The previous value of 1024 silently grew
/// the Vec for any write larger than 512 stereo frames — caught by
/// CodeRabbit on PR #253 (against the parallel CoreAudio backend
/// which inherited the same constant from this file).
const INTERLEAVE_BUF_CAPACITY: usize = RING_CAPACITY;

/// Initial sample capacity for the PipeWire callback read buffer.
/// 4x typical quantum (1024 frames x 2 channels) to avoid RT reallocation.
const READ_BUF_INITIAL_SAMPLES: usize = 8_192;

/// Bytes per sample frame (2 channels x 4 bytes per f32).
const FRAME_SIZE: usize = (AUDIO_CHANNELS as usize) * std::mem::size_of::<f32>();

/// Sentinel message sent via the PipeWire channel to request shutdown.
struct Quit;

/// Audio output sink backed by PipeWire.
pub struct AudioSink {
    sample_rate: f64,
    running: Arc<AtomicBool>,
    ring: Arc<AudioRingBuffer>,
    quit_tx: Option<pipewire::channel::Sender<Quit>>,
    pw_thread: Option<std::thread::JoinHandle<()>>,
    /// Target PipeWire node name for routing (empty = system default).
    target_node: String,
    /// Pre-allocated interleave buffer for write_samples (avoids per-call alloc).
    interleave_buf: Vec<f32>,
}

/// An audio sink device with display name and PipeWire node name.
#[derive(Clone, Debug)]
pub struct AudioDevice {
    /// Human-readable name (from `node.description`).
    pub display_name: String,
    /// PipeWire node name (used for `target.object` routing).
    pub node_name: String,
}

/// Query PipeWire for available audio output sinks.
///
/// Connects briefly to the PipeWire daemon, lists all `Audio/Sink` nodes,
/// and returns their display names and node names. Always includes "Default"
/// as the first entry (routes to the system default sink).
pub fn list_audio_sinks() -> Vec<AudioDevice> {
    let mut sinks = vec![AudioDevice {
        display_name: "Default".to_string(),
        node_name: String::new(), // empty = system default
    }];

    // Run a short-lived PipeWire main loop to collect sink names.
    // Must run on a separate thread because PipeWire main loops
    // are not reentrant and we may already have one running.
    let result = std::thread::Builder::new()
        .name("pw-enumerate".to_string())
        .spawn(move || {
            let Ok(main_loop) = pipewire::main_loop::MainLoopRc::new(None) else {
                return Vec::new();
            };
            let Ok(context) = pipewire::context::ContextRc::new(&main_loop, None) else {
                return Vec::new();
            };
            let Ok(core) = context.connect(None) else {
                return Vec::new();
            };
            let Ok(registry) = core.get_registry() else {
                return Vec::new();
            };

            let found_sinks = std::rc::Rc::new(std::cell::RefCell::new(Vec::<AudioDevice>::new()));
            let found_clone = std::rc::Rc::clone(&found_sinks);

            // Listen for global objects — Audio/Sink nodes are output devices
            let listener = registry
                .add_listener_local()
                .global(move |global| {
                    if let Some(props) = global.props {
                        let media_class = props.get("media.class").unwrap_or("");
                        if media_class == "Audio/Sink" {
                            let display_name = props
                                .get("node.description")
                                .or_else(|| props.get("node.name"))
                                .unwrap_or("Unknown Sink")
                                .to_string();
                            let node_name = props.get("node.name").unwrap_or("unknown").to_string();
                            found_clone.borrow_mut().push(AudioDevice {
                                display_name,
                                node_name,
                            });
                        }
                    }
                })
                .register();

            // Listen for the "done" signal — fires after all globals are enumerated
            let ml_quit = main_loop.downgrade();
            let core_listener = core
                .add_listener_local()
                .done(move |_id, _seq| {
                    if let Some(ml) = ml_quit.upgrade() {
                        ml.quit();
                    }
                })
                .register();

            // Trigger sync — done callback fires after all globals are sent
            core.sync(0).ok();
            main_loop.run();

            // Listeners must stay alive until after run() completes
            drop(listener);
            drop(core_listener);
            found_sinks.borrow().clone()
        })
        .and_then(|handle| {
            handle
                .join()
                .map_err(|_| std::io::Error::other("join failed"))
        });

    if let Ok(found) = result {
        for dev in found {
            if !sinks.iter().any(|s| s.node_name == dev.node_name) {
                sinks.push(dev);
            }
        }
    }

    sinks
}

impl AudioSink {
    /// Create a new audio sink (not yet connected to PipeWire).
    pub fn new() -> Self {
        pipewire::init();

        Self {
            sample_rate: f64::from(AUDIO_SAMPLE_RATE),
            running: Arc::new(AtomicBool::new(false)),
            ring: Arc::new(AudioRingBuffer::new(RING_CAPACITY)),
            quit_tx: None,
            pw_thread: None,
            target_node: String::new(),
            interleave_buf: Vec::with_capacity(INTERLEAVE_BUF_CAPACITY),
        }
    }

    /// Set the target audio device by node name.
    ///
    /// Call before `start()` to route to a specific sink. If the sink is
    /// already running, it will be restarted with the new target.
    ///
    /// Pass an empty string for the system default sink.
    pub fn set_target(&mut self, node_name: &str) -> Result<(), SinkError> {
        let had_state = self.pw_thread.is_some();
        if had_state {
            self.stop()?;
        }
        self.target_node.clear();
        self.target_node.push_str(node_name);
        if had_state {
            self.start()?;
        }
        Ok(())
    }

    /// Send stereo audio samples to PipeWire for playback.
    ///
    /// The interleave buffer is pre-sized to `INTERLEAVE_BUF_CAPACITY`
    /// (= `RING_CAPACITY` f32s = ~48,000 stereo frames). For any input
    /// up to that ceiling the call is **allocation-free**. Inputs
    /// larger than that ceiling would also overflow the ring buffer
    /// itself, so they're a contract violation; debug builds
    /// `debug_assert!` to catch this loudly, release builds let
    /// `Vec::push` reallocate once and then proceed with the larger
    /// capacity (a one-time cost we accept as graceful degradation
    /// rather than dropping samples).
    ///
    /// # Errors
    ///
    /// Returns `SinkError::NotRunning` if the sink has not been started.
    /// Returns `SinkError::Disconnected` if the PipeWire thread has exited.
    pub fn write_samples(&mut self, samples: &[Stereo]) -> Result<(), SinkError> {
        if !self.running.load(Ordering::Acquire) {
            return Err(SinkError::NotRunning);
        }

        debug_assert!(
            samples.len() * 2 <= INTERLEAVE_BUF_CAPACITY,
            "write_samples called with {} stereo frames, exceeds interleave buffer capacity {} (would overflow the ring buffer too)",
            samples.len(),
            INTERLEAVE_BUF_CAPACITY / 2
        );

        // Interleave stereo into pre-allocated buffer (zero allocation
        // under normal sizing).
        self.interleave_buf.clear();
        for s in samples {
            self.interleave_buf.push(s.l);
            self.interleave_buf.push(s.r);
        }

        self.ring.write(&self.interleave_buf);
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
        self.ring.clear();

        let (quit_tx, quit_rx) = pipewire::channel::channel::<Quit>();

        let running = Arc::clone(&self.running);
        let ring = Arc::clone(&self.ring);
        let target = self.target_node.clone();

        // Set running BEFORE spawn so write_samples can start queueing.
        // Roll back on spawn failure.
        self.running.store(true, Ordering::Release);

        let handle = match std::thread::Builder::new()
            .name("pw-audio".into())
            .spawn(move || {
                if let Err(e) = pipewire_thread(ring, quit_rx, &target) {
                    tracing::error!("PipeWire thread failed: {e}");
                }
                running.store(false, Ordering::Release);
            }) {
            Ok(h) => h,
            Err(e) => {
                self.running.store(false, Ordering::Release);
                return Err(SinkError::OpenFailed(format!("spawn PipeWire thread: {e}")));
            }
        };

        self.quit_tx = Some(quit_tx);
        self.pw_thread = Some(handle);
        tracing::info!("audio sink started (PipeWire, {AUDIO_SAMPLE_RATE} Hz stereo f32)");
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        // Always clean up if handles exist, regardless of `running` flag.
        // The worker thread may have exited on its own (setting running=false),
        // but we still need to join and drop channels.
        let had_state = self.pw_thread.is_some();

        if let Some(quit_tx) = self.quit_tx.take() {
            let _ = quit_tx.send(Quit);
        }
        if let Some(handle) = self.pw_thread.take() {
            let _ = handle.join();
        }

        self.running.store(false, Ordering::Release);
        self.ring.clear();

        if had_state {
            tracing::info!("audio sink stopped");
            Ok(())
        } else {
            Err(SinkError::NotRunning)
        }
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SinkError> {
        if !rate.is_finite() || rate <= 0.0 {
            return Err(SinkError::InvalidParameter(format!(
                "sample rate must be positive and finite, got {rate}"
            )));
        }
        #[allow(clippy::cast_lossless)]
        if (rate - f64::from(AUDIO_SAMPLE_RATE)).abs() > f64::EPSILON {
            return Err(SinkError::InvalidParameter(format!(
                "only {AUDIO_SAMPLE_RATE} Hz output is currently supported, got {rate}"
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
            if let Some(handle) = self.pw_thread.take() {
                let _ = handle.join();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PipeWire thread
// ---------------------------------------------------------------------------

fn pipewire_thread(
    ring: Arc<AudioRingBuffer>,
    quit_rx: pipewire::channel::Receiver<Quit>,
    target_node: &str,
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

    let quit_loop = mainloop.clone();
    let _quit_receiver = quit_rx.attach(mainloop.loop_(), move |_: Quit| {
        quit_loop.quit();
    });

    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => "sdr-rs",
        *pw::keys::APP_NAME => "SDR-RS",
    };
    // Route to a specific sink if requested (empty = system default)
    if !target_node.is_empty() {
        props.insert("target.object", target_node);
        tracing::info!(target = target_node, "routing audio to specific sink");
    }

    let stream = pw::stream::StreamBox::new(&core, "sdr-audio", props)
        .map_err(|e| SinkError::OpenFailed(format!("Stream::new: {e}")))?;

    let _listener = stream
        .add_local_listener_with_user_data(AudioCallbackData::new(ring))
        .process(process_callback)
        .register()
        .map_err(|e| SinkError::OpenFailed(format!("stream listener: {e}")))?;

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
    mainloop.run();
    tracing::info!("PipeWire thread exiting");
    Ok(())
}

struct AudioCallbackData {
    ring: Arc<AudioRingBuffer>,
    /// Pre-allocated read buffer — sized for PipeWire's largest request.
    read_buf: Vec<f32>,
}

impl AudioCallbackData {
    fn new(ring: Arc<AudioRingBuffer>) -> Self {
        Self {
            ring,
            // 48 kHz stereo = 96,000 samples/sec. Typical PipeWire quantum is
            // 1024 frames = 2048 samples. Allocate for 4x that to handle any
            // reasonable quantum size without RT reallocation.
            read_buf: vec![0.0; READ_BUF_INITIAL_SAMPLES],
        }
    }
}

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

    // Read from ring buffer into pre-allocated buffer.
    // Grow the buffer if PipeWire requests more than expected (rare, only
    // happens once per new quantum size — not a per-frame allocation).
    if data.read_buf.len() < n_samples {
        data.read_buf.resize(n_samples, 0.0);
    }
    let available = data.ring.read(&mut data.read_buf[..n_samples]);

    for (i, &sample) in data.read_buf[..available].iter().enumerate() {
        let offset = i * std::mem::size_of::<f32>();
        let end = offset + std::mem::size_of::<f32>();
        if end <= slice.len() {
            slice[offset..end].copy_from_slice(&sample.to_le_bytes());
        }
    }

    let written_bytes = available * std::mem::size_of::<f32>();
    let total_bytes = n_frames * FRAME_SIZE;
    if written_bytes < total_bytes {
        for byte in &mut slice[written_bytes..total_bytes] {
            *byte = 0;
        }
    }

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
        let mut sink = AudioSink::new();
        let samples = [Stereo::new(0.0, 0.0)];
        assert!(
            matches!(sink.write_samples(&samples), Err(SinkError::NotRunning)),
            "write_samples should fail before start"
        );
    }

    #[test]
    fn test_set_sample_rate_validation() {
        let mut sink = AudioSink::new();
        assert!(sink.set_sample_rate(f64::from(AUDIO_SAMPLE_RATE)).is_ok());
        assert!(sink.set_sample_rate(44100.0).is_err());
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
