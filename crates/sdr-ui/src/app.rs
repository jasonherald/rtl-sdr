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

    app.connect_startup(|_| {
        css::load_css();

        // Register the local data/ directory as an icon search path so the
        // app icon resolves in uninstalled dev builds (not just after `make install`).
        if let Some(display) = gtk4::gdk::Display::default() {
            let icon_theme = gtk4::IconTheme::for_display(&display);
            icon_theme.add_search_path("data");
        }

        tracing::info!("sdr-rs UI starting");
    });

    app.connect_activate(|app| {
        window::build_window(app);

        // Periodically trim the glibc heap to return freed pages to the OS.
        // Without this, freed allocations (especially from per-frame FFT clones)
        // accumulate in malloc arenas and RSS grows indefinitely.
        #[cfg(target_os = "linux")]
        glib::timeout_add_local(Duration::from_mins(1), || {
            #[allow(unsafe_code)]
            unsafe {
                unsafe extern "C" {
                    fn malloc_trim(pad: usize) -> i32;
                }
                malloc_trim(0);
            }
            glib::ControlFlow::Continue
        });
    });

    app
}
