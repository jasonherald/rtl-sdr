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

/// Snapshot of tuning-profile settings captured from the UI.
///
/// Passed to [`Bookmark::with_profile`] to populate the optional fields.
/// Using a struct avoids long parameter lists and the clippy
/// `fn_params_excessive_bools` lint.
#[allow(clippy::struct_excessive_bools)]
pub struct TuningProfile {
    pub squelch_enabled: bool,
    pub squelch_level: f32,
    pub gain: f64,
    pub agc: bool,
    pub volume: f32,
    pub deemphasis: u32,
    pub nb_enabled: bool,
    pub nb_level: f32,
    pub fm_if_nr: bool,
    pub wfm_stereo: bool,
    pub high_pass: bool,
}

/// A user-saved frequency bookmark with optional tuning profile fields.
///
/// The optional fields use `#[serde(default)]` so existing `bookmarks.json`
/// files (which lack these keys) deserialize without error — the missing
/// fields simply become `None`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Bookmark {
    pub name: String,
    pub frequency: u64,
    pub demod_mode: String,
    pub bandwidth: f64,
    // --- Full tuning profile (all optional for backward compat) ---
    #[serde(default)]
    pub squelch_enabled: Option<bool>,
    #[serde(default)]
    pub squelch_level: Option<f32>,
    #[serde(default)]
    pub gain: Option<f64>,
    #[serde(default)]
    pub agc: Option<bool>,
    #[serde(default)]
    pub volume: Option<f32>,
    #[serde(default)]
    pub deemphasis: Option<u32>,
    #[serde(default)]
    pub nb_enabled: Option<bool>,
    #[serde(default)]
    pub nb_level: Option<f32>,
    #[serde(default)]
    pub fm_if_nr: Option<bool>,
    #[serde(default)]
    pub wfm_stereo: Option<bool>,
    #[serde(default)]
    pub high_pass: Option<bool>,
}

impl Bookmark {
    /// Create a bookmark with only the core tuning state (backward compat).
    pub fn new(name: &str, frequency: u64, demod_mode: DemodMode, bandwidth: f64) -> Self {
        Self {
            name: name.to_string(),
            frequency,
            demod_mode: demod_mode_to_string(demod_mode),
            bandwidth,
            squelch_enabled: None,
            squelch_level: None,
            gain: None,
            agc: None,
            volume: None,
            deemphasis: None,
            nb_enabled: None,
            nb_level: None,
            fm_if_nr: None,
            wfm_stereo: None,
            high_pass: None,
        }
    }

    /// Create a bookmark capturing the full tuning profile.
    pub fn with_profile(
        name: &str,
        frequency: u64,
        demod_mode: DemodMode,
        bandwidth: f64,
        profile: &TuningProfile,
    ) -> Self {
        Self {
            name: name.to_string(),
            frequency,
            demod_mode: demod_mode_to_string(demod_mode),
            bandwidth,
            squelch_enabled: Some(profile.squelch_enabled),
            squelch_level: Some(profile.squelch_level),
            gain: Some(profile.gain),
            agc: Some(profile.agc),
            volume: Some(profile.volume),
            deemphasis: Some(profile.deemphasis),
            nb_enabled: Some(profile.nb_enabled),
            nb_level: Some(profile.nb_level),
            fm_if_nr: Some(profile.fm_if_nr),
            wfm_stereo: Some(profile.wfm_stereo),
            high_pass: Some(profile.high_pass),
        }
    }

    /// Build a compact subtitle: "NFM 495.300 MHz"
    pub fn settings_subtitle(&self) -> String {
        format!("{} {}", self.demod_mode, format_frequency(self.frequency))
    }
}

