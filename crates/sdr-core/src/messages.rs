//! Message types for communication between the DSP thread and the UI thread.

use sdr_dsp::voice_squelch::VoiceSquelchMode;
// Re-export so downstream crates that match on the `DspToUi::AptLine`
// variant can reach the payload type without taking a direct
// `sdr-dsp` dep.
pub use sdr_dsp::apt::AptLine;
use sdr_radio::{DeemphasisMode, af_chain::CtcssMode};
use sdr_types::{DemodMode, Protocol, RtlTcpConnectionState};

use crate::sink_slot::{AudioSinkType, NetworkSinkStatus};

/// Why the scanner↔recording mutex fired. Surfaced to the UI
/// via `DspToUi::ScannerMutexStopped` so the appropriate toast
/// can be shown.
///
/// Scanner ↔ transcription mutex was removed — the two are
/// designed to coexist as of PR #558 (issue #517 emits
/// per-channel markers in the transcript log when the scanner
/// hops). The two surviving variants cover the recording leg,
/// which still mutexes with both scanner activation and
/// transcription start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScannerMutexReason {
    /// Scanner activation stopped a running recording.
    RecordingStoppedForScanner,
    /// Recording start stopped an active scanner.
    ScannerStoppedForRecording,
}

/// Messages sent from the DSP pipeline thread to the UI main loop.
#[derive(Debug)]
pub enum DspToUi {
    /// New FFT magnitude data ready for display.
    FftData(Vec<f32>),
    /// Updated SNR measurement in dB.
    SignalLevel(f32),
    /// A non-fatal error occurred in the pipeline.
    Error(String),
    /// The source has stopped (device disconnected, EOF, etc.).
    SourceStopped,
    /// The effective sample rate changed (after decimation, device reconfiguration).
    SampleRateChanged(f64),
    /// Device information string (e.g., tuner name, USB descriptor).
    DeviceInfo(String),
    /// Available tuner gain values in dB (queried from device on open).
    GainList(Vec<f64>),
    /// Raw (pre-decimation) sample rate for spectrum display bandwidth.
    DisplayBandwidth(f64),
    /// Audio recording started (contains the file path for display).
    AudioRecordingStarted(std::path::PathBuf),
    /// Audio recording stopped.
    AudioRecordingStopped,
    /// IQ recording started (contains the file path for display).
    IqRecordingStarted(std::path::PathBuf),
    /// IQ recording stopped.
    IqRecordingStopped,
    /// Demodulator mode changed. Emitted when `UiToDsp::SetDemodMode`
    /// actually changes the active demod mode (edge detection — not
    /// emitted if the requested mode matches the current mode). The
    /// transcript panel subscribes to this to stop any active
    /// transcription session (band change = new session boundary) and
    /// to re-run Auto Break row visibility rules.
    DemodModeChanged(DemodMode),
    /// Channel bandwidth changed. Emitted from the controller's
    /// `SetBandwidth` handler after the new value has been applied
    /// to `state.vfo`, `state.radio`, and `state.bandwidth`. Lets
    /// the Radio sidebar panel's bandwidth spin row reflect drags
    /// initiated from the spectrum VFO handles — without this, the
    /// spin row would go stale relative to the DSP and confuse the
    /// user about the active filter width.
    ///
    /// Emitted on every successful `SetBandwidth` application, not
    /// edge-filtered — the spin row's `set_value` is idempotent
    /// when called with its current value, so the cost is
    /// negligible and emitting unconditionally keeps the controller
    /// free of per-field before/after comparisons.
    BandwidthChanged(f64),
    /// VFO offset (Hz from tuner center) changed by the DSP.
    /// Symmetric with [`Self::BandwidthChanged`] — lets UI paths
    /// that trigger a VFO offset change indirectly (e.g. a
    /// "reset VFO" button that dispatches `SetVfoOffset(0)`)
    /// receive an echo and update the spectrum overlay without
    /// having to optimistically guess the new value locally.
    /// Per issue #341.
    VfoOffsetChanged(f64),
    /// CTCSS sustained-gate state changed. Emitted only on edges
    /// (closed → open / open → closed), not per-window, so the UI
    /// status indicator can subscribe without flooding the channel.
    /// Always `false` when CTCSS is currently `Off`.
    CtcssSustainedChanged(bool),
    /// Voice-squelch gate state changed. Same edge-triggered
    /// contract as `CtcssSustainedChanged`: only emitted on
    /// closed→open / open→closed transitions. Always `true`
    /// when voice squelch is `Off` (the gate is permanently
    /// open in that mode, so the edge is just a one-shot at
    /// mode-entry that the controller handles by resetting the
    /// tracker).
    VoiceSquelchOpenChanged(bool),
    /// Connection-lifecycle state for the currently active
    /// `rtl_tcp` client source. Emitted only on **edge** — when
    /// the projected `RtlTcpConnectionState` differs from the
    /// previous snapshot — so the UI can subscribe without
    /// flooding the channel at the poll cadence. Controller also
    /// emits `Disconnected` when the active source type is not
    /// `RtlTcp`, so the UI status row can rely on receiving that
    /// value to reset on source-type changes without needing a
    /// separate "hide the row" signal.
    RtlTcpConnectionState(RtlTcpConnectionState),
    /// Lifecycle/health update for the network audio sink.
    /// Emitted on switch boundaries (`Active` / `Inactive`) and
    /// on startup or write failure (`Error`). Hosts use it to
    /// drive a status row in the audio settings panel — green
    /// when streaming, red with the message on failure. Per
    /// issue #247.
    NetworkSinkStatus(NetworkSinkStatus),
    // --- Scanner (#317) ---
    /// Scanner's active channel changed. UI uses this to sync
    /// the frequency selector, spectrum center, status bar,
    /// demod dropdown, and bandwidth row. `key = None` means
    /// scanner went idle (clear the display).
    ScannerActiveChannelChanged {
        key: Option<sdr_scanner::ChannelKey>,
        freq_hz: u64,
        demod_mode: sdr_types::DemodMode,
        bandwidth: f64,
        name: String,
        /// Per-channel CTCSS mode. `None` on the channel means
        /// "no channel-level override"; the scanner applies Off
        /// to the engine in that case, and the UI mirrors that
        /// by setting the CTCSS row to Off. `Some(mode)` maps
        /// directly.
        ctcss: Option<CtcssMode>,
        /// Per-channel voice-squelch mode. `None` means "don't
        /// override" — both engine and UI keep the current value.
        /// `Some(mode)` gets applied by the scanner retune and
        /// reflected on the voice-squelch widget.
        voice_squelch: Option<VoiceSquelchMode>,
    },
    /// Scanner phase transition — UI updates the state label.
    ScannerStateChanged(sdr_scanner::ScannerState),
    /// Rotation exhausted because all channels are absent or
    /// locked out. UI surfaces a toast before the sidebar
    /// display resets.
    ScannerEmptyRotation,
    /// Scanner stopped recording/transcription (or vice versa)
    /// via the mutex. UI shows a toast describing the
    /// transition.
    ScannerMutexStopped(ScannerMutexReason),

