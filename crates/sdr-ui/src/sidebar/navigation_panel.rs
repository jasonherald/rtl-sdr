//! Navigation panel — band presets and frequency bookmarks.

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_types::DemodMode;

use crate::messages::UiToDsp;

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
    /// Include in scanner rotation. Default false so existing
    /// bookmarks don't start getting scanned without opt-in.
    #[serde(default)]
    pub scan_enabled: bool,
    /// Priority tier. 0 = normal, 1 = priority (checked more
    /// often). Higher tiers reserved for future phases.
    #[serde(default)]
    pub priority: u8,
    /// Per-channel dwell override in ms. `None` → resolved to the
    /// UI-side default at `ScannerChannel` projection time (scanner
    /// itself doesn't own a default; timing defaults live in the
    /// UI layer per the design).
    #[serde(default)]
    pub dwell_ms_override: Option<u32>,
    /// Per-channel hang override in ms. `None` → resolved to the
    /// UI-side default at `ScannerChannel` projection time (same
    /// ownership contract as `dwell_ms_override`).
    #[serde(default)]
    pub hang_ms_override: Option<u32>,
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
            scan_enabled: false,
            priority: 0,
            dwell_ms_override: None,
            hang_ms_override: None,
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
            scan_enabled: false,
            priority: 0,
            dwell_ms_override: None,
            hang_ms_override: None,
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
        DemodMode::Lrpt => "LRPT",
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

// ---------------------------------------------------------------------------
// Scanner projection — bookmark list → `Vec<ScannerChannel>`
// ---------------------------------------------------------------------------

/// Project the in-memory bookmark list into the
/// [`sdr_scanner::ScannerChannel`] form the scanner state machine
/// expects. Filters to `scan_enabled == true`, parses the bookmark's
/// string-form demod mode, and folds per-channel dwell/hang overrides
/// on top of the UI-provided defaults (the scanner itself has no
/// notion of defaults — resolution happens at projection time).
///
/// Pure function: no I/O, no threading, no global state. The channel
/// list order mirrors the bookmark list order, which in turn mirrors
/// save order — keeping the scanner rotation predictable and under
/// user control.
#[must_use]
pub fn project_scanner_channels(
    bookmarks: &[Bookmark],
    default_dwell_ms: u32,
    default_hang_ms: u32,
) -> Vec<sdr_scanner::ScannerChannel> {
    bookmarks
        .iter()
        .filter(|b| b.scan_enabled)
        .map(|b| sdr_scanner::ScannerChannel {
            key: sdr_scanner::ChannelKey {
                name: b.name.clone(),
                frequency_hz: b.frequency,
            },
            demod_mode: parse_demod_mode(&b.demod_mode),
            bandwidth: b.bandwidth,
            ctcss: b.ctcss_mode,
            voice_squelch: b.voice_squelch_mode,
            priority: b.priority,
            dwell_ms: b.dwell_ms_override.unwrap_or(default_dwell_ms),
            hang_ms: b.hang_ms_override.unwrap_or(default_hang_ms),
        })
        .collect()
}

