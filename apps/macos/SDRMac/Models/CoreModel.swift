//
// CoreModel.swift — observable model layer bridging SdrCoreKit to
// SwiftUI views.
//
// One root model per app instance. Owns the `SdrCore` handle,
// exposes typed bindings, and consumes the event stream on
// `MainActor` so SwiftUI gets mutations on the thread it expects.
//
// Two command-dispatch patterns:
//
//   1. Lifecycle (start/stop) — strict: only flip `isRunning`
//      when the engine accepts the command. Errors go into
//      `lastError`.
//
//   2. Setters (freq, gain, squelch, etc.) — optimistic: flip
//      the UI binding *first* for input responsiveness, then
//      forward to the engine. The engine processes commands
//      async over its mpsc channel, so a successful return only
//      means "queued". Engine-side corrections come back via
//      the event stream.

import Foundation
import Observation
import SdrCoreKit

@MainActor
@Observable
final class CoreModel {
    // ==========================================================
    //  Engine handle
    // ==========================================================

    /// The live engine. Nil until `bootstrap(configPath:)` runs
    /// successfully. All setters guard on this being non-nil.
    private(set) var core: SdrCore?

    /// Event-consuming task. Created in `bootstrap`, cancelled
    /// by `shutdown` on the normal path and by the class
    /// `deinit` as a fallback safety net (see the deinit
    /// further down — closed #293 once Swift 6 isolated-deinit
    /// support landed). Also self-ends naturally when the
    /// underlying `SdrCore` handle is released:
    /// `sdr_core_destroy` closes the engine's event channel,
    /// the AsyncStream completes, and the `for await` loop
    /// exits cleanly.
    private var eventTask: Task<Void, Never>?

    // ==========================================================
    //  Lifecycle state
    // ==========================================================

    var isRunning: Bool = false
    var lastError: String? = nil

    /// True when the Swift side's compiled-against ABI major
    /// version differs from the runtime library's. Set by
    /// `bootstrap(configPath:)` before any engine work. The UI
    /// presents a fatal modal and skips `SdrCore` creation — the
    /// app can't do anything useful against a mismatched ABI, so
    /// the only option is Quit.
    var abiMismatch: (compiled: (major: UInt16, minor: UInt16),
                      runtime: (major: UInt16, minor: UInt16))?

    // ==========================================================
    //  Tuning
    // ==========================================================

    // Match the engine-side default center frequency
    // (`crates/sdr-core/src/controller.rs::DEFAULT_CENTER_FREQ`,
    // 100.000 MHz) so a side-by-side Linux/Mac launch paints
    // the same tuner state before any user action.
    var centerFrequencyHz: Double = 100_000_000
    var vfoOffsetHz: Double = 0
    /// User-selected source sample rate — what the tuner is
    /// configured at (e.g., 2.048 MHz, 2.4 MHz). Bound to the
    /// Source sidebar's sample-rate picker. Pushed to the engine
    /// via `setSampleRate` on user edit. The engine does not
    /// currently echo back source-rate confirmation events, so
    /// this field is optimistic — it reflects the user's
    /// request, not a post-apply readback.
    ///
    /// Default `2_048_000` must match an entry in the picker's
    /// rate list (see `SourceSection.rtlSdrSampleRates`).
    /// Otherwise the Picker would render with no visible
    /// selection on first launch.
    var sourceSampleRateHz: Double = 2_048_000
    /// Engine-reported post-decimation / post-resample rate —
    /// the width of the demodulator's accepted passband, in Hz.
    /// Updated from `SampleRateChanged` events. Used by the
    /// status bar display of "effective rate". NOT the spectrum
    /// display span — that's `displayBandwidthHz` below.
    var effectiveSampleRateHz: Double = 250_000

    /// Engine-reported raw (pre-decimation) sample rate —
    /// the full width of the FFT the engine publishes, which
    /// is also the full width of the Metal spectrum view. The
    /// VFO overlay uses this as its coordinate span.
    ///
    /// Updated from `DisplayBandwidth` events. Defaults to
    /// 2.048 MHz (the typical RTL-SDR source rate) so the VFO
    /// overlay has a sane span before the first engine event
    /// arrives.
    ///
    /// Matches the GTK UI's `set_display_bandwidth()` + stored
    /// `full_bandwidth` (see
    /// `crates/sdr-ui/src/spectrum/mod.rs:244`): two rates,
    /// distinct semantics.
    var displayBandwidthHz: Double = 2_048_000

    /// Scroll / pinch zoom state. When `displayedSpanHz == 0` OR
    /// `>= displayBandwidthHz`, the viewport shows the full FFT
    /// span (no zoom). A smaller value zooms in: only bins whose
    /// frequency falls in
    /// `[displayedCenterOffsetHz - displayedSpanHz/2,
    ///   displayedCenterOffsetHz + displayedSpanHz/2]`
    /// are shown, stretched across the view.
    ///
    /// `displayedCenterOffsetHz` is the center of the viewport,
    /// measured as offset from the tuner center (same frame as
    /// `vfoOffsetHz`). 0 = the tuner-center is the viewport
    /// center; positive = viewport is shifted right.
    ///
    /// Matches the GTK `VfoState::display_start_hz` /
    /// `display_end_hz` concept (see
    /// `crates/sdr-ui/src/spectrum/vfo_overlay.rs::zoom`), but
    /// stored as (center, span) which is friendlier for
    /// cursor-centered zoom math.
    var displayedSpanHz: Double = 0
    var displayedCenterOffsetHz: Double = 0

    /// Minimum displayed span in Hz. Matches GTK's
    /// `MIN_DISPLAY_SPAN_HZ = 1000`.
    static let minDisplayedSpanHz: Double = 1_000

    /// Effective viewport span — resolves the "0 means full" rule
    /// once, everywhere else reads this instead of
    /// `displayedSpanHz` directly.
    var effectiveDisplayedSpanHz: Double {
        displayedSpanHz > 0 && displayedSpanHz < displayBandwidthHz
            ? displayedSpanHz
            : displayBandwidthHz
    }

    var ppmCorrection: Int = 0

    // ==========================================================
    //  Source (advanced) — #246
    // ==========================================================
    //
    //  Defaults match the engine-side defaults in
    //  `crates/sdr-core/src/controller.rs` so a fresh launch of
    //  the Mac app and the GTK app both present the same source
    //  configuration.

    var dcBlockingEnabled: Bool = false
    var iqInversionEnabled: Bool = false
    var iqCorrectionEnabled: Bool = false

    /// Power-of-two decimation ratio (1 = none). 8 matches the
    /// engine default (`sdr_pipeline::iq_frontend::DEFAULT_DECIM`).
    var decimationFactor: UInt32 = 8

    // ----------------------------------------------------------
    //  Source selection — issues #235, #236 (ABI 0.10)
    // ----------------------------------------------------------

    /// Active IQ source. Default is `.rtlSdr` to match the
    /// engine's startup state (`SourceType::RtlSdr` in
    /// `crates/sdr-core/src/controller.rs`). Persisted to
    /// `UserDefaults` so the pick survives relaunches.
    var sourceType: SourceType = .rtlSdr

    /// Network IQ source hostname. Default "localhost" mirrors
    /// the GTK source panel's initial value.
    var networkSourceHost: String = "localhost"

    /// Network IQ source port. Default 1234 matches the
    /// canonical rtl_tcp / IQ-server port convention.
    var networkSourcePort: UInt16 = 1234

    /// Network IQ source transport. Defaults to TCP (dial
    /// outbound); UDP is the "device binds locally and receives
    /// datagrams" mode.
    var networkSourceProtocol: NetworkSourceProtocol = .tcp

    /// File-playback source filesystem path. Empty until the
    /// user picks a file. The engine rejects the source start
    /// on an empty / nonexistent / non-WAV path; no local
    /// validation here.
    var filePath: String = ""

    /// Loop-on-EOF for the file playback source. `false`
    /// default matches the engine's `FileSource::new`
    /// constructor default (stop at EOF). Persisted via
    /// `UserDefaults` under `SDRMac.fileLooping`. Per issue
    /// #236.
    var fileLoopingEnabled: Bool = false

    // ==========================================================
    //  Tuner
    // ==========================================================

    var availableGains: [Double] = []
    var gainDb: Double = 0

    /// Active AGC type — tristate selector that replaced the
    /// two-state `agcEnabled: Bool`. Default `.software` on
    /// fresh installs (sidesteps tuner-AGC pumping behavior).
    /// Persisted via `UserDefaults` under `SDRMac.agcType`.
    /// Per issue #357.
    var agcType: SdrCore.AgcType = .software

    /// `true` when either AGC loop is on. Convenience shim for
    /// call sites that previously read `agcEnabled: Bool` —
    /// gain-slider disable, bookmark capture, etc. The source
    /// of truth is `agcType`; this is a computed view.
    var agcEnabled: Bool { agcType != .off }

    var deviceInfo: String = ""

    /// `true` when a local RTL-SDR dongle was detected on the
    /// USB bus at the last `refreshDeviceInfo` call. Drives the
    /// rtl_tcp server panel's visibility. Kept separate from
    /// `deviceInfo` (which doubles as a display string and is
    /// overwritten by post-Play `.deviceInfo` engine events)
    /// so the UI never has to parse wording to decide
    /// availability. Per `CodeRabbit` round 1 on PR #362.
    var hasLocalRtlSdr: Bool = false

    // ==========================================================
    //  Demod
    // ==========================================================

    var demodMode: DemodMode = .wfm
    var bandwidthHz: Double = 200_000
    var squelchEnabled: Bool = false
    var squelchDb: Float = -60
    var deemphasis: Deemphasis = .us75

    // ==========================================================
    //  Demod (advanced) — #245
    // ==========================================================
    //
    //  Mode-gating rules for the Radio panel:
    //    - FM IF NR: WFM / NFM only
    //    - WFM stereo: WFM only
    //    - Noise blanker + notch: universal
    //
    //  Defaults mirror the engine-side defaults so a toggle from
    //  off → on → off exactly reproduces the engine's own
    //  initial state.

    /// Noise blanker enable (IF stage). Off by default.
    var noiseBlankerEnabled: Bool = false

    /// Noise-blanker threshold multiplier. Engine clamps to
    /// `>= 1.0`; we pick 2.0 as a sensible mid-range starting
    /// point that matches the GTK slider's initial value.
    var noiseBlankerLevel: Float = 2.0

    /// FM IF noise reduction. Off by default; meaningful only
    /// when the active demod is an FM mode (WFM or NFM).
    var fmIfNrEnabled: Bool = false

    /// WFM stereo decode. Off by default; only honored when
    /// the active demod is WFM.
    var wfmStereoEnabled: Bool = false

    /// Audio-stage notch filter. Off by default.
    var notchEnabled: Bool = false

    /// Notch center frequency in Hz. 1 kHz is the common
    /// starting point (also what the GTK UI defaults to).
    var notchFrequencyHz: Float = 1_000

    // ==========================================================
    //  Scanner — issue #447 (ABI 0.20)
    //
    //  All four fields are engine-driven: the Mac side issues
    //  `setScannerEnabled(_:)` and reads back the resulting
    //  state via the `scannerStateChanged` /
    //  `scannerActiveChannelChanged` event arms. We don't flip
    //  these locally — the engine is authoritative.
    //
    //  Bookmark → `ScannerChannel` projection isn't wired yet;
    //  flipping `scannerEnabled` true with no channels leaves
    //  `scannerState == .idle`. Tracked under #490 (per-bookmark
    //  scan/priority) — the panel footer surfaces the gap.
    //
    //  `scannerDefaultDwellMs` / `scannerDefaultHangMs` ARE
    //  stored on this model (not engine-side) — they're "default
    //  fallbacks the host folds into each `ScannerChannel` at
    //  projection time", same pattern the Linux side uses.
    //  Persisted via `UserDefaults` at write time.
    // ==========================================================

    /// Scanner master switch. Set via `setScannerEnabled(_:)`;
    /// the engine echoes the resulting phase via
    /// `scannerStateChanged` (which lands in `scannerState`).
    var scannerEnabled: Bool = false

    /// Scanner phase as last reported by the engine. Drives the
    /// panel's State row and the lockout button's visibility
    /// (button shows only when `scannerActiveChannel != nil`).
    var scannerState: ScannerState = .idle

    /// Channel the scanner is currently latched on, or `nil` when
    /// idle. Drives the Channel row's subtitle and the lockout
    /// button's identity (passed to
    /// `lockoutScannerChannel(name:frequencyHz:)`).
    var scannerActiveChannel: ScannerActiveChannel? = nil

    /// Default per-channel settle time in ms. The host folds
    /// this into each projected `ScannerChannel`'s `dwell_ms`
    /// when the bookmark doesn't carry an override. Range
    /// matches the Linux side (`DWELL_MIN_MS`..`DWELL_MAX_MS`).
    var scannerDefaultDwellMs: Int = 100

    /// Default per-channel hang time in ms. Same projection-time
    /// fallback contract as `scannerDefaultDwellMs`.
    var scannerDefaultHangMs: Int = 2_000

    // ==========================================================
    //  Audio
    // ==========================================================

    var volume: Float = 0.5

    /// Selected audio output device UID. Empty string routes to
    /// the system default (engine-side behavior). The available
    /// list is re-fetched on demand via
    /// `refreshAudioDevices()` — this is a mirror of the user
    /// selection, not a cached snapshot of the device list.
    var selectedAudioDeviceUid: String = ""

    /// Snapshot of output devices the backend enumerated. Filled
    /// by `refreshAudioDevices()`; the AudioSection view calls
    /// that on panel appear so a hot-plug between app launch and
    /// panel open still shows the current list.
    var audioDevices: [SdrCore.AudioDevice] = []

    // ----------------------------------------------------------
    //  Network audio sink — issue #247 (ABI 0.9)
    // ----------------------------------------------------------

    /// Active audio sink. `.local` routes to the selected
    /// CoreAudio device (the default); `.network` streams to a
    /// configured host:port. Optimistic — the engine applies on
    /// the next command cycle; status confirmation arrives via
    /// the `.networkSinkStatus` event below. Persisted to
    /// `UserDefaults` so the choice survives relaunches.
    var audioSinkType: AudioSinkType = .local

    /// Network sink host. Defaults to "localhost" to mirror the
    /// engine's `DEFAULT_NETWORK_SINK_HOST`. Edited via the
    /// Settings Audio pane; committed on explicit Apply (avoids
    /// per-keystroke engine rebuilds).
    var networkSinkHost: String = "localhost"

    /// Network sink port. Default 1234 matches the engine's
    /// `DEFAULT_NETWORK_SINK_PORT` and the historical SDR++
    /// convention.
    var networkSinkPort: UInt16 = 1234

    /// Network sink protocol. TCP server by default (engine
    /// accepts client connections); UDP is unicast to the
    /// configured endpoint.
    var networkSinkProtocol: NetworkProtocol = .tcpServer

    /// Last status the engine reported for the network sink.
    /// Drives the status row in the Settings Audio pane —
    /// `.inactive` on first launch / after a switch back to
    /// local; `.active(...)` once the engine successfully
    /// starts streaming; `.error(...)` on a startup or write
    /// failure.
    var networkSinkStatus: NetworkSinkStatus = .inactive

    // ----------------------------------------------------------
    //  rtl_tcp server — issue #353 (ABI 0.11)
    //
    //  Lets a host with a locally-connected RTL-SDR dongle
    //  share it over the network so other SDR clients (GQRX,
    //  SDR++, another `sdr-rs` instance) can tune it. Handles
    //  live outside `SdrCore` because the server has its own
    //  lifecycle and claims exclusive access to the dongle —
    //  running the engine on the same dongle while the server
    //  is up would deadlock on USB, so the UI enforces mutual
    //  exclusivity on the engine source type.
    // ----------------------------------------------------------

    /// `true` while the rtl_tcp server has an open dongle and
    /// accept thread running. Drives the UI toggle state.
    var rtlTcpServerRunning: Bool = false

