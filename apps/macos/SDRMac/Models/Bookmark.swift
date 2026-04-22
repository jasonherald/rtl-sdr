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

    /// Legacy AGC boolean — kept for backward-compat with
    /// pre-#357 bookmarks.json. `true` recalls to hardware
    /// (the old default when it was Bool), `false` to off.
    /// Modern bookmarks also carry the tristate `agcType` below
    /// and the apply path prefers that when present. Same
    /// dual-field pattern the Linux side uses (agc + agc_type)
    /// so cross-frontend schemas stay aligned.
    var agcEnabled: Bool?

    var volume: Float?
    var deemphasis: Deemphasis?

    /// Tristate AGC — Off / Hardware / Software. Takes
    /// precedence over `agcEnabled` on recall when present.
    /// Per issue #357.
    var agcType: SdrCore.AgcType?

    // MARK: Organization

    /// `RadioReference` category label (e.g. "Law Dispatch",
    /// "Fire Tac"). Schema parity with the Linux `Bookmark`
    /// struct's `rr_category` field — a future RadioReference
    /// → bookmark import path on macOS will populate this.
    /// Manually-created bookmarks leave it `nil`, which groups
    /// them under the "Uncategorized" sentinel in the flyout.
    /// Per issue #339 (flyout) / #241 (RadioReference import).
    var rrCategory: String?

    /// JSON key mapping so the on-disk schema matches the
    /// Linux side's snake_case field name (`rr_category`).
    /// Enables a user who runs both frontends to share the
    /// same `bookmarks.json` without round-trip drift.
    enum CodingKeys: String, CodingKey {
        case id
        case name
        case updatedAt
        case centerFrequencyHz
        case demodMode
        case bandwidthHz
        case squelchEnabled
        case autoSquelchEnabled
        case squelchDb
        case gainDb
        case agcEnabled
        case volume
        case deemphasis
        case rrCategory = "rr_category"
        case agcType = "agc_type"
    }
}
