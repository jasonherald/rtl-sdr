//
// SdrCore.swift — main Swift wrapper around the sdr-ffi C ABI.
//
// `SdrCore` owns an opaque C handle created via `sdr_core_create`,
// exposes typed Swift methods for every command, hands back an
// `AsyncStream<SdrCoreEvent>` for host consumption, and provides
// a closure-style FFT-pull API for render-loop integration.
//
// Lifecycle:
//
//   1. `init(configPath:)` calls `sdr_core_create`. On success,
//      the C-side dispatcher thread is already running and the
//      Swift side immediately registers a C trampoline callback
//      that yields into an `AsyncStream.Continuation`.
//   2. Commands go through the typed wrapper methods, each of
//      which calls a `sdr_core_*` function and throws on
//      non-zero return codes.
//   3. Events flow via `events` (an `AsyncStream<SdrCoreEvent>`)
//      — hosts `for await`-iterate it, typically on the main
//      actor.
//   4. FFT frames flow via `withLatestFftFrame { ... }` from
//      the render tick.
//   5. Teardown happens in `deinit`, which unregisters the
//      callback, finishes the event stream, and calls
//      `sdr_core_destroy`.
//
// The class is `@unchecked Sendable`: the underlying C handle is
// a `Send + Sync` Rust type (via the FFI contract) and we don't
// mutate any Swift-side state after init except through the
// async event stream, which has its own synchronization. We
// declare this explicitly rather than wrapping in an actor so
// the FFT pull path (hot, synchronous) doesn't cross an actor
// hop on every render tick.

import Foundation
@preconcurrency import sdr_core_c

/// Swift-side handle for the sdr-core SDR engine.
public final class SdrCore: @unchecked Sendable {
    /// Opaque C pointer obtained from `sdr_core_create`. `internal`
    /// rather than `private` so sibling files in SdrCoreKit (e.g.
    /// `SdrCoreAudioTap`) can pass it to the FFI without forcing
    /// those wrappers back into this file.
    internal let handle: OpaquePointer

    /// Async stream of engine events. Consumers `for await`-loop
    /// this to receive the same events the C-side dispatcher
    /// thread hands to the registered callback.
    public let events: AsyncStream<SdrCoreEvent>
    private let eventContinuation: AsyncStream<SdrCoreEvent>.Continuation

    /// Retained box passed as `user_data` to the C event
    /// callback. Lives for the lifetime of this SdrCore so the
    /// callback trampoline can `takeUnretainedValue` it safely
    /// on every call.
    private let callbackBox: CallbackBox

