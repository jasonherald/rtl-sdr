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
    /// by `shutdown`. Also self-ends naturally when the
    /// underlying `SdrCore` handle is released — see the
    /// "No explicit deinit" comment below and issue #293 for
    /// the teardown story. This property stays @MainActor-
    /// isolated like the rest of the model; we experimented
    /// with making it `nonisolated` to allow deinit access
    /// and backed off (the `@ObservationTracked` macro that
    /// backs `@Observable` forbids `nonisolated` on its
    /// generated storage).
    private var eventTask: Task<Void, Never>?

    // ==========================================================
    //  Lifecycle state
    // ==========================================================

    var isRunning: Bool = false
    var lastError: String? = nil

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

        // Probe for RTL-SDR hardware BEFORE creating the engine
        // so the UI can show device presence (or absence) from
        // the first frame, not only after the user hits Play.
        // This is a handle-free libusb device-list query; no USB
        // control transfers, no hardware open.
        refreshDeviceInfo()

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
    /// Called once from `bootstrap()` so the device state shows
    /// up on first paint rather than only after Play. Safe to
    /// call again later (hotplug detection is a future add).
    func refreshDeviceInfo() {
        let count = SdrCore.deviceCount
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
        case .error(let msg):
            lastError = msg
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
        guard displayBandwidthHz > 0, factor > 0, factor.isFinite else { return }
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

    // No explicit `deinit` on CoreModel — tracked by issue #293.
    // The `@MainActor
    // @Observable` class gets @ObservationTracked-macro-generated
    // storage for every `var`, and Swift's current rules don't
    // let macro-generated mutable stored properties be
    // `nonisolated`, which would be required for a `deinit` on a
    // MainActor class to access them. Cleanup relies on:
    //   1. The event-consumer Task's `[weak self]` capture,
    //      which makes `self?.handleEvent` a no-op after the
    //      model is dropped.
    //   2. `SdrCore.deinit` firing when `self.core` is released,
    //      which calls `sdr_core_destroy` → closes the engine's
    //      event channel → the AsyncStream completes → the Task
    //      exits its `for await` loop cleanly.
    //
    // In practice, app shutdown goes through
    // `AppDelegate.applicationWillTerminate → shutdown()` which
    // cancels the task explicitly, so the fallback path only
    // runs in tests that let the model dealloc without calling
    // shutdown.

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
