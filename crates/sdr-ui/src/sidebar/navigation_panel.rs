//! Navigation panel — band presets and frequency bookmarks.

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_types::DemodMode;

// ---------------------------------------------------------------------------
// Band presets — static, well-known frequency bands
// ---------------------------------------------------------------------------

/// A predefined frequency band preset.
struct BandPreset {
    name: &'static str,
    frequency: u64,
    demod_mode: DemodMode,
    bandwidth: f64,
}

/// Common band presets for North America / ITU Region 2.
const BAND_PRESETS: &[BandPreset] = &[
    BandPreset {
        name: "FM Broadcast",
        frequency: 98_100_000,
        demod_mode: DemodMode::Wfm,
        bandwidth: 150_000.0,
    },
    BandPreset {
        name: "NOAA Weather",
        frequency: 162_550_000,
        demod_mode: DemodMode::Nfm,
        bandwidth: 12_500.0,
    },
    BandPreset {
        name: "Aviation (Guard)",
        frequency: 121_500_000,
        demod_mode: DemodMode::Am,
        bandwidth: 8_333.0,
    },
    BandPreset {
        name: "2m Calling",
        frequency: 146_520_000,
        demod_mode: DemodMode::Nfm,
        bandwidth: 12_500.0,
    },
    BandPreset {
        name: "70cm Calling",
        frequency: 446_000_000,
        demod_mode: DemodMode::Nfm,
        bandwidth: 12_500.0,
    },
    BandPreset {
        name: "Marine Ch 16",
        frequency: 156_800_000,
        demod_mode: DemodMode::Nfm,
        bandwidth: 25_000.0,
    },
    BandPreset {
        name: "FRS Ch 1",
        frequency: 462_562_500,
        demod_mode: DemodMode::Nfm,
        bandwidth: 12_500.0,
    },
    BandPreset {
        name: "MURS Ch 1",
        frequency: 151_820_000,
        demod_mode: DemodMode::Nfm,
        bandwidth: 11_250.0,
    },
    BandPreset {
        name: "CB Ch 19",
        frequency: 27_185_000,
        demod_mode: DemodMode::Am,
        bandwidth: 10_000.0,
    },
    BandPreset {
        name: "10m Calling",
        frequency: 28_400_000,
        demod_mode: DemodMode::Usb,
        bandwidth: 2_700.0,
    },
];

// ---------------------------------------------------------------------------
// Bookmarks — user-saved frequencies with JSON persistence
// ---------------------------------------------------------------------------

/// A user-saved frequency bookmark.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Bookmark {
    pub name: String,
    pub frequency: u64,
    pub demod_mode: String,
    pub bandwidth: f64,
}

impl Bookmark {
    /// Create a bookmark from the current tuning state.
    pub fn new(name: &str, frequency: u64, demod_mode: DemodMode, bandwidth: f64) -> Self {
        Self {
            name: name.to_string(),
            frequency,
            demod_mode: demod_mode_to_string(demod_mode),
            bandwidth,
        }
    }
}

fn demod_mode_to_string(mode: DemodMode) -> String {
    match mode {
        DemodMode::Wfm => "WFM",
        DemodMode::Nfm => "NFM",
        DemodMode::Am => "AM",
        DemodMode::Usb => "USB",
        DemodMode::Lsb => "LSB",
        DemodMode::Dsb => "DSB",
        DemodMode::Cw => "CW",
        DemodMode::Raw => "RAW",
    }
    .to_string()
}

fn string_to_demod_mode(s: &str) -> DemodMode {
    match s {
        "WFM" => DemodMode::Wfm,
        "AM" => DemodMode::Am,
        "USB" => DemodMode::Usb,
        "LSB" => DemodMode::Lsb,
        "DSB" => DemodMode::Dsb,
        "CW" => DemodMode::Cw,
        "RAW" => DemodMode::Raw,
        // "NFM" and any unrecognized string default to NFM.
        _ => DemodMode::Nfm,
    }
}

/// Default bookmark file location.
fn bookmarks_path() -> std::path::PathBuf {
    let mut path = glib::user_config_dir();
    path.push("sdr-rs");
    path.push("bookmarks.json");
    path
}

