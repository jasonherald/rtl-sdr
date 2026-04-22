//! Frequency list widget for the `RadioReference` browse dialog.
//!
//! Builds and manages an `adw::ActionRow`-based list inside a `gtk4::ListBox`,
//! where each row represents an `RrFrequency`.  Already-bookmarked frequencies
//! are dimmed; the rest get a check-button for selection.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_radioreference::RrFrequency;

use crate::sidebar::navigation_panel::{Bookmark, format_frequency, load_bookmarks};

// ---------------------------------------------------------------------------
// Row model
// ---------------------------------------------------------------------------

/// View-model for a single row in the frequency list.
pub struct FrequencyRow {
    /// The upstream `RadioReference` frequency record.
    pub freq: RrFrequency,
    /// Whether the user has selected this row for import.
    pub selected: bool,
    /// Whether an existing bookmark already matches this frequency.
    pub already_bookmarked: bool,
    /// First tag description (used as category filter key), or empty.
    pub category: String,
}

// ---------------------------------------------------------------------------
// Populate helper
// ---------------------------------------------------------------------------

/// Rebuild the `list_box` from a fresh set of frequencies.
///
/// Returns the row models (wrapped in `Rc<RefCell>` so check-button closures
/// can mutate `selected`), plus the sorted-unique category and agency lists
/// (each prefixed with "All").
#[allow(clippy::too_many_lines)]
pub fn populate_frequencies(
    list_box: &gtk4::ListBox,
    frequencies: &[RrFrequency],
    import_button: &gtk4::Button,
    category_model: &gtk4::StringList,
    agency_model: &gtk4::StringList,
) -> Vec<Rc<RefCell<FrequencyRow>>> {
    // Clear existing children.
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    // Load existing bookmarks for duplicate detection.
    let existing = load_bookmarks();

    // Build row models.
    let mut rows: Vec<Rc<RefCell<FrequencyRow>>> = Vec::with_capacity(frequencies.len());
    let mut categories = std::collections::BTreeSet::new();
    let mut agencies = std::collections::BTreeSet::new();

    for freq in frequencies {
        let already = is_already_bookmarked(freq, &existing);
        let category = freq
            .tags
            .first()
            .map_or_else(String::new, |t| t.description.clone());

        if !category.is_empty() {
            categories.insert(category.clone());
        }
        if !freq.alpha_tag.is_empty() {
            agencies.insert(freq.alpha_tag.clone());
        }

        let row_model = Rc::new(RefCell::new(FrequencyRow {
            freq: freq.clone(),
            selected: false,
            already_bookmarked: already,
            category,
        }));
        rows.push(row_model);
    }

    // Build GTK rows.
    let import_button_weak = import_button.downgrade();
    let rows_for_count = Rc::new(rows.clone());

    for row_model in &rows {
        let rm = row_model.borrow();
        let freq_label = format_frequency(rm.freq.freq_hz);
        let mapped = sdr_radioreference::mode_map::map_rr_mode(&rm.freq.mode);

        // Title: alpha tag if available, otherwise description
        let title = if rm.freq.alpha_tag.is_empty() {
            rm.freq.description.clone()
        } else {
            rm.freq.alpha_tag.clone()
        };

        // Subtitle: frequency, mapped mode, tone, description (if not in title)
        let mut parts = vec![format!("{freq_label}  {}", mapped.demod_mode)];
        if let Some(tone) = rm.freq.tone {
            parts.push(format!("PL {tone:.1}"));
        }
        if !rm.freq.alpha_tag.is_empty() && !rm.freq.description.is_empty() {
            parts.push(rm.freq.description.clone());
        }
        let subtitle = parts.join("  \u{00b7}  "); // middle dot separator

        let action_row = adw::ActionRow::builder()
            .title(&title)
            .subtitle(&subtitle)
            .build();

        if rm.already_bookmarked {
            let icon = gtk4::Image::from_icon_name("emblem-ok-symbolic");
            icon.set_valign(gtk4::Align::Center);
            action_row.add_prefix(&icon);
            action_row.set_sensitive(false);
        } else {
            let check = gtk4::CheckButton::new();
            check.set_valign(gtk4::Align::Center);

            let model_ref = Rc::clone(row_model);
            let btn_weak = import_button_weak.clone();
            let count_rows = Rc::clone(&rows_for_count);
            check.connect_toggled(move |cb| {
                model_ref.borrow_mut().selected = cb.is_active();
                if let Some(btn) = btn_weak.upgrade() {
                    update_import_count(&btn, &count_rows);
                }
            });
            action_row.add_prefix(&check);
        }

        // Store category and alpha_tag as widget names so filters can match.
        // We use two custom data keys via GObject data.
        action_row.set_widget_name(&format!("{}|||{}", rm.category, rm.freq.alpha_tag));

        list_box.append(&action_row);
    }

    // Update filter dropdown models.
    rebuild_string_list(category_model, &categories);
    rebuild_string_list(agency_model, &agencies);

    // Reset import button.
    update_import_count(import_button, &rows);

    rows
}

