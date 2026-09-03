//! Navigation panel widgets — the band-preset catalog and combo,
//! the left-sidebar bookmark quick-add, and the preset→bookmarks
//! wiring. Split out of `navigation_panel.rs` per the file-size
//! pass (issue #819).

use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_types::DemodMode;

use super::list::{BookmarkListCtx, rebuild_bookmark_list};
use super::{ActiveBookmark, Bookmark, NavigationPanel};

// ---------------------------------------------------------------------------
// Band presets — static, well-known frequency bands
// ---------------------------------------------------------------------------

/// A predefined frequency band preset.
pub(super) struct BandPreset {
    pub(super) name: &'static str,
    pub(super) frequency: u64,
    pub(super) demod_mode: DemodMode,
    pub(super) bandwidth: f64,
}

/// Common band presets for North America / ITU Region 2.
pub(super) const BAND_PRESETS: &[BandPreset] = &[
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
    // One shared context for the closure — the same handle bundle
    // the flyout's own rebuild paths use, derived through the
    // canonical constructor (#819; CR round 1 on PR #887).
    let ctx = BookmarkListCtx::from_panel(bookmarks, &navigation.name_entry);
    let list_weak = bookmarks.bookmark_list.downgrade();
    let scroll_weak = bookmarks.bookmark_scroll.downgrade();

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
        *ctx.active.borrow_mut() = ActiveBookmark::default();
        ctx.name_entry.set_text("");
        let bm = Bookmark::new(
            preset.name,
            preset.frequency,
            preset.demod_mode,
            preset.bandwidth,
        );
        if let Some(cb) = ctx.on_navigate.borrow().as_ref() {
            cb(&bm);
        }
        // Rebuild to remove stale highlight
        if let Some(lb) = list_weak.upgrade()
            && let Some(sc) = scroll_weak.upgrade()
        {
            rebuild_bookmark_list(&lb, &sc, &ctx);
        }
    });
}
