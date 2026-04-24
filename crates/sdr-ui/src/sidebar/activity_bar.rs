//! VS Code-style activity bar — narrow vertical strip of icon toggle
//! buttons used to switch between panel "activities" (General, Radio,
//! Audio, Display, Scanner, Share on the left; Transcript, Bookmarks
//! on the right).
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
    ActivityBarEntry {
        name: "share",
        icon_name: "network-transmit-receive-symbolic",
        display_name: "Share",
        shortcut_label: "Ctrl+6",
        accelerator: "<Ctrl>6",
    },
];

/// Canonical right-activity-bar entries — Transcript + Bookmarks.
/// Consumers derive shortcut registration and help rows from this
/// single source of truth. Same contract as [`LEFT_ACTIVITIES`].
pub const RIGHT_ACTIVITIES: &[ActivityBarEntry] = &[
    ActivityBarEntry {
        name: "transcript",
        icon_name: "user-available-symbolic",
        display_name: "Transcript",
        shortcut_label: "Ctrl+Shift+1",
        accelerator: "<Ctrl><Shift>1",
    },
    ActivityBarEntry {
        name: "bookmarks",
        icon_name: "user-bookmarks-symbolic",
        display_name: "Bookmarks",
        shortcut_label: "Ctrl+Shift+2",
        accelerator: "<Ctrl><Shift>2",
    },
];

// ─── Session persistence (#428) ───────────────────────────────
//
// Four config keys carry the across-restart activity-bar state.
// `width_px` is a separate concern that lands with sub-ticket
// #429 (resize handles). The `expanded.<panel>.<section>` keys
// the design doc envisaged are moot — the panels shipped with
// flat `AdwPreferencesGroup`s instead of `AdwExpanderRow`s, so
// there is no per-section collapse state to remember.

/// Config key for the `name` of the currently selected left
/// activity. Values must be one of [`LEFT_ACTIVITIES`]`.name`.
pub const KEY_LEFT_SELECTED: &str = "ui_sidebar_left_selected";
/// Config key for the left split-view's open/closed state.
pub const KEY_LEFT_OPEN: &str = "ui_sidebar_left_open";
/// Config key for the left panel's pixel width (persisted on
/// resize drag-end). The saved value is stored as pixels per the
/// user-facing contract in #429; `window.rs` converts back to an
/// `AdwOverlaySplitView` fraction once the split view has a real
/// allocation.
pub const KEY_LEFT_WIDTH_PX: &str = "ui_sidebar_left_width_px";
/// Config key for the `name` of the currently selected right
/// activity. Values must be one of [`RIGHT_ACTIVITIES`]`.name`.
pub const KEY_RIGHT_SELECTED: &str = "ui_sidebar_right_selected";
/// Config key for the right split-view's open/closed state.
pub const KEY_RIGHT_OPEN: &str = "ui_sidebar_right_open";
/// Config key for the right panel's pixel width. Same conversion
/// contract as [`KEY_LEFT_WIDTH_PX`].
pub const KEY_RIGHT_WIDTH_PX: &str = "ui_sidebar_right_width_px";

/// Default left selection on a fresh install. Must match an entry
/// in [`LEFT_ACTIVITIES`].
pub const DEFAULT_LEFT_SELECTED: &str = "general";
/// Default left-panel open state on a fresh install.
pub const DEFAULT_LEFT_OPEN: bool = true;
/// Default right selection on a fresh install. Must match an
/// entry in [`RIGHT_ACTIVITIES`].
pub const DEFAULT_RIGHT_SELECTED: &str = "transcript";
/// Default right-panel open state on a fresh install — closed,
/// per the design doc's "Transcript opt-in" note.
pub const DEFAULT_RIGHT_OPEN: bool = false;