// ---------------------------------------------------------------------------
// Import-count helper
// ---------------------------------------------------------------------------

/// Update the import button label and sensitivity based on the number of
/// selected (non-bookmarked) rows.
pub fn update_import_count(button: &gtk4::Button, rows: &[Rc<RefCell<FrequencyRow>>]) {
    let count = rows
        .iter()
        .filter(|r| {
            let r = r.borrow();
            r.selected && !r.already_bookmarked
        })
        .count();

    if count == 0 {
        button.set_label("Import Selected");
        button.set_sensitive(false);
    } else {
        button.set_label(&format!("Import Selected ({count})"));
        button.set_sensitive(true);
    }
}

// ---------------------------------------------------------------------------
// Filter helper
// ---------------------------------------------------------------------------

/// Show/hide rows in the list box according to the current category and agency
/// filter values.  `"All"` (or index 0) means no filtering on that axis.
pub fn apply_filters(
    list_box: &gtk4::ListBox,
    rows: &[Rc<RefCell<FrequencyRow>>],
    category_filter: &str,
    agency_filter: &str,
) {
    let show_all_categories = category_filter == "All" || category_filter.is_empty();
    let show_all_agencies = agency_filter == "All" || agency_filter.is_empty();

    let mut child = list_box.first_child();
    let mut idx = 0;
    while let Some(widget) = child {
        if let Some(row_model) = rows.get(idx) {
            let rm = row_model.borrow();
            let cat_ok = show_all_categories || rm.category == category_filter;
            let agency_ok = show_all_agencies || rm.freq.alpha_tag == agency_filter;
            widget.set_visible(cat_ok && agency_ok);
        }
        child = widget.next_sibling();
        idx += 1;
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Check whether an `RrFrequency` already exists as a bookmark.
///
/// Matches on `rr_import_id` first (exact), then falls back to `freq_hz`.
fn is_already_bookmarked(freq: &RrFrequency, bookmarks: &[Bookmark]) -> bool {
    bookmarks.iter().any(|bm| {
        // Exact match on RadioReference frequency ID.
        if let Some(ref import_id) = bm.rr_import_id
            && *import_id == freq.id
        {
            return true;
        }
        // Fallback: match on frequency in Hz.
        bm.frequency == freq.freq_hz
    })
}

/// Rebuild a `StringList` with "All" followed by sorted unique values.
fn rebuild_string_list(model: &gtk4::StringList, values: &std::collections::BTreeSet<String>) {
    // Remove all existing items.
    let n = model.n_items();
    if n > 0 {
        model.splice(0, n, &[] as &[&str]);
    }

    model.append("All");
    for v in values {
        model.append(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_radioreference::RrTag;

    fn sample_freq(id: &str, freq_hz: u64, mode: &str, desc: &str) -> RrFrequency {
        RrFrequency {
            id: id.to_string(),
            freq_hz,
            mode: mode.to_string(),
            tone: None,
            description: desc.to_string(),
            alpha_tag: String::new(),
            tags: Vec::new(),
        }
    }

    #[test]
    fn duplicate_detection_by_import_id() {
        let bm = Bookmark {
            name: "Test".to_string(),
            frequency: 100_000_000,
            demod_mode: "NFM".to_string(),
            bandwidth: 12_500.0,
            squelch_enabled: None,
            auto_squelch_enabled: None,
            squelch_level: None,
            gain: None,
            agc: None,
            agc_type: None,
            volume: None,
            deemphasis: None,
            nb_enabled: None,
            nb_level: None,
            fm_if_nr: None,
            wfm_stereo: None,
            high_pass: None,
            rr_category: None,
            rr_import_id: Some("42".to_string()),
            ctcss_mode: None,
            ctcss_threshold: None,
            voice_squelch_mode: None,
            scan_enabled: false,
            priority: 0,
            dwell_ms_override: None,
            hang_ms_override: None,
        };

        let freq = sample_freq("42", 999_999_999, "FM", "test");
        assert!(is_already_bookmarked(&freq, &[bm]));
    }

    #[test]
    fn duplicate_detection_by_freq_hz() {
        let bm = Bookmark {
            name: "Test".to_string(),
            frequency: 155_000_000,
            demod_mode: "NFM".to_string(),
            bandwidth: 12_500.0,
            squelch_enabled: None,
            auto_squelch_enabled: None,
            squelch_level: None,
            gain: None,
            agc: None,
            agc_type: None,
            volume: None,
            deemphasis: None,
            nb_enabled: None,
            nb_level: None,
            fm_if_nr: None,
            wfm_stereo: None,
            high_pass: None,
            rr_category: None,
            rr_import_id: None,
            ctcss_mode: None,
            ctcss_threshold: None,
            voice_squelch_mode: None,
            scan_enabled: false,
            priority: 0,
            dwell_ms_override: None,
            hang_ms_override: None,
        };

        let freq = sample_freq("99", 155_000_000, "FM", "test");
        assert!(is_already_bookmarked(&freq, &[bm]));
    }

    #[test]
    fn not_bookmarked_when_no_match() {
        let bm = Bookmark {
            name: "Other".to_string(),
            frequency: 100_000_000,
            demod_mode: "NFM".to_string(),
            bandwidth: 12_500.0,
            squelch_enabled: None,
            auto_squelch_enabled: None,
            squelch_level: None,
            gain: None,
            agc: None,
            agc_type: None,
            volume: None,
            deemphasis: None,
            nb_enabled: None,
            nb_level: None,
            fm_if_nr: None,
            wfm_stereo: None,
            high_pass: None,
            rr_category: None,
            rr_import_id: Some("10".to_string()),
            ctcss_mode: None,
            ctcss_threshold: None,
            voice_squelch_mode: None,
            scan_enabled: false,
            priority: 0,
            dwell_ms_override: None,
            hang_ms_override: None,
        };

        let freq = sample_freq("99", 155_000_000, "FM", "test");
        assert!(!is_already_bookmarked(&freq, &[bm]));
    }

    #[test]
    fn category_from_first_tag() {
        let freq = RrFrequency {
            id: "1".to_string(),
            freq_hz: 155_000_000,
            mode: "FM".to_string(),
            tone: None,
            description: "Test".to_string(),
            alpha_tag: "PD".to_string(),
            tags: vec![
                RrTag {
                    id: 1,
                    description: "Law Dispatch".to_string(),
                },
                RrTag {
                    id: 2,
                    description: "Fire".to_string(),
                },
            ],
        };

        let cat = freq
            .tags
            .first()
            .map_or_else(String::new, |t| t.description.clone());
        assert_eq!(cat, "Law Dispatch");
    }
}
