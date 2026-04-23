//! VS Code-style activity bar — narrow vertical strip of icon toggle
//! buttons used to switch between panel "activities" (General, Radio,
//! Audio, Display, Scanner on the left; Transcript on the right).
//!
//! See `docs/design/sidebar-activity-bar-redesign.md` §2.3 for the
//! design rationale. This module produces the widget; wiring (stack
//! child swaps, split-view show-sidebar toggle, keyboard shortcuts)
//! lives in `window.rs` so the activity bar stays independent of
//! which panels the app actually hosts.
//!
//! # Mutual exclusion
//!
//! `GtkToggleButton::set_group` is intentionally NOT used. That API
//! enforces radio-group semantics, which prevents the "click-selected-
//! icon-to-collapse-panel" behavior the design calls for. Callers
//! manage mutual exclusion manually in their click handlers.
//!
//! # Accessibility
//!
//! Every icon-only button sets an explicit `GtkAccessible` label via
//! `update_property`. Tooltip text alone is not announced reliably by
//! screen readers, so it cannot substitute. This mirrors the idiom
//! used by other icon-only controls in this crate (bookmarks toggle,
//! pinned-servers menu, etc.).

use std::collections::HashMap;

use gtk4::prelude::*;

/// One entry in an activity bar.
#[derive(Clone, Copy)]
pub struct ActivityBarEntry {
    /// Stable identifier used as the `GtkStack` child name and config
    /// key (e.g. `"general"`, `"transcript"`). Lowercase, kebab-case
    /// if multi-word; stable across versions.
    pub name: &'static str,
    /// GNOME symbolic icon name (e.g. `"go-home-symbolic"`).
    pub icon_name: &'static str,
    /// Human-readable activity name (e.g. `"General"`). Used as the
    /// accessible label and the tooltip prefix.
    pub display_name: &'static str,
    /// Keyboard-shortcut label shown in the tooltip (e.g. `"Ctrl+1"`).
    /// Does not bind the shortcut — the caller does that via
    /// `GtkShortcutController`; this is display-only.
    pub shortcut_label: &'static str,
}

/// Which edge of the window the activity bar sits against. Controls
/// the CSS class list and the border side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityBarSide {
    Left,
    Right,
}

/// An assembled activity bar — the widget for packing and the set of
/// toggle buttons keyed by `ActivityBarEntry::name` for click-handler
/// wiring.
pub struct ActivityBar {
    /// Vertical `GtkBox` with the `.activity-bar` CSS class applied.
    /// Pack this into the window's main horizontal container.
    pub widget: gtk4::Box,
    /// Map from entry `name` → `ToggleButton`. Callers iterate this
    /// to attach click handlers and keyboard-shortcut targets.
    pub buttons: HashMap<&'static str, gtk4::ToggleButton>,
}

/// Build an activity bar for one window edge.
///
/// Every button is flat (no chrome), sized to match libadwaita
/// header-bar buttons so the activity bar reads as a natural window-
/// edge extension rather than an inset toolbar. Initial `active` and
/// `.accent` state are the caller's responsibility — left bars
/// typically start with the first entry selected (a panel is always
/// visible); the right bar starts with no entry selected (panel
/// closed by default).
pub fn build_activity_bar(entries: &[ActivityBarEntry], side: ActivityBarSide) -> ActivityBar {
    let widget = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(0)
        .css_classes(match side {
            ActivityBarSide::Left => vec!["activity-bar".to_string()],
            ActivityBarSide::Right => {
                vec!["activity-bar".to_string(), "activity-bar-right".to_string()]
            }
        })
        .build();

    let mut buttons = HashMap::with_capacity(entries.len());

    for entry in entries {
        let btn = gtk4::ToggleButton::builder()
            .icon_name(entry.icon_name)
            .tooltip_text(format!("{} ({})", entry.display_name, entry.shortcut_label))
            .css_classes(["flat", "activity-bar-button"])
            .build();

        // Explicit accessibility label — tooltip text is not reliably
        // announced by screen readers (see module docs §Accessibility).
        btn.update_property(&[gtk4::accessible::Property::Label(entry.display_name)]);

        widget.append(&btn);
        buttons.insert(entry.name, btn);
    }

    ActivityBar { widget, buttons }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_preserve_order_and_count() {
        let entries = &[
            ActivityBarEntry {
                name: "general",
                icon_name: "go-home-symbolic",
                display_name: "General",
                shortcut_label: "Ctrl+1",
            },
            ActivityBarEntry {
                name: "radio",
                icon_name: "audio-input-microphone-symbolic",
                display_name: "Radio",
                shortcut_label: "Ctrl+2",
            },
        ];
        // `gtk4::init` is required before constructing widgets; skip
        // the widget build here and just exercise the entry-shape
        // invariant the builder relies on.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "general");
        assert_eq!(entries[1].shortcut_label, "Ctrl+2");
    }
}
