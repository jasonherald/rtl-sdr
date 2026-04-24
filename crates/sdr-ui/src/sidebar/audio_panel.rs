//! Audio output configuration panel — device, volume, network sink
//! config, and recording toggle.

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

/// Volume slider range — the header `GtkScaleButton` is 0.0..=1.0
/// with a 0.05 step; the panel's spin row is the percentage
/// equivalent (0..=100 step 1) so both feel natural to users.
/// `window.rs` converts between the two via simple ×100 / ÷100.
pub const VOLUME_PERCENT_MIN: f64 = 0.0;
pub const VOLUME_PERCENT_MAX: f64 = 100.0;
pub const VOLUME_PERCENT_STEP: f64 = 1.0;
pub const VOLUME_PERCENT_PAGE: f64 = 10.0;

/// Valid TCP / UDP port range for the network sink port spin row.
pub const PORT_MIN: f64 = 1.0;
pub const PORT_MAX: f64 = 65535.0;
/// Keyboard increment / scroll-wheel step for the port spin row.
pub const PORT_STEP: f64 = 1.0;
/// Page-up / page-down step for the port spin row.
pub const PORT_PAGE: f64 = 100.0;

/// Config key for persisted volume. Stored as a `f64` in
/// `[0.0, 1.0]` (header `GtkScaleButton` domain) so the on-disk
/// value is the same shape the DSP sees via `UiToDsp::SetVolume`.
/// Restored at startup by `window::connect_volume_persistence`
/// (closes #419).
pub const KEY_AUDIO_VOLUME: &str = "audio_volume";

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
    /// The `AdwPreferencesPage` widget packed into the Audio
    /// activity stack slot. Hosts four titled `AdwPreferencesGroup`s
    /// (Output / Volume / Network sink / Recording) — see
    /// [`build_audio_panel`].
    pub widget: adw::PreferencesPage,
    /// Network-sink section group. Stored as a handle so
    /// `window::connect_audio_panel` can toggle the entire
    /// section's visibility when the user flips between the local
    /// and network sink types, rather than hiding four rows
    /// individually.
    pub network_sink_group: adw::PreferencesGroup,
    /// Audio device selector.
    pub device_row: adw::ComboRow,
    /// Sink type selector (Audio, Network).
    pub sink_type_row: adw::ComboRow,
    /// Node names corresponding to device dropdown indices (for routing).
    pub device_node_names: Vec<String>,
    /// Volume slider — 0..=100 percent. Kept in two-way sync with
    /// the header `GtkScaleButton` (which uses 0.0..=1.0). The
    /// header button is the **single source of truth** — only its
    /// `value_changed` handler dispatches `UiToDsp::SetVolume` and
    /// writes `KEY_AUDIO_VOLUME`. This row is mirror-only: its
    /// `value_notify` handler forwards edits into the button via
    /// `set_value`, and the button's handler does the real work.
    /// Programmatic restores (e.g. bookmark recall) must also go
    /// through the button, never call `send_dsp(SetVolume(..))`
    /// directly — see `connect_volume_persistence` in `window.rs`.
    /// Closes #419.
    pub volume_row: adw::SpinRow,
    /// Toggle to start/stop audio recording.
    pub record_audio_row: adw::SwitchRow,
    /// Hostname / IP for the network audio sink. Visible only
    /// when the Network sink type is selected.
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
/// up front and packed into their own titled section group so the
/// outer layout doesn't shift when the user toggles between sink
/// types — `window::connect_audio_panel` hides the whole group via
/// [`AudioPanel::network_sink_group`] when the Local sink is active.
///
/// Lays out as an `AdwPreferencesPage` with four titled sections
/// matching the activity-bar redesign's Apple-style rhythm (design
/// doc §3.3). Flat groups, no `AdwExpanderRow` wrappers — same call
/// as the General / Radio panels.
pub fn build_audio_panel() -> AudioPanel {
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

    // --- Volume (new, closes #419) ---
    let volume_adj = gtk4::Adjustment::new(
        VOLUME_PERCENT_MAX,
        VOLUME_PERCENT_MIN,
        VOLUME_PERCENT_MAX,
        VOLUME_PERCENT_STEP,
        VOLUME_PERCENT_PAGE,
        0.0,
    );
    let volume_row = adw::SpinRow::builder()
        .title("Volume")
        .subtitle("%")
        .adjustment(&volume_adj)
        .digits(0)
        .build();

    // --- Network sink config ---
    let network_host_row = adw::EntryRow::builder()
        .title("Network host")
        .text(NETWORK_SINK_DEFAULT_HOST)
        .build();

    let port_adjustment = gtk4::Adjustment::new(
        f64::from(NETWORK_SINK_DEFAULT_PORT),
        PORT_MIN,
        PORT_MAX,
        PORT_STEP,
        PORT_PAGE,
        0.0,
    );
    let network_port_row = adw::SpinRow::builder()
        .title("Port")
        .adjustment(&port_adjustment)
        .build();

    let protocol_model = gtk4::StringList::new(&["TCP (server)", "UDP"]);
    let network_protocol_row = adw::ComboRow::builder()
        .title("Protocol")
        .model(&protocol_model)
        .build();

    let network_status_row = adw::ActionRow::builder()
        .title("Network sink")
        .subtitle("Inactive")
        .build();

    let record_audio_row = adw::SwitchRow::builder()
        .title("Record Audio")
        .subtitle("48 kHz stereo WAV")
        .build();

    // --- Sectioned preferences page ---
    let output_group = adw::PreferencesGroup::builder()
        .title("Output")
        .description("Where audio plays")
        .build();
    output_group.add(&device_row);
    output_group.add(&sink_type_row);

    let volume_group = adw::PreferencesGroup::builder()
        .title("Volume")
        .description("Playback level — mirrors the header slider")
        .build();
    volume_group.add(&volume_row);

    let network_sink_group = adw::PreferencesGroup::builder()
        .title("Network sink")
        .description("Stream audio to another host over TCP or UDP")
        .visible(false)
        .build();
    network_sink_group.add(&network_host_row);
    network_sink_group.add(&network_port_row);
    network_sink_group.add(&network_protocol_row);
    network_sink_group.add(&network_status_row);

    let recording_group = adw::PreferencesGroup::builder()
        .title("Recording")
        .description("Save demodulated audio to disk")
        .build();
    recording_group.add(&record_audio_row);

    let page = adw::PreferencesPage::new();
    page.add(&output_group);
    page.add(&volume_group);
    page.add(&network_sink_group);
    page.add(&recording_group);

    AudioPanel {
        widget: page,
        network_sink_group,
        device_row,
        sink_type_row,
        device_node_names: node_names,
        volume_row,
        record_audio_row,
        network_host_row,
        network_port_row,
        network_protocol_row,
        network_status_row,
    }
}
