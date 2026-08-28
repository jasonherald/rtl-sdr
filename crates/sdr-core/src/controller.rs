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
use sdr_radio::lrpt_decoder::{LrptDecoder, LrptDownlink};
use slowrx::SstvDecoder;

use crate::sink_slot::{
    AudioSinkSlot, AudioSinkType, DEFAULT_NETWORK_SINK_HOST, DEFAULT_NETWORK_SINK_PORT,
    DEFAULT_NETWORK_SINK_PROTOCOL, NetworkSinkStatus,
};
use sdr_radio::RadioModule;
// `AudioSink` and `NetworkSink` are no longer used directly here —
// both live behind `AudioSinkSlot` (see `crate::sink_slot`) so the
// controller's audio path stays uniform regardless of which sink
// the user has selected.
use sdr_source_rtlsdr::{RtlSdrSource, apply_bias_tee_idle};
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

/// Minimum interval between successive
/// "transcription channel full; retrying squelch edge next block"
/// warnings. Without throttling, the warning fires on every DSP
/// block the edge stays pending — at typical block cadence
/// that's 100+ lines/sec for as long as the worker is decoding,
/// which buries the rest of the trace log. The suppressed-count
/// is reported alongside the next emitted warning so the
/// operator can see the burst magnitude without the line spam.
const TRANSCRIPTION_FULL_WARN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

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

    let mut state = match DspState::new(dsp_tx.clone()) {
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

use crate::acars_output::AcarsOutputs;

mod acars;
mod apt;
mod audio;
mod lrpt;
mod radio;
mod scanner;
mod source;
mod sstv;
mod transcription;

use acars::{
    AcarsHandlerOutcome, acars_decode_tap, acars_lock_rejects_geometry_change,
    apply_acars_geometry, handle_set_acars_enabled, handle_set_acars_jsonl_enabled,
    handle_set_acars_jsonl_path, handle_set_acars_network_addr, handle_set_acars_network_enabled,
    handle_set_acars_station_id,
};
use apt::apt_decode_tap;
use audio::{iq_recording_rejects_rate_change, recording_write_error_message, stop_any_recording};
use lrpt::lrpt_decode_tap;
use scanner::{apply_scanner_commands, scanner_carrier_present};
use source::{
    auto_decimation_ratio, on_tune_change, poll_rtl_tcp_connection_state, rebuild_frontend,
    rebuild_vfo_echoing,
};
use sstv::{SstvPassStats, sstv_decode_tap};
use transcription::stop_transcription;

// Only the unit tests drive these helpers from the controller-root scope;
// production callers live inside their own modules.
#[cfg(test)]
use source::{rebuild_vfo, rtl_sdr_pre_start_settings};
#[cfg(test)]
use sstv::handle_sstv_event;

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
    /// IQ-imbalance correction engaged (independent of `dc_blocking`; #692).
    iq_correction: bool,
    invert_iq: bool,
    window_fn: FftWindow,
    fft_rate: f64,
    /// Master FFT compute gate. Mirrors `IqFrontend::fft_enabled` and
    /// is reapplied to any newly-built frontend so a frontend rebuild
    /// (e.g. on source switch / decimation change) doesn't silently
    /// resume the waterfall when the user had it toggled off. Driven
    /// by `UiToDsp::SetFftEnabled` (#646 / #647).
    fft_enabled: bool,
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
    /// RTL-SDR USB bias-T (5 V on coax) toggle. Persisted in
    /// state so a `SetBiasTee` dispatched while no source is
    /// open isn't silently dropped — `open_source` re-applies
    /// the value to the freshly-built RTL-SDR source. Without
    /// this, first-play after restart would always start with
    /// bias-T off regardless of the persisted UI switch.
    /// Per CR round 1 on PR #550.
    bias_tee_enabled: bool,
    /// Upconverter offset in Hz — replayed to the source on every
    /// open so the setting survives stop/start (#848 phase 4).
    converter_offset_hz: f64,
    /// Airspy device serial to open (`None` = first available) —
    /// applied by the source factory at the next open (#848 phase 5).
    airspy_serial: Option<u64>,
    /// RTL-SDR direct-sampling mode (0 = disabled, 1 = I, 2 = Q).
    /// Persist-and-replay companion to `bias_tee_enabled` —
    /// see [issue `#551`].
    direct_sampling_mode: i32,
    /// RTL-SDR tuner offset-tuning toggle (E4000-only, harmless
    /// no-op on other tuners). Per issue `#551`.
    offset_tuning_enabled: bool,
    /// RTL2832U digital AGC (separate from the tuner AGC tracked
    /// by `tuner_agc_auto`). Per issue `#551`.
    rtl_agc_enabled: bool,
    /// Tuner gain mode — `true` = automatic (AGC), `false` =
    /// manual (caller-supplied gain). Per issue `#551`.
    tuner_agc_auto: bool,
    /// Manual tuner gain in tenths of a dB (librtlsdr units).
    /// Only meaningful when `tuner_agc_auto` is `false` — applied
    /// unconditionally on replay because librtlsdr ignores it
    /// while AGC is on. Per issue `#551`.
    tuner_gain_tenths_db: i32,
    /// Manual gain index for `UiToDsp::SetGainByIndex` (FFI-side
    /// alternative to `tuner_gain_tenths_db`). `None` until the
    /// caller has explicitly chosen one — replay skipped while
    /// `None`. Per issue `#551`.
    tuner_gain_index: Option<u32>,
    /// Clock PPM correction. Per issue `#551`.
    ppm_correction: i32,

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
    /// from `transcription_tx` so embedders can receive
    /// recognizer-ready samples without pulling in the
    /// sdr-transcription dependency.
    audio_tap_tx: Option<std::sync::mpsc::SyncSender<Vec<f32>>>,

    /// Decimation phase carried across `stereo_48k_to_mono_16k`
    /// calls on the audio tap path. Without it, successive DSP
    /// blocks whose lengths aren't multiples of 3 would produce
    /// duplicate / dropped samples at block boundaries. Reset on
    /// `EnableAudioTap` so a fresh session starts at phase 0. Per
    /// `CodeRabbit` round 1 on PR #349.
    audio_tap_phase: usize,

    /// Last known squelch gate state, used by the SCANNER edge detector
    /// to emit one `ScannerEvent::SquelchEdge` per transition instead
    /// of one per audio chunk. Initialized to `false` (matches
    /// `IfChain`'s initial closed state). Reset on tune / mode /
    /// bandwidth boundaries so a fresh open at the new channel always
    /// emits an Open edge. Per `CodeRabbit` round 1 on PR #558 — the
    /// scanner ↔ transcription mutex removal exposed that the same
    /// field used to be reset by `EnableTranscription` /
    /// `DisableTranscription`, which now perturbs the scanner's edge
    /// detection. The transcription tap has its own
    /// `transcription_squelch_was_open` tracker below.
    squelch_was_open: bool,

    /// Last known squelch gate state, used by the TRANSCRIPTION audio
    /// tap to decide when to emit `TranscriptionInput::SquelchOpened`
    /// / `SquelchClosed` edge messages. Independent of
    /// `squelch_was_open` so toggling the transcription tap (which
    /// resets this tracker) doesn't fire a spurious scanner edge on
    /// the next block. Per `CodeRabbit` round 1 on PR #558.
    transcription_squelch_was_open: bool,

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

    /// Last wall-clock instant we emitted the
    /// "transcription channel full; retrying squelch edge next
    /// block" warning. Used to throttle the warning to one per
    /// `TRANSCRIPTION_FULL_WARN_INTERVAL` window — without this,
    /// a backed-up worker (sherpa Moonshine inference taking 1–2
    /// seconds, Whisper taking 5+) would log on every DSP block
    /// the edge stays pending, drowning the rest of the trace
    /// log at 100+ lines/sec. Per FYI-flood reported during PR
    /// for issues #538 / #539.
    transcription_full_warn_at: std::time::Instant,
    /// Count of warnings suppressed by the throttle since the
    /// last emitted warning. Logged alongside the next warn so
    /// the operator can see how bad the backpressure burst was
    /// rather than "one per second forever" obscuring the
    /// magnitude.
    transcription_full_suppressed: u32,

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
    /// is well above the decoder's 10.6 kHz input-rate floor.
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
    /// One-shot guard: set to `true` when `LrptDecoder::new`
    /// fails, so subsequent `lrpt_decode_tap` calls return
    /// early instead of retrying init (and warn-logging) on
    /// every IQ chunk. Cleared in `cleanup` so a fresh `Start`
    /// gets a fresh attempt. Mirrors `apt_init_failed_at_rate`
    /// — without this, a sustained init failure would spam the
    /// log at the IQ block rate (~100 Hz) until source-stop.
    /// Per `CodeRabbit` round 12 on PR #543.
    lrpt_init_failed: bool,
    /// Downlink profile the LRPT decoder should be built with on
    /// the next lazy-init. Set by `UiToDsp::SetLrptDownlink` from
    /// the wiring layer (which knows the current satellite from
    /// the `KnownSatellite` catalog). Defaults to plain OQPSK
    /// because every active Meteor satellite (M2-3, M2-4)
    /// transmits that — manual LRPT-mode use without a
    /// satellite-aware caller still works for current Meteors. A
    /// change here drops the existing decoder so the next IQ chunk
    /// re-inits with the new profile. Per #662 / #730.
    lrpt_downlink: LrptDownlink,
    /// ISS SSTV decoder. Lazy-init on first `sstv_decode_tap` call
    /// at the `RadioModule`'s current audio sample rate (typically
    /// 48 kHz). The decoder internally resamples to `11_025` Hz, so
    /// any rate within `slowrx`'s accepted range is fine.
    /// `None` means "not yet built" — built once, kept across demod
    /// toggles so re-entering NFM during a pass picks up where it
    /// left off. Per epic #472.
    sstv_decoder: Option<slowrx::SstvDecoder>,
    /// Pre-allocated mono downmix buffer for the SSTV decoder input.
    /// Reused across DSP blocks, resized in place each call so we
    /// don't alloc inside the hot loop. Mirrors `apt_mono_buf`.
    sstv_mono_buf: Vec<f32>,
    /// Per-SSTV-pass statistics. Reset between passes by
    /// `reset_imaging_decoders`, which also logs a summary line so
    /// a `grep "SSTV pass summary"` of the log shows whether each
    /// pass produced imagery, partial frames, or just VIS detections
    /// without complete images. Important for duty-cycled events
    /// like ARISS Series 32 where a pass might yield 1-3 complete
    /// images out of 3-4 detected VIS bursts. Per #648.
    sstv_pass_stats: SstvPassStats,
    /// Most recent audio sample rate that `SstvDecoder::new` rejected,
    /// or `None` if every prior init succeeded / hasn't been tried.
    /// Guards against the audio-block hot loop retrying on a rate the
    /// decoder will never accept. Cleared in `cleanup` alongside the
    /// decoder reset so a fresh source restart gets one fresh attempt.
    /// Mirrors `apt_init_failed_at_rate`. Per epic #472.
    sstv_init_failed_at_rate: Option<u32>,
    /// Shared SSTV image handle. `None` until the UI side wires it
    /// via `UiToDsp::SetSstvImage`; when `None` the tap runs but
    /// lines are discarded (manual NFM use without a viewer is
    /// silent-but-harmless). Set at AOS by the auto-record wiring;
    /// cleared at LOS by `UiToDsp::ClearSstvImage`. Per epic #472.
    sstv_image: Option<sdr_radio::sstv_image::SstvImageHandle>,
    /// Live ACARS bank. May be temporarily `None` while ACARS
    /// is still engaged — specifically, the `Start` path
    /// invalidates this so `acars_decode_tap`'s lazy-init can
    /// rebuild at the live streaming rate (which differs from
    /// the engage-time rate when the device rounds). Use
    /// **`acars_pre_lock.is_some()` as the canonical "ACARS
    /// engaged" signal**; `acars_bank.is_some()` is only
    /// meaningful where the bank object itself is needed
    /// (per-block stats emission, lazy-init self-check inside
    /// the tap). Per CR rounds 4-5 on PR #584.
    acars_bank: Option<sdr_acars::ChannelBank>,
    /// Snapshot of the prior source config taken at engage.
    /// Used by disengage to restore the user's tuning.
    /// (Consumer lands in T7 `handle_set_acars_enabled`.)
    acars_pre_lock: Option<crate::acars_airband_lock::PreLockSnapshot>,
    /// One-shot guard: a previous `ChannelBank::new` failed.
    /// Mirrors `lrpt_init_failed` — prevents warn-spam on
    /// every subsequent IQ block. Cleared on source-stop.
    /// (Consumer lands in T6 `acars_decode_tap`.)
    acars_init_failed: bool,
    /// Last `DspToUi::AcarsChannelStats` emission timestamp.
    /// Throttles stats emission to ~1 Hz per spec.
    /// (Consumer lands in T8 stats-throttle in `process_iq_block`.)
    acars_stats_emitted_at: std::time::Instant,
    /// Region selecting which ACARS channel set the airband
    /// lock tunes to (issue #581). Defaults to US-6 — the
    /// only region available pre-#581. Set by the UI via
    /// `UiToDsp::SetAcarsRegion` before engage; read at
    /// engage time to pick channels + center.
    acars_region: crate::acars_airband_lock::AcarsRegion,
    /// Output-writer bundle: bounded `mpsc` channel to a
    /// dedicated writer thread that owns the JSONL file +
    /// UDP socket. Shared `Arc<RwLock<AcarsWriterConfig>>`
    /// holds the runtime-mutable jsonl path / network addr /
    /// station ID; the DSP thread calls `try_send` per
    /// decoded message. Issues #578 + #596.
    acars_outputs: AcarsOutputs,
    /// Most-recent user-set JSONL destination, preserved across
    /// disable/enable toggles so re-enabling restores the user's
    /// previously-chosen path rather than the default. Mirrors
    /// what would otherwise live in `AcarsWriterConfig.jsonl_path`
    /// but separated so the writer's `Some/None` "is enabled"
    /// semantics stay clean. CR round 2 on PR #598.
    acars_last_user_jsonl_path: Option<std::path::PathBuf>,
    /// Same pattern as `acars_last_user_jsonl_path` for the
    /// UDP feeder. CR round 2 on PR #598.
    acars_last_user_network_addr: Option<String>,
}