    /// Build and start a new engine.
    ///
    /// `configPath` is the on-disk config file the engine should
    /// eventually load from and persist to. Pass `nil` to run
    /// with in-memory defaults and no persistence. Must be a
    /// file URL if non-nil. v1 engines accept the path and
    /// store it but do not yet read or write through it —
    /// passing a valid path now means persistence can land in
    /// a follow-up without an API change.
    ///
    /// - Throws: `SdrCoreError` with a non-zero code if the
    ///   underlying FFI fails (spawn failure, invalid path,
    ///   etc.).
    public init(configPath: URL?) throws {
        // Resolve the path to a C string. An empty/nil URL
        // maps to an empty C string, which the FFI treats as
        // "no persistence".
        if let url = configPath, !url.isFileURL {
            throw SdrCoreError(
                code: .invalidArg,
                message: "configPath must be a file URL, got \(url.scheme ?? "unknown") scheme"
            )
        }
        let pathString: String = configPath?.path ?? ""

        // Create the handle.
        var rawHandle: OpaquePointer? = nil
        let rc = pathString.withCString { cPath in
            sdr_core_create(cPath, &rawHandle)
        }
        try checkRc(rc)
        guard let h = rawHandle else {
            throw SdrCoreError(
                code: .internal,
                message: "sdr_core_create returned OK but wrote null handle"
            )
        }
        self.handle = h

        // Set up the event AsyncStream.
        //
        // **Bounded buffering**: we use `.bufferingNewest(1)`
        // instead of the default unbounded policy. Without a
        // bound, a stalled consumer would accumulate events
        // forever (SignalLevel alone runs at ~50 Hz).
        //
        // Trade-off: the 1-slot buffer can drop rare one-shot
        // events (DeviceInfo, GainList, Error) when a burst of
        // SignalLevel updates fills the slot between consumer
        // reads. For v1 this is acceptable — the consumer is
        // expected to iterate promptly. A v2 improvement is to
        // split into a high-frequency telemetry stream
        // (SignalLevel, bufferingNewest(1)) and a separate
        // control stream (everything else, larger bounded
        // buffer) so one-shot events are never starved.
        //
        // The continuation is captured by the C trampoline via
        // the retained `CallbackBox`.
        var continuation: AsyncStream<SdrCoreEvent>.Continuation! = nil
        self.events = AsyncStream<SdrCoreEvent>(
            bufferingPolicy: .bufferingNewest(1)
        ) { continuation = $0 }
        self.eventContinuation = continuation

        // Retain the box so the C side can safely dereference
        // the opaque `user_data` pointer on every callback.
        let box = CallbackBox(continuation: continuation)
        self.callbackBox = box

        // Register the trampoline. The box is passed as
        // `user_data`; inside the callback we recover it with
        // `Unmanaged.fromOpaque(...).takeUnretainedValue()`
        // (not `takeRetainedValue` — we don't want the
        // callback to decrement the retain count).
        //
        // **Cleanup on failure**: if `sdr_core_set_event_callback`
        // fails (or if checkRc throws on its return code), the
        // initializer exits without running `deinit`, which
        // would leak the native handle created above. We
        // explicitly destroy the handle on the error path
        // before rethrowing so the FFI-side resources are
        // reclaimed. CodeRabbit caught this on PR #256 round 1.
        let boxPtr = Unmanaged.passUnretained(box).toOpaque()
        let registerRc = sdr_core_set_event_callback(
            handle,
            SdrCore.eventTrampoline,
            boxPtr
        )
        if registerRc != 0 {
            // Capture the error BEFORE destroy — sdr_core_destroy
            // can overwrite the thread-local last-error message.
            let error = SdrCoreError.fromCurrentError(rawCode: registerRc)
            continuation.finish()
            sdr_core_destroy(handle)
            throw error
        }
    }

    deinit {
        // Unregister the callback first so the C side stops
        // firing into a CallbackBox that's about to be freed.
        _ = sdr_core_set_event_callback(handle, nil, nil)

        // Finish the async stream — any consumer currently
        // awaiting the next event gets a completion signal.
        eventContinuation.finish()

        // Destroy the handle. This joins the dispatcher thread
        // and detaches the DSP thread, in that order (see the
        // sdr_core_destroy implementation for the teardown
        // sequencing).
        sdr_core_destroy(handle)
    }

    // ==========================================================
    //  Static trampoline bridging the C callback into Swift.
    // ==========================================================

    /// Internal box the C trampoline unwraps on every call.
    /// Holding the continuation here (instead of on `SdrCore`
    /// directly) means the trampoline can operate without
    /// needing to know about the enclosing class.
    private final class CallbackBox {
        let continuation: AsyncStream<SdrCoreEvent>.Continuation
        init(continuation: AsyncStream<SdrCoreEvent>.Continuation) {
            self.continuation = continuation
        }
        deinit {}
    }

    /// C callback trampoline. Fires from the FFI dispatcher
    /// thread. Translates the borrowed `SdrEvent*` into an
    /// owned `SdrCoreEvent` and yields it into the async
    /// stream.
    private static let eventTrampoline: @convention(c) (
        UnsafePointer<SdrEvent>?,
        UnsafeMutableRawPointer?
    ) -> Void = { eventPtr, userData in
        guard let eventPtr, let userData else { return }

        // Recover the CallbackBox. takeUnretainedValue because
        // `init` used passUnretained — the box is owned by the
        // enclosing SdrCore, not by this callback invocation.
        let box = Unmanaged<CallbackBox>.fromOpaque(userData).takeUnretainedValue()

        guard let swiftEvent = SdrCoreEvent.fromC(eventPtr) else {
            return
        }

        // yield is non-blocking and thread-safe — exactly
        // what we want from a dispatcher-thread callback.
        box.continuation.yield(swiftEvent)
    }

