//
// PowerModeObserver.swift — watches AC/battery + Low Power Mode
// and publishes a coarse "full throttle OK" vs "conserve power"
// signal.
//
// The Metal display link runs at display-native refresh when
// allowed (ProMotion: 120 Hz on an M-series laptop). Burning
// that rate while on battery with no external signal to show is
// a waste — an FFT engine at 20 Hz doesn't need 120 render ticks
// per second to look smooth. Throttling the display link's
// preferred rate range on battery drops CPU/GPU usage noticeably
// (mostly the CPU side — the render pass itself is cheap, but
// display-link setup, command-buffer queueing, and completion
// handlers all add up at 120 Hz).
//
// ## Signal sources
//
// Two independent inputs, combined with OR:
//
//   1. IOKit power source (IOPSCopyPowerSourcesInfo +
//      IOPSGetProvidingPowerSourceType). Returns "AC Power" or
//      "Battery Power". On desktops (Mac mini / Studio / iMac)
//      always reports AC, so the code Just Works there.
//
//   2. ProcessInfo.isLowPowerModeEnabled. User-toggled from the
//      Battery preference pane (even on AC), and macOS auto-
//      toggles it on low battery. Respecting this is an Apple
//      HIG item — apps should visibly downshift when LPM is on.
//
// Combined rule: if either "on battery" OR "LPM enabled", we
// publish `.conserve`. Otherwise `.acFull`.
//
// ## Delivery
//
// `onChange` fires on the main thread (the IOKit run-loop source
// is added to the main runloop; LPM notifications are posted to
// the default notification center and observed without a queue
// override, so they also land on main).

import Foundation
import IOKit.ps

/// Coarse power posture for render-loop rate selection.
enum PowerMode {
    /// AC power and Low Power Mode is off. Free to run the
    /// display link at its full preferred rate.
    case acFull

    /// Running on battery or user-enabled Low Power Mode.
    /// Throttle the display link to a lower rate — roughly
    /// matching typical FFT engine cadence so the waterfall
    /// still looks smooth.
    case conserve
}

final class PowerModeObserver {
    /// Current mode. Updated when IOKit notifies of a power-source
    /// change OR when LPM is toggled. Read on the main thread
    /// only — the observer writes from the main thread via both
    /// its IOKit run-loop source and its NotificationCenter
    /// observer.
    private(set) var mode: PowerMode

    /// Fires after `mode` changes. Called on the main thread.
    /// The view sets this to re-apply `preferredFrameRateRange`
    /// on the display link.
    var onChange: ((PowerMode) -> Void)?

    private var runLoopSource: CFRunLoopSource?
    private var lowPowerObserver: NSObjectProtocol?

    init() {
        self.mode = Self.currentMode()
        startObserving()
    }

    deinit {
        stopObserving()
    }

    // ----------------------------------------------------------
    //  Mode computation
    // ----------------------------------------------------------

    private static func currentMode() -> PowerMode {
        if ProcessInfo.processInfo.isLowPowerModeEnabled {
            return .conserve
        }
        if isOnBattery() {
            return .conserve
        }
        return .acFull
    }

    /// Query IOKit for the "providing" power source type. Returns
    /// true only on a clear "Battery Power" response — any other
    /// answer (including errors, missing data, or the desktop
    /// case where there's no battery) defaults to AC. That's the
    /// conservative default: when in doubt, don't penalize the
    /// user with a low-rate render loop.
    private static func isOnBattery() -> Bool {
        guard let infoUnmanaged = IOPSCopyPowerSourcesInfo() else {
            return false
        }
        let info = infoUnmanaged.takeRetainedValue()
        guard let typeUnmanaged = IOPSGetProvidingPowerSourceType(info) else {
            return false
        }
        let typeString = typeUnmanaged.takeUnretainedValue() as String
        return typeString == kIOPSBatteryPowerValue
    }

    private func refresh() {
        let newMode = Self.currentMode()
        if newMode != mode {
            mode = newMode
            onChange?(newMode)
        }
    }

    // ----------------------------------------------------------
    //  Observer lifecycle
    // ----------------------------------------------------------

    private func startObserving() {
        // 1. IOKit power-source change callback. Triggers on
        //    AC ⇄ battery transitions. The callback gets `self`
        //    via an opaque pointer — we pass unretained because
        //    this observer owns the run-loop source's lifetime
        //    via stopObserving(), so self outlives the callback.
        let opaqueSelf = Unmanaged.passUnretained(self).toOpaque()
        if let source = IOPSNotificationCreateRunLoopSource(
            { context in
                guard let context else { return }
                let observer = Unmanaged<PowerModeObserver>
                    .fromOpaque(context)
                    .takeUnretainedValue()
                observer.refresh()
            },
            opaqueSelf
        )?.takeRetainedValue() {
            CFRunLoopAddSource(CFRunLoopGetMain(), source, .defaultMode)
            self.runLoopSource = source
        }

        // 2. Low Power Mode notification. Posted when the user
        //    flips the toggle in the Battery preference pane OR
        //    when macOS auto-enables LPM at low battery. No
        //    queue override means it lands on whatever thread
        //    the system posts from — in practice main, but be
        //    defensive and hop if we ever see it off-main.
        lowPowerObserver = NotificationCenter.default.addObserver(
            forName: .NSProcessInfoPowerStateDidChange,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            self?.refresh()
        }
    }

    private func stopObserving() {
        if let source = runLoopSource {
            CFRunLoopRemoveSource(CFRunLoopGetMain(), source, .defaultMode)
            runLoopSource = nil
        }
        if let token = lowPowerObserver {
            NotificationCenter.default.removeObserver(token)
            lowPowerObserver = nil
        }
    }
}