impl DspState {
    // One flat struct literal initializing every controller field —
    // splitting it would need Default impls the DSP members can't
    // provide. Same allowance as handle_command / process_iq_block.
    #[allow(clippy::too_many_lines)]
    fn new(dsp_tx: mpsc::Sender<DspToUi>) -> Result<Self, String> {
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
            iq_correction: false,
            invert_iq: false,
            window_fn: FftWindow::Nuttall,
            fft_rate: DEFAULT_FFT_RATE,
            // Default-on so existing setup paths that don't explicitly
            // toggle the gate keep historical behavior. UI is the only
            // caller that turns this off (Display sidebar toggle for
            // #646; window-minimize handler for #647).
            fft_enabled: true,
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
            bias_tee_enabled: false,
            converter_offset_hz: 0.0,
            airspy_serial: None,
            direct_sampling_mode: 0,
            offset_tuning_enabled: false,
            rtl_agc_enabled: false,
            tuner_agc_auto: false,
            tuner_gain_tenths_db: 0,
            tuner_gain_index: None,
            ppm_correction: 0,
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
            transcription_squelch_was_open: false,
            ctcss_was_sustained: false,
            voice_squelch_was_open: true,
            audio_frames_written: 0,
            iq_samples_read: 0,
            diag_log_at: std::time::Instant::now(),
            transcription_full_warn_at: std::time::Instant::now(),
            transcription_full_suppressed: 0,
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
            lrpt_init_failed: false,
            lrpt_downlink: LrptDownlink::new(sdr_dsp::lrpt::LrptMode::Oqpsk, false),
            sstv_decoder: None,
            sstv_mono_buf: Vec::new(),
            sstv_init_failed_at_rate: None,
            sstv_pass_stats: SstvPassStats::default(),
            sstv_image: None,
            acars_bank: None,
            acars_pre_lock: None,
            acars_init_failed: false,
            acars_stats_emitted_at: std::time::Instant::now(),
            acars_region: crate::acars_airband_lock::AcarsRegion::default(),
            acars_outputs: AcarsOutputs::new(dsp_tx)
                .map_err(|e| format!("ACARS output writer thread: {e}"))?,
            acars_last_user_jsonl_path: None,
            acars_last_user_network_addr: None,
        })
    }
}