    /// `true` from the moment `stopRtlTcpServer()` is called
    /// until its serialized `performStopRtlTcpServer()` (and the
    /// detached `server.stop()` join) has returned. During that
    /// window the UI toggle has already flipped to "off" — the
    /// user's intent landed — but the dongle is still held by
    /// the accept-thread drain, so we must NOT let the engine
    /// selection re-open it as `.rtlSdr`. The mutex guards use
    /// the computed `rtlTcpServerHoldsDongle` which is
    /// `running || stopping`. Per `CodeRabbit` round 7 on PR
    /// #362.
    var rtlTcpServerStopping: Bool = false

    /// `true` whenever the rtl_tcp path currently owns — or is
    /// about to release — the local USB dongle. Engine-side
    /// `.rtlSdr` paths and the sidebar picker both gate on this
    /// rather than `rtlTcpServerRunning` alone; otherwise a
    /// rapid "Stop sharing → Play" can try to open the dongle
    /// while `server.stop()` is still draining the accept thread
    /// and trip a transient busy/open error.
    var rtlTcpServerHoldsDongle: Bool {
        rtlTcpServerRunning || rtlTcpServerStopping
    }

    /// Most-recent poll snapshot of server stats. `nil` while
    /// the server isn't running. Aggregates only — per-client
    /// state moved to the multi-client surface in #391 and
    /// lands on the Mac side in #496.
    var rtlTcpServerStats: SdrRtlTcpServer.Stats? = nil

    // The pre-#391 single-client `rtlTcpRecentCommands` ring
    // is gone — recent-commands tracking is per-client now and
    // requires the multi-client list surface (#496) to map a
    // client id back to its commands. The panel renders the
    // server-wide aggregates `connectedCount` / `lifetimeAccepted`
    // / `totalBytesSent` / `totalBuffersDropped` instead.

    /// Last error surfaced from a rtl_tcp server start or poll
    /// attempt. Cleared on successful start; mirrors into a
    /// UI toast / status line.
    var rtlTcpServerError: String? = nil

    // Persisted config — these seed the `SdrRtlTcpServer.Config`
    // passed on start. UserDefaults round-trip keeps them across
    // launches; the Rust side has no config persistence yet.

    var rtlTcpServerNickname: String = ""
    var rtlTcpServerPort: UInt16 = 1234
    var rtlTcpServerBindAddress: SdrRtlTcpServer.Config.BindAddress = .loopback
    var rtlTcpServerMdnsEnabled: Bool = true

    /// Center frequency the server applies on dongle open.
    /// Defaults to 100.000 MHz matching the engine's
    /// `DEFAULT_CENTER_FREQ`.
    var rtlTcpServerInitialFreqHz: UInt32 = 100_000_000

    /// Initial sample rate. 2.048 Msps matches the engine
    /// default and is the safest RTL-SDR rate.
    var rtlTcpServerInitialSampleRateHz: UInt32 = 2_048_000

    /// Initial tuner gain in 0.1 dB steps. 0 = auto.
    var rtlTcpServerInitialGainTenthsDb: Int32 = 0

    /// Initial PPM correction.
    var rtlTcpServerInitialPpm: Int32 = 0

    /// Initial bias-tee state.
    var rtlTcpServerInitialBiasTee: Bool = false

    /// Initial direct-sampling mode. Typed so invalid values
    /// can't reach the FFI.
    var rtlTcpServerInitialDirectSampling: SdrCore.DirectSamplingMode = .off

    /// Private handle to the running server. Observable
    /// @Observable storage forbids holding non-`Sendable`
    /// reference types through its macro-generated shadow
    /// state, so this goes alongside the @Observable fields
    /// via a nested private class indirection like `eventTask`
    /// above.
    private var rtlTcpServer: SdrRtlTcpServer? = nil

    /// Matching mDNS advertiser handle. Started alongside the
    /// server when `rtlTcpServerMdnsEnabled` is on; stopped
    /// and dropped on server stop.
    private var rtlTcpAdvertiser: SdrRtlTcpAdvertiser? = nil

    /// Background poller that refreshes `rtlTcpServerStats` /
    /// `rtlTcpRecentCommands` every second while the server
    /// is running.
    private var rtlTcpPollTask: Task<Void, Never>? = nil

    /// Monotonic lifecycle token. Incremented on every
    /// start/stop transition. A detached `startRtlTcpServer`
    /// task captures the generation at kickoff and discards
    /// its success result if the generation has moved on by
    /// the time it publishes — otherwise a fast on → off → on
    /// sequence could let an older start complete after a
    /// subsequent stop, republishing stale handles and
    /// restarting the poller while the user thinks the server
    /// is off. Matches the pattern `TranscriptionDriver`
    /// already uses in this repo. Per `CodeRabbit` round 2
    /// on PR #362.
    private var rtlTcpServerLifecycleGeneration: UInt64 = 0

    /// Serializing chain for start/stop transitions. Each
    /// public lifecycle method chains its real work behind the
    /// previous task's `.value`, so a rapid off → on can't
    /// launch a new `SdrRtlTcpServer(config:)` (USB open) while
    /// the prior `server.stop()` accept-thread join is still
    /// draining — that collision surfaces as a transient
    /// "device busy" start failure. Per `CodeRabbit` round 3
    /// on PR #362.
    ///
    /// The generation token (above) catches stale completions
    /// within a single start's detached hop; the task chain
    /// catches cross-transition overlap. Both are needed —
    /// generation alone lets the start's USB open race with a
    /// concurrent stop's USB close.
    private var rtlTcpServerLifecycleTask: Task<Void, Never>? = nil

    // ----------------------------------------------------------
    //  rtl_tcp client — issue #326
    //
    //  Observable state for the SwiftUI client picker: discovered
    //  servers from the mDNS browser, persisted favorites + last-
    //  connected snapshot, and the live connection state surfaced
    //  by the engine's `RtlTcpConnectionState` event. Mirrors the
    //  Linux GTK `source_panel.rs` treatment (favorites keys,
    //  bandwidth advisory threshold, state formatter) so both
    //  frontends have parity.
    // ----------------------------------------------------------

    /// Live mDNS discoveries from `SdrRtlTcpBrowser`, sorted by
    /// `instanceName` for stable ordering. Browser runs from
    /// bootstrap to shutdown — discovering while the picker is
    /// closed is harmless and means the list is already populated
    /// the first time the user expands it.
    var rtlTcpDiscoveredServers: [SdrRtlTcpBrowser.DiscoveredServer] = []

    /// User-pinned favorite servers, persisted as a JSON array in
    /// `UserDefaults`. Starred rows promote to the top of the
    /// picker (UI layer) and survive across sessions even when
    /// the server is offline. Mirrors the Linux `FavoriteEntry`
    /// on-disk schema.
    var rtlTcpFavorites: [RtlTcpClientFavorite] = []

    /// Most recent server the user actually connected to — host,
    /// port, nickname. Populated on successful connect, persisted
    /// in `UserDefaults` so the next launch repopulates the
    /// manual-entry fields + can auto-reconnect without waiting
    /// for mDNS rediscovery.
    var rtlTcpLastConnected: RtlTcpClientLastConnected? = nil

    /// Live connection-state snapshot, mirrored from
    /// `SdrCoreEvent.rtlTcpConnectionState`. `.disconnected`
    /// initial value before the engine reports anything. Drives
    /// the status row's subtitle and the disconnect/retry button
    /// sensitivity in the UI.
    var rtlTcpConnectionState: RtlTcpConnectionState = .disconnected

    /// `true` when the selected **source** sample rate is at or
    /// above 2.4 Msps AND the active source is `.rtlTcp`. At
    /// 8-bit I/Q pairs that works out to ≈38 Mbps on the wire —
    /// below typical Wi-Fi practical throughput for older
    /// routers. The UI shows a caption warning about drops; the
    /// rate still commits because wired gigabit handles it fine.
    /// Threshold matches `HIGH_BANDWIDTH_SAMPLE_RATE_IDX` in
    /// `sdr-ui/src/sidebar/source_panel.rs` (index 7 = 2.4 MHz).
    ///
    /// Keyed on `sourceSampleRateHz` (pre-decimation) NOT
    /// `effectiveSampleRateHz` (post-decimation) — rtl_tcp
    /// streams raw IQ samples at the source rate over the
    /// wire; decimation is applied locally after receive, so
    /// the network bitrate is insensitive to the decimation
    /// factor. Per `CodeRabbit` round 1 on PR #366.
    var rtlTcpShowsBandwidthAdvisory: Bool {
        sourceType == .rtlTcp
            && sourceSampleRateHz >= Self.rtlTcpHighBandwidthThresholdHz
    }

    /// Sample-rate threshold (Hz) at which `rtlTcpShowsBandwidthAdvisory`
    /// kicks in. Matches the Linux constant. `Double` to match
    /// the `effectiveSampleRateHz` @Observable field type.
    static let rtlTcpHighBandwidthThresholdHz: Double = 2_400_000

    /// Bias-T power on the LNA coax. `false` = off (dongle
    /// default). Optimistic — the UI flip commits immediately
    /// and the FFI dispatch follows; the engine has no
    /// observable state event for this toggle so we treat the
    /// model as the source of truth.
    var biasTeeEnabled: Bool = false

    /// Tuner offset-tuning. `false` = off.
    var offsetTuningEnabled: Bool = false

    /// RTL2832 direct-sampling mode. `.off` = normal tuner path.
    /// Distinct from the analog tuner AGC (`agcEnabled`) which
    /// controls the gain loop inside the tuner chip; direct
    /// sampling bypasses the tuner entirely and samples the
    /// antenna input straight into the ADC.
    var directSamplingMode: SdrCore.DirectSamplingMode = .off

    /// RTL2832 digital AGC loop. Distinct from the analog tuner
    /// AGC (`agcEnabled`); an RTL-SDR dongle exposes both
    /// independently and most real-world setups want tuner
    /// AGC ON and RTL AGC OFF. Default follows the engine's
    /// `rtlsdr_set_agc_mode(dev, 0)`.
    var rtlAgcEnabled: Bool = false

    /// Current rtl_tcp gain-by-index selection. `gain_count`
    /// from the `Connected` state is the list length; values
    /// are `0..<gainCount`. Stays at `0` between sessions
    /// because gain tables aren't comparable across servers
    /// (a Mini-2+ and an R820T have different tables), so
    /// persisting the last index would restore a meaningless
    /// slider position on reconnect to a different server.
    var rtlTcpGainIndex: UInt32 = 0

    /// Live USB hotplug monitor. Calls `refreshDeviceInfo()`
    /// on any plug / unplug event so `hasLocalRtlSdr` stays
    /// in sync with reality without the user having to flip
    /// focus or restart the app. Per issue #363. `nil` if
    /// IOKit registration failed at bootstrap — the
    /// scenePhase-on-active probe in `ContentView` remains as
    /// a fallback safety net either way.
    private var usbHotplugMonitor: UsbHotplugMonitor? = nil

    /// Live mDNS browser handle. Started in `bootstrap()` and
    /// stopped in `shutdown()`; `nil` if the OS denied browser
    /// start (rare — mDNS doesn't need special entitlements on
    /// macOS). Events land on a dispatcher thread and hop to
    /// MainActor via `Task { @MainActor in … }` inside the
    /// callback closure.
    private var rtlTcpBrowser: SdrRtlTcpBrowser? = nil

    /// ID of the bookmark currently applied to the engine state.
    /// Set by `apply(_:)` after all its setters run; cleared by
    /// any user-facing setter a bookmark would also touch
    /// (`setCenter`, `setDemodMode`, `setBandwidth`, …) via
    /// `clearActiveBookmark()`, so tuning away from a recalled
    /// bookmark — through ANY path (slider, picker, keyboard
    /// shortcut, spectrum click) — invalidates the flyout's
    /// highlight immediately. Per issue #339.
    var activeBookmarkId: UUID? = nil

    /// Active audio-recording state. `nil` = not recording,
    /// `some(path)` = engine confirmed it opened `path` for
    /// writing. Mirrors `DspToUi::AudioRecordingStarted/Stopped`
    /// — the engine is authoritative; the UI never flips this
    /// optimistically.
    var audioRecordingPath: String? = nil

    /// Active IQ-recording state. Same engine-authoritative flip
    /// as `audioRecordingPath` above, but for the raw IQ stream.
    /// Mirrors `DspToUi::IqRecordingStarted/Stopped`. Independent
    /// from audio recording — both can be active simultaneously.
    var iqRecordingPath: String? = nil

    // ==========================================================
    //  RadioReference
    // ==========================================================

    /// Cached "has stored credentials" flag so SwiftUI views that
    /// gate on it (the sidebar RR section, the Settings pane's
    /// "Clear stored" button) update reactively when credentials
    /// are saved or deleted. `SdrCore.hasRadioReferenceCredentials`
    /// is a static so it doesn't drive view invalidation on its
    /// own — `refreshRadioReferenceCredentialsFlag()` is the
    /// mutation hook that writes this @Observable field after
    /// a save / delete.
    ///
    /// Initialized from the keyring at bootstrap; stays accurate
    /// for the lifetime of the app as long as every mutation
    /// path goes through `refreshRadioReferenceCredentialsFlag`.
    var radioReferenceHasCredentials: Bool = false

    // ==========================================================
    //  Display
    // ==========================================================

    var fftSize: Int = 2048
    var fftWindow: FftWindow = .blackman
    var fftRateFps: Double = 20
    /// Spectrum averaging mode. Display-only — applied in the
    /// Swift renderer before blit; the engine is unaware.
    var averagingMode: AveragingMode = .none
    // Default dB range matches the GTK UI (see
    // `crates/sdr-ui/src/spectrum/mod.rs:58`). -70 dB floor
    // hides the ADC noise floor so the waterfall background is
    // black / cold without the user having to adjust sliders on
    // first launch.
    var minDb: Float = -70
    var maxDb: Float = 0

    // ==========================================================
    //  Status
    // ==========================================================

    var signalLevelDb: Float = -120

    /// Auto-squelch tracks the noise floor in the DSP and
    /// self-adjusts the threshold. This is an engine-side
    /// feature (`sdr-radio::IfChain::set_auto_squelch_enabled`);
    /// the UI just toggles it. Only meaningful when
    /// `squelchEnabled` is also on.
    var autoSquelchEnabled: Bool = false

    // ==========================================================
    //  Bootstrap / shutdown
    // ==========================================================

