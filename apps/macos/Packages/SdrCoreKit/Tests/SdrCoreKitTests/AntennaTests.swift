//
// AntennaTests.swift — anchor tests pinning Mac/Linux parity for
// `Antenna.swift`. Exact mirrors of the fixtures in
// `crates/sdr-ui/src/antenna.rs::tests` — if a Rust constant
// shifts, the matching test here fails loudly. Issue #487.

import XCTest
@testable import SdrCoreKit

final class AntennaTests: XCTestCase {

    // ----------------------------------------------------------
    //  Tolerances + frequency / length fixtures — values match
    //  the Rust test module symbol-for-symbol.
    // ----------------------------------------------------------

    private let floatEpsMeters: Double = 1e-6
    private let atisMatchToleranceMeters: Double = 1e-4

    private let freq100MHz: Double = 100_000_000.0
    private let wavelengthAt100MHzMeters: Double = 2.997_924_58
    private let freqAtis255MHz: Double = 255_000_000.0
    private let halfWaveAtisMeters: Double = 0.587_828
    private let freq2mCenterHz: Double = 146_000_000.0
    private let freq30GHz: Double = 30_000_000_000.0
    private let freqJustBelowFloorHz: Double = 2_999.0
    private let freq1kHz: Double = 1_000.0

    private let freqNoaa19: Double = 137_620_000.0
    private let freqIssVoice: Double = 145_800_000.0
    private let freq70cmSat: Double = 436_500_000.0
    private let freq13cmSat: Double = 2_400_000_000.0
    private let freqFmBroadcast: Double = 88_500_000.0
    private let freqAirband: Double = 124_050_000.0

    // ----------------------------------------------------------
    //  Wavelength / half-wave / quarter-wave anchors
    // ----------------------------------------------------------

    func testWavelengthAt100MHzIsApproxThreeMetres() {
        let w = Antenna.wavelengthMeters(freqHz: freq100MHz)
        XCTAssertNotNil(w)
        XCTAssertEqual(w!, wavelengthAt100MHzMeters, accuracy: floatEpsMeters)
    }

    func testHalfWaveAtAtis255MHzMatchesTicketExample() {
        // Issue #157 quotes ATIS 255 MHz → λ/2 ≈ 58.8 cm.
        // Exact: 299_792_458 / (255_000_000 · 2) = 0.587_828… m.
        let half = Antenna.halfWaveMeters(freqHz: freqAtis255MHz)
        XCTAssertNotNil(half)
        XCTAssertEqual(half!, halfWaveAtisMeters, accuracy: atisMatchToleranceMeters)
    }

    func testQuarterWaveIsHalfOfHalfWave() {
        let half = Antenna.halfWaveMeters(freqHz: freq2mCenterHz)!
        let quarter = Antenna.quarterWaveMeters(freqHz: freq2mCenterHz)!
        XCTAssertEqual(half, 2.0 * quarter, accuracy: floatEpsMeters)
    }

    func testSubFloorFrequenciesReturnNil() {
        // Renderable-floor guard: a mis-tuned value near DC must
        // not blow up the status bar with "λ/2: 149_896 km".
        XCTAssertNil(Antenna.wavelengthMeters(freqHz: 0))
        XCTAssertNil(Antenna.wavelengthMeters(freqHz: -100))
        XCTAssertNil(Antenna.wavelengthMeters(freqHz: freqJustBelowFloorHz))
        XCTAssertNil(Antenna.wavelengthMeters(freqHz: .nan))
        XCTAssertNil(Antenna.wavelengthMeters(freqHz: .infinity))
    }

    func testHalfAndQuarterPropagateNilFromWavelength() {
        XCTAssertNil(Antenna.halfWaveMeters(freqHz: 0))
        XCTAssertNil(Antenna.quarterWaveMeters(freqHz: 0))
    }

    // ----------------------------------------------------------
    //  Length formatter
    // ----------------------------------------------------------

