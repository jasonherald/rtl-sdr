//! Application setup — creates the `AdwApplication`, holds it
//! across window-close, spawns the tray sidecar, and routes
//! [`sdr_tray::TrayEvent`]s to GIO actions on the GTK main loop.

use std::sync::mpsc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

use crate::{css, window};

/// Application ID for the SDR-RS application.
const APP_ID: &str = "com.sdr.rs";

/// Build and return the `AdwApplication` (caller runs it).
///
/// Equivalent to `build_app_with_options(false)` — used by callers
/// that don't need to control whether the window is presented at
/// activate time.
pub fn build_app() -> adw::Application {
    build_app_with_options(false)
}

/// Build the `AdwApplication`, optionally suppressing the initial
/// `window.present()`. `start_hidden = true` is used by the autostart
/// `.desktop` Exec line so the app launches into the tray without a
/// visible window.
pub fn build_app_with_options(start_hidden: bool) -> adw::Application {
    let app = adw::Application::builder().application_id(APP_ID).build();

    crate::notify::register_actions(&app);

    app.connect_startup(|_app| {
        css::load_css();

        // Register the local data/ directory as an icon search path
        // so the app icon resolves in uninstalled dev builds (not
        // just after `make install`).
        if let Some(display) = gtk4::gdk::Display::default() {
            let icon_theme = gtk4::IconTheme::for_display(&display);
            icon_theme.add_search_path("data");
        }

        // Default every window the app creates to the app icon.
        // Without this the WM/compositor only ties the main
        // `AdwApplicationWindow` to `com.sdr.rs` (via the matching
        // `application_id`); the satellite-image viewers (APT /
        // LRPT / SSTV) and the ACARS log viewer use bare
        // `adw::Window::builder()` and otherwise show up in the
        // window list / task switcher with the default fallback
        // icon. `set_default_icon_name` covers existing AND
        // future windows without per-builder churn.
        gtk4::Window::set_default_icon_name(APP_ID);

        // Periodically trim the glibc heap to return freed pages
        // to the OS.
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

    let config_path = gtk4::glib::user_config_dir()
        .join("sdr-rs")
        .join("config.json");
    let defaults = serde_json::json!({});
    let config = match sdr_config::ConfigManager::load(&config_path, &defaults) {
        Ok(mut c) => {
            c.enable_auto_save();
            std::sync::Arc::new(c)
        }
        Err(e) => {
            tracing::warn!("config load failed, using in-memory defaults: {e}");
            std::sync::Arc::new(sdr_config::ConfigManager::in_memory(&defaults))
        }
    };

    app.connect_activate(move |app| {
        // Re-activation guard. A second `sdr-rs` process forwarded
        // its activate signal here via GApplication's D-Bus uniqueness
        // plumbing — raise our existing window rather than spawning
        // a second engine + window. Without this guard the primary
        // would accumulate windows on every remote launch and the
        // second window would fight the first for the RTL-SDR USB
        // device.
        if let Some(existing) = app.windows().into_iter().next() {
            existing.present();
            return;
        }

        // `build_window` returns `None` if the DSP engine failed to
        // spawn — in that case it has already called `app.quit()`,
        // so we just bail out of the activate handler. The hold +
        // tray plumbing below would either no-op against an empty
        // window list or try to spawn a tray for a process that's
        // about to exit; both are wasteful.
        let Some(state) = window::build_window(app, &config) else {
            return;
        };

        // Hold the application so it doesn't exit when the last
        // window closes — the tray icon and DSP thread keep us
        // alive in the background. The gio binding returns an
        // RAII guard whose `Drop` calls `release()`, so we stash
        // it on `AppState` for the `tray-quit` action (CT-11) to
        // take + drop when the user actually wants to exit. Per #512.
        *state.app_hold_guard.borrow_mut() = Some(app.hold());

        // Spawn the tray on the first activate (not connect_startup
        // so we can poke `state.tray_available`). On failure flip
        // the cell to false; downstream code (close-request handler,
        // prefs Behavior switch) reads that to degrade gracefully.
        // Per #512.
        spawn_tray_and_route(app, &state);

        // Hydrate close-to-tray + first-close-seen from config —
        // CT-8 added the `read_*` helpers as the canonical reads.
        state
            .close_to_tray
            .set(crate::preferences::general_page::read_close_to_tray(
                &config,
            ));
        state
            .tray_first_close_seen
            .set(crate::preferences::general_page::read_tray_first_close_seen(&config));

        // Replay persisted ACARS state. If `acars_enabled = true`
        // in config, dispatch SetAcarsEnabled(true) so the DSP
        // re-engages on app start. ACARS-related AppState fields
        // (acars_enabled, acars_recent, acars_total_count, etc.)
        // are populated by the DspToUi::AcarsEnabledChanged ack
        // arm in window.rs, NOT here. Per epic #474 sub-project 2.
        if crate::acars_config::read_acars_enabled(&config) {
            tracing::info!("ACARS startup-replay: dispatching SetAcarsEnabled(true)");
            state.send_dsp(crate::messages::UiToDsp::SetAcarsEnabled(true));
        }

        // Default `present()` unless the autostart path passed
        // --start-hidden AND the tray is actually available. If the
        // tray is unavailable we force-present so the user isn't
        // stranded with an invisible process — there'd be no other
        // affordance to interact with sdr-rs in that case. Per
        // spec §Error handling.
        let tray_ok = state.tray_available.get();
        if !start_hidden || !tray_ok {
            if start_hidden && !tray_ok {
                tracing::warn!(
                    "start-hidden requested but tray unavailable; \
                     presenting window to avoid an invisible process",
                );
            }
            if let Some(toplevel) = app.windows().into_iter().next() {
                toplevel.present();
            }
        }
    });

    app
}

/// Spawn the tray service on a worker thread and route its events
/// to GIO actions on the main loop. On failure, flip
/// `state.tray_available` to false so the close-request handler
/// short-circuits to "exit on close" and the prefs row greys out.
fn spawn_tray_and_route(app: &adw::Application, state: &std::rc::Rc<crate::state::AppState>) {
    let (tx, rx) = mpsc::channel::<sdr_tray::TrayEvent>();
    match sdr_tray::spawn(tx) {
        Ok(handle) => {
            *state.tray_handle.borrow_mut() = Some(handle);
            let app_for_route = app.clone();
            // 50 ms tick to drain the cross-thread mpsc into GIO
            // actions on the main loop. Cheaper than wiring up a
            // glib worker source; granularity is well below human
            // perception for click→action latency.
            glib::timeout_add_local(Duration::from_millis(50), move || {
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        sdr_tray::TrayEvent::Show => {
                            app_for_route.activate_action("tray-show", None);
                        }
                        sdr_tray::TrayEvent::Hide => {
                            app_for_route.activate_action("tray-hide", None);
                        }
                        sdr_tray::TrayEvent::ToggleVisibility => {
                            app_for_route.activate_action("tray-toggle", None);
                        }
                        sdr_tray::TrayEvent::Quit => {
                            app_for_route.activate_action("tray-quit", None);
                        }
                    }
                }
                glib::ControlFlow::Continue
            });
            tracing::info!("tray spawned and event router started");
        }
        Err(e) => {
            tracing::warn!(error = %e, "tray spawn failed — close-to-tray disabled this session");
            state.tray_available.set(false);
        }
    }
}
