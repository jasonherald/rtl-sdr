//
// SdrCoreTests.swift — integration tests for the SdrCoreKit
// Swift wrapper against the live sdr-ffi static library.
//
// These run `swift test` and require `libsdr_ffi.a` to have
// been built by cargo first. The repo-root Makefile has a
// `make swift-test` target that orders the two correctly.

import XCTest
@testable import SdrCoreKit

final class SdrCoreTests: XCTestCase {

    // ==========================================================
    //  ABI version / logging / static helpers
    // ==========================================================

    func testAbiVersionMatchesCurrent() {
        // Lock in that the Swift wrapper parses the packed
        // version from the C side consistently. Current: 0.10
        // (0.2 device enumeration; 0.3 auto-squelch;
        // 0.4 audio routing + recording; 0.5 IQ recording;
        // 0.6 RadioReference; 0.7 advanced demod; 0.8 audio tap;
        // 0.9 network audio sink; 0.10 source selection + network
        // / file config — issues #235, #236.)
        let v = SdrCore.abiVersion
        XCTAssertEqual(v.major, 0)
        XCTAssertEqual(v.minor, 10)
    }

    func testInitLoggingIsIdempotent() {
        // Calling initLogging more than once (even with
        // different levels) must not panic or throw.
        SdrCore.initLogging(minLevel: .info)
        SdrCore.initLogging(minLevel: .debug)
        SdrCore.initLogging(minLevel: .warn)
    }

    // ==========================================================
    //  Lifecycle
    // ==========================================================

    func testCreateAndDestroyWithInMemoryConfig() throws {
        // Empty URL → empty C string → in-memory engine.
        let core = try SdrCore(configPath: nil)
        // Deinit happens at end of scope; destroy joins the
        // dispatcher and detaches the DSP thread.
        _ = core
    }

    func testCreateWithValidConfigPath() throws {
        let tmp = URL(fileURLWithPath: "/tmp/sdr-core-kit-test.json")
        let core = try SdrCore(configPath: tmp)
        _ = core
    }

    // ==========================================================
    //  Commands (argument validation only — no real source
    //  running, so we can't test that the pipeline actually
    //  changes state, but we can prove the throwing wrappers
    //  round-trip error codes correctly)
    // ==========================================================

    func testTuneWithValidFrequency() throws {
        let core = try SdrCore(configPath: nil)
        try core.tune(100_700_000)
    }

    func testTuneRejectsNaN() throws {
        let core = try SdrCore(configPath: nil)
        XCTAssertThrowsError(try core.tune(.nan)) { error in
            guard let sdrError = error as? SdrCoreError else {
                XCTFail("expected SdrCoreError, got \(error)")
                return
            }
            XCTAssertEqual(sdrError.code, .invalidArg)
            XCTAssertTrue(
                sdrError.message.contains("finite"),
                "expected message to mention 'finite', got: \(sdrError.message)"
            )
        }
    }

    func testSetDemodModeRoundTrips() throws {
        let core = try SdrCore(configPath: nil)
        try core.setDemodMode(.wfm)
        try core.setDemodMode(.nfm)
        try core.setDemodMode(.usb)
    }

    func testSetDecimationRejectsNonPowerOfTwo() throws {
        let core = try SdrCore(configPath: nil)
        XCTAssertThrowsError(try core.setDecimation(3)) { error in
            XCTAssertEqual((error as? SdrCoreError)?.code, .invalidArg)
        }
        // 8 is a valid power of two — must succeed.
        try core.setDecimation(8)
    }

    func testSetFftSizeRejectsNonPowerOfTwo() throws {
        let core = try SdrCore(configPath: nil)
        XCTAssertThrowsError(try core.setFftSize(1000)) { error in
            XCTAssertEqual((error as? SdrCoreError)?.code, .invalidArg)
        }
        try core.setFftSize(2048)
    }

    // ==========================================================
    //  Event stream
    // ==========================================================

