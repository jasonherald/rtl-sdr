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
    pub auto_squelch_enabled: bool,
    pub squelch_level: f32,
    pub gain: f64,
    /// Three-way AGC selection — replaces the pre-#354 `agc: bool`.
    /// The save path populates both the new `agc_type` and the
    /// legacy `agc: Option<bool>` on the persisted `Bookmark` so
    /// older builds loading the file still get a sensible
    /// (if reduced) restore.
    pub agc_type: crate::sidebar::source_panel::AgcType,
    pub volume: Option<f32>,
    pub deemphasis: u32,
    pub nb_enabled: bool,
    pub nb_level: f32,
    pub fm_if_nr: bool,
    pub wfm_stereo: bool,
    pub high_pass: Option<bool>,
    /// CTCSS sub-audible tone squelch mode. `None` means "don't
    /// touch the current setting on restore" — bookmarks saved
    /// before PR 3 are all `None` after deserialization so they
    /// preserve the user's current CTCSS setting when loaded.
    pub ctcss_mode: Option<sdr_radio::af_chain::CtcssMode>,
    /// CTCSS detection threshold (normalized magnitude, `(0, 1]`).
    /// Same backward-compat semantics as `ctcss_mode`.
    pub ctcss_threshold: Option<f32>,
    /// Voice-activity squelch mode — tagged enum carrying its
    /// threshold inline. Same backward-compat contract as CTCSS:
    /// `None` on restore means leave the current voice squelch
    /// setting alone.
    pub voice_squelch_mode: Option<sdr_dsp::voice_squelch::VoiceSquelchMode>,
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
    pub auto_squelch_enabled: Option<bool>,
    #[serde(default)]
    pub squelch_level: Option<f32>,
    #[serde(default)]
    pub gain: Option<f64>,
    /// Legacy hardware-AGC flag — `Some(true)` meant tuner AGC
    /// on, `Some(false)` meant manual gain. Preserved for read-
    /// path compatibility with bookmarks saved before #354
    /// landed; superseded by `agc_type` for new bookmarks. Save
    /// path writes both fields when AGC is `Off` or `Hardware`
    /// so older builds loading a new bookmark still get a
    /// sensible (if reduced) restore.
    #[serde(default)]
    pub agc: Option<bool>,
    /// Three-way AGC selection (Off / Hardware / Software).
    /// Added with #354 / #356. Pre-existing bookmarks deserialize
    /// to `None`; the restore path falls back to the legacy
    /// `agc: Option<bool>` field mapping `true → Hardware` and
    /// `false → Off`. When both fields are present the new
    /// `agc_type` wins.
    #[serde(default)]
    pub agc_type: Option<crate::sidebar::source_panel::AgcType>,
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
    /// `RadioReference` category (e.g., "Law Dispatch"). Metadata for future
    /// bookmark tree organization.
    #[serde(default)]
    pub rr_category: Option<String>,
    /// `RadioReference` frequency ID for duplicate detection and future sync.
    #[serde(default)]
    pub rr_import_id: Option<String>,
    /// CTCSS sub-audible tone squelch mode. Added in PR 3 of #269.
    /// Serialized as the tagged form `{"kind":"off"}` or
    /// `{"kind":"tone","hz":100.0}`. Pre-PR-3 bookmarks lack this
    /// key and deserialize to `None`, which the restore path
    /// interprets as "leave the current CTCSS setting alone".
    #[serde(default)]
    pub ctcss_mode: Option<sdr_radio::af_chain::CtcssMode>,
    /// CTCSS detection threshold in `(0, 1]`. Same backward-compat
    /// semantics as `ctcss_mode`.
    #[serde(default)]
    pub ctcss_threshold: Option<f32>,
    /// Voice-activity squelch mode — tagged `VoiceSquelchMode`
    /// enum carrying its threshold inline. Added in the voice-
    /// squelch PR; pre-PR bookmarks deserialize to `None` which
    /// the restore path interprets as "leave the current voice
    /// squelch setting alone."
    #[serde(default)]
    pub voice_squelch_mode: Option<sdr_dsp::voice_squelch::VoiceSquelchMode>,
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
            auto_squelch_enabled: Some(profile.auto_squelch_enabled),
            squelch_level: Some(profile.squelch_level),
            gain: Some(profile.gain),
            // Populate both the new `agc_type` AND the legacy
            // `agc` field so a bookmark saved on a post-#354
            // build still round-trips through a pre-#354 build
            // as a sensible (if reduced) setting. Software AGC
            // has no legacy representation — map it to `false`
            // (AGC off) on the legacy path, which is the safer
            // default than "hardware on" for users who haven't
            // opted into either AGC type.
            agc: Some(matches!(
                profile.agc_type,
                crate::sidebar::source_panel::AgcType::Hardware
            )),
            agc_type: Some(profile.agc_type),
            volume: profile.volume,
            deemphasis: Some(profile.deemphasis),
            nb_enabled: Some(profile.nb_enabled),
            nb_level: Some(profile.nb_level),
            fm_if_nr: Some(profile.fm_if_nr),
            wfm_stereo: Some(profile.wfm_stereo),
            high_pass: profile.high_pass,
            rr_category: None,
            rr_import_id: None,
            ctcss_mode: profile.ctcss_mode,
            ctcss_threshold: profile.ctcss_threshold,
            voice_squelch_mode: profile.voice_squelch_mode,
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

