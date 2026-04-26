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

use sdr_dsp::apt::{AptDecoder, AptLine, READY_QUEUE_CAP};
use sdr_dsp::channel::RxVfo;
use sdr_pipeline::iq_frontend::{FftWindow, IqFrontend};
use sdr_pipeline::source_manager::Source;
use sdr_radio::lrpt_decoder::LrptDecoder;

use crate::sink_slot::{
    AudioSinkSlot, AudioSinkType, DEFAULT_NETWORK_SINK_HOST, DEFAULT_NETWORK_SINK_PORT,
    DEFAULT_NETWORK_SINK_PROTOCOL, NetworkSinkStatus,
};
use sdr_radio::RadioModule;
// `AudioSink` and `NetworkSink` are no longer used directly here —
// both live behind `AudioSinkSlot` (see `crate::sink_slot`) so the
// controller's audio path stays uniform regardless of which sink
// the user has selected.
use sdr_source_rtlsdr::RtlSdrSource;
use sdr_types::{Complex, RtlTcpConnectionState, SinkError, Stereo};

use crate::fft_buffer::SharedFftBuffer;
use crate::messages::{DspToUi, ScannerMutexReason, SourceType, UiToDsp};
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

/// Legal range for the `SetDirectSampling` command's `mode`
/// argument. Mirrors the RTL2832 direct-sampling mode register:
/// `0` = off (normal tuner path), `1` = I branch, `2` = Q
/// branch. Named so the FFI validation, the controller's
/// handler, and the diagnostic message all reference the same
/// bounds — per `CodeRabbit` round 1 on PR #360.
const DIRECT_SAMPLING_MIN: i32 = 0;
const DIRECT_SAMPLING_MAX: i32 = 2;

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
                        cleanup(&mut state, &dsp_tx);
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
    audio_sink: AudioSinkSlot,
    /// Which sink variant is currently active. Mirror of
    /// `audio_sink.kind()` kept on the state so handlers can
    /// branch on type without matching the enum every time. Per
    /// issue #247.
    audio_sink_type: AudioSinkType,
    /// Last user-picked local audio device UID (`PipeWire` node
    /// name on Linux, `AudioDevice` UID on macOS). Empty string =
    /// system default. Persisted across sink-type swaps so a
    /// Network → Local switch reapplies the user's prior device
    /// pick instead of falling back to default.
    audio_device_uid: String,
    /// Network sink hostname. Defaults to `localhost` to match
    /// the GTK source-network panel's defaults so switching to
    /// the network sink without an explicit configure step still
    /// produces a usable bind.
    network_sink_host: String,
    /// Network sink port. Defaults to `1234` matching the
    /// existing IQ source-network port default.
    network_sink_port: u16,
    /// Network sink protocol. Defaults to TCP server.
    network_sink_protocol: sdr_types::Protocol,
    /// Latched after a terminal `write_samples` failure
    /// (`SinkError::Disconnected` / `NotRunning`) so the next
    /// audio block doesn't re-fire the same warning + status
    /// event. Cleared on every successful `audio_sink.start()`
    /// — which means a sink-type swap, a network reconfig
    /// rebuild, or a fresh engine `Start` all rearm the path.
    /// Per `CodeRabbit` round 2 on PR #351.
    audio_sink_offline: bool,
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
    /// Role the `rtl_tcp` client requests in its `ClientHello`.
    /// Default `Role::Control` matches the pre-#392 single-
    /// client flow every legacy client assumes; UI flips this
    /// to `Role::Listen` when the user picks the Listen option
    /// in the connection-role combo row. Per #396.
    rtl_tcp_requested_role: sdr_server_rtltcp::extension::Role,
    /// Pre-shared key (#394) to send eagerly on `rtl_tcp`
    /// connect. `None` disables the auth gate; `Some(bytes)`
    /// activates the eager-auth path. Per #396.
    rtl_tcp_auth_key: Option<Vec<u8>>,
    file_path: std::path::PathBuf,
    /// Loop-on-EOF flag for the file playback source. Default
    /// `false` (stop at EOF). Updated by `UiToDsp::SetFileLooping`
    /// and applied both to the currently-running source (if any)
    /// and to the newly-opened source when the source is rebuilt
    /// from a path or source-type change. Per issue #236.
    file_looping: bool,

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

    /// Scanner state machine. Fed sample ticks + squelch edges
    /// from the IQ loop + UI command events from `handle_command`.
    /// Emitted commands are applied inline via
    /// `apply_scanner_commands`.
    scanner: sdr_scanner::Scanner,
    /// Cache of the last-pushed `ScannerChannel` list — read by
    /// `emit_scanner_active_channel` when building the
    /// `DspToUi::ScannerActiveChannelChanged` payload, since the
    /// scanner itself emits only the `ChannelKey` and the UI
    /// payload needs the full freq/demod/bandwidth/name tuple.
    scanner_channels: Vec<sdr_scanner::ScannerChannel>,
    /// Scanner-driven audio mute flag. Set by
    /// `ScannerCommand::MuteAudio(true)` during Retuning /
    /// Dwelling / Hanging phases, cleared on Listening entry.
    /// When `true`, the audio-sink write path fills `audio_buf`
    /// with silence in-place so the user hears nothing during
    /// retune / no-activity windows while the DSP chain still
    /// runs (squelch edges still fire → scanner state machine
    /// stays live).
    scanner_muted: bool,

    /// NOAA APT decoder, lazily constructed on first use. Fed
    /// from the post-`radio.process` audio path when the active
    /// demod mode is NFM (the only mode the APT 2400 Hz subcarrier
    /// rides through cleanly). Audio output rate is 48 kHz which
    /// is well above the decoder's 4800 Hz Nyquist floor.
    ///
    /// `None` means "not yet built" — built once, kept across
    /// demod-mode toggles so re-entering NFM during a pass picks
    /// up where it left off rather than restarting decoder state.
    /// Per epic #468 / ticket #482.
    apt_decoder: Option<AptDecoder>,
    /// Pre-allocated mono downmix buffer for the APT decoder
    /// input. Reused across DSP blocks; resized in place each
    /// call so we don't alloc inside the hot loop.
    apt_mono_buf: Vec<f32>,
    /// Pre-allocated output buffer for `AptDecoder::process`. Sized
    /// to match the decoder's internal queue cap (8 lines per the
    /// `AptDecoder` docs); the decoder won't emit more than this in
    /// a single call.
    apt_lines_buf: Vec<AptLine>,
    /// Most recent audio sample rate that `AptDecoder::new` rejected
    /// (or `None` if every prior init succeeded / hasn't been tried).
    /// Guards against the audio-block hot loop retrying — and
    /// log-spamming — on a rate the decoder will never accept (e.g.
    /// a future audio-rate change to something below the 4800 Hz
    /// Nyquist floor). Cleared in `cleanup` alongside the decoder
    /// reset so a fresh source restart always gets one fresh
    /// init attempt.
    apt_init_failed_at_rate: Option<u32>,
    /// Meteor-M LRPT decoder driver. Lazy-init when the demod
    /// mode first switches to `DemodMode::Lrpt`; teardown in
    /// `cleanup` (per-source-stop). The driver owns
    /// `LrptDemod` + `LrptPipeline` + the per-APID line-
    /// watermark map; the shared `LrptImage` handle is wired
    /// in from the UI side via `UiToDsp::SetLrptImage` so the
    /// live viewer reads from the same buffer this driver
    /// pushes lines into. Per epic #469 task 7.
    lrpt_decoder: Option<LrptDecoder>,
    /// Shared image handle the LRPT decoder pushes scan lines
    /// into. `None` until the UI side wires it via
    /// `UiToDsp::SetLrptImage`; in that case the controller
    /// simply doesn't run the LRPT tap (auto-record will set
    /// it at AOS, manual LRPT-mode use without a viewer is a
    /// silent-but-harmless state).
    lrpt_image: Option<sdr_radio::lrpt_image::LrptImage>,
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
            audio_sink: AudioSinkSlot::local_default(),
            audio_sink_type: AudioSinkType::Local,
            audio_device_uid: String::new(),
            network_sink_host: DEFAULT_NETWORK_SINK_HOST.to_string(),
            network_sink_port: DEFAULT_NETWORK_SINK_PORT,
            network_sink_protocol: DEFAULT_NETWORK_SINK_PROTOCOL,
            audio_sink_offline: false,
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
            rtl_tcp_requested_role: sdr_server_rtltcp::extension::Role::Control,
            rtl_tcp_auth_key: None,
            file_path: std::path::PathBuf::new(),
            file_looping: false,
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
            scanner: sdr_scanner::Scanner::new(),
            scanner_channels: Vec::new(),
            scanner_muted: false,
            apt_decoder: None,
            apt_mono_buf: Vec::new(),
            apt_lines_buf: Vec::new(),
            apt_init_failed_at_rate: None,
            lrpt_decoder: None,
            lrpt_image: None,
        })
    }
}

