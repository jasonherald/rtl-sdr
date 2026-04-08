//! Accounts preferences page — `RadioReference` credential management.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::KeyringStore;

/// Keyring service name for the application.
const KEYRING_SERVICE: &str = "sdr-rs";

/// Keyring key for the `RadioReference` username.
const KEY_RR_USERNAME: &str = "radioreference-username";

/// Keyring key for the `RadioReference` password.
const KEY_RR_PASSWORD: &str = "radioreference-password";

/// Check whether `RadioReference` credentials exist in the keyring.
///
/// Returns `false` if the keyring backend is unavailable, logging the error.
pub fn has_rr_credentials() -> bool {
    let store = KeyringStore::new(KEYRING_SERVICE);
    let has_user = store.has(KEY_RR_USERNAME).unwrap_or_else(|e| {
        tracing::warn!("keyring check failed: {e}");
        false
    });
    let has_pass = store.has(KEY_RR_PASSWORD).unwrap_or_else(|e| {
        tracing::warn!("keyring check failed: {e}");
        false
    });
    has_user && has_pass
}

/// Load `RadioReference` credentials from the keyring, if present.
///
/// Returns `Some((username, password))` when both are stored, `None` otherwise.
pub fn load_rr_credentials() -> Option<(String, String)> {
    let store = KeyringStore::new(KEYRING_SERVICE);
    let username = store.get(KEY_RR_USERNAME).ok().flatten()?;
    let password = store.get(KEY_RR_PASSWORD).ok().flatten()?;
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some((username, password))
}

/// Build the Accounts preferences page.
///
/// Returns the page widget and a reactive flag indicating whether credentials
/// are currently stored in the keyring.
#[allow(clippy::too_many_lines)]
pub fn build_accounts_page() -> (adw::PreferencesPage, Rc<Cell<bool>>) {
    let has_credentials = Rc::new(Cell::new(has_rr_credentials()));

    let page = adw::PreferencesPage::builder()
        .title("Accounts")
        .icon_name("system-users-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("RadioReference")
        .description("Premium account required for frequency database access")
        .build();

    // --- Username row ---
    let username_row = adw::EntryRow::builder().title("Username").build();

    // --- Password row ---
    let password_row = adw::PasswordEntryRow::builder().title("Password").build();

    // Pre-fill username if credentials already exist
    if let Some((stored_user, _)) = load_rr_credentials() {
        username_row.set_text(&stored_user);
    }

    group.add(&username_row);
    group.add(&password_row);

    // --- Status label (hidden by default) ---
    let status_label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .margin_top(8)
        .visible(false)
        .build();

    // --- Button box ---
    let button_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(12)
        .margin_top(12)
        .halign(gtk4::Align::Start)
        .build();

    let spinner = gtk4::Spinner::builder().visible(false).build();

    let test_button = gtk4::Button::builder()
        .label("Test & Save")
        .css_classes(["suggested-action"])
        .build();

    let remove_button = gtk4::Button::builder()
        .label("Remove Credentials")
        .css_classes(["destructive-action"])
        .visible(has_credentials.get())
        .build();

    button_box.append(&test_button);
    button_box.append(&spinner);
    button_box.append(&remove_button);

    // Wrap status + buttons in a vertical box for the group
    let controls_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .build();
    controls_box.append(&button_box);
    controls_box.append(&status_label);

    group.add(&controls_box);
    page.add(&group);

    // --- "Test & Save" click handler ---
    {
        let cb_username_row = username_row.clone();
        let cb_password_row = password_row.clone();
        let cb_status_label = status_label.clone();
        let cb_spinner = spinner.clone();
        let cb_remove_button = remove_button.clone();
        let cb_has_credentials = Rc::clone(&has_credentials);

        test_button.connect_clicked(move |btn| {
            let username = cb_username_row.text().to_string();
            let password = cb_password_row.text().to_string();

            // Validate fields are not empty
            if username.trim().is_empty() || password.trim().is_empty() {
                show_status(
                    &cb_status_label,
                    "Username and password are required",
                    false,
                );
                return;
            }

            // Disable button, show spinner
            btn.set_sensitive(false);
            cb_spinner.set_visible(true);
            cb_spinner.start();
            cb_status_label.set_visible(false);

            // Capture clones for the async block (runs on the main thread)
            let status_label = cb_status_label.clone();
            let btn_ref = btn.clone();
            let spinner = cb_spinner.clone();
            let remove_button = cb_remove_button.clone();
            let has_credentials = Rc::clone(&cb_has_credentials);

            // Spawn blocking SOAP test, then handle result on main thread
            glib::spawn_future_local(async move {
                let result = gtk4::gio::spawn_blocking(move || {
                    let client = match sdr_radioreference::RrClient::new(&username, &password) {
                        Ok(c) => c,
                        Err(e) => return Err(format!("client init: {e}")),
                    };
                    let test_result = client.test_connection().map_err(|e| e.to_string());

                    // Save to keyring on success
                    if test_result.is_ok() {
                        let store = KeyringStore::new(KEYRING_SERVICE);
                        if let Err(e) = store.set(KEY_RR_USERNAME, &username) {
                            return Err(format!("keyring error: {e}"));
                        }
                        if let Err(e) = store.set(KEY_RR_PASSWORD, &password) {
                            return Err(format!("keyring error: {e}"));
                        }
                    }

                    test_result
                })
                .await
                .unwrap_or_else(|_| Err("background task panicked".to_string()));

                // Back on the main thread — update UI
                spinner.stop();
                spinner.set_visible(false);
                btn_ref.set_sensitive(true);

                match result {
                    Ok(()) => {
                        show_status(&status_label, "Connected \u{2014} credentials saved", true);
                        has_credentials.set(true);
                        remove_button.set_visible(true);
                    }
                    Err(ref msg) => {
                        tracing::warn!("RadioReference test failed: {msg}");
                        show_status(&status_label, msg, false);
                    }
                }
            });
        });
    }

    // --- "Remove Credentials" click handler ---
    {
        let username_row = username_row.clone();
        let status_label = status_label.clone();
        let has_credentials = Rc::clone(&has_credentials);

        remove_button.connect_clicked(move |btn| {
            let store = KeyringStore::new(KEYRING_SERVICE);
            let mut ok = true;

            if let Err(e) = store.delete(KEY_RR_USERNAME) {
                tracing::error!("failed to delete username from keyring: {e}");
                ok = false;
            }
            if let Err(e) = store.delete(KEY_RR_PASSWORD) {
                tracing::error!("failed to delete password from keyring: {e}");
                ok = false;
            }

            if ok {
                username_row.set_text("");
                password_row.set_text("");
                has_credentials.set(false);
                btn.set_visible(false);
                show_status(&status_label, "Credentials removed", true);
            } else {
                show_status(&status_label, "Failed to remove credentials", false);
            }
        });
    }

    (page, has_credentials)
}

use crate::ui_helpers::show_status;
