//! Application setup — creates the `AdwApplication` and connects signals.

use gtk4::prelude::*;
use libadwaita as adw;

use crate::{css, window};

/// Application ID for the SDR-RS application.
const APP_ID: &str = "com.sdr.rs";

/// Build and return the `AdwApplication` (caller runs it).
pub fn build_app() -> adw::Application {
    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        css::load_css();
        tracing::info!("sdr-rs UI starting");
    });

    app.connect_activate(|app| {
        window::build_window(app);
    });

    app
}