    /// Build the engine handle and kick off the event-consumption
    /// task. Called once from `ContentView.task` on app launch.
    /// Safe to call multiple times — subsequent calls are no-ops
    /// if the engine is already up.
    func bootstrap(configPath: URL) async {
        guard core == nil else { return }

        // ABI guard. Runs BEFORE any engine work so a mismatched
        // lib can't silently misbehave — a major-version drift
        // between the compiled Swift wrapper and the statically-
        // linked `libsdr_ffi.a` means struct layouts / enum
        // discriminants likely differ and the engine would crash
        // or misinterpret commands. Catch it at the front door.
        let compiled = SdrCore.compiledAbiVersion
        let runtime = SdrCore.abiVersion
        if compiled.major != runtime.major {
            abiMismatch = (compiled: compiled, runtime: runtime)
            lastError = """
                SDR engine ABI major mismatch: compiled against \
                \(compiled.major).\(compiled.minor), runtime reports \
                \(runtime.major).\(runtime.minor). The app can't start.
                """
            return
        }

        // Install the Rust tracing subscriber once at process
        // start so engine errors and info logs land on stderr
        // (captured by Console.app / the xcrun log stream).
        // `initLogging` is idempotent via a OnceLock on the Rust
        // side — safe to call more than once, subsequent calls
        // are no-ops.
        SdrCore.initLogging(minLevel: .info)

        // Probe for RTL-SDR hardware BEFORE creating the engine
        // so the UI can show device presence (or absence) from
        // the first frame, not only after the user hits Play.
        // This is a handle-free libusb device-list query; no USB
        // control transfers, no hardware open.
        refreshDeviceInfo()

        // Wire up live USB hotplug notifications via IOKit so
        // plug / unplug events flip `hasLocalRtlSdr` without
        // waiting for a window-focus refresh. `onChange` is
        // already MainActor-isolated (the IOKit notification
        // port is attached to the main run loop) so we can
        // call refreshDeviceInfo() directly. Per issue #363.
        usbHotplugMonitor = UsbHotplugMonitor { [weak self] in
            self?.refreshDeviceInfo()
        }

        // Start the rtl_tcp mDNS browser early so the picker is
        // already populated when the user first expands it. No-
        // op on macOS without mDNS (Bonjour is always on), but
        // the failure path is non-fatal either way. Per issue
        // #326.
        startRtlTcpBrowser()

        // Restore persisted favorites + last-connected snapshot
        // so the picker's "Known servers" list and the manual-
        // entry defaults round-trip across launches. Per issue
        // #326.
        restoreRtlTcpClientState()

        // Restore rtl_tcp-specific toggle state (bias-T, offset
        // tuning, direct-sampling mode, RTL AGC). Absent keys
        // leave the defaults intact. Gain index is per-session
        // so it's not persisted — see `rtlTcpGainIndex` docs.
        if UserDefaults.standard.object(forKey: Self.rtlTcpBiasTeeKey) != nil {
            biasTeeEnabled = UserDefaults.standard.bool(forKey: Self.rtlTcpBiasTeeKey)
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpOffsetTuningKey) != nil {
            offsetTuningEnabled = UserDefaults.standard.bool(forKey: Self.rtlTcpOffsetTuningKey)
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpDirectSamplingKey) != nil {
            let raw = Int32(UserDefaults.standard.integer(forKey: Self.rtlTcpDirectSamplingKey))
            directSamplingMode = SdrCore.DirectSamplingMode(rawValue: raw) ?? .off
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpRtlAgcKey) != nil {
            rtlAgcEnabled = UserDefaults.standard.bool(forKey: Self.rtlTcpRtlAgcKey)
        }

        // Restore the previously-selected audio output device UID
        // so the user's preference survives across launches.
        // syncToEngine() (on Start) re-applies it; we don't call
        // `core?.setAudioDevice` here because `core` doesn't exist
        // yet — and the engine's default is the same empty-string
        // "system default" sentinel anyway.
        if let saved = UserDefaults.standard.string(forKey: Self.audioDeviceDefaultsKey) {
            selectedAudioDeviceUid = saved
        }
        refreshAudioDevices()

        // Restore source-selection preferences — issues #235, #236.
        // Same additive pattern as the audio-sink block below:
        // absent keys leave the SourceType default (.rtlSdr) and
        // the Rust-side defaults intact.
        if UserDefaults.standard.object(forKey: Self.sourceTypeDefaultsKey) != nil {
            let raw = Int32(UserDefaults.standard.integer(forKey: Self.sourceTypeDefaultsKey))
            sourceType = SourceType(rawValue: raw) ?? .rtlSdr
        }
        // Last band preset the user picked from the General
        // panel — restored as just the ID; resolution against
        // the canonical `bandPresets` slice happens via the
        // `lastSelectedBandPreset` computed accessor. Doesn't
        // re-apply the tuning state (the engine's persisted
        // center/demod/bandwidth are authoritative on launch).
        if let id = UserDefaults.standard.string(
            forKey: Self.lastSelectedBandPresetDefaultsKey
        ) {
            lastSelectedBandPresetID = id
        }
        if UserDefaults.standard.object(forKey: Self.agcTypeDefaultsKey) != nil {
            let raw = Int32(UserDefaults.standard.integer(forKey: Self.agcTypeDefaultsKey))
            agcType = SdrCore.AgcType(rawValue: raw) ?? .software
        }
        if UserDefaults.standard.object(forKey: Self.fileLoopingDefaultsKey) != nil {
            fileLoopingEnabled = UserDefaults.standard.bool(forKey: Self.fileLoopingDefaultsKey)
        }
        if let host = UserDefaults.standard.string(forKey: Self.networkSourceHostDefaultsKey),
           !host.isEmpty {
            networkSourceHost = host
        }
        if UserDefaults.standard.object(forKey: Self.networkSourcePortDefaultsKey) != nil {
            let stored = UserDefaults.standard.integer(forKey: Self.networkSourcePortDefaultsKey)
            if (1...Int(UInt16.max)).contains(stored) {
                networkSourcePort = UInt16(stored)
            }
        }
        if UserDefaults.standard.object(forKey: Self.networkSourceProtocolDefaultsKey) != nil {
            let raw = Int32(UserDefaults.standard.integer(forKey: Self.networkSourceProtocolDefaultsKey))
            networkSourceProtocol = NetworkSourceProtocol(rawValue: raw) ?? .tcp
        }
        if let path = UserDefaults.standard.string(forKey: Self.filePathDefaultsKey),
           !path.isEmpty {
            filePath = path
        }

        // Restore network audio sink preferences — issue #247.
        // Each field is independent; a partial write from a
        // previous version (e.g., only host was set) still
        // upgrades cleanly because the `.object(forKey:)` nil
        // branch leaves the Rust-default value untouched.
        if UserDefaults.standard.object(forKey: Self.audioSinkTypeDefaultsKey) != nil {
            let raw = Int32(UserDefaults.standard.integer(forKey: Self.audioSinkTypeDefaultsKey))
            audioSinkType = AudioSinkType(rawValue: raw) ?? .local
        }
        if let host = UserDefaults.standard.string(forKey: Self.networkSinkHostDefaultsKey),
           !host.isEmpty {
            networkSinkHost = host
        }
        if UserDefaults.standard.object(forKey: Self.networkSinkPortDefaultsKey) != nil {
            let stored = UserDefaults.standard.integer(forKey: Self.networkSinkPortDefaultsKey)
            // Clamp to UInt16 range — a stored value outside that
            // range would come from a previous bug or a manual
            // defaults write; fall back to the default port.
            if (0...Int(UInt16.max)).contains(stored) {
                networkSinkPort = UInt16(stored)
            }
        }
        if UserDefaults.standard.object(forKey: Self.networkSinkProtocolDefaultsKey) != nil {
            let raw = Int32(UserDefaults.standard.integer(forKey: Self.networkSinkProtocolDefaultsKey))
            networkSinkProtocol = NetworkProtocol(rawValue: raw) ?? .tcpServer
        }

        // Restore scanner timing defaults — issue #447.
        // Out-of-range values are silently ignored (the Rust side
        // also clamps); same defensive pattern as the network
        // sink port restore above. The setters clamp at write
        // time so a poisoned defaults entry can only show up
        // here on first restore — subsequent writes go through
        // `setScannerDefault{Dwell,Hang}Ms` which clamp.
        if UserDefaults.standard.object(forKey: Self.scannerDefaultDwellMsDefaultsKey) != nil {
            let stored = UserDefaults.standard.integer(forKey: Self.scannerDefaultDwellMsDefaultsKey)
            if Self.scannerDwellMsRange.contains(stored) {
                scannerDefaultDwellMs = stored
            }
        }
        if UserDefaults.standard.object(forKey: Self.scannerDefaultHangMsDefaultsKey) != nil {
            let stored = UserDefaults.standard.integer(forKey: Self.scannerDefaultHangMsDefaultsKey)
            if Self.scannerHangMsRange.contains(stored) {
                scannerDefaultHangMs = stored
            }
        }

        // Seed the RadioReference credentials flag from the
        // keyring so the sidebar panel / settings pane render
        // correctly on first paint without waiting for a user
        // action.
        refreshRadioReferenceCredentialsFlag()

        // Restore rtl_tcp server config from UserDefaults —
        // issue #353. Same "absent keys leave struct defaults
        // intact" pattern as the audio sink restore above.
        if let nickname = UserDefaults.standard.string(forKey: Self.rtlTcpServerNicknameKey) {
            rtlTcpServerNickname = nickname
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpServerPortKey) != nil {
            let stored = UserDefaults.standard.integer(forKey: Self.rtlTcpServerPortKey)
            if (1...Int(UInt16.max)).contains(stored) {
                rtlTcpServerPort = UInt16(stored)
            }
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpServerBindAddressKey) != nil {
            let raw = Int32(UserDefaults.standard.integer(forKey: Self.rtlTcpServerBindAddressKey))
            rtlTcpServerBindAddress =
                SdrRtlTcpServer.Config.BindAddress(rawValue: raw) ?? .loopback
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpServerMdnsKey) != nil {
            rtlTcpServerMdnsEnabled = UserDefaults.standard.bool(forKey: Self.rtlTcpServerMdnsKey)
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpServerInitialFreqHzKey) != nil {
            let stored = UserDefaults.standard.integer(forKey: Self.rtlTcpServerInitialFreqHzKey)
            if (0...Int(UInt32.max)).contains(stored) {
                rtlTcpServerInitialFreqHz = UInt32(stored)
            }
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpServerInitialSampleRateHzKey) != nil {
            let stored = UserDefaults.standard.integer(
                forKey: Self.rtlTcpServerInitialSampleRateHzKey
            )
            if (1...Int(UInt32.max)).contains(stored) {
                rtlTcpServerInitialSampleRateHz = UInt32(stored)
            }
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpServerInitialGainTenthsDbKey) != nil {
            let stored = UserDefaults.standard.integer(
                forKey: Self.rtlTcpServerInitialGainTenthsDbKey
            )
            if (Int(Int32.min)...Int(Int32.max)).contains(stored) {
                rtlTcpServerInitialGainTenthsDb = Int32(stored)
            }
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpServerInitialPpmKey) != nil {
            let stored = UserDefaults.standard.integer(forKey: Self.rtlTcpServerInitialPpmKey)
            if (Int(Int32.min)...Int(Int32.max)).contains(stored) {
                rtlTcpServerInitialPpm = Int32(stored)
            }
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpServerInitialBiasTeeKey) != nil {
            rtlTcpServerInitialBiasTee =
                UserDefaults.standard.bool(forKey: Self.rtlTcpServerInitialBiasTeeKey)
        }
        if UserDefaults.standard.object(forKey: Self.rtlTcpServerInitialDirectSamplingKey) != nil {
            let raw =
                Int32(UserDefaults.standard.integer(forKey: Self.rtlTcpServerInitialDirectSamplingKey))
            rtlTcpServerInitialDirectSampling =
                SdrCore.DirectSamplingMode(rawValue: raw) ?? .off
        }

        do {
            let c = try SdrCore(configPath: configPath)
            self.core = c
            // Restore sidebar session from the shared config —
            // issue #449. Runs AFTER the engine handle is set
            // because `loadSidebarSession()` reads through the
            // FFI's config surface, which depends on the
            // ConfigManager owned by the engine handle. Has to
            // happen before the SwiftUI scene first paints so the
            // remembered selection / open state / width is on the
            // observable properties before ContentView's bindings
            // wire up. Out-of-range values fall through to the
            // already-set defaults — same defensive policy as the
            // rest of the bootstrap restore block.
            loadSidebarSession()
            // `[weak self]` breaks the retain cycle that would
            // otherwise form: CoreModel → eventTask → closure →
            // self. If the model is dropped (e.g., from a future
            // test that bootstraps + releases in a tight scope),
            // the task ends cleanly on the next iteration instead
            // of pinning the model alive. We keep a strong ref to
            // the stream itself via the `events` capture so the
            // for-await doesn't get cancelled by the weak self
            // going nil mid-event.
            self.eventTask = Task { [weak self, events = c.events] in
                for await event in events {
                    guard let self else { return }
                    self.handleEvent(event)
                }
            }
        } catch {
            self.lastError = "Failed to start engine: \(error)"
        }
    }

    /// Called from `AppDelegate.applicationWillTerminate`. Stops
    /// the engine (best-effort), cancels the event task, and
    /// drops the handle so `SdrCore.deinit` runs and persists
    /// config.
    func shutdown() {
        eventTask?.cancel()
        eventTask = nil
        // Stop any running rtl_tcp server before the engine
        // tears down. Keeps the dongle free for whatever comes
        // next and avoids the Drop-time join running after the
        // model has gone out of scope. Uses the synchronous
        // stop path here — the `async` variant is for the UI
        // toggle; shutdown runs during app termination where
        // blocking briefly is the desired behavior.
        stopRtlTcpServerSync()
        // Stop the rtl_tcp mDNS browser on shutdown — joins the
        // dispatcher thread so no stray callbacks arrive after
        // the model deinits. Per issue #326.
        stopRtlTcpBrowser()

        // Release the USB hotplug monitor's IOKit notification
        // port so no stray plug/unplug callbacks fire after
        // shutdown. `deinit` would do this too, but dropping
        // the reference explicitly here removes any ambiguity
        // about when the IOKit resources get released. Per
        // issue #363.
        usbHotplugMonitor = nil
        if let core {
            // Best-effort stop — a thrown error shouldn't leave
            // the model claiming `isRunning == true` alongside a
            // nil `core`, which the start() idempotency guard
            // would then misread as "already running" and
            // refuse to recover from. Clear `isRunning`
            // unconditionally below so the next bootstrap+start
            // cycle starts from a clean slate.
            try? core.stop()
        }
        isRunning = false
        core = nil
    }

    /// Probe the USB bus for RTL-SDR hardware and populate
    /// `deviceInfo` with the detected device name (or a clear
    /// "not found" string). Handle-free — calls straight into
    /// `sdr-rtlsdr` via the C ABI; no engine instance needed.
    ///
    /// Called from `bootstrap()` for the initial probe, from
    /// the live `UsbHotplugMonitor` on every USB plug/unplug
    /// event (closed issue #363), and from `ContentView`'s
    /// `scenePhase` hook on main-window refocus as a safety-net
    /// fallback. Safe to call repeatedly — the underlying
    /// libusb device-list query is cheap (<1 ms typical).
    func refreshDeviceInfo() {
        let count = SdrCore.deviceCount
        hasLocalRtlSdr = count > 0
        if count == 0 {
            deviceInfo = "No RTL-SDR device found"
            return
        }
        // Only one device is wired through the pipeline today
        // (`RtlSdrSource::new(0)`); when we add a source picker
        // we can list `(0..<count)` and let the user choose.
        // For now, show device 0's name.
        deviceInfo = SdrCore.deviceName(at: 0) ?? "RTL-SDR"
    }

