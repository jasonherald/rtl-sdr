//! Desktop notification helper via `GNotification`.
//!
//! Uses GTK's `gio::Notification` API which integrates with the running
//! `GtkApplication` and supports click-to-open actions.

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

/// Send a desktop notification. If `open_path` is provided, clicking the
/// notification opens the file with the default application.
pub fn send(summary: &str, body: &str, open_path: Option<&std::path::Path>) {
    let Some(app) = gio::Application::default() else {
        tracing::debug!("no default GApplication — skipping notification");
        return;
    };

    let notification = gio::Notification::new(summary);
    notification.set_body(Some(body));

    if let Some(path) = open_path {
        let uri = format!("file://{}", path.display());
        notification.set_default_action_and_target_value("app.open-uri", Some(&uri.to_variant()));
    }

    app.send_notification(Some("sdr-rs-notify"), &notification);
}

/// Register the `app.open-uri` action on the application.
///
/// Call once during startup so notification click actions work.
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
