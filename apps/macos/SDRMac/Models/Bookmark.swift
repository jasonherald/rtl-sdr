//
// Bookmark.swift — saved tuning profile.
//
// Mirrors the GTK `Bookmark` struct in
// `crates/sdr-ui/src/sidebar/navigation_panel.rs` but stays
// Swift-native (separate `bookmarks.json`, not round-tripped
// through sdr-config) — the Linux side also stores bookmarks
// in their own file rather than in the engine config.
//
// All tuning fields are optional. On recall, an absent field
// means "leave that setting alone", which makes it easy to save
// e.g. a "weather band VFO bandwidth" bookmark that doesn't
// override the user's current demod mode.

import Foundation
import SdrCoreKit

// `DemodMode` / `Deemphasis` are `Int32`-raw-value enums defined
// in SdrCoreKit. Swift synthesizes `Codable` automatically for
// `RawRepresentable` enums once conformance is declared — we
// just need the retroactive conformance for Codable-nested
// bookmark fields to work. Nothing stored in the JSON except
// the raw integer.
extension DemodMode: Codable {}
extension Deemphasis: Codable {}

struct Bookmark: Codable, Identifiable, Hashable {
    /// Stable identity for SwiftUI list diffing; randomly
    /// assigned at creation, persisted across runs.
    var id: UUID = UUID()

    /// Human-readable name. If the user doesn't supply one we
    /// format the frequency as the default.
    var name: String

    /// Creation / last-modified timestamp; shown as a relative
    /// date ("2 days ago") in the list for quick disambiguation
    /// when several bookmarks share similar frequencies.
    var updatedAt: Date = Date()

    // MARK: Tuning fields (all optional)

    var centerFrequencyHz: Double?
    var demodMode: DemodMode?
    var bandwidthHz: Double?
    var squelchEnabled: Bool?
    var autoSquelchEnabled: Bool?
    var squelchDb: Float?
    var gainDb: Double?
    var agcEnabled: Bool?
    var volume: Float?
    var deemphasis: Deemphasis?
}