    /// Apply one event to the model. Split out from the `for
    /// await` loop so the task can iterate the stream against a
    /// weak self without duplicating the switch.
    private func handleEvent(_ event: SdrCoreEvent) {
        switch event {
        case .sourceStopped:
            isRunning = false
        case .sampleRateChanged(let hz):
            effectiveSampleRateHz = hz
        case .signalLevel(let db):
            signalLevelDb = db
        case .deviceInfo(let s):
            // The engine publishes `DeviceInfo` when a source
            // opens (see `crates/sdr-core/src/controller.rs`).
            // This is the post-Play confirmation path — for
            // the pre-Play "what's plugged in?" display, see
            // `refreshDeviceInfo()` called from `bootstrap()`.
            // The engine string takes precedence when it arrives
            // because it reflects the device that actually
            // opened (may differ from index 0 if source picker
            // lands in a future version).
            deviceInfo = s
        case .gainList(let gains):
            availableGains = gains
        case .displayBandwidth(let hz):
            let oldBandwidth = displayBandwidthHz
            // Engine-reported raw (pre-decimation) sample rate
            // — the full FFT span, distinct from the post-
            // decimation `effectiveSampleRateHz` published by
            // `SampleRateChanged`. The GTK UI makes the same
            // split; see `crates/sdr-ui/src/window.rs:474` where
            // `DisplayBandwidth(raw_rate)` is routed to
            // `spectrum_handle.set_display_bandwidth(raw_rate)`
            // while `SampleRateChanged` only updates the status
            // bar.
            displayBandwidthHz = hz
            // Keep zoom state consistent with the new full-span
            // value. Without this, a tuner/source switch that
            // shrinks the reported bandwidth leaves
            // `displayedCenterOffsetHz` pointing outside the new
            // range; `SpectrumRenderer.applyZoomWindow` then
            // clamps both fractions to the same edge, collapsing
            // the view to a sliver until the user manually
            // resets zoom. Per #320 review.
            if oldBandwidth != hz {
                normalizeZoomState()
            }
        case .error(let msg):
            lastError = msg
        case .audioRecordingStarted(let path):
            audioRecordingPath = path
        case .audioRecordingStopped:
            audioRecordingPath = nil
        case .iqRecordingStarted(let path):
            iqRecordingPath = path
        case .iqRecordingStopped:
            iqRecordingPath = nil
        case .networkSinkStatus(let status):
            // Engine is authoritative — trust its view of whether
            // the network sink is up, down, or errored. The UI
            // reads this field to render the status row; we don't
            // locally flip state on the setter return code.
            networkSinkStatus = status
        case .rtlTcpConnectionState(let state):
            // Engine is authoritative for rtl_tcp client state
            // — `Connecting`, `Connected(tuner)`, `Retrying(n,
            // secs)`, `Failed(reason)`, `Disconnected`. The UI
            // status row subtitle renders `state` directly;
            // disconnect / retry buttons gate on it. Per issue
            // #326.
            rtlTcpConnectionState = state
        case .scannerStateChanged(let state):
            // Engine is authoritative for the scanner phase —
            // its state machine fires this on every Idle/Retuning
            // /Dwelling/Listening/Hanging transition. The panel
            // reads `scannerState` directly to render the State
            // row.
            scannerState = state
            // Idle is the engine's "rotation parked" sentinel.
            // The active channel readout could still be lying in
            // the previous latched value if `scannerEnabled` was
            // toggled off without the engine emitting a
            // matching null active-channel event first; clear
            // proactively so the panel doesn't keep showing a
            // stale Channel row while State says Off.
            if state == .idle {
                scannerActiveChannel = nil
            }
        case .scannerActiveChannelChanged(let channel):
            // `nil` means "scanner returned to idle / no latched
            // channel" — the panel's Channel row resets to its
            // placeholder. Non-nil carries the bookmark name +
            // frequency the lockout button targets.
            scannerActiveChannel = channel
        case .scannerEmptyRotation:
            // All projected channels are absent or locked out
            // — engine has exhausted its rotation. Surface the
            // exhausted state immediately rather than waiting
            // for the next `scannerStateChanged(.idle)` event:
            // the engine doesn't always emit one (a rotation
            // that can't find a non-locked channel may settle
            // straight back to dwelling, then re-emit empty on
            // the next sweep). Snap state + clear the active
            // readout so the panel reflects "no rotation
            // possible right now" without lag.
            scannerState = .idle
            scannerActiveChannel = nil
        case .scannerMutexStopped(let reason):
            // The scanner ↔ recording / transcription mutex
            // fired. Two of the four reasons mean the scanner
            // itself was stopped (ScannerStoppedFor* directions)
            // — flip `scannerEnabled` false locally so the
            // panel's master toggle reflects engine truth
            // immediately. The other two reasons mean the
            // scanner stopped recording / transcription; the
            // matching recording-state event arms handle their
            // own UI sync, so the scanner panel doesn't need
            // extra work for those directions.
            //
            // Toast surfacing for any of the four directions
            // is a follow-up — we have `reason.toastMessage`
            // ready, but routing it through a notifications
            // surface lands separately.
            switch reason {
            case .scannerStoppedForRecording, .scannerStoppedForTranscription:
                scannerEnabled = false
                scannerState = .idle
                scannerActiveChannel = nil
            case .recordingStoppedForScanner, .transcriptionStoppedForScanner:
                // Inverse direction — the recording / transcription
                // side stopped, scanner is now running. The
                // scanner-state event arm above handles the
                // panel sync; nothing extra here.
                break
            }
        @unknown default:
            // Surface new engine event variants during
            // development. SdrCoreEvent is a non-frozen enum
            // from SdrCoreKit — a future `SDR_EVT_*`
            // discriminant can be added via a minor ABI bump
            // without breaking older hosts, and this arm keeps
            // those extra events visible in the log instead
            // of silently dropped.
            print("[CoreModel] unhandled SdrCoreEvent: \(event)")
        }
    }

    // ==========================================================
    //  Commands — strict (lifecycle)
    // ==========================================================

    func start() {
        // Idempotency guard — repeated Play clicks / Cmd-R
        // presses don't re-sync state or re-enter the engine's
        // start path. The engine warns on "start requested but
        // already running", but cheaper to short-circuit here.
        if isRunning { return }
        guard let core else { lastError = "engine not initialized"; return }
        // Reciprocal guard against the rtl_tcp server. Without
        // this a user could toggle the server on with the
        // source set to `.rtlSdr` (server claims dongle) and
        // then hit Play — the engine would try to open the
        // same USB device and deadlock. Either stop the
        // server or switch source first. The server-side
        // guard in `startRtlTcpServer` covers the opposite
        // direction; the `SourceSection` picker disables
        // `.rtlSdr` cosmetically. Per `CodeRabbit` round 1 on
        // PR #362.
        if rtlTcpServerHoldsDongle && sourceType == .rtlSdr {
            lastError =
                "Local dongle is shared over the network. Stop the rtl_tcp server " +
                "or switch the source away from RTL-SDR before starting the engine."
            return
        }
        // Clear any stale error BEFORE syncing so a setter
        // failure inside syncToEngine() lands on a clean slate
        // and is detectable below.
        lastError = nil
        // Push the UI's current configuration to the engine
        // BEFORE asking it to start. UI defaults and engine
        // defaults don't agree out of the box (engine has its
        // own Rust-side defaults — see `DEFAULT_CENTER_FREQ`
        // etc. in `crates/sdr-core/src/controller.rs`), and
        // optimistic setters only fire when the user touches a
        // control. Syncing on Start guarantees "what you see is
        // what the engine runs with" without waiting for the
        // user to tap every knob.
        syncToEngine()
        // Fail fast if the sync produced a setter error — don't
        // then flip `isRunning` true while the engine is
        // partially configured. `capture` in each setter records
        // the error in `lastError`; if that's non-nil after
        // sync, the engine state doesn't match what the UI
        // displays and starting anyway would produce confusing
        // mismatched behaviour (e.g., tuning landed but demod
        // mode didn't).
        if lastError != nil { return }
        do {
            try core.start()
            isRunning = true
        } catch {
            lastError = "start failed: \(error)"
        }
    }

    /// Push every optimistic-setter UI field to the engine.
    /// Called from `start()` so the engine comes up in the same
    /// state the UI is displaying. Safe to call anytime; each
    /// command is a no-op if the value already matches. Errors
    /// land in `lastError` via the individual setters' `capture`
    /// helper.
    /// Snapshot the current tuning state as a Bookmark. The
    /// user-visible name defaults to the current center
    /// frequency; callers can override. Only the fields the user
    /// would reasonably want to recall are captured; FFT / PPM /
    /// volume are intentionally NOT part of a bookmark — those
    /// feel more like environmental settings than per-station
    /// preferences.
    func snapshotBookmark(name: String? = nil) -> Bookmark {
        Bookmark(
            name: name ?? formatRate(centerFrequencyHz),
            centerFrequencyHz: centerFrequencyHz,
            demodMode: demodMode,
            bandwidthHz: bandwidthHz,
            squelchEnabled: squelchEnabled,
            autoSquelchEnabled: autoSquelchEnabled,
            squelchDb: squelchDb,
            gainDb: gainDb,
            // Legacy boolean kept for backward-compat with
            // pre-#357 bookmarks.json (same dual-field pattern
            // the Linux side uses — agc_type alongside agc).
            agcEnabled: agcEnabled,
            volume: nil,       // feels more like env setting than per-bookmark
            deemphasis: deemphasis,
            agcType: agcType
        )
    }

    /// Apply a saved Bookmark. Each field that's non-nil goes
    /// through the matching setter (so the engine sees the same
    /// command stream a user tapping the UI would send). Fields
    /// left nil are untouched — e.g. a "600 MHz memory channel"
    /// bookmark saved without a squelch setting won't unintentionally
    /// flip the user's current squelch state.
    func apply(_ bookmark: Bookmark) {
        // Each setter below calls `clearActiveBookmark()` as
        // its first line — that's the normal "user tuned away"
        // path. For bookmark recall we re-set the id at the
        // tail after all setters have run, so the flyout
        // highlight lands on this bookmark even though the
        // setters cleared it transiently.
        //
        // NOTE: if a new field is added to `Bookmark`, the
        // matching setter here AND its `clearActiveBookmark()`
        // call must both land to keep the highlight coherent.
        if let hz = bookmark.centerFrequencyHz { setCenter(hz) }
        if let m = bookmark.demodMode          { setDemodMode(m) }
        if let bw = bookmark.bandwidthHz       { setBandwidth(bw) }
        if let on = bookmark.squelchEnabled    { setSquelchEnabled(on) }
        if let db = bookmark.squelchDb         { setSquelchDb(db) }
        if let auto = bookmark.autoSquelchEnabled { setAutoSquelch(auto) }
        if let g = bookmark.gainDb             { setGain(g) }
        // Prefer the tristate `agcType` field when present —
        // pre-#357 bookmarks only carry the legacy `agcEnabled`
        // boolean, which `setAgc(_:)` forwards to the tristate
        // as a fallback.
        if let type = bookmark.agcType         { setAgcType(type) }
        else if let agc = bookmark.agcEnabled  { setAgc(agc) }
        if let v = bookmark.volume             { setVolume(v) }
        if let d = bookmark.deemphasis         { setDeemphasis(d) }
        activeBookmarkId = bookmark.id
    }

    /// Clear the active-bookmark highlight. Called at the top
    /// of every setter a bookmark would also touch so tuning
    /// away from a recalled bookmark via any path invalidates
    /// the flyout indicator immediately. `apply(_:)` re-sets
    /// the id at its tail.
    private func clearActiveBookmark() {
        activeBookmarkId = nil
    }

    func syncToEngine() {
        guard core != nil else { return }
        // Push source-selection state first so the engine has
        // the right network/file config in place before the
        // source actually opens on Start. Per issues #235, #236.
        // Host configs are pushed before the type switch so a
        // restored `.network` / `.file` source opens with the
        // user's values rather than the engine defaults.
        applyNetworkSourceConfig(
            host: networkSourceHost,
            port: networkSourcePort,
            protocol: networkSourceProtocol
        )
        if !filePath.isEmpty {
            setFilePath(filePath)
        }
        // File-loop flag is applied regardless of the current
        // filePath — the engine stores it on DspState so a
        // future `.file` source-type switch picks it up even
        // if no path is set yet. Per issue #236.
        setFileLooping(fileLoopingEnabled)
        setSourceType(sourceType)
        setCenter(centerFrequencyHz)
        setVfoOffset(vfoOffsetHz)
        setSampleRate(sourceSampleRateHz)
        // Source (advanced) — #246. Replayed alongside the
        // basic source fields so a reconnect doesn't leave the
        // engine at its defaults while the UI shows the user's
        // last-picked advanced settings.
        setDecimation(decimationFactor)
        setDcBlocking(dcBlockingEnabled)
        setIqInversion(iqInversionEnabled)
        setIqCorrection(iqCorrectionEnabled)
        setPpm(ppmCorrection)
        setGain(gainDb)
        setAgcType(agcType)
        setDemodMode(demodMode)
        setBandwidth(bandwidthHz)
        setSquelchEnabled(squelchEnabled)
        setSquelchDb(squelchDb)
        setAutoSquelch(autoSquelchEnabled)
        setDeemphasis(deemphasis)
        // Demod (advanced) — #245. See above comment; replay
        // keeps startup/reconnect in sync with the UI's
        // optimistic state.
        setNoiseBlankerEnabled(noiseBlankerEnabled)
        setNoiseBlankerLevel(noiseBlankerLevel)
        setFmIfNrEnabled(fmIfNrEnabled)
        setWfmStereo(wfmStereoEnabled)
        setNotchEnabled(notchEnabled)
        setNotchFrequencyHz(notchFrequencyHz)
        setVolume(volume)
        // Route to the user's last-picked output device. The
        // engine default is "" (system default) so a fresh install
        // re-applies that harmlessly.
        setAudioDevice(selectedAudioDeviceUid)
        // Audio sink type + network endpoint — issue #247.
        // Push the endpoint first so a switch to `.network` lands
        // with the user's chosen host/port instead of the engine
        // defaults.
        applyNetworkSinkConfig(
            host: networkSinkHost,
            port: networkSinkPort,
            protocol: networkSinkProtocol
        )
        setAudioSinkType(audioSinkType)
        setFftSize(fftSize)
        setFftWindow(fftWindow)
        setFftRate(fftRateFps)
    }

    func stop() {
        // Mirror of `start`'s idempotency guard.
        if !isRunning { return }
        guard let core else { return }
        do {
            try core.stop()
            isRunning = false
        } catch {
            lastError = "stop failed: \(error)"
        }
    }

    // ==========================================================
    //  Commands — optimistic setters
    // ==========================================================

    /// Upper bound for center frequency in Hz. Matches the
    /// 12-digit display range in `FrequencyDigitsEntry`
    /// (999.999.999.999 Hz — well above any known SDR tuner).
    /// The clamp in `setCenter` is the canonical validation
    /// point for every tune path — digit entry, VFO click-to-
    /// tune retune, menu shortcuts, engine-event syncs —
    /// instead of each caller reinventing the check.
    static let maxCenterFrequencyHz: Double = 999_999_999_999

    func setCenter(_ hz: Double) {
        clearActiveBookmark()
        // Clamp non-finite and out-of-range values before both
        // the UI write and the engine call. Prevents NaN / Inf /
        // negative tune commands from any caller (per #327 review).
        let clamped: Double
        if !hz.isFinite {
            clamped = centerFrequencyHz
        } else {
            clamped = max(0, min(Self.maxCenterFrequencyHz, hz))
        }
        centerFrequencyHz = clamped
        capture { try core?.tune(clamped) }
    }

    func setSampleRate(_ hz: Double) {
        sourceSampleRateHz = hz
        capture { try core?.setSampleRate(hz) }
    }

    func setVfoOffset(_ hz: Double) {
        vfoOffsetHz = hz
        capture { try core?.setVfoOffset(hz) }
    }

    /// Apply a cursor-centered zoom to the display viewport.
    /// `factor > 1` zooms IN (narrower visible span); `factor < 1`
    /// zooms OUT. `focalOffsetHz` is the frequency under the
    /// cursor (or pinch centroid), measured as an offset from
    /// the tuner center — it stays at the same relative viewport
    /// position through the zoom so the thing you're looking at
    /// doesn't drift out of view.
    ///
    /// Display-only state — does not send anything to the engine.
    /// Matches the GTK behaviour in
    /// `crates/sdr-ui/src/spectrum/vfo_overlay.rs::zoom`.
    func zoomView(by factor: Double, around focalOffsetHz: Double) {
        // Reject non-finite inputs before they propagate into
        // `displayedCenterOffsetHz` and later into grid math /
        // renderer uniforms. Per #320 review.
        guard displayBandwidthHz > 0,
              factor > 0, factor.isFinite,
              focalOffsetHz.isFinite else { return }
        let oldSpan = effectiveDisplayedSpanHz
        let rawSpan = oldSpan / factor
        let newSpan = max(Self.minDisplayedSpanHz, min(displayBandwidthHz, rawSpan))

        // Cursor-centered rescale: keep focalOffsetHz at the
        // same relative fraction of the viewport before and
        // after.
        let oldLeft = displayedCenterOffsetHz - oldSpan / 2
        let frac = oldSpan > 0 ? (focalOffsetHz - oldLeft) / oldSpan : 0.5
        var newCenter = focalOffsetHz - (frac - 0.5) * newSpan

        // Keep viewport inside the full FFT range.
        let halfBw = displayBandwidthHz / 2
        let minCenter = -halfBw + newSpan / 2
        let maxCenter = halfBw - newSpan / 2
        if minCenter <= maxCenter {
            newCenter = max(minCenter, min(maxCenter, newCenter))
        } else {
            newCenter = 0
        }

        displayedSpanHz = newSpan
        displayedCenterOffsetHz = newCenter
    }

