//
// BookmarkTests.swift — JSON schema regression tests for the
// Bookmark struct.
//
// The on-disk format is shared with the Linux GTK frontend
// (`crates/sdr-ui/src/sidebar/navigation_panel.rs::Bookmark`),
// so silent schema drift between the two sides would break
// cross-frontend round-tripping of `bookmarks.json`. These
// tests guard the wire contract — they encode and decode a
// representative Bookmark and assert that both the legacy
// fields (`agcEnabled: Bool?`) and modern fields
// (`agcType: AgcType?`, JSON key `agc_type`) survive
// intact.

import XCTest
@testable import SDRMac
import SdrCoreKit

final class BookmarkTests: XCTestCase {
    /// Mirrors Linux's `bookmark_full_roundtrip` regression
    /// guard (see `crates/sdr-ui/src/sidebar/navigation_panel.rs`).
    /// Builds a Bookmark with BOTH the legacy `agcEnabled`
    /// boolean and the modern `agcType` enum set, JSON-
    /// encodes, decodes, and asserts both fields come back
    /// unchanged. A serde-shape change on `AgcType` or a
    /// dropped CodingKey would fail this test loudly instead
    /// of silently breaking the shared schema. Per `CodeRabbit`
    /// round 1 on PR #371.
    func testBookmarkDualFieldAgcRoundTrip() throws {
        let original = Bookmark(
            name: "WWV 5 MHz",
            centerFrequencyHz: 5_000_000,
            demodMode: .am,
            bandwidthHz: 10_000,
            squelchEnabled: nil,
            autoSquelchEnabled: nil,
            squelchDb: nil,
            gainDb: 20.0,
            agcEnabled: true,
            volume: nil,
            deemphasis: nil,
            agcType: .software
        )

        let encoder = JSONEncoder()
        let data = try encoder.encode(original)

        // Raw key check — the on-disk spelling must be
        // `agc_type` (snake_case) so the Linux side can
        // decode what Mac wrote.
        let json = try XCTUnwrap(String(data: data, encoding: .utf8))
        XCTAssertTrue(
            json.contains("\"agc_type\""),
            "expected snake_case `agc_type` key in JSON; got: \(json)"
        )
        XCTAssertTrue(
            json.contains("\"agcEnabled\""),
            "expected legacy `agcEnabled` key in JSON; got: \(json)"
        )

        let back = try JSONDecoder().decode(Bookmark.self, from: data)
        XCTAssertEqual(back.agcEnabled, true, "legacy bool must round-trip")
        XCTAssertEqual(back.agcType, .software, "modern enum must round-trip")
        XCTAssertEqual(back.name, original.name)
        XCTAssertEqual(back.centerFrequencyHz, original.centerFrequencyHz)
        XCTAssertEqual(back.demodMode, original.demodMode)
        XCTAssertEqual(back.id, original.id)
    }

    /// Legacy bookmark (pre-#357) contains only `agcEnabled`,
    /// not `agc_type`. Guard that it still decodes without
    /// the new field and `agcType` comes back nil rather
    /// than failing the whole decode.
    func testLegacyBookmarkWithoutAgcTypeDecodes() throws {
        let legacyJson = """
            {
              "id": "11111111-1111-1111-1111-111111111111",
              "name": "Legacy NOAA",
              "updatedAt": 0,
              "centerFrequencyHz": 162550000,
              "agcEnabled": false
            }
            """
        let data = try XCTUnwrap(legacyJson.data(using: .utf8))
        let bm = try JSONDecoder().decode(Bookmark.self, from: data)
        XCTAssertEqual(bm.agcEnabled, false)
        XCTAssertNil(bm.agcType, "pre-#357 bookmarks carry no agc_type; should decode as nil")
        XCTAssertEqual(bm.name, "Legacy NOAA")
    }
}
