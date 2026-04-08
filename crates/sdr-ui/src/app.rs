//! Application setup — creates the `AdwApplication` and connects signals.

use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

use crate::{css, window};

/// Application ID for the SDR-RS application.
const APP_ID: &str = "com.sdr.rs";

/// Build and return the `AdwApplication` (caller runs it).
pub fn build_app() -> adw::Application {
    let app = adw::Application::builder().application_id(APP_ID).build();

    // Register notification click action before any notifications are sent.
    crate::notify::register_actions(&app);

    app.connect_startup(|_| {
        css::load_css();

        // Register the local data/ directory as an icon search path so the
        // app icon resolves in uninstalled dev builds (not just after `make install`).
        if let Some(display) = gtk4::gdk::Display::default() {
            let icon_theme = gtk4::IconTheme::for_display(&display);
            icon_theme.add_search_path("data");
        }

        // Periodically trim the glibc heap to return freed pages to the OS.
        // Registered in startup (not activate) since activate can fire
        // multiple times on re-activation.
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        glib::timeout_add_local(Duration::from_secs(10), || {
            #[allow(unsafe_code)]
            unsafe {
                unsafe extern "C" {
                    fn malloc_trim(pad: usize) -> i32;
                }
                malloc_trim(0);
            }
            glib::ControlFlow::Continue
        });

        tracing::info!("sdr-rs UI starting");
    });

    app.connect_activate(|app| {
        window::build_window(app);
    });

    app
}
