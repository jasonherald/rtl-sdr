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

    // MARK: Scanner participation

    /// Include this bookmark in scanner rotation. `nil` is
    /// treated as `false` (off) — same default as the Linux
    /// `Bookmark.scan_enabled` field, which serializes via
    /// `#[serde(default)]` and so produces the same on-disk
    /// shape (key absent OR `false`) for an existing bookmark
    /// the user hasn't opted in. Per issue #490, mirrors
    /// Linux PR #375.
    var scanEnabled: Bool?

    /// Priority tier — `nil` or `0` is normal rotation, `>= 1`
    /// is priority (the scanner state machine checks priority
    /// channels more often). Schema parity with the Linux
    /// `Bookmark.priority: u8` field. The bookmark-row toggle
    /// in the flyout is a Bool affordance over this — see
    /// `priorityEnabled` below.
    ///
    /// `UInt8?` rather than `Bool?` because the Linux side
    /// already encodes higher tiers (reserved for #365 priority-
    /// interrupt follow-up). Storing the underlying integer
    /// keeps the Mac-saved `bookmarks.json` round-trippable
    /// through the Linux frontend without losing tier
    /// information that a future Linux release might write.
    var priority: UInt8?

    /// `Bool`-valued view of `priority` for the UI toggle.
    /// `true` when `priority >= 1`. Setting to `true` writes
    /// `1` (single non-zero tier the Mac UI surfaces today);
    /// setting to `false` clears to `nil` rather than `0` so
    /// the JSON omits the key for never-configured bookmarks
    /// — matches the Linux `#[serde(default)]` "absent → 0"
    /// shape on round-trip.
    var priorityEnabled: Bool {
        get { (priority ?? 0) >= 1 }
        set { priority = newValue ? 1 : nil }
    }

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
        // Snake_case JSON keys that match the Linux
        // `Bookmark` struct field names exactly so a
        // `bookmarks.json` written by either frontend
        // round-trips through the other unchanged. Per #490.
        case scanEnabled = "scan_enabled"
        case priority
    }
}
