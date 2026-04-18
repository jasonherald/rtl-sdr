//
// FrequencyAxisTests.swift — parity fixture for the port of
// `crates/sdr-ui/src/spectrum/frequency_axis.rs`.
//
// Every test name below maps 1:1 to a Rust test in the source
// file; the assertions mirror the Linux `assert_eq!` calls
// verbatim. If either side drifts (new step-candidate added,
// precision changed, threshold tier adjusted) one of these tests
// fails on the next Mac build and the fixture has to be updated
// alongside the Rust side.
//
// See issue #312 — "Snapshot parity test for freq-axis
// formatter". A full shared source (Option B from the issue) is
// the long-term answer if drift becomes a habit; this test is
// the cheap, effective tripwire for now.

import XCTest
@testable import SDRMac

final class FrequencyAxisTests: XCTestCase {

    // ==========================================================
    //  format_frequency parity
    // ==========================================================

    func testFormatHz() {
        // Rust: `format_hz`
        XCTAssertEqual(FrequencyAxis.formatFrequency(455), "455.0 Hz")
        XCTAssertEqual(FrequencyAxis.formatFrequency(0), "0.0 Hz")
    }

    func testFormatKHz() {
        // Rust: `format_khz`
        XCTAssertEqual(FrequencyAxis.formatFrequency(7_055), "7.1 kHz")
        XCTAssertEqual(FrequencyAxis.formatFrequency(1_000), "1.0 kHz")
    }

    func testFormatMHz() {
        // Rust: `format_mhz`
        XCTAssertEqual(FrequencyAxis.formatFrequency(100_000_000), "100.000 MHz")
        XCTAssertEqual(FrequencyAxis.formatFrequency(433_500_000), "433.500 MHz")
        XCTAssertEqual(FrequencyAxis.formatFrequency(7_055_000), "7.055 MHz")
    }

    func testFormatGHz() {
        // Rust: `format_ghz`
        XCTAssertEqual(FrequencyAxis.formatFrequency(1_200_000_000), "1.200 GHz")
        XCTAssertEqual(FrequencyAxis.formatFrequency(2_400_000_000), "2.400 GHz")
    }

    func testFormatNegative() {
        // Rust: `format_negative`
        XCTAssertEqual(FrequencyAxis.formatFrequency(-100_000_000), "-100.000 MHz")
    }

    // ==========================================================
    //  compute_grid_lines parity
    // ==========================================================

    func testGridLinesEmptyOnZeroMax() {
        // Rust: `grid_lines_empty_on_zero_max`
        let lines = FrequencyAxis.computeGridLines(
            startHz: 0, endHz: 1_000_000, maxLines: 0)
        XCTAssertTrue(lines.isEmpty)
    }

    func testGridLinesEmptyOnInvertedRange() {
        // Rust: `grid_lines_empty_on_inverted_range`
        let lines = FrequencyAxis.computeGridLines(
            startHz: 1_000_000, endHz: 0, maxLines: 10)
        XCTAssertTrue(lines.isEmpty)
    }

    func testGridLinesReasonableCount() {
        // Rust: `grid_lines_reasonable_count`
        // 2 MHz span, up to 10 lines
        let lines = FrequencyAxis.computeGridLines(
            startHz: 99_000_000, endHz: 101_000_000, maxLines: 10)
        XCTAssertFalse(lines.isEmpty)
        XCTAssertLessThanOrEqual(lines.count, 10)
    }

    func testGridLinesWithinRange() {
        // Rust: `grid_lines_within_range`
        let start = 100_000_000.0
        let end = 102_000_000.0
        let lines = FrequencyAxis.computeGridLines(
            startHz: start, endHz: end, maxLines: 10)
        for (freq, _) in lines {
            XCTAssertGreaterThanOrEqual(freq, start)
            XCTAssertLessThanOrEqual(freq, end)
        }
    }

    func testGridLinesAreSorted() {
        // Rust: `grid_lines_are_sorted`
        let lines = FrequencyAxis.computeGridLines(
            startHz: 88_000_000, endHz: 108_000_000, maxLines: 20)
        for pair in zip(lines, lines.dropFirst()) {
            XCTAssertLessThan(pair.0.0, pair.1.0)
        }
    }

    // ==========================================================
    //  Additional sanity — step-table ordering invariant
    // ==========================================================

    /// Catches accidental reorderings or deletions in
    /// `stepCandidates`. The Rust side is "smallest → largest,
    /// including common SI-ish prefixes" — check the Mac copy
    /// matches that invariant even if individual values change.
    func testStepCandidatesAreMonotonicallyIncreasing() {
        let steps = FrequencyAxis.stepCandidates
        XCTAssertFalse(steps.isEmpty)
        for (prev, next) in zip(steps, steps.dropFirst()) {
            XCTAssertLessThan(prev, next)
        }
        // Catch if someone removes the 1 Hz floor or the 1 GHz
        // ceiling — would change visible label density on small
        // / large spans respectively.
        XCTAssertEqual(steps.first, 1)
        XCTAssertEqual(steps.last, 1_000_000_000)
    }
}
