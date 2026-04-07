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
    let sink_names: Vec<&str> = sinks.iter().map(String::as_str).collect();
    let device_model = gtk4::StringList::new(&sink_names);
    let device_row = adw::ComboRow::builder()
        .title("Device")
        .model(&device_model)
        .build();

    let sink_model = gtk4::StringList::new(&["Audio", "Network"]);
    let sink_type_row = adw::ComboRow::builder()
        .title("Sink Type")
        .model(&sink_model)
        .build();

    // TODO(issue #92): connect rows to DSP pipeline for device/sink switching

    group.add(&device_row);
    group.add(&sink_type_row);

    AudioPanel {
        widget: group,
        device_row,
        sink_type_row,
    }
}