fn load_bookmarks() -> Vec<Bookmark> {
    let path = bookmarks_path();
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str(&data) {
        Ok(bookmarks) => bookmarks,
        Err(e) => {
            tracing::warn!(?path, "failed to parse bookmarks, starting fresh: {e}");
            Vec::new()
        }
    }
}

pub fn save_bookmarks(bookmarks: &[Bookmark]) {
    let path = bookmarks_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(bookmarks) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                tracing::warn!("failed to save bookmarks: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to serialize bookmarks: {e}"),
    }
}

/// Format a frequency as a human-readable string (e.g., "98.100 MHz").
pub fn format_frequency(freq: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let freq_f64 = freq as f64;
    if freq >= 1_000_000_000 {
        format!("{:.3} GHz", freq_f64 / 1_000_000_000.0)
    } else if freq >= 1_000_000 {
        format!("{:.3} MHz", freq_f64 / 1_000_000.0)
    } else if freq >= 1_000 {
        format!("{:.1} kHz", freq_f64 / 1_000.0)
    } else {
        format!("{freq} Hz")
    }
}

// ---------------------------------------------------------------------------
// Navigation panel widget
// ---------------------------------------------------------------------------

/// Callback type for navigation actions (tune to frequency + set mode + bandwidth).
pub type NavigationCallback = Box<dyn Fn(u64, DemodMode, f64)>;

/// Identity of the currently active bookmark (name + frequency).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActiveBookmark {
    pub name: String,
    pub frequency: u64,
}

/// Navigation panel containing band presets and frequency bookmarks.
pub struct NavigationPanel {
    /// Band presets group widget.
    pub presets_widget: adw::PreferencesGroup,
    /// Bookmarks container widget.
    pub bookmarks_widget: gtk4::Box,
    /// Band preset combo row (for connection in window.rs).
    pub preset_row: adw::ComboRow,
    /// Bookmark name entry (user-editable, defaults to formatted frequency).
    pub name_entry: adw::EntryRow,
    /// Add bookmark button.
    pub add_button: gtk4::Button,
    /// Bookmark scroll container (height adjusted dynamically).
    pub bookmark_scroll: gtk4::ScrolledWindow,
    /// Bookmark list box (rebuilt on add/remove).
    pub bookmark_list: gtk4::ListBox,
    /// Current bookmarks (shared state for closures).
    pub bookmarks: std::rc::Rc<std::cell::RefCell<Vec<Bookmark>>>,
    /// Callback fired when a preset or bookmark is recalled.
    pub on_navigate: std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>>,
    /// Currently active bookmark identity (for visual highlighting).
    pub active_bookmark: std::rc::Rc<std::cell::RefCell<ActiveBookmark>>,
}

impl NavigationPanel {
    /// Register a callback invoked when the user selects a preset or bookmark.
    pub fn connect_navigate<F: Fn(u64, DemodMode, f64) + 'static>(&self, f: F) {
        *self.on_navigate.borrow_mut() = Some(Box::new(f));
    }
}