    // ==========================================================
    //  Commands
    // ==========================================================

    /// Start the active source.
    public func start() throws {
        try checkRc(sdr_core_start(handle))
    }

    /// Stop the active source.
    public func stop() throws {
        try checkRc(sdr_core_stop(handle))
    }

    /// Tune to a center frequency in Hz.
    public func tune(_ freqHz: Double) throws {
        try checkRc(sdr_core_tune(handle, freqHz))
    }

    /// Set the VFO offset from the tuner center in Hz.
    public func setVfoOffset(_ offsetHz: Double) throws {
        try checkRc(sdr_core_set_vfo_offset(handle, offsetHz))
    }

    /// Set the tuner sample rate in Hz.
    public func setSampleRate(_ rateHz: Double) throws {
        try checkRc(sdr_core_set_sample_rate(handle, rateHz))
    }

    /// Set the decimation factor (power of 2, 1 = none).
    public func setDecimation(_ factor: UInt32) throws {
        try checkRc(sdr_core_set_decimation(handle, factor))
    }

    /// Set PPM correction for the tuner crystal offset.
    public func setPpmCorrection(_ ppm: Int32) throws {
        try checkRc(sdr_core_set_ppm_correction(handle, ppm))
    }

    /// Set the tuner gain in dB.
    public func setGain(_ gainDb: Double) throws {
        try checkRc(sdr_core_set_gain(handle, gainDb))
    }

