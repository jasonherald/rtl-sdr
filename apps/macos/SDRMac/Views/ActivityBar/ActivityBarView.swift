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
// Click semantics:
// - Click an unselected icon → selects that activity, opens
//   its panel.
// - Click the currently-selected icon → clears selection,
//   closes the panel. (Icon stays visible; just the panel
//   slides away.)
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
        .keyboardShortcut(shortcutKey, modifiers: shortcutModifiers)
        .help(helpText)
        .accessibilityLabel(helpText)
        // Surface the active state to VoiceOver. The visual
        // highlight (tint + background) tells sighted users
        // which panel is active; the `.isSelected` trait
        // tells screen readers the same thing. Per `CodeRabbit`
        // round 1 on PR #491.
        .accessibilityAddTraits(isPanelOpen ? .isSelected : [])
    }

    /// Key equivalent for the 1..9 shortcut mapping. Out-of-range
    /// indices degrade to no shortcut rather than crash — the
    /// enums above all stay within 1..9, but belt-and-suspenders
    /// in case a future activity is added past 9.
    private var shortcutKey: KeyEquivalent {
        switch activity.shortcutIndex {
        case 1: return "1"
        case 2: return "2"
        case 3: return "3"
        case 4: return "4"
        case 5: return "5"
        case 6: return "6"
        case 7: return "7"
        case 8: return "8"
        case 9: return "9"
        default: return .tab  // unreachable under current enums
        }
    }

    private var helpText: String {
        // Build the shortcut string Mac-style: ⌘1 / ⌘⇧1.
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