/// Build the complete navigation panel (band presets + bookmarks).
pub fn build_navigation_panel() -> NavigationPanel {
    // --- Band Presets ---
    let presets_group = adw::PreferencesGroup::builder()
        .title("Band Presets")
        .description("Quick-tune to common frequencies")
        .build();

    let preset_names: Vec<&str> = BAND_PRESETS.iter().map(|p| p.name).collect();
    let preset_model = gtk4::StringList::new(&preset_names);
    let preset_row = adw::ComboRow::builder()
        .title("Band")
        .model(&preset_model)
        .selected(gtk4::INVALID_LIST_POSITION)
        .build();
    presets_group.add(&preset_row);

    // --- Bookmarks ---
    // Use a plain Box instead of PreferencesGroup so ScrolledWindow sizing works.
    let bookmarks_group = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(8)
        .build();

    let bookmarks_label = gtk4::Label::builder()
        .label("Bookmarks")
        .css_classes(["heading"])
        .halign(gtk4::Align::Start)
        .build();
    bookmarks_group.append(&bookmarks_label);

    let name_entry = adw::EntryRow::builder().title("Name").build();
    bookmarks_group.append(&name_entry);

    let bookmark_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    let bookmark_scroll = gtk4::ScrolledWindow::builder()
        .child(&bookmark_list)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .build();
    bookmarks_group.append(&bookmark_scroll);

    let add_button = gtk4::Button::builder()
        .label("Add Bookmark")
        .css_classes(["suggested-action"])
        .build();
    bookmarks_group.append(&add_button);

    let bookmarks = std::rc::Rc::new(std::cell::RefCell::new(load_bookmarks()));
    let on_navigate: std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));

    let active_bookmark = std::rc::Rc::new(std::cell::RefCell::new(ActiveBookmark::default()));

    // Build initial bookmark list
    rebuild_bookmark_list(
        &bookmark_list,
        &bookmark_scroll,
        &bookmarks,
        &on_navigate,
        &active_bookmark,
        &name_entry,
    );

    // Connect preset row — auto-tune on selection, clear active bookmark
    let on_nav_preset = std::rc::Rc::clone(&on_navigate);
    let active_for_preset = std::rc::Rc::clone(&active_bookmark);
    let entry_for_preset = name_entry.clone();
    let list_for_preset = bookmark_list.downgrade();
    let scroll_for_preset = bookmark_scroll.downgrade();
    let bm_for_preset = std::rc::Rc::clone(&bookmarks);
    preset_row.connect_selected_notify(move |row| {
        let idx = row.selected() as usize;
        if let Some(preset) = BAND_PRESETS.get(idx)
            && let Some(cb) = on_nav_preset.borrow().as_ref()
        {
            // Clear active bookmark — we're tuning via preset, not bookmark.
            *active_for_preset.borrow_mut() = ActiveBookmark::default();
            entry_for_preset.set_text("");
            cb(preset.frequency, preset.demod_mode, preset.bandwidth);
            // Rebuild to remove stale highlight
            if let Some(lb) = list_for_preset.upgrade()
                && let Some(sc) = scroll_for_preset.upgrade()
            {
                rebuild_bookmark_list(
                    &lb,
                    &sc,
                    &bm_for_preset,
                    &on_nav_preset,
                    &active_for_preset,
                    &entry_for_preset,
                );
            }
        }
    });

    NavigationPanel {
        presets_widget: presets_group,
        bookmarks_widget: bookmarks_group,
        preset_row,
        name_entry,
        add_button,
        bookmark_scroll,
        bookmark_list,
        bookmarks,
        on_navigate,
        active_bookmark,
    }
}

/// Approximate height of one `AdwActionRow` with subtitle in pixels.
const BOOKMARK_ROW_HEIGHT: i32 = 56;
/// Maximum visible bookmark rows before scrolling.
const MAX_VISIBLE_BOOKMARKS: i32 = 3;

