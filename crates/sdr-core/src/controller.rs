//! DSP thread bridge — owns all backend DSP objects and routes commands /
//! events between the UI consumer and the signal pipeline.
//!
//! Moved verbatim from `crates/sdr-ui/src/dsp_controller.rs` as part of the
//! `sdr-core` extraction (M1, see `docs/superpowers/specs/2026-04-12-sdr-core-extraction-design.md`).
//! The previous in-tree path is now owned here; the GTK UI consumes this
//! module through the [`crate::engine::Engine`] facade rather than calling
//! `spawn_dsp_thread` directly.
//!
//! The DSP thread runs a loop that:
//! 1. Checks for UI commands (non-blocking when running, blocking when stopped).
//! 2. Reads IQ samples from the active source via `Source::read_samples`.
//! 3. Processes samples through `IqFrontend` (decimation, DC blocking, FFT).
//! 4. Processes through `RxVfo` (frequency translation, resampling, channel filter).
//! 5. Processes through `RadioModule` (IF chain, demod, AF chain).
//! 6. Publishes FFT data into the [`crate::fft_buffer::SharedFftBuffer`].

use std::sync::mpsc;
use std::time::Duration;

use sdr_dsp::channel::RxVfo;
use sdr_pipeline::iq_frontend::{FftWindow, IqFrontend};
use sdr_pipeline::sink_manager::Sink;
use sdr_pipeline::source_manager::Source;
use sdr_radio::RadioModule;
use sdr_sink_audio::AudioSink;
use sdr_source_rtlsdr::RtlSdrSource;
use sdr_types::{Complex, RtlTcpConnectionState, SinkError, Stereo};

use crate::fft_buffer::SharedFftBuffer;
use crate::messages::{DspToUi, SourceType, UiToDsp};
use crate::wav_writer::WavWriter;

/// Number of IQ sample pairs per USB bulk read.
const IQ_PAIRS_PER_READ: usize = 16_384;

/// Default FFT size for spectrum display.
const DEFAULT_FFT_SIZE: usize = 2048;

/// How often to emit the diagnostic `pipeline rates` log line.
/// Short enough that a regression shows up within a few seconds
/// of starting playback, long enough that the log doesn't flood
/// on busy UIs. Controller-local constant so both the reset
/// (on `Start`) and the emission site agree without a magic
/// number in either place.
const DIAG_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Default FFT display rate in FPS (matches SDR++ default of 20).
/// Lower rate reduces Mesa GL driver memory pressure from per-frame
/// buffer uploads.
const DEFAULT_FFT_RATE: f64 = 20.0;

/// Default sample rate in Hz (2.0 Msps).
/// With decimation 8, effective rate = 250 kHz, matching WFM IF exactly.
/// This avoids the input resampler entirely for WFM.
const DEFAULT_SAMPLE_RATE: f64 = 2_000_000.0;

/// Default decimation ratio (2.0M / 8 = 250 kHz effective rate).
const DEFAULT_DECIMATION: u32 = 8;

/// Default center frequency in Hz (100 MHz — FM broadcast).
const DEFAULT_CENTER_FREQ: f64 = 100_000_000.0;

/// Timeout for blocking `recv` when the pipeline is stopped (ms).
const RECV_TIMEOUT_MS: u64 = 50;

/// Padding added to VFO output buffer to handle resampler edge effects.
const VFO_OUTPUT_PADDING: usize = 64;

/// RTL-SDR device index to open.
const DEVICE_INDEX: u32 = 0;

/// Audio recording sample rate in Hz (matches `PipeWire` output).
const AUDIO_SAMPLE_RATE: u32 = 48_000;

/// Audio recording channel count (stereo).
const AUDIO_CHANNELS: u16 = 2;

/// IQ recording channel count (I + Q).
const IQ_CHANNELS: u16 = 2;

/// Spawn the DSP controller thread.
///
/// The thread owns all backend DSP objects and communicates with the UI
/// via `ui_rx` (commands from UI) and `dsp_tx` (data/status to UI). FFT
/// frames are published into `fft_shared` directly to avoid per-frame
/// allocation across thread boundaries.
///
/// Returns the spawned [`std::thread::JoinHandle`] so callers can join on
/// shutdown. The DSP thread exits when `ui_rx` is dropped.
///
/// `pub(crate)`: only [`crate::engine::Engine`] calls this. External
/// consumers go through the `Engine` facade.
pub(crate) fn spawn_dsp_thread(
    dsp_tx: mpsc::Sender<DspToUi>,
    ui_rx: mpsc::Receiver<UiToDsp>,
    fft_shared: std::sync::Arc<SharedFftBuffer>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("dsp-controller".into())
        .spawn(move || {
            dsp_thread_main(dsp_tx, ui_rx, fft_shared);
        })
}

/// Main function for the DSP controller thread.
///
/// Runs until the `ui_rx` channel is disconnected (UI closed).
#[allow(clippy::needless_pass_by_value)]
fn dsp_thread_main(
    dsp_tx: mpsc::Sender<DspToUi>,
    ui_rx: mpsc::Receiver<UiToDsp>,
    fft_shared: std::sync::Arc<SharedFftBuffer>,
) {
    tracing::info!("DSP controller thread started");

    let mut state = match DspState::new() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to initialize DSP state: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("DSP init failed: {e}")));
            return;
        }
    };

    loop {
        if state.running {
            // Non-blocking: drain all pending commands.
            loop {
                match ui_rx.try_recv() {
                    Ok(cmd) => handle_command(&mut state, &dsp_tx, cmd),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        tracing::info!("UI channel disconnected — DSP thread exiting");
                        cleanup(&mut state);
                        return;
                    }
                }
            }

            // Read and process one IQ block.
            process_iq_block(&mut state, &dsp_tx, &fft_shared);
            // Edge-emit rtl_tcp connection-state changes. Poll is
            // time-throttled inside the helper so at ~106 Hz block
            // cadence we only hit the source's state mutex twice a
            // second.
            poll_rtl_tcp_connection_state(&mut state, &dsp_tx);
        } else {
            // Pipeline stopped — block with timeout to avoid busy-waiting.
            match ui_rx.recv_timeout(Duration::from_millis(RECV_TIMEOUT_MS)) {
                Ok(cmd) => handle_command(&mut state, &dsp_tx, cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::info!("UI channel disconnected — DSP thread exiting");
                    return;
                }
            }
        }
    }
}

/// Poll cadence for the `rtl_tcp` connection-state check. 500 ms
/// matches the UI-side stats poll on the server panel and is fast
/// enough that "Connecting → Connected" transitions feel
/// instantaneous while keeping the per-tick state-mutex lock off
/// the IQ-block hot path.
const RTL_TCP_STATE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Poll the active source's projected `rtl_tcp_connection_state()`
/// and emit `DspToUi::RtlTcpConnectionState` on edge (state changed
/// since last emit). Throttled via `state.rtl_tcp_poll_at`.
///
/// Non-`RtlTcp` sources return `None` from the trait method — we
/// map that to `Disconnected` so the UI can track the absence
/// uniformly (source-type change → status row collapses without a
/// separate teardown signal).
fn poll_rtl_tcp_connection_state(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    let now = std::time::Instant::now();
    if now < state.rtl_tcp_poll_at {
        return;
    }
    state.rtl_tcp_poll_at = now + RTL_TCP_STATE_POLL_INTERVAL;

    let current = state
        .source
        .as_ref()
        .and_then(|s| s.rtl_tcp_connection_state())
        .unwrap_or(RtlTcpConnectionState::Disconnected);

    // `RtlTcpConnectionState` derives PartialEq; Retrying variants
    // with a different `retry_in` compare unequal, so the poll
    // emits twice a second during the backoff wait. That's what we
    // want — the UI renders a live countdown without the status
    // text going stale between attempt-counter bumps.
    if state.last_rtl_tcp_state != current {
        state.last_rtl_tcp_state = current.clone();
        let _ = dsp_tx.send(DspToUi::RtlTcpConnectionState(current));
    }
}

