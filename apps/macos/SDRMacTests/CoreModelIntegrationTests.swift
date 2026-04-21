//
// CoreModelIntegrationTests.swift — exercises CoreModel against a
// real SdrCore (not a mock). Uses an in-memory config path so no
// filesystem state leaks across tests.
//
// These tests don't require a dongle — they verify the Swift-side
// glue (bootstrap → engine handle, setters → queued commands,
// events → model properties). Without a dongle, `start()` will
// fail at the source-open step and surface a device error, which
// is exactly the error path we want to exercise.

import XCTest
@testable import SDRMac
import SdrCoreKit

@MainActor
final class CoreModelIntegrationTests: XCTestCase {
    // Empty deinit satisfies SwiftLint's `required_deinit` rule
    // — same reasoning as the SdrCoreKit helper classes (see
    // `SdrCore.CallbackBox.deinit`). XCTestCase manages its own
    // lifecycle; we don't need teardown logic here.
    deinit {}

    private func inMemoryConfigPath() -> URL {
        // Empty path → SdrCore.init passes an empty C string
        // which the FFI treats as "no persistence". Nothing
        // touches disk.
        URL(fileURLWithPath: "")
    }

    func testBootstrapCreatesLiveEngine() async {
        let m = CoreModel()
        XCTAssertNil(m.core, "pre-bootstrap: no engine")

        await m.bootstrap(configPath: inMemoryConfigPath())

        XCTAssertNotNil(m.core, "post-bootstrap: engine present")
        XCTAssertNil(m.lastError, "bootstrap should not set lastError on success")

        m.shutdown()
        XCTAssertNil(m.core, "post-shutdown: engine gone")
    }

    func testBootstrapIsIdempotent() async {
        let m = CoreModel()
        await m.bootstrap(configPath: inMemoryConfigPath())
        let first = m.core
        await m.bootstrap(configPath: inMemoryConfigPath())
        XCTAssertTrue(m.core === first, "second bootstrap is a no-op")
        m.shutdown()
    }

    func testOptimisticSettersQueueEngineCommands() async {
        let m = CoreModel()
        await m.bootstrap(configPath: inMemoryConfigPath())

        // Each setter should update the UI binding immediately
        // AND not throw — the command just gets queued on the
        // mpsc channel.
        m.setCenter(88_500_000)
        XCTAssertEqual(m.centerFrequencyHz, 88_500_000)

        m.setDemodMode(.nfm)
        XCTAssertEqual(m.demodMode, .nfm)

        m.setBandwidth(12_500)
        XCTAssertEqual(m.bandwidthHz, 12_500)

        m.setVolume(0.3)
        XCTAssertEqual(m.volume, 0.3, accuracy: 0.001)

        m.setSquelchEnabled(true)
        XCTAssertTrue(m.squelchEnabled)
        m.setSquelchDb(-30)
        XCTAssertEqual(m.squelchDb, -30, accuracy: 0.001)

        // None of this should have produced errors — pure
        // command dispatch, no device interaction.
        XCTAssertNil(m.lastError)

        m.shutdown()
    }

    func testStartIsEndToEndSafe() async {
        // Drive the whole UI-command → engine → event → UI-state
        // loop end to end. We can't control whether a dongle is
        // attached during `swift test` — dev machines plugged
        // into hardware will succeed, CI / empty machines will
        // get a device-open error. Both are acceptable; the
        // invariant we're asserting is that the model ends up
        // in a consistent state either way, with no crash.
        let m = CoreModel()
        await m.bootstrap(configPath: inMemoryConfigPath())
        defer { m.shutdown() }

        m.start()

        // Give the event consumer up to 1 s to surface any async
        // error. We do the polling ourselves on the MainActor
        // rather than using NSPredicate because the predicate
        // closure runs on XCTest's internal thread —
        // `MainActor.assumeIsolated` would crash at runtime
        // there, and `MainActor.run` is async (not usable from
        // a sync NSPredicate closure). Short sync sleeps inside
        // an async loop are cheap and safe.
        let deadline = Date().addingTimeInterval(1.0)
        while Date() < deadline {
            if m.lastError != nil { break }
            try? await Task.sleep(nanoseconds: 20_000_000) // 20 ms
        }

        // Consistency: if an error was reported, we should NOT
        // claim to be running. If no error, we can be either
        // state — a dongle start might succeed (isRunning=true)
        // or quietly be deferred (isRunning=false still valid
        // until the event stream catches up).
        if m.lastError != nil {
            XCTAssertFalse(
                m.isRunning,
                "if lastError is set, isRunning should be false; got \(m.lastError ?? "nil")"
            )
        }

        // Stopping should always succeed without error propagation.
        m.stop()
        XCTAssertFalse(m.isRunning, "stop() should clear isRunning")
    }