    /// Reset the viewport to show the full FFT span.
    func resetZoom() {
        displayedSpanHz = 0
        displayedCenterOffsetHz = 0
    }

    /// Clamp the stored zoom state into the current
    /// `displayBandwidthHz` range. Called when the engine
    /// reports a new full-span value so a shrinking bandwidth
    /// doesn't leave the viewport pointing outside anything.
    ///
    /// Safe to call at any time — a no-op when span / center
    /// are already inside bounds.
    private func normalizeZoomState() {
        guard displayBandwidthHz > 0 else {
            // Can't normalize against a bogus bandwidth — punt
            // until a sane value arrives.
            return
        }
        // Span: 0 means "full span", no clamp needed.
        // Anything >= displayBandwidthHz collapses back to full.
        if displayedSpanHz > displayBandwidthHz {
            displayedSpanHz = 0
        }
        // Center: keep the viewport inside the FFT range. Use
        // the resolved effective span for the bounds so a
        // fully-zoomed-out viewport (span == 0) collapses to
        // center == 0 cleanly.
        let effSpan = effectiveDisplayedSpanHz
        let halfBw = displayBandwidthHz / 2
        let halfSpan = effSpan / 2
        let minCenter = -halfBw + halfSpan
        let maxCenter = halfBw - halfSpan
        if minCenter <= maxCenter {
            displayedCenterOffsetHz = max(minCenter, min(maxCenter, displayedCenterOffsetHz))
        } else {
            displayedCenterOffsetHz = 0
        }
    }

    func setDemodMode(_ m: DemodMode) {
        clearActiveBookmark()
        demodMode = m
        capture { try core?.setDemodMode(m) }
    }

    func setBandwidth(_ hz: Double) {
        clearActiveBookmark()
        bandwidthHz = hz
        capture { try core?.setBandwidth(hz) }
    }

    func setGain(_ db: Double) {
        clearActiveBookmark()
        gainDb = db
        capture { try core?.setGain(db) }
    }

    /// Legacy two-state AGC setter — now a thin forwarder
    /// onto the tristate `setAgcType(_:)` so a bookmark saved
    /// under the old `agcEnabled: Bool` schema still recalls
    /// correctly. `true` maps to `.hardware` (the pre-#357
    /// default); `false` maps to `.off`. New call sites should
    /// use `setAgcType(_:)` directly.
    func setAgc(_ on: Bool) {
        setAgcType(on ? .hardware : .off)
    }

    /// Tristate AGC setter. Persists to `UserDefaults` so the
    /// user's choice survives launches (fresh install default:
    /// `.software` — see `agcType` field docs). Dispatches
    /// both the hardware and software AGC loops atomically
    /// through the ABI 0.13 `setAgcType` FFI.
    func setAgcType(_ type: SdrCore.AgcType) {
        clearActiveBookmark()
        agcType = type
        UserDefaults.standard.set(Int(type.rawValue), forKey: Self.agcTypeDefaultsKey)
        capture { try core?.setAgcType(type) }
    }

    /// UserDefaults key for the persisted AGC type. Absent
    /// key leaves the default (`.software`) intact.
    static let agcTypeDefaultsKey = "SDRMac.agcType"

    /// Toggle loop-on-EOF for the file playback source. `true`
    /// rewinds to the start of the WAV file on EOF and keeps
    /// streaming; `false` stops at EOF. Persisted so the user's
    /// choice survives launches. Engine applies it both to the
    /// running source (next EOF) and to future source rebuilds.
    /// Per issue #236.
    func setFileLooping(_ looping: Bool) {
        fileLoopingEnabled = looping
        UserDefaults.standard.set(looping, forKey: Self.fileLoopingDefaultsKey)
        capture { try core?.setFileLooping(looping) }
    }

    /// UserDefaults key for the persisted file-loop flag.
    /// Absent key leaves the default (`false`) intact.
    static let fileLoopingDefaultsKey = "SDRMac.fileLooping"

    func setSquelchDb(_ db: Float) {
        clearActiveBookmark()
        squelchDb = db
        capture { try core?.setSquelchDb(db) }
    }

    func setSquelchEnabled(_ on: Bool) {
        clearActiveBookmark()
        squelchEnabled = on
        capture { try core?.setSquelchEnabled(on) }
    }

    func setAutoSquelch(_ on: Bool) {
        clearActiveBookmark()
        autoSquelchEnabled = on
        capture { try core?.setAutoSquelch(on) }
    }

    func setDeemphasis(_ m: Deemphasis) {
        clearActiveBookmark()
        deemphasis = m
        capture { try core?.setDeemphasis(m) }
    }

    // ----------------------------------------------------------
    //  Demod (advanced) — #245
    // ----------------------------------------------------------

    func setNoiseBlankerEnabled(_ on: Bool) {
        noiseBlankerEnabled = on
        capture { try core?.setNoiseBlankerEnabled(on) }
    }

    func setNoiseBlankerLevel(_ level: Float) {
        // Guard BEFORE the optimistic write so a programmatic
        // caller passing NaN / Inf / < 1.0 (the UI slider itself
        // is bounded at 1.0...10.0, but any Swift call path can
        // reach this setter) doesn't leave the UI showing a
        // value the engine rejected. Keeps the ABI-level
        // constraint from NB_LEVEL_MIN in sync on both sides.
        // Per CodeRabbit round 2 on PR #347.
        guard level.isFinite, level >= 1.0 else {
            lastError = "invalid noise blanker level: \(level)"
            return
        }
        noiseBlankerLevel = level
        capture { try core?.setNoiseBlankerLevel(level) }
    }

    func setFmIfNrEnabled(_ on: Bool) {
        fmIfNrEnabled = on
        capture { try core?.setFmIfNrEnabled(on) }
    }

    func setWfmStereo(_ on: Bool) {
        wfmStereoEnabled = on
        capture { try core?.setWfmStereo(on) }
    }

    func setNotchEnabled(_ on: Bool) {
        notchEnabled = on
        capture { try core?.setNotchEnabled(on) }
    }

    func setNotchFrequencyHz(_ hz: Float) {
        // Matches the FFI's `freq_hz > 0` contract — reject
        // invalid input up front so UI state can't diverge from
        // engine state when a caller bypasses the slider bounds.
        guard hz.isFinite, hz > 0 else {
            lastError = "invalid notch frequency: \(hz)"
            return
        }
        notchFrequencyHz = hz
        capture { try core?.setNotchFrequencyHz(hz) }
    }

    // ----------------------------------------------------------
    //  Scanner — issue #447 (ABI 0.20)
    // ----------------------------------------------------------

    /// Toggle the scanner master switch. Optimistic — we flip
    /// `scannerEnabled` immediately so the UI doesn't lag, but
    /// the engine's `scannerStateChanged` reply is authoritative
    /// for the resulting phase (`scannerState`). Until #490
    /// lands the per-bookmark `scan_enabled` projection,
    /// flipping this on with no scan-enabled bookmarks leaves
    /// the engine in `.idle` (no rotation to drive), and the
    /// panel's State row reflects that.
    func setScannerEnabled(_ on: Bool) {
        scannerEnabled = on
        capture { try core?.setScannerEnabled(on) }
    }

    /// Lock out the channel currently latched (if any) for the
    /// rest of the scanner session. No-op when the scanner
    /// isn't latched on a channel. The engine's lockout set is
    /// session-scoped — disabling the scanner clears it.
    func lockoutCurrentScannerChannel() {
        guard let channel = scannerActiveChannel else { return }
        capture {
            try core?.lockoutScannerChannel(
                name: channel.name,
                frequencyHz: channel.frequencyHz
            )
        }
    }

    /// Allowed range for the default-dwell setting (ms). Mirrors
    /// the Linux `DWELL_MIN_MS` / `DWELL_MAX_MS` constants; kept
    /// model-side because the SwiftUI Stepper enforces the same
    /// bounds AND a non-UI caller (scripted defaults edit, future
    /// AppleScript / shortcut, test code) can write through the
    /// setter directly.
    static let scannerDwellMsRange: ClosedRange<Int> = 50...500
    /// Allowed range for the default-hang setting (ms). Mirrors
    /// the Linux `HANG_MIN_MS` / `HANG_MAX_MS` constants.
    static let scannerHangMsRange: ClosedRange<Int> = 500...5_000

    /// Update the default settle time per-channel (ms). The
    /// host applies this at projection time; engine doesn't
    /// store a separate "default" — it sees a fully-resolved
    /// `dwell_ms` on each `ScannerChannel`. Persisted via
    /// `UserDefaults` so the choice survives relaunches.
    ///
    /// Out-of-range values are clamped at the boundary so a
    /// non-UI caller can't poison persistent state with a value
    /// that's silently dropped on next launch (bootstrap also
    /// validates the range when reading back). Per `CodeRabbit`
    /// round 1 on PR #497.
    func setScannerDefaultDwellMs(_ ms: Int) {
        let range = Self.scannerDwellMsRange
        let clamped = min(max(ms, range.lowerBound), range.upperBound)
        scannerDefaultDwellMs = clamped
        UserDefaults.standard.set(clamped, forKey: Self.scannerDefaultDwellMsDefaultsKey)
    }

    /// Update the default hang time per-channel (ms). Same
    /// projection-time fallback contract — and same clamp
    /// rationale — as the dwell setter.
    func setScannerDefaultHangMs(_ ms: Int) {
        let range = Self.scannerHangMsRange
        let clamped = min(max(ms, range.lowerBound), range.upperBound)
        scannerDefaultHangMs = clamped
        UserDefaults.standard.set(clamped, forKey: Self.scannerDefaultHangMsDefaultsKey)
    }

    /// `UserDefaults` key for the scanner default-dwell (ms).
    /// Mirrors the Linux config key
    /// (`crates/sdr-ui/src/sidebar/scanner_panel.rs`'s
    /// `CONFIG_KEY_DEFAULT_DWELL_MS`); we don't share storage
    /// with the GTK side yet (separate `UserDefaults` /
    /// `sdr-config`) but the key name stays identical so a
    /// future shared-config layer can round-trip without
    /// renaming.
    static let scannerDefaultDwellMsDefaultsKey = "scanner_default_dwell_ms"

    /// `UserDefaults` key for the scanner default-hang (ms).
    /// Same name-parity rationale as the dwell key.
    static let scannerDefaultHangMsDefaultsKey = "scanner_default_hang_ms"

    // ----------------------------------------------------------
    //  Source (advanced) — #246
    // ----------------------------------------------------------

    func setDcBlocking(_ on: Bool) {
        dcBlockingEnabled = on
        capture { try core?.setDcBlocking(on) }
    }

    func setIqInversion(_ on: Bool) {
        iqInversionEnabled = on
        capture { try core?.setIqInversion(on) }
    }

    func setIqCorrection(_ on: Bool) {
        iqCorrectionEnabled = on
        capture { try core?.setIqCorrection(on) }
    }

    func setDecimation(_ factor: UInt32) {
        // Engine's `SetDecimation` handler requires a nonzero
        // power of two. `nonzeroBitCount == 1` is equivalent to
        // "exactly one bit set," which captures both conditions
        // in one expression. The UI picker only emits values
        // from {1, 2, 4, 8, 16}, but a programmatic caller
        // could pass anything — reject-with-lastError keeps the
        // UI honest.
        guard factor.nonzeroBitCount == 1 else {
            lastError = "invalid decimation factor: \(factor)"
            return
        }
        decimationFactor = factor
        capture { try core?.setDecimation(factor) }
    }

    func setVolume(_ v: Float) {
        clearActiveBookmark()
        volume = v
        capture { try core?.setVolume(v) }
    }

    /// Select an audio output device by UID. Empty string routes
    /// to the system default. The engine re-opens the sink
    /// transactionally — on a failed swap the previous device is
    /// restored (see `AudioSink::set_target` docs).
    ///
    /// The selection is persisted to `UserDefaults` so the same
    /// device is picked on next launch; the Rust config layer
    /// doesn't currently round-trip this value (v3 when config
    /// JSON grows to match the GTK layout).
    func setAudioDevice(_ uid: String) {
        selectedAudioDeviceUid = uid
        UserDefaults.standard.set(uid, forKey: Self.audioDeviceDefaultsKey)
        capture { try core?.setAudioDevice(uid) }
    }

    /// UserDefaults key for the persisted audio device UID.
    static let audioDeviceDefaultsKey = "SDRMac.selectedAudioDeviceUid"

    // ----------------------------------------------------------
    //  Network audio sink setters — issue #247
    // ----------------------------------------------------------

    /// Switch between local and network audio sinks. Optimistic:
    /// flip the UI field before dispatching so the Settings
    /// picker tracks the tap immediately. Engine status
    /// transitions come back via `.networkSinkStatus` events.
    func setAudioSinkType(_ type: AudioSinkType) {
        audioSinkType = type
        UserDefaults.standard.set(Int(type.rawValue), forKey: Self.audioSinkTypeDefaultsKey)
        capture { try core?.setAudioSinkType(type) }
    }