pub fn demod_mode_to_string(mode: DemodMode) -> String {
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

/// Parse a demod mode string back to a `DemodMode` enum value.
///
/// Unrecognized strings default to `Nfm`.
pub fn parse_demod_mode(s: &str) -> DemodMode {
    string_to_demod_mode(s)
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

/// Callback type for navigation actions.
///
/// Receives the full `Bookmark` so the handler can restore all tuning-profile
/// settings (squelch, gain, de-emphasis, etc.) in addition to frequency, mode,
/// and bandwidth.
pub type NavigationCallback = Box<dyn Fn(&Bookmark)>;

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
    /// Callback fired when the user clicks save on the active bookmark.
    pub on_save: SaveCallback,
}

impl NavigationPanel {
    /// Register a callback invoked when the user selects a preset or bookmark.
    pub fn connect_navigate<F: Fn(&Bookmark) + 'static>(&self, f: F) {
        *self.on_navigate.borrow_mut() = Some(Box::new(f));
    }

    /// Register a callback invoked when the user clicks save on the active bookmark.
    pub fn connect_save<F: Fn() + 'static>(&self, f: F) {
        *self.on_save.borrow_mut() = Some(Box::new(f));
    }
}

/// Build the complete navigation panel (band presets + bookmarks).
#[allow(clippy::too_many_lines)]
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
    let on_save: SaveCallback = std::rc::Rc::new(std::cell::RefCell::new(None));

    // Build initial bookmark list
    rebuild_bookmark_list(
        &bookmark_list,
        &bookmark_scroll,
        &bookmarks,
        &on_navigate,
        &active_bookmark,
        &name_entry,
        &on_save,
    );

    // Connect preset row — auto-tune on selection, clear active bookmark
    let on_nav_preset = std::rc::Rc::clone(&on_navigate);
    let active_for_preset = std::rc::Rc::clone(&active_bookmark);
    let entry_for_preset = name_entry.clone();
    let list_for_preset = bookmark_list.downgrade();
    let scroll_for_preset = bookmark_scroll.downgrade();
    let bm_for_preset = std::rc::Rc::clone(&bookmarks);
    let save_for_preset = std::rc::Rc::clone(&on_save);
    preset_row.connect_selected_notify(move |row| {
        let idx = row.selected() as usize;
        if let Some(preset) = BAND_PRESETS.get(idx)
            && let Some(cb) = on_nav_preset.borrow().as_ref()
        {
            // Clear active bookmark — we're tuning via preset, not bookmark.
            *active_for_preset.borrow_mut() = ActiveBookmark::default();
            entry_for_preset.set_text("");
            let bm = Bookmark::new(
                preset.name,
                preset.frequency,
                preset.demod_mode,
                preset.bandwidth,
            );
            cb(&bm);
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
                    &save_for_preset,
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
        on_save,
    }
}

/// Approximate height of one `AdwActionRow` with subtitle in pixels.
const BOOKMARK_ROW_HEIGHT: i32 = 56;
/// Maximum visible bookmark rows before scrolling.
const MAX_VISIBLE_BOOKMARKS: i32 = 3;

/// Callback type for save actions on the active bookmark.
pub type SaveCallback = std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn()>>>>;

/// Rebuild the bookmark `ListBox` from the current bookmark list.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn rebuild_bookmark_list(
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    bookmarks: &std::rc::Rc<std::cell::RefCell<Vec<Bookmark>>>,
    on_navigate: &std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>>,
    active: &std::rc::Rc<std::cell::RefCell<ActiveBookmark>>,
    name_entry: &adw::EntryRow,
    on_save: &SaveCallback,
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
            .subtitle(bm.settings_subtitle())
            .activatable(true)
            .build();

        // Highlight the active bookmark with an accent icon + save button.
        if is_active {
            let icon = gtk4::Image::from_icon_name("media-playback-start-symbolic");
            icon.set_valign(gtk4::Align::Center);
            row.add_prefix(&icon);

            // Save button — updates the active bookmark with current settings.
            let save_btn = gtk4::Button::builder()
                .icon_name("media-floppy-symbolic")
                .valign(gtk4::Align::Center)
                .tooltip_text("Save current settings to this bookmark")
                .css_classes(["flat"])
                .build();
            let save_cb = std::rc::Rc::clone(on_save);
            save_btn.connect_clicked(move |_| {
                if let Some(cb) = save_cb.borrow().as_ref() {
                    cb();
                }
            });
            row.add_suffix(&save_btn);
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
        let save_del = std::rc::Rc::clone(on_save);
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
                rebuild_bookmark_list(&lb, &sc, &bm_rc, &nav_rc, &active_rc, &entry_del, &save_del);
            }
        });
        row.add_suffix(&delete_btn);

        // Recall on row activation — set active, update name entry, rebuild list
        let recall_bookmark = bm.clone();
        let on_nav_recall = std::rc::Rc::clone(on_navigate);
        let active_recall = std::rc::Rc::clone(active);
        let save_recall = std::rc::Rc::clone(on_save);
        let bm_recall = std::rc::Rc::clone(bookmarks);
        let list_recall = list_box.downgrade();
        let scroll_recall = scroll.downgrade();
        let entry_recall = name_entry.clone();
        row.connect_activated(move |_| {
            // Set this bookmark as active
            *active_recall.borrow_mut() = ActiveBookmark {
                name: recall_bookmark.name.clone(),
                frequency: recall_bookmark.frequency,
            };
            // Show the active bookmark name in the entry (read-only indication)
            entry_recall.set_text(&recall_bookmark.name);

            // Fire the navigate callback with the full bookmark
            if let Some(cb) = on_nav_recall.borrow().as_ref() {
                cb(&recall_bookmark);
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
                    &save_recall,
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
        // Optional fields default to None for basic bookmark
        assert!(back.squelch_enabled.is_none());
        assert!(back.gain.is_none());
    }

    #[test]
    fn bookmark_full_roundtrip() {
        let profile = TuningProfile {
            squelch_enabled: true,
            squelch_level: -40.0,
            gain: 33.8,
            agc: false,
            volume: 0.75,
            deemphasis: 2,
            nb_enabled: false,
            nb_level: 5.0,
            fm_if_nr: true,
            wfm_stereo: true,
            high_pass: false,
        };
        let bm = Bookmark::with_profile("Full", 98_100_000, DemodMode::Wfm, 150_000.0, &profile);
        let json = serde_json::to_string(&bm).unwrap();
        let back: Bookmark = serde_json::from_str(&json).unwrap();
        assert_eq!(back.squelch_enabled, Some(true));
        assert_eq!(back.squelch_level, Some(-40.0));
        assert_eq!(back.gain, Some(33.8));
        assert_eq!(back.agc, Some(false));
        assert_eq!(back.volume, Some(0.75));
        assert_eq!(back.deemphasis, Some(2));
        assert_eq!(back.nb_enabled, Some(false));
        assert_eq!(back.nb_level, Some(5.0));
        assert_eq!(back.fm_if_nr, Some(true));
        assert_eq!(back.wfm_stereo, Some(true));
        assert_eq!(back.high_pass, Some(false));
    }

    #[test]
    fn bookmark_backward_compat_deserialize() {
        // Simulates loading an old bookmarks.json that lacks optional fields.
        let old_json =
            r#"{"name":"Old","frequency":162550000,"demod_mode":"NFM","bandwidth":12500.0}"#;
        let bm: Bookmark = serde_json::from_str(old_json).unwrap();
        assert_eq!(bm.name, "Old");
        assert_eq!(bm.frequency, 162_550_000);
        assert!(bm.squelch_enabled.is_none());
        assert!(bm.gain.is_none());
        assert!(bm.volume.is_none());
    }

    #[test]
    fn bookmark_settings_subtitle_basic() {
        let bm = Bookmark::new("Test", 98_100_000, DemodMode::Wfm, 150_000.0);
        let sub = bm.settings_subtitle();
        assert!(sub.contains("98.100 MHz"));
        assert!(sub.contains("WFM"));
    }

    #[test]
    fn bookmark_settings_subtitle_with_squelch() {
        let mut bm = Bookmark::new("Test", 162_550_000, DemodMode::Nfm, 12_500.0);
        bm.squelch_enabled = Some(true);
        bm.squelch_level = Some(-40.0);
        bm.gain = Some(33.8);
        let sub = bm.settings_subtitle();
        // Compact format: "NFM 162.550 MHz"
        assert!(sub.contains("NFM"));
        assert!(sub.contains("162.550 MHz"));
    }
}