/// Handle a single UI command.
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn handle_command(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, cmd: UiToDsp) {
    match cmd {
        UiToDsp::Start => {
            source::handle_start(state, dsp_tx);
        }

        UiToDsp::Stop => {
            source::handle_stop(state, dsp_tx);
        }

        UiToDsp::Tune(freq) => {
            source::handle_tune(state, dsp_tx, freq);
        }

        UiToDsp::SetDemodMode(mode) => {
            radio::handle_set_demod_mode(state, dsp_tx, mode);
        }

        UiToDsp::SetBandwidth(bw) => {
            radio::handle_set_bandwidth(state, dsp_tx, bw);
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
            source::handle_set_sample_rate(state, dsp_tx, rate);
        }

        UiToDsp::SetDecimation(ratio) => {
            source::handle_set_decimation(state, dsp_tx, ratio);
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
            source::handle_set_fft_size(state, dsp_tx, size);
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
            source::handle_set_gain(state, dsp_tx, gain_db);
        }

        UiToDsp::SetAgc(enabled) => {
            tracing::debug!(enabled, "set AGC");
            // Persist FIRST so a dispatch with no live source
            // survives until `open_source` runs. Per #551.
            state.tuner_agc_auto = enabled;
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
            state.radio.set_software_agc_enabled(enabled);
        }

        UiToDsp::SetIqCorrection(enabled) => {
            // Adaptive I/Q-imbalance (image) correction — its own
            // pipeline stage, NOT the DC blocker. The two used to share
            // `state.dc_blocking`, so the startup replay of this switch
            // (default off) silently disabled DC blocking (default on)
            // on every launch. Per #692.
            tracing::debug!(enabled, "set IQ correction");
            state.iq_correction = enabled;
            state.frontend.set_iq_correction(enabled);
        }

        UiToDsp::SetWindowFunction(window) => {
            source::handle_set_window_function(state, dsp_tx, window);
        }

        UiToDsp::SetVfoOffset(offset) => {
            source::handle_set_vfo_offset(state, dsp_tx, offset);
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

        UiToDsp::SetFftEnabled(enabled) => {
            tracing::debug!(enabled, "set FFT enabled");
            state.fft_enabled = enabled;
            state.frontend.set_fft_enabled(enabled);
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
            audio::handle_set_audio_sink_type(state, dsp_tx, new_type);
        }

        UiToDsp::SetNetworkSinkConfig {
            hostname,
            port,
            protocol,
        } => {
            audio::handle_set_network_sink_config(state, dsp_tx, &hostname, port, protocol);
        }

        UiToDsp::SetSourceType(source_type) => {
            source::handle_set_source_type(state, dsp_tx, source_type);
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
            source::handle_set_rtl_tcp_client_config(state, requested_role, auth_key);
        }

        UiToDsp::SetFilePath(path) => {
            tracing::debug!(?path, "set file path");
            state.file_path = path;
        }

        UiToDsp::SetFileLooping(looping) => {
            source::handle_set_file_looping(state, dsp_tx, looping);
        }

        UiToDsp::SetConverterOffset(offset_hz) => {
            source::handle_set_converter_offset(state, dsp_tx, offset_hz);
        }
        UiToDsp::SetAirspyDeviceSerial(serial) => {
            source::handle_set_airspy_device_serial(state, serial);
        }
        UiToDsp::RefreshAirspyDevices => {
            source::handle_refresh_airspy_devices(dsp_tx);
        }
        UiToDsp::SetBiasTee(enabled) => {
            source::handle_set_bias_tee(state, dsp_tx, enabled);
        }

        UiToDsp::SetDirectSampling(mode) => {
            source::handle_set_direct_sampling(state, dsp_tx, mode);
        }

        UiToDsp::SetOffsetTuning(enabled) => {
            tracing::debug!(enabled, "set offset tuning");
            // Persist FIRST so a dispatch with no live source
            // survives until `open_source` runs. Per #551.
            state.offset_tuning_enabled = enabled;
            if let Some(source) = &mut state.source
                && let Err(e) = source.set_offset_tuning(enabled)
            {
                tracing::warn!("set offset tuning failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Offset tuning failed: {e}")));
            }
        }

        UiToDsp::SetRtlAgc(enabled) => {
            tracing::debug!(enabled, "set RTL AGC");
            // Persist FIRST so a dispatch with no live source
            // survives until `open_source` runs. Per #551.
            state.rtl_agc_enabled = enabled;
            if let Some(source) = &mut state.source
                && let Err(e) = source.set_rtl_agc(enabled)
            {
                tracing::warn!("set RTL AGC failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("RTL AGC failed: {e}")));
            }
        }

        UiToDsp::SetGainByIndex(index) => {
            source::handle_set_gain_by_index(state, dsp_tx, index);
        }

        UiToDsp::SetPpmCorrection(ppm) => {
            tracing::debug!(ppm, "set PPM correction");
            // Persist FIRST so a dispatch with no live source
            // survives until `open_source` runs. Per #551.
            state.ppm_correction = ppm;
            if let Some(source) = &mut state.source
                && let Err(e) = source.set_ppm_correction(ppm)
            {
                tracing::warn!("set PPM correction failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("PPM correction failed: {e}")));
            }
        }

        UiToDsp::StartAudioRecording(path) => {
            audio::handle_start_audio_recording(state, dsp_tx, path);
        }

        UiToDsp::StopAudioRecording => {
            tracing::info!("stop audio recording");
            // Drop the writer — `Drop` finalizes the WAV header.
            state.audio_writer = None;
            let _ = dsp_tx.send(DspToUi::AudioRecordingStopped);
        }

        UiToDsp::SetLrptImage(image) => {
            lrpt::handle_set_lrpt_image(state, image);
        }

        UiToDsp::SetLrptDownlink(downlink) => {
            lrpt::handle_set_lrpt_downlink(state, downlink);
        }

        UiToDsp::ClearLrptImageContents(image) => {
            // Ordered after `SetLrptDownlink` on this queue, so any
            // held-back rows of the previous profile's decoder have
            // already been flushed into `image` (CR on PR #806).
            image.clear();
        }

        UiToDsp::ClearLrptImage => {
            lrpt::handle_clear_lrpt_image(state);
        }

        UiToDsp::SetSstvImage(handle) => {
            tracing::info!("SSTV image handle attached — decoder tap will push lines");
            state.sstv_image = Some(handle);
            // Decoder state intentionally NOT dropped here —
            // same contract as `SetLrptImage`. The handle is
            // a long-lived singleton; re-attaching is a no-op
            // for the decoder. Lifecycle stays owned by the
            // source-stop cleanup path. Per epic #472.
        }

        UiToDsp::ClearSstvImage => {
            tracing::info!("SSTV image handle cleared — line writes silently discarded");
            state.sstv_image = None;
            // Decoder stays alive — mirrors `ClearLrptImage`.
            // Per epic #472.
        }

        UiToDsp::ResetImagingDecoders => {
            // Between-pass reset for the auto-record flow when
            // the source stays open across pass boundaries
            // (`was_running == true` pre-AOS keeps `set_playing`
            // engaged at LOS, so the source-stop path doesn't
            // run and `cleanup`'s reset never fires). Without
            // this, the LRPT pipeline's internal
            // `ImageAssembler` retains every previous pass's
            // pixels (~6 MB per channel per pass) and the APT
            // decoder's accumulator + ready queue grow
            // monotonically. Same field reset as `cleanup`,
            // factored out into `reset_imaging_decoders` so
            // both call sites stay in lockstep. Per issue #544.
            tracing::info!("auto-record: imaging decoders reset between passes");
            reset_imaging_decoders(state);
        }

        UiToDsp::StartIqRecording(path) => {
            audio::handle_start_iq_recording(state, dsp_tx, path);
        }

        UiToDsp::StopIqRecording => {
            tracing::info!("stop IQ recording");
            state.iq_writer = None;
            let _ = dsp_tx.send(DspToUi::IqRecordingStopped);
        }

        UiToDsp::EnableTranscription(tx) => {
            transcription::handle_enable_transcription(state, dsp_tx, tx);
        }
        UiToDsp::DisableTranscription => {
            transcription::handle_disable_transcription(state);
        }

        UiToDsp::EnableAudioTap(tx) => {
            audio::handle_enable_audio_tap(state, tx);
        }
        UiToDsp::DisableAudioTap => {
            audio::handle_disable_audio_tap(state);
        }
        UiToDsp::DisconnectRtlTcp => {
            source::handle_disconnect_rtl_tcp(state, dsp_tx);
        }
        UiToDsp::RetryRtlTcpNow => {
            source::handle_retry_rtl_tcp_now(state, dsp_tx);
        }
        UiToDsp::RetryRtlTcpWithTakeover => {
            source::handle_retry_rtl_tcp_with_takeover(state, dsp_tx);
        }
        // --- Scanner (#317) ---
        UiToDsp::SetScannerEnabled(enabled) => {
            scanner::handle_set_scanner_enabled(state, dsp_tx, enabled);
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
        UiToDsp::SetAcarsEnabled(enable) => {
            // Honor TeardownNeeded — handler signaled an
            // unrecoverable double-failure, so tear down the
            // source per the AcarsHandlerOutcome contract.
            // CR round 18 on PR #584.
            if matches!(
                handle_set_acars_enabled(state, enable, dsp_tx),
                AcarsHandlerOutcome::TeardownNeeded
            ) && state.running
            {
                tracing::error!("ACARS SetAcarsEnabled double-failure; tearing down source");
                cleanup(state, dsp_tx);
                state.running = false;
                let _ = dsp_tx.send(DspToUi::SourceStopped);
            }
        }
        UiToDsp::SetAcarsRegion(region) => {
            acars::handle_set_acars_region(state, region);
        }
        // --- ACARS output commands (#578) ---
        UiToDsp::SetAcarsJsonlEnabled(enabled) => {
            handle_set_acars_jsonl_enabled(state, dsp_tx, enabled);
        }
        UiToDsp::SetAcarsJsonlPath(path) => {
            handle_set_acars_jsonl_path(state, dsp_tx, &path);
        }
        UiToDsp::SetAcarsNetworkEnabled(enabled) => {
            handle_set_acars_network_enabled(state, dsp_tx, enabled);
        }
        UiToDsp::SetAcarsNetworkAddr(addr) => {
            handle_set_acars_network_addr(state, dsp_tx, &addr);
        }
        UiToDsp::SetAcarsStationId(id) => {
            handle_set_acars_station_id(state, &id);
        }
    }
}

/// Stop the source and release resources.
fn cleanup(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    // ACARS teardown (epic #474). MUST run BEFORE `source.stop()`
    // so the synthetic disengage can physically retune the live
    // source back to the user's pre-lock rate/center via
    // `apply_acars_geometry`. Without this, `Stop → Start` would
    // re-open at the airband-locked `configured_sample_rate`
    // (2.5 MSps) but with `acars_bank == None`, which makes
    // `process_iq_block`'s tap a permanent no-op until the user
    // toggles ACARS off and on again — the half-enabled state
    // CodeRabbit flagged on PR #584.
    //
    // Force-clear the ACARS session state after the call so a
    // disengage Err (rare) doesn't leave a stale snapshot
    // lingering across the source teardown. Use
    // `acars_pre_lock.is_some()` (the canonical "ACARS engaged"
    // signal); `acars_bank.is_some()` is too narrow because the
    // Start path intentionally invalidates the bank for the
    // lazy-rebuild window. CR round 5 on PR #584.
    // Stop recordings FIRST (Drop patches the WAV header sizes) and tell
    // the UI via `AudioRecordingStopped` / `IqRecordingStopped` so its
    // recording-active flags clear — `SourceStopped` alone does not.
    // The ACARS disengage below refuses to change geometry while an IQ
    // writer is open (#695), and cleanup is the one path that must
    // always get through — the recording is ending anyway.
    if stop_any_recording(state, dsp_tx) {
        tracing::info!("recording finalized on cleanup");
    }

    let mut acars_forced_off = false;
    if state.acars_pre_lock.is_some() {
        // Cleanup is already tearing the source down, so
        // intentionally IGNORE any TeardownNeeded outcome from
        // the synthetic disengage — acting on it would recurse
        // back into cleanup. The outcome is unused here on
        // purpose; `let _ = ...` documents the intent. CR round 18.
        let _ = handle_set_acars_enabled(state, false, dsp_tx);
        // If pre_lock is STILL Some after the call, the disengage
        // path Err'd (re-stashed the snapshot for retry).
        // handle_dsp_message in sdr-ui preserves AppState's
        // `acars_enabled` on Err — by design, since Err doesn't
        // disambiguate engage-vs-disengage failure (CR round 1).
        // But we ARE force-clearing the DSP-side session right
        // below, so we need a definitive Ok(false) ack to keep
        // the UI toggle in sync; otherwise the user is left with
        // a latched-on toggle they have to manually flip off.
        // CR round 7 on PR #584.
        //
        // ALSO: copy the snapshot's tuning back into the
        // controller's in-memory fields BEFORE force-clearing
        // it. The disengage Err path leaves the controller at
        // airband geometry (since apply_acars_geometry's restore
        // failed and the best-effort re-apply re-engages
        // airband). Without this restore, the next Start would
        // reopen at `configured_sample_rate = 2.5 MSps` even
        // though ACARS is now off. CR round 8 on PR #584.
        if let Some(snapshot) = state.acars_pre_lock.as_ref() {
            acars_forced_off = true;
            tracing::warn!(
                source_rate = snapshot.source_rate_hz,
                center = snapshot.center_freq_hz,
                vfo_offset = snapshot.vfo_offset_hz,
                "ACARS disengage Err'd during cleanup; restoring snapshot to \
                 controller state + forcing UI off via Ok(false) ack"
            );
            state.configured_sample_rate = snapshot.source_rate_hz;
            state.center_freq = snapshot.center_freq_hz;
            state.vfo_offset = snapshot.vfo_offset_hz;
            // sample_rate stays at whatever apply_acars_geometry
            // last wrote (likely airband 2.5 MSps from the
            // best-effort re-engage). It'll be overwritten on
            // the next Start when open_source picks up
            // configured_sample_rate. No live source means we
            // can't read back hardware rate here either way.
        }
    }
    state.acars_bank = None;
    state.acars_init_failed = false;
    state.acars_pre_lock = None;
    if acars_forced_off {
        let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Ok(false)));
    }

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

    state.source = None;

    // Hard stream discontinuity — flush imaging-decoder state so
    // a subsequent Start can't bleed pre-stop accumulator / ready
    // lines / Viterbi traceback / image-assembler pixels into
    // the new session. Decoders themselves stay allocated so the
    // next Start doesn't pay re-init cost (filter taps,
    // resampler tables, FEC tables); only their in-flight
    // buffers get cleared. Cross-mode preservation (NFM → WFM →
    // NFM mid-pass) is a *soft* discontinuity and intentionally
    // stays untouched — the user keeps decoding the same pass.
    reset_imaging_decoders(state);

    // (ACARS teardown happens at the TOP of this function via the
    // synthetic-disengage path, BEFORE source.stop, so the live
    // retune can run while the source is still active.)

    tracing::info!("source closed");
}