    // --- APT decoder (#482) ---
    /// One decoded NOAA APT image line. Emitted from the DSP
    /// thread when the live FM-demodulated audio path's `AptDecoder`
    /// produces a new line. The UI handler routes it to the open
    /// `AptImageView` (no-op if the viewer isn't open).
    ///
    /// Cadence: ~2 lines/sec during a NOAA APT pass (the spec's
    /// fixed line rate). Boxed because `AptLine` is ~2 KB while
    /// every other variant is tiny — boxing keeps the enum's
    /// stack size in line with the rest, which matters for the
    /// `mpsc::Receiver::try_recv()` hot path that copies the
    /// returned `DspToUi` value once per drain.
    AptLine(Box<AptLine>),
    // --- SSTV decoder (#472 — ISS SSTV) ---
    /// VIS header detected — the decoder has identified the SSTV
    /// mode and a new image is starting. Emitted from the DSP
    /// thread when `sstv_decode_tap` receives a
    /// `slowrx::SstvEvent::VisDetected`.
    ///
    /// `mode_label` is the static, human-readable name of the
    /// detected mode (e.g. `"PD120"`, `"PD180"`, `"PD240"`).
    /// Strings are `&'static str` because the slowrx mode name
    /// list is bounded and stable; there's no allocation per VIS
    /// event. The UI surfaces this in the SSTV viewer's window
    /// title subtitle so the user can see which mode the active
    /// image is being decoded as.
    SstvVisDetected {
        /// Human-readable mode name (e.g. `"PD120"`).
        mode_label: &'static str,
    },
    /// One decoded SSTV scan line. Emitted from the DSP thread when
    /// `sstv_decode_tap` receives a `slowrx::SstvEvent::LineDecoded`.
    /// The UI handler uses this as a redraw trigger for the live
    /// viewer — the actual pixels are written into the shared
    /// `SstvImageHandle` before this message is sent, so the viewer
    /// only needs the index to know a new row is ready.
    ///
    /// `line_index` is 0-based. Cadence depends on the SSTV mode:
    /// PD120 produces 248 line pairs (496 rows) over ~120 s;
    /// PD180 over ~180 s; PD240 over ~240 s (all 640 × 496).
    /// ISS ARISS events typically send ~12 images per active pass
    /// window.
    SstvLineDecoded(u32),
    /// One complete SSTV image. Emitted from the DSP thread when
    /// `sstv_decode_tap` receives a `slowrx::SstvEvent::ImageComplete`.
    /// The UI wiring layer accumulates these for LOS save —
    /// `interpret_action::SaveSstvPass` drains them into per-image
    /// PNG files named `img0.png`, `img1.png`, etc. Boxed so the
    /// pixel `Vec` doesn't inflate the enum's stack footprint on
    /// the drain path. Per epic #472.
    SstvImageComplete {
        /// Width in pixels (640 for the entire PD family —
        /// PD120 / PD180 / PD240).
        width: u32,
        /// Height in scan lines.
        height: u32,
        /// Row-major RGB triples, length = `width * height`.
        pixels: Vec<[u8; 3]>,
    },
    /// One decoded ACARS frame. Boxed because `AcarsMessage`
    /// holds an inline `String` body and `arrayvec` fields,
    /// so the enum's stack footprint stays small.
    AcarsMessage(Box<sdr_acars::AcarsMessage>),
    /// Per-channel ACARS stats. Emitted no more than once per
    /// `ACARS_STATS_EMIT_INTERVAL_MS` while ACARS is on.
    AcarsChannelStats(Box<[sdr_acars::ChannelStats]>),
    /// Ack for `UiToDsp::SetAcarsEnabled`. `Ok(true)` after a
    /// successful engage; `Ok(false)` after disengage; `Err`
    /// on any failure (bank init, source retune, etc).
    AcarsEnabledChanged(Result<bool, crate::acars_airband_lock::AcarsEnableError>),
    /// Surfaces an output writer's open / DNS / I/O error to
    /// the UI for toast display. Sent on `JsonlWriter::open`
    /// or `UdpFeeder::open` failure. Issue #578.
    AcarsOutputError {
        /// `"jsonl"` or `"udp"` — used to scope the toast.
        kind: &'static str,
        /// Human-readable error message (already includes
        /// the file path or host:port).
        message: String,
    },
}