/// NOAA APT decode tap. Lazy-initialises the decoder at the
/// `RadioModule`'s current audio sample rate, downmixes the post-
/// `radio.process` stereo audio block to mono, runs the decoder,
/// and emits any newly-produced lines through the DSP→UI channel
/// as `DspToUi::AptLine`.
///
/// Per epic #468 / ticket #482. Caller must ensure
/// `audio_count > 0` and the active demod is NFM.
fn apt_decode_tap(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, audio_count: usize) {
    // Lazy-init. Audio rate comes from `RadioModule::audio_sample_rate`
    // (typically 48 kHz, well above the decoder's 4800 Hz floor).
    if state.apt_decoder.is_none() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rate_hz = state.radio.audio_sample_rate() as u32;
        // Guard against retry-spamming the warn log on a rate the
        // decoder will reject. If we've already tried this exact
        // rate and it failed, silently bail until either the rate
        // changes (next-block check) or `cleanup` clears the cache.
        if state.apt_init_failed_at_rate == Some(rate_hz) {
            return;
        }
        match AptDecoder::new(rate_hz) {
            Ok(decoder) => {
                tracing::info!("APT decoder initialised at {rate_hz} Hz");
                state.apt_decoder = Some(decoder);
                state.apt_init_failed_at_rate = None;
                // Size the output slice to the decoder's documented
                // per-call emission cap so a single `process` call
                // can never need to flush.
                state
                    .apt_lines_buf
                    .resize(READY_QUEUE_CAP, AptLine::default());
            }
            Err(e) => {
                tracing::warn!("APT decoder init failed at {rate_hz} Hz: {e}");
                state.apt_init_failed_at_rate = Some(rate_hz);
                return;
            }
        }
    }
    let Some(decoder) = state.apt_decoder.as_mut() else {
        return;
    };

    // Mono downmix. APT is mono by spec — averaging L+R is
    // equivalent to taking either channel for FM-demodulated
    // audio (both channels carry the same baseband signal once
    // any stereo pilot is filtered out by the channel filter).
    // `extend` over a `map` iterator is exact-size, so `Vec`'s
    // internal reserve is precise — no manual `reserve` needed.
    state.apt_mono_buf.clear();
    state.apt_mono_buf.extend(
        state.audio_buf[..audio_count]
            .iter()
            .map(|s| (s.l + s.r) * 0.5),
    );

    match decoder.process(&state.apt_mono_buf, &mut state.apt_lines_buf) {
        Ok(produced) => {
            // `mem::take` lifts each emitted line out by swapping in
            // `AptLine::default()` — moves ownership without the
            // ~2 KB clone. The next `process` call overwrites the
            // (now-default) slot regardless, so leaving an empty
            // line behind is harmless.
            for slot in state.apt_lines_buf.iter_mut().take(produced) {
                let line = std::mem::take(slot);
                let _ = dsp_tx.send(DspToUi::AptLine(Box::new(line)));
            }
        }
        Err(e) => {
            tracing::warn!("APT decode failed: {e}");
        }
    }
}

