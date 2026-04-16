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
    /// Raw source rate (display bandwidth). Updated from
    /// `DisplayBandwidth` events.
    var sampleRateHz: Double = 2_000_000
    /// Post-decimation. Updated from `SampleRateChanged` events.
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
        do {
            let c = try SdrCore(configPath: configPath)
            self.core = c
            self.eventTask = Task { [events = c.events] in
                await self.consumeEvents(from: events)
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

    private func consumeEvents(from stream: AsyncStream<SdrCoreEvent>) async {
        for await event in stream {
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
                sampleRateHz = hz
            case .error(let msg):
                lastError = msg
            }
        }
    }

    // ==========================================================
    //  Commands — strict (lifecycle)
    // ==========================================================

    func start() {
        guard let core else { lastError = "engine not initialized"; return }
        do {
            try core.start()
            isRunning = true
            lastError = nil
        } catch {
            lastError = "start failed: \(error)"
        }
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

    // ==========================================================
    //  Internal helpers
    // ==========================================================

    private func capture(_ work: () throws -> Void) {
        do { try work() }
        catch { lastError = "\(error)" }
    }
}
