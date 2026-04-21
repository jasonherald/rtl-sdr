//
// UsbHotplugMonitor.swift — live USB plug/unplug notifications.
//
// Wraps IOKit's `IOServiceAddMatchingNotification` for
// `IOUSBDevice` matched + terminated events. On any plug or
// unplug (of any USB device, not just RTL-SDR), the `onChange`
// closure fires on the main actor. The consumer is expected to
// re-probe via `CoreModel.refreshDeviceInfo()` — libusb's
// enumeration inside that probe already filters to the RTL-SDR
// known-devices table, so we don't need to filter by VID/PID
// here. Unrelated USB events (keyboard plug, storage mount)
// cause a cheap no-op probe that leaves `hasLocalRtlSdr`
// unchanged; RTL-SDR events flip the flag live, no focus-flip
// or restart required.
//
// Addresses issue #363 — the scenePhase-on-active fallback
// from PR #362 round 6 was a stop-gap for this proper IOKit
// path.

import Foundation
import IOKit
import IOKit.usb

/// Live USB hotplug monitor. One instance owned by
/// `CoreModel`; starts in `bootstrap()`, tears down in
/// `shutdown()`. Failing to construct (rare — IOKit is always
/// available on macOS) yields `nil` and the app continues
/// without hotplug detection (falls back to the scenePhase
/// probe safety net).
final class UsbHotplugMonitor {
    /// Handler invoked on every USB added or removed event.
    /// `@MainActor` because the notification port is attached
    /// to the main run loop, so the C trampoline fires on the
    /// main thread; callers can mutate `@Observable` state
    /// directly without hopping.
    typealias OnChange = @MainActor () -> Void

    /// Notification port + two iterators, one per event kind
    /// (matched = plug, terminated = unplug). IOKit routes
    /// events through the port's run-loop source on each
    /// iterator, and the callback MUST drain its iterator each
    /// time or no further events arrive for that subscription.
    private let notifyPort: IONotificationPortRef
    private var addedIterator: io_iterator_t = IO_OBJECT_NULL
    private var removedIterator: io_iterator_t = IO_OBJECT_NULL

    /// Retained box passed as `refCon` to the C trampoline.
    /// Lives for the monitor's lifetime so the callback can
    /// recover it via `takeUnretainedValue`. Pattern matches
    /// the `SdrRtlTcpBrowser` wrapper.
    private let callbackBox: CallbackBox

    private final class CallbackBox {
        let handler: OnChange
        init(handler: @escaping OnChange) {
            self.handler = handler
        }
    }

    /// Construct and start the monitor. Returns `nil` if the
    /// notification port or the initial `IOServiceAddMatchingNotification`
    /// registration fails. Both outcomes are rare on macOS —
    /// IOKit is always present — and the app falls back to the
    /// scenePhase probe if construction fails.
    init?(onChange: @escaping OnChange) {
        let box = CallbackBox(handler: onChange)
        self.callbackBox = box

        guard let port = IONotificationPortCreate(kIOMainPortDefault) else {
            return nil
        }
        self.notifyPort = port

        // Attach the port's run-loop source to the main run
        // loop so callbacks fire on the main thread. Using
        // `.commonModes` means events still fire while the UI
        // is in modal presentation (sheets, menus) rather than
        // getting deferred until the user dismisses.
        let source = IONotificationPortGetRunLoopSource(port).takeUnretainedValue()
        CFRunLoopAddSource(CFRunLoopGetMain(), source, .commonModes)

        let refCon = Unmanaged.passUnretained(box).toOpaque()

        // Register for device-added events. `IOServiceMatching`
        // with `kIOUSBDeviceClassName` matches every USB device
        // — we filter to RTL-SDR inside `refreshDeviceInfo()`
        // via libusb's known-devices table, so a broad subscription
        // here is simpler and cheap.
        //
        // Note: `IOServiceAddMatchingNotification` consumes the
        // matching dict, so we re-create a fresh dict for the
        // second (removed) registration below.
        let addedMatching = IOServiceMatching(kIOUSBDeviceClassName)
        let addedRc = IOServiceAddMatchingNotification(
            port,
            kIOMatchedNotification,
            addedMatching,
            Self.notificationCallback,
            refCon,
            &addedIterator
        )
        guard addedRc == KERN_SUCCESS else {
            IONotificationPortDestroy(port)
            return nil
        }
        // Drain the initial batch to arm the iterator. On first
        // registration IOKit delivers the set of already-matching
        // devices through the iterator; we don't invoke the
        // user's handler for those (bootstrap already probed),
        // but we MUST drain or no future events arrive.
        Self.drainIterator(addedIterator)

        let removedMatching = IOServiceMatching(kIOUSBDeviceClassName)
        let removedRc = IOServiceAddMatchingNotification(
            port,
            kIOTerminatedNotification,
            removedMatching,
            Self.notificationCallback,
            refCon,
            &removedIterator
        )
        guard removedRc == KERN_SUCCESS else {
            IOObjectRelease(addedIterator)
            IONotificationPortDestroy(port)
            return nil
        }
        Self.drainIterator(removedIterator)
    }

    deinit {
        if addedIterator != IO_OBJECT_NULL {
            IOObjectRelease(addedIterator)
        }
        if removedIterator != IO_OBJECT_NULL {
            IOObjectRelease(removedIterator)
        }
        IONotificationPortDestroy(notifyPort)
    }

    // ----------------------------------------------------------
    //  C trampoline
    // ----------------------------------------------------------

    /// Shared callback for both `kIOMatchedNotification` and
    /// `kIOTerminatedNotification` — we don't distinguish
    /// between plug and unplug at this layer because the
    /// consumer re-probes either way. The iterator must be
    /// drained before the callback returns or IOKit stops
    /// delivering events on that subscription.
    private static let notificationCallback: IOServiceMatchingCallback = { refCon, iterator in
        guard let refCon else { return }
        let box = Unmanaged<CallbackBox>.fromOpaque(refCon).takeUnretainedValue()
        drainIterator(iterator)
        // The run-loop source was added to `CFRunLoopGetMain()`
        // in `init`, so this callback is running on the main
        // thread. Assume MainActor isolation to invoke the
        // `@MainActor` handler synchronously — skips the
        // `Task { @MainActor in … }` indirection that the
        // dispatcher-threaded `SdrRtlTcpBrowser` needs.
        MainActor.assumeIsolated {
            box.handler()
        }
    }

    /// Drain an `io_iterator_t` by repeatedly calling
    /// `IOIteratorNext` and releasing each returned object.
    /// IOKit requires this — an un-drained iterator silently
    /// stops delivering events.
    private static func drainIterator(_ iterator: io_iterator_t) {
        var obj = IOIteratorNext(iterator)
        while obj != IO_OBJECT_NULL {
            IOObjectRelease(obj)
            obj = IOIteratorNext(iterator)
        }
    }
}