/// Mutable state owned by the DSP thread.
///
/// This is a god-struct that holds every piece of DSP-thread state by
/// design — the DSP thread owns everything exclusively. The
/// `struct_excessive_bools` lint triggers at 4 bools (`running`,
/// `dc_blocking`, `invert_iq`, `squelch_was_open`); splitting them
/// into an enum state machine would be a significant refactor for
/// zero runtime benefit, so suppress locally.
#[allow(clippy::struct_excessive_bools)]
struct DspState {
    source: Option<Box<dyn Source>>,
    frontend: IqFrontend,
    radio: RadioModule,
    audio_sink: AudioSink,
    running: bool,
    center_freq: f64,
    sample_rate: f64,
    volume: f32,

    // Persisted frontend settings (restored after rebuild)
    dc_blocking: bool,
    invert_iq: bool,
    window_fn: FftWindow,
    fft_rate: f64,
    /// Current channel bandwidth (persisted so VFO rebuilds use it, not mode default).
    bandwidth: f64,

    // RxVFO — frequency translation + resampling + channel filter
    vfo: Option<RxVfo>,
    vfo_buf: Vec<Complex>,
    vfo_offset: f64,

    // Source type and configuration
    /// User-configured sample rate (persisted across source switches).
    configured_sample_rate: f64,
    source_type: SourceType,
    network_host: String,
    network_port: u16,
    network_protocol: sdr_types::Protocol,
    file_path: std::path::PathBuf,

    // Pre-allocated buffers
    iq_buf: Vec<Complex>,
    processed_buf: Vec<Complex>,
    fft_buf: Vec<f32>,
    audio_buf: Vec<Stereo>,

    // Recording state
    audio_writer: Option<WavWriter>,
    iq_writer: Option<WavWriter>,

    /// Transcription audio tap — when Some, audio is copied to this channel.
    transcription_tx: Option<std::sync::mpsc::SyncSender<sdr_transcription::TranscriptionInput>>,

    /// Generic audio tap — when Some, post-demod audio is downsampled
    /// to 16 kHz mono f32 and dropped into this channel. Distinct
    /// from `transcription_tx` so FFI consumers (e.g. the macOS
    /// `SpeechAnalyzer` driver for issue #314) can receive
    /// recognizer-ready samples without the sdr-transcription
    /// dependency cross-compiling into the FFI surface.
    audio_tap_tx: Option<std::sync::mpsc::SyncSender<Vec<f32>>>,

    /// Decimation phase carried across `stereo_48k_to_mono_16k`
    /// calls on the audio tap path. Without it, successive DSP
    /// blocks whose lengths aren't multiples of 3 would produce
    /// duplicate / dropped samples at block boundaries. Reset on
    /// `EnableAudioTap` so a fresh session starts at phase 0. Per
    /// `CodeRabbit` round 1 on PR #349.
    audio_tap_phase: usize,

    /// Last known squelch gate state, used to detect open/close edge
    /// transitions so we only emit one `SquelchOpened` / `SquelchClosed`
    /// event per transition instead of one per audio chunk. Initialized
    /// to `false` (matches `IfChain`'s initial closed state).
    squelch_was_open: bool,

    /// Last observed CTCSS sustained-gate state, used to emit
    /// `DspToUi::CtcssSustainedChanged` only on edges so the UI
    /// status indicator can subscribe without the channel being
    /// flooded at detector-window rate. Initialized to `false` to
    /// match the detector's initial closed state.
    ctcss_was_sustained: bool,

    /// Diagnostic: total stereo frames handed to the audio sink
    /// since the last `Start`. Paired with `diag_log_at` to emit
    /// a periodic `info` log so we can confirm the pipeline is
    /// actually producing audio without flooding the log every
    /// DSP block.
    audio_frames_written: u64,
    /// Diagnostic: total IQ samples read from the source since
    /// the last `Start`. Logged alongside `audio_frames_written`
    /// so the ratio (expected: `source_sample_rate /
    /// audio_sample_rate`) makes USB-vs-DSP bottlenecks visible.
    iq_samples_read: u64,
    /// Next wall-clock deadline for the periodic diagnostic log.
    diag_log_at: std::time::Instant,

    /// Last observed voice-squelch open state. Mirrors the CTCSS
    /// tracker pattern — we only emit edge events, and the UI
    /// status indicator subscribes to those. The initial value
    /// intentionally starts as `true` to match the `Off` default
    /// (gate permanently open); the first real edge fires when
    /// the user picks Syllabic or Snr and the fresh detector
    /// reports closed.
    voice_squelch_was_open: bool,

    /// Last emitted `rtl_tcp` connection state. Edge-filters the
    /// `DspToUi::RtlTcpConnectionState` emissions so we don't
    /// flood the channel at poll cadence when the state is static
    /// (Connected for a long session, Retrying between attempts,
    /// etc.). Initialized to `Disconnected` — matches the initial
    /// UI render and the state of a freshly-constructed
    /// `RtlTcpSource` before its first `start()`.
    last_rtl_tcp_state: RtlTcpConnectionState,
    /// Next wall-clock deadline for polling the active source's
    /// connection state. We poll at ~2 Hz (500 ms) rather than on
    /// every IQ block because the underlying state is a
    /// `Mutex<ConnectionState>` lock — cheap but not free, and the
    /// UI cadence doesn't need sub-second resolution to render the
    /// "Connecting… / Connected / Retrying in N s" text.
    rtl_tcp_poll_at: std::time::Instant,
}

impl DspState {
    fn new() -> Result<Self, String> {
        let frontend = IqFrontend::new(
            DEFAULT_SAMPLE_RATE,
            DEFAULT_DECIMATION,
            DEFAULT_FFT_SIZE,
            FftWindow::Nuttall,
            true, // DC blocking on by default
        )
        .map_err(|e| format!("IqFrontend init: {e}"))?;

        let radio =
            RadioModule::with_default_rate().map_err(|e| format!("RadioModule init: {e}"))?;
        let initial_bandwidth = radio.demod_config().default_bandwidth;

        // The RxVfo and RadioModule input rate are configured in open_source()
        // once we know the actual effective sample rate from the hardware.

        Ok(Self {
            source: None,
            frontend,
            radio,
            audio_sink: AudioSink::new(),
            running: false,
            center_freq: DEFAULT_CENTER_FREQ,
            sample_rate: DEFAULT_SAMPLE_RATE,
            configured_sample_rate: DEFAULT_SAMPLE_RATE,
            volume: 1.0,
            dc_blocking: true,
            invert_iq: false,
            window_fn: FftWindow::Nuttall,
            fft_rate: DEFAULT_FFT_RATE,
            bandwidth: initial_bandwidth,
            vfo: None,
            vfo_buf: Vec::new(),
            vfo_offset: 0.0,
            source_type: SourceType::RtlSdr,
            network_host: "127.0.0.1".to_string(),
            network_port: 1234,
            network_protocol: sdr_types::Protocol::TcpClient,
            file_path: std::path::PathBuf::new(),
            iq_buf: vec![Complex::default(); IQ_PAIRS_PER_READ],
            processed_buf: vec![Complex::default(); IQ_PAIRS_PER_READ],
            fft_buf: vec![0.0; DEFAULT_FFT_SIZE],
            audio_buf: Vec::new(),
            audio_writer: None,
            iq_writer: None,
            transcription_tx: None,
            audio_tap_tx: None,
            audio_tap_phase: 0,
            squelch_was_open: false,
            ctcss_was_sustained: false,
            voice_squelch_was_open: true,
            audio_frames_written: 0,
            iq_samples_read: 0,
            diag_log_at: std::time::Instant::now(),
            last_rtl_tcp_state: RtlTcpConnectionState::Disconnected,
            rtl_tcp_poll_at: std::time::Instant::now(),
        })
    }
}

