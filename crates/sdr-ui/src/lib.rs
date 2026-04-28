//! GTK4 + libadwaita UI for sdr-rs.
//!
//! This crate is **Linux-only**. It depends on the GTK4 stack (`gtk4`,
//! `libadwaita`, `cairo-rs`) which we don't ship on macOS or Windows. The
//! macOS frontend lives in `apps/macos/` (`SwiftUI`) and consumes the engine
//! through the planned `sdr-core` + `sdr-ffi` crates instead.
//!
//! On non-Linux targets the crate compiles to an empty rlib so that
//! `cargo build --workspace` works on every platform without surprising
//! `pkg-config` / system-library failures.

#![cfg(target_os = "linux")]

pub mod antenna;
pub mod app;
pub mod apt_viewer;
pub mod autostart;
pub mod css;
pub mod doppler_tracker;
pub mod header;
pub mod lrpt_viewer;
pub mod notify;
pub mod preferences;
pub mod radioreference;
pub mod shortcuts;
pub mod sidebar;
pub mod spectrum;
pub mod state;
pub mod status_bar;
pub mod ui_helpers;
pub mod viewer;
pub mod window;

// Re-exports from sdr-core for backward compat with internal modules.
// `messages` and `wav_writer` used to live under `crate::` directly; the
// re-exports keep `crate::messages::UiToDsp` and friends resolving without
// touching every panel/state/spectrum file in this PR.
pub use sdr_core::messages;
pub use sdr_core::wav_writer;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

/// Build the `AdwApplication` without running it.
///
/// Separated from [`run`] so `main.rs` can perform an early
/// single-instance D-Bus probe (see [`register_and_check_primary`])
/// before doing any expensive startup work like the sherpa-onnx init.
pub fn build_app() -> adw::Application {
    app::build_app()
}

/// Build the `AdwApplication`, optionally suppressing the initial
/// `window.present()` so the app launches into the tray. Used by
/// `main.rs` to forward the `--start-hidden` CLI flag through to
/// [`app::build_app_with_options`]. Per #512.
pub fn build_app_with_options(start_hidden: bool) -> adw::Application {
    app::build_app_with_options(start_hidden)
}

/// Register the application on the session bus and determine whether
/// we're the primary instance.
///
/// Returns `true` if this process is the primary and should continue
/// starting up normally, or `false` if another sdr-rs process is
/// already running — in which case the primary has been asked to
/// activate its existing window and this process should exit 0.
///
/// Must be called before any heavy startup work so a secondary
/// instance doesn't download the sherpa bundle, open the RTL-SDR
/// device, or spawn a splash subprocess before bailing.
pub fn register_and_check_primary(app: &adw::Application) -> bool {
    if let Err(e) = app.register(gio::Cancellable::NONE) {
        tracing::warn!(
            "failed to register application on the session bus: {e}. \
             Single-instance enforcement disabled for this launch."
        );
        return true;
    }
    if app.is_remote() {
        tracing::info!("another sdr-rs instance is already running — activating it and exiting");
        app.activate();
        return false;
    }
    true
}

/// Run the SDR-RS application, returning the GTK exit code.
///
/// `start_hidden` skips the initial `window.present()` so the app
/// launches with only the tray icon visible — used by the autostart
/// `.desktop` Exec line.
pub fn run(start_hidden: bool) -> glib::ExitCode {
    let app = app::build_app_with_options(start_hidden);
    if !register_and_check_primary(&app) {
        return glib::ExitCode::SUCCESS;
    }
    app.run()
}