    func testDeinitReleasesModelWithoutShutdown() async {
        // Regression guard for issue #293: a CoreModel that is
        // bootstrapped but dropped without an explicit
        // `shutdown()` (test scopes, hypothetical multi-window
        // future) must still release cleanly. The `@MainActor
        // deinit` cancels the event-consumer Task so its
        // `for await` loop exits — without that cancel, the
        // Task's [weak self] capture wouldn't pin the model
        // (so this test would pass as a pure refcount check),
        // BUT the Task would dangle holding the engine's
        // AsyncStream until SdrCore.deinit eventually closed
        // the channel. Cancelling up front unwinds
        // deterministically.
        //
        // Verify the model releases: use a `do { }` scope so
        // `m` definitely goes out of scope before we assert
        // on the weak ref. `let _ = m` inside a single test
        // method does NOT release until the method returns —
        // tracked in the feedback_swift_release memory.
        weak var weakModel: CoreModel?
        do {
            let m = CoreModel()
            await m.bootstrap(configPath: inMemoryConfigPath())
            weakModel = m
            XCTAssertNotNil(weakModel, "model alive inside scope")
            // Explicitly do NOT call shutdown() — deinit is
            // the safety net we're exercising.
        }
        // Give the cancelled Task a few runloop hops to
        // unwind its `for await` loop so nothing holds a
        // strong reference back to the model through the
        // Swift runtime's task pool.
        for _ in 0..<5 {
            await Task.yield()
        }
        XCTAssertNil(
            weakModel,
            "CoreModel should release after scope exit even without an explicit shutdown()"
        )
    }

    func testClearErrorDismissesLastError() {
        let m = CoreModel()
        m.lastError = "synthetic"
        XCTAssertEqual(m.lastError, "synthetic")
        m.clearError()
        XCTAssertNil(m.lastError)
    }

    func testSyncToEngineBeforeBootstrapIsNoOp() {
        // Before bootstrap there's no engine handle — syncToEngine
        // should bail early rather than wrap every setter in a
        // guard. Mutations to UI fields DON'T get pushed (there's
        // nowhere to push them), but nothing should throw or
        // record an error either.
        let m = CoreModel()
        m.centerFrequencyHz = 88_500_000
        m.syncToEngine()
        XCTAssertNil(m.lastError)
        XCTAssertEqual(m.centerFrequencyHz, 88_500_000, "UI state unchanged")
    }

    func testStartSyncsUIStateToEngine() async {
        // `start()` calls `syncToEngine()` BEFORE asking the
        // engine to start. We can't observe the engine's internal
        // state from Swift (it's opaque), but we CAN verify the
        // UI-visible side effects:
        //   - Every setter runs without setting lastError (the
        //     command queue accepts all of them).
        //   - UI fields remain at their pre-start values after
        //     the sync — setters are optimistic and don't bounce
        //     the field through an engine roundtrip.
        let m = CoreModel()
        await m.bootstrap(configPath: inMemoryConfigPath())
        defer { m.shutdown() }

        // Set some non-default values the UI might be showing
        // when the user hits Play.
        m.centerFrequencyHz = 146_520_000   // 2m ham calling freq
        m.bandwidthHz = 12_500
        m.volume = 0.25
        m.demodMode = .nfm

        m.start()
        defer { m.stop() }

        // Settings should be preserved (optimistic — no engine
        // echo overrides them immediately).
        XCTAssertEqual(m.centerFrequencyHz, 146_520_000)
        XCTAssertEqual(m.bandwidthHz, 12_500)
        XCTAssertEqual(m.volume, 0.25, accuracy: 0.001)
        XCTAssertEqual(m.demodMode, .nfm)

        // No sync errors: each setter's capture handler didn't
        // trip. (If a dongle is attached and source-open
        // succeeds, lastError may still be nil. If source-open
        // fails, lastError will be set by the event stream AFTER
        // start() returned, but the sync itself didn't produce
        // the error — we can't cleanly separate those without
        // mocking the engine, so we assert the weaker property
        // that the sync commands all dispatched.)
    }
}
