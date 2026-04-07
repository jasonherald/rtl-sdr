//! Audio output configuration panel — device and sink type selection.

use libadwaita as adw;
use libadwaita::prelude::*;

/// Audio output configuration panel with references to interactive rows.
pub struct AudioPanel {
    /// The `AdwPreferencesGroup` widget to pack into the sidebar.
    pub widget: adw::PreferencesGroup,
    /// Audio device selector.
    pub device_row: adw::ComboRow,
    /// Sink type selector (Audio, Network).
    pub sink_type_row: adw::ComboRow,
    /// Node names corresponding to device dropdown indices (for routing).
    pub device_node_names: Vec<String>,
}

/// Build the audio output configuration panel.
///
/// Queries `PipeWire` for available audio output sinks and populates
/// the device selector dropdown.
pub fn build_audio_panel() -> AudioPanel {
    let group = adw::PreferencesGroup::builder()
        .title("Audio")
        .description("Output configuration")
        .build();

    // Query PipeWire for available audio sinks
    let sinks = sdr_sink_audio::list_audio_sinks();
    let display_names: Vec<&str> = sinks.iter().map(|s| s.display_name.as_str()).collect();
    let node_names: Vec<String> = sinks.iter().map(|s| s.node_name.clone()).collect();
    let device_model = gtk4::StringList::new(&display_names);
    let device_row = adw::ComboRow::builder()
        .title("Device")
        .model(&device_model)
        .build();

    let sink_model = gtk4::StringList::new(&["Audio", "Network"]);
    let sink_type_row = adw::ComboRow::builder()
        .title("Sink Type")
        .model(&sink_model)
        .build();

    group.add(&device_row);
    group.add(&sink_type_row);

    AudioPanel {
        widget: group,
        device_row,
        sink_type_row,
        device_node_names: node_names,
    }
}
