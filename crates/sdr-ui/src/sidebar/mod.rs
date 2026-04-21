//! Sidebar configuration panels — source, audio, radio, display, navigation.
//!
//! The transcript panel lives in a separate right-side revealer, not here.

use gtk4::prelude::*;

pub mod audio_panel;
pub mod bookmarks_panel;
pub mod display_panel;
pub mod navigation_panel;
pub mod radio_panel;
pub mod server_panel;
pub mod source_panel;
pub mod transcript_panel;

pub use audio_panel::{AudioPanel, build_audio_panel};
pub use bookmarks_panel::{BookmarksPanel, build_bookmarks_panel};
pub use display_panel::{DisplayPanel, build_display_panel};
pub use navigation_panel::{NavigationPanel, build_navigation_panel};
pub use radio_panel::{RadioPanel, build_radio_panel};
pub use server_panel::{ServerPanel, build_server_panel};
pub use source_panel::{SourcePanel, build_source_panel};
pub use transcript_panel::{TranscriptPanel, build_transcript_panel};

/// Spacing between sidebar preference groups in pixels.
const SIDEBAR_SPACING: i32 = 12;
/// Margin around the sidebar content in pixels.
const SIDEBAR_MARGIN: i32 = 12;

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
    pub bookmarks: BookmarksPanel,
    /// Share-over-network (`rtl_tcp` server) controls. Hidden by
    /// default; `window.rs` reveals it when a local RTL-SDR dongle
    /// is plugged in and not currently the active source.
    pub server: ServerPanel,
}

/// Build the complete sidebar `ScrolledWindow` containing all configuration panels.
///
/// Returns both the scroll widget (for embedding in the split view) and the
/// `SidebarPanels` struct (for DSP bridge signal wiring — see issue #92).
/// The returned `SidebarPanels::bookmarks` is the right-side flyout widget;
/// `window.rs` packs it into the bookmarks revealer.
pub fn build_sidebar() -> (gtk4::ScrolledWindow, SidebarPanels) {
    let source = build_source_panel();
    let server = build_server_panel();
    let audio = build_audio_panel();
    let radio = build_radio_panel();
    let display = build_display_panel();
    let navigation = build_navigation_panel();
    // Flyout is built after navigation because it borrows the
    // left-sidebar `name_entry` — its row actions (recall,
    // delete-of-active) sync the entry field.
    let bookmarks = build_bookmarks_panel(&navigation.name_entry);
    // Preset selection clears the active-bookmark highlight and
    // rebuilds the flyout list. Wiring lives outside
    // `build_navigation_panel` because it closes over state owned
    // by the flyout.
    navigation_panel::connect_preset_to_bookmarks(&navigation, &bookmarks);

    let sidebar_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(SIDEBAR_SPACING)
        .margin_top(SIDEBAR_MARGIN)
        .margin_bottom(SIDEBAR_MARGIN)
        .margin_start(SIDEBAR_MARGIN)
        .margin_end(SIDEBAR_MARGIN)
        .build();

    sidebar_box.append(&navigation.presets_widget);
    sidebar_box.append(&navigation.bookmarks_widget);
    sidebar_box.append(&source.widget);
    // Server panel sits directly under Source so the "consume local
    // dongle" vs "share local dongle" decision is one visual group.
    // Hidden by default; revealed dynamically by the wiring layer.
    sidebar_box.append(&server.widget);
    sidebar_box.append(&audio.widget);
    sidebar_box.append(&radio.widget);
    sidebar_box.append(&display.widget);

    let scroll = gtk4::ScrolledWindow::builder()
        .child(&sidebar_box)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build();

    let panels = SidebarPanels {
        source,
        audio,
        radio,
        display,
        navigation,
        bookmarks,
        server,
    };

    (scroll, panels)
}
