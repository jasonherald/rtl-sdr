//
// CoreModelTests.swift — smoke tests for the observable model's
// bootstrap path.

import XCTest
@testable import SDRMac
import SdrCoreKit  // DemodMode/Deemphasis/FftWindow referenced below

@MainActor
final class CoreModelTests: XCTestCase {
    func testDefaultsAreSensible() {
        let m = CoreModel()
        XCTAssertFalse(m.isRunning)
        XCTAssertNil(m.lastError)
        // Default center freq aligns with the Rust engine's
        // `DEFAULT_CENTER_FREQ` (100.000 MHz) so Linux / Mac
        // both paint the same tuner state at first launch —
        // see commit 31e7f7f.
        XCTAssertEqual(m.centerFrequencyHz, 100_000_000)
        XCTAssertEqual(m.demodMode, .wfm)
        XCTAssertEqual(m.fftSize, 2048)
    }

    func testOptimisticSetterUpdatesImmediately() {
        let m = CoreModel()
        m.setCenter(88_500_000)
        XCTAssertEqual(m.centerFrequencyHz, 88_500_000)
        m.setBandwidth(12_500)
        XCTAssertEqual(m.bandwidthHz, 12_500)
    }

    func testMinMaxDbAreUIOnly() {
        let m = CoreModel()
        m.setMinDb(-90)
        m.setMaxDb(-10)
        XCTAssertEqual(m.minDb, -90)
        XCTAssertEqual(m.maxDb, -10)
    }

    // ==========================================================
    //  setCenter clamping (guards all tune paths uniformly)
    // ==========================================================

    func testSetCenterClampsBelowZero() {
        let m = CoreModel()
        m.setCenter(-1_000_000)
        XCTAssertEqual(m.centerFrequencyHz, 0)
    }

    func testSetCenterClampsAboveMax() {
        let m = CoreModel()
        m.setCenter(10_000_000_000_000)  // 10 THz — over the cap
        XCTAssertEqual(m.centerFrequencyHz, CoreModel.maxCenterFrequencyHz)
    }

    func testSetCenterIgnoresNonFinite() {
        let m = CoreModel()
        m.setCenter(1_000_000)
        m.setCenter(.infinity)
        // Should leave the previous valid value in place rather
        // than write Inf / NaN.
        XCTAssertEqual(m.centerFrequencyHz, 1_000_000)
        m.setCenter(.nan)
        XCTAssertEqual(m.centerFrequencyHz, 1_000_000)
    }

    func testSetCenterKeepsInRangeValues() {
        let m = CoreModel()
        m.setCenter(88_500_000)
        XCTAssertEqual(m.centerFrequencyHz, 88_500_000)
        m.setCenter(1_700_000_000)  // 1.7 GHz — real RTL-SDR top end
        XCTAssertEqual(m.centerFrequencyHz, 1_700_000_000)
    }
}
