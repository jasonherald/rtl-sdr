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

/// One entry in an activity bar. Used as the single source of truth
/// for the activity's identity, icon, accessible/tooltip label, and
/// keyboard shortcut — any consumer that needs one piece of this
/// metadata derives the rest from the entry so the activity bar,
/// keyboard-shortcut registration, help dialog, and (future) config
/// persistence never drift out of sync.
#[derive(Clone, Copy, Debug)]
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
    /// Keyboard-shortcut label shown in the tooltip and help dialog
    /// (e.g. `"Ctrl+1"`). Human-readable sibling of [`accelerator`].
    ///
    /// [`accelerator`]: Self::accelerator
    pub shortcut_label: &'static str,
    /// `GtkShortcutTrigger::parse_string` input for this entry's
    /// keyboard binding (e.g. `"<Ctrl>1"`, `"<Ctrl><Shift>1"`). The
    /// machine-readable sibling of [`shortcut_label`].
    ///
    /// [`shortcut_label`]: Self::shortcut_label
    pub accelerator: &'static str,
}

/// Canonical left-activity-bar entries — sub-tickets #422–#426 swap
/// the `name` entries' stack children for real panels, but the
/// order, names, icons, and keyboard bindings defined here are the
/// single source of truth. `shortcuts.rs` iterates this slice to
/// register `Ctrl+N` bindings AND to generate the help-dialog
/// Navigation rows; changing an entry here propagates to both.
pub const LEFT_ACTIVITIES: &[ActivityBarEntry] = &[
    ActivityBarEntry {
        name: "general",
        icon_name: "go-home-symbolic",
        display_name: "General",
        shortcut_label: "Ctrl+1",
        accelerator: "<Ctrl>1",
    },
    ActivityBarEntry {
        name: "radio",
        icon_name: "audio-input-microphone-symbolic",
        display_name: "Radio",
        shortcut_label: "Ctrl+2",
        accelerator: "<Ctrl>2",
    },
    ActivityBarEntry {
        name: "audio",
        icon_name: "audio-speakers-symbolic",
        display_name: "Audio",
        shortcut_label: "Ctrl+3",
        accelerator: "<Ctrl>3",
    },
    ActivityBarEntry {
        name: "display",
        icon_name: "video-display-symbolic",
        display_name: "Display",
        shortcut_label: "Ctrl+4",
        accelerator: "<Ctrl>4",
    },
    ActivityBarEntry {
        name: "scanner",
        icon_name: "media-seek-forward-symbolic",
        display_name: "Scanner",
        shortcut_label: "Ctrl+5",
        accelerator: "<Ctrl>5",
    },
];

/// Canonical right-activity-bar entries. Single entry today; the
/// slice-based API future-proofs the pattern for Recordings /
/// Event log / etc. Consumers derive shortcut registration and help
/// rows from this — same single-source-of-truth contract as
/// [`LEFT_ACTIVITIES`].
pub const RIGHT_ACTIVITIES: &[ActivityBarEntry] = &[ActivityBarEntry {
    name: "transcript",
    icon_name: "user-available-symbolic",
    display_name: "Transcript",
    shortcut_label: "Ctrl+Shift+1",
    accelerator: "<Ctrl><Shift>1",
}];

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
    use std::collections::HashSet;

    /// Every activity entry's invariants the consumers rely on:
    /// `name`/`display_name`/`icon_name`/`accelerator` all non-empty,
    /// `shortcut_label` non-empty, and each field reasonably formed.
    fn assert_entry_well_formed(entry: &ActivityBarEntry) {
        assert!(!entry.name.is_empty(), "name empty: {entry:?}");
        assert!(
            entry.name == entry.name.to_lowercase(),
            "name must be lowercase: {}",
            entry.name
        );
        assert!(
            !entry.icon_name.is_empty(),
            "icon_name empty for {}",
            entry.name
        );
        assert!(
            !entry.display_name.is_empty(),
            "display_name empty for {}",
            entry.name
        );
        assert!(
            !entry.shortcut_label.is_empty(),
            "shortcut_label empty for {}",
            entry.name
        );
        assert!(
            !entry.accelerator.is_empty(),
            "accelerator empty for {}",
            entry.name
        );
    }

    /// Stack-child names are used as config-persistence keys
    /// (design doc §5) and as visible-child identifiers in
    /// `GtkStack`; duplicates would silently collapse stack
    /// children and corrupt saved expansion state on the next
    /// launch.
    fn assert_names_unique(entries: &[ActivityBarEntry]) {
        let mut seen: HashSet<&'static str> = HashSet::new();
        for entry in entries {
            assert!(
                seen.insert(entry.name),
                "duplicate name in activity list: {}",
                entry.name
            );
        }
    }

    /// `accelerator` and `shortcut_label` are parallel — if one
    /// gets renamed without the other, the tooltip and the
    /// registered binding drift. Enforce a loose contract: both
    /// reference the same modifier + key set.
    fn assert_accelerator_matches_label(entry: &ActivityBarEntry) {
        for fragment in ["Ctrl", "Shift", "Alt"] {
            let label_mentions = entry.shortcut_label.contains(fragment);
            let accelerator_mentions = entry
                .accelerator
                .to_ascii_lowercase()
                .contains(&fragment.to_ascii_lowercase());
            assert_eq!(
                label_mentions, accelerator_mentions,
                "shortcut_label ({}) and accelerator ({}) disagree on {fragment}",
                entry.shortcut_label, entry.accelerator,
            );
        }
    }

    #[test]
    fn left_activities_are_well_formed() {
        assert!(!LEFT_ACTIVITIES.is_empty());
        for entry in LEFT_ACTIVITIES {
            assert_entry_well_formed(entry);
            assert_accelerator_matches_label(entry);
        }
        assert_names_unique(LEFT_ACTIVITIES);
    }

    #[test]
    fn right_activities_are_well_formed() {
        assert!(!RIGHT_ACTIVITIES.is_empty());
        for entry in RIGHT_ACTIVITIES {
            assert_entry_well_formed(entry);
            assert_accelerator_matches_label(entry);
        }
        assert_names_unique(RIGHT_ACTIVITIES);
    }

    #[test]
    fn activity_names_do_not_collide_across_bars() {
        // Left/right stacks are independent, but config-persistence
        // keys (`ui.sidebar.<side>.expanded[<name>][...]`) are
        // namespaced by side. Names colliding across bars would
        // still be technically safe but makes key collisions easy
        // to mistake for an error in code review — keep the two
        // sets disjoint as a belt-and-braces contract.
        let mut combined: HashSet<&'static str> = HashSet::new();
        for entry in LEFT_ACTIVITIES.iter().chain(RIGHT_ACTIVITIES.iter()) {
            assert!(
                combined.insert(entry.name),
                "activity name collides across bars: {}",
                entry.name
            );
        }
    }
}