    /// Enable or disable tuner AGC.
    public func setAgc(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_agc(handle, enabled))
    }

    /// Set the active demodulation mode.
    public func setDemodMode(_ mode: DemodMode) throws {
        try checkRc(sdr_core_set_demod_mode(handle, mode.rawValue))
    }

    /// Set the channel bandwidth in Hz.
    public func setBandwidth(_ bwHz: Double) throws {
        try checkRc(sdr_core_set_bandwidth(handle, bwHz))
    }

    /// Enable or disable squelch.
    public func setSquelchEnabled(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_squelch_enabled(handle, enabled))
    }

    /// Enable or disable auto-squelch (engine-side noise-floor
    /// tracking). While on, the engine adjusts the squelch
    /// threshold continuously; manual `setSquelchDb` writes are
    /// accepted but will be overwritten on the next tracker cycle.
    public func setAutoSquelch(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_auto_squelch(handle, enabled))
    }

    /// Set the squelch threshold in dB.
    public func setSquelchDb(_ db: Float) throws {
        try checkRc(sdr_core_set_squelch_db(handle, db))
    }

    /// Set the FM de-emphasis mode.
    public func setDeemphasis(_ mode: Deemphasis) throws {
        try checkRc(sdr_core_set_deemphasis(handle, mode.rawValue))
    }

    // ==========================================================
    //  Advanced demod — #245. Each wraps a one-line FFI call to
    //  a matching `UiToDsp::Set*` variant. Mode gating (WFM
    //  stereo only meaningful in WFM, etc.) is left to the host
    //  UI — the engine no-ops a toggle that doesn't apply to
    //  the current demod.
    // ==========================================================

    /// Enable or disable the noise blanker.
    public func setNoiseBlankerEnabled(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_nb_enabled(handle, enabled))
    }

    /// Set the noise-blanker threshold multiplier (>= 1.0).
    public func setNoiseBlankerLevel(_ level: Float) throws {
        try checkRc(sdr_core_set_nb_level(handle, level))
    }

    /// Enable or disable FM IF noise reduction (WFM / NFM only).
    public func setFmIfNrEnabled(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_fm_if_nr_enabled(handle, enabled))
    }

    /// Enable or disable WFM stereo decode.
    public func setWfmStereo(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_wfm_stereo(handle, enabled))
    }

    /// Enable or disable the audio-stage notch filter.
    public func setNotchEnabled(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_notch_enabled(handle, enabled))
    }

    /// Set the audio notch frequency in Hz (> 0).
    public func setNotchFrequencyHz(_ hz: Float) throws {
        try checkRc(sdr_core_set_notch_frequency(handle, hz))
    }

    /// Set the audio output volume (clamped internally to `[0, 1]`).
    public func setVolume(_ volume: Float) throws {
        try checkRc(sdr_core_set_volume(handle, volume))
    }

    /// Select the audio output device by UID. Pass `""` to route
    /// to the system default output. The UID is the opaque string
    /// returned by `SdrCore.audioDeviceUid(at:)`.
    public func setAudioDevice(_ uid: String) throws {
        try checkRc(uid.withCString { sdr_core_set_audio_device(handle, $0) })
    }

    /// Switch the active IQ source between the local RTL-SDR
    /// dongle, a network IQ stream, a WAV file, or an rtl_tcp
    /// client. The engine stops the current source, rebuilds
    /// from the persisted per-type config (network host/port,
    /// file path, etc.), and restarts if the engine is running.
    /// Per issues #235, #236.
    public func setSourceType(_ type: SourceType) throws {
        try checkRc(sdr_core_set_source_type(handle, type.rawValue))
    }

    /// Configure the network IQ source endpoint. `hostname`
    /// must be non-empty; `port` is the TCP / UDP port;
    /// `protocol` picks the transport. The engine stores the
    /// values; they take effect on the next switch into
    /// `.network` (or when the engine restarts while `.network`
    /// is already active).
    public func setNetworkConfig(
        hostname: String,
        port: UInt16,
        protocol proto: NetworkSourceProtocol
    ) throws {
        try checkRc(hostname.withCString { cHost in
            sdr_core_set_network_config(handle, cHost, port, proto.rawValue)
        })
    }

    /// Set the filesystem path the file-playback source reads
    /// from the next time `.file` is activated (or the source
    /// is restarted while `.file` is already active). The
    /// engine does not open the file here — only stores the
    /// path. Open errors surface as `.error(...)` /
    /// `.sourceStopped` events once the source actually starts.
    public func setFilePath(_ path: String) throws {
        try checkRc(path.withCString { cPath in
            sdr_core_set_file_path(handle, cPath)
        })
    }

    // ==========================================================
    //  rtl_tcp-specific client commands (ABI 0.11, issue #325)
    //
    //  Non-rtl_tcp active sources silently accept these — the
    //  Rust `Source` trait's default no-op impl keeps the ABI
    //  callable regardless of current source. Hosts don't need
    //  to gate UI toggles on the active source type.
    // ==========================================================

    /// Enable or disable the dongle's bias tee.
    public func setBiasTee(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_bias_tee(handle, enabled))
    }

    /// RTL2832 direct-sampling mode.
    public enum DirectSamplingMode: Int32, Sendable, CaseIterable, Codable {
        case off = 0
        case iBranch = 1
        case qBranch = 2

        public var label: String {
            switch self {
            case .off: return "Off"
            case .iBranch: return "I branch"
            case .qBranch: return "Q branch"
            }
        }
    }

    /// Set direct-sampling mode.
    public func setDirectSampling(_ mode: DirectSamplingMode) throws {
        try checkRc(sdr_core_set_direct_sampling(handle, mode.rawValue))
    }

    /// Enable or disable tuner offset-tuning.
    public func setOffsetTuning(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_offset_tuning(handle, enabled))
    }

    /// Enable or disable RTL2832 digital AGC. Distinct from the
    /// analog tuner AGC that `setAgc(_:)` controls.
    public func setRtlAgc(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_rtl_agc(handle, enabled))
    }

    /// Set tuner gain by index. Useful for rtl_tcp clients
    /// where the server publishes a gain count but not the dB
    /// values. Engine bounds-checks against the active source's
    /// `gains()` count; out-of-range indices surface as
    /// `.error(...)` events rather than silent drops.
    public func setGainByIndex(_ index: UInt32) throws {
        try checkRc(sdr_core_set_gain_by_index(handle, index))
    }

    /// Switch the active audio sink between the local output
    /// device and the network stream. The engine stops the
    /// current sink, builds the replacement from the persisted
    /// device / network config, and restarts it if the engine is
    /// currently running. Status transitions land on the
    /// `events` stream as `.networkSinkStatus(...)` — hosts use
    /// them to drive a status row in the audio settings panel.
    /// Per issue #247.
    public func setAudioSinkType(_ type: AudioSinkType) throws {
        try checkRc(sdr_core_set_audio_sink_type(handle, type.rawValue))
    }

    /// Configure the network audio sink endpoint. `hostname`
    /// must be non-empty; `port` is the TCP / UDP port the
    /// engine should listen / send on; `protocol` picks the
    /// transport. If the network sink is currently active the
    /// engine rebuilds it inline so the new endpoint takes
    /// effect immediately; otherwise the values are stored for
    /// the next `setAudioSinkType(.network)` switch.
    public func setNetworkSinkConfig(
        hostname: String,
        port: UInt16,
        protocol proto: NetworkProtocol
    ) throws {
        try checkRc(hostname.withCString { cHost in
            sdr_core_set_network_sink_config(handle, cHost, port, proto.rawValue)
        })
    }

    /// Start recording the demodulated audio stream to a WAV file
    /// at `path`. The engine emits `.audioRecordingStarted(path:)`
    /// on success or `.error(...)` on failure.
    public func startAudioRecording(path: String) throws {
        try checkRc(path.withCString { sdr_core_start_audio_recording(handle, $0) })
    }

    /// Stop audio recording. Safe to call when nothing is active —
    /// the engine always emits `.audioRecordingStopped` in response.
    public func stopAudioRecording() throws {
        try checkRc(sdr_core_stop_audio_recording(handle))
    }

    /// Start recording the raw IQ stream to a WAV file at `path`.
    /// The engine writes at the current tuner sample rate with
    /// two channels (I / Q); file size per second scales with
    /// the source rate, so fast source rates produce large files.
    /// Emits `.iqRecordingStarted(path:)` on success or
    /// `.error(...)` on failure.
    public func startIqRecording(path: String) throws {
        try checkRc(path.withCString { sdr_core_start_iq_recording(handle, $0) })
    }

    /// Stop IQ recording. Safe to call when nothing is active —
    /// the engine always emits `.iqRecordingStopped` in response.
    public func stopIqRecording() throws {
        try checkRc(sdr_core_stop_iq_recording(handle))
    }

    /// Enable or disable DC blocking on the IQ frontend.
    public func setDcBlocking(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_dc_blocking(handle, enabled))
    }

    /// Enable or disable IQ inversion (conjugation).
    public func setIqInversion(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_iq_inversion(handle, enabled))
    }

    /// Enable or disable adaptive IQ imbalance correction.
    public func setIqCorrection(_ enabled: Bool) throws {
        try checkRc(sdr_core_set_iq_correction(handle, enabled))
    }

    /// Set the FFT size (nonzero power of two).
    public func setFftSize(_ n: Int) throws {
        try checkRc(sdr_core_set_fft_size(handle, n))
    }

    /// Set the FFT window function.
    public func setFftWindow(_ window: FftWindow) throws {
        try checkRc(sdr_core_set_fft_window(handle, window.rawValue))
    }

    /// Set the FFT display frame rate in fps.
    public func setFftRate(_ fps: Double) throws {
        try checkRc(sdr_core_set_fft_rate(handle, fps))
    }

    // ==========================================================
    //  FFT frame pull
    // ==========================================================

    /// Render-tick FFT pull.
    ///
    /// Calls `body` synchronously with a borrowed view over the
    /// most recent FFT frame when a new one is available. The
    /// buffer passed to `body` is only valid for the duration
    /// of the call — copy anything you want to keep out before
    /// returning.
    ///
    /// Returns `true` when a frame was available (and `body`
    /// was called), `false` when no new frame has arrived since
    /// the previous pull.
    ///
    /// Designed for the SwiftUI/Metal `draw(in:)` path: call
    /// this on every render tick, render the previous frame
    /// again on `false`.
    ///
    /// `body` is marked `@escaping` because it's captured by
    /// the internal `FftBodyBox` whose opaque pointer is passed
    /// into the C function — even though in practice the box
    /// only lives for the duration of this function via
    /// `withExtendedLifetime`, the type system has to assume
    /// escape is possible.
    @discardableResult
    public func withLatestFftFrame(
        _ body: @escaping (UnsafeBufferPointer<Float>, _ sampleRateHz: Double, _ centerFreqHz: Double) -> Void
    ) -> Bool {
        // Box the closure so the C trampoline can retrieve it
        // from the opaque user_data pointer. The box must be
        // bound to a `let` and kept alive via
        // `withExtendedLifetime` for the duration of the call —
        // the `Unmanaged.passUnretained` pattern would otherwise
        // free the box before the callback runs.
        //
        // (This is exactly the dangling-pointer bug CodeRabbit
        // caught in the FFI design spec on PR #227. The fix
        // pattern is documented there.)
        let box = FftBodyBox(body: body)
        let opaque = Unmanaged.passUnretained(box).toOpaque()

        return withExtendedLifetime(box) {
            sdr_core_pull_fft(handle, SdrCore.fftTrampoline, opaque)
        }
    }

    /// Internal box holding a closure passed to the FFT pull.
    private final class FftBodyBox {
        let body: (UnsafeBufferPointer<Float>, Double, Double) -> Void
        init(
            body: @escaping (UnsafeBufferPointer<Float>, Double, Double) -> Void
        ) {
            self.body = body
        }
        deinit {}
    }

    /// C callback trampoline for the FFT pull path.
    private static let fftTrampoline: @convention(c) (
        UnsafePointer<SdrFftFrame>?,
        UnsafeMutableRawPointer?
    ) -> Void = { framePtr, userData in
        guard let framePtr, let userData else { return }
        let box = Unmanaged<FftBodyBox>.fromOpaque(userData).takeUnretainedValue()
        let frame = framePtr.pointee
        guard let mags = frame.magnitudes_db else { return }
        let buf = UnsafeBufferPointer(start: mags, count: frame.len)
        box.body(buf, frame.sample_rate_hz, frame.center_freq_hz)
    }

    // ==========================================================
    //  Metadata
    // ==========================================================

    /// The ABI version the library was built with. Host apps
    /// should call this once at startup and refuse to run on
    /// a major mismatch against what they were compiled for.
    public static var abiVersion: (major: UInt16, minor: UInt16) {
        let packed = sdr_core_abi_version()
        return (
            major: UInt16(truncatingIfNeeded: packed >> 16),
            minor: UInt16(truncatingIfNeeded: packed & 0xFFFF)
        )
    }

    /// ABI version the Swift wrapper was COMPILED against, pulled
    /// from the C header's `SDR_CORE_ABI_VERSION_*` `#define`s at
    /// build time via Clang's macro import. Compare with
    /// `abiVersion` (runtime) at launch to catch a catastrophic
    /// packaging bug where the Swift side and the statically-
    /// linked `libsdr_ffi.a` drifted apart — a mismatched major
    /// means struct layouts / enum discriminants likely differ
    /// and the engine will misbehave unpredictably. Fail fast
    /// instead.
    public static let compiledAbiVersion: (major: UInt16, minor: UInt16) = (
        UInt16(SDR_CORE_ABI_VERSION_MAJOR),
        UInt16(SDR_CORE_ABI_VERSION_MINOR)
    )

    /// Initialize `tracing` log routing to stderr. Optional —
    /// call once before creating any `SdrCore` instance if you
    /// want Rust-side log output visible.
    public static func initLogging(minLevel: LogLevel = .info) {
        sdr_core_init_logging(minLevel.rawValue)
    }

    /// Log level for `initLogging`.
    public enum LogLevel: Int32, Sendable {
        case error = 0
        case warn  = 1
        case info  = 2
        case debug = 3
        case trace = 4
    }

    // ==========================================================
    //  Device enumeration (static, no handle required)
    // ==========================================================

    /// Number of RTL-SDR devices currently attached to the USB
    /// bus. Safe to call before `SdrCore` is created — hosts
    /// typically call this at app launch to surface whether a
    /// dongle is plugged in, independent of the engine lifecycle.
    ///
    /// Returns 0 when no devices are present (or enumeration
    /// failed; check `SdrCore.lastErrorMessage()` in that case).
    public static var deviceCount: UInt32 {
        sdr_core_device_count()
    }

    /// Human-readable name for the RTL-SDR device at `index`.
    /// Returns `nil` if `index` is out of range or the name
    /// couldn't be probed. Safe to call at any time — no handle
    /// required.
    public static func deviceName(at index: UInt32) -> String? {
        // 128 bytes is comfortably more than any RTL-SDR name
        // ever printed. Fixed buffer on the stack is cheap and
        // avoids a heap alloc for what's typically a one-shot
        // probe at startup.
        var buf = [CChar](repeating: 0, count: 128)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            guard let base = ptr.baseAddress else { return -1 }
            return sdr_core_device_name(index, base, ptr.count)
        }
        guard rc >= 0 else { return nil }
        return cStringToSwiftString(buf)
    }

    // ==========================================================
    //  Audio device enumeration (static, no handle required)
    // ==========================================================

    /// Descriptor for an audio output device the backend knows about.
    /// `uid` is the opaque identifier to pass to `setAudioDevice` —
    /// empty string means "system default output".
    public struct AudioDevice: Sendable, Hashable, Identifiable {
        public let displayName: String
        public let uid: String
        public var id: String { uid }

        public init(displayName: String, uid: String) {
            self.displayName = displayName
            self.uid = uid
        }
    }

    /// Snapshot of audio output devices currently enumerable.
    /// Safe to call before `SdrCore` is created — the list comes
    /// from the backend (CoreAudio on macOS), not the engine.
    ///
    /// Each call re-runs the backend query; hosts typically call
    /// this on Settings panel open. Index 0 is typically the
    /// "system default" entry (UID `""`).
    public static var audioDevices: [AudioDevice] {
        let count = sdr_core_audio_device_count()
        guard count > 0 else { return [] }
        var result: [AudioDevice] = []
        result.reserveCapacity(Int(count))
        for i in 0..<count {
            guard let name = audioDeviceName(at: i),
                  let uid = audioDeviceUid(at: i) else {
                continue
            }
            result.append(AudioDevice(displayName: name, uid: uid))
        }
        return result
    }

    /// Display name for the audio output device at `index`.
    /// Returns `nil` if `index` is out of range.
    public static func audioDeviceName(at index: UInt32) -> String? {
        audioDeviceString(index: index, call: sdr_core_audio_device_name)
    }

    /// Opaque UID for the audio output device at `index`. Pass
    /// this to `setAudioDevice` to route. Empty string means
    /// "system default output".
    public static func audioDeviceUid(at index: UInt32) -> String? {
        audioDeviceString(index: index, call: sdr_core_audio_device_uid)
    }

    /// Shared fixed-buffer caller for `sdr_core_audio_device_name`
    /// / `_uid`. 512 bytes covers any reasonable CoreAudio name —
    /// real device names max out around 60 characters and UIDs
    /// (once we migrate to `kAudioDevicePropertyDeviceUID`) are
    /// typically <128 bytes.
    private static func audioDeviceString(
        index: UInt32,
        call: (UInt32, UnsafeMutablePointer<CChar>?, Int) -> Int32
    ) -> String? {
        var buf = [CChar](repeating: 0, count: 512)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            guard let base = ptr.baseAddress else { return -1 }
            return call(index, base, ptr.count)
        }
        guard rc >= 0 else { return nil }
        return cStringToSwiftString(buf)
    }
}

/// Shared helper that decodes a NUL-terminated `[CChar]` buffer
/// into a Swift `String`. Used in place of `String(cString: [CChar])`
/// which was deprecated in the Swift 6.2 standard library in favor
/// of a bring-your-own-truncation API. Using the `UnsafePointer`
/// form of `init(cString:)` is still valid and picks the NUL up
/// natively, so route the array through its base pointer.
@inline(__always)
func cStringToSwiftString(_ buf: [CChar]) -> String {
    buf.withUnsafeBufferPointer { ptr in
        guard let base = ptr.baseAddress else { return "" }
        return String(cString: base)
    }
}
