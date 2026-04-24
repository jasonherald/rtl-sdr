//! General activity panel — the landing view behind the 🏠 icon.
//!
//! Composes the two most-common control groups (band presets +
//! source) into an `AdwPreferencesPage`. Sections are flat
//! `AdwPreferencesGroup`s — not `AdwExpanderRow`s. The expander
//! pattern gave every group a visible left indent from the list-row
//! inset + another inset from the preferences-group's own margins,
//! which read cluttered once populated; flat groups match the
//! GNOME Settings / iOS Settings section idiom cleanly without the
//! double inset.
//!
//! Bookmarks live behind the right-side Bookmarks activity icon —
//! they're browse/manage controls, not landing-page quick tunes,
//! and the activity-bar separation keeps the left panel focused on
//! "configure this radio" rather than mixing in a bookmark
//! directory. The `rtl_tcp` share-over-network controls have their
//! own left-side Share activity.
//!
//! # Ownership
//!
//! `build_general_panel` reparents child widgets borrowed from the
//! navigation and source panels. GTK auto-unparents on
//! `PreferencesPage::add`, so callers stop being the parent the
//! moment this panel is constructed.

use libadwaita as adw;
use libadwaita::prelude::*;

use super::navigation_panel::NavigationPanel;
use super::source_panel::SourcePanel;

/// Assembled General activity panel.
pub struct GeneralPanel {
    /// Root widget — `AdwPreferencesPage` so the content scrolls
    /// vertically inside the left stack child and inherits the
    /// preferences-page margin/spacing rhythm.
    pub widget: adw::PreferencesPage,
}

/// Build the General activity panel by composing pre-existing
/// preference groups into an `AdwPreferencesPage`.
///
/// The borrowed panels stay functional — this function just
/// reparents their root `AdwPreferencesGroup`s onto the page.
pub fn build_general_panel(navigation: &NavigationPanel, source: &SourcePanel) -> GeneralPanel {
    let page = adw::PreferencesPage::new();
    page.add(&navigation.presets_widget);
    page.add(&source.widget);
    GeneralPanel { widget: page }
}