pub fn load_bookmarks() -> Vec<Bookmark> {
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

/// Navigation panel containing band presets and the left-sidebar
/// "Add Bookmark" quick-entry controls.
///
/// The full bookmark list (browse, recall, delete, save-over-active)
/// lives in [`crate::sidebar::BookmarksPanel`] — the right-side
/// flyout — so this panel intentionally does **not** own the list
/// widget or the backing store. Both panels share the `name_entry`:
/// this struct owns it, the flyout panel borrows it at build time
/// and captures clones in its row callbacks.
pub struct NavigationPanel {
    /// Band presets group widget.
    pub presets_widget: adw::PreferencesGroup,
    /// Left-sidebar bookmark quick-add container (heading +
    /// name entry + Add button). The full list is in the flyout
    /// — see [`crate::sidebar::BookmarksPanel`].
    pub bookmarks_widget: gtk4::Box,
    /// Band preset combo row (for connection in window.rs).
    pub preset_row: adw::ComboRow,
    /// Bookmark name entry (user-editable, defaults to formatted frequency).
    /// Owned here because the Add button sits next to it; the
    /// flyout panel borrows a reference for its row actions.
    pub name_entry: adw::EntryRow,
    /// Add bookmark button. Lives on the left sidebar so users
    /// can stash a bookmark without opening the flyout.
    pub add_button: gtk4::Button,
}

/// Build the navigation panel — band presets + left-sidebar
/// bookmark quick-add.
///
/// Does not build the bookmark list widget; that lives in the
/// right-side flyout and is constructed by
/// [`build_bookmarks_panel`](crate::sidebar::build_bookmarks_panel).
/// The preset row's selection handler also lives outside this
/// function — see [`connect_preset_to_bookmarks`] — because it
/// needs access to the flyout's shared state (active-bookmark
/// highlight, list rebuild, navigation callback) which only
/// exists after both panels have been built.
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

    // --- Left-sidebar bookmark quick-add ---
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

    let bookmarks_hint = gtk4::Label::builder()
        .label("Press Ctrl+B or use the bookmark icon to browse")
        .css_classes(["caption", "dim-label"])
        .halign(gtk4::Align::Start)
        .wrap(true)
        .build();
    bookmarks_group.append(&bookmarks_hint);

    let name_entry = adw::EntryRow::builder().title("Name").build();
    bookmarks_group.append(&name_entry);

    let add_button = gtk4::Button::builder()
        .label("Add Bookmark")
        .css_classes(["suggested-action"])
        .build();
    bookmarks_group.append(&add_button);

    NavigationPanel {
        presets_widget: presets_group,
        bookmarks_widget: bookmarks_group,
        preset_row,
        name_entry,
        add_button,
    }
}