/// Available source types for IQ input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    /// RTL-SDR USB dongle.
    RtlSdr,
    /// Raw TCP/UDP network IQ stream (generic, fixed-format).
    Network,
    /// WAV file playback.
    File,
    /// rtl_tcp-protocol network source — speaks the RTL0 handshake,
    /// supports discovery via mDNS, and tunes the remote dongle via
    /// the 5-byte command channel. Distinct from `Network` because
    /// the wire protocol and feature set diverge.
    RtlTcp,
    /// Airspy R2 / Mini USB receiver (`libairspy-rs`). 12-bit ADC,
    /// firmware-reported rate table, composite linearity gain. Per
    /// issue #848.
    Airspy,
}

/// Messages sent from the UI thread to the DSP pipeline thread.
#[derive(Debug)]
pub enum UiToDsp {
    /// Start the DSP pipeline.
    Start,
    /// Stop the DSP pipeline.
    Stop,
    /// Tune to a new center frequency (Hz).
    Tune(f64),
    /// Change the demodulation mode.
    SetDemodMode(DemodMode),
    /// Set the radio channel bandwidth (Hz).
    SetBandwidth(f64),
    /// Set the squelch threshold (dB).
    SetSquelch(f32),
    /// Enable or disable the squelch gate.
    SetSquelchEnabled(bool),
    /// Enable or disable auto-squelch (noise floor tracking).
    SetAutoSquelch(bool),
    /// Set the audio output volume (0.0..=1.0).
    SetVolume(f32),
    /// Set the FM deemphasis mode.
    SetDeemphasis(DeemphasisMode),
    /// Change the source sample rate (Hz).
    SetSampleRate(f64),
    /// Set the decimation ratio (power-of-2, 1 = none).
    SetDecimation(u32),
    /// Enable or disable DC blocking.
    SetDcBlocking(bool),
    /// Enable or disable IQ inversion (conjugation).
    SetIqInversion(bool),
    /// Change the FFT size for spectrum display.
    SetFftSize(usize),
    /// Enable or disable the noise blanker.
    SetNbEnabled(bool),
    /// Enable or disable FM IF noise reduction.
    SetFmIfNrEnabled(bool),
    /// Set the RTL-SDR tuner gain (dB). Converted to tenths internally.
    SetGain(f64),
    /// Enable or disable the RTL-SDR **hardware** tuner AGC
    /// (tuner's internal VGA switches to AGC mode). Mutually
    /// exclusive with the software AGC path via the UI selector
    /// shipping in #356 / #357 — not at the DSP layer, though,
    /// since in principle both could run simultaneously (the
    /// tuner-side AGC would normalize the RF level and the
    /// software AGC would further refine on the IQ side). The
    /// UI mutex is the policy layer.
    SetAgc(bool),
    /// Enable or disable the **software** IF AGC — a pure-DSP
    /// envelope follower on the IQ stream inside `IfChain`. Well-
    /// behaved alternative to the tuner's hardware AGC for the
    /// strong-signal distortion case documented in #332 / #354.
    SetSoftwareAgc(bool),
    /// Enable or disable IQ correction.
    SetIqCorrection(bool),
    /// Set the FFT window function.
    SetWindowFunction(sdr_pipeline::iq_frontend::FftWindow),
    /// Set the VFO frequency offset from center in Hz (for click-to-tune).
    SetVfoOffset(f64),
    /// Set the noise blanker level (threshold multiplier, >= 1.0).
    SetNbLevel(f32),
    /// Enable or disable WFM stereo decode.
    SetWfmStereo(bool),
    /// Set the FFT display frame rate (FPS).
    SetFftRate(f64),
    /// Master FFT compute gate. When `false`, the IQ frontend skips
    /// the entire FFT accumulation + compute loop — saving the
    /// per-sample copy into the FFT accumulator, the windowing
    /// pass, and the FFT itself. Audio / demod / decimation /
    /// recording paths continue to run normally; only the spectrum-
    /// display path is suspended.
    ///
    /// Used by the UI to pause the waterfall when the user toggles
    /// it off via the Display sidebar (#646) or the window is
    /// minimized (#647). Independent of `SetFftRate` — the rate is
    /// preserved across enable / disable cycles so re-enabling
    /// resumes at the previously-configured frame rate without a
    /// settings round-trip.
    SetFftEnabled(bool),
    /// Enable or disable the audio high-pass filter (voice modes).
    SetHighPass(bool),
    /// Enable or disable the audio notch filter.
    SetNotchEnabled(bool),
    /// Set the audio notch filter frequency in Hz.
    SetNotchFrequency(f32),
    /// Set the CTCSS sub-audible tone squelch mode.
    SetCtcssMode(CtcssMode),
    /// Set the CTCSS detection threshold (normalized magnitude, `(0, 1]`).
    SetCtcssThreshold(f32),
    /// Set the voice-activity squelch mode (Off / Syllabic / Snr).
    SetVoiceSquelchMode(VoiceSquelchMode),
    /// Set the voice-squelch threshold for the currently active
    /// mode. Unit depends on the mode: normalized envelope ratio
    /// for Syllabic, dB for Snr. No-op when mode is Off.
    SetVoiceSquelchThreshold(f32),
    /// Set the audio output device by `PipeWire` node name.
    SetAudioDevice(String),
    /// Switch the audio sink type (local audio device vs network
    /// stream). The controller stops the current sink, swaps to
    /// the new variant using the persisted device/network config,
    /// and restarts it if the engine is currently running. Per
    /// issue #247.
    SetAudioSinkType(AudioSinkType),
    /// Configure the network audio sink hostname, port, and
    /// protocol. The controller stores the config on `DspState`
    /// so a future switch to `AudioSinkType::Network` (or a
    /// rebuild of an already-active network sink) picks the new
    /// values up. If the network sink is currently active, the
    /// controller also rebuilds it inline so the new endpoint
    /// takes effect immediately. Per issue #247.
    SetNetworkSinkConfig {
        hostname: String,
        port: u16,
        protocol: Protocol,
    },
    /// Switch the source type (stops current source if running).
    SetSourceType(SourceType),
    /// Configure network source hostname, port, and protocol.
    SetNetworkConfig {
        hostname: String,
        port: u16,
        protocol: sdr_types::Protocol,
    },
    /// Configure `rtl_tcp` client role + auth key. Takes effect
    /// on the NEXT connect (already-open sessions keep their
    /// admitted role until they disconnect). `requested_role`
    /// drives the `ClientHello.role` byte; `auth_key` activates
    /// the eager-auth path (#394) when `Some`. Both fields are
    /// independent — a caller can change just the role
    /// (Control ↔ Listen) or just rotate the key. Per issue
    /// #396.
    SetRtlTcpClientConfig {
        /// Role to request in the next connect. `Role::Control`
        /// is the default / back-compat path; `Role::Listen`
        /// opts into the #392 concurrent-listener flow.
        requested_role: sdr_server_rtltcp::extension::Role,
        /// Pre-shared key (#394) to send eagerly with the hello.
        /// `None` disables the auth gate (no key on the wire);
        /// `Some(bytes)` sets `FLAG_HAS_AUTH` and emits an
        /// `AuthKeyMessage` follow-up.
        auth_key: Option<Vec<u8>>,
    },
    /// Set the file path for file source playback.
    SetFilePath(std::path::PathBuf),
    /// Toggle loop-on-EOF for the file playback source. `true`
    /// rewinds to the start of the file on EOF and keeps
    /// streaming; `false` stops the source at EOF. No-op when
    /// the active source isn't `.file`. Per issue #236.
    SetFileLooping(bool),
    /// Set PPM frequency correction for RTL-SDR crystal offset.
    SetPpmCorrection(i32),
    // ------------------------------------------------------
    //  rtl_tcp-specific commands (#325)
    //
    //  These dispatch to the active `Source` via the new hook
    //  methods on `sdr_pipeline::source_manager::Source`. Non-
    //  rtl_tcp sources no-op; the rtl_tcp client forwards each
    //  to the matching wire command. Generic tuning commands
    //  (tune / set_gain / set_ppm_correction / etc.) still flow
    //  through `Source::set_*` — these cover only the knobs the
    //  rtl_tcp wire protocol exposes that aren't on the generic
    //  source surface.
    // ------------------------------------------------------
    /// Enable or disable the tuner's bias tee (powers an LNA
    /// over coax).
    SetBiasTee(bool),
    /// Set direct-sampling mode (0 = off, 1 = I branch, 2 = Q
    /// branch). Engine rejects values outside that range.
    SetDirectSampling(i32),
    /// Enable or disable tuner offset-tuning mode.
    SetOffsetTuning(bool),
    /// Enable or disable RTL2832 digital AGC. Distinct from
    /// the tuner (analog) AGC that `SetAgc` controls.
    SetRtlAgc(bool),
    /// Set tuner gain by index into the tuner's discrete gain
    /// table. Index is bounds-checked against `Source::gains()`
    /// at dispatch time.
    SetGainByIndex(u32),
    /// Start recording demodulated audio to a WAV file.
    StartAudioRecording(std::path::PathBuf),
    /// Stop audio recording and finalize the WAV file.
    StopAudioRecording,
    /// Start recording raw IQ samples to a WAV file.
    StartIqRecording(std::path::PathBuf),
    /// Stop IQ recording and finalize the WAV file.
    StopIqRecording,
    /// Hand the DSP thread a clone of the shared
    /// `sdr_radio::lrpt_image::LrptImage` handle the live LRPT
    /// viewer reads from. Sent by the wiring layer at AOS for
    /// LRPT auto-record passes (or whenever the user opens the
    /// LRPT viewer manually). The DSP thread stores the handle
    /// and pushes decoded scan lines into it whenever the LRPT
    /// decoder tap runs (`current_mode` == `DemodMode::Lrpt`).
    /// Per epic #469 task 7.
    SetLrptImage(sdr_radio::lrpt_image::LrptImage),
    /// Drop the shared LRPT image handle. Sent at LOS / when
    /// the live viewer closes — stops the LRPT decoder from
    /// pushing further lines (next AOS will re-set with a fresh
    /// handle). Decoder state itself stays alive until the
    /// next source-stop, mirroring the APT decoder's
    /// "decoder kept across mode toggles" behavior.
    ClearLrptImage,
    /// Clear the contents of the given shared LRPT image on the
    /// DSP thread. Sent at AOS right after `SetLrptDownlink` so
    /// the two are processed in queue order: a profile change
    /// flushes the old decoder's held-back row group into the
    /// image first, then the canvas is wiped for the new pass.
    /// Clearing from the UI thread directly raced that flush and
    /// could leave the previous pass's tail on the fresh canvas
    /// (CR on PR #806). The handle is passed explicitly so the
    /// clear works whether or not the DSP currently holds it.
    ClearLrptImageContents(sdr_radio::lrpt_image::LrptImage),
    /// Tell the DSP thread which Meteor LRPT downlink profile
    /// (modulation + differential precoding) to use for the next
    /// decoder init. METEOR-M N2 is QPSK with differential
    /// precoding; the active METEOR-M2 3 / METEOR-M2 4 satellites
    /// transmit plain OQPSK. Sent by the wiring layer at AOS from
    /// the `KnownSatellite::lrpt_modulation` / `lrpt_differential`
    /// catalog fields. Drops any existing LRPT decoder so the next
    /// IQ chunk re-inits with the new profile; safe to send
    /// mid-pass. Per #662 / #730.
    SetLrptDownlink(sdr_radio::lrpt_decoder::LrptDownlink),
    /// Hand the DSP thread a clone of the shared
    /// `sdr_radio::sstv_image::SstvImageHandle` the live SSTV
    /// viewer reads from. Sent by the wiring layer at AOS for
    /// SSTV auto-record passes (or whenever the user opens the
    /// SSTV viewer manually). The DSP thread stores the handle
    /// and writes decoded scan lines into it whenever the SSTV
    /// decoder tap runs (`current_mode` == `DemodMode::Nfm`).
    /// Per epic #472.
    SetSstvImage(sdr_radio::sstv_image::SstvImageHandle),
    /// Drop the shared SSTV image handle. Sent at LOS / when
    /// the live viewer closes — the DSP decoder continues
    /// running (lines are silently discarded) until the next
    /// source-stop. Mirrors the LRPT `ClearLrptImage` pattern.
    /// Per epic #472.
    ClearSstvImage,
    /// Start sending audio to the transcription engine.
    EnableTranscription(std::sync::mpsc::SyncSender<sdr_transcription::TranscriptionInput>),
    /// Stop sending audio to the transcription engine.
    DisableTranscription,
    /// Enable a generic audio tap that receives 16 kHz mono f32
    /// samples downsampled from the post-demod 48 kHz stereo
    /// stream. Distinct from `EnableTranscription` which pushes
    /// 48 kHz interleaved stereo to the sdr-transcription backends;
    /// this path targets embedders that want a speech-recognizer-ready
    /// stream without pulling in the sdr-transcription dependency.
    EnableAudioTap(std::sync::mpsc::SyncSender<Vec<f32>>),
    /// Disable the audio tap enabled by `EnableAudioTap`. No-op when
    /// no tap is active.
    DisableAudioTap,
    /// Stop the `rtl_tcp` client connection without changing the
    /// selected source type. Sends `source.stop()` so the manager
    /// thread tears down and the connection state transitions to
    /// `Disconnected`. User can reconnect via the Play button or
    /// `RetryRtlTcpNow`.
    DisconnectRtlTcp,
    /// Force an immediate reconnect of the active `rtl_tcp` client
    /// by stopping and restarting the source. Useful when the
    /// server just came back online and the user doesn't want to
    /// wait for the current exponential-backoff delay to expire.
    /// No-op when the active source is not `RtlTcp`.
    RetryRtlTcpNow,
    /// One-shot "Take control" reconnect (#393 takeover handshake).
    /// Sets `FLAG_REQUEST_TAKEOVER` on the NEXT `ClientHello`
    /// and triggers an immediate reconnect; the bit auto-clears
    /// after that single attempt so subsequent reconnects (e.g.,
    /// transport-level retries) don't keep displacing whoever
    /// just got admitted. Surfaced by the UI when the user
    /// clicks "Take control" on the `ControllerBusy` toast. No-op
    /// when the active source is not `RtlTcp`. Per issue #396.
    RetryRtlTcpWithTakeover,
    // --- Scanner (#317) ---
    /// Master scanner on/off toggle.
    SetScannerEnabled(bool),
    /// Replace the scanner's channel list. UI projects bookmarks
    /// with `scan_enabled = true` into `ScannerChannel`s (folding
    /// defaults + overrides at projection time) and dispatches
    /// this on startup + any bookmark/default change.
    UpdateScannerChannels(Vec<sdr_scanner::ScannerChannel>),
    /// Session-scoped lockout — scanner skips this channel until
    /// unlocked or scanner is disabled.
    LockoutScannerChannel(sdr_scanner::ChannelKey),
    /// Clear a lockout. If scanner stalled into `Idle` via
    /// `EmptyRotation` (all channels locked) this resumes
    /// rotation automatically.
    UnlockScannerChannel(sdr_scanner::ChannelKey),
    /// Flush in-flight imaging-decoder state (APT + LRPT) without
    /// closing the source. The source-stop path (`cleanup`)
    /// already does this on full Stop, but the auto-record flow
    /// when `was_running == true` pre-AOS keeps the source open
    /// across pass boundaries — leaving the LRPT pipeline's
    /// `ImageAssembler` and the APT decoder's accumulator
    /// retaining state from pass N when pass N+1 begins. The
    /// auto-record state machine emits this at the
    /// `Recording → Finalizing` transition (LOS) so each pass
    /// starts with clean decoder state regardless of whether the
    /// source is power-cycled. Per issue #544.
    ResetImagingDecoders,
    /// Engage or release the ACARS airband lock. `true` snapshots
    /// the prior source config and forces (2.5 `MSps`, 130.3375 MHz,
    /// frontend decim=1); `false` restores the snapshot.
    SetAcarsEnabled(bool),
    /// Toggle the ACARS JSONL log writer on/off. Issue #578.
    SetAcarsJsonlEnabled(bool),
    /// Update the JSONL log path. Empty string ⇒ default
    /// path (`~/sdr-recordings/acars.jsonl`). Issue #578.
    SetAcarsJsonlPath(String),
    /// Toggle the ACARS UDP JSON feeder on/off. Issue #578.
    SetAcarsNetworkEnabled(bool),
    /// Update the feeder host:port. Issue #578.
    SetAcarsNetworkAddr(String),
    /// Switch the ACARS channel set / region (issue #581). The
    /// DSP records this on `DspState::acars_region`; the next
    /// `SetAcarsEnabled(true)` consults it to pick channels and
    /// the source center frequency. No-op while engaged (the
    /// airband lock rejects geometry mutations); the user
    /// disengages, switches region, then re-engages.
    SetAcarsRegion(crate::acars_airband_lock::AcarsRegion),
    /// Update the operator station ID embedded in the JSON's
    /// `station_id` field. Empty string ⇒ field omitted.
    /// Issue #578.
    SetAcarsStationId(String),
}

#[cfg(test)]
mod tests;
