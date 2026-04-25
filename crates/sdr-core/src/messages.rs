//! Message types for communication between the DSP thread and the UI thread.

use sdr_dsp::apt::AptLine;
use sdr_dsp::voice_squelch::VoiceSquelchMode;
use sdr_radio::{DeemphasisMode, af_chain::CtcssMode};
use sdr_types::{DemodMode, Protocol, RtlTcpConnectionState};

use crate::sink_slot::{AudioSinkType, NetworkSinkStatus};

/// Why the scanner↔recording/transcription mutex fired.
/// Surfaced to the UI via `DspToUi::ScannerMutexStopped` so the
/// appropriate toast can be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScannerMutexReason {
    /// Scanner activation stopped a running recording.
    RecordingStoppedForScanner,
    /// Scanner activation stopped a running transcription.
    TranscriptionStoppedForScanner,
    /// Recording start stopped an active scanner.
    ScannerStoppedForRecording,
    /// Transcription start stopped an active scanner.
    ScannerStoppedForTranscription,
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
    /// Start sending audio to the transcription engine.
    EnableTranscription(std::sync::mpsc::SyncSender<sdr_transcription::TranscriptionInput>),
    /// Stop sending audio to the transcription engine.
    DisableTranscription,
    /// Enable a generic audio tap that receives 16 kHz mono f32
    /// samples downsampled from the post-demod 48 kHz stereo
    /// stream. Distinct from `EnableTranscription` which pushes
    /// 48 kHz interleaved stereo to the sdr-transcription backends;
    /// this path targets FFI consumers that want a
    /// speech-recognizer-ready stream (e.g. the macOS `SpeechAnalyzer`
    /// path for issue #314) without pulling the sdr-transcription
    /// dependency across the FFI.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed bandwidth used by the message-variant round-trip
    /// tests. 12.5 kHz is NFM's default and the value the VFO-drag
    /// feedback loop most commonly emits in practice — hoisting it
    /// to a const both removes the magic-number duplication in the
    /// construct + match and documents the choice of value.
    const TEST_BANDWIDTH_HZ: f64 = 12_500.0;

    /// Fixed VFO offset used by the `VfoOffsetChanged` round-trip
    /// test. 25 kHz is a representative non-zero offset that
    /// click-to-tune / drag flows routinely emit — same hoisting
    /// rationale as `TEST_BANDWIDTH_HZ`: avoids a magic-number
    /// duplicated between construct and match.
    const TEST_VFO_OFFSET_HZ: f64 = 25_000.0;

    #[test]
    fn test_dsp_to_ui_variants() {
        let fft = DspToUi::FftData(vec![1.0, 2.0, 3.0]);
        assert!(matches!(fft, DspToUi::FftData(v) if v.len() == 3));

        let snr = DspToUi::SignalLevel(12.5);
        assert!(matches!(snr, DspToUi::SignalLevel(s) if (s - 12.5).abs() < f32::EPSILON));

        let err = DspToUi::Error("test error".to_string());
        assert!(matches!(err, DspToUi::Error(ref s) if s == "test error"));

        let stopped = DspToUi::SourceStopped;
        assert!(matches!(stopped, DspToUi::SourceStopped));

        let sr = DspToUi::SampleRateChanged(2_400_000.0);
        assert!(
            matches!(sr, DspToUi::SampleRateChanged(r) if (r - 2_400_000.0).abs() < f64::EPSILON)
        );

        let info = DspToUi::DeviceInfo("RTL2838UHIDIR".to_string());
        assert!(matches!(info, DspToUi::DeviceInfo(ref s) if s == "RTL2838UHIDIR"));

        let audio_rec = DspToUi::AudioRecordingStarted(std::path::PathBuf::from("/tmp/test.wav"));
        assert!(matches!(audio_rec, DspToUi::AudioRecordingStarted(_)));

        let audio_stop = DspToUi::AudioRecordingStopped;
        assert!(matches!(audio_stop, DspToUi::AudioRecordingStopped));

        let iq_rec = DspToUi::IqRecordingStarted(std::path::PathBuf::from("/tmp/iq.wav"));
        assert!(matches!(iq_rec, DspToUi::IqRecordingStarted(_)));

        let iq_stop = DspToUi::IqRecordingStopped;
        assert!(matches!(iq_stop, DspToUi::IqRecordingStopped));
    }

    #[test]
    fn demod_mode_changed_message_constructs() {
        let m = DspToUi::DemodModeChanged(DemodMode::Nfm);
        assert!(matches!(m, DspToUi::DemodModeChanged(DemodMode::Nfm)));
    }

    #[test]
    fn bandwidth_changed_message_constructs() {
        // Pins the variant shape + payload round-trip so future
        // refactors that accidentally change the f64 carrier
        // (e.g. to `u32` Hz or a `Bandwidth` newtype) trip this
        // test.
        let bw = DspToUi::BandwidthChanged(TEST_BANDWIDTH_HZ);
        assert!(
            matches!(bw, DspToUi::BandwidthChanged(v) if (v - TEST_BANDWIDTH_HZ).abs() < f64::EPSILON)
        );
    }

    #[test]
    fn vfo_offset_changed_message_constructs() {
        // Same shape regression as `bandwidth_changed_message_constructs`
        // — future refactors that change the f64 carrier type
        // fail here first.
        let offset = DspToUi::VfoOffsetChanged(TEST_VFO_OFFSET_HZ);
        assert!(matches!(
            offset,
            DspToUi::VfoOffsetChanged(v) if (v - TEST_VFO_OFFSET_HZ).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn ctcss_sustained_changed_message_constructs() {
        let open = DspToUi::CtcssSustainedChanged(true);
        assert!(matches!(open, DspToUi::CtcssSustainedChanged(true)));
        let closed = DspToUi::CtcssSustainedChanged(false);
        assert!(matches!(closed, DspToUi::CtcssSustainedChanged(false)));
    }

    #[test]
    fn voice_squelch_open_changed_message_constructs() {
        let open = DspToUi::VoiceSquelchOpenChanged(true);
        assert!(matches!(open, DspToUi::VoiceSquelchOpenChanged(true)));
        let closed = DspToUi::VoiceSquelchOpenChanged(false);
        assert!(matches!(closed, DspToUi::VoiceSquelchOpenChanged(false)));
    }

    #[test]
    fn rtl_tcp_connection_state_message_constructs() {
        // Constructing each variant through the DspToUi wrapper
        // exercises the `#[derive(Debug)]` + message plumbing end
        // to end. Catches the class of bugs where a future refactor
        // changes the `RtlTcpConnectionState` shape without updating
        // the message-side re-export — the build would still pass
        // but the variant wouldn't wrap.
        let disc = DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Disconnected);
        assert!(matches!(
            disc,
            DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Disconnected)
        ));

        let connecting = DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Connecting);
        assert!(matches!(
            connecting,
            DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Connecting)
        ));

        let connected = DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Connected {
            tuner_name: "R820T".into(),
            gain_count: 29,
            codec: "None".into(),
            granted_role: Some(true),
        });
        assert!(matches!(
            connected,
            DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Connected {
                gain_count: 29,
                ref codec,
                ..
            }) if codec == "None"
        ));

        let retrying = DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Retrying {
            attempt: 3,
            retry_in: std::time::Duration::from_secs(5),
        });
        assert!(matches!(
            retrying,
            DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Retrying { attempt: 3, .. })
        ));

        let failed = DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Failed {
            reason: "bad handshake".into(),
        });
        assert!(matches!(
            failed,
            DspToUi::RtlTcpConnectionState(RtlTcpConnectionState::Failed { .. })
        ));

        // Network audio sink status (issue #247) — three
        // variants exercising each shape so a future payload
        // tweak (e.g. adding a bytes_sent counter) trips this
        // regression net rather than silently going quiet at
        // the GTK status-row renderer. Per CodeRabbit round 1
        // on PR #351.
        let net_active = DspToUi::NetworkSinkStatus(NetworkSinkStatus::Active {
            endpoint: "0.0.0.0:1234".to_string(),
            protocol: sdr_types::Protocol::TcpClient,
        });
        assert!(matches!(
            &net_active,
            DspToUi::NetworkSinkStatus(NetworkSinkStatus::Active {
                endpoint,
                protocol: sdr_types::Protocol::TcpClient,
            }) if endpoint == "0.0.0.0:1234"
        ));

        let net_inactive = DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive);
        assert!(matches!(
            net_inactive,
            DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive)
        ));

        let net_err = DspToUi::NetworkSinkStatus(NetworkSinkStatus::Error {
            message: "bind: Address already in use".to_string(),
        });
        assert!(matches!(
            &net_err,
            DspToUi::NetworkSinkStatus(NetworkSinkStatus::Error { message })
                if message == "bind: Address already in use"
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_ui_to_dsp_variants() {
        let start = UiToDsp::Start;
        assert!(matches!(start, UiToDsp::Start));

        let stop = UiToDsp::Stop;
        assert!(matches!(stop, UiToDsp::Stop));

        let tune = UiToDsp::Tune(144_000_000.0);
        assert!(matches!(tune, UiToDsp::Tune(f) if (f - 144_000_000.0).abs() < f64::EPSILON));

        let mode = UiToDsp::SetDemodMode(DemodMode::Am);
        assert!(matches!(mode, UiToDsp::SetDemodMode(DemodMode::Am)));

        let bw = UiToDsp::SetBandwidth(12_500.0);
        assert!(matches!(bw, UiToDsp::SetBandwidth(b) if (b - 12_500.0).abs() < f64::EPSILON));

        let sq = UiToDsp::SetSquelch(-50.0);
        assert!(matches!(sq, UiToDsp::SetSquelch(s) if (s - (-50.0)).abs() < f32::EPSILON));

        let sqe = UiToDsp::SetSquelchEnabled(true);
        assert!(matches!(sqe, UiToDsp::SetSquelchEnabled(true)));

        let auto_sq = UiToDsp::SetAutoSquelch(true);
        assert!(matches!(auto_sq, UiToDsp::SetAutoSquelch(true)));

        let vol = UiToDsp::SetVolume(0.75);
        assert!(matches!(vol, UiToDsp::SetVolume(v) if (v - 0.75).abs() < f32::EPSILON));

        let deemp = UiToDsp::SetDeemphasis(DeemphasisMode::Eu50);
        assert!(matches!(
            deemp,
            UiToDsp::SetDeemphasis(DeemphasisMode::Eu50)
        ));

        let sr = UiToDsp::SetSampleRate(2_400_000.0);
        assert!(matches!(sr, UiToDsp::SetSampleRate(r) if (r - 2_400_000.0).abs() < f64::EPSILON));

        let dec = UiToDsp::SetDecimation(4);
        assert!(matches!(dec, UiToDsp::SetDecimation(4)));

        let dc = UiToDsp::SetDcBlocking(true);
        assert!(matches!(dc, UiToDsp::SetDcBlocking(true)));

        let iq = UiToDsp::SetIqInversion(false);
        assert!(matches!(iq, UiToDsp::SetIqInversion(false)));

        let fft = UiToDsp::SetFftSize(2048);
        assert!(matches!(fft, UiToDsp::SetFftSize(2048)));

        let nb = UiToDsp::SetNbEnabled(true);
        assert!(matches!(nb, UiToDsp::SetNbEnabled(true)));

        let nr = UiToDsp::SetFmIfNrEnabled(false);
        assert!(matches!(nr, UiToDsp::SetFmIfNrEnabled(false)));

        let gain = UiToDsp::SetGain(33.8);
        assert!(matches!(gain, UiToDsp::SetGain(g) if (g - 33.8).abs() < f64::EPSILON));

        let agc = UiToDsp::SetAgc(true);
        assert!(matches!(agc, UiToDsp::SetAgc(true)));

        // Software AGC runs alongside hardware AGC — the UI
        // selector (#356 / #357) mutually excludes them, but
        // the engine-side messages are independent.
        let sw_agc_on = UiToDsp::SetSoftwareAgc(true);
        assert!(matches!(sw_agc_on, UiToDsp::SetSoftwareAgc(true)));
        let sw_agc_off = UiToDsp::SetSoftwareAgc(false);
        assert!(matches!(sw_agc_off, UiToDsp::SetSoftwareAgc(false)));

        let iq_corr = UiToDsp::SetIqCorrection(false);
        assert!(matches!(iq_corr, UiToDsp::SetIqCorrection(false)));

        let wf = UiToDsp::SetWindowFunction(sdr_pipeline::iq_frontend::FftWindow::Blackman);
        assert!(matches!(
            wf,
            UiToDsp::SetWindowFunction(sdr_pipeline::iq_frontend::FftWindow::Blackman)
        ));

        let vfo = UiToDsp::SetVfoOffset(25_000.0);
        assert!(matches!(vfo, UiToDsp::SetVfoOffset(o) if (o - 25_000.0).abs() < f64::EPSILON));

        let nb = UiToDsp::SetNbLevel(5.0);
        assert!(matches!(nb, UiToDsp::SetNbLevel(l) if (l - 5.0).abs() < f32::EPSILON));

        let stereo = UiToDsp::SetWfmStereo(true);
        assert!(matches!(stereo, UiToDsp::SetWfmStereo(true)));

        let fft_rate = UiToDsp::SetFftRate(30.0);
        assert!(matches!(fft_rate, UiToDsp::SetFftRate(r) if (r - 30.0).abs() < f64::EPSILON));

        let hp = UiToDsp::SetHighPass(true);
        assert!(matches!(hp, UiToDsp::SetHighPass(true)));

        let notch_en = UiToDsp::SetNotchEnabled(true);
        assert!(matches!(notch_en, UiToDsp::SetNotchEnabled(true)));

        let notch_freq = UiToDsp::SetNotchFrequency(60.0);
        assert!(
            matches!(notch_freq, UiToDsp::SetNotchFrequency(f) if (f - 60.0).abs() < f32::EPSILON)
        );

        let ctcss_off = UiToDsp::SetCtcssMode(CtcssMode::Off);
        assert!(matches!(ctcss_off, UiToDsp::SetCtcssMode(CtcssMode::Off)));

        let ctcss_tone = UiToDsp::SetCtcssMode(CtcssMode::Tone(100.0));
        assert!(matches!(
            ctcss_tone,
            UiToDsp::SetCtcssMode(CtcssMode::Tone(hz)) if (hz - 100.0).abs() < f32::EPSILON
        ));

        let ctcss_thresh = UiToDsp::SetCtcssThreshold(0.15);
        assert!(
            matches!(ctcss_thresh, UiToDsp::SetCtcssThreshold(t) if (t - 0.15).abs() < f32::EPSILON)
        );

        let vs_off = UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Off);
        assert!(matches!(
            vs_off,
            UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Off)
        ));

        let vs_syl = UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Syllabic { threshold: 0.15 });
        assert!(matches!(
            vs_syl,
            UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Syllabic { threshold })
                if (threshold - 0.15).abs() < f32::EPSILON
        ));

        let vs_snr = UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Snr { threshold_db: 6.0 });
        assert!(matches!(
            vs_snr,
            UiToDsp::SetVoiceSquelchMode(VoiceSquelchMode::Snr { threshold_db })
                if (threshold_db - 6.0).abs() < f32::EPSILON
        ));

        let vs_thresh = UiToDsp::SetVoiceSquelchThreshold(0.2);
        assert!(
            matches!(vs_thresh, UiToDsp::SetVoiceSquelchThreshold(t) if (t - 0.2).abs() < f32::EPSILON)
        );

        let device = UiToDsp::SetAudioDevice("default".to_string());
        assert!(matches!(device, UiToDsp::SetAudioDevice(ref s) if s == "default"));

        let src_type = UiToDsp::SetSourceType(SourceType::RtlSdr);
        assert!(matches!(
            src_type,
            UiToDsp::SetSourceType(SourceType::RtlSdr)
        ));

        let src_net = UiToDsp::SetSourceType(SourceType::Network);
        assert!(matches!(
            src_net,
            UiToDsp::SetSourceType(SourceType::Network)
        ));

        let src_file = UiToDsp::SetSourceType(SourceType::File);
        assert!(matches!(src_file, UiToDsp::SetSourceType(SourceType::File)));

        let net_cfg = UiToDsp::SetNetworkConfig {
            hostname: "192.168.1.1".to_string(),
            port: 4321,
            protocol: sdr_types::Protocol::TcpClient,
        };
        assert!(matches!(
            net_cfg,
            UiToDsp::SetNetworkConfig { ref hostname, port: 4321, .. } if hostname == "192.168.1.1"
        ));

        let file_path = UiToDsp::SetFilePath(std::path::PathBuf::from("/tmp/test.wav"));
        assert!(matches!(
            file_path,
            UiToDsp::SetFilePath(ref p) if p == std::path::Path::new("/tmp/test.wav")
        ));

        // Loop-on-EOF toggle — both polarities, since the
        // controller's handler branches on the value and we
        // want a shape regression on either to fail loudly.
        // Per `CodeRabbit` round 1 on PR #371.
        let loop_on = UiToDsp::SetFileLooping(true);
        assert!(matches!(loop_on, UiToDsp::SetFileLooping(true)));
        let loop_off = UiToDsp::SetFileLooping(false);
        assert!(matches!(loop_off, UiToDsp::SetFileLooping(false)));

        let ppm = UiToDsp::SetPpmCorrection(42);
        assert!(matches!(ppm, UiToDsp::SetPpmCorrection(42)));

        let audio_rec = UiToDsp::StartAudioRecording(std::path::PathBuf::from("/tmp/audio.wav"));
        assert!(matches!(audio_rec, UiToDsp::StartAudioRecording(_)));

        let audio_stop = UiToDsp::StopAudioRecording;
        assert!(matches!(audio_stop, UiToDsp::StopAudioRecording));

        let iq_rec = UiToDsp::StartIqRecording(std::path::PathBuf::from("/tmp/iq.wav"));
        assert!(matches!(iq_rec, UiToDsp::StartIqRecording(_)));

        let iq_stop = UiToDsp::StopIqRecording;
        assert!(matches!(iq_stop, UiToDsp::StopIqRecording));

        let (tx, _rx) = std::sync::mpsc::sync_channel::<sdr_transcription::TranscriptionInput>(1);
        let enable = UiToDsp::EnableTranscription(tx);
        assert!(matches!(enable, UiToDsp::EnableTranscription(_)));

        let disable = UiToDsp::DisableTranscription;
        assert!(matches!(disable, UiToDsp::DisableTranscription));

        // Audio tap (issue #314) — constructed here so a future
        // signature tweak to the Vec<f32> payload or the
        // SyncSender<...> type fails this regression net rather
        // than silently going quiet at the FFI handler site. Per
        // CodeRabbit round 1 on PR #349.
        let (tap_tx, _tap_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(1);
        let enable_tap = UiToDsp::EnableAudioTap(tap_tx);
        assert!(matches!(enable_tap, UiToDsp::EnableAudioTap(_)));

        let disable_tap = UiToDsp::DisableAudioTap;
        assert!(matches!(disable_tap, UiToDsp::DisableAudioTap));

        // Network audio sink (issue #247) — constructed here so a
        // future signature tweak to AudioSinkType, the
        // SetNetworkSinkConfig field set, or the Protocol type
        // fails this regression net rather than silently going
        // quiet at the controller's handler. Per CodeRabbit
        // round 1 on PR #351.
        let set_sink_local = UiToDsp::SetAudioSinkType(crate::sink_slot::AudioSinkType::Local);
        assert!(matches!(
            set_sink_local,
            UiToDsp::SetAudioSinkType(crate::sink_slot::AudioSinkType::Local)
        ));
        let set_sink_network = UiToDsp::SetAudioSinkType(crate::sink_slot::AudioSinkType::Network);
        assert!(matches!(
            set_sink_network,
            UiToDsp::SetAudioSinkType(crate::sink_slot::AudioSinkType::Network)
        ));

        let net_cfg = UiToDsp::SetNetworkSinkConfig {
            hostname: "192.0.2.1".to_string(),
            port: 4242,
            protocol: sdr_types::Protocol::Udp,
        };
        assert!(matches!(
            &net_cfg,
            UiToDsp::SetNetworkSinkConfig {
                hostname,
                port: 4242,
                protocol: sdr_types::Protocol::Udp,
            } if hostname == "192.0.2.1"
        ));

        // RTL-TCP connection controls (commit 3 of PR #335) —
        // constructed directly so a future signature change (e.g.
        // adding an instance-selector param) fails this test
        // rather than silently going quiet at the UI-handler site.
        let disc = UiToDsp::DisconnectRtlTcp;
        assert!(matches!(disc, UiToDsp::DisconnectRtlTcp));
        let retry = UiToDsp::RetryRtlTcpNow;
        assert!(matches!(retry, UiToDsp::RetryRtlTcpNow));

        // RTL-TCP role + auth-key config (issue #396). Constructed
        // with a non-default Listen role and a plausible 32-byte
        // key so the shape regression fires on either a field
        // rename / retyping OR the re-export path going stale. The
        // matching `SetRtlTcpClientConfig` handler is load-bearing
        // for the role picker and per-server keyring flows.
        let cfg = UiToDsp::SetRtlTcpClientConfig {
            requested_role: sdr_server_rtltcp::extension::Role::Listen,
            auth_key: Some(vec![0xAB; 32]),
        };
        assert!(matches!(
            cfg,
            UiToDsp::SetRtlTcpClientConfig {
                requested_role: sdr_server_rtltcp::extension::Role::Listen,
                auth_key: Some(ref bytes),
            } if bytes.len() == 32 && bytes.iter().all(|&b| b == 0xAB)
        ));

        // `RetryRtlTcpWithTakeover` is a unit variant today, but
        // the pattern match fails loudly if that changes (e.g.
        // a future refactor adds a scoped-reason payload).
        let takeover = UiToDsp::RetryRtlTcpWithTakeover;
        assert!(matches!(takeover, UiToDsp::RetryRtlTcpWithTakeover));
    }

    #[test]
    fn test_source_type_variants() {
        // Equality + discrimination across all four variants. RtlTcp
        // is the rtl_tcp-protocol network client added alongside the
        // existing raw Network variant; keep them distinct at the
        // type level.
        assert_eq!(SourceType::RtlSdr, SourceType::RtlSdr);
        assert_ne!(SourceType::RtlSdr, SourceType::Network);
        assert_ne!(SourceType::Network, SourceType::File);
        assert_ne!(SourceType::Network, SourceType::RtlTcp);
        assert_ne!(SourceType::RtlTcp, SourceType::RtlSdr);

        let types = [
            SourceType::RtlSdr,
            SourceType::Network,
            SourceType::File,
            SourceType::RtlTcp,
        ];
        assert_eq!(types.len(), 4);
    }

    #[test]
    fn test_set_source_type_rtl_tcp_message() {
        // Regression coverage for the new variant — make sure the
        // message wraps it and pattern-matches cleanly, same shape as
        // the existing RtlSdr / Network / File branches elsewhere in
        // this test suite.
        let msg = UiToDsp::SetSourceType(SourceType::RtlTcp);
        assert!(matches!(msg, UiToDsp::SetSourceType(SourceType::RtlTcp)));
    }

    #[test]
    fn test_scanner_dsp_to_ui_variants() {
        // Shape regression for the four scanner events added in
        // PR 2 of #317. Catches silent payload changes — if a
        // field gets renamed or the tuple arity changes, the
        // pattern match here fails at compile or runtime.
        let key = sdr_scanner::ChannelKey {
            name: "Test".to_string(),
            frequency_hz: 162_550_000,
        };
        let active = DspToUi::ScannerActiveChannelChanged {
            key: Some(key.clone()),
            freq_hz: 162_550_000,
            demod_mode: sdr_types::DemodMode::Nfm,
            bandwidth: TEST_BANDWIDTH_HZ,
            name: "Test".to_string(),
            ctcss: Some(CtcssMode::Off),
            voice_squelch: None,
        };
        assert!(matches!(
            active,
            DspToUi::ScannerActiveChannelChanged {
                key: Some(_),
                freq_hz: 162_550_000,
                demod_mode: sdr_types::DemodMode::Nfm,
                ctcss: Some(CtcssMode::Off),
                voice_squelch: None,
                ..
            }
        ));

        let idle = DspToUi::ScannerActiveChannelChanged {
            key: None,
            freq_hz: 0,
            demod_mode: sdr_types::DemodMode::Nfm,
            bandwidth: 0.0,
            name: String::new(),
            ctcss: None,
            voice_squelch: None,
        };
        assert!(matches!(
            idle,
            DspToUi::ScannerActiveChannelChanged { key: None, .. }
        ));

        let state_changed = DspToUi::ScannerStateChanged(sdr_scanner::ScannerState::Listening);
        assert!(matches!(
            state_changed,
            DspToUi::ScannerStateChanged(sdr_scanner::ScannerState::Listening)
        ));

        let empty = DspToUi::ScannerEmptyRotation;
        assert!(matches!(empty, DspToUi::ScannerEmptyRotation));

        // Pin each mutex-reason variant — the UI toast text is
        // selected by matching these, so a silent rename would
        // misroute toasts rather than fail compilation.
        let mutex_rec =
            DspToUi::ScannerMutexStopped(ScannerMutexReason::RecordingStoppedForScanner);
        assert!(matches!(
            mutex_rec,
            DspToUi::ScannerMutexStopped(ScannerMutexReason::RecordingStoppedForScanner)
        ));
        let mutex_trans =
            DspToUi::ScannerMutexStopped(ScannerMutexReason::TranscriptionStoppedForScanner);
        assert!(matches!(
            mutex_trans,
            DspToUi::ScannerMutexStopped(ScannerMutexReason::TranscriptionStoppedForScanner)
        ));
        let mutex_scan_rec =
            DspToUi::ScannerMutexStopped(ScannerMutexReason::ScannerStoppedForRecording);
        assert!(matches!(
            mutex_scan_rec,
            DspToUi::ScannerMutexStopped(ScannerMutexReason::ScannerStoppedForRecording)
        ));
        let mutex_scan_trans =
            DspToUi::ScannerMutexStopped(ScannerMutexReason::ScannerStoppedForTranscription);
        assert!(matches!(
            mutex_scan_trans,
            DspToUi::ScannerMutexStopped(ScannerMutexReason::ScannerStoppedForTranscription)
        ));
    }

    #[test]
    fn test_scanner_ui_to_dsp_variants() {
        // Shape regression for the four scanner commands the UI
        // dispatches. Same rationale as the DspToUi test above —
        // enum-shape drift fails here first.
        let key = sdr_scanner::ChannelKey {
            name: "Test".to_string(),
            frequency_hz: 146_520_000,
        };

        let enable = UiToDsp::SetScannerEnabled(true);
        assert!(matches!(enable, UiToDsp::SetScannerEnabled(true)));

        let disable = UiToDsp::SetScannerEnabled(false);
        assert!(matches!(disable, UiToDsp::SetScannerEnabled(false)));

        let update = UiToDsp::UpdateScannerChannels(Vec::new());
        assert!(matches!(
            update,
            UiToDsp::UpdateScannerChannels(ref v) if v.is_empty()
        ));

        let lockout = UiToDsp::LockoutScannerChannel(key.clone());
        assert!(matches!(lockout, UiToDsp::LockoutScannerChannel(_)));

        let unlock = UiToDsp::UnlockScannerChannel(key);
        assert!(matches!(unlock, UiToDsp::UnlockScannerChannel(_)));
    }
}