    func testFormatLengthAutoScalesUnits() {
        XCTAssertEqual(Antenna.formatLengthMeters(1.176_25), "1.18 m")
        XCTAssertEqual(Antenna.formatLengthMeters(0.587_8), "58.8 cm")
        XCTAssertEqual(Antenna.formatLengthMeters(0.007), "7.0 mm")
    }

    func testFormatLengthRejectsBadInput() {
        XCTAssertEqual(Antenna.formatLengthMeters(0), "")
        XCTAssertEqual(Antenna.formatLengthMeters(-1), "")
        XCTAssertEqual(Antenna.formatLengthMeters(.nan), "")
        XCTAssertEqual(Antenna.formatLengthMeters(.infinity), "")
    }

    // ----------------------------------------------------------
    //  V-angle suggestion — band table parity
    // ----------------------------------------------------------

    func testSuggestedVAngleNoaaAptIsSat120() {
        let (angle, hint) = Antenna.suggestedVAngle(freqHz: freqNoaa19)
        XCTAssertEqual(angle, 120)
        XCTAssertEqual(hint, "sat")
    }

    func testSuggestedVAngle2mHamIsSat120() {
        let (angle, hint) = Antenna.suggestedVAngle(freqHz: freqIssVoice)
        XCTAssertEqual(angle, 120)
        XCTAssertEqual(hint, "sat")
    }

    func testSuggestedVAngle70cmSatIsSat120() {
        let (angle, hint) = Antenna.suggestedVAngle(freqHz: freq70cmSat)
        XCTAssertEqual(angle, 120)
        XCTAssertEqual(hint, "sat")
    }

    func testSuggestedVAngle13cmSatIsSat120() {
        let (angle, hint) = Antenna.suggestedVAngle(freqHz: freq13cmSat)
        XCTAssertEqual(angle, 120)
        XCTAssertEqual(hint, "sat")
    }

    func testSuggestedVAngleFmBroadcastIsHorizon180() {
        let (angle, hint) = Antenna.suggestedVAngle(freqHz: freqFmBroadcast)
        XCTAssertEqual(angle, 180)
        XCTAssertEqual(hint, "horizon")
    }

    func testSuggestedVAngleAirbandIsHorizon180() {
        let (angle, hint) = Antenna.suggestedVAngle(freqHz: freqAirband)
        XCTAssertEqual(angle, 180)
        XCTAssertEqual(hint, "horizon")
    }

    func testSuggestedVAngleJustOutside2mIsHorizon() {
        // Boundary check: 2 m band is 144..=148 MHz. 149 MHz must
        // fall to the horizon default — guards against `..=` /
        // `..<` drift between the Rust ranges and the Swift
        // ClosedRange.contains().
        let (angle, _) = Antenna.suggestedVAngle(freqHz: 149_000_000.0)
        XCTAssertEqual(angle, 180)
    }

    // ----------------------------------------------------------
    //  formatAntennaLine — full-string parity with Rust
    // ----------------------------------------------------------

    func testFormatAntennaLineCombinesBothValues() {
        // ATIS 255 MHz → λ/2 58.8 cm, λ/4 29.4 cm, V 180° horizon.
        // Byte-for-byte parity with Rust `format_antenna_line`.
        let line = Antenna.formatAntennaLine(freqHz: freqAtis255MHz)
        XCTAssertEqual(line, "λ/2 58.8 cm · λ/4 29.4 cm · V 180° horizon")
    }

    func testFormatAntennaLineReturnsNilBelowFloor() {
        XCTAssertNil(Antenna.formatAntennaLine(freqHz: 0))
        XCTAssertNil(Antenna.formatAntennaLine(freqHz: freq1kHz))
    }

    func testHighFrequencyUhfFormatsInMmRange() {
        // 30 GHz λ/4 ≈ 2.5 mm — exercises the millimetre branch
        // of the formatter through the full `formatAntennaLine`
        // pipeline.
        let line = Antenna.formatAntennaLine(freqHz: freq30GHz)!
        XCTAssertTrue(line.contains("λ/4 2.5 mm"), "line: \(line)")
    }
}