/// Wire the band-preset combo row to the bookmark flyout state.
///
/// Selecting a preset clears the active-bookmark highlight,
/// clears the name entry, fires the shared navigate callback,
/// and rebuilds the flyout's bookmark list. This wiring can't
/// live inside [`build_navigation_panel`] because the state it
/// closes over is owned by the flyout panel, which is built
/// afterwards.
pub fn connect_preset_to_bookmarks(
    navigation: &NavigationPanel,
    bookmarks: &crate::sidebar::BookmarksPanel,
) {
    let on_nav = std::rc::Rc::clone(&bookmarks.on_navigate);
    let active = std::rc::Rc::clone(&bookmarks.active_bookmark);
    let bm_rc = std::rc::Rc::clone(&bookmarks.bookmarks);
    let on_save = std::rc::Rc::clone(&bookmarks.on_save);
    let filter_text = std::rc::Rc::clone(&bookmarks.filter_text);
    let list_weak = bookmarks.bookmark_list.downgrade();
    let scroll_weak = bookmarks.bookmark_scroll.downgrade();
    let name_entry = navigation.name_entry.clone();

    navigation.preset_row.connect_selected_notify(move |row| {
        let idx = row.selected() as usize;
        if let Some(preset) = BAND_PRESETS.get(idx)
            && let Some(cb) = on_nav.borrow().as_ref()
        {
            // Clear active bookmark — we're tuning via preset, not bookmark.
            *active.borrow_mut() = ActiveBookmark::default();
            name_entry.set_text("");
            let bm = Bookmark::new(
                preset.name,
                preset.frequency,
                preset.demod_mode,
                preset.bandwidth,
            );
            cb(&bm);
            // Rebuild to remove stale highlight
            if let Some(lb) = list_weak.upgrade()
                && let Some(sc) = scroll_weak.upgrade()
            {
                rebuild_bookmark_list(
                    &lb,
                    &sc,
                    &bm_rc,
                    &on_nav,
                    &active,
                    &name_entry,
                    &on_save,
                    &filter_text,
                );
            }
        }
    });
}

/// Approximate height of one `AdwActionRow` with subtitle in pixels.
const BOOKMARK_ROW_HEIGHT: i32 = 56;
/// Maximum visible bookmark rows before scrolling.
const MAX_VISIBLE_BOOKMARKS: i32 = 3;

/// Callback type for save actions on the active bookmark.
pub type SaveCallback = std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn()>>>>;

/// Sentinel category title for bookmarks without an `rr_category`.
/// Only shown when the list contains a mix of categorized and
/// uncategorized bookmarks — pure-uncategorized lists render
/// flat, skipping expander grouping entirely.
const UNCATEGORIZED_LABEL: &str = "Uncategorized";

/// Does the bookmark's name or subtitle contain the (already-
/// lowercased) search needle? Empty needle matches everything.
fn bookmark_matches_filter(bm: &Bookmark, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let name_lc = bm.name.to_lowercase();
    let subtitle_lc = bm.settings_subtitle().to_lowercase();
    name_lc.contains(needle) || subtitle_lc.contains(needle)
}

