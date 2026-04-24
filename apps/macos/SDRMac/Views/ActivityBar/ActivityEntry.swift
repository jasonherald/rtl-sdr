//
// ActivityEntry.swift — canonical slices for the sidebar activity
// bars on macOS, mirroring the Linux `LEFT_ACTIVITIES` /
// `RIGHT_ACTIVITIES` slices in
// `crates/sdr-ui/src/sidebar/activity_bar.rs`.
//
// Each case's `rawValue` is the persistence key that session
// storage (#449) will consume from the shared `sdr-config`
// JSON — lowercase kebab-case, matching the Linux `name:`
// field exactly. Do not rename without also renaming the
// Linux constants; both frontends read the same file and a
// schema mismatch would wipe the user's sidebar layout on
// cross-frontend launch.
//
// Per epic #441 and sub-ticket #442.

import SwiftUI

/// Protocol shared by `LeftActivity` and `RightActivity` so
/// `ActivityBarView` can render either column with the same
/// code.
protocol ActivityEntry: Hashable, Identifiable, CaseIterable {
    /// Short human-readable label; used as tooltip + accessibility
    /// label on the icon button and as the header of the
    /// corresponding panel.
    var label: String { get }

    /// SF Symbol name for the icon-only activity-bar button.
    /// Chosen to approximate the Linux `icon_name` Symbolic
    /// icon (Adwaita glyphs → SF Symbols is approximate by
    /// necessity; the names below are judgment calls).
    var systemImage: String { get }

    /// 1-based number key for the keyboard shortcut. Left
    /// activities use `⌘1..6`; right activities use `⌘⇧1..2`.
    /// `ActivityBarView` reads this to wire `.keyboardShortcut`.
    var shortcutIndex: Int { get }

    /// Lowercase kebab-case persistence key. Stored as-is in
    /// the shared config JSON that the Linux side also reads.
    /// Same string as `rawValue` below — this accessor exists
    /// so `ActivityBarView` can log / debug without coupling
    /// to the raw-value semantic.
    var persistenceName: String { get }
}

// Default for Int-free `id` on String-raw enums — Swift can't
// synthesize both automatically when the protocol also demands
// `CaseIterable`.
extension ActivityEntry where Self: RawRepresentable, RawValue == String {
    var id: String { rawValue }
    var persistenceName: String { rawValue }
}

/// Left activity bar — six slots, ordered identically to
/// `LEFT_ACTIVITIES` in `activity_bar.rs`. Reordering would
/// drift the `⌘1..6` shortcuts between frontends.
enum LeftActivity: String, CaseIterable, Identifiable, ActivityEntry {
    case general
    case radio
    case audio
    case display
    case scanner
    case share

    var label: String {
        switch self {
        case .general: return "General"
        case .radio:   return "Radio"
        case .audio:   return "Audio"
        case .display: return "Display"
        case .scanner: return "Scanner"
        case .share:   return "Share"
        }
    }

    /// SF Symbol picks approximating the Linux Adwaita icons:
    /// - general → `slider.horizontal.3` (parity settings)
    /// - radio → `antenna.radiowaves.left.and.right`
    /// - audio → `speaker.wave.2`
    /// - display → `waveform`
    /// - scanner → `dot.radiowaves.forward`
    /// - share → `network` (`network-transmit-receive-symbolic`)
    var systemImage: String {
        switch self {
        case .general: return "slider.horizontal.3"
        case .radio:   return "antenna.radiowaves.left.and.right"
        case .audio:   return "speaker.wave.2"
        case .display: return "waveform"
        case .scanner: return "dot.radiowaves.forward"
        case .share:   return "network"
        }
    }

    var shortcutIndex: Int {
        switch self {
        case .general: return 1
        case .radio:   return 2
        case .audio:   return 3
        case .display: return 4
        case .scanner: return 5
        case .share:   return 6
        }
    }
}

/// Right activity bar — transcript + bookmarks. Both panels
/// migrated off their pre-redesign toolbar-toggled flyouts in
/// `#448`; clicking each icon (or pressing `⌘⇧1` / `⌘⇧2`)
/// drives the right-side panel directly, replacing the
/// previous `showingTranscription` / `showingBookmarks` Bool
/// pair on `ContentView`.
///
/// Kept as a separate enum from `LeftActivity` so the compiler
/// enforces that a left selection can't accidentally be used
/// in the right-bar binding (the two columns have different
/// shortcut-modifier sets and different default widths).
enum RightActivity: String, CaseIterable, Identifiable, ActivityEntry {
    case transcript
    case bookmarks

    var label: String {
        switch self {
        case .transcript: return "Transcript"
        case .bookmarks:  return "Bookmarks"
        }
    }

    var systemImage: String {
        switch self {
        case .transcript: return "text.bubble"
        case .bookmarks:  return "bookmark"
        }
    }

    var shortcutIndex: Int {
        switch self {
        case .transcript: return 1
        case .bookmarks:  return 2
        }
    }
}