/// Meteor-M LRPT decode tap — IQ counterpart of [`apt_decode_tap`].
/// Lazy-initialises the decoder against the shared
/// `LrptImage` handle the wiring layer set via
/// `UiToDsp::SetLrptImage`, then streams the post-VFO IQ slice
/// (`radio_input` — already at 144 ksps thanks to the
/// `DemodMode::Lrpt` IF rate) through the full LRPT chain
/// (QPSK demod, FEC, image assembler). Emitted scan lines
/// land in the shared `LrptImage` for the live viewer to read.
///
/// Only runs when (a) `current_mode == DemodMode::Lrpt` and (b)
/// the wiring layer has handed us a `LrptImage` handle. Without
/// the handle, the tap is silent — manual LRPT-mode use without
/// a viewer harmlessly produces no output. Per epic #469 task 7.
///
/// Takes the decoder + image references directly (rather than
/// `&mut DspState`) so the call site can hold a live borrow of
/// `radio_input` — which itself points into a separate state
/// field (`vfo_buf` or `processed_buf`) — without violating
/// borrow-disjointness.
fn lrpt_decode_tap(
    decoder_slot: &mut Option<LrptDecoder>,
    image: Option<&sdr_radio::lrpt_image::LrptImage>,
    radio_input: &[Complex],
) {
    let Some(image) = image else {
        return;
    };
    if decoder_slot.is_none() {
        match LrptDecoder::new(image.clone()) {
            Ok(decoder) => {
                tracing::info!(
                    "LRPT decoder initialised at {} Hz IF rate",
                    sdr_dsp::lrpt::SAMPLE_RATE_HZ
                );
                *decoder_slot = Some(decoder);
            }
            Err(e) => {
                tracing::warn!("LRPT decoder init failed: {e}");
                return;
            }
        }
    }
    let Some(decoder) = decoder_slot.as_mut() else {
        return;
    };
    decoder.process(radio_input);
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
                    // so the spectrum display still works. Discriminate the
                    // error path by sink type so the network status row in
                    // the UI sees a real `NetworkSinkStatus::Error` event
                    // instead of a generic toast.
                    let start_result = state.audio_sink.start();
                    // Re-arm or latch the write path based on
                    // the start outcome. See the
                    // `audio_sink_offline` docstring for the
                    // full one-shot rationale — failed starts
                    // must latch, otherwise the next DSP block
                    // would re-fire the same terminal error
                    // when `write_samples` hits the stopped
                    // sink. Per CodeRabbit round 6 on PR #351.
                    state.audio_sink_offline = start_result.is_err();
                    let is_network = matches!(state.audio_sink_type, AudioSinkType::Network);
                    if let Err(e) = start_result {
                        tracing::warn!(
                            sink_type = ?state.audio_sink_type,
                            "audio sink failed to start (spectrum still works): {e}"
                        );
                        if is_network {
                            let _ =
                                dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Error {
                                    message: format!("{e}"),
                                }));
                        } else {
                            let _ =
                                dsp_tx.send(DspToUi::Error(format!("Audio output failed: {e}")));
                        }
                    } else if is_network {
                        // Successful start of a network sink — this is the
                        // moment the panel's status row should flip to
                        // "Streaming to ...". Driving status from real
                        // start/stop transitions (rather than the
                        // sink-type swap) keeps the UI honest about what's
                        // actually on the wire. Per CodeRabbit round 1 on
                        // PR #351.
                        let _ =
                            dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Active {
                                endpoint: format!(
                                    "{}:{}",
                                    state.network_sink_host, state.network_sink_port
                                ),
                                protocol: state.network_sink_protocol,
                            }));
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
            // `cleanup` now emits `NetworkSinkStatus::Inactive` itself
            // when the active sink was Network — same path used by the
            // file-EOF, fatal-source-error, and source-type restart
            // sites so every real stop transition reports Inactive.
            cleanup(state, dsp_tx);
            state.running = false;
            let _ = dsp_tx.send(DspToUi::SourceStopped);
        }

        UiToDsp::Tune(freq) => {
            tracing::debug!(frequency_hz = freq, "tune command");
            on_tune_change(state);
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
            on_tune_change(state);
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
            on_tune_change(state);
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

        UiToDsp::SetSoftwareAgc(enabled) => {
            tracing::debug!(enabled, "set software AGC");
            // No failure path here — the IF chain's envelope
            // state is purely in-memory. Unlike hardware AGC,
            // we can't miss the source device.
            state.radio.if_chain_mut().set_software_agc_enabled(enabled);
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
            // Expanded tracing for the #337 click-to-tune-no-audio
            // investigation: the #337 hypotheses point at a
            // display-span vs. VFO-input-sample-rate mismatch
            // (decim > 1) and/or clicks landing outside the AA-
            // filter-safe subset, so surface BOTH rates + whether
            // the VFO chain exists so the next smoke-test trace
            // shows the offset's relationship to the filterable
            // range at a glance.
            let raw_rate = state.frontend.sample_rate();
            let effective_rate = state.frontend.effective_sample_rate();
            let vfo_exists = state.vfo.is_some();
            tracing::debug!(
                offset_hz = offset,
                raw_sample_rate_hz = raw_rate,
                effective_sample_rate_hz = effective_rate,
                offset_within_effective = offset.abs() < effective_rate / 2.0,
                vfo_exists,
                "set VFO offset"
            );
            state.vfo_offset = offset;
            if let Some(vfo) = &mut state.vfo {
                vfo.set_offset(offset);
            }
            // Echo so UI paths that trigger this indirectly
            // (reset-to-defaults button, future scanner / scripting
            // hooks) reflect the new offset in their overlay /
            // frequency readout without optimistically guessing
            // locally. Matches the `BandwidthChanged` echo above.
            let _ = dsp_tx.send(DspToUi::VfoOffsetChanged(offset));
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
            // Persist the UID so a future Network → Local
            // sink-type swap can re-apply the user's pick
            // instead of falling back to the system default.
            // Per issue #247.
            state.audio_device_uid.clone_from(&node_name);
            if let Err(e) = state.audio_sink.set_target(&node_name) {
                tracing::warn!("audio device switch failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Audio device switch failed: {e}")));
            }
        }

        UiToDsp::SetAudioSinkType(new_type) => {
            tracing::info!(?new_type, "set audio sink type");
            if state.audio_sink_type == new_type {
                return;
            }
            // Snapshot the previous type so the post-swap
            // status logic can emit the correct "transitioning
            // away from network" event even when the
            // replacement local sink fails to start. Per
            // CodeRabbit round 2 on PR #351.
            let prev_type = state.audio_sink_type;
            // Stop the current sink so it releases its underlying
            // resource (audio device handle / socket) before we
            // construct the replacement.
            if let Err(e) = state.audio_sink.stop() {
                tracing::warn!("audio sink stop during type swap failed: {e}");
            }
            // Build the new sink.
            state.audio_sink = match new_type {
                AudioSinkType::Local => AudioSinkSlot::local_default(),
                AudioSinkType::Network => AudioSinkSlot::network(
                    &state.network_sink_host,
                    state.network_sink_port,
                    state.network_sink_protocol,
                ),
            };
            state.audio_sink_type = new_type;
            // Re-apply the persisted local-device pick so the
            // post-swap Local sink routes to the user's last
            // choice instead of the system default. No-op for
            // Network.
            if matches!(new_type, AudioSinkType::Local)
                && let Err(e) = state.audio_sink.set_target(&state.audio_device_uid)
            {
                tracing::warn!("post-swap set_target failed: {e}");
            }
            // Bring the new sink online if the engine is already
            // running. Otherwise it'll start on the next Start
            // command — and we emit `Inactive` rather than
            // `Active` because the sink isn't really on the wire
            // yet. Per CodeRabbit round 1 on PR #351, status
            // events must reflect REAL lifecycle, not just the
            // user's selected type.
            if state.running {
                match state.audio_sink.start() {
                    Ok(()) => {
                        // Successful start clears the offline
                        // latch so the audio write path resumes.
                        state.audio_sink_offline = false;
                        if matches!(new_type, AudioSinkType::Network) {
                            let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(
                                NetworkSinkStatus::Active {
                                    endpoint: format!(
                                        "{}:{}",
                                        state.network_sink_host, state.network_sink_port
                                    ),
                                    protocol: state.network_sink_protocol,
                                },
                            ));
                        } else {
                            // Switched away from network → that
                            // sink is no longer streaming. Emit
                            // Inactive so the panel's status row
                            // clears.
                            let _ = dsp_tx
                                .send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive));
                        }
                    }
                    Err(e) => {
                        // Latch so the next DSP block doesn't re-fire
                        // the same terminal error against a stopped
                        // sink. Per CodeRabbit round 6 on PR #351.
                        state.audio_sink_offline = true;
                        tracing::warn!("audio sink start after type swap failed: {e}");
                        if matches!(new_type, AudioSinkType::Network) {
                            let _ =
                                dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Error {
                                    message: format!("{e}"),
                                }));
                        } else {
                            let _ = dsp_tx
                                .send(DspToUi::Error(format!("Audio sink failed to start: {e}")));
                            // Even on failure, the network sink
                            // is gone — emit Inactive so the
                            // panel's status row clears its
                            // "Active" state. Per CodeRabbit
                            // round 2 on PR #351.
                            if matches!(prev_type, AudioSinkType::Network) {
                                let _ = dsp_tx
                                    .send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive));
                            }
                        }
                    }
                }
            } else {
                // Engine not running — nothing is on the wire.
                // Always emit Inactive so the panel doesn't
                // misreport a not-yet-bound sink as Active. The
                // matching Active will fire from the Start
                // handler if/when the user starts the engine.
                let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive));
            }
        }

        UiToDsp::SetNetworkSinkConfig {
            hostname,
            port,
            protocol,
        } => {
            tracing::info!(%hostname, port, ?protocol, "set network sink config");
            // Persist on state so a future SetAudioSinkType swap
            // picks the new values up.
            state.network_sink_host.clone_from(&hostname);
            state.network_sink_port = port;
            state.network_sink_protocol = protocol;
            // If the network sink is currently selected, rebuild
            // it inline so the new endpoint takes effect now.
            // Status events fire only on the real start
            // outcome (Active on success, Error on failure,
            // Inactive when the engine isn't running yet) — per
            // CodeRabbit round 1 on PR #351.
            if matches!(state.audio_sink_type, AudioSinkType::Network) {
                if let Err(e) = state.audio_sink.stop() {
                    tracing::warn!("network sink stop during reconfig failed: {e}");
                }
                state.audio_sink = AudioSinkSlot::network(&hostname, port, protocol);
                if state.running {
                    match state.audio_sink.start() {
                        Ok(()) => {
                            state.audio_sink_offline = false;
                            let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(
                                NetworkSinkStatus::Active {
                                    endpoint: format!("{hostname}:{port}"),
                                    protocol,
                                },
                            ));
                        }
                        Err(e) => {
                            // Latch — per CodeRabbit round 6 on PR #351.
                            state.audio_sink_offline = true;
                            tracing::warn!("network sink restart after reconfig failed: {e}");
                            let _ =
                                dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Error {
                                    message: format!("{e}"),
                                }));
                        }
                    }
                } else {
                    // Engine not running — sink rebuilt but not
                    // bound. Status stays Inactive.
                    let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive));
                }
            }
        }

        UiToDsp::SetSourceType(source_type) => {
            tracing::info!(?source_type, "switching source type");
            let was_running = state.running;
            if was_running {
                cleanup(state, dsp_tx);
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
                        // Clear the audio-sink offline latch on
                        // a successful restart, same as the
                        // other successful-start paths (engine
                        // Start, SetAudioSinkType,
                        // SetNetworkSinkConfig). Without this,
                        // a prior-session terminal write
                        // failure could leave the latch set
                        // through a source-type swap and gate
                        // writes off until the next explicit
                        // Start command. Per `CodeRabbit`
                        // round 3 on PR #351.
                        // Mirror the network-specific lifecycle
                        // events the other start paths emit
                        // (engine Start, SetAudioSinkType,
                        // SetNetworkSinkConfig). Without these,
                        // a source-type swap could leave the
                        // GTK network status row stuck on a
                        // stale Active or Error from before the
                        // swap. Per `CodeRabbit` round 5 on
                        // PR #351.
                        let is_network = matches!(state.audio_sink_type, AudioSinkType::Network);
                        match state.audio_sink.start() {
                            Ok(()) => {
                                state.audio_sink_offline = false;
                                if is_network {
                                    let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(
                                        NetworkSinkStatus::Active {
                                            endpoint: format!(
                                                "{}:{}",
                                                state.network_sink_host, state.network_sink_port
                                            ),
                                            protocol: state.network_sink_protocol,
                                        },
                                    ));
                                }
                            }
                            Err(e) => {
                                // Latch — per CodeRabbit round 6 on PR #351.
                                state.audio_sink_offline = true;
                                tracing::warn!("audio sink restart failed: {e}");
                                if is_network {
                                    let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(
                                        NetworkSinkStatus::Error {
                                            message: format!("{e}"),
                                        },
                                    ));
                                } else {
                                    let _ = dsp_tx
                                        .send(DspToUi::Error(format!("Audio output failed: {e}")));
                                }
                            }
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

        UiToDsp::SetRtlTcpClientConfig {
            requested_role,
            auth_key,
        } => {
            // Role-only updates log the role; auth-key updates
            // log the has/not-has state, not the bytes.
            tracing::debug!(
                ?requested_role,
                auth_key_set = auth_key.is_some(),
                "set rtl_tcp client config"
            );
            state.rtl_tcp_requested_role = requested_role;
            state.rtl_tcp_auth_key = auth_key;
            // Takes effect on the NEXT connect. An already-
            // running rtl_tcp session keeps its admitted role
            // until it disconnects — changing role mid-stream
            // would require the server to re-admit the client,
            // which the wire protocol doesn't support (the
            // role byte is part of the hello). Per issue #396.
        }

        UiToDsp::SetFilePath(path) => {
            tracing::debug!(?path, "set file path");
            state.file_path = path;
        }

        UiToDsp::SetFileLooping(looping) => {
            // Store on the state so a source rebuild (e.g. after
            // a file-path change) picks up the latest setting,
            // and also apply to the live source so an already-
            // playing file starts / stops looping at its next
            // EOF. Non-file sources silently accept per the
            // trait default. Per issue #236.
            tracing::debug!(looping, "set file looping");
            state.file_looping = looping;
            if let Some(source) = &mut state.source
                && let Err(e) = source.set_looping(looping)
            {
                tracing::warn!("set file looping failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("File looping failed: {e}")));
            }
        }

        UiToDsp::SetBiasTee(enabled) => {
            tracing::debug!(enabled, "set bias tee");
            if let Some(source) = &mut state.source
                && let Err(e) = source.set_bias_tee(enabled)
            {
                tracing::warn!("set bias tee failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Bias tee failed: {e}")));
            }
        }

        UiToDsp::SetDirectSampling(mode) => {
            tracing::debug!(mode, "set direct sampling");
            if !(DIRECT_SAMPLING_MIN..=DIRECT_SAMPLING_MAX).contains(&mode) {
                tracing::warn!(
                    "set direct sampling rejected: mode {mode} out of range \
                     ({DIRECT_SAMPLING_MIN}..={DIRECT_SAMPLING_MAX})"
                );
                let _ = dsp_tx.send(DspToUi::Error(format!(
                    "Direct sampling mode {mode} out of range \
                     ({DIRECT_SAMPLING_MIN}..={DIRECT_SAMPLING_MAX})"
                )));
            } else if let Some(source) = &mut state.source
                && let Err(e) = source.set_direct_sampling(mode)
            {
                tracing::warn!("set direct sampling failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Direct sampling failed: {e}")));
            }
        }

        UiToDsp::SetOffsetTuning(enabled) => {
            tracing::debug!(enabled, "set offset tuning");
            if let Some(source) = &mut state.source
                && let Err(e) = source.set_offset_tuning(enabled)
            {
                tracing::warn!("set offset tuning failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Offset tuning failed: {e}")));
            }
        }

        UiToDsp::SetRtlAgc(enabled) => {
            tracing::debug!(enabled, "set RTL AGC");
            if let Some(source) = &mut state.source
                && let Err(e) = source.set_rtl_agc(enabled)
            {
                tracing::warn!("set RTL AGC failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("RTL AGC failed: {e}")));
            }
        }

        UiToDsp::SetGainByIndex(index) => {
            tracing::debug!(index, "set gain by index");
            if let Some(source) = &mut state.source {
                // Bounds-check the index. Two sources of truth for
                // the legal count:
                //
                //   1. `source.gains()` — populated for local
                //      RTL-SDR USB (the tuner's discrete gain
                //      table).
                //   2. The rtl_tcp `Connected` connection state's
                //      `gain_count` field — servers publish the
                //      count but not the values, and
                //      `RtlTcpSource::gains()` returns an empty
                //      slice.
                //
                // Prefer (1) when it's non-empty; fall back to
                // (2) for the rtl_tcp case. If neither is
                // available we dispatch the command unchecked —
                // the source may no-op (default trait impl) or
                // surface a wire-level error later. Per
                // `CodeRabbit` round 1 on PR #360.
                let max_count = {
                    let gains_len = source.gains().len();
                    if gains_len > 0 {
                        Some(gains_len)
                    } else {
                        match source.rtl_tcp_connection_state() {
                            Some(sdr_types::RtlTcpConnectionState::Connected {
                                gain_count,
                                ..
                            }) => Some(gain_count as usize),
                            _ => None,
                        }
                    }
                };
                if let Some(max) = max_count
                    && (index as usize) >= max
                {
                    tracing::warn!("set gain by index rejected: {index} >= {max}");
                    let _ = dsp_tx.send(DspToUi::Error(format!(
                        "Gain index {index} out of range (source has {max} gains)"
                    )));
                } else if let Err(e) = source.set_gain_by_index(index) {
                    tracing::warn!("set gain by index failed: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("Set gain failed: {e}")));
                }
            }
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
            // Open the writer FIRST. If it fails we want to leave
            // the scanner untouched — sending `ScannerMutexStopped`
            // before knowing the recording actually started would
            // visibly kill the scanner in the UI and misleadingly
            // tell the user recording started.
            match WavWriter::new(&path, AUDIO_SAMPLE_RATE, AUDIO_CHANNELS) {
                Ok(writer) => {
                    // Recording committed — now apply the mutex.
                    // Scanner, per-hit recording, and transcription
                    // are mutually exclusive in Phase 1.
                    if state.scanner.is_enabled() {
                        let cmds = state
                            .scanner
                            .handle_event(sdr_scanner::ScannerEvent::SetEnabled(false));
                        apply_scanner_commands(state, dsp_tx, cmds);
                        let _ = dsp_tx.send(DspToUi::ScannerMutexStopped(
                            ScannerMutexReason::ScannerStoppedForRecording,
                        ));
                    }
                    // Recording ↔ transcription leg: stop any active
                    // transcription tap so the two don't run concurrently.
                    // `stop_transcription` is silent (no DspToUi event) —
                    // the transcription lifecycle has no feedback channel
                    // today, matching the existing DisableTranscription
                    // path. UI-switch resync is a known follow-up.
                    stop_transcription(state);
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

        UiToDsp::SetLrptImage(image) => {
            tracing::info!("LRPT image handle attached — decoder tap will push lines");
            state.lrpt_image = Some(image);
            // Decoder state intentionally NOT dropped here.
            // `AppState::lrpt_image` is a long-lived singleton
            // — every `SetLrptImage` carries the same handle —
            // so reattach is logically a no-op for the decoder.
            // Earlier draft dropped it defensively, but that
            // turned the round-11 (`CodeRabbit` PR #543)
            // defensive re-send in `open_lrpt_viewer_if_needed`
            // into a mid-pass decoder reset that lost Viterbi /
            // sync state on every viewer reuse. Decoder
            // lifecycle stays owned by source-stop cleanup —
            // same contract `ClearLrptImage` codifies (round 1).
        }

        UiToDsp::ClearLrptImage => {
            tracing::info!("LRPT image handle cleared — decoder tap is silent");
            state.lrpt_image = None;
            // Decoder state stays alive — the tap is already
            // disabled because `lrpt_image` is None, and
            // teardown / reset belong to the source-stop
            // cleanup path. Mirrors the APT decoder, which
            // also keeps its state across stop-listening /
            // resume-listening cycles so resumed listening
            // doesn't pay re-init cost. The `messages.rs`
            // doc-comment for `ClearLrptImage` codifies this
            // contract; an earlier draft contradicted it by
            // dropping the decoder here. Per CodeRabbit
            // round 1 on PR #543.
        }

        UiToDsp::StartIqRecording(path) => {
            tracing::info!(?path, "start IQ recording");
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let iq_rate = state.sample_rate as u32;
            // Open-first, apply-mutex-on-success — same rationale
            // as `StartAudioRecording` above.
            match WavWriter::new(&path, iq_rate, IQ_CHANNELS) {
                Ok(writer) => {
                    if state.scanner.is_enabled() {
                        let cmds = state
                            .scanner
                            .handle_event(sdr_scanner::ScannerEvent::SetEnabled(false));
                        apply_scanner_commands(state, dsp_tx, cmds);
                        let _ = dsp_tx.send(DspToUi::ScannerMutexStopped(
                            ScannerMutexReason::ScannerStoppedForRecording,
                        ));
                    }
                    // Recording ↔ transcription mutex — see
                    // StartAudioRecording for rationale.
                    stop_transcription(state);
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
            if state.scanner.is_enabled() {
                let cmds = state
                    .scanner
                    .handle_event(sdr_scanner::ScannerEvent::SetEnabled(false));
                apply_scanner_commands(state, dsp_tx, cmds);
                let _ = dsp_tx.send(DspToUi::ScannerMutexStopped(
                    ScannerMutexReason::ScannerStoppedForTranscription,
                ));
            }
            // Recording ↔ transcription leg of the mutex. Both
            // `stop_any_recording` sends cover the UI (it emits
            // `AudioRecordingStopped` / `IqRecordingStopped`), so
            // the recording buttons flip off automatically.
            stop_any_recording(state, dsp_tx);
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
            // "Retry now" REBUILDS the `RtlTcpSource` from the
            // latest `DspState` (role + auth_key) instead of just
            // stopping + starting the existing instance. Rebuild
            // is required because the role / auth-key config is
            // baked into `RtlTcpSource` at construction via
            // `with_config(...)`; a subsequent `start()` on the
            // same instance replays its original `ClientHello`,
            // which means a newly-entered key or flipped role
            // from the UI would never land on the wire until the
            // user forced a full source tear-down (Stop + Play,
            // source-type switch). After an `AuthRequired` /
            // `AuthFailed` / `ControllerBusy` denial those retry
            // semantics are explicitly user-driven, so the
            // rebuild is the correct behavior.
            //
            // The sticky-command replay cache (gain, AGC, PPM,
            // bias tee, direct sampling, etc.) is carried across
            // the rebuild via the Source-trait snapshot hooks so
            // the reconnect lands with the pre-retry device state.
            // Per `CodeRabbit` round 3 on PR #408.
            if state.source_type != SourceType::RtlTcp {
                tracing::debug!(
                    active = ?state.source_type,
                    "RetryRtlTcpNow ignored — active source is not RtlTcp"
                );
                return;
            }
            if state.source.is_none() {
                tracing::debug!("RetryRtlTcpNow ignored — no live source (was disconnected)");
                return;
            }
            rebuild_rtl_tcp_source(state, dsp_tx, /* request_takeover */ false);
        }
        UiToDsp::RetryRtlTcpWithTakeover => {
            // One-shot Take-control reconnect per #396. Same
            // rebuild machinery as `RetryRtlTcpNow`, but with
            // `request_takeover = true` set on the rebuilt
            // config's `ClientHello`. The flag doesn't persist on
            // `DspState` — the next non-takeover retry or a
            // fresh `open_source` (Play after Stop, source-type
            // switch) rebuilds without it. Keeping takeover
            // "one-shot per action" matches the #393 spec:
            // takeover is an explicit user decision, not a
            // persistent preference.
            if state.source_type != SourceType::RtlTcp {
                tracing::debug!(
                    active = ?state.source_type,
                    "RetryRtlTcpWithTakeover ignored — active source is not RtlTcp"
                );
                return;
            }
            // Gate on a live source. After `DisconnectRtlTcp`
            // the source is gone (`state.source = None`) but
            // `state.source_type` remains `RtlTcp`, so a stale
            // "Take control" toast action could otherwise
            // recreate + start a fresh source here — breaking
            // the disconnect contract (reopen path after an
            // explicit disconnect is Play/Start, not a retry
            // command). Mirrors the `RetryRtlTcpNow` gate above.
            if state.source.is_none() {
                tracing::debug!(
                    "RetryRtlTcpWithTakeover ignored — no live source (was disconnected)"
                );
                return;
            }
            rebuild_rtl_tcp_source(state, dsp_tx, /* request_takeover */ true);
        }
        // --- Scanner (#317) ---
        UiToDsp::SetScannerEnabled(enabled) => {
            if enabled {
                if stop_any_recording(state, dsp_tx) {
                    let _ = dsp_tx.send(DspToUi::ScannerMutexStopped(
                        ScannerMutexReason::RecordingStoppedForScanner,
                    ));
                }
                if stop_transcription(state) {
                    let _ = dsp_tx.send(DspToUi::ScannerMutexStopped(
                        ScannerMutexReason::TranscriptionStoppedForScanner,
                    ));
                }
            }
            let cmds = state
                .scanner
                .handle_event(sdr_scanner::ScannerEvent::SetEnabled(enabled));
            apply_scanner_commands(state, dsp_tx, cmds);
        }
        UiToDsp::UpdateScannerChannels(channels) => {
            state.scanner_channels.clone_from(&channels);
            let cmds = state
                .scanner
                .handle_event(sdr_scanner::ScannerEvent::ChannelsChanged(channels));
            apply_scanner_commands(state, dsp_tx, cmds);
        }
        UiToDsp::LockoutScannerChannel(key) => {
            let cmds = state
                .scanner
                .handle_event(sdr_scanner::ScannerEvent::LockoutChannel(key));
            apply_scanner_commands(state, dsp_tx, cmds);
        }
        UiToDsp::UnlockScannerChannel(key) => {
            let cmds = state
                .scanner
                .handle_event(sdr_scanner::ScannerEvent::UnlockChannel(key));
            apply_scanner_commands(state, dsp_tx, cmds);
        }
    }
}

/// Reset the per-tune engine state that MUST NOT carry over a
/// frequency / demod / bandwidth change:
///
/// 1. The controller-side squelch-edge tracker
///    (`state.squelch_was_open`) — so a fresh `SquelchEdge::Open`
///    at the new channel isn't suppressed by the previous
///    channel's trailing open state. Originally added for the
///    scanner retune path (PR #372 round 3); the same risk
///    applies to every manual tune / mode / bandwidth change.
///
/// 2. Auto-squelch noise-floor tracking
///    (`state.radio.rearm_auto_squelch`) — the floor estimate
///    settles over seconds; carrying it from one band to
///    another leaves the threshold pinned to the wrong value,
///    so the new channel hard-opens (old floor was louder) or
///    stays hard-closed (old floor was quieter). No-op when
///    auto-squelch is disabled. Per issue #374.
///
/// Call from every UI-origin retune site (`UiToDsp::Tune`,
/// `SetDemodMode`, `SetBandwidth`) and the scanner retune
/// path. Cheap — two field writes plus an `if`-guarded reset.
fn on_tune_change(state: &mut DspState) {
    state.squelch_was_open = false;
    state.radio.rearm_auto_squelch();
}

/// Is audio or IQ recording currently active?
///
/// Used by future scanner tasks; suppress the unused-function lint
/// so it survives until the first call site arrives.
#[allow(dead_code)]
fn recording_active(state: &DspState) -> bool {
    state.audio_writer.is_some() || state.iq_writer.is_some()
}

/// Stop any active recording. Returns `true` if anything was
/// actually stopped (caller emits a mutex-stopped event only in
/// that case, avoiding spurious toasts when scanner enables
/// with nothing to stop).
fn stop_any_recording(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) -> bool {
    let mut stopped = false;
    if state.audio_writer.take().is_some() {
        let _ = dsp_tx.send(DspToUi::AudioRecordingStopped);
        stopped = true;
    }
    if state.iq_writer.take().is_some() {
        let _ = dsp_tx.send(DspToUi::IqRecordingStopped);
        stopped = true;
    }
    stopped
}

/// Stop the transcription tap. Returns `true` if it was active.
fn stop_transcription(state: &mut DspState) -> bool {
    if state.transcription_tx.take().is_some() {
        // Mirror the reset from the explicit DisableTranscription
        // handler — next EnableTranscription starts fresh.
        state.squelch_was_open = false;
        true
    } else {
        false
    }
}

/// Apply scanner-emitted commands to the DSP state.
fn apply_scanner_commands(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    commands: Vec<sdr_scanner::ScannerCommand>,
) {
    for cmd in commands {
        match cmd {
            sdr_scanner::ScannerCommand::Retune {
                freq_hz,
                demod_mode,
                bandwidth,
                ctcss,
                voice_squelch,
            } => {
                // Mirror the manual `Tune` / `SetDemodMode` /
                // `SetBandwidth` handlers so scanner hops end up
                // with the same persisted state + VFO rebuild
                // behavior. Omissions would leave `state.center_freq`
                // / `state.bandwidth` / the RxVfo config stale —
                // a subsequent `open_source()` restart would tune
                // back to whatever the user manually picked before
                // scanner started, and the IF filter width could
                // stay locked to the previous channel's mode.
                //
                // Deliberately NOT emitting the corresponding
                // `DspToUi::SampleRateChanged` / `DisplayBandwidth`
                // / `DemodModeChanged` / `BandwidthChanged` events
                // the manual handlers send — those are UI-sync
                // signals for user-initiated changes. Scanner
                // retunes carry their own `ScannerActiveChannelChanged`
                // payload with freq/mode/bandwidth/name that the
                // UI handler fans out to the same widgets; emitting
                // both paths would double-drive the sync.

                // Reset the squelch edge tracker AND re-arm the
                // auto-squelch noise-floor estimate for the new
                // channel. See `on_tune_change` for the full
                // rationale — both are critical: without the
                // edge reset a fresh `SquelchEdge::Open` would
                // be suppressed by a trailing-open state from
                // the previous channel (scanner invariant
                // `persistent_open_during_settle_goes_directly_to_listening`
                // relies on this); without the auto-squelch
                // re-arm the scanner inherits the previous
                // band's noise floor, which is the same bug
                // issue #374 describes for manual tunes.
                on_tune_change(state);

                // 1. Center frequency (mirrors `UiToDsp::Tune`).
                #[allow(clippy::cast_precision_loss)]
                let freq_f64 = freq_hz as f64;
                state.center_freq = freq_f64;
                if let Some(source) = state.source.as_mut()
                    && let Err(e) = source.tune(freq_f64)
                {
                    tracing::warn!(?e, "scanner retune: source.tune failed");
                }

                // 2. Demod mode + VFO rebuild on change (mirrors
                // `UiToDsp::SetDemodMode`). The scanner doesn't
                // emit retune commands redundantly — each Retune
                // marks a new channel — but the target mode may
                // equal the current mode (rotation pass on same-
                // mode channels), so guard to avoid gratuitous
                // rebuilds.
                let old_mode = state.radio.current_mode();
                if old_mode != demod_mode {
                    if let Err(e) = state.radio.set_mode(demod_mode) {
                        tracing::warn!(?e, "scanner retune: set_mode failed");
                    } else {
                        // Generic audio tap: same hard-boundary
                        // treatment the `UiToDsp::SetDemodMode` path
                        // applies. Scanner retunes deliberately
                        // suppress `DemodModeChanged` to the UI
                        // (per-hop chatter would be noise), which
                        // means FFI tap consumers never see the
                        // normal restart signal — so without this
                        // reset, one audio stream would span mixed
                        // demod outputs with stale 3:1 decimation
                        // phase state. Mirrors the treatment at
                        // L652-657 for the user-driven mode switch.
                        state.audio_tap_tx = None;
                        state.audio_tap_phase = 0;

                        // Auto-adjust decimation for the new
                        // demod's IF rate.
                        let if_rate = state.radio.demod_config().if_sample_rate;
                        let auto_decim = auto_decimation_ratio(state.sample_rate, if_rate);
                        if auto_decim != state.frontend.decim_ratio()
                            && let Err(e) = state.frontend.set_decimation(auto_decim)
                        {
                            tracing::warn!(?e, "scanner retune: auto-decimation failed");
                        }
                        // Rebuild the VFO for the new demod's IF
                        // rate + bandwidth. Bandwidth is set
                        // below; rebuild picks it up via
                        // `state.bandwidth`.
                        state.bandwidth = bandwidth;
                        if let Err(e) = rebuild_vfo(state) {
                            tracing::warn!(?e, "scanner retune: VFO rebuild failed");
                        }
                    }
                }

                // 3. Bandwidth (mirrors `UiToDsp::SetBandwidth`).
                // Applied to the VFO channel filter first; only
                // persist on success. For same-mode retunes the
                // VFO already exists; for mode-change retunes the
                // rebuild above already used `state.bandwidth`
                // so the two paths converge.
                if let Some(vfo) = &mut state.vfo {
                    match vfo.set_bandwidth(bandwidth) {
                        Ok(()) => state.bandwidth = bandwidth,
                        Err(e) => {
                            tracing::warn!(?e, "scanner retune: VFO bandwidth update failed");
                        }
                    }
                } else {
                    state.bandwidth = bandwidth;
                }
                state.radio.set_bandwidth(bandwidth);

                // 4. CTCSS is per-channel: force-Off when the new
                // channel doesn't carry a tone, otherwise a stale
                // tone gate would silence the new channel.
                let ctcss_mode = ctcss.unwrap_or(sdr_radio::af_chain::CtcssMode::Off);
                if let Err(e) = state.radio.set_ctcss_mode(ctcss_mode) {
                    tracing::warn!(?e, "scanner retune: set_ctcss_mode failed");
                }
                // 5. Voice squelch is device-level — preserve
                // current setting when the channel doesn't
                // override it.
                if let Some(m) = voice_squelch
                    && let Err(e) = state.radio.set_voice_squelch_mode(m)
                {
                    tracing::warn!(?e, "scanner retune: set_voice_squelch_mode failed");
                }
            }
            sdr_scanner::ScannerCommand::MuteAudio(muted) => {
                state.scanner_muted = muted;
            }
            sdr_scanner::ScannerCommand::ActiveChannelChanged(key) => {
                emit_scanner_active_channel(state, dsp_tx, key);
            }
            sdr_scanner::ScannerCommand::StateChanged(scanner_state) => {
                let _ = dsp_tx.send(DspToUi::ScannerStateChanged(scanner_state));
            }
            sdr_scanner::ScannerCommand::EmptyRotation => {
                let _ = dsp_tx.send(DspToUi::ScannerEmptyRotation);
            }
        }
    }
}

/// Build the `ScannerActiveChannelChanged` payload by looking
/// up the full channel info for the given key in the cached
/// channel list.
///
/// If `key` is `Some(k)` but `k` isn't in `scanner_channels`
/// (a race between `UpdateScannerChannels` and
/// `ActiveChannelChanged`), we degrade to the idle-shape payload
/// (`key = None`, zeroed fields) rather than sending a non-None
/// key with zeroed freq/bandwidth/name — the UI can't tell
/// those apart from a valid zero-frequency channel, and the
/// resulting display would be incoherent (key says "active
/// channel X" but fields say "no channel"). A warning log
/// surfaces the cache miss so this stays diagnosable if it ever
/// fires in practice.
#[allow(
    clippy::needless_pass_by_value,
    reason = "owned key is passed from ScannerCommand::ActiveChannelChanged \
              and this helper decides whether it lands in the outgoing DspToUi \
              payload (cache hit) or gets logged + dropped (cache miss); \
              taking a reference would force callers to clone unnecessarily \
              on the common-case hit path"
)]
fn emit_scanner_active_channel(
    state: &DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    key: Option<sdr_scanner::ChannelKey>,
) {
    let channel = key
        .as_ref()
        .and_then(|k| state.scanner_channels.iter().find(|c| c.key == *k).cloned());
    if key.is_some() && channel.is_none() {
        tracing::warn!(
            ?key,
            "scanner active-channel key not found in cached ScannerChannels — \
             degrading to idle payload; likely an UpdateScannerChannels race"
        );
    }
    let msg = match channel {
        Some(c) => DspToUi::ScannerActiveChannelChanged {
            freq_hz: c.key.frequency_hz,
            demod_mode: c.demod_mode,
            bandwidth: c.bandwidth,
            name: c.key.name.clone(),
            // CTCSS + voice-squelch mirror the channel record
            // verbatim (including `None`). The UI decides how to
            // interpret `None` — CTCSS forces the row to Off to
            // match the scanner's engine-side behavior;
            // voice-squelch leaves the row alone, also matching
            // the scanner's "no override → preserve" contract.
            ctcss: c.ctcss,
            voice_squelch: c.voice_squelch,
            key: Some(c.key),
        },
        None => DspToUi::ScannerActiveChannelChanged {
            freq_hz: 0,
            demod_mode: sdr_types::DemodMode::Nfm,
            bandwidth: 0.0,
            name: String::new(),
            ctcss: None,
            voice_squelch: None,
            key: None,
        },
    };
    let _ = dsp_tx.send(msg);
}

/// Destroy the current `RtlTcpSource` and construct a fresh one
/// with the latest role / `auth_key` config from `DspState`, then
/// start it. Used by both `RetryRtlTcpNow` (ordinary manual
/// retry after an `AuthRequired` / `AuthFailed` / `ControllerBusy`
/// denial) and `RetryRtlTcpWithTakeover` (the #393 "Take
/// control" one-shot).
///
/// **Why rebuild instead of stop+start the existing source:** the
/// `ClientHello` is built at `with_config(...)` time from the
/// `RtlTcpConfig` passed to the constructor. Calling `start()` on
/// the same instance replays its original hello — a newly entered
/// auth key or a flipped role would never land on the wire until
/// a full source tear-down (Stop + Play, source-type switch).
/// Rebuilding picks up the current `state.rtl_tcp_requested_role`
/// and `state.rtl_tcp_auth_key` for the next hello, which is the
/// behavior the UI expects after any denial arm.
///
/// **Sticky-command cache:** the previous source's replay
/// snapshot (gain, AGC, PPM, bias tee, direct sampling, etc.) is
/// captured via `Source::rtl_tcp_sticky_snapshot()` and restored
/// onto the new instance BEFORE `start()` so the reconnect's
/// `replay_sticky_commands` emits the same setters the old
/// session had. Without this, a takeover / auth-retry rebuild
/// would reset device state to defaults (gain = 0, AGC off, PPM
/// = 0, ...) and the user would lose their tuning setup.
///
/// `request_takeover` is the one bit of per-call config not read
/// from `DspState` — we keep takeover as an explicit one-shot
/// parameter so the next non-takeover retry or `open_source` call
/// cleanly starts without the flag.
///
/// Caller must have already ensured `state.source_type ==
/// RtlTcp` and `state.source.is_some()`. Per `CodeRabbit` round
/// 3 on PR #408.
fn rebuild_rtl_tcp_source(
    state: &mut DspState,
    dsp_tx: &std::sync::mpsc::Sender<DspToUi>,
    request_takeover: bool,
) {
    let error_prefix = if request_takeover {
        "Take control failed"
    } else {
        "Retry failed"
    };
    // Snapshot the replay cache + drop the old source. The
    // Source-trait hook returns `None` for non-RtlTcp sources —
    // can't happen here given the caller's `source_type` gate,
    // but `unwrap_or_default()` keeps this defensive.
    let sticky_snapshot = state
        .source
        .as_ref()
        .and_then(|s| s.rtl_tcp_sticky_snapshot())
        .unwrap_or_default();
    if let Some(mut source) = state.source.take()
        && let Err(e) = source.stop()
    {
        tracing::warn!(
            error = %e,
            request_takeover,
            "rtl_tcp stop before rebuild failed"
        );
    }
    // Build the fresh config from the latest DspState.
    // `Default` covers timeouts + compression; role and auth
    // come from state, and `request_takeover` is the caller's
    // one-shot choice.
    let rtl_tcp_config = sdr_source_network::rtl_tcp::RtlTcpConfig {
        requested_role: state.rtl_tcp_requested_role,
        auth_key: state.rtl_tcp_auth_key.clone(),
        request_takeover,
        ..Default::default()
    };
    let mut source: Box<dyn Source> = Box::new(sdr_source_network::RtlTcpSource::with_config(
        &state.network_host,
        state.network_port,
        rtl_tcp_config,
    ));
    // Restore sticky cache BEFORE `start()` so the manager
    // thread's `replay_sticky_commands` call on the freshly-
    // opened stream already sees the pre-rebuild values.
    source.rtl_tcp_restore_sticky_snapshot(&sticky_snapshot);
    // Reapply sample rate + tune on the new instance — these
    // are derived from `DspState`, not the snapshot, because
    // they can change between the snapshot and the restart
    // (e.g., a user sample-rate switch while the old source
    // was in `AuthRequired`). Both calls also update the
    // sticky cache on the new source, which is fine — any
    // subsequent reconnect replays the fresher value.
    if let Err(e) = source.set_sample_rate(state.configured_sample_rate) {
        tracing::warn!(
            error = %e,
            request_takeover,
            "rtl_tcp rebuild set_sample_rate failed"
        );
    }
    if let Err(e) = source.tune(state.center_freq) {
        tracing::warn!(
            error = %e,
            request_takeover,
            "rtl_tcp rebuild tune failed"
        );
    }
    if let Err(e) = source.start() {
        tracing::warn!(
            error = %e,
            request_takeover,
            "rtl_tcp rebuild start failed"
        );
        let _ = dsp_tx.send(DspToUi::Error(format!("{error_prefix}: {e}")));
        return;
    }
    state.source = Some(source);
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
        SourceType::File => {
            // Apply the persisted loop flag to the freshly-
            // constructed source so a replay after a path
            // change honors the latest setting — without
            // this, switching files would reset looping to
            // the constructor default. Per issue #236.
            let mut fs = sdr_source_file::FileSource::new(&state.file_path);
            fs.set_looping(state.file_looping);
            Box::new(fs)
        }
        // rtl_tcp client: connects to a remote `rtl_tcp`-compatible
        // server, handshakes the 12-byte RTL0 header, and routes
        // future tune / gain / PPM messages through the 5-byte
        // command channel. Reuses the `network_host` + `network_port`
        // config fields for address, but also threads the #396
        // `requested_role` + `auth_key` fields from state into an
        // `RtlTcpConfig` so the hello carries the user's choices.
        //
        // **Capability signals, in narrow scope:**
        // - `codecs=3` in the mDNS TXT record says "this server
        //   parses `ClientHello`, so a hello won't be mis-framed as
        //   two 5-byte commands." That makes `compression`,
        //   `request_takeover`, and `requested_role` opt-ins wire-
        //   safe on this server. It does **NOT** prove the server
        //   supports auth — auth uses the v2 protocol path, which
        //   `codecs=3` doesn't speak to.
        // - Auth capability is a separate signal: the #394 servers
        //   advertise `auth_required` (present in the mDNS TXT
        //   record and persisted on `FavoriteEntry`) AND accept
        //   hellos with `PROTOCOL_VERSION_V2`. Eager-auth hellos
        //   therefore require v2 — `required_protocol_version(flags)`
        //   in `sdr-source-network` picks the minimum viable version
        //   from the flag set, returning v2 when `FLAG_HAS_AUTH` is
        //   set. Sending an auth-bearing hello to a `codecs=3`-only
        //   server that doesn't understand v2 will bounce at the
        //   server's version gate.
        //
        // The source panel's discovery gating is responsible for
        // refusing role / compression / takeover opt-ins against
        // legacy-only (non-`codecs=3`) servers, and for only
        // offering the auth field on servers that advertise
        // `auth_required`. Per #396 / `CodeRabbit` round 2 on
        // PR #408.
        SourceType::RtlTcp => {
            let rtl_tcp_config = sdr_source_network::rtl_tcp::RtlTcpConfig {
                requested_role: state.rtl_tcp_requested_role,
                auth_key: state.rtl_tcp_auth_key.clone(),
                ..Default::default()
            };
            Box::new(sdr_source_network::RtlTcpSource::with_config(
                &state.network_host,
                state.network_port,
                rtl_tcp_config,
            ))
        }
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
fn cleanup(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    if let Some(source) = &mut state.source {
        let _ = source.stop();
    }

    // Snapshot whether the network sink was active BEFORE we
    // stop it so the post-stop status emit reports the right
    // discriminant. Centralized here (rather than at each
    // caller) so file-EOF, fatal-source-error, and source-type
    // restart paths all emit the matching `Inactive` event
    // alongside the explicit `UiToDsp::Stop` path. Per
    // `CodeRabbit` round 6 on PR #351.
    let was_network_sink = matches!(state.audio_sink_type, AudioSinkType::Network);

    // Stop the audio sink so it doesn't try to read stale data.
    if let Err(e) = state.audio_sink.stop() {
        tracing::debug!("audio sink stop: {e}");
    }

    if was_network_sink {
        let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive));
    }

    // Finalize any active recordings (Drop patches the WAV header sizes).
    if state.audio_writer.take().is_some() {
        tracing::info!("audio recording finalized on cleanup");
    }
    if state.iq_writer.take().is_some() {
        tracing::info!("IQ recording finalized on cleanup");
    }

    state.source = None;

    // Hard stream discontinuity — flush APT decoder state so a
    // subsequent Start can't bleed pre-stop accumulator/ready
    // lines into the new session and emit a stale first line.
    // The decoder itself stays allocated so the next Start
    // doesn't pay re-init cost (filter taps, resampler tables);
    // we only clear its in-flight buffers via `AptDecoder::reset`.
    // Cross-mode preservation (NFM → WFM → NFM mid-pass) is a
    // *soft* discontinuity and intentionally stays untouched —
    // the user keeps decoding the same pass.
    if let Some(decoder) = state.apt_decoder.as_mut() {
        decoder.reset();
    }
    state.apt_mono_buf.clear();
    // Clear the failed-init guard so a fresh Start gets a fresh
    // init attempt — the user may have tweaked the radio audio
    // rate between sessions, and we don't want a stale failure
    // memo to silently suppress a now-valid rate.
    state.apt_init_failed_at_rate = None;

    // Same semantics for the LRPT decoder (#469): flush
    // in-flight Viterbi traceback / sync window / RS path /
    // image assembler so the next Start paints a clean canvas.
    // If reset's demod-rebuild fails (practically unreachable;
    // see `LrptDemod::new`), drop the decoder entirely so the
    // next tap call lazily re-initialises.
    if let Some(decoder) = state.lrpt_decoder.as_mut()
        && let Err(e) = decoder.reset()
    {
        tracing::warn!("LRPT decoder reset failed; dropping for re-init: {e}");
        state.lrpt_decoder = None;
    }

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
                cleanup(state, dsp_tx);
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
                cleanup(state, dsp_tx);
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

                // Meteor-M LRPT decode tap (#469). Only runs in
                // LRPT mode — the demod is a silent passthrough
                // sized so `radio_input` is at the LRPT working
                // sample rate (144 ksps), which is exactly what
                // the QPSK demod + FEC chain expects. Tapped
                // BEFORE `radio.process` so we read the IQ
                // before the passthrough discards it; harvested
                // scan lines flow into the shared `LrptImage`
                // the live viewer reads from.
                if state.radio.current_mode() == sdr_types::DemodMode::Lrpt {
                    lrpt_decode_tap(
                        &mut state.lrpt_decoder,
                        state.lrpt_image.as_ref(),
                        radio_input,
                    );
                }

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

                        // NOAA APT decode tap (#482). Only runs in
                        // NFM mode — the APT 2400 Hz subcarrier rides
                        // on a Wide-FM-style demod with a narrow
                        // (~38 kHz) channel filter, which the user's
                        // NFM mode is set up for. WFM's deemphasis
                        // would smear the subcarrier; AM/SSB don't
                        // demodulate it at all. Pre-volume audio
                        // (this point) so the volume knob doesn't
                        // affect decode quality. Worker is the DSP
                        // thread — `AptDecoder` is internally
                        // single-threaded which fits perfectly.
                        if audio_count > 0
                            && state.radio.current_mode() == sdr_types::DemodMode::Nfm
                        {
                            apt_decode_tap(state, dsp_tx, audio_count);
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

                        // Feed the scanner the squelch edge regardless of demod
                        // mode — the scanner's rotation state transitions
                        // (Dwelling→Listening, Listening→Hanging) apply to any
                        // mode. This runs outside the transcription gate below so
                        // the scanner sees every transition even when the
                        // transcription tap is inactive.
                        let now_open = state.radio.if_chain().squelch_open();
                        if now_open != state.squelch_was_open {
                            let scanner_edge = if now_open {
                                sdr_scanner::SquelchState::Open
                            } else {
                                sdr_scanner::SquelchState::Closed
                            };
                            let scan_cmds = state
                                .scanner
                                .handle_event(sdr_scanner::ScannerEvent::SquelchEdge(scanner_edge));
                            apply_scanner_commands(state, dsp_tx, scan_cmds);
                        }

                        // Send audio copy to transcription worker BEFORE volume
                        // scaling so recognition isn't affected by the volume knob. Also
                        // emit squelch edge events on open/close transitions so offline
                        // sherpa backends can use them as Auto Break segmentation
                        // boundaries. Edge events are NFM-only — WFM and other modes
                        // don't have meaningful squelch transitions for speech.
                        if let Some(ref tx) = state.transcription_tx {
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
                        } else {
                            // No transcription tap: advance the tracker
                            // unconditionally so the scanner doesn't see
                            // the same edge repeatedly on every block.
                            state.squelch_was_open = now_open;
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

                        // Write to audio recording file BEFORE the
                        // volume scale (closes #532). The recorder is
                        // a diagnostic artifact — it should capture
                        // what the demodulator produced, not what the
                        // speaker played. A muted overnight pass used
                        // to fill 200+ MB of disk with all-zero
                        // samples; the user only discovered the bug
                        // when they tried to replay the WAV. Now the
                        // recording is independent of the volume knob,
                        // matching the APT decoder tap (line ~2632)
                        // which is also pre-volume.
                        if let Some(writer) = &mut state.audio_writer
                            && let Err(e) = writer.write_stereo(&state.audio_buf[..audio_count])
                        {
                            tracing::warn!("audio recording write error: {e}");
                            state.audio_writer = None;
                            let _ = dsp_tx
                                .send(DspToUi::Error("Audio recording write failed".to_string()));
                            let _ = dsp_tx.send(DspToUi::AudioRecordingStopped);
                        }

                        // Apply volume with perceptual (power-law) scaling.
                        // Quadratic curve maps the linear slider to perceived loudness.
                        let vol = state.volume * state.volume;
                        for s in &mut state.audio_buf[..audio_count] {
                            s.l *= vol;
                            s.r *= vol;
                        }

                        // Scanner mute: fill the audio buffer with
                        // silence in-place when the scanner is in a
                        // non-Listening phase (Retuning / Dwelling /
                        // Hanging). The DSP chain still runs — we only
                        // silence the PCM that reaches the audio device.
                        // No allocation per block; `slice.fill` overwrites
                        // existing contents.
                        if state.scanner_muted {
                            state.audio_buf[..audio_count].fill(sdr_types::Stereo::default());
                        }

                        // Send to the audio sink (PipeWire on Linux,
                        // CoreAudio on macOS).
                        if audio_count > 0 {
                            state.audio_frames_written = state
                                .audio_frames_written
                                .saturating_add(audio_count as u64);
                        }
                        // Skip the write if the sink has already
                        // gone offline this session. Without this
                        // gate, every audio block would re-trip
                        // the terminal-error branch below (the
                        // sink stays in place after stop(), so
                        // `write_samples` keeps returning
                        // NotRunning) and re-emit the same
                        // status/error event at DSP cadence —
                        // ~50 events/sec of log noise + UI churn.
                        // Per `CodeRabbit` round 2 on PR #351.
                        // Cleared on the next successful start.
                        if !state.audio_sink_offline
                            && let Err(e) = state
                                .audio_sink
                                .write_samples(&state.audio_buf[..audio_count])
                        {
                            // Terminal failures: surface to UI once and stop the sink.
                            if matches!(e, SinkError::Disconnected | SinkError::NotRunning) {
                                tracing::warn!(
                                    sink_type = ?state.audio_sink_type,
                                    "audio sink died: {e}"
                                );
                                // Distinct event for the network sink so the
                                // settings panel's status row can update
                                // independently of the toast for local
                                // device failures. Per issue #247.
                                if matches!(state.audio_sink_type, AudioSinkType::Network) {
                                    let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(
                                        NetworkSinkStatus::Error {
                                            message: format!("{e}"),
                                        },
                                    ));
                                } else {
                                    let _ = dsp_tx.send(DspToUi::Error(
                                        "Audio output lost — restart playback".to_string(),
                                    ));
                                }
                                let _ = state.audio_sink.stop();
                                // Latch — see the docstring on
                                // `audio_sink_offline` for the
                                // full one-shot rationale.
                                state.audio_sink_offline = true;
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

            // Feed the sample tick into the scanner. Scanner uses this to
            // drive settle/dwell/hang countdowns — decoupled from radio
            // output. `NonZeroU32` enforces the rate invariant at the
            // event type level; if `state.sample_rate` ever truncates
            // to 0 we skip the tick and warn rather than panicking the
            // DSP thread. Any live source has a non-zero rate; this is
            // defense against future state-init bugs, not a hot-path
            // concern.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let sample_rate_u32 = state.sample_rate as u32;
            if let Some(sample_rate_hz) = std::num::NonZeroU32::new(sample_rate_u32) {
                #[allow(clippy::cast_possible_truncation)]
                let tick_cmds = state
                    .scanner
                    .handle_event(sdr_scanner::ScannerEvent::SampleTick {
                        samples_consumed: iq_count as u32,
                        sample_rate_hz,
                    });
                apply_scanner_commands(state, dsp_tx, tick_cmds);
            } else {
                tracing::warn!(
                    sample_rate = state.sample_rate,
                    "scanner sample tick skipped: source sample rate is 0 after u32 cast"
                );
            }
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
