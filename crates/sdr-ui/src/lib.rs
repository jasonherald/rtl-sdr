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

pub mod app;
pub mod css;
pub mod header;
pub mod notify;
pub mod preferences;
pub mod radioreference;
pub mod shortcuts;
pub mod sidebar;
pub mod spectrum;
pub mod state;
pub mod status_bar;
pub mod ui_helpers;
pub mod window;

// Re-exports from sdr-core for backward compat with internal modules.
// `messages` and `wav_writer` used to live under `crate::` directly; the
// re-exports keep `crate::messages::UiToDsp` and friends resolving without
// touching every panel/state/spectrum file in this PR.
pub use sdr_core::messages;
pub use sdr_core::wav_writer;

use gtk4::glib;
use gtk4::prelude::*;

/// Run the SDR-RS application, returning the GTK exit code.
pub fn run() -> glib::ExitCode {
    app::build_app().run()
}
