//! Audio output configuration panel — device, sink type, network
//! sink config, and recording toggle.

use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_types::Protocol;

/// Sink-type combo discriminants. Keep stable so the
/// `connect_selected_notify` handler in `window::connect_audio_panel`
/// can dispatch the right `UiToDsp::SetAudioSinkType` value without
/// fragile by-string matching.
pub const SINK_TYPE_LOCAL_IDX: u32 = 0;
pub const SINK_TYPE_NETWORK_IDX: u32 = 1;

/// Protocol combo discriminants for the network sink. Order
/// must match the model strings in `build_audio_panel`.
pub const NETWORK_SINK_PROTOCOL_TCP_IDX: u32 = 0;
pub const NETWORK_SINK_PROTOCOL_UDP_IDX: u32 = 1;

// Defaults are owned by `sdr_core::sink_slot` so the engine
// initializer and the panel always agree. Re-exported here as
// pub use rather than redefined to keep the audit trail clear
// — both crates point at the same byte. Per CodeRabbit round 1
// on PR #351.
pub use sdr_core::{
    DEFAULT_NETWORK_SINK_HOST as NETWORK_SINK_DEFAULT_HOST,
    DEFAULT_NETWORK_SINK_PORT as NETWORK_SINK_DEFAULT_PORT,
};

/// Map a combo index to a `Protocol`. Returns `Protocol::TcpClient`
/// (TCP server mode — see `sdr_sink_network` docs) for the TCP
/// index, `Protocol::Udp` for the UDP index, and falls back to
/// TCP server for any unknown index so a future combo expansion
/// without this lookup updated still produces a sane default.
#[must_use]
pub fn protocol_from_combo_idx(idx: u32) -> Protocol {
    match idx {
        NETWORK_SINK_PROTOCOL_UDP_IDX => Protocol::Udp,
        // TCP_IDX (0) and any unknown idx fall through to TCP.
        _ => Protocol::TcpClient,
    }
}

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
    /// Toggle to start/stop audio recording.
    pub record_audio_row: adw::SwitchRow,
    /// Hostname / IP for the network audio sink. Hidden unless
    /// `Network` is the active sink type.
    pub network_host_row: adw::EntryRow,
    /// Port for the network audio sink (1..=65535).
    pub network_port_row: adw::SpinRow,
    /// Protocol picker — TCP server (default) or UDP unicast.
    pub network_protocol_row: adw::ComboRow,
    /// Status row showing the network sink's current state
    /// (Active / Inactive / Error). Driven by
    /// `DspToUi::NetworkSinkStatus` events.
    pub network_status_row: adw::ActionRow,
}

/// Build the audio output configuration panel.
///
/// Queries `PipeWire` for available audio output sinks and populates
/// the device selector dropdown. The network config rows are built
/// up front (so the sidebar layout doesn't shift when the user
/// toggles between sink types) but `set_visible(false)` until
/// `Network` is the active sink type.
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

    // Network config rows — built unconditionally so the
    // visibility toggle in `connect_audio_panel` is a cheap
    // `set_visible` rather than a structural insert/remove.
    let network_host_row = adw::EntryRow::builder()
        .title("Network host")
        .text(NETWORK_SINK_DEFAULT_HOST)
        .visible(false)
        .build();

    let port_adjustment = gtk4::Adjustment::new(
        f64::from(NETWORK_SINK_DEFAULT_PORT),
        1.0,
        65535.0,
        1.0,
        100.0,
        0.0,
    );
    let network_port_row = adw::SpinRow::builder()
        .title("Port")
        .adjustment(&port_adjustment)
        .visible(false)
        .build();

    let protocol_model = gtk4::StringList::new(&["TCP (server)", "UDP"]);
    let network_protocol_row = adw::ComboRow::builder()
        .title("Protocol")
        .model(&protocol_model)
        .visible(false)
        .build();

    let network_status_row = adw::ActionRow::builder()
        .title("Network sink")
        .subtitle("Inactive")
        .visible(false)
        .build();

    let record_audio_row = adw::SwitchRow::builder()
        .title("Record Audio")
        .subtitle("48 kHz stereo WAV")
        .build();

    group.add(&device_row);
    group.add(&sink_type_row);
    group.add(&network_host_row);
    group.add(&network_port_row);
    group.add(&network_protocol_row);
    group.add(&network_status_row);
    group.add(&record_audio_row);

    AudioPanel {
        widget: group,
        device_row,
        sink_type_row,
        device_node_names: node_names,
        record_audio_row,
        network_host_row,
        network_port_row,
        network_protocol_row,
        network_status_row,
    }
}