/// Flush APT and LRPT decoder state without dropping the
/// decoders themselves. Used by both [`cleanup`] (full source
/// stop) and the [`UiToDsp::ResetImagingDecoders`] handler
/// (between-pass reset for auto-record sessions where the
/// source stays open). Per issue #544.
///
/// What gets reset:
/// - `state.apt_decoder` — accumulator + ready queue + sync
///   tracker via [`sdr_dsp::AptDecoder::reset`]. The filter
///   taps and resampler tables stay allocated.
/// - `state.apt_mono_buf` — scratch buffer for the L+R downmix
///   (cleared, kept allocated).
/// - `state.apt_init_failed_at_rate` — clear the cached
///   per-rate init-failure memo so the next session retries
///   even if a previous attempt failed.
/// - `state.lrpt_decoder` — Viterbi traceback + ASM sync
///   window + RS path + image assembler via
///   [`sdr_radio::LrptDecoder::reset`]. If `reset` fails (the
///   demod rebuild it does internally is practically
///   unreachable), drop the decoder so the next tap call lazily
///   re-initialises.
/// - `state.lrpt_init_failed` — same rationale as the APT
///   memo.
fn reset_imaging_decoders(state: &mut DspState) {
    if let Some(decoder) = state.apt_decoder.as_mut() {
        decoder.reset();
    }
    state.apt_mono_buf.clear();
    state.apt_init_failed_at_rate = None;

    if let Some(decoder) = state.lrpt_decoder.as_mut()
        && let Err(e) = decoder.reset()
    {
        tracing::warn!("LRPT decoder reset failed; dropping for re-init: {e}");
        state.lrpt_decoder = None;
    }
    // Clear the shared LRPT canvas directly rather than relying on the
    // decoder's reset to do it: `SetLrptDownlink` and the reset-Err
    // branch above leave `lrpt_decoder = None`, and a between-pass
    // reset then left pass 1's pixels for pass 2 to composite over —
    // the LOS PNG held both passes (#700). The handle itself survives
    // (the viewer holds a clone); only its pixels are wiped.
    if let Some(image) = state.lrpt_image.as_ref() {
        image.clear();
    }
    state.lrpt_init_failed = false;

    // SSTV decoder reset: drop and re-init on next tap call so
    // the next pass starts from a clean VIS-detection state.
    // Mirrors the LRPT `drop + None` approach — slowrx doesn't
    // expose a `reset()` method so we reconstruct lazily. The
    // `sstv_mono_buf` is left allocated (reused), and the
    // `sstv_init_failed_at_rate` memo is cleared so the next
    // source-start gets a fresh init attempt. Per epic #472.
    //
    // Clear the shared image handle so stale pixels from a
    // mid-image LOS don't bleed into the next pass's live
    // viewer. The handle survives here (the DSP tap re-uses it
    // across passes if the user keeps the source running);
    // only the in-flight buffer is wiped.
    if let Some(handle) = state.sstv_image.as_ref() {
        handle.clear();
    }
    state.sstv_decoder = None;
    state.sstv_mono_buf.clear();
    state.sstv_init_failed_at_rate = None;
    // Per-pass SSTV diagnostic summary. Skip the log when the
    // pass produced no events — a no-op reset between two non-
    // SSTV passes (e.g. back-to-back LRPT recordings on a
    // shared SSTV-decoder lifecycle) shouldn't clutter the
    // trace with a "0 VIS / 0 images" line. Per #648.
    if state.sstv_pass_stats.saw_any_event() {
        // `stats` triggered `clippy::similar_names` against the
        // surrounding `state` binding — rename to `pass_stats` for
        // visual distinctness while keeping the access concise.
        // Per CI clippy round on PR #658.
        let pass_stats = &state.sstv_pass_stats;
        tracing::info!(
            vis_count = pass_stats.vis_count,
            image_complete_count = pass_stats.image_complete_count,
            lines_decoded = pass_stats.lines_decoded,
            "SSTV pass summary"
        );
    }
    state.sstv_pass_stats = SstvPassStats::default();
}