/// Rebuild the bookmark `ListBox` from the current bookmark list.
///
/// Honors the search filter in `filter_text` (lowercase substring
/// match against name + subtitle). Emits an `AdwExpanderRow` per
/// unique `rr_category` when any bookmark is categorized; falls
/// back to a flat list when all bookmarks are uncategorized so
/// users who don't import from `RadioReference` keep the original
/// single-level view.
#[allow(clippy::too_many_arguments)]
pub fn rebuild_bookmark_list(
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    bookmarks: &std::rc::Rc<std::cell::RefCell<Vec<Bookmark>>>,
    on_navigate: &std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>>,
    active: &std::rc::Rc<std::cell::RefCell<ActiveBookmark>>,
    name_entry: &adw::EntryRow,
    on_save: &SaveCallback,
    filter_text: &std::rc::Rc<std::cell::RefCell<String>>,
) {
    // Remove all existing rows.
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    let bm_list = bookmarks.borrow();
    let current_active = active.borrow().clone();
    let needle = filter_text.borrow().clone();
    let uses_categories = bm_list.iter().any(|b| b.rr_category.is_some());

    if uses_categories {
        // Collect bookmarks into category buckets, preserving
        // within-category insertion order. `BTreeMap` gives
        // deterministic alphabetical category ordering; use
        // a single `Uncategorized` bucket for loose bookmarks.
        let mut groups: std::collections::BTreeMap<String, Vec<&Bookmark>> =
            std::collections::BTreeMap::new();
        for bm in bm_list.iter() {
            if !bookmark_matches_filter(bm, &needle) {
                continue;
            }
            let cat = bm
                .rr_category
                .clone()
                .unwrap_or_else(|| UNCATEGORIZED_LABEL.to_string());
            groups.entry(cat).or_default().push(bm);
        }
        for (cat, items) in groups {
            if items.is_empty() {
                continue;
            }
            let expander = adw::ExpanderRow::builder()
                .title(&cat)
                .subtitle(format!("{} bookmark{}", items.len(), if items.len() == 1 { "" } else { "s" }))
                .build();
            // Auto-expand when a filter is active so matches
            // surface without a manual click; leave collapsed
            // otherwise to keep long lists scannable.
            expander.set_expanded(!needle.is_empty());
            for bm in items {
                let row = build_bookmark_row(
                    bm,
                    &current_active,
                    list_box,
                    scroll,
                    bookmarks,
                    on_navigate,
                    active,
                    name_entry,
                    on_save,
                    filter_text,
                );
                expander.add_row(&row);
            }
            list_box.append(&expander);
        }
    } else {
        for bm in bm_list.iter() {
            if !bookmark_matches_filter(bm, &needle) {
                continue;
            }
            let row = build_bookmark_row(
                bm,
                &current_active,
                list_box,
                scroll,
                bookmarks,
                on_navigate,
                active,
                name_entry,
                on_save,
                filter_text,
            );
            list_box.append(&row);
        }
    }

    // Left-sidebar legacy sizing only makes sense in flat mode —
    // the flyout is vexpand and doesn't need a min height. Keep
    // the 3-row cap for flat lists (where this function is still
    // called from the sidebar's pre-flyout code path) and skip
    // entirely for expander-grouped views.
    if uses_categories {
        scroll.set_height_request(-1);
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let count = bm_list.len() as i32;
        let visible = count.clamp(0, MAX_VISIBLE_BOOKMARKS);
        let height = visible * BOOKMARK_ROW_HEIGHT;
        scroll.set_height_request(height);
    }
}

