//! GTK4 + libadwaita UI for sdr-rs.

pub mod app;
pub mod css;
pub mod messages;
pub mod state;
pub mod window;

use gtk4::glib;
use gtk4::prelude::*;

/// Run the SDR-RS application, returning the GTK exit code.
pub fn run() -> glib::ExitCode {
    app::build_app().run()
}