/// Persisted activity-bar state, loaded once at launch by
/// [`load_session`] and applied before the window is presented.
/// Selection + open state resolve to concrete values even on a
/// fresh install; width fields stay `Option`-wrapped because the
/// builder-time default (a fraction of the split view's allocated
/// width) is a good answer in the absence of a user preference,
/// and forcing a pixel value at build time would require knowing
/// the split view's allocation before it exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidebarSession {
    /// Stable `name` from [`LEFT_ACTIVITIES`]. Guaranteed valid
    /// (a stale config naming a removed activity falls back to
    /// [`DEFAULT_LEFT_SELECTED`]).
    pub left_selected: &'static str,
    pub left_open: bool,
    /// Persisted left panel width in pixels. `None` on fresh
    /// install; `window.rs` applies the value after the left
    /// split view's first allocation (fractions are the only
    /// knob `AdwOverlaySplitView` exposes, and those need
    /// allocation width to convert from pixels).
    pub left_width_px: Option<u32>,
    /// Stable `name` from [`RIGHT_ACTIVITIES`]. Same validity
    /// guarantee as [`left_selected`](Self::left_selected).
    pub right_selected: &'static str,
    pub right_open: bool,
    /// Persisted right panel width in pixels. Same semantics as
    /// [`left_width_px`](Self::left_width_px).
    pub right_width_px: Option<u32>,
}

impl Default for SidebarSession {
    fn default() -> Self {
        Self {
            left_selected: DEFAULT_LEFT_SELECTED,
            left_open: DEFAULT_LEFT_OPEN,
            left_width_px: None,
            right_selected: DEFAULT_RIGHT_SELECTED,
            right_open: DEFAULT_RIGHT_OPEN,
            right_width_px: None,
        }
    }
}

/// Resolve a persisted activity name against the canonical entry
/// list. Returns the entry's `&'static str` (not the caller's
/// borrowed config value) so downstream code — notably
/// `wire_activity_bar_clicks` — can hold it in a
/// `RefCell<&'static str>` without lifetime headaches. Any stale
/// or typo'd name falls back to `default`.
fn pick_entry_name(
    candidate: Option<&str>,
    entries: &'static [ActivityBarEntry],
    default: &'static str,
) -> &'static str {
    candidate
        .and_then(|c| entries.iter().find(|e| e.name == c).map(|e| e.name))
        .unwrap_or(default)
}

