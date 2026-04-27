//! Desktop notification helper via `GNotification`.
//!
//! Uses GTK's `gio::Notification` API which on Linux maps directly
//! onto the freedesktop `org.freedesktop.Notifications` D-Bus
//! interface — same path `notify-send` (and `nwg-notifications` on
//! the receiving side) speaks. We get urgency hints, action buttons,
//! and click-to-open via the standard spec without any zbus / dbus
//! plumbing of our own.
//!
//! Two flavors of notification:
//!
//! * [`send`] — plain "summary + body + optional click-to-open"
//!   used by the existing one-off pings (PNG export success/failure,
//!   etc.).
//! * [`send_pass_alert`] — richer pre-pass alert used by #510. Sets
//!   High priority (so notification daemons that honor it surface
//!   the popup over an idle desktop), adds a "Tune" button bound to
//!   the `app.tune-satellite` action with the NORAD id as target,
//!   and falls back gracefully if no `GApplication` is running.

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use sdr_sat::Pass;

/// `GApplication` action name fired when the user clicks the "Tune"
/// button on a pre-pass alert. Target type is `u32` (NORAD id);
/// the handler — registered in `window.rs::connect_satellites_panel`
/// — looks the satellite up in [`sdr_sat::KNOWN_SATELLITES`] for
/// downlink frequency / demod mode / channel bandwidth.
pub const TUNE_SATELLITE_ACTION: &str = "tune-satellite";

/// Send a desktop notification. If `open_path` is provided, clicking the
/// notification opens the file with the default application.
pub fn send(summary: &str, body: &str, open_path: Option<&std::path::Path>) {
    let Some(app) = gio::Application::default() else {
        tracing::debug!("no default `GApplication` — skipping notification");
        return;
    };

    let notification = gio::Notification::new(summary);
    notification.set_body(Some(body));

    if let Some(path) = open_path {
        let uri = gio::File::for_path(path).uri();
        notification.set_default_action_and_target_value("app.open-uri", Some(&uri.to_variant()));
    }

    app.send_notification(Some("sdr-rs-notify"), &notification);
}

/// Fire a pre-pass alert for a watched satellite. Per #510.
///
/// The notification includes:
///
/// * Title: `"NOAA 19 — overhead in 5 min"`.
/// * Body: peak elevation + AOS/LOS azimuths (multi-line).
/// * Priority: `High` — picked over `Urgent` because pass alerts
///   are timely but not life-critical; `Urgent` (which maps to
///   urgency=2 / "critical" on the freedesktop bus) is reserved
///   for actual emergencies and many daemons keep critical popups
///   on screen until manually dismissed.
/// * A "Tune" action button that invokes `app.tune-satellite`
///   with the NORAD id as a `u32` target value. The handler is
///   registered separately in `window.rs::connect_satellites_panel`.
///
/// `lead_min` is the lead time in minutes the user configured —
/// passed in rather than recomputed from `pass.start - now` so the
/// title matches what the scheduler decided when it crossed the
/// threshold.
pub fn send_pass_alert(pass: &Pass, norad_id: u32, lead_min: u32) {
    let Some(app) = gio::Application::default() else {
        tracing::debug!("no default `GApplication` — skipping pass alert");
        return;
    };
    let summary = format!("{} — overhead in {} min", pass.satellite, lead_min);
    let body = format!(
        "Peak elevation {:.0}°  ·  AOS {:.0}° → LOS {:.0}°",
        pass.max_elevation_deg, pass.start_az_deg, pass.end_az_deg,
    );
    let notification = gio::Notification::new(&summary);
    notification.set_body(Some(&body));
    notification.set_priority(gio::NotificationPriority::High);
    notification.add_button_with_target_value(
        "Tune",
        &format!("app.{TUNE_SATELLITE_ACTION}"),
        Some(&norad_id.to_variant()),
    );
    // Notification id keyed by `(norad, pass_start)` so a redelivery
    // of the same pass (e.g. user reconnects to the bus) replaces
    // rather than stacks. Different passes / different sats get
    // distinct ids and surface as separate notifications.
    let id = format!("sdr-rs-pass-{norad_id}-{}", pass.start.timestamp());
    app.send_notification(Some(&id), &notification);
}

/// Register the `app.open-uri` action on the application.
///
/// Call once during startup so notification click actions work.
/// The companion `app.tune-satellite` action is registered later
/// in `window.rs::connect_satellites_panel` because it needs
/// access to the per-window `tune_to_satellite` closure.
pub fn register_actions(app: &impl IsA<gio::ActionMap>) {
    let open_action = gio::SimpleAction::new("open-uri", Some(glib::VariantTy::STRING));
    open_action.connect_activate(|_, param| {
        let Some(uri) = param.and_then(glib::Variant::get::<String>) else {
            return;
        };
        tracing::info!(uri, "opening from notification");
        if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE) {
            tracing::warn!("failed to open URI: {e}");
        }
    });
    app.add_action(&open_action);
}
