//! GTK4 + libadwaita UI for sdr-rs.

pub mod app;
pub mod css;
pub mod dsp_controller;
pub mod header;
pub mod messages;
pub mod notify;
pub mod preferences;
pub mod shortcuts;
pub mod sidebar;
pub mod spectrum;
pub mod state;
pub mod status_bar;
pub mod wav_writer;
pub mod window;

use gtk4::glib;
use gtk4::prelude::*;

/// Run the SDR-RS application, returning the GTK exit code.
pub fn run() -> glib::ExitCode {
    app::build_app().run()
}
