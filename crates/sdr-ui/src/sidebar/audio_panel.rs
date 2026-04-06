//! Audio output configuration panel — sink type selection.

use libadwaita as adw;
use libadwaita::prelude::*;

/// Audio output configuration panel with references to interactive rows.
pub struct AudioPanel {
    /// The `AdwPreferencesGroup` widget to pack into the sidebar.
    pub widget: adw::PreferencesGroup,
    /// Sink type selector (Audio, Network).
    pub sink_type_row: adw::ComboRow,
}

/// Build the audio output configuration panel.
pub fn build_audio_panel() -> AudioPanel {
    let group = adw::PreferencesGroup::builder()
        .title("Audio")
        .description("Output configuration")
        .build();

    let sink_model = gtk4::StringList::new(&["Audio", "Network"]);
    let sink_type_row = adw::ComboRow::builder()
        .title("Sink Type")
        .model(&sink_model)
        .build();

    // TODO: Connect sink_type_row to DSP pipeline (PR #7)
    // TODO: Add device enumeration when DSP bridge is wired

    group.add(&sink_type_row);

    AudioPanel {
        widget: group,
        sink_type_row,
    }
}