    /// Apply the current host/port/protocol to the engine.
    /// Called on explicit Apply in the Settings pane rather than
    /// on every keystroke — the engine rebuilds the listener /
    /// socket on receipt, so per-keystroke commits would thrash
    /// TCP/UDP state while the user is still typing.
    func applyNetworkSinkConfig(
        host: String,
        port: UInt16,
        protocol proto: NetworkProtocol
    ) {
        // Guard against an empty host that the FFI would reject
        // anyway — surface a local error without the FFI round-trip
        // so the Settings pane can point at the field cleanly.
        let trimmed = host.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            lastError = "Network sink host cannot be empty"
            return
        }
        networkSinkHost = trimmed
        networkSinkPort = port
        networkSinkProtocol = proto
        UserDefaults.standard.set(trimmed, forKey: Self.networkSinkHostDefaultsKey)
        UserDefaults.standard.set(Int(port), forKey: Self.networkSinkPortDefaultsKey)
        UserDefaults.standard.set(Int(proto.rawValue), forKey: Self.networkSinkProtocolDefaultsKey)
        capture {
            try core?.setNetworkSinkConfig(hostname: trimmed, port: port, protocol: proto)
        }
    }

    // ----------------------------------------------------------
    //  rtl_tcp server — issue #353
    // ----------------------------------------------------------

    /// Start the rtl_tcp server from the current persisted
    /// config. Async — the blocking FFI lifecycle work runs
    /// off the `@MainActor` so the sidebar doesn't freeze for
    /// the USB open + accept-thread spawn (100-500 ms on typical
    /// hardware). The running flag flips optimistically so the
    /// toggle reflects user intent immediately; a failed start
    /// rolls it back and sets `rtlTcpServerError`. Per
    /// `CodeRabbit` round 1 on PR #362.
    ///
    /// The actual work is serialized behind any in-flight
    /// lifecycle transition via `rtlTcpServerLifecycleTask`, so
    /// a fast stop→start can't open USB before the prior
    /// server's accept-thread join has released the dongle. Per
    /// `CodeRabbit` round 3 on PR #362.
    func startRtlTcpServer() async {
        if rtlTcpServerRunning {
            return
        }
        // Guard: we only allow starting when the engine isn't
        // currently tied to the local RTL-SDR dongle. The UI
        // disables the toggle in that state too, but non-UI
        // paths (tests, keyboard shortcuts) could bypass the
        // visual guard.
        if isRunning && sourceType == .rtlSdr {
            rtlTcpServerError =
                "Stop the engine or switch the source off RTL-SDR before starting the server."
            return
        }
        // Reject port 0 before the optimistic flip. The UI
        // Stepper clamps to 1024…65535, but non-UI paths
        // (tests, keyboard shortcuts, stale UserDefaults) could
        // stage a zero here; `SdrRtlTcpServer.Config` would
        // interpret it as "use the crate default 1234", while
        // the mDNS advertiser rejects 0 — split-brain where the
        // server is live on 1234 but the UI still shows 0 and
        // the panel surfaces an mDNS warning. Per `CodeRabbit`
        // round 5 on PR #362.
        guard rtlTcpServerPort != 0 else {
            rtlTcpServerError = "rtl_tcp server port must be in 1…65535"
            return
        }
        rtlTcpServerError = nil

        // Optimistic: flip the flag now so the toggle settles
        // on the "on" position and the config form disables
        // itself. Rollback on failure inside `performStartRtlTcpServer`.
        rtlTcpServerRunning = true

        // Chain behind any in-flight stop/start. `await
        // prior?.value` suspends until the previous lifecycle
        // transition has fully drained its detached USB
        // work — so when `performStartRtlTcpServer` runs, the
        // dongle is guaranteed to be free.
        let prior = rtlTcpServerLifecycleTask
        let task = Task { [weak self] in
            await prior?.value
            await self?.performStartRtlTcpServer()
        }
        rtlTcpServerLifecycleTask = task
        await task.value
    }

    /// Serialized body of `startRtlTcpServer`. Runs on the main
    /// actor from inside `rtlTcpServerLifecycleTask` only — the
    /// caller has already done the quick main-actor guards and
    /// optimistic state flip.
    @MainActor
    private func performStartRtlTcpServer() async {
        // Defensive double-check: if a stop flipped the flag
        // between optimistic `rtlTcpServerRunning = true` and
        // the chained task actually running (e.g. user spammed
        // the toggle), bail rather than resurrect a session
        // the user already cancelled.
        guard rtlTcpServerRunning else { return }

        // Bump the lifecycle token before kickoff so this
        // start's completion can check whether a later stop /
        // start has already moved past it. Overflow via
        // wrapping-add — the stdlib can't reach `u64::MAX`
        // flips in any realistic session, but the `&+=` keeps
        // the code warning-free if that day ever came.
        rtlTcpServerLifecycleGeneration &+= 1
        let generation = rtlTcpServerLifecycleGeneration

        // Snapshot the config on the main actor before we hop
        // off — the fields are `@Observable` so reading them
        // post-hop would require re-hopping.
        let cfg = SdrRtlTcpServer.Config(
            bindAddress: rtlTcpServerBindAddress,
            port: rtlTcpServerPort,
            deviceIndex: 0,
            bufferCapacity: 0,
            initialFreqHz: rtlTcpServerInitialFreqHz,
            initialSampleRateHz: rtlTcpServerInitialSampleRateHz,
            initialGainTenthsDb: rtlTcpServerInitialGainTenthsDb,
            initialPpm: rtlTcpServerInitialPpm,
            initialBiasTee: rtlTcpServerInitialBiasTee,
            initialDirectSampling: rtlTcpServerInitialDirectSampling
        )
        let mdnsEnabled = rtlTcpServerMdnsEnabled
        let nickname = rtlTcpServerNickname
        let port = rtlTcpServerPort
        // App version for the mDNS TXT record. Sourced from
        // the bundle's `CFBundleShortVersionString` so the
        // advertised version tracks the installed release
        // rather than a hardcoded constant that rots on every
        // bump. Per `CodeRabbit` round 4 on PR #362.
        let appVersion = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String
            ?? "0.1.0"

        let result: StartResult = await Task.detached(priority: .userInitiated) {
            let server: SdrRtlTcpServer
            do {
                server = try SdrRtlTcpServer(config: cfg)
            } catch {
                return .failed("Start failed: \(error)")
            }

            // Best-effort mDNS announce. Failure here doesn't
            // block the server — a LAN without mDNS is still
            // usable by direct host:port entry on the client.
            var advertiser: SdrRtlTcpAdvertiser? = nil
            var warning: String? = nil
            if mdnsEnabled {
                // Probe tuner metadata via a stats snapshot so
                // the TXT record has accurate tuner +
                // gain-count fields.
                let tunerName: String
                let gainCount: UInt32
                if let s = try? server.stats() {
                    tunerName = s.tunerName.isEmpty ? "unknown" : s.tunerName
                    gainCount = s.gainCount
                } else {
                    tunerName = "unknown"
                    gainCount = 0
                }
                let instanceName = nickname.isEmpty
                    ? ProcessInfo.processInfo.hostName
                    : nickname
                let opts = SdrRtlTcpAdvertiser.Options(
                    port: port,
                    instanceName: instanceName,
                    hostname: "",
                    tuner: tunerName,
                    version: appVersion,
                    gains: gainCount,
                    nickname: nickname
                )
                advertiser = try? SdrRtlTcpAdvertiser(options: opts)
                if advertiser == nil {
                    warning = "mDNS announce failed; server is still running on port \(port)"
                }
            }
            return .succeeded(server: server, advertiser: advertiser, warning: warning)
        }.value

        // Back on @MainActor. Bail out if a later stop / start
        // bumped the generation while this task was off-main —
        // the result is stale. For a successful-but-stale
        // result we still have to tear the handles down
        // explicitly (otherwise the server keeps running
        // despite `rtlTcpServerRunning == false`); failures
        // are safe to drop on the floor. Per `CodeRabbit`
        // round 2 on PR #362.
        guard generation == rtlTcpServerLifecycleGeneration else {
            if case .succeeded(let server, let advertiser, _) = result {
                await Task.detached(priority: .userInitiated) {
                    advertiser?.stop()
                    server.stop()
                }.value
            }
            return
        }

        switch result {
        case .succeeded(let server, let advertiser, let warning):
            rtlTcpServer = server
            rtlTcpAdvertiser = advertiser
            if let warning {
                rtlTcpServerError = warning
            }
            startRtlTcpPoller()
        case .failed(let message):
            rtlTcpServerRunning = false
            rtlTcpServerError = message
        }
    }

    /// Private result shape for the off-main start task. Holds
    /// the constructed handles in the success case so we can
    /// publish them in one MainActor step.
    private enum StartResult: Sendable {
        case succeeded(server: SdrRtlTcpServer, advertiser: SdrRtlTcpAdvertiser?, warning: String?)
        case failed(String)
    }

    /// Stop the server and the advertiser. Async — the
    /// blocking accept-thread join happens off the main actor
    /// so the toggle flip doesn't freeze the sidebar while the
    /// poll-interval epilogue drains (~100-200 ms). Observable
    /// state is cleared immediately so the UI reflects the
    /// intent; the background task then tears down the
    /// handles. Per `CodeRabbit` round 1 on PR #362.
    ///
    /// The actual teardown is serialized behind any in-flight
    /// lifecycle transition via `rtlTcpServerLifecycleTask`, so
    /// if a prior start's USB open is still running we wait for
    /// it to publish before tearing down — otherwise stop would
    /// read `nil` handles from the optimistic main-actor state,
    /// and the start (when it finally lands) would republish a
    /// live server that nobody is watching. Per `CodeRabbit`
    /// round 3 on PR #362.
    func stopRtlTcpServer() async {
        // Bump the lifecycle token so any in-flight start
        // treats itself as stale when it comes back to publish.
        rtlTcpServerLifecycleGeneration &+= 1
        rtlTcpPollTask?.cancel()
        rtlTcpPollTask = nil
        rtlTcpServerRunning = false
        // Keep the USB mutex held (via `rtlTcpServerHoldsDongle`)
        // until teardown actually finishes. Flipping `Running`
        // to false already makes the Toggle snap to "off" so the
        // user's intent is reflected instantly, but the accept-
        // thread join inside the detached `server.stop()` can
        // still take ~100-200 ms — during that window the engine
        // must NOT be allowed to re-open the dongle as `.rtlSdr`.
        // Per `CodeRabbit` round 7 on PR #362.
        rtlTcpServerStopping = true
        rtlTcpServerStats = nil

        let prior = rtlTcpServerLifecycleTask
        let task = Task { [weak self] in
            await prior?.value
            await self?.performStopRtlTcpServer()
        }
        rtlTcpServerLifecycleTask = task
        await task.value
        // Teardown fully drained — release the mutex.
        rtlTcpServerStopping = false
    }

    /// Serialized body of `stopRtlTcpServer`. Runs on the main
    /// actor from inside `rtlTcpServerLifecycleTask` only — the
    /// caller has already cleared observable state.
    @MainActor
    private func performStopRtlTcpServer() async {
        let server = rtlTcpServer
        let advertiser = rtlTcpAdvertiser
        rtlTcpServer = nil
        rtlTcpAdvertiser = nil

        // If there's nothing to tear down, skip the detached
        // task entirely — common case for rapid toggle flips
        // where the prior start bailed on stale generation.
        if server == nil && advertiser == nil {
            return
        }

        await Task.detached(priority: .userInitiated) {
            // Swift `deinit` would eventually call the same
            // FFI stop, but we want the join to complete
            // deterministically (so a follow-up start can
            // reclaim the dongle without racing). Explicit
            // `stop()` on each handle does that; dropping the
            // locals after makes the `deinit` a no-op.
            advertiser?.stop()
            server?.stop()
        }.value
    }

    /// Shutdown-time stop path. Synchronous — the app is
    /// quitting, so the background-dispatch dance buys
    /// nothing; we'd rather the dongle release deterministically
    /// before process exit. Called from `shutdown()` which is
    /// itself synchronous per the `AppDelegate.applicationWillTerminate`
    /// contract.
    private func stopRtlTcpServerSync() {
        // Same lifecycle-token bump as the async variant so an
        // in-flight start (e.g. user toggled on during app
        // shutdown) still treats its completion as stale.
        rtlTcpServerLifecycleGeneration &+= 1
        // Cancel the serializing chain so any still-suspended
        // tasks drop immediately rather than holding onto
        // `self` past shutdown. The FFI stop below is
        // idempotent (mutex-guarded in `sdr-ffi`), so a
        // detached start/stop that finishes after this runs is
        // safe — it just operates on already-consumed handles.
        rtlTcpServerLifecycleTask?.cancel()
        rtlTcpServerLifecycleTask = nil
        rtlTcpPollTask?.cancel()
        rtlTcpPollTask = nil
        let server = rtlTcpServer
        let advertiser = rtlTcpAdvertiser
        rtlTcpServer = nil
        rtlTcpAdvertiser = nil
        rtlTcpServerRunning = false
        // Clear any leftover `rtlTcpServerStopping` flag that an
        // interrupted async `stopRtlTcpServer()` may have left
        // set. Safe regardless of entry state — by the time the
        // synchronous `server?.stop()` below returns, the dongle
        // is released. Per `CodeRabbit` round 7 on PR #362.
        rtlTcpServerStopping = false
        rtlTcpServerStats = nil
        advertiser?.stop()
        server?.stop()
    }

    /// Persist the rtl_tcp server config to UserDefaults so
    /// the same values seed the next launch. Called from the
    /// UI on field edits — start() reads from the persisted
    /// state.
    func persistRtlTcpServerConfig() {
        UserDefaults.standard.set(rtlTcpServerNickname, forKey: Self.rtlTcpServerNicknameKey)
        UserDefaults.standard.set(Int(rtlTcpServerPort), forKey: Self.rtlTcpServerPortKey)
        UserDefaults.standard.set(
            Int(rtlTcpServerBindAddress.rawValue),
            forKey: Self.rtlTcpServerBindAddressKey
        )
        UserDefaults.standard.set(rtlTcpServerMdnsEnabled, forKey: Self.rtlTcpServerMdnsKey)
        UserDefaults.standard.set(
            Int(rtlTcpServerInitialFreqHz),
            forKey: Self.rtlTcpServerInitialFreqHzKey
        )
        UserDefaults.standard.set(
            Int(rtlTcpServerInitialSampleRateHz),
            forKey: Self.rtlTcpServerInitialSampleRateHzKey
        )
        UserDefaults.standard.set(
            Int(rtlTcpServerInitialGainTenthsDb),
            forKey: Self.rtlTcpServerInitialGainTenthsDbKey
        )
        UserDefaults.standard.set(
            Int(rtlTcpServerInitialPpm),
            forKey: Self.rtlTcpServerInitialPpmKey
        )
        UserDefaults.standard.set(rtlTcpServerInitialBiasTee, forKey: Self.rtlTcpServerInitialBiasTeeKey)
        UserDefaults.standard.set(
            Int(rtlTcpServerInitialDirectSampling.rawValue),
            forKey: Self.rtlTcpServerInitialDirectSamplingKey
        )
    }

    /// Background poller that refreshes `rtlTcpServerStats`
    /// on a one-second tick. Runs on the main actor (the whole
    /// `CoreModel` is `@MainActor`) which matches what
    /// `@Observable` needs for writes.
    ///
    /// The pre-#391 per-client `rtlTcpRecentCommands` refresh
    /// is gone — recent-commands tracking is per-client now and
    /// returns under a separate poll keyed by `client.id` once
    /// the multi-client surface lands (#496).
    private func startRtlTcpPoller() {
        rtlTcpPollTask = Task { [weak self] in
            // Tick cadence slow enough to be negligible on the
            // main thread, fast enough that aggregate counters
            // look live. 1 Hz is the GTK panel's tick too.
            let tickNanos: UInt64 = 1_000_000_000
            // Poll-then-sleep ordering so `rtlTcpServerStats`
            // populates immediately on server start rather than
            // after a full tick of nil. Per `CodeRabbit` round 4
            // on PR #362.
            while !Task.isCancelled {
                guard let self, let server = self.rtlTcpServer else { return }
                do {
                    self.rtlTcpServerStats = try server.stats()
                } catch {
                    // Server has gone away (stopped externally,
                    // USB unplug, panic caught by the FFI). Tear
                    // down the handles so the UI reflects reality.
                    // Use the sync path — we're already on the
                    // poll loop and blocking briefly on a single
                    // `stop()` call is fine; the alternative
                    // `await stopRtlTcpServer()` would race with
                    // this Task's own cancellation inside stop.
                    self.rtlTcpServerError = "Poll failed: \(error)"
                    self.stopRtlTcpServerSync()
                    return
                }
                try? await Task.sleep(nanoseconds: tickNanos)
            }
        }
    }

    /// UserDefaults keys for the persisted rtl_tcp server
    /// config. Namespaced to `SDRMac.rtlTcpServer.*` so they
    /// don't collide with the regular engine config keys.
    static let rtlTcpServerNicknameKey = "SDRMac.rtlTcpServer.nickname"
    static let rtlTcpServerPortKey = "SDRMac.rtlTcpServer.port"
    static let rtlTcpServerBindAddressKey = "SDRMac.rtlTcpServer.bindAddress"
    static let rtlTcpServerMdnsKey = "SDRMac.rtlTcpServer.mdns"
    static let rtlTcpServerInitialFreqHzKey = "SDRMac.rtlTcpServer.initialFreqHz"
    static let rtlTcpServerInitialSampleRateHzKey = "SDRMac.rtlTcpServer.initialSampleRateHz"
    static let rtlTcpServerInitialGainTenthsDbKey = "SDRMac.rtlTcpServer.initialGainTenthsDb"
    static let rtlTcpServerInitialPpmKey = "SDRMac.rtlTcpServer.initialPpm"
    static let rtlTcpServerInitialBiasTeeKey = "SDRMac.rtlTcpServer.initialBiasTee"
    static let rtlTcpServerInitialDirectSamplingKey = "SDRMac.rtlTcpServer.initialDirectSampling"

    /// UserDefaults keys for the persisted rtl_tcp *client*
    /// favorites and last-connected snapshot. Namespaced to
    /// `SDRMac.rtlTcpClient.*` to stay distinct from the server
    /// keys above. Stored as JSON strings (arrays / objects via
    /// `JSONEncoder`) — matches the Linux `source_panel.rs`
    /// on-disk shape where the same records live under
    /// `rtl_tcp_client_favorites` / `rtl_tcp_client_last_connected`
    /// inside the engine `ConfigManager` JSON.
    static let rtlTcpClientFavoritesKey = "SDRMac.rtlTcpClient.favorites"
    static let rtlTcpClientLastConnectedKey = "SDRMac.rtlTcpClient.lastConnected"

    // ----------------------------------------------------------
    //  rtl_tcp client — issue #326
    // ----------------------------------------------------------

    /// Start the mDNS browser for `_rtl_tcp._tcp.local.`
    /// announcements. Called from `bootstrap()`; the browser
    /// runs for the app's lifetime so the discovered list is
    /// pre-populated the first time the user expands the
    /// picker, matching the Linux GTK behavior. Failure here is
    /// a warning — the picker still works via favorites and
    /// manual entry, just without auto-discovery.
    private func startRtlTcpBrowser() {
        // Idempotent — a re-bootstrap (hypothetically) won't
        // spawn a second browser.
        if rtlTcpBrowser != nil { return }
        do {
            rtlTcpBrowser = try SdrRtlTcpBrowser { [weak self] event in
                // Callback fires on the browser's dispatcher
                // thread — hop to MainActor to mutate
                // `@Observable` state.
                Task { @MainActor [weak self] in
                    self?.applyRtlTcpBrowserEvent(event)
                }
            }
        } catch {
            print("[CoreModel] rtl_tcp mDNS browser failed to start: \(error)")
            rtlTcpBrowser = nil
        }
    }

    /// Stop the browser. Called from `shutdown()` — the
    /// `deinit` would also release the handle, but explicit
    /// teardown joins the dispatcher thread before we exit.
    private func stopRtlTcpBrowser() {
        rtlTcpBrowser?.stop()
        rtlTcpBrowser = nil
    }

    /// Merge one browser event into `rtlTcpDiscoveredServers`,
    /// keyed on `instanceName` (the mDNS DNS-SD instance name
    /// is the stable identity; same instance re-announcing from
    /// a new IP replaces the existing row). Keeps the list
    /// sorted by instance name for stable UI rendering.
    private func applyRtlTcpBrowserEvent(_ event: SdrRtlTcpBrowser.Event) {
        switch event {
        case .announced(let server):
            if let idx = rtlTcpDiscoveredServers.firstIndex(where: {
                $0.instanceName == server.instanceName
            }) {
                rtlTcpDiscoveredServers[idx] = server
            } else {
                rtlTcpDiscoveredServers.append(server)
                rtlTcpDiscoveredServers.sort { $0.instanceName < $1.instanceName }
            }
            // Refresh cached metadata on any favorite that
            // matches this announce — so a reopened sidebar
            // shows current tuner / gain-count / last-seen
            // even when offline-starred earlier.
            let key = "\(server.hostname):\(server.port)"
            if let idx = rtlTcpFavorites.firstIndex(where: { $0.key == key }) {
                var fav = rtlTcpFavorites[idx]
                let newNickname = server.nickname.isEmpty
                    ? server.instanceName
                    : server.nickname
                if !newNickname.isEmpty {
                    fav.nickname = newNickname
                }
                if !server.tuner.isEmpty {
                    fav.tunerName = server.tuner
                }
                if server.gains != 0 {
                    fav.gainCount = server.gains
                }
                fav.lastSeenUnix = UInt64(Date().timeIntervalSince1970)
                rtlTcpFavorites[idx] = fav
                persistRtlTcpFavorites()
            }
        case .withdrawn(let instanceName):
            rtlTcpDiscoveredServers.removeAll { $0.instanceName == instanceName }
        }
    }

    /// Add (or refresh) a favorite for the given key. If an
    /// entry with the same key already exists, its metadata is
    /// updated in place rather than duplicated.
    func addRtlTcpFavorite(_ favorite: RtlTcpClientFavorite) {
        if let idx = rtlTcpFavorites.firstIndex(where: { $0.key == favorite.key }) {
            rtlTcpFavorites[idx] = favorite
        } else {
            rtlTcpFavorites.append(favorite)
        }
        persistRtlTcpFavorites()
    }

    /// Remove a favorite by key. No-op if the key isn't found.
    func removeRtlTcpFavorite(key: String) {
        rtlTcpFavorites.removeAll { $0.key == key }
        persistRtlTcpFavorites()
    }

    /// Persist the favorites array to `UserDefaults` as a JSON
    /// string. Called from `add/removeRtlTcpFavorite` on any
    /// mutation, and from the browser event handler when it
    /// refreshes cached metadata for a starred entry.
    func persistRtlTcpFavorites() {
        if let data = try? JSONEncoder().encode(rtlTcpFavorites),
           let json = String(data: data, encoding: .utf8) {
            UserDefaults.standard.set(json, forKey: Self.rtlTcpClientFavoritesKey)
        }
    }

    /// Persist the last-connected snapshot to `UserDefaults`.
    /// Called on a successful client connect (commit 2).
    func persistRtlTcpLastConnected() {
        if let last = rtlTcpLastConnected,
           let data = try? JSONEncoder().encode(last),
           let json = String(data: data, encoding: .utf8) {
            UserDefaults.standard.set(json, forKey: Self.rtlTcpClientLastConnectedKey)
        } else {
            UserDefaults.standard.removeObject(forKey: Self.rtlTcpClientLastConnectedKey)
        }
    }

    /// Connect the engine's rtl_tcp client to `host:port`.
    /// Mirrors `applyNetworkSourceConfig` + `setSourceType(.rtlTcp)`
    /// — the engine uses the same stored host/port for both
    /// `.network` and `.rtlTcp` sources, so this path writes
    /// to that shared storage and then flips the source type.
    /// On a successful dispatch, `rtlTcpLastConnected` is
    /// updated and persisted so the next launch can repopulate
    /// the manual-entry fields. Connection *progress* (actual
    /// socket connect, handshake, ready-to-stream) arrives
    /// asynchronously via `rtlTcpConnectionState` events from
    /// the engine. Per issue #326.
    func connectToRtlTcp(host: String, port: UInt16, nickname: String) {
        let trimmed = host.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            lastError = "rtl_tcp server host cannot be empty"
            return
        }
        guard port != 0 else {
            lastError = "rtl_tcp server port must be in 1…65535"
            return
        }
        // Clear any stale error from a prior action so we can
        // detect whether the FFI calls below introduce a new
        // one (either validation inside `applyNetworkSourceConfig`
        // or a `capture {}` failure on the setter).
        lastError = nil
        // Write through the shared network-source config path.
        // `applyNetworkSourceConfig` persists host/port/protocol
        // in UserDefaults and dispatches `set_network_config` —
        // both the `.network` IQ source and `.rtlTcp` source
        // read from the same slot, matching the engine side.
        applyNetworkSourceConfig(host: trimmed, port: port, protocol: .tcp)
        if lastError != nil { return }
        // Remember this endpoint. The nickname falls back to
        // `host:port` when the caller didn't have a better label
        // (manual-entry without a matching mDNS announce).
        let displayNickname = nickname.isEmpty ? "\(trimmed):\(port)" : nickname
        rtlTcpLastConnected = RtlTcpClientLastConnected(
            host: trimmed,
            port: port,
            nickname: displayNickname
        )
        persistRtlTcpLastConnected()
        // Flip to `.rtlTcp` — the engine tears down whatever is
        // currently open and builds the rtl_tcp client from the
        // just-written network config.
        setSourceType(.rtlTcp)
    }

    /// Disconnect the rtl_tcp client without changing source
    /// type. The engine tears down the current TCP socket and
    /// the connection-state machine transitions to
    /// `.disconnected` (which arrives asynchronously on the
    /// event stream). Source type stays `.rtlTcp` so a
    /// subsequent `retryRtlTcpNow()` reopens against the same
    /// host/port from stored config. Per issue #326.
    func disconnectRtlTcp() {
        capture { try core?.disconnectRtlTcp() }
    }

    /// Retry the rtl_tcp connection immediately, bypassing the
    /// exponential-backoff sleep the reconnect loop is in after
    /// a transport failure. Useful wired to a "Retry now"
    /// button that shouldn't make the user wait out the
    /// countdown. Per issue #326.
    func retryRtlTcpNow() {
        capture { try core?.retryRtlTcpNow() }
    }

    // ----------------------------------------------------------
    //  rtl_tcp-specific command setters (ABI 0.11/0.12)
    //
    //  These apply to ANY active source per the engine's
    //  silent-accept contract — but only the .rtlTcp arm of
    //  SourceSection surfaces the UI. Optimistic pattern:
    //  flip the observable field first for input responsiveness,
    //  then dispatch through the FFI. The engine doesn't publish
    //  state events for these, so the model field is the
    //  source of truth once it's flipped.
    // ----------------------------------------------------------

    func setBiasTee(_ on: Bool) {
        biasTeeEnabled = on
        UserDefaults.standard.set(on, forKey: Self.rtlTcpBiasTeeKey)
        capture { try core?.setBiasTee(on) }
    }

    func setOffsetTuning(_ on: Bool) {
        offsetTuningEnabled = on
        UserDefaults.standard.set(on, forKey: Self.rtlTcpOffsetTuningKey)
        capture { try core?.setOffsetTuning(on) }
    }

    func setDirectSampling(_ mode: SdrCore.DirectSamplingMode) {
        directSamplingMode = mode
        UserDefaults.standard.set(Int(mode.rawValue), forKey: Self.rtlTcpDirectSamplingKey)
        capture { try core?.setDirectSampling(mode) }
    }

    func setRtlAgc(_ on: Bool) {
        rtlAgcEnabled = on
        UserDefaults.standard.set(on, forKey: Self.rtlTcpRtlAgcKey)
        capture { try core?.setRtlAgc(on) }
    }

    /// Dispatch rtl_tcp `set_gain_by_index` (protocol cmd 0x0d).
    /// The index is bounds-checked by the engine against the
    /// `Connected` state's `gain_count` field — out-of-range
    /// surfaces as an `.error(...)` event rather than silently
    /// failing on the wire. Not persisted across sessions (see
    /// the `rtlTcpGainIndex` field docs).
    func setRtlTcpGainIndex(_ index: UInt32) {
        rtlTcpGainIndex = index
        capture { try core?.setGainByIndex(index) }
    }

    // ----------------------------------------------------------
    //  UserDefaults keys for rtl_tcp-specific command toggles
    // ----------------------------------------------------------
    static let rtlTcpBiasTeeKey = "SDRMac.rtlTcpClient.biasTee"
    static let rtlTcpOffsetTuningKey = "SDRMac.rtlTcpClient.offsetTuning"
    static let rtlTcpDirectSamplingKey = "SDRMac.rtlTcpClient.directSampling"
    static let rtlTcpRtlAgcKey = "SDRMac.rtlTcpClient.rtlAgc"

    /// Render an `RtlTcpConnectionState` to a one-line status
    /// string suitable for a picker subtitle row. Matches the
    /// Linux `format_rtl_tcp_state()` wording (Connected /
    /// Retrying / Failed / …). Static so views can render it
    /// without reaching into the model. Per issue #326.
    static func formatRtlTcpConnectionState(_ state: RtlTcpConnectionState) -> String {
        switch state {
        case .disconnected:
            return "Not connected"
        case .connecting:
            return "Connecting…"
        case .connected(let tunerName, let gainCount):
            return "Connected — \(tunerName) (\(gainCount) gains)"
        case .retrying(let attempt, let retryInSecs):
            // Ceil, not floor — `retryInSecs` of 1.9 would read
            // as 1 s and understate the wait. Clamp to ≥1 so
            // sub-1 s retries still display something rather
            // than "0 s" (which reads like the retry already
            // fired). Matches Linux treatment.
            let secs = max(Int(retryInSecs.rounded(.up)), 1)
            return "Retrying in \(secs) s (attempt \(attempt))"
        case .failed(let reason):
            return "Failed — \(reason)"
        }
    }

    /// Restore favorites + last-connected from `UserDefaults`.
    /// Called from `bootstrap()`. Absent or corrupt entries
    /// degrade to empty / nil silently — a bad JSON blob should
    /// never block the app from launching.
    private func restoreRtlTcpClientState() {
        if let json = UserDefaults.standard.string(forKey: Self.rtlTcpClientFavoritesKey),
           let data = json.data(using: .utf8),
           let decoded = try? JSONDecoder().decode([RtlTcpClientFavorite].self, from: data) {
            rtlTcpFavorites = decoded
        }
        if let json = UserDefaults.standard.string(forKey: Self.rtlTcpClientLastConnectedKey),
           let data = json.data(using: .utf8),
           let decoded = try? JSONDecoder().decode(RtlTcpClientLastConnected.self, from: data) {
            rtlTcpLastConnected = decoded
        }
    }

    // ----------------------------------------------------------
    //  Source selection setters — issues #235, #236
    // ----------------------------------------------------------

    /// Switch the active IQ source. Optimistic — flips the UI
    /// field before dispatching so dependent sections redraw
    /// immediately. A source-open error lands later as an
    /// `.error(...)` / `.sourceStopped` event.
    ///
    /// Guards against `.file` with an empty `filePath` — that
    /// would tear down the current source and immediately fail
    /// at open time. Callers (including `syncToEngine` on a
    /// fresh install where the user hasn't picked a WAV yet)
    /// should either set the path first or stay on the previous
    /// source. Per `CodeRabbit` round 1 on PR #358.
    func setSourceType(_ type: SourceType) {
        if type == .file && filePath.isEmpty {
            lastError = "Choose a WAV file before switching to File playback"
            return
        }
        // Second half of the rtl_tcp server / engine mutex.
        // The server holds the USB device exclusively while
        // running; flipping the engine source to `.rtlSdr`
        // would then fight it on open. `SourceSection`
        // disables the picker option cosmetically, but this
        // guard covers non-UI callers (bookmarks, menu
        // shortcuts, programmatic `syncToEngine` replay).
        // Per `CodeRabbit` round 1 on PR #362.
        if type == .rtlSdr && rtlTcpServerHoldsDongle {
            lastError =
                "Local dongle is shared over the network. Stop the rtl_tcp " +
                "server before selecting RTL-SDR as the source."
            return
        }
        sourceType = type
        UserDefaults.standard.set(Int(type.rawValue), forKey: Self.sourceTypeDefaultsKey)
        capture { try core?.setSourceType(type) }
    }

    // ----------------------------------------------------------
    //  Band presets — persisted last selection
    //
    //  Owned by the model rather than by `BandPresetsSection`'s
    //  local `@State` so the picker's chosen value survives
    //  panel close + activity swap (the General activity panel
    //  is rebuilt every time the user reopens it). Persisted
    //  to UserDefaults too, so the dropdown reflects the user's
    //  last pick across launches. Per `CodeRabbit` round 1 on
    //  PR #493.
    // ----------------------------------------------------------

    /// ID of the last band preset the user picked from the
    /// General panel's dropdown. Defaults to `"FM Broadcast"`
    /// on a fresh install — the model's default tuner state
    /// (100 MHz / WFM) lives in the FM band so this is the
    /// most natural pick for first launch. The `BandPreset.id`
    /// is the preset's name string (`"FM Broadcast"`, `"NOAA
    /// Weather"`, …) — see `apps/macos/SDRMac/Models/BandPreset.swift`.
    var lastSelectedBandPresetID: String? = "FM Broadcast"

    /// Resolved view of `lastSelectedBandPresetID`. Returns
    /// `nil` when no preset has been picked yet, or when the
    /// persisted ID no longer matches any entry in the
    /// canonical `bandPresets` slice (slice rename or removal).
    var lastSelectedBandPreset: BandPreset? {
        guard let id = lastSelectedBandPresetID else { return nil }
        return bandPresets.first { $0.id == id }
    }

    /// Apply a preset by routing through the standard tuner
    /// setters (so squelch / auto-squelch / VFO echoes behave
    /// identically to a manual tune), then remember the
    /// selection so the picker reflects it next time the
    /// General panel opens. Passing `nil` clears the remembered
    /// pick without retuning. Per `CodeRabbit` round 1 on PR
    /// #493 (split selection from tune action so the picker can
    /// also be cleared programmatically).
    func setLastSelectedBandPreset(_ preset: BandPreset?) {
        lastSelectedBandPresetID = preset?.id
        if let id = preset?.id {
            UserDefaults.standard.set(id, forKey: Self.lastSelectedBandPresetDefaultsKey)
        } else {
            UserDefaults.standard.removeObject(forKey: Self.lastSelectedBandPresetDefaultsKey)
        }
        if let preset {
            setCenter(preset.centerFrequencyHz)
            setDemodMode(preset.demodMode)
            setBandwidth(preset.bandwidthHz)
        }
    }

    /// UserDefaults key for the persisted last-selected preset
    /// ID. The Mac app persists locally; the Rust config layer
    /// doesn't round-trip this value (no Linux equivalent —
    /// the GTK panel uses an in-memory `ComboRow` and resets on
    /// app restart).
    static let lastSelectedBandPresetDefaultsKey = "SDRMac.lastSelectedBandPreset"

    // ----------------------------------------------------------
    //  Sidebar session — issue #449 (ABI 0.21)
    //
    //  Six fields, three per side, persisted to the shared
    //  `sdr-config` JSON file via the engine's config FFI. Keys
    //  match the Linux side exactly so a user who runs both
    //  frontends sees consistent state. Values for the
    //  `*_selected` fields are the activity raw-value strings
    //  defined on `LeftActivity` / `RightActivity` (which match
    //  the Linux activity-bar entry names — same source of
    //  truth).
    //
    //  Default values match the Linux `SidebarSession::default`:
    //  left = General open at 320 px, right = Transcript closed
    //  at 320 px. The 320 px width matches the GTK
    //  `DEFAULT_SIDEBAR_WIDTH_PX` constant.
    //
    //  These fields are observable @Observable storage so
    //  ContentView can bind through them (and the activity bar /
    //  resize gesture writes flow back through the matching
    //  setters). The setters double-write: update the
    //  observable property AND push to the engine config so the
    //  on-disk JSON stays in sync.
    // ----------------------------------------------------------

    /// Activity selected in the left sidebar — one of the
    /// `LeftActivity` raw values. Loaded from
    /// `ui_sidebar_left_selected` on bootstrap; written through
    /// `setSidebarLeftSelected(_:)` on user picks.
    var sidebarLeftSelected: String = "general"
    /// Whether the left panel is currently visible.
    var sidebarLeftOpen: Bool = true
    /// Width of the left panel in pixels. Default matches the
    /// Linux `DEFAULT_SIDEBAR_WIDTH_PX` constant; the per-side
    /// clamp range below is the spec's floor/ceiling pair.
    var sidebarLeftWidth: UInt32 = sidebarLeftDefaultWidth

    /// Activity selected in the right sidebar — one of the
    /// `RightActivity` raw values.
    var sidebarRightSelected: String = "transcript"
    /// Whether the right panel is currently visible.
    var sidebarRightOpen: Bool = false
    /// Width of the right panel in pixels. Default is wider than
    /// the left because the right side hosts content-heavy
    /// panels (Transcript / Bookmarks) that read better with
    /// extra room.
    var sidebarRightWidth: UInt32 = sidebarRightDefaultWidth

    /// Config keys — match the Linux constants in
    /// `crates/sdr-ui/src/sidebar/activity_bar.rs` exactly so a
    /// user who runs both frontends sees consistent state.
    static let sidebarLeftSelectedKey = "ui_sidebar_left_selected"
    static let sidebarLeftOpenKey = "ui_sidebar_left_open"
    static let sidebarLeftWidthKey = "ui_sidebar_left_width_px"
    static let sidebarRightSelectedKey = "ui_sidebar_right_selected"
    static let sidebarRightOpenKey = "ui_sidebar_right_open"
    static let sidebarRightWidthKey = "ui_sidebar_right_width_px"

    /// Per-side clamp ranges + defaults, matching the spec for
    /// #450. Different floors and ceilings on each side reflect
    /// what the panels need to render usefully — left holds a
    /// `Form` with grouped sections (220 px is the minimum that
    /// keeps the labels readable), right holds Transcript /
    /// Bookmarks list views (360 px is the minimum that keeps
    /// timestamps + content side-by-side without truncation).
    /// Upper ceilings prevent a single panel from monopolising
    /// the window.
    ///
    /// Ranges live as `Int` rather than `UInt32` because both
    /// AppKit (`NSSplitView` constraints) and SwiftUI's
    /// `.frame(minWidth:maxWidth:)` modifier take `CGFloat`,
    /// and the conversion path is simpler from `Int`. The
    /// model still stores width as `UInt32` because the
    /// shared `sdr-config` file uses unsigned ints there.
    static let sidebarLeftWidthRange: ClosedRange<Int> = 220...640
    static let sidebarRightWidthRange: ClosedRange<Int> = 360...840
    static let sidebarLeftDefaultWidth: UInt32 = 320
    static let sidebarRightDefaultWidth: UInt32 = 420

    /// Restore all six sidebar fields from the shared config.
    /// Called once during `bootstrap()` AFTER the engine is
    /// alive (the engine handle owns the `ConfigManager` we
    /// read through). Out-of-range / malformed entries fall
    /// through to the default value already on the property —
    /// same defensive policy as the network-sink restore above.
    private func loadSidebarSession() {
        guard let core else { return }
        if let s = core.configString(key: Self.sidebarLeftSelectedKey),
           LeftActivity(rawValue: s) != nil {
            sidebarLeftSelected = s
        }
        if let b = core.configBool(key: Self.sidebarLeftOpenKey) {
            sidebarLeftOpen = b
        }
        if let w = core.configUInt32(key: Self.sidebarLeftWidthKey),
           Self.sidebarLeftWidthRange.contains(Int(w)) {
            sidebarLeftWidth = w
        }
        if let s = core.configString(key: Self.sidebarRightSelectedKey),
           RightActivity(rawValue: s) != nil {
            sidebarRightSelected = s
        }
        if let b = core.configBool(key: Self.sidebarRightOpenKey) {
            sidebarRightOpen = b
        }
        if let w = core.configUInt32(key: Self.sidebarRightWidthKey),
           Self.sidebarRightWidthRange.contains(Int(w)) {
            sidebarRightWidth = w
        }
    }

    /// Update the left sidebar's selected activity. Validates
    /// against `LeftActivity` raw values before writing — a
    /// non-UI caller passing a bogus string can't poison the
    /// shared config (Linux side would silently ignore it on
    /// next load anyway, but the round-trip would be sticky
    /// across sessions).
    func setSidebarLeftSelected(_ name: String) {
        guard LeftActivity(rawValue: name) != nil else { return }
        sidebarLeftSelected = name
        capture {
            try core?.setConfigString(key: Self.sidebarLeftSelectedKey, value: name)
        }
    }

    func setSidebarLeftOpen(_ open: Bool) {
        sidebarLeftOpen = open
        capture { try core?.setConfigBool(key: Self.sidebarLeftOpenKey, value: open) }
    }

    func setSidebarLeftWidth(_ width: UInt32) {
        let lo = UInt32(Self.sidebarLeftWidthRange.lowerBound)
        let hi = UInt32(Self.sidebarLeftWidthRange.upperBound)
        let clamped = min(max(width, lo), hi)
        sidebarLeftWidth = clamped
        capture {
            try core?.setConfigUInt32(key: Self.sidebarLeftWidthKey, value: clamped)
        }
    }

    func setSidebarRightSelected(_ name: String) {
        guard RightActivity(rawValue: name) != nil else { return }
        sidebarRightSelected = name
        capture {
            try core?.setConfigString(key: Self.sidebarRightSelectedKey, value: name)
        }
    }

    func setSidebarRightOpen(_ open: Bool) {
        sidebarRightOpen = open
        capture { try core?.setConfigBool(key: Self.sidebarRightOpenKey, value: open) }
    }

    func setSidebarRightWidth(_ width: UInt32) {
        let lo = UInt32(Self.sidebarRightWidthRange.lowerBound)
        let hi = UInt32(Self.sidebarRightWidthRange.upperBound)
        let clamped = min(max(width, lo), hi)
        sidebarRightWidth = clamped
        capture {
            try core?.setConfigUInt32(key: Self.sidebarRightWidthKey, value: clamped)
        }
    }

    /// Apply the current network-source host/port/protocol to
    /// the engine. Called on explicit Apply in the Source pane
    /// rather than per-keystroke — the engine rebuilds the
    /// connection on receipt, so typing into the host field
    /// shouldn't thrash it.
    func applyNetworkSourceConfig(
        host: String,
        port: UInt16,
        protocol proto: NetworkSourceProtocol
    ) {
        let trimmed = host.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            lastError = "Network source host cannot be empty"
            return
        }
        // Mirror the FFI boundary check (`sdr_core_set_network_config`
        // rejects port 0) at the model so non-UI callers — e.g.
        // `syncToEngine`, future programmatic paths — can't
        // persist a bogus endpoint. Per `CodeRabbit` round 1 on
        // PR #358.
        guard port != 0 else {
            lastError = "Network source port must be in 1…65535"
            return
        }
        networkSourceHost = trimmed
        networkSourcePort = port
        networkSourceProtocol = proto
        UserDefaults.standard.set(trimmed, forKey: Self.networkSourceHostDefaultsKey)
        UserDefaults.standard.set(Int(port), forKey: Self.networkSourcePortDefaultsKey)
        UserDefaults.standard.set(Int(proto.rawValue), forKey: Self.networkSourceProtocolDefaultsKey)
        capture {
            try core?.setNetworkConfig(hostname: trimmed, port: port, protocol: proto)
        }
    }

    /// Set the file-playback source path. Empty input is
    /// rejected locally to match the FFI contract — opening
    /// with an empty path is an InvalidArg anyway, but failing
    /// here keeps the UI responsive.
    ///
    /// The path itself is **not** trimmed or otherwise
    /// normalized. macOS filesystem paths can legally begin or
    /// end with whitespace (or any non-NUL byte, really), and
    /// silently rewriting the string would break playback for
    /// a valid-but-unusual filename that `fileImporter`
    /// returned. Validation uses a trimmed copy only; the
    /// stored / persisted / dispatched value is the exact
    /// caller-supplied string. Per `CodeRabbit` round 1 on
    /// PR #358.
    func setFilePath(_ path: String) {
        guard !path.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            lastError = "File path cannot be empty"
            return
        }
        filePath = path
        UserDefaults.standard.set(path, forKey: Self.filePathDefaultsKey)
        capture { try core?.setFilePath(path) }
    }

    /// UserDefaults keys for the persisted source-selection
    /// state. Matches the `audioDeviceDefaultsKey` / network-sink
    /// pattern. The Rust config layer doesn't round-trip these
    /// yet — tracked against the larger v3 config alignment.
    static let sourceTypeDefaultsKey            = "SDRMac.sourceType"
    static let networkSourceHostDefaultsKey     = "SDRMac.networkSourceHost"
    static let networkSourcePortDefaultsKey     = "SDRMac.networkSourcePort"
    static let networkSourceProtocolDefaultsKey = "SDRMac.networkSourceProtocol"
    static let filePathDefaultsKey              = "SDRMac.filePath"

    /// UserDefaults keys for the persisted network-sink config.
    /// Matches the existing `audioDeviceDefaultsKey` pattern —
    /// the Rust config layer doesn't round-trip these values yet
    /// (tracked against the larger v3 config alignment), so the
    /// Mac app persists locally in the meantime.
    static let audioSinkTypeDefaultsKey      = "SDRMac.audioSinkType"
    static let networkSinkHostDefaultsKey    = "SDRMac.networkSinkHost"
    static let networkSinkPortDefaultsKey    = "SDRMac.networkSinkPort"
    static let networkSinkProtocolDefaultsKey = "SDRMac.networkSinkProtocol"

    /// Re-query the backend for the current output device list.
    /// Called by the AudioSection view on appear; safe to call
    /// any time. Handle-free (static on SdrCore).
    func refreshAudioDevices() {
        audioDevices = SdrCore.audioDevices
    }

    /// Start writing the demodulated audio stream to `path` (a
    /// full filesystem path). The engine confirms via
    /// `.audioRecordingStarted` which flips
    /// `audioRecordingPath` to non-nil. Failure comes back as
    /// an `.error(...)` event and `audioRecordingPath` stays nil.
    func startAudioRecording(to path: String) {
        capture { try core?.startAudioRecording(path: path) }
    }

    /// Stop recording. Safe to call at any time — the engine
    /// always emits `.audioRecordingStopped` in response, which
    /// clears `audioRecordingPath`.
    func stopAudioRecording() {
        capture { try core?.stopAudioRecording() }
    }

    /// Start writing the raw IQ stream to `path`. The engine
    /// confirms via `.iqRecordingStarted` which flips
    /// `iqRecordingPath` to non-nil. Failure comes back as an
    /// `.error(...)` event and `iqRecordingPath` stays nil.
    func startIqRecording(to path: String) {
        capture { try core?.startIqRecording(path: path) }
    }

    /// Stop IQ recording. Safe to call at any time — the engine
    /// always emits `.iqRecordingStopped` in response, which
    /// clears `iqRecordingPath`.
    func stopIqRecording() {
        capture { try core?.stopIqRecording() }
    }

    /// Re-read the keyring to sync the observable
    /// `radioReferenceHasCredentials` flag. Callers that mutate
    /// credentials (save/delete in Settings) must invoke this so
    /// views depending on the flag redraw.
    func refreshRadioReferenceCredentialsFlag() {
        radioReferenceHasCredentials = SdrCore.hasRadioReferenceCredentials
    }

    func setFftSize(_ n: Int) {
        fftSize = n
        capture { try core?.setFftSize(n) }
    }

    func setFftWindow(_ w: FftWindow) {
        fftWindow = w
        capture { try core?.setFftWindow(w) }
    }

    func setFftRate(_ fps: Double) {
        fftRateFps = fps
        capture { try core?.setFftRate(fps) }
    }

    func setPpm(_ ppm: Int) {
        ppmCorrection = ppm
        capture { try core?.setPpmCorrection(Int32(ppm)) }
    }

    /// Pure UI — no engine call. The min/max dB sliders only
    /// affect local rendering contrast.
    func setMinDb(_ db: Float) { minDb = db }
    func setMaxDb(_ db: Float) { maxDb = db }

    /// Dismiss the current error banner. Called from the status
    /// bar's "X" button and reset on the next successful start.
    func clearError() {
        lastError = nil
    }

    /// Explicit `deinit` safety net — closes issue #293.
    ///
    /// The `@MainActor` isolation on this class applies to
    /// `deinit` too (Swift 6 / SE-0371 isolated-deinit
    /// semantics), so we can read `@ObservationTracked`-
    /// generated storage here without fighting the macro.
    /// That wasn't allowed when the model was first written —
    /// see the original PR #292 round 2 thread linked from
    /// #293 — but the Swift 6.3 / Xcode 26 toolchain the app
    /// now builds against has caught up.
    ///
    /// Cancels the event-consumer Task so a model drop
    /// without an explicit `shutdown()` (test scopes, a
    /// hypothetical multi-window future) releases the
    /// background `for await` loop deterministically rather
    /// than waiting for `SdrCore.deinit` → `sdr_core_destroy`
    /// → channel-close → AsyncStream-end to unwind it.
    /// The normal shutdown path still goes through
    /// `shutdown()` and this deinit runs after — the
    /// second-cancel is a no-op on an already-cancelled task.
    @MainActor
    deinit {
        eventTask?.cancel()
    }

    // ==========================================================
    //  Internal helpers
    // ==========================================================

    private func capture(_ work: () throws -> Void) {
        do {
            try work()
        } catch {
            // Preserve both the concrete error type and its
            // localized description so diagnostics aren't
            // reduced to a bare `Optional(...)` or a raw
            // `Debug`-style string. `type(of:)` captures the
            // Swift type (e.g., `SdrCoreError`) and lets the
            // user / status bar distinguish between command
            // rejections, FFI panics, etc.
            lastError = "\(type(of: error)) — \(error.localizedDescription)"
        }
    }
}