/// Rebuild the bookmark `ListBox` from the current bookmark list.
#[allow(clippy::too_many_arguments)]
pub fn rebuild_bookmark_list(
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    bookmarks: &std::rc::Rc<std::cell::RefCell<Vec<Bookmark>>>,
    on_navigate: &std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>>,
    active: &std::rc::Rc<std::cell::RefCell<ActiveBookmark>>,
    name_entry: &adw::EntryRow,
) {
    // Remove all existing rows.
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    let bm_list = bookmarks.borrow();
    let current_active = active.borrow().clone();
    for bm in bm_list.iter() {
        let is_active = bm.name == current_active.name && bm.frequency == current_active.frequency;
        let row = adw::ActionRow::builder()
            .title(&bm.name)
            .subtitle(format!(
                "{} — {}",
                format_frequency(bm.frequency),
                bm.demod_mode
            ))
            .activatable(true)
            .build();

        // Highlight the active bookmark with an accent icon.
        if is_active {
            let icon = gtk4::Image::from_icon_name("media-playback-start-symbolic");
            icon.set_valign(gtk4::Align::Center);
            row.add_prefix(&icon);
        }

        // Delete button — identify by name + frequency rather than index
        let delete_btn = gtk4::Button::builder()
            .icon_name("user-trash-symbolic")
            .valign(gtk4::Align::Center)
            .css_classes(["flat"])
            .build();

        let bm_rc = std::rc::Rc::clone(bookmarks);
        let nav_rc = std::rc::Rc::clone(on_navigate);
        let active_rc = std::rc::Rc::clone(active);
        let list_ref = list_box.downgrade();
        let scroll_ref = scroll.downgrade();
        let entry_del = name_entry.clone();
        let del_name = bm.name.clone();
        let del_freq = bm.frequency;
        delete_btn.connect_clicked(move |_| {
            // Clear active state if deleting the active bookmark.
            {
                let active = active_rc.borrow();
                if active.name == del_name && active.frequency == del_freq {
                    drop(active);
                    *active_rc.borrow_mut() = ActiveBookmark::default();
                    entry_del.set_text("");
                }
            }
            bm_rc
                .borrow_mut()
                .retain(|b| !(b.name == del_name && b.frequency == del_freq));
            save_bookmarks(&bm_rc.borrow());
            if let Some(lb) = list_ref.upgrade()
                && let Some(sc) = scroll_ref.upgrade()
            {
                rebuild_bookmark_list(&lb, &sc, &bm_rc, &nav_rc, &active_rc, &entry_del);
            }
        });
        row.add_suffix(&delete_btn);

        // Recall on row activation — set active, update name entry, rebuild list
        let freq = bm.frequency;
        let mode = string_to_demod_mode(&bm.demod_mode);
        let bw = bm.bandwidth;
        let recall_name = bm.name.clone();
        let on_nav_recall = std::rc::Rc::clone(on_navigate);
        let active_recall = std::rc::Rc::clone(active);
        let bm_recall = std::rc::Rc::clone(bookmarks);
        let list_recall = list_box.downgrade();
        let scroll_recall = scroll.downgrade();
        let entry_recall = name_entry.clone();
        row.connect_activated(move |_| {
            // Set this bookmark as active
            *active_recall.borrow_mut() = ActiveBookmark {
                name: recall_name.clone(),
                frequency: freq,
            };
            // Show the active bookmark name in the entry (read-only indication)
            entry_recall.set_text(&recall_name);

            // Fire the navigate callback
            if let Some(cb) = on_nav_recall.borrow().as_ref() {
                cb(freq, mode, bw);
            }

            // Rebuild list to update active highlighting
            if let Some(lb) = list_recall.upgrade()
                && let Some(sc) = scroll_recall.upgrade()
            {
                rebuild_bookmark_list(
                    &lb,
                    &sc,
                    &bm_recall,
                    &on_nav_recall,
                    &active_recall,
                    &entry_recall,
                );
            }
        });

        list_box.append(&row);
    }

    // Dynamically size: show all items up to 3, then scroll.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let count = bm_list.len() as i32;
    let visible = count.clamp(0, MAX_VISIBLE_BOOKMARKS);
    let height = visible * BOOKMARK_ROW_HEIGHT;
    scroll.set_height_request(height);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn format_frequency_mhz() {
        assert_eq!(format_frequency(98_100_000), "98.100 MHz");
        assert_eq!(format_frequency(162_550_000), "162.550 MHz");
    }

    #[test]
    fn format_frequency_ghz() {
        assert_eq!(format_frequency(1_090_000_000), "1.090 GHz");
    }

    #[test]
    fn format_frequency_khz() {
        assert_eq!(format_frequency(500_000), "500.0 kHz");
    }

    #[test]
    fn format_frequency_hz() {
        assert_eq!(format_frequency(440), "440 Hz");
    }

    #[test]
    fn demod_mode_roundtrip() {
        let modes = [
            DemodMode::Wfm,
            DemodMode::Nfm,
            DemodMode::Am,
            DemodMode::Usb,
            DemodMode::Lsb,
            DemodMode::Dsb,
            DemodMode::Cw,
            DemodMode::Raw,
        ];
        for mode in modes {
            let s = demod_mode_to_string(mode);
            let back = string_to_demod_mode(&s);
            assert_eq!(mode, back);
        }
    }

    #[test]
    fn band_presets_have_valid_data() {
        for preset in BAND_PRESETS {
            assert!(!preset.name.is_empty());
            assert!(preset.frequency > 0);
            assert!(preset.bandwidth > 0.0);
        }
    }

    #[test]
    fn bookmark_serialization_roundtrip() {
        let bm = Bookmark::new("Test", 100_000_000, DemodMode::Wfm, 150_000.0);
        let json = serde_json::to_string(&bm).unwrap();
        let back: Bookmark = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "Test");
        assert_eq!(back.frequency, 100_000_000);
        assert_eq!(back.demod_mode, "WFM");
        assert!((back.bandwidth - 150_000.0).abs() < f64::EPSILON);
    }
}
