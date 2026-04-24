//
// ActivityBarView.swift — narrow icon-only column that selects
// which panel is visible next to it. Mirrors the GTK
// `build_activity_bar` widget from
// `crates/sdr-ui/src/sidebar/activity_bar.rs`.
//
// Generic over `ActivityEntry` so the same view renders both
// the left column (6 entries, `⌘1..6`) and the right column
// (1 entry in scaffolding — `⌘⇧1` — grows to 2 after #448).
//
// Click semantics (mirrors the GTK
// `wire_activity_bar_clicks` doc in `sdr-ui/src/window.rs`):
//
// - Click an unselected icon → selects that activity AND
//   opens its panel.
// - Click the currently-selected icon → toggles the panel
//   open / closed; selection stays put so the icon keeps
//   its highlight even when the panel is collapsed.
//
// Keyboard shortcut semantics match the Linux accelerators:
// the index (1..N) binds to a `KeyEquivalent` of "1".."N" with
// the supplied modifier set (`.command` for left, `.command
// + .shift` for right).

import SwiftUI

struct ActivityBarView<Activity: ActivityEntry>: View {
    /// Which activity is currently "selected" in this column.
    /// Stays stable across open/close — tapping the same icon
    /// twice flips `isOpen` but leaves `selection` alone, so
    /// the icon keeps its active highlight even when the panel
    /// is hidden. This split maps 1:1 onto the Linux
    /// `ui_sidebar_{left,right}_selected` config key for
    /// session persistence in #449. Per `CodeRabbit` round 1
    /// on PR #491.
    @Binding var selection: Activity

    /// Whether the panel next to this bar is currently
    /// visible. Independent of `selection` — closing the
    /// panel doesn't clear which activity was selected.
    /// Maps onto Linux `ui_sidebar_{left,right}_open`.
    @Binding var isOpen: Bool

    /// Modifier set applied to each activity's shortcut.
    /// `.command` for the left column, `[.command, .shift]`
    /// for the right — matches the Linux accelerator scheme.
    let shortcutModifiers: EventModifiers

    /// Static column width. Matches the GTK 48 px icon strip —
    /// wide enough for a 24 pt SF Symbol + comfortable padding,
    /// narrow enough that it doesn't steal space from the
    /// panels it flanks.
    static var columnWidth: CGFloat { 44 }

    var body: some View {
        VStack(spacing: 4) {
            ForEach(Array(Activity.allCases), id: \.self) { activity in
                ActivityBarButton(
                    activity: activity,
                    isSelected: selection == activity,
                    isPanelOpen: selection == activity && isOpen,
                    shortcutModifiers: shortcutModifiers,
                    onTap: { tap(activity) }
                )
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 6)
        .frame(width: Self.columnWidth)
        .background(Color(nsColor: .underPageBackgroundColor))
    }

    /// Click-handler semantics — matches the GTK
    /// `wire_activity_bar_clicks` comment in
    /// `sdr-ui/src/window.rs`:
    ///
    /// - Tapping the **currently-selected** icon toggles the
    ///   panel open/closed. Selection stays put so the icon
    ///   remains visually active.
    /// - Tapping a **different** icon switches selection AND
    ///   opens the panel (matching Linux "different-button
    ///   swaps stack + opens panel").
    private func tap(_ activity: Activity) {
        if selection == activity {
            isOpen.toggle()
        } else {
            selection = activity
            isOpen = true
        }
    }
}

/// Single icon-only button in the activity bar. Extracted so
/// the `.keyboardShortcut` modifier and the selection-state
/// visual treatment live in one place.
private struct ActivityBarButton<Activity: ActivityEntry>: View {
    let activity: Activity
    /// `true` when this is the column's currently-selected
    /// activity. Persists across open/close so the icon stays
    /// visually highlighted even when the panel is closed —
    /// matches the Linux activity-bar contract.
    let isSelected: Bool
    /// `true` only when this activity is selected AND its
    /// panel is open. Drives the `.isSelected` accessibility
    /// trait so VoiceOver announces "selected" only for the
    /// active panel, not for a remembered-but-collapsed
    /// selection.
    let isPanelOpen: Bool
    let shortcutModifiers: EventModifiers
    let onTap: () -> Void

    var body: some View {
        Button(action: onTap) {
            Image(systemName: activity.systemImage)
                .font(.system(size: 18, weight: .regular))
                .frame(width: 36, height: 32)
                .background(
                    RoundedRectangle(cornerRadius: 6)
                        .fill(isSelected
                              ? Color.accentColor.opacity(0.22)
                              : Color.clear)
                )
                .foregroundStyle(isSelected ? Color.accentColor : .primary)
        }
        .buttonStyle(.plain)
        // Pass the shortcut as an optional `KeyboardShortcut?`
        // so an out-of-range `shortcutIndex` (>9) registers no
        // shortcut at all rather than silently binding the Tab
        // key. Per `CodeRabbit` round 2 on PR #491.
        .keyboardShortcut(keyboardShortcut)
        .help(helpText)
        .accessibilityLabel(helpText)
        // Surface the active state to VoiceOver. The visual
        // highlight (tint + background) tells sighted users
        // which panel is active; the `.isSelected` trait
        // tells screen readers the same thing. Per `CodeRabbit`
        // round 1 on PR #491.
        .accessibilityAddTraits(isPanelOpen ? .isSelected : [])
    }

    /// `KeyboardShortcut?` for this button. Returns `nil` when
    /// `shortcutIndex` falls outside `1...9`, which causes
    /// `.keyboardShortcut(_:)` to register no shortcut at all
    /// (the optional API form is the documented "no shortcut"
    /// path; passing a real `KeyEquivalent` like `.tab` as a
    /// fallback would actually bind the Tab key, which is the
    /// trap CodeRabbit caught).
    ///
    /// All current activity enums stay within 1...9, so this
    /// branch is a defensive guard for a future enum that adds
    /// a 10th entry without expanding the digit table.
    private var keyboardShortcut: KeyboardShortcut? {
        let key: KeyEquivalent
        switch activity.shortcutIndex {
        case 1: key = "1"
        case 2: key = "2"
        case 3: key = "3"
        case 4: key = "4"
        case 5: key = "5"
        case 6: key = "6"
        case 7: key = "7"
        case 8: key = "8"
        case 9: key = "9"
        default: return nil
        }
        return KeyboardShortcut(key, modifiers: shortcutModifiers)
    }

    /// Tooltip + accessibility label. Falls back to just the
    /// activity label when `keyboardShortcut` is `nil`, so an
    /// out-of-range `shortcutIndex` doesn't show a phantom
    /// `⌘10` glyph that wouldn't actually fire. Per
    /// `CodeRabbit` round 3 on PR #491.
    private var helpText: String {
        guard keyboardShortcut != nil else {
            return activity.label
        }
        let shortcut = formatShortcut(
            index: activity.shortcutIndex,
            modifiers: shortcutModifiers
        )
        return "\(activity.label) (\(shortcut))"
    }

    private func formatShortcut(index: Int, modifiers: EventModifiers) -> String {
        var s = ""
        if modifiers.contains(.control) { s += "⌃" }
        if modifiers.contains(.option)  { s += "⌥" }
        if modifiers.contains(.shift)   { s += "⇧" }
        if modifiers.contains(.command) { s += "⌘" }
        s += "\(index)"
        return s
    }
}
