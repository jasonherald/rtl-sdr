//
// CoreModelTests.swift — smoke tests for the observable model's
// bootstrap path.

import XCTest
@testable import SDRMac

@MainActor
final class CoreModelTests: XCTestCase {
    func testDefaultsAreSensible() {
        let m = CoreModel()
        XCTAssertFalse(m.isRunning)
        XCTAssertNil(m.lastError)
        XCTAssertEqual(m.centerFrequencyHz, 100_700_000)
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
}