/// Read one block of IQ data from the source, process it, and send FFT data
/// to the UI.
#[allow(clippy::too_many_lines)]
fn process_iq_block(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    fft_shared: &SharedFftBuffer,
) {
    if state.source.is_none() {
        // Reachable after `rebuild_rtl_tcp_source` fails `start()`.
        // Tear the rest of the session down through `cleanup()` so the
        // sink / recorders / ACARS lock don't outlive the source (#693).
        tracing::warn!("process_iq_block called without source");
        cleanup(state, dsp_tx);
        state.running = false;
        let _ = dsp_tx.send(DspToUi::SourceStopped);
        return;
    }
    let Some(source) = &mut state.source else {
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
        let _ = dsp_tx.send(DspToUi::Error(recording_write_error_message("IQ", &e)));
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
                // ACARS decode tap (#474). Runs at source rate
                // (ACARS forces frontend decim=1). Tapped BEFORE
                // the VFO so we read the full 2.5 MHz airband
                // window unchanged. Mirror of `lrpt_decode_tap`
                // but at source rate vs post-VFO 144 ksps.
                //
                // Outer guard is `acars_pre_lock.is_some()` (the
                // "ACARS engaged" signal), NOT `acars_bank.is_some()`.
                // Otherwise the lazy-init in `acars_decode_tap`
                // never runs after the Start path invalidates the
                // bank for an enable-while-stopped/startup-replay
                // path (CR round 4 on PR #584).
                if state.acars_pre_lock.is_some() {
                    acars_decode_tap(
                        &mut state.acars_bank,
                        &mut state.acars_init_failed,
                        state.sample_rate,
                        state.center_freq,
                        state.acars_region.channels(),
                        &state.processed_buf[..processed_count],
                        dsp_tx,
                        &state.acars_outputs,
                    );

                    // ~1 Hz channel-stats emission throttle.
                    let now = std::time::Instant::now();
                    let elapsed = now.duration_since(state.acars_stats_emitted_at);
                    if elapsed
                        >= std::time::Duration::from_millis(
                            crate::acars_airband_lock::ACARS_STATS_EMIT_INTERVAL_MS,
                        )
                        && let Some(bank) = state.acars_bank.as_ref()
                    {
                        let ch_stats = bank.channels();
                        // Box the slice as `Box<[ChannelStats]>` so the
                        // message variant is variable-width — the
                        // active region's channel count can be 6 (US-6
                        // / Europe) or up to `MAX_CUSTOM_CHANNELS` for
                        // a Custom region. Per Task 9 of the bundled
                        // ACARS PR plan.
                        let _ = dsp_tx.send(crate::messages::DspToUi::AcarsChannelStats(
                            ch_stats.to_vec().into_boxed_slice(),
                        ));
                        state.acars_stats_emitted_at = now;
                    }
                }

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
                        &mut state.lrpt_init_failed,
                        state.lrpt_downlink,
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
                            // ISS SSTV decode tap (#472). Also NFM-gated —
                            // the SSTV 1200–2300 Hz subcarrier rides the
                            // same wide-FM audio path as NOAA APT. Both
                            // decoders run in parallel when in NFM mode;
                            // only one will see a valid VIS header for
                            // the active signal, so the other runs cheaply
                            // as a no-op pass through the correlator.
                            sstv_decode_tap(state, dsp_tx, audio_count);
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
                        // transcription tap is inactive. The scanner tracker
                        // is advanced IMMEDIATELY on emit so it stays
                        // independent of the transcription tap's retry-on-Full
                        // logic below — toggling transcription must not perturb
                        // scanner edge detection. Per CodeRabbit round 1 on PR
                        // #558.
                        // Raw gate state: `true` whenever nothing is muting,
                        // including "no squelch configured". The
                        // transcription edge tracker below consumes this
                        // as-is (an ungated open squelch must still emit
                        // `SquelchOpened` after `EnableTranscription`).
                        let now_open = state.radio.if_chain().squelch_open();
                        // Scanner view: only a *gating* squelch can signal a
                        // carrier (#755). Kept separate from `now_open` so
                        // the two consumers don't share one meaning. Per
                        // CodeRabbit round 1 on PR #783.
                        let scanner_open = scanner_carrier_present(
                            state.radio.if_chain().squelch_active(),
                            now_open,
                        );
                        if scanner_open != state.squelch_was_open {
                            let scanner_edge = if scanner_open {
                                sdr_scanner::SquelchState::Open
                            } else {
                                sdr_scanner::SquelchState::Closed
                            };
                            let scan_cmds = state
                                .scanner
                                .handle_event(sdr_scanner::ScannerEvent::SquelchEdge(scanner_edge));
                            state.squelch_was_open = scanner_open;
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
                            // state transitions — if we advance the transcription
                            // tracker without the downstream having received the
                            // edge, the Auto Break state machine misses the
                            // transition entirely and gets stuck in
                            // Idle/Recording until the 30s safety flush fires.
                            // Retry on the next block by leaving the tracker
                            // unchanged.
                            let mut advance_transcription_tracker = true;

                            if now_open != state.transcription_squelch_was_open
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
                                        advance_transcription_tracker = false;
                                        // Throttle the warning to one per
                                        // `TRANSCRIPTION_FULL_WARN_INTERVAL`
                                        // window — a backed-up worker can
                                        // hold this edge pending for many
                                        // blocks, and an unthrottled warn
                                        // floods the trace log. The
                                        // suppressed-count gets reported
                                        // alongside the next emitted
                                        // warning so the burst magnitude
                                        // stays observable. Note: the
                                        // counter is incremented ONLY on
                                        // the suppressed path — the event
                                        // that actually triggers the warn
                                        // is reported as the warn itself
                                        // and is not added to
                                        // `suppressed_in_window`. So
                                        // `suppressed_in_window=0` means
                                        // "the warn is firing for the
                                        // first event in this window;
                                        // nothing extra was hidden". Per
                                        // `CodeRabbit` round 1 on PR #559.
                                        if state.transcription_full_warn_at.elapsed()
                                            >= TRANSCRIPTION_FULL_WARN_INTERVAL
                                        {
                                            let suppressed = state.transcription_full_suppressed;
                                            tracing::warn!(
                                                ?now_open,
                                                suppressed_in_window = suppressed,
                                                "transcription channel full; retrying squelch edge next block"
                                            );
                                            state.transcription_full_suppressed = 0;
                                            state.transcription_full_warn_at =
                                                std::time::Instant::now();
                                        } else {
                                            state.transcription_full_suppressed = state
                                                .transcription_full_suppressed
                                                .saturating_add(1);
                                        }
                                    }
                                }
                            }
                            if advance_transcription_tracker {
                                state.transcription_squelch_was_open = now_open;
                            }

                            // Skip the sample send while a squelch edge is
                            // pending retry (i.e. we hit `TrySendError::Full`
                            // above and chose not to advance the tracker).
                            // Without this, if the worker drains one slot
                            // between the failed edge `try_send` and this
                            // sample `try_send`, audio for the new state
                            // would arrive BEFORE the missing
                            // `SquelchOpened` / `SquelchClosed` edge — Auto
                            // Break would keep buffering the new utterance
                            // into the previous segment until the edge
                            // finally lands on the next block. Per
                            // `CodeRabbit` round 2 on PR #558.
                            if !send_error && advance_transcription_tracker {
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
                        // No `else` branch — the scanner block above
                        // advances `state.squelch_was_open` itself, so
                        // edge tracking stays correct whether or not
                        // the transcription tap is wired. Per CodeRabbit
                        // round 1 on PR #558.

                        // Generic audio tap: downsample to 16 kHz mono
                        // and try_send. Pre-volume (like the transcription
                        // tap) so the consumer's recognizer sees the raw
                        // demod output regardless of how the user has
                        // set the volume slider. `try_send` with
                        // `TrySendError::Full` → drop the chunk rather
                        // than block — the DSP thread MUST NOT stall on
                        // a slow consumer. A recognizer can tolerate
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
                                .send(DspToUi::Error(recording_write_error_message("Audio", &e)));
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

                        // Send to the selected audio sink.
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

// ---------------------------------------------------------------------------
// ACARS output helpers + handlers (Issue #578)
// ---------------------------------------------------------------------------

/// Default UDP feeder address for the airframes.io public
/// feed. Used when the network output is enabled without an
/// explicit address.
const ACARS_NETWORK_DEFAULT_ADDR: &str = "feed.airframes.io:5550";

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests;
