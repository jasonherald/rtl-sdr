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

    /// Event-consuming task. Cancelled on shutdown.
    private var eventTask: Task<Void, Never>?

    // ==========================================================
    //  Lifecycle state
    // ==========================================================

    var isRunning: Bool = false
    var lastError: String? = nil

    // ==========================================================
    //  Tuning
    // ==========================================================

    var centerFrequencyHz: Double = 100_700_000
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
    /// Engine-reported post-decimation / post-resample rate.
    /// Updated from `SampleRateChanged` and `DisplayBandwidth`
    /// events (both carry the same effective rate in v1). Used
    /// by the status bar and, in M4, by the spectrum renderer
    /// as its display span.
    var effectiveSampleRateHz: Double = 250_000
    var ppmCorrection: Int = 0

    // ==========================================================
    //  Tuner
    // ==========================================================

    var availableGains: [Double] = []
    var gainDb: Double = 0
    var agcEnabled: Bool = false
    var deviceInfo: String = ""

    // ==========================================================
    //  Demod
    // ==========================================================

    var demodMode: DemodMode = .wfm
    var bandwidthHz: Double = 200_000
    var squelchEnabled: Bool = false
    var squelchDb: Float = -60
    var deemphasis: Deemphasis = .us75

    // ==========================================================
    //  Audio
    // ==========================================================

    var volume: Float = 0.5

    // ==========================================================
    //  Display
    // ==========================================================

    var fftSize: Int = 2048
    var fftWindow: FftWindow = .blackman
    var fftRateFps: Double = 20
    var minDb: Float = -100
    var maxDb: Float = 0

    // ==========================================================
    //  Status
    // ==========================================================

    var signalLevelDb: Float = -120

    // ==========================================================
    //  Bootstrap / shutdown
    // ==========================================================

    /// Build the engine handle and kick off the event-consumption
    /// task. Called once from `ContentView.task` on app launch.
    /// Safe to call multiple times — subsequent calls are no-ops
    /// if the engine is already up.
    func bootstrap(configPath: URL) async {
        guard core == nil else { return }
        // Install the Rust tracing subscriber once at process
        // start so engine errors and info logs land on stderr
        // (captured by Console.app / the xcrun log stream).
        // `initLogging` is idempotent via a OnceLock on the Rust
        // side — safe to call more than once, subsequent calls
        // are no-ops.
        SdrCore.initLogging(minLevel: .info)
        do {
            let c = try SdrCore(configPath: configPath)
            self.core = c
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
                    await self.handleEvent(event)
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
        if isRunning, let core {
            try? core.stop()
        }
        core = nil
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
            deviceInfo = s
        case .gainList(let gains):
            availableGains = gains
        case .displayBandwidth(let hz):
            // Engine-reported spectrum-display bandwidth.
            // Same value as `sampleRateChanged` in v1 (both
            // are effective_sample_rate on the Rust side);
            // we land them on the same UI field rather than
            // maintain two copies.
            effectiveSampleRateHz = hz
        case .error(let msg):
            lastError = msg
        }
    }

    // ==========================================================
    //  Commands — strict (lifecycle)
    // ==========================================================

    func start() {
        guard let core else { lastError = "engine not initialized"; return }
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
        do {
            try core.start()
            isRunning = true
            lastError = nil
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
    func syncToEngine() {
        guard core != nil else { return }
        setCenter(centerFrequencyHz)
        setVfoOffset(vfoOffsetHz)
        setSampleRate(sourceSampleRateHz)
        setPpm(ppmCorrection)
        setGain(gainDb)
        setAgc(agcEnabled)
        setDemodMode(demodMode)
        setBandwidth(bandwidthHz)
        setSquelchEnabled(squelchEnabled)
        setSquelchDb(squelchDb)
        setDeemphasis(deemphasis)
        setVolume(volume)
        setFftSize(fftSize)
        setFftWindow(fftWindow)
        setFftRate(fftRateFps)
    }

    func stop() {
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

    func setCenter(_ hz: Double) {
        centerFrequencyHz = hz
        capture { try core?.tune(hz) }
    }

    func setSampleRate(_ hz: Double) {
        sourceSampleRateHz = hz
        capture { try core?.setSampleRate(hz) }
    }

    func setVfoOffset(_ hz: Double) {
        vfoOffsetHz = hz
        capture { try core?.setVfoOffset(hz) }
    }

    func setDemodMode(_ m: DemodMode) {
        demodMode = m
        capture { try core?.setDemodMode(m) }
    }

    func setBandwidth(_ hz: Double) {
        bandwidthHz = hz
        capture { try core?.setBandwidth(hz) }
    }

    func setGain(_ db: Double) {
        gainDb = db
        capture { try core?.setGain(db) }
    }

    func setAgc(_ on: Bool) {
        agcEnabled = on
        capture { try core?.setAgc(on) }
    }

    func setSquelchDb(_ db: Float) {
        squelchDb = db
        capture { try core?.setSquelchDb(db) }
    }

    func setSquelchEnabled(_ on: Bool) {
        squelchEnabled = on
        capture { try core?.setSquelchEnabled(on) }
    }

    func setDeemphasis(_ m: Deemphasis) {
        deemphasis = m
        capture { try core?.setDeemphasis(m) }
    }

    func setVolume(_ v: Float) {
        volume = v
        capture { try core?.setVolume(v) }
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
