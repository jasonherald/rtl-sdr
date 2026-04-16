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
//   5. `shutdown()` is called on `deinit`, but hosts that want
//      deterministic teardown can call it explicitly.
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
    /// Opaque C pointer obtained from `sdr_core_create`.
    private let handle: OpaquePointer

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
    /// eventually load from and persist to. Pass an empty URL
    /// (`URL(fileURLWithPath: "")`) or a nil-equivalent to run
    /// with in-memory defaults — v1 engines do not yet read or
    /// write through this path, but passing a valid path now
    /// means persistence can land in a follow-up without an
    /// API change.
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
            // Drain and finish the continuation so anything
            // listening gets a clean end signal. Finishing
            // first, destroying second — the C handle is still
            // valid when we call `sdr_core_destroy`; if we
            // tore it down first the continuation's `finish()`
            // would be a no-op (no harm, just unnecessary
            // cleanup ordering noise).
            continuation.finish()
            sdr_core_destroy(handle)
            throw SdrCoreError.fromCurrentError(rawCode: registerRc)
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

    /// Set the squelch threshold in dB.
    public func setSquelchDb(_ db: Float) throws {
        try checkRc(sdr_core_set_squelch_db(handle, db))
    }

    /// Set the FM de-emphasis mode.
    public func setDeemphasis(_ mode: Deemphasis) throws {
        try checkRc(sdr_core_set_deemphasis(handle, mode.rawValue))
    }

    /// Set the audio output volume (clamped internally to `[0, 1]`).
    public func setVolume(_ volume: Float) throws {
        try checkRc(sdr_core_set_volume(handle, volume))
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
}