    func testEventStreamDeliversStopSignalOnShutdown() async throws {
        // The AsyncStream finishes (via `continuation.finish()`
        // in deinit) when the SdrCore is destroyed. This test
        // verifies the for-await loop exits cleanly in that
        // case rather than hanging forever.
        //
        // Important: Swift's `_ = core` pattern does NOT release
        // a `let`-bound object — the variable stays in scope
        // until the end of the enclosing function, so deinit
        // doesn't fire and the for-await loop hangs. We use a
        // nested `do { ... }` block to create a real scope that
        // bounds the object's lifetime.
        //
        // We also bound the drain task with a timeout via
        // `withTimeout` below so a future regression in the
        // deinit path fails the test within a couple of
        // seconds instead of hanging indefinitely.

        let drainTask: Task<Int, Never>
        do {
            let core = try SdrCore(configPath: nil)
            let events = core.events
            drainTask = Task {
                var count = 0
                for await _ in events {
                    count += 1
                    if count > 1000 {
                        break // defensive cap
                    }
                }
                return count
            }

            // Give the dispatcher a moment to process any
            // startup events.
            try await Task.sleep(nanoseconds: 50_000_000)
            // `core` goes out of scope at end of this `do` block
            // → ARC releases the last strong reference → deinit
            // → continuation.finish() → for-await exits.
        }

        // Bound the wait so a regression in the lifecycle path
        // fails loudly within a couple of seconds instead of
        // hanging the whole test suite.
        let count = try await withTimeout(seconds: 3) {
            await drainTask.value
        }
        XCTAssertGreaterThanOrEqual(count, 0)
    }

    // ==========================================================
    //  FFT pull
    // ==========================================================

    func testWithLatestFftFrameReturnsFalseOnFreshEngine() throws {
        // A fresh engine has never produced an FFT frame, so
        // the pull returns false without calling the closure.
        let core = try SdrCore(configPath: nil)
        var called = false
        let got = core.withLatestFftFrame { _, _, _ in
            called = true
        }
        XCTAssertFalse(got)
        XCTAssertFalse(called)
    }

    // ==========================================================
    //  Error message round-trip
    // ==========================================================

    // ==========================================================
    //  Source selection (ABI 0.10, issues #235, #236)
    // ==========================================================

    func testSetSourceTypeRoundTripsAllVariants() throws {
        let core = try SdrCore(configPath: nil)
        for t in SourceType.allCases {
            try core.setSourceType(t)
        }
    }

    func testSetNetworkConfigAcceptsValidInput() throws {
        let core = try SdrCore(configPath: nil)
        try core.setNetworkConfig(hostname: "127.0.0.1", port: 1234, protocol: .tcp)
        try core.setNetworkConfig(hostname: "iq.example.com", port: 9000, protocol: .udp)
    }

    func testSetNetworkConfigRejectsEmptyHostAndZeroPort() throws {
        let core = try SdrCore(configPath: nil)
        XCTAssertThrowsError(
            try core.setNetworkConfig(hostname: "", port: 1234, protocol: .tcp)
        ) { error in
            XCTAssertEqual((error as? SdrCoreError)?.code, .invalidArg)
        }
        XCTAssertThrowsError(
            try core.setNetworkConfig(hostname: "127.0.0.1", port: 0, protocol: .tcp)
        ) { error in
            XCTAssertEqual((error as? SdrCoreError)?.code, .invalidArg)
        }
    }

    func testSetFilePathAcceptsValidPath() throws {
        let core = try SdrCore(configPath: nil)
        try core.setFilePath("/tmp/some-iq.wav")
    }

    func testSetFilePathRejectsEmpty() throws {
        let core = try SdrCore(configPath: nil)
        XCTAssertThrowsError(try core.setFilePath("")) { error in
            XCTAssertEqual((error as? SdrCoreError)?.code, .invalidArg)
        }
    }

    // ==========================================================
    //  Network audio sink (ABI 0.9, issue #247)
    // ==========================================================

    func testSetAudioSinkTypeRoundTrips() throws {
        let core = try SdrCore(configPath: nil)
        try core.setAudioSinkType(.local)
        try core.setAudioSinkType(.network)
        try core.setAudioSinkType(.local)
    }

    func testSetNetworkSinkConfigAcceptsValidInput() throws {
        let core = try SdrCore(configPath: nil)
        try core.setNetworkSinkConfig(hostname: "127.0.0.1", port: 1234, protocol: .tcpServer)
        try core.setNetworkSinkConfig(hostname: "localhost", port: 9000, protocol: .udp)
    }

    func testSetNetworkSinkConfigRejectsEmptyHostname() throws {
        let core = try SdrCore(configPath: nil)
        XCTAssertThrowsError(
            try core.setNetworkSinkConfig(hostname: "", port: 1234, protocol: .tcpServer)
        ) { error in
            XCTAssertEqual((error as? SdrCoreError)?.code, .invalidArg)
        }
    }

