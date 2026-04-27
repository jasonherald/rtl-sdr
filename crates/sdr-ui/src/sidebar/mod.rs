//! Sidebar configuration panels — source, audio, radio, display, navigation.
//!
//! The transcript panel lives in a separate right-side revealer, not here.

use std::rc::Rc;

pub mod activity_bar;
pub mod audio_panel;
pub mod bookmarks_panel;
pub mod display_panel;
pub mod general_panel;
pub mod navigation_panel;
pub mod radio_panel;
pub mod satellites_notify;
pub mod satellites_panel;
pub mod satellites_recorder;
pub mod scanner_panel;
pub mod server_panel;
pub mod source_panel;
pub mod transcript_panel;

pub use activity_bar::{
    ActivityBar, ActivityBarEntry, ActivityBarSide, LEFT_ACTIVITIES, RIGHT_ACTIVITIES,
    build_activity_bar,
};
pub use audio_panel::{AudioPanel, build_audio_panel};
pub use bookmarks_panel::{BookmarksPanel, build_bookmarks_panel};
pub use display_panel::{DisplayPanel, build_display_panel};
pub use general_panel::{GeneralPanel, build_general_panel};
pub use navigation_panel::{NavigationPanel, build_navigation_panel};
pub use radio_panel::{RadioPanel, build_radio_panel};
pub use satellites_panel::{SatellitesPanel, build_satellites_panel};
pub use scanner_panel::{ScannerPanel, build_scanner_panel};
pub use server_panel::{ServerPanel, build_server_panel};
pub use source_panel::{SourcePanel, build_source_panel};
pub use transcript_panel::{TranscriptPanel, build_transcript_panel};

/// All sidebar panels, for DSP bridge wiring.
pub struct SidebarPanels {
    /// Source device configuration.
    pub source: SourcePanel,
    /// Audio output configuration.
    pub audio: AudioPanel,
    /// Radio / demodulator configuration.
    pub radio: RadioPanel,
    /// Display / spectrum settings.
    pub display: DisplayPanel,
    /// Navigation — band presets and the left-sidebar bookmark
    /// quick-add (name entry + Add button). The full bookmark
    /// list lives in [`bookmarks`](Self::bookmarks).
    pub navigation: NavigationPanel,
    /// Right-side bookmarks flyout — owns the bookmark list,
    /// backing store, and row-action callbacks. Toggled via
    /// the header bookmark button / `Ctrl+B`.
    ///
    /// Wrapped in `Rc` so long-lived GTK closures (RR import,
    /// Add button, Save button) can capture a clone and call
    /// [`BookmarksPanel::rebuild`] without hand-threading each
    /// internal `Rc` field. The Save closure uses `Rc::downgrade`
    /// to break the otherwise-cyclic `on_save → stored closure`
    /// reference chain.
    pub bookmarks: Rc<BookmarksPanel>,
    /// Share-over-network (`rtl_tcp` server) controls. Packed as
    /// the `"share"` child of the left activity stack, so the panel
    /// is always reachable via the 📡 Share icon; the Start switch
    /// errors gracefully when no dongle is plugged in.
    pub server: ServerPanel,
    /// Scanner control panel at bottom of left sidebar (Phase 1,
    /// issue #317). Master switch, active-channel / state
    /// display, lockout button, default dwell/hang sliders.
    pub scanner: ScannerPanel,
    /// Satellites scheduler — ground station settings, TLE refresh
    /// status, upcoming passes, auto-record toggle. Drives the
    /// "Satellites" left activity (#481), with the auto-record
    /// switch driving #482's APT-on-pass wiring.
    pub satellites: SatellitesPanel,
}

/// Build every sidebar panel. Activity-bar migration: each panel
/// widget is packed individually into its matching `GtkStack` child
/// by `window.rs::build_layout`, so this builder no longer wraps
/// them in a shared `ScrolledWindow`. The returned `SidebarPanels`
/// carries the full panel set for both widget placement and DSP
/// bridge signal wiring (see issue #92).
pub fn build_panels() -> SidebarPanels {
    let source = build_source_panel();
    let server = build_server_panel();
    let audio = build_audio_panel();
    let radio = build_radio_panel();
    let display = build_display_panel();
    let navigation = build_navigation_panel();
    let scanner = build_scanner_panel();
    let satellites = build_satellites_panel();
    // Flyout is built after navigation because it borrows the
    // left-sidebar `name_entry` — its row actions (recall,
    // delete-of-active) sync the entry field.
    let bookmarks = build_bookmarks_panel(&navigation.name_entry);
    // Preset selection clears the active-bookmark highlight and
    // rebuilds the flyout list. Wiring lives outside
    // `build_navigation_panel` because it closes over state owned
    // by the flyout. Done before the `Rc` wrap below so the
    // preset handler keeps capturing individual `Rc` fields by
    // clone — wiring it through an `Rc<BookmarksPanel>` would
    // add an unnecessary upgrade dance on every preset click.
    navigation_panel::connect_preset_to_bookmarks(&navigation, &bookmarks);
    let bookmarks = Rc::new(bookmarks);

    SidebarPanels {
        source,
        audio,
        radio,
        display,
        navigation,
        bookmarks,
        server,
        scanner,
        satellites,
    }
}
