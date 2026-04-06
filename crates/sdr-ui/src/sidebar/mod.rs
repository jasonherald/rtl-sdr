//! Sidebar configuration panels — source, audio, radio, display.

use gtk4::prelude::*;

pub mod audio_panel;
pub mod display_panel;
pub mod radio_panel;
pub mod source_panel;

pub use audio_panel::{AudioPanel, build_audio_panel};
pub use display_panel::{DisplayPanel, build_display_panel};
pub use radio_panel::{RadioPanel, build_radio_panel};
pub use source_panel::{SourcePanel, build_source_panel};

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
}

/// Build the complete sidebar `ScrolledWindow` containing all configuration panels.
///
/// Returns both the scroll widget (for embedding in the split view) and the
/// `SidebarPanels` struct (for DSP bridge signal wiring — see issue #92).
pub fn build_sidebar() -> (gtk4::ScrolledWindow, SidebarPanels) {
    let source = build_source_panel();
    let audio = build_audio_panel();
    let radio = build_radio_panel();
    let display = build_display_panel();

    let sidebar_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(SIDEBAR_SPACING)
        .margin_top(SIDEBAR_MARGIN)
        .margin_bottom(SIDEBAR_MARGIN)
        .margin_start(SIDEBAR_MARGIN)
        .margin_end(SIDEBAR_MARGIN)
        .build();

    sidebar_box.append(&source.widget);
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
    };

    (scroll, panels)
}