    /// Switching to the network sink on a stopped engine must
    /// not fabricate an `.active` status — the engine only
    /// emits `.active` after `audio_sink.start()` succeeds with
    /// the engine running. Before then the status is either
    /// `.inactive` (initial state) or `.error(...)` (startup
    /// failure). We subscribe to the event stream, flip the
    /// sink type, and assert no `.active` arrives. Per
    /// `CodeRabbit` round 1 on PR #352.
    func testSwitchingSinkToNetworkOnStoppedEngineDoesNotEmitActive() async throws {
        // Same lifetime trick as `testEventStreamDelivers…`:
        // launch the drain task up top, bound the engine's
        // strong reference inside a `do { }` block so its
        // deinit (and thus the stream's `finish()`) fires
        // when the block ends. Awaiting `drainTask.value`
        // outside the block then returns cleanly once the
        // stream terminates, instead of blocking forever on
        // a live engine that isn't producing events.
        let drainTask: Task<[NetworkSinkStatus], Never>
        do {
            let core = try SdrCore(configPath: nil)
            let events = core.events

            drainTask = Task {
                var collected: [NetworkSinkStatus] = []
                for await event in events {
                    if case .networkSinkStatus(let s) = event {
                        collected.append(s)
                    }
                    // Defensive cap: terminate if the controller
                    // ever emits an unreasonable flood.
                    if collected.count > 32 { break }
                }
                return collected
            }

            try core.setNetworkSinkConfig(hostname: "127.0.0.1", port: 1234, protocol: .tcpServer)
            try core.setAudioSinkType(.network)

            // Small window for the controller to react before
            // we drop the engine.
            try await Task.sleep(nanoseconds: 150_000_000)
        }
        // Engine dropped above → stream finishes → drain exits.
        // Bound the wait so a regression can't hang the suite.
        let statuses = try await withTimeout(seconds: 3) {
            await drainTask.value
        }

        // Engine was never started, so no `.active` event
        // should fire. `.inactive` / `.error` are fine —
        // we only guard against a bogus `.active`.
        for status in statuses {
            if case .active = status {
                XCTFail("unexpected .active status on stopped engine: \(statuses)")
            }
        }
    }

    func testErrorMessageIsCopiedIntoOwnedString() throws {
        let core = try SdrCore(configPath: nil)
        do {
            try core.tune(.nan)
            XCTFail("expected throw")
        } catch let error as SdrCoreError {
            // The message is copied into a Swift String by
            // SdrCoreError.fromCurrentError, so it survives
            // any subsequent FFI call that would overwrite
            // the thread-local last-error buffer.
            let msg = error.message

            // Make an unrelated FFI call that writes a
            // different error message.
            XCTAssertThrowsError(try core.setDecimation(3))

            // The original message must still be intact.
            XCTAssertTrue(
                msg.contains("finite"),
                "expected original tune error to survive subsequent FFI call, got: \(msg)"
            )
        }
    }
}

// MARK: - Test helpers

/// Error thrown when a bounded operation exceeds its deadline.
struct TestTimeoutError: Error, CustomStringConvertible {
    let seconds: Double
    var description: String { "test operation exceeded \(seconds)s timeout" }
}

/// Bound an async operation with a wall-clock timeout so a
/// lifecycle bug can't hang the entire test run. If `operation`
/// doesn't complete within `seconds`, throws `TestTimeoutError`.
///
/// Used by tests that wait on AsyncStream / AsyncSequence values
/// whose completion depends on a `deinit`-triggered signal — a
/// future regression that breaks the deinit path fails within
/// the bounded window instead of hanging forever.
func withTimeout<T: Sendable>(
    seconds: Double,
    operation: @Sendable @escaping () async -> T
) async throws -> T {
    try await withThrowingTaskGroup(of: T.self) { group in
        group.addTask {
            await operation()
        }
        group.addTask {
            try await Task.sleep(nanoseconds: UInt64(seconds * 1_000_000_000))
            throw TestTimeoutError(seconds: seconds)
        }
        // Whichever task finishes first wins; cancel the rest.
        guard let first = try await group.next() else {
            throw TestTimeoutError(seconds: seconds)
        }
        group.cancelAll()
        return first
    }
}