/// Build a single bookmark `ActionRow` with its active-highlight
/// prefix, save/delete suffix buttons, and recall-on-activate
/// handler wired up.
///
/// Shared between the flat and categorized rebuild paths so the
/// row's behavior is identical regardless of whether it's a
/// top-level child of the `ListBox` or nested under an
/// `AdwExpanderRow`.
#[allow(clippy::too_many_arguments)]
fn build_bookmark_row(
    bm: &Bookmark,
    current_active: &ActiveBookmark,
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    bookmarks: &std::rc::Rc<std::cell::RefCell<Vec<Bookmark>>>,
    on_navigate: &std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>>,
    active: &std::rc::Rc<std::cell::RefCell<ActiveBookmark>>,
    name_entry: &adw::EntryRow,
    on_save: &SaveCallback,
    filter_text: &std::rc::Rc<std::cell::RefCell<String>>,
) -> adw::ActionRow {
    let is_active = bm.name == current_active.name && bm.frequency == current_active.frequency;
    let row = adw::ActionRow::builder()
        .title(&bm.name)
        .subtitle(bm.settings_subtitle())
        .activatable(true)
        .build();

    if is_active {
        let icon = gtk4::Image::from_icon_name("media-playback-start-symbolic");
        icon.set_valign(gtk4::Align::Center);
        row.add_prefix(&icon);

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

    let delete_btn = gtk4::Button::builder()
        .icon_name("user-trash-symbolic")
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();

    let bm_rc = std::rc::Rc::clone(bookmarks);
    let nav_rc = std::rc::Rc::clone(on_navigate);
    let active_rc = std::rc::Rc::clone(active);
    let save_del = std::rc::Rc::clone(on_save);
    let filter_del = std::rc::Rc::clone(filter_text);
    let list_ref = list_box.downgrade();
    let scroll_ref = scroll.downgrade();
    let entry_del = name_entry.clone();
    let del_name = bm.name.clone();
    let del_freq = bm.frequency;
    delete_btn.connect_clicked(move |_| {
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
            rebuild_bookmark_list(
                &lb, &sc, &bm_rc, &nav_rc, &active_rc, &entry_del, &save_del, &filter_del,
            );
        }
    });
    row.add_suffix(&delete_btn);

    let recall_bookmark = bm.clone();
    let on_nav_recall = std::rc::Rc::clone(on_navigate);
    let active_recall = std::rc::Rc::clone(active);
    let save_recall = std::rc::Rc::clone(on_save);
    let filter_recall = std::rc::Rc::clone(filter_text);
    let bm_recall = std::rc::Rc::clone(bookmarks);
    let list_recall = list_box.downgrade();
    let scroll_recall = scroll.downgrade();
    let entry_recall = name_entry.clone();
    row.connect_activated(move |_| {
        *active_recall.borrow_mut() = ActiveBookmark {
            name: recall_bookmark.name.clone(),
            frequency: recall_bookmark.frequency,
        };
        entry_recall.set_text(&recall_bookmark.name);

        if let Some(cb) = on_nav_recall.borrow().as_ref() {
            cb(&recall_bookmark);
        }

        if let Some(lb) = list_recall.upgrade()
            && let Some(sc) = scroll_recall.upgrade()
        {
            let saved_scroll = sc.vadjustment().value();
            rebuild_bookmark_list(
                &lb,
                &sc,
                &bm_recall,
                &on_nav_recall,
                &active_recall,
                &entry_recall,
                &save_recall,
                &filter_recall,
            );
            let adj = sc.vadjustment();
            glib::idle_add_local_once(move || {
                adj.set_value(saved_scroll);
            });
        }
    });

    row
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
            auto_squelch_enabled: true,
            squelch_level: -40.0,
            gain: 33.8,
            agc_type: crate::sidebar::source_panel::AgcType::Off,
            volume: Some(0.75),
            deemphasis: 2,
            nb_enabled: false,
            nb_level: 5.0,
            fm_if_nr: true,
            wfm_stereo: true,
            high_pass: Some(false),
            ctcss_mode: Some(sdr_radio::af_chain::CtcssMode::Tone(100.0)),
            ctcss_threshold: Some(0.15),
            voice_squelch_mode: Some(sdr_dsp::voice_squelch::VoiceSquelchMode::Snr {
                threshold_db: 9.0,
            }),
        };
        let bm = Bookmark::with_profile("Full", 98_100_000, DemodMode::Wfm, 150_000.0, &profile);
        let json = serde_json::to_string(&bm).unwrap();
        let back: Bookmark = serde_json::from_str(&json).unwrap();
        assert_eq!(back.squelch_enabled, Some(true));
        assert_eq!(back.auto_squelch_enabled, Some(true));
        assert_eq!(back.squelch_level, Some(-40.0));
        assert_eq!(back.gain, Some(33.8));
        // Legacy `agc` boolean is written alongside the new
        // `agc_type` for forward-compat with older builds. For
        // `AgcType::Off` that legacy value is `false`.
        assert_eq!(back.agc, Some(false));
        // New `agc_type` round-trip. Regression guard against a
        // serde-shape change on `AgcType` silently breaking the
        // bookmark schema.
        assert_eq!(
            back.agc_type,
            Some(crate::sidebar::source_panel::AgcType::Off)
        );
        assert_eq!(back.volume, Some(0.75));
        assert_eq!(back.deemphasis, Some(2));
        assert_eq!(back.nb_enabled, Some(false));
        assert_eq!(back.nb_level, Some(5.0));
        assert_eq!(back.fm_if_nr, Some(true));
        assert_eq!(back.wfm_stereo, Some(true));
        assert_eq!(back.high_pass, Some(false));
        assert_eq!(
            back.ctcss_mode,
            Some(sdr_radio::af_chain::CtcssMode::Tone(100.0))
        );
        assert_eq!(back.ctcss_threshold, Some(0.15));
        assert_eq!(
            back.voice_squelch_mode,
            Some(sdr_dsp::voice_squelch::VoiceSquelchMode::Snr { threshold_db: 9.0 })
        );
    }

    #[test]
    fn bookmark_backward_compat_ctcss_none() {
        // Old bookmark JSON (pre-PR-3) has neither ctcss_mode nor
        // ctcss_threshold. Deserialization must yield None for
        // both, which restore_bookmark_profile interprets as
        // "leave the current CTCSS setting alone."
        let old_json =
            r#"{"name":"Legacy","frequency":162550000,"demod_mode":"NFM","bandwidth":12500.0}"#;
        let bm: Bookmark = serde_json::from_str(old_json).unwrap();
        assert!(bm.ctcss_mode.is_none());
        assert!(bm.ctcss_threshold.is_none());
        // Voice squelch is newer than CTCSS, so pre-PR bookmarks
        // also lack this key — must deserialize to None which
        // restore_bookmark_profile interprets as "leave current
        // voice squelch setting alone."
        assert!(bm.voice_squelch_mode.is_none());
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
        assert!(bm.auto_squelch_enabled.is_none());
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

    #[test]
    fn filter_empty_needle_matches_everything() {
        let bm = Bookmark::new("Anything", 162_550_000, DemodMode::Nfm, 12_500.0);
        assert!(bookmark_matches_filter(&bm, ""));
    }

    #[test]
    fn filter_matches_name_case_insensitive() {
        let bm = Bookmark::new("Weather", 162_550_000, DemodMode::Nfm, 12_500.0);
        // `bookmark_matches_filter` assumes the caller has already
        // lowercased the needle (the search-entry handler does so).
        assert!(bookmark_matches_filter(&bm, "weather"));
        assert!(!bookmark_matches_filter(&bm, "aviation"));
    }

    #[test]
    fn filter_matches_subtitle_demod_and_frequency() {
        let bm = Bookmark::new("Stuff", 162_550_000, DemodMode::Nfm, 12_500.0);
        // Subtitle is "NFM 162.550 MHz" — lowercase matches both parts.
        assert!(bookmark_matches_filter(&bm, "nfm"));
        assert!(bookmark_matches_filter(&bm, "162.550"));
        assert!(bookmark_matches_filter(&bm, "mhz"));
    }

    #[test]
    fn filter_no_match_hides_bookmark() {
        let bm = Bookmark::new("Weather", 162_550_000, DemodMode::Nfm, 12_500.0);
        assert!(!bookmark_matches_filter(&bm, "xyz-no-match"));
    }
}