/// Convenience: read the persisted default dwell/hang from config,
/// project the bookmark list into scanner channels, and dispatch
/// `UiToDsp::UpdateScannerChannels` so the running scanner picks up
/// the change on its next tick. Call sites are every bookmark-list
/// mutation (Add, Delete, RR import, scan-toggle, priority-toggle)
/// plus both default-slider notify handlers.
pub fn project_and_push_scanner_channels(
    bookmarks: &[Bookmark],
    state: &std::rc::Rc<crate::state::AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    let default_dwell_ms = crate::sidebar::scanner_panel::load_default_dwell_ms(config);
    let default_hang_ms = crate::sidebar::scanner_panel::load_default_hang_ms(config);
    let channels = project_scanner_channels(bookmarks, default_dwell_ms, default_hang_ms);
    state.send_dsp(UiToDsp::UpdateScannerChannels(channels));
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
        .label("Use the bookmark icon or keyboard shortcut to browse")
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
    let manual_expanded = std::rc::Rc::clone(&bookmarks.manual_expanded);
    let on_mutated = std::rc::Rc::clone(&bookmarks.on_mutated);
    let list_weak = bookmarks.bookmark_list.downgrade();
    let scroll_weak = bookmarks.bookmark_scroll.downgrade();
    let name_entry = navigation.name_entry.clone();

    navigation.preset_row.connect_selected_notify(move |row| {
        let idx = row.selected() as usize;
        let Some(preset) = BAND_PRESETS.get(idx) else {
            return;
        };
        // Apply preset-driven UI state regardless of whether a
        // navigate callback is registered — the active-bookmark
        // reset, name-entry clear, and list rebuild describe
        // "we're tuning via preset, not bookmark" and that's
        // true whether or not anyone's listening. Gating these
        // on `on_nav` being Some would leave stale highlight /
        // name-entry state visible in the rare window between
        // panel construction and callback registration.
        *active.borrow_mut() = ActiveBookmark::default();
        name_entry.set_text("");
        let bm = Bookmark::new(
            preset.name,
            preset.frequency,
            preset.demod_mode,
            preset.bandwidth,
        );
        if let Some(cb) = on_nav.borrow().as_ref() {
            cb(&bm);
        }
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
                &manual_expanded,
                &on_mutated,
            );
        }
    });
}

/// Approximate height of one `AdwActionRow` with subtitle in pixels.
const BOOKMARK_ROW_HEIGHT: i32 = 56;
/// Maximum visible bookmark rows before scrolling.
const MAX_VISIBLE_BOOKMARKS: i32 = 3;

/// Callback type for save actions on the active bookmark.
pub type SaveCallback = std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn()>>>>;

/// Callback invoked whenever the in-memory bookmark list mutates in
/// a way that affects scanner projection (scan checkbox toggled,
/// priority star toggled, row deleted). Window-level wiring installs
/// a closure that re-projects the bookmark list and dispatches
/// `UiToDsp::UpdateScannerChannels`; the callback is `Option`-wrapped
/// so panels can be built standalone (and in tests) without requiring
/// a live `AppState` / `ConfigManager` pair to register it.
pub type BookmarksMutatedCallback = std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn()>>>>;

/// Sentinel category title for bookmarks without an `rr_category`.
/// Only shown when the list contains a mix of categorized and
/// uncategorized bookmarks — pure-uncategorized lists render
/// flat, skipping expander grouping entirely.
const UNCATEGORIZED_LABEL: &str = "Uncategorized";

/// Does the bookmark's name, subtitle, or category contain the
/// (already-lowercased) search needle? Empty needle matches
/// everything. Category matching lets users filter by the
/// `rr_category` label (e.g., typing "dispatch" or "fire")
/// when bookmarks are imported from `RadioReference`, not just
/// by name or demod/frequency.
fn bookmark_matches_filter(bm: &Bookmark, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let name_lc = bm.name.to_lowercase();
    let subtitle_lc = bm.settings_subtitle().to_lowercase();
    let category_lc = bm.rr_category.as_deref().unwrap_or_default().to_lowercase();
    name_lc.contains(needle) || subtitle_lc.contains(needle) || category_lc.contains(needle)
}

