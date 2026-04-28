//! Preferences window — application settings organized into pages.

pub mod accounts_page;
pub mod general_page;

use std::rc::Rc;
use std::sync::Arc;

use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;

use crate::state::AppState;

/// Default preferences window width in pixels.
const PREFS_WIDTH: i32 = 600;

/// Default preferences window height in pixels.
const PREFS_HEIGHT: i32 = 500;

/// Build and return the preferences window.
///
/// The window is modal and transient for `parent`.
pub fn build_preferences_window(
    parent: &adw::ApplicationWindow,
    config: &Arc<ConfigManager>,
    state: &Rc<AppState>,
) -> adw::PreferencesWindow {
    let window = adw::PreferencesWindow::builder()
        .title("Preferences")
        .default_width(PREFS_WIDTH)
        .default_height(PREFS_HEIGHT)
        .modal(true)
        .transient_for(parent)
        .build();

    let general_page = general_page::build_general_page(&window, config, state);
    window.add(&general_page);

    let (accounts_page, _has_credentials) = accounts_page::build_accounts_page();
    window.add(&accounts_page);

    window
}
