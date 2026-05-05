//
// PropagationTests.swift — anchor tests pinning Mac/Linux parity
// for `Propagation.swift`. Mirrors the fixtures in
// `crates/sdr-dsp/src/propagation.rs::tests` so drift between
// the two frontends fails loudly. Issue #486.

import XCTest
@testable import SdrCoreKit

final class PropagationTests: XCTestCase {

    private let dbTolerance: Double = 0.01
    private let distanceRelativeTolerance: Double = 1e-9

    // ----------------------------------------------------------
    //  watts ↔ dBm
    // ----------------------------------------------------------

    func testWattsToDbmAnchors() {
        // Reference points every RF engineer has memorised.
        XCTAssertEqual(Propagation.wattsToDbm(1.0), 30.0, accuracy: dbTolerance)
        XCTAssertEqual(Propagation.wattsToDbm(0.001), 0.0, accuracy: dbTolerance)
        XCTAssertEqual(Propagation.wattsToDbm(100.0), 50.0, accuracy: dbTolerance)
    }

    func testWattsDbmRoundTrip() {
        for watts in [0.001, 0.1, 1.0, 5.0, 25.0, 50.0, 100.0, 1000.0] {
            let dbm = Propagation.wattsToDbm(watts)
            let back = Propagation.dbmToWatts(dbm)
            let relErr = abs(back - watts) / watts
            XCTAssertLessThan(
                relErr, 1e-12,
                "watts round-trip failed: \(watts) → \(dbm) dBm → \(back) W (rel err \(relErr))"
            )
        }
    }

    func testWattsToDbmEdgeCases() {
        // Zero or negative input → −∞ ("no transmitter"), which
        // makes downstream fsplDistanceMeters return 0.
        let z = Propagation.wattsToDbm(0)
        XCTAssertTrue(z.isInfinite && z < 0)
        let n = Propagation.wattsToDbm(-1)
        XCTAssertTrue(n.isInfinite && n < 0)
    }

    // ----------------------------------------------------------
    //  fsplDb anchors
    // ----------------------------------------------------------

    func testFsplDbAnchors() {
        // 100 MHz / 1 m: 20·log10(1) = 0, 20·log10(1e8) = 160,
        // FSPL = 0 + 160 - 147.55 = 12.45 dB.
        let lossNear = Propagation.fsplDb(distanceMeters: 1.0, frequencyHz: 100e6)
        XCTAssertEqual(lossNear, 12.45, accuracy: dbTolerance)

        // 1 GHz / 10 km: 20·log10(1e4) = 80, 20·log10(1e9) = 180,
        // FSPL = 80 + 180 - 147.55 = 112.45 dB.
        let lossFar = Propagation.fsplDb(distanceMeters: 10_000.0, frequencyHz: 1e9)
        XCTAssertEqual(lossFar, 112.45, accuracy: dbTolerance)
    }

    // ----------------------------------------------------------
    //  Distance round-trip + scenarios
    // ----------------------------------------------------------

    func testFsplRoundTripDistanceToLossToDistance() {
        // For a range of frequencies and distances, compute the
        // loss, then recover the distance from the loss.
        for freq in [50e6, 155e6, 446e6, 1.575e9, 2.4e9] {
            for d in [1.0, 100.0, 1_000.0, 50_000.0, 1e6] {
                let loss = Propagation.fsplDb(distanceMeters: d, frequencyHz: freq)
                // Treat the path loss as (erp - received) with
                // erp = 0 dBm, so received = -loss dBm.
                let dBack = Propagation.fsplDistanceMeters(
                    erpDbm: 0,
                    receivedDbm: -loss,
                    frequencyHz: freq
                )
                let rel = abs(dBack - d) / d
                XCTAssertLessThan(
                    rel, distanceRelativeTolerance,
                    "round-trip failed at f=\(freq), d=\(d): got \(dBack) (rel err \(rel))"
                )
            }
        }
    }

    func testFsplDistanceRejectsNonPhysicalInputs() {
        // Non-positive frequency → NaN.
        XCTAssertTrue(
            Propagation.fsplDistanceMeters(erpDbm: 30, receivedDbm: -80, frequencyHz: 0).isNaN
        )
        XCTAssertTrue(
            Propagation.fsplDistanceMeters(erpDbm: 30, receivedDbm: -80, frequencyHz: -1).isNaN
        )

        // Non-finite powers → NaN.
        XCTAssertTrue(
            Propagation.fsplDistanceMeters(erpDbm: .nan, receivedDbm: -80, frequencyHz: 100e6).isNaN
        )
        XCTAssertTrue(
            Propagation.fsplDistanceMeters(erpDbm: 30, receivedDbm: .nan, frequencyHz: 100e6).isNaN
        )
        XCTAssertTrue(
            Propagation.fsplDistanceMeters(erpDbm: .infinity, receivedDbm: -80, frequencyHz: 100e6).isNaN
        )

        // Received ≥ transmitted → physically impossible FSPL.
        // Return 0 (calibration issue) rather than negative or
        // NaN distance so the UI degrades gracefully.
        XCTAssertEqual(
            Propagation.fsplDistanceMeters(erpDbm: 30, receivedDbm: 30, frequencyHz: 100e6),
            0,
            accuracy: 1e-12
        )
        XCTAssertEqual(
            Propagation.fsplDistanceMeters(erpDbm: 30, receivedDbm: 40, frequencyHz: 100e6),
            0,
            accuracy: 1e-12
        )
    }

    func testPublicSafetyVhfScenario() {
        // 50 W transmitter at 155 MHz received at -90 dBm —
        // ticket #486's headline scenario. Expected ≈ 1100 km
        // under ideal FSPL; allow 500 km – 2 Mm so the test
        // remains stable under the same tolerance the Rust
        // fixture uses.
        let erp = Propagation.wattsToDbm(50)
        let d = Propagation.fsplDistanceMeters(erpDbm: erp, receivedDbm: -90, frequencyHz: 155e6)
        XCTAssertGreaterThan(d, 500_000)
        XCTAssertLessThan(d, 2_000_000)
    }

    // ----------------------------------------------------------
    //  Distance formatter
    // ----------------------------------------------------------

    func testFormatDistanceAutoScales() {
        XCTAssertEqual(Propagation.formatDistance(0.50), "50.0 cm")
        XCTAssertEqual(Propagation.formatDistance(1.0), "1.0 m")
        XCTAssertEqual(Propagation.formatDistance(50.0), "50.0 m")
        XCTAssertEqual(Propagation.formatDistance(500.0), "500 m")
        XCTAssertEqual(Propagation.formatDistance(1_500.0), "1.5 km")
        XCTAssertEqual(Propagation.formatDistance(15_000.0), "15 km")
        XCTAssertEqual(Propagation.formatDistance(1_100_000.0), "1100 km")
    }

    func testFormatDistanceRejectsBadInput() {
        XCTAssertEqual(Propagation.formatDistance(0), "—")
        XCTAssertEqual(Propagation.formatDistance(-1), "—")
        XCTAssertEqual(Propagation.formatDistance(.nan), "—")
        XCTAssertEqual(Propagation.formatDistance(.infinity), "—")
    }
}
