//! Navigation panel — band presets and frequency bookmarks.

use libadwaita as adw;

// ---------------------------------------------------------------------------
// Navigation panel widget
// ---------------------------------------------------------------------------

/// Callback type for navigation actions.
///
/// Receives the full `Bookmark` so the handler can restore all tuning-profile
/// settings (squelch, gain, de-emphasis, etc.) in addition to frequency, mode,
/// and bandwidth.
pub type NavigationCallback = Box<dyn Fn(&Bookmark)>;

/// Identity of the currently active bookmark (name + frequency).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActiveBookmark {
    pub name: String,
    pub frequency: u64,
}

/// Navigation panel containing band presets and the left-sidebar
/// "Add Bookmark" quick-entry controls.
///
/// The full bookmark list (browse, recall, delete, save-over-active)
/// lives in [`crate::sidebar::BookmarksPanel`] — the right-side
/// flyout — so this panel intentionally does **not** own the list
/// widget or the backing store. Both panels share the `name_entry`:
/// this struct owns it, the flyout panel borrows it at build time
/// and captures clones in its row callbacks.
pub struct NavigationPanel {
    /// Band presets group widget.
    pub presets_widget: adw::PreferencesGroup,
    /// Left-sidebar bookmark quick-add container (heading +
    /// name entry + Add button). The full list is in the flyout
    /// — see [`crate::sidebar::BookmarksPanel`].
    pub bookmarks_widget: gtk4::Box,
    /// Band preset combo row (for connection in window.rs).
    pub preset_row: adw::ComboRow,
    /// Bookmark name entry (user-editable, defaults to formatted frequency).
    /// Owned here because the Add button sits next to it; the
    /// flyout panel borrows a reference for its row actions.
    pub name_entry: adw::EntryRow,
    /// Add bookmark button. Lives on the left sidebar so users
    /// can stash a bookmark without opening the flyout.
    pub add_button: gtk4::Button,
}

/// Callback type for save actions on the active bookmark.
pub type SaveCallback = std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn()>>>>;

/// Callback invoked whenever the in-memory bookmark list mutates in
/// a way that affects scanner projection (scan checkbox toggled,
/// priority star toggled, row deleted). Window-level wiring installs
/// a closure that re-projects the bookmark list and dispatches
/// `UiToDsp::UpdateScannerChannels`; the callback is `Option`-wrapped
/// so panels can be built standalone (and in tests) without requiring
/// a live `AppState` / `ConfigManager` pair to register it.
pub type BookmarksMutatedCallback = std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn()>>>>;

mod bookmarks;
mod list;
mod panel;
mod scan_projection;

pub use bookmarks::{
    Bookmark, TuningProfile, demod_mode_to_string, format_frequency, load_bookmarks,
    parse_demod_mode, save_bookmarks,
};
pub use list::rebuild_bookmark_list;
pub use panel::{build_navigation_panel, connect_preset_to_bookmarks};
pub use scan_projection::{
    project_and_push_scanner_channels, project_scanner_channels, scanner_channel_envelope,
};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