/// Rebuild the bookmark `ListBox` from the current bookmark list.
///
/// Honors the search filter in `filter_text` by delegating to
/// [`bookmark_matches_filter`] — lowercase substring match
/// against the bookmark's name, subtitle (demod + frequency),
/// and `rr_category`. Emits an `AdwExpanderRow` per unique
/// `rr_category` when any bookmark is categorized; falls back
/// to a flat list when all bookmarks are uncategorized so users
/// who don't import from `RadioReference` keep the original
/// single-level view.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::implicit_hasher,
    reason = "manual_expanded is a private handle passed Rc-clone-style between internal callers — the default `RandomState` hasher is fine and genericizing would force every caller site to spell the hasher"
)]
pub fn rebuild_bookmark_list(
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    bookmarks: &std::rc::Rc<std::cell::RefCell<Vec<Bookmark>>>,
    on_navigate: &std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>>,
    active: &std::rc::Rc<std::cell::RefCell<ActiveBookmark>>,
    name_entry: &adw::EntryRow,
    on_save: &SaveCallback,
    filter_text: &std::rc::Rc<std::cell::RefCell<String>>,
    manual_expanded: &std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
    on_mutated: &BookmarksMutatedCallback,
) {
    // Remove all existing rows.
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    let bm_list = bookmarks.borrow();
    let current_active = active.borrow().clone();
    let needle = filter_text.borrow().clone();
    let uses_categories = bm_list.iter().any(|b| b.rr_category.is_some());
    // Read manual expansion state separately from widget state.
    // We can't snapshot widget expansion on every rebuild: the
    // search path force-opens every expander, and if the user
    // then clears the search, we'd treat those forced opens as
    // manual intent. Instead `manual_expanded` is only mutated
    // by the `expanded-notify` handler below, and only when no
    // filter is active — so programmatic expansions (search,
    // active-category restore, initial apply on rebuild) never
    // pollute it.
    let manual_open: std::collections::HashSet<String> = manual_expanded.borrow().clone();

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
        // The category containing the active bookmark, so we can
        // guarantee it stays expanded even if the user hadn't
        // opened it before the recall (e.g., clicking through
        // search results into a previously-collapsed section).
        let active_category = bm_list
            .iter()
            .find(|b| b.name == current_active.name && b.frequency == current_active.frequency)
            .map(|b| {
                b.rr_category
                    .clone()
                    .unwrap_or_else(|| UNCATEGORIZED_LABEL.to_string())
            });
        for (cat, items) in groups {
            if items.is_empty() {
                continue;
            }
            let expander = adw::ExpanderRow::builder()
                .title(&cat)
                .subtitle(format!(
                    "{} bookmark{}",
                    items.len(),
                    if items.len() == 1 { "" } else { "s" }
                ))
                .build();
            // Expand when: a filter is active (so matches
            // surface without a manual click); the user
            // manually expanded this category before (preserved
            // across rebuilds via `manual_expanded`); or this
            // category holds the active bookmark (so recall
            // keeps its section open).
            let keep_expanded = !needle.is_empty()
                || manual_open.contains(&cat)
                || active_category.as_deref() == Some(cat.as_str());
            expander.set_expanded(keep_expanded);

            // Track user-driven expansion toggles. Connects
            // *after* the initial `set_expanded` above so the
            // programmatic apply doesn't fire the handler —
            // GLib signals only reach handlers connected at the
            // time of emission. Gated on filter being empty
            // because during search all expanders are force-
            // open; a user toggle under those conditions
            // reflects "show/hide matches in this category
            // right now", not lasting intent.
            let manual_for_notify = std::rc::Rc::clone(manual_expanded);
            let filter_for_notify = std::rc::Rc::clone(filter_text);
            let cat_for_notify = cat.clone();
            expander.connect_expanded_notify(move |row| {
                if !filter_for_notify.borrow().is_empty() {
                    return;
                }
                if row.is_expanded() {
                    manual_for_notify
                        .borrow_mut()
                        .insert(cat_for_notify.clone());
                } else {
                    manual_for_notify.borrow_mut().remove(&cat_for_notify);
                }
            });

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
                    manual_expanded,
                    on_mutated,
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
                manual_expanded,
                on_mutated,
            );
            list_box.append(&row);
        }
    }

    // Left-sidebar legacy sizing only makes sense in flat mode —
    // the flyout is vexpand and doesn't need a min height. Keep
    // the 3-row cap for flat lists (where this function is still
    // called from the sidebar's pre-flyout code path) and skip
    // entirely for expander-grouped views. Use the filtered row
    // count, not the total — when a search is active the scroll
    // region should shrink with the visible list rather than
    // reserving space for filtered-out rows.
    if uses_categories {
        scroll.set_height_request(-1);
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let count = bm_list
            .iter()
            .filter(|bm| bookmark_matches_filter(bm, &needle))
            .count() as i32;
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
    manual_expanded: &std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
    on_mutated: &BookmarksMutatedCallback,
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
        save_btn.update_property(&[gtk4::accessible::Property::Label(
            "Save current settings to this bookmark",
        )]);
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
        .tooltip_text("Delete bookmark")
        .css_classes(["flat"])
        .build();
    delete_btn.update_property(&[gtk4::accessible::Property::Label("Delete bookmark")]);

    let bm_rc = std::rc::Rc::clone(bookmarks);
    let nav_rc = std::rc::Rc::clone(on_navigate);
    let active_rc = std::rc::Rc::clone(active);
    let save_del = std::rc::Rc::clone(on_save);
    let filter_del = std::rc::Rc::clone(filter_text);
    let manual_expanded_del = std::rc::Rc::clone(manual_expanded);
    let on_mutated_del = std::rc::Rc::clone(on_mutated);
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
        // Remove only the first matching bookmark rather than
        // every entry with the same (name, frequency). Quick-add
        // intentionally always creates a new `Bookmark`, so
        // duplicates are a supported state — one click on the
        // trash icon should delete the one row the user pointed
        // at, not wipe the whole set. Stable bookmark IDs will
        // supersede this first-match contract once they land.
        let remove_idx = bm_rc
            .borrow()
            .iter()
            .position(|b| b.name == del_name && b.frequency == del_freq);
        if let Some(idx) = remove_idx {
            bm_rc.borrow_mut().remove(idx);
        }
        save_bookmarks(&bm_rc.borrow());
        // Fire the mutation callback *before* the rebuild so the
        // scanner sees the post-delete channel list in the same
        // tick the UI re-renders — no transient "channel still
        // present" window.
        if let Some(cb) = on_mutated_del.borrow().as_ref() {
            cb();
        }
        if let Some(lb) = list_ref.upgrade()
            && let Some(sc) = scroll_ref.upgrade()
        {
            rebuild_bookmark_list(
                &lb,
                &sc,
                &bm_rc,
                &nav_rc,
                &active_rc,
                &entry_del,
                &save_del,
                &filter_del,
                &manual_expanded_del,
                &on_mutated_del,
            );
        }
    });
    row.add_suffix(&delete_btn);

    // --- Scanner scan-enable checkbox ---
    // Checked = include this bookmark in scanner rotation. Drop
    // the borrow before firing `on_mutated` so the callback can
    // itself borrow the bookmarks list (for projection) without
    // panicking on a nested mutable + immutable borrow.
    let scan_check = gtk4::CheckButton::builder()
        .tooltip_text("Include in scanner")
        .active(bm.scan_enabled)
        .valign(gtk4::Align::Center)
        .build();
    scan_check.update_property(&[gtk4::accessible::Property::Label("Include in scanner")]);
    let bm_scan = std::rc::Rc::clone(bookmarks);
    let on_mutated_scan = std::rc::Rc::clone(on_mutated);
    let scan_name = bm.name.clone();
    let scan_freq = bm.frequency;
    scan_check.connect_toggled(move |btn| {
        let active = btn.is_active();
        {
            let mut bms = bm_scan.borrow_mut();
            if let Some(b) = bms
                .iter_mut()
                .find(|b| b.name == scan_name && b.frequency == scan_freq)
            {
                b.scan_enabled = active;
            }
            save_bookmarks(&bms);
        }
        if let Some(cb) = on_mutated_scan.borrow().as_ref() {
            cb();
        }
    });
    row.add_suffix(&scan_check);

    // --- Scanner priority star toggle ---
    // Toggled = priority 1 (checked more often by the scanner).
    // Phase 1 is binary — higher tiers are reserved for later
    // phases, so the UI exposes on/off rather than a spinner.
    let pri_btn = gtk4::ToggleButton::builder()
        .icon_name(if bm.priority >= 1 {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        })
        .tooltip_text("Scanner priority channel")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .active(bm.priority >= 1)
        .build();
    pri_btn.update_property(&[gtk4::accessible::Property::Label(
        "Scanner priority channel",
    )]);
    let bm_pri = std::rc::Rc::clone(bookmarks);
    let on_mutated_pri = std::rc::Rc::clone(on_mutated);
    let pri_name = bm.name.clone();
    let pri_freq = bm.frequency;
    pri_btn.connect_toggled(move |btn| {
        let active = btn.is_active();
        btn.set_icon_name(if active {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });
        {
            let mut bms = bm_pri.borrow_mut();
            if let Some(b) = bms
                .iter_mut()
                .find(|b| b.name == pri_name && b.frequency == pri_freq)
            {
                b.priority = u8::from(active);
            }
            save_bookmarks(&bms);
        }
        if let Some(cb) = on_mutated_pri.borrow().as_ref() {
            cb();
        }
    });
    row.add_suffix(&pri_btn);

    let recall_bookmark = bm.clone();
    let on_nav_recall = std::rc::Rc::clone(on_navigate);
    let active_recall = std::rc::Rc::clone(active);
    let save_recall = std::rc::Rc::clone(on_save);
    let filter_recall = std::rc::Rc::clone(filter_text);
    let manual_expanded_recall = std::rc::Rc::clone(manual_expanded);
    let on_mutated_recall = std::rc::Rc::clone(on_mutated);
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
                &manual_expanded_recall,
                &on_mutated_recall,
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

    #[test]
    fn filter_matches_rr_category() {
        // Users who import from RadioReference expect to search
        // by category ("dispatch", "fire") even when that text
        // isn't in the bookmark's name or subtitle.
        let mut bm = Bookmark::new("Unit 14", 462_562_500, DemodMode::Nfm, 12_500.0);
        bm.rr_category = Some("Law Dispatch".to_string());
        assert!(bookmark_matches_filter(&bm, "dispatch"));
        assert!(bookmark_matches_filter(&bm, "law"));
        assert!(!bookmark_matches_filter(&bm, "fire"));
    }

    #[test]
    fn bookmark_scanner_fields_default_on_old_json() {
        // Old pre-scanner bookmark JSON (no scanner fields present).
        let old_json =
            r#"{"name":"Old","frequency":162550000,"demod_mode":"NFM","bandwidth":12500.0}"#;
        let bm: Bookmark = serde_json::from_str(old_json).unwrap();
        assert!(!bm.scan_enabled);
        assert_eq!(bm.priority, 0);
        assert!(bm.dwell_ms_override.is_none());
        assert!(bm.hang_ms_override.is_none());
    }

    #[test]
    fn bookmark_scanner_fields_roundtrip() {
        /// Override dwell in ms; distinct from the scanner's
        /// `DEFAULT_DWELL_MS` (100) so the roundtrip assertion
        /// would fail if serde dropped the override field and
        /// the default re-hydrated.
        const TEST_SCANNER_DWELL_MS: u32 = 200;
        /// Override hang in ms; same rationale — distinct from
        /// the scanner default (2000).
        const TEST_SCANNER_HANG_MS: u32 = 3_000;

        let mut bm = Bookmark::new("Test", 146_520_000, DemodMode::Nfm, 12_500.0);
        bm.scan_enabled = true;
        bm.priority = 1;
        bm.dwell_ms_override = Some(TEST_SCANNER_DWELL_MS);
        bm.hang_ms_override = Some(TEST_SCANNER_HANG_MS);
        let json = serde_json::to_string(&bm).unwrap();
        let back: Bookmark = serde_json::from_str(&json).unwrap();
        assert!(back.scan_enabled);
        assert_eq!(back.priority, 1);
        assert_eq!(back.dwell_ms_override, Some(TEST_SCANNER_DWELL_MS));
        assert_eq!(back.hang_ms_override, Some(TEST_SCANNER_HANG_MS));
    }

    // ---- project_scanner_channels ----

    /// Test default dwell, distinct from `sdr_scanner::DEFAULT_DWELL_MS`
    /// (100) so the default-vs-override assertions can tell which one
    /// the projector resolved.
    const TEST_DEFAULT_DWELL_MS: u32 = 125;
    /// Test default hang, distinct from `sdr_scanner::DEFAULT_HANG_MS`
    /// (2000) for the same reason.
    const TEST_DEFAULT_HANG_MS: u32 = 1_500;
    /// A per-channel dwell override distinct from both the scanner
    /// default and `TEST_DEFAULT_DWELL_MS`, so "override wins" is
    /// observable as a unique value in assertions.
    const TEST_OVERRIDE_DWELL_MS: u32 = 250;
    /// A per-channel hang override distinct from both the scanner
    /// default and `TEST_DEFAULT_HANG_MS`, same rationale.
    const TEST_OVERRIDE_HANG_MS: u32 = 3_250;

    #[test]
    fn project_scanner_channels_filters_disabled() {
        let mut enabled = Bookmark::new("On", 146_520_000, DemodMode::Nfm, 12_500.0);
        enabled.scan_enabled = true;
        let disabled = Bookmark::new("Off", 162_550_000, DemodMode::Nfm, 12_500.0);
        let bms = vec![enabled, disabled];

        let channels = project_scanner_channels(&bms, TEST_DEFAULT_DWELL_MS, TEST_DEFAULT_HANG_MS);

        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].key.name, "On");
        assert_eq!(channels[0].key.frequency_hz, 146_520_000);
    }

    #[test]
    fn project_scanner_channels_override_wins_over_default() {
        let mut with_overrides = Bookmark::new("Overrides", 146_520_000, DemodMode::Nfm, 12_500.0);
        with_overrides.scan_enabled = true;
        with_overrides.dwell_ms_override = Some(TEST_OVERRIDE_DWELL_MS);
        with_overrides.hang_ms_override = Some(TEST_OVERRIDE_HANG_MS);
        let mut no_overrides = Bookmark::new("Defaults", 162_550_000, DemodMode::Nfm, 12_500.0);
        no_overrides.scan_enabled = true;
        let bms = vec![with_overrides, no_overrides];

        let channels = project_scanner_channels(&bms, TEST_DEFAULT_DWELL_MS, TEST_DEFAULT_HANG_MS);

        assert_eq!(channels.len(), 2);
        // First bookmark: override wins.
        assert_eq!(channels[0].dwell_ms, TEST_OVERRIDE_DWELL_MS);
        assert_eq!(channels[0].hang_ms, TEST_OVERRIDE_HANG_MS);
        // Second bookmark: no overrides, default folds in.
        assert_eq!(channels[1].dwell_ms, TEST_DEFAULT_DWELL_MS);
        assert_eq!(channels[1].hang_ms, TEST_DEFAULT_HANG_MS);
    }

    #[test]
    fn project_scanner_channels_propagates_priority_and_squelch() {
        let mut bm = Bookmark::new("Priority", 146_520_000, DemodMode::Nfm, 12_500.0);
        bm.scan_enabled = true;
        bm.priority = 1;
        bm.ctcss_mode = Some(sdr_radio::af_chain::CtcssMode::Tone(100.0));
        bm.voice_squelch_mode =
            Some(sdr_dsp::voice_squelch::VoiceSquelchMode::Snr { threshold_db: 9.0 });

        let channels = project_scanner_channels(&[bm], TEST_DEFAULT_DWELL_MS, TEST_DEFAULT_HANG_MS);

        assert_eq!(channels.len(), 1);
        let ch = &channels[0];
        assert_eq!(ch.priority, 1);
        assert_eq!(ch.ctcss, Some(sdr_radio::af_chain::CtcssMode::Tone(100.0)));
        assert_eq!(
            ch.voice_squelch,
            Some(sdr_dsp::voice_squelch::VoiceSquelchMode::Snr { threshold_db: 9.0 })
        );
        // Demod mode is parsed from the string form on the bookmark.
        assert_eq!(ch.demod_mode, DemodMode::Nfm);
        assert!((ch.bandwidth - 12_500.0).abs() < f64::EPSILON);
    }
}
