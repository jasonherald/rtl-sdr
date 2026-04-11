//! `RadioReference` browse dialog — zip code search, frequency list, and import.

mod frequency_list;

use std::cell::RefCell;
use std::rc::Rc;

use crate::preferences::accounts_page::load_rr_credentials;
use crate::sidebar::navigation_panel::{
    Bookmark, format_frequency, load_bookmarks, parse_demod_mode, save_bookmarks,
};
use frequency_list::FrequencyRow;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

/// Dialog content width in pixels.
const DIALOG_WIDTH: i32 = 700;
/// Dialog content height in pixels.
const DIALOG_HEIGHT: i32 = 600;

/// Show the `RadioReference` browse dialog.
///
/// `on_import` is called (on the GTK main thread) after bookmarks are saved,
/// so the caller can rebuild the bookmark list in the sidebar.
#[allow(clippy::too_many_lines)]
pub fn show_browse_dialog<F: Fn() + 'static>(parent: &impl IsA<gtk4::Widget>, on_import: F) {
    let on_import = Rc::new(on_import);

    // -----------------------------------------------------------------------
    // Top-level dialog shell
    // -----------------------------------------------------------------------
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(12)
        .margin_start(16)
        .margin_end(16)
        .margin_top(12)
        .margin_bottom(12)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&content));

    let dialog = adw::Dialog::builder()
        .title("RadioReference")
        .content_width(DIALOG_WIDTH)
        .content_height(DIALOG_HEIGHT)
        .build();
    dialog.set_child(Some(&toolbar));

    // -----------------------------------------------------------------------
    // Search section
    // -----------------------------------------------------------------------
    let search_group = adw::PreferencesGroup::builder()
        .title("Search")
        .description("Enter a US ZIP code to find local frequencies (US only)")
        .build();

    let zip_entry = adw::EntryRow::builder().title("ZIP Code").build();

    let search_button = gtk4::Button::builder()
        .label("Search")
        .valign(gtk4::Align::Center)
        .css_classes(["suggested-action"])
        .build();
    zip_entry.add_suffix(&search_button);

    search_group.add(&zip_entry);
    content.append(&search_group);

    // Spinner + status row — outside the preferences group for better alignment
    let status_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(8)
        .margin_start(4)
        .margin_top(4)
        .build();

    let spinner = gtk4::Spinner::builder().visible(false).build();

    let status_label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .hexpand(true)
        .wrap(true)
        .visible(false)
        .build();

    status_box.append(&spinner);
    status_box.append(&status_label);
    content.append(&status_box);

    // -----------------------------------------------------------------------
    // Results section (hidden until search succeeds)
    // -----------------------------------------------------------------------
    let results_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(8)
        .visible(false)
        .vexpand(true)
        .build();

    // Category filter
    let category_model = gtk4::StringList::new(&["All"]);
    let category_dropdown = gtk4::DropDown::builder()
        .model(&category_model)
        .valign(gtk4::Align::Center)
        .build();
    let category_row = adw::ActionRow::builder()
        .title("Category")
        .activatable(false)
        .build();
    category_row.add_suffix(&category_dropdown);

    // Agency filter
    let agency_model = gtk4::StringList::new(&["All"]);
    let agency_dropdown = gtk4::DropDown::builder()
        .model(&agency_model)
        .valign(gtk4::Align::Center)
        .build();
    let agency_row = adw::ActionRow::builder()
        .title("Agency")
        .activatable(false)
        .build();
    agency_row.add_suffix(&agency_dropdown);

    let filter_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    filter_list.append(&category_row);
    filter_list.append(&agency_row);
    results_box.append(&filter_list);

    // Scrollable frequency list
    let freq_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    let freq_scroll = gtk4::ScrolledWindow::builder()
        .child(&freq_list)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .build();
    results_box.append(&freq_scroll);

    // Import button
    let import_button = gtk4::Button::builder()
        .label("Import Selected")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    results_box.append(&import_button);

    content.append(&results_box);

    // -----------------------------------------------------------------------
    // Shared mutable state for closures
    // -----------------------------------------------------------------------
    let rows: Rc<RefCell<Vec<Rc<RefCell<FrequencyRow>>>>> = Rc::new(RefCell::new(Vec::new()));

    // -----------------------------------------------------------------------
    // Search handler
    // -----------------------------------------------------------------------
    {
        let zip_entry = zip_entry.clone();
        let spinner = spinner.clone();
        let status_label = status_label.clone();
        let results_box = results_box.clone();
        let freq_list = freq_list.clone();
        let import_button = import_button.clone();
        let category_model = category_model.clone();
        let agency_model = agency_model.clone();
        let rows = Rc::clone(&rows);
        let search_button_ref = search_button.clone();

        search_button.connect_clicked(move |_| {
            let zip = zip_entry.text().trim().to_string();

            // Validate 5-digit US zip code.
            if zip.len() != 5 || !zip.chars().all(|c| c.is_ascii_digit()) {
                show_status(&status_label, "Please enter a valid 5-digit ZIP code", false);
                return;
            }

            // Load credentials.
            let Some((username, password)) = load_rr_credentials() else {
                show_status(
                    &status_label,
                    "No RadioReference credentials — configure them in Preferences \u{2192} Accounts",
                    false,
                );
                return;
            };

            // Show spinner, disable button.
            spinner.set_visible(true);
            spinner.start();
            status_label.set_visible(false);
            search_button_ref.set_sensitive(false);
            results_box.set_visible(false);

            let status_label = status_label.clone();
            let spinner = spinner.clone();
            let results_box = results_box.clone();
            let freq_list = freq_list.clone();
            let import_button = import_button.clone();
            let category_model = category_model.clone();
            let agency_model = agency_model.clone();
            let rows = Rc::clone(&rows);
            let search_button_ref = search_button_ref.clone();

            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || {
                    let client = sdr_radioreference::RrClient::new(&username, &password)?;
                    let zip_info = client.get_zip_info(&zip)?;
                    let (county_name, freqs) =
                        client.get_county_frequencies(zip_info.county_id)?;
                    Ok::<_, sdr_radioreference::SoapError>((zip_info, county_name, freqs))
                })
                .await
                .unwrap_or_else(|_| Err(sdr_radioreference::SoapError::Fault(
                    "background task panicked".to_string(),
                )));

                // Back on the main thread.
                spinner.stop();
                spinner.set_visible(false);
                search_button_ref.set_sensitive(true);

                match result {
                    Ok((zip_info, county_name, freqs)) => {
                        let location = if county_name.is_empty() {
                            zip_info.city.clone()
                        } else {
                            format!("{}, {}", zip_info.city, county_name)
                        };
                        let msg = format!(
                            "{location} \u{2014} {} frequencies",
                            freqs.len()
                        );
                        show_status(&status_label, &msg, true);

                        let new_rows = frequency_list::populate_frequencies(
                            &freq_list,
                            &freqs,
                            &import_button,
                            &category_model,
                            &agency_model,
                        );
                        *rows.borrow_mut() = new_rows;
                        results_box.set_visible(true);
                    }
                    Err(e) => {
                        tracing::warn!("RadioReference search failed: {e}");
                        show_status(&status_label, &e.to_string(), false);
                    }
                }
            });
        });
    }

    // -----------------------------------------------------------------------
    // Category filter handler
    // -----------------------------------------------------------------------
    {
        let freq_list = freq_list.clone();
        let rows = Rc::clone(&rows);
        let agency_model = agency_model.clone();
        let agency_dropdown = agency_dropdown.clone();

        category_dropdown.connect_selected_notify(move |dd| {
            let cat = selected_string(&category_model, dd.selected());
            let agency = selected_string(&agency_model, agency_dropdown.selected());
            frequency_list::apply_filters(&freq_list, &rows.borrow(), &cat, &agency);
        });
    }

    // -----------------------------------------------------------------------
    // Agency filter handler
    // -----------------------------------------------------------------------
    {
        let freq_list = freq_list.clone();
        let rows = Rc::clone(&rows);
        let category_dropdown_ref = category_dropdown.clone();

        agency_dropdown.connect_selected_notify(move |dd| {
            let cat = category_dropdown_ref.model().map_or_else(
                || "All".to_string(),
                |m| string_at(&m, category_dropdown_ref.selected()),
            );
            let agency_val = dd
                .model()
                .map_or_else(|| "All".to_string(), |m| string_at(&m, dd.selected()));

            frequency_list::apply_filters(&freq_list, &rows.borrow(), &cat, &agency_val);
        });
    }

    // -----------------------------------------------------------------------
    // Import handler
    // -----------------------------------------------------------------------
    {
        let rows = Rc::clone(&rows);
        let dialog_weak = dialog.downgrade();
        let on_import = Rc::clone(&on_import);

        import_button.connect_clicked(move |_| {
            let rows = rows.borrow();
            let selected: Vec<_> = rows
                .iter()
                .filter(|r| {
                    let r = r.borrow();
                    r.selected && !r.already_bookmarked
                })
                .collect();

            if selected.is_empty() {
                return;
            }

            let mut bookmarks = load_bookmarks();

            for row_rc in &selected {
                let row = row_rc.borrow();
                let freq = &row.freq;
                let mapped = sdr_radioreference::mode_map::map_rr_mode(&freq.mode);
                let demod = parse_demod_mode(mapped.demod_mode);

                let name = if freq.alpha_tag.is_empty() {
                    freq.description.clone()
                } else {
                    format!("{} - {}", freq.alpha_tag, freq.description)
                };
                // Use formatted frequency as fallback name if both are empty.
                let name = if name.trim().is_empty() {
                    format_frequency(freq.freq_hz)
                } else {
                    name
                };

                let mut bookmark = Bookmark::new(&name, freq.freq_hz, demod, mapped.bandwidth);
                bookmark.rr_category = if row.category.is_empty() {
                    None
                } else {
                    Some(row.category.clone())
                };
                bookmark.rr_import_id = Some(freq.id.clone());

                bookmarks.push(bookmark);
            }

            save_bookmarks(&bookmarks);

            let count = selected.len();
            tracing::info!(count, "imported RadioReference frequencies as bookmarks");

            on_import();

            if let Some(d) = dialog_weak.upgrade() {
                d.close();
            }
        });
    }

    dialog.present(Some(parent));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use crate::ui_helpers::show_status;

/// Read the string at the given index from a `DropDown`'s `StringList` model.
fn selected_string(model: &gtk4::StringList, index: u32) -> String {
    model
        .string(index)
        .map_or_else(|| "All".to_string(), |s| s.to_string())
}

/// Read the string at the given index from a generic `ListModel`.
fn string_at(model: &gio::ListModel, index: u32) -> String {
    model
        .item(index)
        .and_then(|obj| obj.downcast::<gtk4::StringObject>().ok())
        .map_or_else(|| "All".to_string(), |s| s.string().to_string())
}