/// Handle a single UI command.
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn handle_command(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, cmd: UiToDsp) {
    match cmd {
        UiToDsp::Start => {
            if state.running {
                tracing::warn!("start requested but already running");
                return;
            }
            tracing::info!("starting DSP pipeline");
            state.audio_frames_written = 0;
            state.iq_samples_read = 0;
            state.diag_log_at = std::time::Instant::now();
            match open_source(state) {
                Ok(()) => {
                    // Start the audio sink -- if it fails, log but continue
                    // so the spectrum display still works.
                    if let Err(e) = state.audio_sink.start() {
                        tracing::warn!("audio sink failed to start (spectrum still works): {e}");
                        let _ = dsp_tx.send(DspToUi::Error(format!("Audio output failed: {e}")));
                    }
                    state.running = true;
                    tracing::info!("DSP pipeline started");

                    // Send display bandwidth (raw sample rate) so
                    // the spectrum display shows the full tuner
                    // bandwidth. The FFT is computed on the pre-
                    // decimation stream (see
                    // `crates/sdr-pipeline/src/iq_frontend.rs:156`),
                    // so bins span `sample_rate()`, not
                    // `effective_sample_rate()`.
                    let _ = dsp_tx.send(DspToUi::DisplayBandwidth(state.frontend.sample_rate()));

                    // Send the source's display name + supported gain
                    // values to the UI.
                    if let Some(source) = &state.source {
                        let _ = dsp_tx.send(DspToUi::DeviceInfo(source.name().to_string()));
                        let gains: Vec<f64> = source
                            .gains()
                            .iter()
                            .map(|&g| f64::from(g) / 10.0) // tenths of dB → dB
                            .collect();
                        if !gains.is_empty() {
                            let _ = dsp_tx.send(DspToUi::GainList(gains));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("failed to start source: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("Start failed: {e}")));
                    let _ = dsp_tx.send(DspToUi::SourceStopped);
                }
            }
        }

        UiToDsp::Stop => {
            if !state.running {
                tracing::warn!("stop requested but not running");
                return;
            }
            tracing::info!("stopping DSP pipeline");
            // Disconnect transcription tap so the worker stops receiving audio.
            state.transcription_tx = None;
            // Same treatment for the generic audio tap — the DSP pipeline
            // is tearing down and any registered FFI consumer is about to
            // see a `Disconnected` on their next pull regardless.
            state.audio_tap_tx = None;
            cleanup(state);
            state.running = false;
            let _ = dsp_tx.send(DspToUi::SourceStopped);
        }

        UiToDsp::Tune(freq) => {
            tracing::debug!(frequency_hz = freq, "tune command");
            state.center_freq = freq;
            if let Some(source) = &mut state.source
                && let Err(e) = source.tune(freq)
            {
                tracing::warn!("tune failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Tune failed: {e}")));
            }
        }

        UiToDsp::SetDemodMode(mode) => {
            tracing::debug!(?mode, "set demod mode");
            let old_mode = state.radio.current_mode();
            if let Err(e) = state.radio.set_mode(mode) {
                tracing::warn!("set demod mode failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Mode switch failed: {e}")));
            } else {
                // Reset bandwidth to the new mode's default.
                state.bandwidth = state.radio.demod_config().default_bandwidth;

                // Auto-adjust decimation for the new demod's IF rate.
                let if_rate = state.radio.demod_config().if_sample_rate;
                let auto_decim = auto_decimation_ratio(state.sample_rate, if_rate);
                if auto_decim != state.frontend.decim_ratio() {
                    tracing::info!(auto_decim, if_rate, "auto-adjusting decimation for mode");
                    if let Err(e) = state.frontend.set_decimation(auto_decim) {
                        tracing::warn!("auto-decimation on mode switch failed: {e}");
                    }
                }

                // Rebuild the RxVfo for the new demod's IF rate and bandwidth.
                if let Err(e) = rebuild_vfo(state) {
                    tracing::warn!("VFO rebuild on mode switch failed: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("VFO rebuild failed: {e}")));
                }
                let _ = dsp_tx.send(DspToUi::SampleRateChanged(
                    state.frontend.effective_sample_rate(),
                ));
                let _ = dsp_tx.send(DspToUi::DisplayBandwidth(state.frontend.sample_rate()));

                // Notify the UI of the mode transition (edge detection — only
                // when the mode actually changed so idempotent refreshes do not
                // trigger the transcript-session boundary logic).
                //
                // The UI layer's response to `DemodModeChanged` is to toggle
                // the transcription enable row off, which eventually drops
                // the transcription channel via `DisableTranscription`. That
                // round-trip is async — until it completes the DSP thread
                // would otherwise keep pushing post-switch audio into the old
                // session, violating the "band change = hard session
                // boundary" contract in the Auto Break design spec. Drop the
                // tap locally FIRST so no post-switch samples leak into the
                // old backend, then notify the UI. The UI's eventual
                // `DisableTranscription` is idempotent on an already-cleared
                // tap.
                if old_mode != mode {
                    state.transcription_tx = None;
                    // Same hard-boundary treatment for the generic
                    // audio tap. The SpeechAnalyzer session on the
                    // Mac side treats every mode change as an
                    // utterance boundary — letting post-switch
                    // audio leak into the old session until the
                    // UI round-trip sends DisableAudioTap would
                    // corrupt the transcript across the mode
                    // transition. Per CodeRabbit round 1 on PR
                    // #349.
                    state.audio_tap_tx = None;
                    // Reset the decimation phase so a subsequent
                    // EnableAudioTap starts at a clean 3:1
                    // alignment instead of carrying a stale phase
                    // from before the mode switch.
                    state.audio_tap_phase = 0;
                    state.squelch_was_open = false;
                    // Mode switch rebuilds the AF chain + CTCSS
                    // detector + voice squelch — edge trackers
                    // must match the new closed state.
                    state.ctcss_was_sustained = false;
                    // Voice squelch reset to closed in an active
                    // mode; in Off mode it's still "open" so the
                    // tracker should track whatever the AF chain
                    // reports after the rebuild. Simpler to just
                    // snapshot it here and let the next process
                    // iteration emit an edge if anything changed.
                    state.voice_squelch_was_open = state.radio.voice_squelch_open();
                    let _ = dsp_tx.send(DspToUi::DemodModeChanged(mode));
                }
            }
        }

        UiToDsp::SetBandwidth(bw) => {
            tracing::debug!(bandwidth_hz = bw, "set bandwidth");
            // Update the VFO channel filter first; only persist on success.
            if let Some(vfo) = &mut state.vfo {
                match vfo.set_bandwidth(bw) {
                    Ok(()) => state.bandwidth = bw,
                    Err(e) => {
                        tracing::warn!("VFO bandwidth update failed: {e}");
                        let _ = dsp_tx.send(DspToUi::Error(format!("Bandwidth failed: {e}")));
                    }
                }
            } else {
                state.bandwidth = bw;
            }
            // Also pass to the radio module (some demods use it internally).
            state.radio.set_bandwidth(bw);
            // Notify UI so widgets that initiate bandwidth changes
            // via a different path (VFO drag handles on the
            // spectrum) can reflect the new value in the Radio
            // panel's bandwidth spin row. The `bandwidth_row`'s
            // own `set_value` path guards against feedback loops
            // via a `suppress_notify` flag on the UI side.
            let _ = dsp_tx.send(DspToUi::BandwidthChanged(state.bandwidth));
        }

        UiToDsp::SetSquelch(level) => {
            tracing::debug!(squelch_db = level, "set squelch level");
            state.radio.set_squelch(level);
        }

        UiToDsp::SetSquelchEnabled(enabled) => {
            tracing::debug!(enabled, "set squelch enabled");
            state.radio.set_squelch_enabled(enabled);
        }

        UiToDsp::SetAutoSquelch(enabled) => {
            tracing::debug!(enabled, "set auto-squelch");
            state.radio.set_auto_squelch_enabled(enabled);
        }

        UiToDsp::SetVolume(vol) => {
            tracing::debug!(volume = vol, "set volume");
            state.volume = vol;
        }

        UiToDsp::SetDeemphasis(mode) => {
            tracing::debug!(?mode, "set deemphasis");
            if let Err(e) = state.radio.set_deemp_mode(mode) {
                tracing::warn!("set deemphasis failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Deemphasis failed: {e}")));
            }
        }

        UiToDsp::SetSampleRate(rate) => {
            tracing::debug!(sample_rate = rate, "set sample rate");
            state.configured_sample_rate = rate;
            if let Some(source) = &mut state.source {
                if let Err(e) = source.set_sample_rate(rate) {
                    tracing::warn!("set sample rate failed: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("Sample rate failed: {e}")));
                    return;
                }
                // Use the source's actual rate (may differ due to hardware rounding)
                state.sample_rate = source.sample_rate();
            } else {
                state.sample_rate = rate;
            }

            // Auto-select decimation ratio so the effective rate is close to
            // the demod IF rate. This prevents the VFO from having to process
            // all raw samples when the sample rate is much higher than needed.
            let if_rate = state.radio.demod_config().if_sample_rate;
            let auto_decim = auto_decimation_ratio(rate, if_rate);
            if auto_decim != state.frontend.decim_ratio() {
                tracing::info!(
                    sample_rate = rate,
                    auto_decim,
                    effective = rate / f64::from(auto_decim),
                    "auto-adjusting decimation for sample rate"
                );
                if let Err(e) = state.frontend.set_decimation(auto_decim) {
                    tracing::warn!("auto-decimation failed: {e}");
                }
            }

            match rebuild_frontend(state) {
                Ok(()) => {
                    if let Err(e) = rebuild_vfo(state) {
                        tracing::warn!("VFO rebuild on sample rate change failed: {e}");
                        let _ = dsp_tx.send(DspToUi::Error(format!("VFO rebuild failed: {e}")));
                    }
                    let _ = dsp_tx.send(DspToUi::SampleRateChanged(
                        state.frontend.effective_sample_rate(),
                    ));
                    let _ = dsp_tx.send(DspToUi::DisplayBandwidth(state.frontend.sample_rate()));
                }
                Err(e) => {
                    tracing::warn!("frontend rebuild failed: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("Frontend rebuild: {e}")));
                }
            }
        }

        UiToDsp::SetDecimation(ratio) => {
            tracing::debug!(ratio, "set decimation");
            if let Err(e) = state.frontend.set_decimation(ratio) {
                tracing::warn!("set decimation failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Decimation failed: {e}")));
            } else {
                // Rebuild VFO for the new effective sample rate.
                if let Err(e) = rebuild_vfo(state) {
                    tracing::warn!("VFO rebuild on decimation change failed: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("VFO rebuild failed: {e}")));
                }
                let _ = dsp_tx.send(DspToUi::SampleRateChanged(
                    state.frontend.effective_sample_rate(),
                ));
                let _ = dsp_tx.send(DspToUi::DisplayBandwidth(state.frontend.sample_rate()));
            }
        }

        UiToDsp::SetDcBlocking(enabled) => {
            tracing::debug!(enabled, "set DC blocking");
            state.dc_blocking = enabled;
            if let Err(e) = state.frontend.set_dc_blocking(enabled) {
                tracing::warn!("set DC blocking failed: {e}");
            }
        }

        UiToDsp::SetIqInversion(enabled) => {
            tracing::debug!(enabled, "set IQ inversion");
            state.invert_iq = enabled;
            state.frontend.set_invert_iq(enabled);
        }

        UiToDsp::SetFftSize(size) => {
            tracing::debug!(fft_size = size, "set FFT size");
            match IqFrontend::new(
                state.frontend.sample_rate(),
                state.frontend.decim_ratio(),
                size,
                state.window_fn,
                state.dc_blocking,
            ) {
                Ok(mut new_frontend) => {
                    new_frontend.set_invert_iq(state.invert_iq);
                    new_frontend.set_fft_rate(state.fft_rate);
                    state.frontend = new_frontend;
                    state.fft_buf = vec![0.0; size];
                }
                Err(e) => {
                    tracing::warn!("set FFT size failed: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("FFT size failed: {e}")));
                }
            }
        }

        UiToDsp::SetNbEnabled(enabled) => {
            tracing::debug!(enabled, "set noise blanker");
            state.radio.if_chain_mut().set_nb_enabled(enabled);
        }

        UiToDsp::SetFmIfNrEnabled(enabled) => {
            tracing::debug!(enabled, "set FM IF NR");
            state.radio.if_chain_mut().set_fm_if_nr_enabled(enabled);
        }

        UiToDsp::SetGain(gain_db) => {
            tracing::debug!(gain_db, "set gain");
            #[allow(clippy::cast_possible_truncation)]
            if let Some(source) = &mut state.source {
                // Source gain is in tenths of dB (e.g., 49.6 dB = 496)
                let gain_tenths = (gain_db * 10.0) as i32;
                if let Err(e) = source.set_gain(gain_tenths) {
                    tracing::warn!("set gain failed: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("Set gain failed: {e}")));
                }
            }
        }

        UiToDsp::SetAgc(enabled) => {
            tracing::debug!(enabled, "set AGC");
            if let Some(source) = &mut state.source {
                // AGC enabled = automatic gain (manual=false), AGC disabled = manual gain
                if let Err(e) = source.set_gain_mode(!enabled) {
                    tracing::warn!("set AGC failed: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("AGC failed: {e}")));
                }
            }
        }

        UiToDsp::SetIqCorrection(enabled) => {
            // IQ correction removes DC offset from the IQ signal.
            // Route to the DC blocker which serves the same purpose.
            tracing::debug!(enabled, "set IQ correction (via DC blocker)");
            state.dc_blocking = enabled;
            if let Err(e) = state.frontend.set_dc_blocking(enabled) {
                tracing::warn!("set IQ correction failed: {e}");
            }
        }

        UiToDsp::SetWindowFunction(window) => {
            tracing::debug!(?window, "set window function");
            state.window_fn = window;
            match IqFrontend::new(
                state.frontend.sample_rate(),
                state.frontend.decim_ratio(),
                state.frontend.fft_size(),
                window,
                state.dc_blocking,
            ) {
                Ok(mut new_frontend) => {
                    new_frontend.set_invert_iq(state.invert_iq);
                    new_frontend.set_fft_rate(state.fft_rate);
                    state.fft_buf = vec![0.0; new_frontend.fft_size()];
                    state.frontend = new_frontend;
                }
                Err(e) => {
                    tracing::warn!("set window function failed: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("Window function failed: {e}")));
                }
            }
        }

        UiToDsp::SetVfoOffset(offset) => {
            tracing::debug!(offset_hz = offset, "set VFO offset");
            state.vfo_offset = offset;
            if let Some(vfo) = &mut state.vfo {
                vfo.set_offset(offset);
            }
        }

        UiToDsp::SetNbLevel(level) => {
            tracing::debug!(level, "set noise blanker level");
            if let Err(e) = state.radio.if_chain_mut().set_nb_level(level) {
                tracing::warn!("set NB level failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("NB level failed: {e}")));
            }
        }

        UiToDsp::SetWfmStereo(enabled) => {
            tracing::debug!(enabled, "set WFM stereo");
            state.radio.set_wfm_stereo(enabled);
        }

        UiToDsp::SetFftRate(fps) => {
            tracing::debug!(fps, "set FFT rate");
            state.fft_rate = fps;
            state.frontend.set_fft_rate(fps);
        }

        UiToDsp::SetHighPass(enabled) => {
            tracing::debug!(enabled, "set high-pass filter");
            state.radio.set_high_pass_enabled(enabled);
        }

        UiToDsp::SetNotchEnabled(enabled) => {
            tracing::debug!(enabled, "set notch filter");
            state.radio.set_notch_enabled(enabled);
        }

        UiToDsp::SetNotchFrequency(freq) => {
            tracing::debug!(freq, "set notch frequency");
            state.radio.set_notch_frequency(freq);
        }

        UiToDsp::SetCtcssMode(mode) => {
            tracing::debug!(?mode, "set CTCSS mode");
            if let Err(e) = state.radio.set_ctcss_mode(mode) {
                tracing::warn!("CTCSS mode set failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("CTCSS mode failed: {e}")));
            }
        }

        UiToDsp::SetCtcssThreshold(threshold) => {
            tracing::debug!(threshold, "set CTCSS threshold");
            if let Err(e) = state.radio.set_ctcss_threshold(threshold) {
                tracing::warn!("CTCSS threshold set failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("CTCSS threshold failed: {e}")));
            }
        }

        UiToDsp::SetVoiceSquelchMode(mode) => {
            tracing::debug!(?mode, "set voice squelch mode");
            if let Err(e) = state.radio.set_voice_squelch_mode(mode) {
                tracing::warn!("voice squelch mode set failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Voice squelch failed: {e}")));
            }
        }

        UiToDsp::SetVoiceSquelchThreshold(threshold) => {
            tracing::debug!(threshold, "set voice squelch threshold");
            if let Err(e) = state.radio.set_voice_squelch_threshold(threshold) {
                tracing::warn!("voice squelch threshold set failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!(
                    "Voice squelch threshold failed: {e}"
                )));
            }
        }

        UiToDsp::SetAudioDevice(node_name) => {
            tracing::info!(target_node = %node_name, "set audio device");
            if let Err(e) = state.audio_sink.set_target(&node_name) {
                tracing::warn!("audio device switch failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Audio device switch failed: {e}")));
            }
        }

        UiToDsp::SetSourceType(source_type) => {
            tracing::info!(?source_type, "switching source type");
            let was_running = state.running;
            if was_running {
                cleanup(state);
                state.running = false;
            }
            state.source_type = source_type;
            // Force the rtl_tcp status row to reset when switching
            // away from RTL-TCP. Without this, a user mid-session
            // who switches to a different source would see the
            // stale "Connected — R820T" text linger until the next
            // poll tick (which won't fire if running=false). Only
            // emits on an actual edge.
            if source_type != SourceType::RtlTcp
                && !matches!(
                    state.last_rtl_tcp_state,
                    RtlTcpConnectionState::Disconnected
                )
            {
                state.last_rtl_tcp_state = RtlTcpConnectionState::Disconnected;
                let _ = dsp_tx.send(DspToUi::RtlTcpConnectionState(
                    RtlTcpConnectionState::Disconnected,
                ));
            }
            // Restart with the new source type if was playing
            if was_running {
                match open_source(state) {
                    Ok(()) => {
                        if let Err(e) = state.audio_sink.start() {
                            tracing::warn!("audio sink restart failed: {e}");
                            let _ =
                                dsp_tx.send(DspToUi::Error(format!("Audio output failed: {e}")));
                        }
                        state.running = true;
                        // Refresh UI with new source capabilities
                        if let Some(source) = &state.source {
                            let gains: Vec<f64> = source
                                .gains()
                                .iter()
                                .map(|&g| f64::from(g) / 10.0)
                                .collect();
                            if !gains.is_empty() {
                                let _ = dsp_tx.send(DspToUi::GainList(gains));
                            }
                        }
                        let _ = dsp_tx.send(DspToUi::SampleRateChanged(
                            state.frontend.effective_sample_rate(),
                        ));
                        let _ =
                            dsp_tx.send(DspToUi::DisplayBandwidth(state.frontend.sample_rate()));
                    }
                    Err(e) => {
                        tracing::warn!("source switch failed: {e}");
                        let _ = dsp_tx.send(DspToUi::Error(format!("Source switch failed: {e}")));
                        let _ = dsp_tx.send(DspToUi::SourceStopped);
                    }
                }
            }
        }

        UiToDsp::SetNetworkConfig {
            hostname,
            port,
            protocol,
        } => {
            tracing::debug!(%hostname, port, ?protocol, "set network config");
            state.network_host = hostname;
            state.network_port = port;
            state.network_protocol = protocol;
        }

        UiToDsp::SetFilePath(path) => {
            tracing::debug!(?path, "set file path");
            state.file_path = path;
        }

        UiToDsp::SetPpmCorrection(ppm) => {
            tracing::debug!(ppm, "set PPM correction");
            if let Some(source) = &mut state.source
                && let Err(e) = source.set_ppm_correction(ppm)
            {
                tracing::warn!("set PPM correction failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("PPM correction failed: {e}")));
            }
        }

        UiToDsp::StartAudioRecording(path) => {
            tracing::info!(?path, "start audio recording");
            match WavWriter::new(&path, AUDIO_SAMPLE_RATE, AUDIO_CHANNELS) {
                Ok(writer) => {
                    state.audio_writer = Some(writer);
                    let _ = dsp_tx.send(DspToUi::AudioRecordingStarted(path));
                }
                Err(e) => {
                    tracing::warn!("failed to start audio recording: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("Audio record failed: {e}")));
                }
            }
        }

        UiToDsp::StopAudioRecording => {
            tracing::info!("stop audio recording");
            // Drop the writer — `Drop` finalizes the WAV header.
            state.audio_writer = None;
            let _ = dsp_tx.send(DspToUi::AudioRecordingStopped);
        }

        UiToDsp::StartIqRecording(path) => {
            tracing::info!(?path, "start IQ recording");
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let iq_rate = state.sample_rate as u32;
            match WavWriter::new(&path, iq_rate, IQ_CHANNELS) {
                Ok(writer) => {
                    state.iq_writer = Some(writer);
                    let _ = dsp_tx.send(DspToUi::IqRecordingStarted(path));
                }
                Err(e) => {
                    tracing::warn!("failed to start IQ recording: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("IQ record failed: {e}")));
                }
            }
        }

        UiToDsp::StopIqRecording => {
            tracing::info!("stop IQ recording");
            state.iq_writer = None;
            let _ = dsp_tx.send(DspToUi::IqRecordingStopped);
        }

        UiToDsp::EnableTranscription(tx) => {
            // Reset the squelch edge tracker when a new tap is wired up.
            // Without this, a previous session that ended with squelch open
            // leaves `squelch_was_open == true`, so the first chunk of the
            // new session sees `now_open == was_open` and no SquelchOpened
            // edge is emitted — the offline Auto Break state machine would
            // stay in Idle and drop the entire current transmission until
            // the next open/close cycle.
            state.squelch_was_open = false;
            state.transcription_tx = Some(tx);
            tracing::info!("transcription audio tap enabled");
        }
        UiToDsp::DisableTranscription => {
            state.transcription_tx = None;
            // Mirror the reset on disable so a subsequent EnableTranscription
            // always starts from a known state.
            state.squelch_was_open = false;
            tracing::info!("transcription audio tap disabled");
        }

        UiToDsp::EnableAudioTap(tx) => {
            // Generic audio tap — post-demod, pre-volume, resampled to
            // 16 kHz mono and dropped into `tx`. Distinct from the
            // transcription tap above so FFI consumers (e.g. macOS
            // `SpeechAnalyzer` for issue #314) receive recognizer-ready
            // samples without pulling the sdr-transcription dep across
            // the FFI.
            state.audio_tap_tx = Some(tx);
            // Reset the decimation phase so a new tap session starts
            // at clean 3:1 alignment — otherwise a stale phase from
            // a previous session (disabled, then re-enabled) would
            // desynchronize the 16 kHz timebase until the phase
            // wraps.
            state.audio_tap_phase = 0;
            tracing::info!("audio tap enabled");
        }
        UiToDsp::DisableAudioTap => {
            state.audio_tap_tx = None;
            tracing::info!("audio tap disabled");
        }
        UiToDsp::DisconnectRtlTcp => {
            // Only meaningful while `RtlTcp` is the active source
            // type. For any other source we log-and-drop so
            // misrouted commands from buggy UI paths don't panic.
            if state.source_type != SourceType::RtlTcp {
                tracing::debug!(
                    active = ?state.source_type,
                    "DisconnectRtlTcp ignored — active source is not RtlTcp"
                );
                return;
            }
            if let Some(source) = state.source.as_mut()
                && let Err(e) = source.stop()
            {
                tracing::warn!(error = %e, "rtl_tcp source stop failed");
                let _ = dsp_tx.send(DspToUi::Error(format!("Disconnect failed: {e}")));
            }
            // Drop the source outright so `rtl_tcp_connection_state`
            // returns `None` (→ Disconnected) on the next poll,
            // cascading into a UI row that reflects reality.
            state.source = None;
            state.running = false;
            let _ = dsp_tx.send(DspToUi::SourceStopped);
        }
        UiToDsp::RetryRtlTcpNow => {
            // Same source-type gate as Disconnect. "Retry now"
            // implements as stop+start so the current backoff
            // sleep is short-circuited; the existing start_manager
            // path tears up a fresh connection from Connecting.
            if state.source_type != SourceType::RtlTcp {
                tracing::debug!(
                    active = ?state.source_type,
                    "RetryRtlTcpNow ignored — active source is not RtlTcp"
                );
                return;
            }
            if let Some(source) = state.source.as_mut() {
                if let Err(e) = source.stop() {
                    tracing::warn!(error = %e, "rtl_tcp stop before retry failed");
                }
                if let Err(e) = source.start() {
                    tracing::warn!(error = %e, "rtl_tcp restart failed");
                    let _ = dsp_tx.send(DspToUi::Error(format!("Retry failed: {e}")));
                }
            }
        }
    }
}

/// Open the active IQ source and configure it for streaming.
fn open_source(state: &mut DspState) -> Result<(), String> {
    let mut source: Box<dyn Source> = match state.source_type {
        SourceType::RtlSdr => Box::new(RtlSdrSource::new(DEVICE_INDEX)),
        SourceType::Network => Box::new(sdr_source_network::NetworkSource::new(
            &state.network_host,
            state.network_port,
            state.network_protocol,
        )),
        SourceType::File => Box::new(sdr_source_file::FileSource::new(&state.file_path)),
        // rtl_tcp client: connects to a remote `rtl_tcp`-compatible
        // server, handshakes the 12-byte RTL0 header, and routes
        // future tune / gain / PPM messages through the 5-byte
        // command channel. Reuses the `network_host` + `network_port`
        // config fields since the connection shape is the same (no
        // separate RtlTcpConfig state field needed).
        SourceType::RtlTcp => Box::new(sdr_source_network::RtlTcpSource::new(
            &state.network_host,
            state.network_port,
        )),
    };

    if let Err(e) = source.set_sample_rate(state.configured_sample_rate) {
        if state.source_type == SourceType::File {
            tracing::warn!("file source sample rate mismatch: {e}");
        } else {
            return Err(e.to_string());
        }
    }

    // Tune is a meaningful operation for both the local RTL-SDR and
    // any remote (RtlTcp) — both need the initial center frequency.
    // Network raw-IQ and File sources ignore it.
    if matches!(state.source_type, SourceType::RtlSdr | SourceType::RtlTcp) {
        source.tune(state.center_freq).map_err(|e| e.to_string())?;
    }

    source.start().map_err(|e| e.to_string())?;

    // Sync sample rate from the source (file sources have fixed rates).
    state.sample_rate = source.sample_rate();

    // Auto-adjust decimation for the source's actual sample rate.
    let if_rate = state.radio.demod_config().if_sample_rate;
    let auto_decim = auto_decimation_ratio(state.sample_rate, if_rate);
    if auto_decim != state.frontend.decim_ratio() {
        tracing::info!(auto_decim, "auto-adjusting decimation for source rate");
        let _ = state.frontend.set_decimation(auto_decim);
    }

    // Rebuild frontend and VFO before committing the source to state.
    // If either fails, stop the source to avoid a leaked running source.
    if let Err(e) = rebuild_frontend(state).and_then(|()| rebuild_vfo(state)) {
        let _ = source.stop();
        return Err(e);
    }
    state.source = Some(source);

    tracing::info!(
        sample_rate = state.sample_rate,
        center_freq = state.center_freq,
        "source opened"
    );
    Ok(())
}

/// Stop the source and release resources.
fn cleanup(state: &mut DspState) {
    if let Some(source) = &mut state.source {
        let _ = source.stop();
    }

    // Stop the audio sink so it doesn't try to read stale data.
    if let Err(e) = state.audio_sink.stop() {
        tracing::debug!("audio sink stop: {e}");
    }

    // Finalize any active recordings (Drop patches the WAV header sizes).
    if state.audio_writer.take().is_some() {
        tracing::info!("audio recording finalized on cleanup");
    }
    if state.iq_writer.take().is_some() {
        tracing::info!("IQ recording finalized on cleanup");
    }

    state.source = None;
    tracing::info!("source closed");
}

/// Rebuild the IQ frontend with the current sample rate, preserving user settings.
fn rebuild_frontend(state: &mut DspState) -> Result<(), String> {
    let mut new_frontend = IqFrontend::new(
        state.sample_rate,
        state.frontend.decim_ratio(),
        state.frontend.fft_size(),
        state.window_fn,
        state.dc_blocking,
    )
    .map_err(|e| format!("frontend rebuild: {e}"))?;

    new_frontend.set_invert_iq(state.invert_iq);
    new_frontend.set_fft_rate(state.fft_rate);
    state.frontend = new_frontend;
    Ok(())
}

/// Build or rebuild the `RxVfo` from the current frontend and demod configuration.
///
/// Also tells `RadioModule` that its input is now at the demod IF rate (since the
/// VFO handles resampling from the frontend effective rate to the IF rate).
fn rebuild_vfo(state: &mut DspState) -> Result<(), String> {
    let effective_rate = state.frontend.effective_sample_rate();
    let demod_cfg = state.radio.demod_config();
    let if_rate = demod_cfg.if_sample_rate;

    let vfo = RxVfo::new(effective_rate, if_rate, state.bandwidth, state.vfo_offset)
        .map_err(|e| format!("RxVfo build: {e}"))?;

    state.vfo = Some(vfo);

    // Tell RadioModule it receives samples at the demod IF rate — no internal
    // resampling needed since the VFO already handled it.
    state
        .radio
        .set_input_sample_rate(if_rate)
        .map_err(|e| format!("radio input rate: {e}"))?;

    tracing::debug!(
        frontend_rate = effective_rate,
        if_rate,
        bandwidth = state.bandwidth,
        offset = state.vfo_offset,
        "RxVfo rebuilt"
    );
    Ok(())
}

/// Compute the optimal power-of-2 decimation ratio to bring the sample rate
/// close to the demod IF rate. The effective rate will be >= `if_rate` (never
/// below, since undersampling causes aliasing).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn auto_decimation_ratio(sample_rate: f64, if_rate: f64) -> u32 {
    if sample_rate <= if_rate {
        return 1;
    }
    // Largest power-of-2 that keeps effective rate >= if_rate
    let ratio = (sample_rate / if_rate).floor() as u32;
    if ratio < 2 {
        return 1;
    }
    // Round down to nearest power of 2
    let pow2 = 1_u32 << ratio.ilog2();
    pow2.clamp(1, 8192) // MAX_POWER_DECIM_RATIO
}

/// Read one block of IQ data from the source, process it, and send FFT data
/// to the UI.
#[allow(clippy::too_many_lines)]
fn process_iq_block(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    fft_shared: &SharedFftBuffer,
) {
    let Some(source) = &mut state.source else {
        tracing::warn!("process_iq_block called without source");
        state.running = false;
        let _ = dsp_tx.send(DspToUi::SourceStopped);
        return;
    };

    let iq_count = match source.read_samples(&mut state.iq_buf) {
        Ok(0) => {
            // File sources return Ok(0) at EOF — stop playback cleanly
            if state.source_type == SourceType::File {
                tracing::info!("file source reached EOF");
                cleanup(state);
                state.running = false;
                let _ = dsp_tx.send(DspToUi::SourceStopped);
            }
            std::thread::yield_now();
            return;
        }
        Ok(n) => {
            state.iq_samples_read = state.iq_samples_read.saturating_add(n as u64);
            // Periodic rate diagnostic. Logs IQ read rate + audio
            // output rate side-by-side so USB-vs-DSP bottlenecks
            // are immediately visible: expected ratio is roughly
            // `source_sample_rate / audio_sample_rate`. If IQ
            // drops below the configured source rate, USB is
            // starved; if audio drops below IQ/ratio, the DSP
            // chain is behind.
            if state.diag_log_at.elapsed() >= DIAG_LOG_INTERVAL {
                let elapsed = state.diag_log_at.elapsed().as_secs_f64().max(f64::EPSILON);
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let iq_rate_sps = (state.iq_samples_read as f64 / elapsed).round() as u64;
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let audio_rate_fps = (state.audio_frames_written as f64 / elapsed).round() as u64;
                tracing::info!(
                    iq_samples = state.iq_samples_read,
                    iq_rate_sps,
                    audio_frames = state.audio_frames_written,
                    audio_rate_fps,
                    "pipeline rates"
                );
                state.iq_samples_read = 0;
                state.audio_frames_written = 0;
                state.diag_log_at = std::time::Instant::now();
            }
            n
        }
        Err(e) => {
            // Fatal errors (USB reader death, device lost) — stop the pipeline
            if matches!(
                e,
                sdr_types::SourceError::ReadFailed(_) | sdr_types::SourceError::NotRunning
            ) {
                tracing::error!("fatal source error: {e}");
                cleanup(state);
                state.running = false;
                let _ = dsp_tx.send(DspToUi::Error(format!("Source error: {e}")));
                let _ = dsp_tx.send(DspToUi::SourceStopped);
            } else {
                tracing::warn!("source read error: {e}");
            }
            return;
        }
    };

    // Write raw IQ samples to recording file (before any processing).
    if let Some(writer) = &mut state.iq_writer
        && let Err(e) = writer.write_iq(&state.iq_buf[..iq_count])
    {
        tracing::warn!("IQ recording write error: {e}");
        state.iq_writer = None;
        let _ = dsp_tx.send(DspToUi::Error("IQ recording write failed".to_string()));
        let _ = dsp_tx.send(DspToUi::IqRecordingStopped);
    }

    // Process through IQ frontend (decimation, DC blocking, FFT).
    match state.frontend.process(
        &state.iq_buf[..iq_count],
        &mut state.processed_buf,
        &mut state.fft_buf,
    ) {
        Ok((processed_count, fft_ready)) => {
            // Write FFT data to shared buffer (zero allocation — no Vec
            // cloned across threads, avoiding glibc arena fragmentation).
            if fft_ready {
                fft_shared.write(&state.fft_buf);
                state.fft_buf.fill(0.0);
            }

            if processed_count > 0 {
                // Pass through RxVfo: frequency translate, resample, channel filter.
                let radio_input = if let Some(vfo) = &mut state.vfo {
                    // Size VFO output buffer generously for resampling expansion.
                    let demod_cfg = state.radio.demod_config();
                    #[allow(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        clippy::cast_precision_loss
                    )]
                    let ratio = (demod_cfg.if_sample_rate / state.frontend.effective_sample_rate())
                        .ceil() as usize;
                    let vfo_out_size = processed_count * ratio.max(1) + VFO_OUTPUT_PADDING;
                    state.vfo_buf.resize(vfo_out_size, Complex::default());

                    match vfo.process(&state.processed_buf[..processed_count], &mut state.vfo_buf) {
                        Ok(vfo_count) => &state.vfo_buf[..vfo_count],
                        Err(e) => {
                            tracing::warn!("VFO processing error: {e}");
                            return;
                        }
                    }
                } else {
                    // No VFO configured — pass frontend output directly (fallback).
                    &state.processed_buf[..processed_count]
                };

                // Process through radio module for audio output.
                let max_out = state.radio.max_output_samples(radio_input.len());
                state.audio_buf.resize(max_out, Stereo::default());
                match state.radio.process(radio_input, &mut state.audio_buf) {
                    Ok(audio_count) => {
                        // Compute signal level for SNR display (before volume).
                        if audio_count > 0 {
                            let sum_sq: f32 = state.audio_buf[..audio_count]
                                .iter()
                                .map(|s| s.l * s.l + s.r * s.r)
                                .sum();
                            #[allow(clippy::cast_precision_loss)]
                            let rms = (sum_sq / (2.0 * audio_count as f32)).sqrt();
                            let level_db = 20.0 * rms.max(f32::MIN_POSITIVE).log10();
                            let _ = dsp_tx.send(DspToUi::SignalLevel(level_db));
                        }

                        // Emit CTCSS sustained-gate edges for the UI
                        // status indicator. Edge-triggered (not per
                        // block) so the channel isn't flooded at
                        // detector-window rate.
                        let now_ctcss = state.radio.ctcss_sustained();
                        if now_ctcss != state.ctcss_was_sustained {
                            let _ = dsp_tx.send(DspToUi::CtcssSustainedChanged(now_ctcss));
                            state.ctcss_was_sustained = now_ctcss;
                        }

                        // Voice squelch edges — same pattern, different
                        // source. Gate state comes from the AF-chain
                        // voice squelch which uses a rolling RMS
                        // window, so edges happen on timescales of
                        // ~100 ms (the RMS integration length) rather
                        // than CTCSS's 400 ms windows.
                        let now_voice = state.radio.voice_squelch_open();
                        if now_voice != state.voice_squelch_was_open {
                            let _ = dsp_tx.send(DspToUi::VoiceSquelchOpenChanged(now_voice));
                            state.voice_squelch_was_open = now_voice;
                        }

                        // Send audio copy to transcription worker BEFORE volume
                        // scaling so recognition isn't affected by the volume knob. Also
                        // emit squelch edge events on open/close transitions so offline
                        // sherpa backends can use them as Auto Break segmentation
                        // boundaries. Edge events are NFM-only — WFM and other modes
                        // don't have meaningful squelch transitions for speech.
                        if let Some(ref tx) = state.transcription_tx {
                            let now_open = state.radio.if_chain().squelch_open();
                            let mut send_error = false;
                            // True unless we tried to send an edge event and hit
                            // `TrySendError::Full`. Squelch edges are one-shot
                            // state transitions — if we advance `squelch_was_open`
                            // without the downstream having received the edge,
                            // the Auto Break state machine misses the transition
                            // entirely and gets stuck in Idle/Recording until the
                            // 30s safety flush fires. Retry on the next block by
                            // leaving the tracker unchanged.
                            let mut advance_tracker = true;

                            if now_open != state.squelch_was_open
                                && state.radio.current_mode() == sdr_types::DemodMode::Nfm
                            {
                                let edge = if now_open {
                                    sdr_transcription::TranscriptionInput::SquelchOpened
                                } else {
                                    sdr_transcription::TranscriptionInput::SquelchClosed
                                };
                                match tx.try_send(edge) {
                                    Ok(()) => {}
                                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                        send_error = true;
                                    }
                                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                        // Backend is busy (likely decoding an
                                        // earlier segment). Don't advance the
                                        // tracker so we retry this edge on the
                                        // next audio block instead of silently
                                        // dropping it.
                                        advance_tracker = false;
                                        tracing::warn!(
                                            ?now_open,
                                            "transcription channel full; retrying squelch edge next block"
                                        );
                                    }
                                }
                            }
                            if advance_tracker {
                                state.squelch_was_open = now_open;
                            }

                            if !send_error {
                                let mut interleaved = Vec::with_capacity(audio_count * 2);
                                for s in &state.audio_buf[..audio_count] {
                                    interleaved.push(s.l);
                                    interleaved.push(s.r);
                                }
                                if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) = tx
                                    .try_send(sdr_transcription::TranscriptionInput::Samples(
                                        interleaved,
                                    ))
                                {
                                    send_error = true;
                                }
                            }

                            if send_error {
                                state.transcription_tx = None;
                                tracing::info!(
                                    "transcription receiver disconnected, disabling tap"
                                );
                            }
                        }

                        // Generic audio tap: downsample to 16 kHz mono
                        // and try_send. Pre-volume (like the transcription
                        // tap) so the consumer's recognizer sees the raw
                        // demod output regardless of how the user has
                        // set the volume slider. `try_send` with
                        // `TrySendError::Full` → drop the chunk rather
                        // than block — the DSP thread MUST NOT stall on
                        // a slow consumer. `SpeechAnalyzer` can tolerate
                        // occasional frame drops; audio underruns are
                        // much worse.
                        if let Some(ref tx) = state.audio_tap_tx {
                            // Upper bound on output size — the phase-
                            // carrying resampler may write fewer than
                            // this depending on the carried phase, so
                            // we truncate to the returned count
                            // before sending.
                            let mono_cap = state.audio_buf[..audio_count]
                                .len()
                                .div_ceil(sdr_dsp::convert::AUDIO_TAP_DECIMATION_FACTOR);
                            let mut mono = vec![0.0_f32; mono_cap];
                            match sdr_dsp::convert::stereo_48k_to_mono_16k(
                                &state.audio_buf[..audio_count],
                                &mut mono,
                                &mut state.audio_tap_phase,
                            ) {
                                Ok(n) => {
                                    mono.truncate(n);
                                    // Skip the send on an empty chunk
                                    // (short input + unfavorable phase
                                    // can produce zero output on a
                                    // given call). Sending an empty
                                    // Vec would wake the dispatcher
                                    // for no reason.
                                    if mono.is_empty() {
                                        // no-op
                                    } else {
                                        match tx.try_send(mono) {
                                            Ok(()) => {}
                                            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                                // Consumer is lagging; drop
                                                // this chunk and carry on.
                                                tracing::debug!(
                                                    "audio tap channel full; dropping chunk"
                                                );
                                            }
                                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                                state.audio_tap_tx = None;
                                                tracing::info!(
                                                    "audio tap receiver disconnected, disabling"
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    // Sizing bug — the buffer is sized by
                                    // the same div_ceil expression as the
                                    // resampler's own calculation, so
                                    // this arm should be unreachable.
                                    // Log once and disable the tap.
                                    state.audio_tap_tx = None;
                                    tracing::error!(?e, "audio tap resampler failed");
                                }
                            }
                        }

                        // Apply volume with perceptual (power-law) scaling.
                        // Quadratic curve maps the linear slider to perceived loudness.
                        let vol = state.volume * state.volume;
                        for s in &mut state.audio_buf[..audio_count] {
                            s.l *= vol;
                            s.r *= vol;
                        }

                        // Write to audio recording file (post-volume).
                        if let Some(writer) = &mut state.audio_writer
                            && let Err(e) = writer.write_stereo(&state.audio_buf[..audio_count])
                        {
                            tracing::warn!("audio recording write error: {e}");
                            state.audio_writer = None;
                            let _ = dsp_tx
                                .send(DspToUi::Error("Audio recording write failed".to_string()));
                            let _ = dsp_tx.send(DspToUi::AudioRecordingStopped);
                        }

                        // Send to the audio sink (PipeWire on Linux,
                        // CoreAudio on macOS).
                        if audio_count > 0 {
                            state.audio_frames_written = state
                                .audio_frames_written
                                .saturating_add(audio_count as u64);
                        }
                        if let Err(e) = state
                            .audio_sink
                            .write_samples(&state.audio_buf[..audio_count])
                        {
                            // Terminal failures: surface to UI once and stop the sink.
                            if matches!(e, SinkError::Disconnected | SinkError::NotRunning) {
                                tracing::warn!("audio sink died: {e}");
                                let _ = dsp_tx.send(DspToUi::Error(
                                    "Audio output lost — restart playback".to_string(),
                                ));
                                let _ = state.audio_sink.stop();
                            } else {
                                tracing::debug!("audio write: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("radio processing error: {e}");
                    }
                }
            } // end if processed_count > 0
        }
        Err(e) => {
            tracing::warn!("frontend processing error: {e}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests {
    use super::*;

    /// Compile-time validation that DSP buffer constants are consistent.
    const _: () = {
        assert!(DEFAULT_FFT_SIZE > 0);
        assert!(DEFAULT_SAMPLE_RATE > 0.0);
        assert!(DEFAULT_CENTER_FREQ > 0.0);
        assert!(RECV_TIMEOUT_MS > 0);
        assert!(VFO_OUTPUT_PADDING > 0);
    };

    #[test]
    fn dsp_state_creates_successfully() {
        let state = DspState::new().unwrap();
        assert!(!state.running);
        assert!(state.source.is_none());
        assert_eq!(state.iq_buf.len(), IQ_PAIRS_PER_READ);
        assert_eq!(state.fft_buf.len(), DEFAULT_FFT_SIZE);
        // VFO starts as None (created on device open).
        assert!(state.vfo.is_none());
        assert!((state.vfo_offset - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rebuild_vfo_creates_vfo_and_sets_radio_rate() {
        let mut state = DspState::new().unwrap();
        // Simulate what open_source does: frontend is already built at default rate.
        rebuild_vfo(&mut state).unwrap();
        assert!(state.vfo.is_some());
    }

    #[test]
    fn rebuild_vfo_after_mode_switch_changes_rates() {
        let mut state = DspState::new().unwrap();
        // Start with NFM (default) — IF rate 50 kHz
        rebuild_vfo(&mut state).unwrap();

        // Switch to WFM — IF rate 250 kHz
        state.radio.set_mode(sdr_types::DemodMode::Wfm).unwrap();
        rebuild_vfo(&mut state).unwrap();
        assert!(state.vfo.is_some());

        // Switch to NFM — IF rate 50 kHz (different from WFM)
        state.radio.set_mode(sdr_types::DemodMode::Nfm).unwrap();
        rebuild_vfo(&mut state).unwrap();
        assert!(state.vfo.is_some());
    }

    #[test]
    fn vfo_preserves_signal_at_zero_offset() {
        // Create an RxVfo at same in/out rate, full bandwidth, offset 0.
        // The signal at DC should pass through essentially unchanged.
        let rate = 250_000.0;
        let mut vfo = RxVfo::new(rate, rate, rate, 0.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 1000];
        let mut output = vec![Complex::default(); 1100];
        let count = vfo.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
        // DC signal at zero offset should pass through with ~unity amplitude.
        for (i, s) in output[..count].iter().enumerate() {
            assert!(
                s.amplitude() > 0.9,
                "sample {i}: amplitude {} too low",
                s.amplitude()
            );
        }
    }

    #[test]
    fn vfo_translates_offset_signal_to_baseband() {
        // Generate a tone at +10 kHz offset within a 250 kHz stream.
        // Set VFO offset to +10 kHz so the tone lands at DC after translation.
        let in_rate = 250_000.0;
        let offset_hz = 10_000.0;
        let n = 2500;

        // Generate a pure tone at +offset_hz.
        let input: Vec<Complex> = (0..n)
            .map(|i| {
                let phase = 2.0 * std::f64::consts::PI * offset_hz * (i as f64) / in_rate;
                #[allow(clippy::cast_possible_truncation)]
                Complex::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();

        let mut vfo = RxVfo::new(in_rate, in_rate, in_rate, offset_hz).unwrap();
        let mut output = vec![Complex::default(); n + 100];
        let count = vfo.process(&input, &mut output).unwrap();
        assert!(count > 0);

        // After translation by -offset_hz, the signal should be near DC.
        // Skip the first few samples (filter settling) and check that the
        // imaginary part is small (signal is near real-only at DC).
        let settle = count / 4;
        let avg_imag: f32 = output[settle..count]
            .iter()
            .map(|s| s.im.abs())
            .sum::<f32>()
            / (count - settle) as f32;
        assert!(
            avg_imag < 0.15,
            "after translation, signal should be near DC — avg |imag| = {avg_imag}"
        );
    }

    #[test]
    fn vfo_resamples_250k_to_50k() {
        // Simulates WFM frontend (250 kHz) feeding NFM demod (50 kHz).
        let in_rate = 250_000.0;
        let out_rate = 50_000.0;
        let bandwidth = 12_500.0;
        let n = 2500; // 10 ms at 250 kHz

        let mut vfo = RxVfo::new(in_rate, out_rate, bandwidth, 0.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); n];
        let mut output = vec![Complex::default(); n]; // more than enough
        let count = vfo.process(&input, &mut output).unwrap();

        // Expected ~500 samples (2500 * 50k/250k)
        assert!(
            (400..=600).contains(&count),
            "expected ~500 samples at 50 kHz, got {count}"
        );
    }
}