/// Load a persisted pixel width, narrowing JSON `u64` → `u32`
/// and silently falling back to `None` if the value is missing,
/// malformed, or overflows.
fn read_optional_width_px(value: &serde_json::Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// Load the persisted activity-bar session from config, filling
/// missing or malformed fields with the fresh-install defaults.
#[must_use]
pub fn load_session(config: &std::sync::Arc<sdr_config::ConfigManager>) -> SidebarSession {
    config.read(|v| SidebarSession {
        left_selected: pick_entry_name(
            v.get(KEY_LEFT_SELECTED).and_then(serde_json::Value::as_str),
            LEFT_ACTIVITIES,
            DEFAULT_LEFT_SELECTED,
        ),
        left_open: v
            .get(KEY_LEFT_OPEN)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(DEFAULT_LEFT_OPEN),
        left_width_px: read_optional_width_px(v, KEY_LEFT_WIDTH_PX),
        right_selected: pick_entry_name(
            v.get(KEY_RIGHT_SELECTED)
                .and_then(serde_json::Value::as_str),
            RIGHT_ACTIVITIES,
            DEFAULT_RIGHT_SELECTED,
        ),
        right_open: v
            .get(KEY_RIGHT_OPEN)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(DEFAULT_RIGHT_OPEN),
        right_width_px: read_optional_width_px(v, KEY_RIGHT_WIDTH_PX),
    })
}

/// Persist the left-activity selection.
pub fn save_left_selected(config: &std::sync::Arc<sdr_config::ConfigManager>, name: &str) {
    config.write(|v| {
        v[KEY_LEFT_SELECTED] = serde_json::json!(name);
    });
}

/// Persist the left panel's pixel width.
pub fn save_left_width_px(config: &std::sync::Arc<sdr_config::ConfigManager>, px: u32) {
    config.write(|v| {
        v[KEY_LEFT_WIDTH_PX] = serde_json::json!(px);
    });
}

/// Persist the right panel's pixel width.
pub fn save_right_width_px(config: &std::sync::Arc<sdr_config::ConfigManager>, px: u32) {
    config.write(|v| {
        v[KEY_RIGHT_WIDTH_PX] = serde_json::json!(px);
    });
}

/// Persist the left panel open/closed state.
pub fn save_left_open(config: &std::sync::Arc<sdr_config::ConfigManager>, open: bool) {
    config.write(|v| {
        v[KEY_LEFT_OPEN] = serde_json::json!(open);
    });
}

/// Persist the right-activity selection.
pub fn save_right_selected(config: &std::sync::Arc<sdr_config::ConfigManager>, name: &str) {
    config.write(|v| {
        v[KEY_RIGHT_SELECTED] = serde_json::json!(name);
    });
}

/// Persist the right panel open/closed state.
pub fn save_right_open(config: &std::sync::Arc<sdr_config::ConfigManager>, open: bool) {
    config.write(|v| {
        v[KEY_RIGHT_OPEN] = serde_json::json!(open);
    });
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

    // ─── Session persistence ────────────────────────────────

    use std::sync::Arc;

    use sdr_config::ConfigManager;

    fn make_config() -> Arc<ConfigManager> {
        Arc::new(ConfigManager::in_memory(&serde_json::json!({})))
    }

    #[test]
    fn session_defaults_on_fresh_config() {
        let config = make_config();
        let loaded = load_session(&config);
        assert_eq!(loaded, SidebarSession::default());
    }

    #[test]
    fn session_default_agrees_with_canonical_activity_list() {
        // `DEFAULT_LEFT_SELECTED` / `DEFAULT_RIGHT_SELECTED` must
        // resolve against their respective entry lists — a typo
        // would silently fall back to the entry's own default
        // string but then `pick_entry_name` with the stale key
        // would still work because it matches by name. Pin the
        // direct presence here to catch the misspelling earlier.
        assert!(
            LEFT_ACTIVITIES
                .iter()
                .any(|e| e.name == DEFAULT_LEFT_SELECTED),
            "DEFAULT_LEFT_SELECTED not in LEFT_ACTIVITIES"
        );
        assert!(
            RIGHT_ACTIVITIES
                .iter()
                .any(|e| e.name == DEFAULT_RIGHT_SELECTED),
            "DEFAULT_RIGHT_SELECTED not in RIGHT_ACTIVITIES"
        );
    }

    #[test]
    fn session_round_trips_full_state() {
        let config = make_config();
        save_left_selected(&config, "radio");
        save_left_open(&config, false);
        save_left_width_px(&config, 400);
        save_right_selected(&config, "bookmarks");
        save_right_open(&config, true);
        save_right_width_px(&config, 500);
        let loaded = load_session(&config);
        assert_eq!(loaded.left_selected, "radio");
        assert!(!loaded.left_open);
        assert_eq!(loaded.left_width_px, Some(400));
        assert_eq!(loaded.right_selected, "bookmarks");
        assert!(loaded.right_open);
        assert_eq!(loaded.right_width_px, Some(500));
    }

    #[test]
    fn session_missing_widths_resolve_to_none() {
        // Fresh install (or a pre-#429 config with no width keys)
        // loads with `None` so the builder-time fractional default
        // stands and no post-realize callback runs.
        let config = make_config();
        save_left_selected(&config, "general");
        let loaded = load_session(&config);
        assert_eq!(loaded.left_width_px, None);
        assert_eq!(loaded.right_width_px, None);
    }

    #[test]
    fn session_malformed_widths_resolve_to_none() {
        // A hand-edited or upgrade-corrupted config shouldn't
        // crash — `u64` that can't narrow to `u32`, wrong JSON
        // types all become `None`.
        let config = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            KEY_LEFT_WIDTH_PX: "not-a-number",
            KEY_RIGHT_WIDTH_PX: u64::from(u32::MAX) + 1,
        })));
        let loaded = load_session(&config);
        assert_eq!(loaded.left_width_px, None);
        assert_eq!(loaded.right_width_px, None);
    }

    #[test]
    fn session_stale_selection_falls_back_to_default() {
        // A config left behind by an older build (pre-Share)
        // might name an activity we removed. Fall back to the
        // default rather than crash or show a blank stack.
        let config = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            KEY_LEFT_SELECTED: "removed-activity",
            KEY_RIGHT_SELECTED: "also-removed",
        })));
        let loaded = load_session(&config);
        assert_eq!(loaded.left_selected, DEFAULT_LEFT_SELECTED);
        assert_eq!(loaded.right_selected, DEFAULT_RIGHT_SELECTED);
    }

    #[test]
    fn session_malformed_values_fall_back_to_defaults() {
        let config = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            KEY_LEFT_SELECTED: 42,
            KEY_LEFT_OPEN: "not-a-bool",
            KEY_RIGHT_SELECTED: [1, 2, 3],
            KEY_RIGHT_OPEN: "nope",
        })));
        let loaded = load_session(&config);
        assert_eq!(loaded, SidebarSession::default());
    }
}
