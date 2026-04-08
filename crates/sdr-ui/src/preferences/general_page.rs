//! General preferences page — directory settings for recordings and screenshots.

use std::sync::Arc;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;

/// Config key for the recording directory path.
const KEY_RECORDING_DIR: &str = "recording_directory";

/// Config key for the screenshot directory path.
const KEY_SCREENSHOT_DIR: &str = "screenshot_directory";

/// Default recording directory name under `$HOME`.
const DEFAULT_RECORDING_DIR_NAME: &str = "sdr-recordings";

/// Build the General preferences page.
pub fn build_general_page(
    window: &adw::PreferencesWindow,
    config: &Arc<ConfigManager>,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("preferences-system-symbolic")
        .build();

    let directories_group = adw::PreferencesGroup::builder()
        .title("Directories")
        .description("Default locations for saved files")
        .build();

    // --- Recording directory row ---
    let recording_row = build_directory_row(
        "Recording Directory",
        "Location for IQ and audio recordings",
        &current_recording_dir(config),
    );
    let recording_button = folder_button();
    recording_row.add_suffix(&recording_button);

    let config_rec = Arc::clone(config);
    let row_rec = recording_row.clone();
    let window_rec = window.clone();
    recording_button.connect_clicked(move |_| {
        open_folder_picker(
            "Select Recording Directory",
            &window_rec,
            &config_rec,
            KEY_RECORDING_DIR,
            &row_rec,
        );
    });

    directories_group.add(&recording_row);

    // --- Screenshot directory row ---
    let screenshot_row = build_directory_row(
        "Screenshot Directory",
        "Location for waterfall screenshots",
        &current_screenshot_dir(config),
    );
    let screenshot_button = folder_button();
    screenshot_row.add_suffix(&screenshot_button);

    let config_ss = Arc::clone(config);
    let row_ss = screenshot_row.clone();
    let window_ss = window.clone();
    screenshot_button.connect_clicked(move |_| {
        open_folder_picker(
            "Select Screenshot Directory",
            &window_ss,
            &config_ss,
            KEY_SCREENSHOT_DIR,
            &row_ss,
        );
    });

    directories_group.add(&screenshot_row);

    page.add(&directories_group);
    page
}

/// Read the current recording directory from config, falling back to `~/sdr-recordings`.
fn current_recording_dir(config: &ConfigManager) -> String {
    config.read(|v| {
        v.get(KEY_RECORDING_DIR)
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || {
                    glib::home_dir()
                        .join(DEFAULT_RECORDING_DIR_NAME)
                        .to_string_lossy()
                        .into_owned()
                },
                String::from,
            )
    })
}

/// Read the current screenshot directory from config, falling back to `~/Pictures`.
fn current_screenshot_dir(config: &ConfigManager) -> String {
    config.read(|v| {
        v.get(KEY_SCREENSHOT_DIR)
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || {
                    glib::user_special_dir(glib::UserDirectory::Pictures)
                        .unwrap_or_else(|| glib::home_dir().join("Pictures"))
                        .to_string_lossy()
                        .into_owned()
                },
                String::from,
            )
    })
}

/// Build a directory `ActionRow` with a subtitle showing the current path.
fn build_directory_row(title: &str, subtitle: &str, current_path: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(current_path)
        .subtitle_lines(1)
        .tooltip_text(subtitle)
        .build()
}

/// Build a flat folder-open button for use as an `ActionRow` suffix.
fn folder_button() -> gtk4::Button {
    gtk4::Button::builder()
        .icon_name("folder-open-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text("Choose folder")
        .css_classes(["flat"])
        .build()
}

/// Open a `FileDialog` folder picker, saving the result to config and updating the row subtitle.
fn open_folder_picker(
    title: &str,
    window: &adw::PreferencesWindow,
    config: &Arc<ConfigManager>,
    config_key: &str,
    row: &adw::ActionRow,
) {
    let dialog = gtk4::FileDialog::builder().title(title).build();

    let config = Arc::clone(config);
    let key = config_key.to_owned();
    let row = row.clone();

    dialog.select_folder(Some(window), gtk4::gio::Cancellable::NONE, move |result| {
        if let Ok(folder) = result
            && let Some(path) = folder.path()
        {
            let path_str = path.to_string_lossy().into_owned();
            row.set_subtitle(&path_str);
            config.write(|v| {
                v[&key] = serde_json::Value::String(path_str);
            });
        }
    });
}
