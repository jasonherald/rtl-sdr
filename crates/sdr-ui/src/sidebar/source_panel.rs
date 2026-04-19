//! Source device configuration panel — device selector, RTL-SDR /
//! Network / File / RTL-TCP controls.

use std::sync::Arc;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;
use sdr_types::RtlTcpConnectionState;

/// Config key for the persisted list of favorited `rtl_tcp` server
/// instance names. Stored as a JSON array of strings; unknown /
/// stale entries are tolerated by the read path.
pub const KEY_RTL_TCP_CLIENT_FAVORITES: &str = "rtl_tcp_client_favorites";
/// Config key for the persisted last-connected server. Stored as
/// a JSON object `{ host, port, nickname }` so we can repopulate
/// the hostname / port rows on app launch without waiting for
/// mDNS to rediscover.
pub const KEY_RTL_TCP_CLIENT_LAST_CONNECTED: &str = "rtl_tcp_client_last_connected";

/// Device selector index for RTL-SDR.
pub const DEVICE_RTLSDR: u32 = 0;
/// Device selector index for Network.
pub const DEVICE_NETWORK: u32 = 1;
/// Device selector index for File.
pub const DEVICE_FILE: u32 = 2;
/// Device selector index for RTL-TCP (rtl_tcp-protocol network client).
pub const DEVICE_RTLTCP: u32 = 3;

/// Default subtitle for the RTL-TCP status row before any
/// `DspToUi::RtlTcpConnectionState` event has arrived (or after a
/// Disconnect). Kept as a const so the empty-at-startup and
/// empty-after-disconnect paths render identical text.
pub const RTL_TCP_STATUS_DISCONNECTED_SUBTITLE: &str = "Disconnected";

/// Sample-rate selector index at which we start showing the
/// "high bandwidth" advisory caption. Index 7 = 2.4 MHz, which
/// at 8-bit I/Q pairs wire-format works out to ~38 Mbps — over
/// a typical home Wi-Fi link (11/24/54 Mbps practical throughput
/// for older hardware) this produces silent drops. Anything at
/// or above this index triggers the caption so the user gets a
/// heads-up before commanding the remote server.
pub const HIGH_BANDWIDTH_SAMPLE_RATE_IDX: u32 = 7;

/// Title shown on the advisory row when a network-heavy sample
/// rate is selected. Kept as a const so source + server panels
/// render identical copy.
pub const HIGH_BANDWIDTH_ADVISORY_TITLE: &str = "High sample rate";
/// Subtitle for the advisory row — the supporting detail under
/// the title.
pub const HIGH_BANDWIDTH_ADVISORY_SUBTITLE: &str =
    "Your network may not keep up (≈38 Mbps at 2.4 Msps with 8-bit I/Q).";

/// Network protocol selector index for TCP (client). Load-bearing:
/// both `build_network_rows()` (protocol `StringList`) and callers in
/// `window.rs` that set or read this row rely on this exact mapping.
/// Reorder the `StringList` and these constants must move in lockstep.
pub const NETWORK_PROTOCOL_TCPCLIENT_IDX: u32 = 0;
/// Network protocol selector index for UDP.
pub const NETWORK_PROTOCOL_UDP_IDX: u32 = 1;

/// Default gain in dB.
const DEFAULT_GAIN_DB: f64 = 0.0;
/// Minimum gain in dB.
const MIN_GAIN_DB: f64 = 0.0;
/// Maximum gain in dB.
const MAX_GAIN_DB: f64 = 49.6;
/// Gain step in dB.
const GAIN_STEP_DB: f64 = 0.1;
/// Gain page increment in dB.
const GAIN_PAGE_DB: f64 = 1.0;

/// Default network port.
const DEFAULT_PORT: f64 = 1234.0;
/// Minimum port number.
const MIN_PORT: f64 = 1.0;
/// Maximum port number.
const MAX_PORT: f64 = 65535.0;
/// Port spin step.
const PORT_STEP: f64 = 1.0;
/// Port page increment.
const PORT_PAGE: f64 = 100.0;

/// Default PPM correction.
const DEFAULT_PPM: f64 = 0.0;
/// Minimum PPM correction.
const MIN_PPM: f64 = -200.0;
/// Maximum PPM correction.
const MAX_PPM: f64 = 200.0;
/// PPM step increment.
const PPM_STEP: f64 = 1.0;
/// PPM page increment.
const PPM_PAGE: f64 = 10.0;

/// Source device configuration panel with references to all interactive rows.
pub struct SourcePanel {
    /// The `AdwPreferencesGroup` widget to pack into the sidebar.
    pub widget: adw::PreferencesGroup,
    /// Device type selector (RTL-SDR, Network).
    pub device_row: adw::ComboRow,
    /// RTL-SDR sample rate selector.
    pub sample_rate_row: adw::ComboRow,
    /// RTL-SDR gain control.
    pub gain_row: adw::SpinRow,
    /// RTL-SDR AGC toggle.
    pub agc_row: adw::SwitchRow,
    /// RTL-SDR PPM frequency correction.
    pub ppm_row: adw::SpinRow,
    /// Network hostname entry.
    pub hostname_row: adw::EntryRow,
    /// Network port number.
    pub port_row: adw::SpinRow,
    /// Network protocol selector (TCP, UDP).
    pub protocol_row: adw::ComboRow,
    /// File path entry (File source).
    pub file_path_row: adw::EntryRow,
    /// DC blocking filter toggle (always visible).
    pub dc_blocking_row: adw::SwitchRow,
    /// IQ correction toggle (always visible).
    pub iq_correction_row: adw::SwitchRow,
    /// IQ inversion toggle (always visible).
    pub iq_inversion_row: adw::SwitchRow,
    /// Decimation factor selector (always visible).
    pub decimation_row: adw::ComboRow,
    /// Toggle to start/stop IQ recording.
    pub record_iq_row: adw::SwitchRow,
    /// Discovered `rtl_tcp` servers (live from mDNS). Collapsed by
    /// default; expands when servers are seen.
    pub rtl_tcp_discovered_row: adw::ExpanderRow,
    /// Second entry point into the header-bar favorites popover.
    /// Packed as a suffix on `rtl_tcp_discovered_row` — visible
    /// only when the RTL-TCP device is selected (same visibility
    /// as its parent expander). Click handler in `window.rs` calls
    /// the header-bar favorites `MenuButton::popup()` so the slide-
    /// out appears anchored to the header regardless of which
    /// button the user clicked.
    pub manage_favorites_button: gtk4::Button,

    /// Connection status line shown only while the RTL-TCP source
    /// type is selected. Subtitle reflects the current
    /// `RtlTcpConnectionState` — "Connected to R820T (29 gains)",
    /// "Retrying in 5 s (attempt 3)", "Failed: bad handshake", etc.
    pub rtl_tcp_status_row: adw::ActionRow,
    /// Stops the current `rtl_tcp` connection without changing
    /// source type. Packed as a suffix on `rtl_tcp_status_row`,
    /// sensitive only when there's something to disconnect from.
    pub rtl_tcp_disconnect_button: gtk4::Button,
    /// Forces a reconnect attempt immediately, skipping the
    /// exponential-backoff sleep. Packed as a suffix on
    /// `rtl_tcp_status_row`, sensitive only when the state
    /// indicates we're between attempts (Retrying / Failed /
    /// Disconnected).
    pub rtl_tcp_retry_button: gtk4::Button,

    /// Advisory caption shown when the selected sample rate is at
    /// or above `HIGH_BANDWIDTH_SAMPLE_RATE_IDX` AND the source
    /// type routes over the network (RTL-TCP). Silent for local
    /// RTL-SDR and File sources — the wire-bandwidth concern only
    /// applies to network paths.
    pub bandwidth_advisory_row: adw::ActionRow,
}

/// Render a connection state into a one-line human-readable form
/// for the status row subtitle. Free function + pure formatter so
/// it's unit-testable without instantiating GTK widgets.
pub fn format_rtl_tcp_state(state: &RtlTcpConnectionState) -> String {
    match state {
        RtlTcpConnectionState::Disconnected => RTL_TCP_STATUS_DISCONNECTED_SUBTITLE.to_string(),
        RtlTcpConnectionState::Connecting => "Connecting…".to_string(),
        RtlTcpConnectionState::Connected {
            tuner_name,
            gain_count,
        } => format!("Connected — {tuner_name} ({gain_count} gains)"),
        RtlTcpConnectionState::Retrying { attempt, retry_in } => {
            // Ceil, not floor — `as_secs` truncates fractional
            // seconds, so `1.9 s` would read as "1 s" and the row
            // would understate the remaining delay. Bump by one
            // whenever there are any subsec nanos, then clamp to
            // at least 1 so sub-1 s retries still show something
            // rather than "0 s" (which reads like the retry
            // already fired).
            let secs_ceil = retry_in.as_secs() + u64::from(retry_in.subsec_nanos() > 0);
            let secs = secs_ceil.max(1);
            format!("Retrying in {secs} s (attempt {attempt})")
        }
        RtlTcpConnectionState::Failed { reason } => format!("Failed — {reason}"),
    }
}

/// Default sample rate selector index (2.4 MHz = index 7).
const DEFAULT_SAMPLE_RATE_INDEX: u32 = 7;

/// Build RTL-SDR-specific rows: sample rate, gain, AGC, PPM correction.
fn build_rtlsdr_rows() -> (adw::ComboRow, adw::SpinRow, adw::SwitchRow, adw::SpinRow) {
    let sample_rate_model = gtk4::StringList::new(&[
        "250 kHz",
        "1.024 MHz",
        "1.536 MHz",
        "1.792 MHz",
        "1.920 MHz",
        "2.048 MHz",
        "2.160 MHz",
        "2.4 MHz",
        "2.560 MHz",
        "2.880 MHz",
        "3.2 MHz",
    ]);
    let sample_rate_row = adw::ComboRow::builder()
        .title("Sample Rate")
        .model(&sample_rate_model)
        .selected(DEFAULT_SAMPLE_RATE_INDEX)
        .build();

    let gain_adj = gtk4::Adjustment::new(
        DEFAULT_GAIN_DB,
        MIN_GAIN_DB,
        MAX_GAIN_DB,
        GAIN_STEP_DB,
        GAIN_PAGE_DB,
        0.0,
    );
    let gain_row = adw::SpinRow::builder()
        .title("Gain")
        .subtitle("dB")
        .adjustment(&gain_adj)
        .digits(1)
        .build();

    let agc_row = adw::SwitchRow::builder()
        .title("AGC")
        .subtitle("Automatic gain control")
        .build();

    let ppm_adj = gtk4::Adjustment::new(DEFAULT_PPM, MIN_PPM, MAX_PPM, PPM_STEP, PPM_PAGE, 0.0);
    let ppm_row = adw::SpinRow::builder()
        .title("PPM Correction")
        .subtitle("Crystal frequency offset")
        .adjustment(&ppm_adj)
        .digits(0)
        .build();

    (sample_rate_row, gain_row, agc_row, ppm_row)
}

/// Build network-specific rows: hostname, port, protocol.
fn build_network_rows() -> (adw::EntryRow, adw::SpinRow, adw::ComboRow) {
    let hostname_row = adw::EntryRow::builder()
        .title("Hostname")
        .text("localhost")
        .build();

    let port_adj =
        gtk4::Adjustment::new(DEFAULT_PORT, MIN_PORT, MAX_PORT, PORT_STEP, PORT_PAGE, 0.0);
    let port_row = adw::SpinRow::builder()
        .title("Port")
        .adjustment(&port_adj)
        .digits(0)
        .build();

    // Order is load-bearing — must match
    // `NETWORK_PROTOCOL_TCPCLIENT_IDX` / `NETWORK_PROTOCOL_UDP_IDX`.
    let protocol_model = gtk4::StringList::new(&["TCP", "UDP"]);
    let protocol_row = adw::ComboRow::builder()
        .title("Protocol")
        .model(&protocol_model)
        .build();

    (hostname_row, port_row, protocol_row)
}

/// Build common controls: DC blocking, IQ correction, IQ inversion, decimation.
fn build_common_rows() -> (
    adw::SwitchRow,
    adw::SwitchRow,
    adw::SwitchRow,
    adw::ComboRow,
) {
    let dc_blocking_row = adw::SwitchRow::builder()
        .title("DC Blocking")
        .active(true)
        .build();

    let iq_correction_row = adw::SwitchRow::builder().title("IQ Correction").build();

    let iq_inversion_row = adw::SwitchRow::builder().title("Invert IQ").build();

    let decimation_model = gtk4::StringList::new(&["None", "2x", "4x", "8x", "16x"]);
    let decimation_row = adw::ComboRow::builder()
        .title("Decimation")
        .model(&decimation_model)
        .build();

    (
        dc_blocking_row,
        iq_correction_row,
        iq_inversion_row,
        decimation_row,
    )
}

/// Wire the device selector to show/hide source-specific rows.
#[allow(clippy::too_many_arguments)]
fn connect_device_visibility(
    device_row: &adw::ComboRow,
    sample_rate_row: &adw::ComboRow,
    gain_row: &adw::SpinRow,
    agc_row: &adw::SwitchRow,
    ppm_row: &adw::SpinRow,
    hostname_row: &adw::EntryRow,
    port_row: &adw::SpinRow,
    protocol_row: &adw::ComboRow,
    file_path_row: &adw::EntryRow,
) {
    device_row.connect_selected_notify(glib::clone!(
        #[weak]
        sample_rate_row,
        #[weak]
        gain_row,
        #[weak]
        agc_row,
        #[weak]
        ppm_row,
        #[weak]
        hostname_row,
        #[weak]
        port_row,
        #[weak]
        protocol_row,
        #[weak]
        file_path_row,
        move |row| {
            let selected = row.selected();
            let is_rtlsdr = selected == DEVICE_RTLSDR;
            let is_network = selected == DEVICE_NETWORK;
            let is_file = selected == DEVICE_FILE;
            let is_rtltcp = selected == DEVICE_RTLTCP;

            // Tuning controls are meaningful for both the local dongle
            // AND a remote rtl_tcp server (they route through the
            // Source trait's set_gain / set_gain_mode / set_ppm_correction
            // hooks, which RtlTcpSource implements by forwarding wire
            // commands). Sample-rate row too — UiToDsp::SetSampleRate
            // goes through Source::set_sample_rate either way.
            let tune_controls_visible = is_rtlsdr || is_rtltcp;
            sample_rate_row.set_visible(tune_controls_visible);
            gain_row.set_visible(tune_controls_visible);
            agc_row.set_visible(tune_controls_visible);
            ppm_row.set_visible(tune_controls_visible);

            // Hostname / port entry is shared between raw-IQ Network
            // and RTL-TCP modes. Protocol (TCP/UDP) only applies to
            // raw Network — RTL-TCP always rides on TCP.
            hostname_row.set_visible(is_network || is_rtltcp);
            port_row.set_visible(is_network || is_rtltcp);
            protocol_row.set_visible(is_network);

            file_path_row.set_visible(is_file);

            tracing::debug!(device = selected, "source device changed");
        }
    ));
}

/// Build the source device configuration panel.
#[allow(
    clippy::too_many_lines,
    reason = "widget-assembly function — splitting would scatter one-time wire-up across many helpers with no readability win"
)]
pub fn build_source_panel() -> SourcePanel {
    let group = adw::PreferencesGroup::builder()
        .title("Source")
        .description("Device and input configuration")
        .build();

    // Order is load-bearing — matches `DEVICE_RTLSDR / NETWORK / FILE /
    // RTLTCP` index constants. If you change the order here, update the
    // constants AND the `SourceType` match in window.rs at the same time.
    let device_model = gtk4::StringList::new(&["RTL-SDR", "Network", "File", "RTL-TCP (network)"]);
    let device_row = adw::ComboRow::builder()
        .title("Device")
        .model(&device_model)
        .build();

    let (sample_rate_row, gain_row, agc_row, ppm_row) = build_rtlsdr_rows();
    let (hostname_row, port_row, protocol_row) = build_network_rows();
    let file_path_row = adw::EntryRow::builder()
        .title("File Path")
        .text("")
        .visible(false)
        .build();
    let (dc_blocking_row, iq_correction_row, iq_inversion_row, decimation_row) =
        build_common_rows();

    let record_iq_row = adw::SwitchRow::builder()
        .title("Record IQ")
        .subtitle("Raw IQ samples to WAV")
        .build();

    // RTL-TCP-specific rows. Built always, shown only when the RTL-TCP
    // source type is selected (see connect_device_visibility + the
    // initial-visibility block below).
    let rtl_tcp_discovered_row = adw::ExpanderRow::builder()
        .title("Discovered rtl_tcp servers")
        .subtitle("No servers discovered on the local network yet.")
        .visible(false)
        .build();
    // Second entry point into the favorites slide-out. The header
    // bar's star button is the always-visible path; this one lives
    // inside the RTL-TCP section so users who are actively picking
    // a server don't have to route up to the header. Click handler
    // is wired in window.rs because the MenuButton whose `popup()`
    // we call is owned by the header bar, not the source panel.
    let manage_favorites_button = gtk4::Button::builder()
        .label("Manage favorites…")
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .tooltip_text("Open the favorites slide-out from the header bar")
        .build();
    rtl_tcp_discovered_row.add_suffix(&manage_favorites_button);

    // Connection-state status row — subtitle updated by the DSP
    // bridge via `DspToUi::RtlTcpConnectionState`. Suffix buttons
    // let the user tear down or force-retry the connection without
    // leaving the RTL-TCP source type.
    let rtl_tcp_status_row = adw::ActionRow::builder()
        .title("Connection")
        .subtitle(RTL_TCP_STATUS_DISCONNECTED_SUBTITLE)
        .visible(false)
        .build();
    let rtl_tcp_disconnect_button = gtk4::Button::with_label("Disconnect");
    rtl_tcp_disconnect_button.set_valign(gtk4::Align::Center);
    rtl_tcp_disconnect_button.set_sensitive(false);
    let rtl_tcp_retry_button = gtk4::Button::with_label("Retry now");
    rtl_tcp_retry_button.set_valign(gtk4::Align::Center);
    rtl_tcp_retry_button.add_css_class("suggested-action");
    rtl_tcp_retry_button.set_sensitive(false);
    rtl_tcp_status_row.add_suffix(&rtl_tcp_disconnect_button);
    rtl_tcp_status_row.add_suffix(&rtl_tcp_retry_button);

    // Bandwidth advisory row — hidden by default. Visibility is
    // toggled by the sample-rate and device-type notify handlers
    // in window.rs. Title + subtitle copy come from shared consts
    // so the source and server panels render identical text.
    let bandwidth_advisory_row = adw::ActionRow::builder()
        .title(HIGH_BANDWIDTH_ADVISORY_TITLE)
        .subtitle(HIGH_BANDWIDTH_ADVISORY_SUBTITLE)
        .visible(false)
        .build();
    bandwidth_advisory_row.add_prefix(&gtk4::Image::from_icon_name("dialog-information-symbolic"));

    // Add all rows to the group.
    group.add(&device_row);
    group.add(&sample_rate_row);
    group.add(&gain_row);
    group.add(&agc_row);
    group.add(&ppm_row);
    group.add(&hostname_row);
    group.add(&port_row);
    group.add(&protocol_row);
    group.add(&file_path_row);
    group.add(&dc_blocking_row);
    group.add(&iq_correction_row);
    group.add(&iq_inversion_row);
    group.add(&decimation_row);
    group.add(&record_iq_row);
    group.add(&rtl_tcp_discovered_row);
    group.add(&rtl_tcp_status_row);
    group.add(&bandwidth_advisory_row);

    // Derive initial visibility from the selected device.
    let selected = device_row.selected();
    let is_rtlsdr = selected == DEVICE_RTLSDR;
    let is_network = selected == DEVICE_NETWORK;
    let is_file = selected == DEVICE_FILE;
    let is_rtltcp = selected == DEVICE_RTLTCP;
    let tune_controls_visible = is_rtlsdr || is_rtltcp;
    sample_rate_row.set_visible(tune_controls_visible);
    gain_row.set_visible(tune_controls_visible);
    agc_row.set_visible(tune_controls_visible);
    ppm_row.set_visible(tune_controls_visible);
    hostname_row.set_visible(is_network || is_rtltcp);
    port_row.set_visible(is_network || is_rtltcp);
    protocol_row.set_visible(is_network);
    file_path_row.set_visible(is_file);
    rtl_tcp_discovered_row.set_visible(is_rtltcp);
    rtl_tcp_status_row.set_visible(is_rtltcp);

    connect_device_visibility(
        &device_row,
        &sample_rate_row,
        &gain_row,
        &agc_row,
        &ppm_row,
        &hostname_row,
        &port_row,
        &protocol_row,
        &file_path_row,
    );
    connect_rtl_tcp_visibility(&device_row, &rtl_tcp_discovered_row, &rtl_tcp_status_row);

    // Controls connected to DSP pipeline via window.rs

    SourcePanel {
        widget: group,
        device_row,
        sample_rate_row,
        gain_row,
        agc_row,
        ppm_row,
        hostname_row,
        port_row,
        protocol_row,
        file_path_row,
        dc_blocking_row,
        iq_correction_row,
        iq_inversion_row,
        decimation_row,
        record_iq_row,
        rtl_tcp_discovered_row,
        manage_favorites_button,
        rtl_tcp_status_row,
        rtl_tcp_disconnect_button,
        rtl_tcp_retry_button,
        bandwidth_advisory_row,
    }
}

/// Toggle visibility of the RTL-TCP-specific rows based on the device
/// selector. Kept separate from `connect_device_visibility` so the
/// existing function's argument list doesn't grow further.
fn connect_rtl_tcp_visibility(
    device_row: &adw::ComboRow,
    rtl_tcp_discovered_row: &adw::ExpanderRow,
    rtl_tcp_status_row: &adw::ActionRow,
) {
    device_row.connect_selected_notify(glib::clone!(
        #[weak]
        rtl_tcp_discovered_row,
        #[weak]
        rtl_tcp_status_row,
        move |row| {
            let is_rtltcp = row.selected() == DEVICE_RTLTCP;
            rtl_tcp_discovered_row.set_visible(is_rtltcp);
            rtl_tcp_status_row.set_visible(is_rtltcp);
        }
    ));
}

/// Snapshot of a previously-connected `rtl_tcp` server. Serialized
/// into the `rtl_tcp_client_last_connected` config entry so the
/// next app launch can repopulate the hostname / port / nickname
/// fields without waiting for mDNS to rediscover.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LastConnectedServer {
    /// Hostname or IP literal the Connect button dialed — either
    /// a resolved address (`192.168.1.5`) or an mDNS hostname
    /// (`shack-pi.local.`), whichever the discovery layer yielded.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// User-facing nickname — normally the mDNS TXT nickname, or
    /// the `instance_name` when no nickname was published.
    pub nickname: String,
}

/// Rich favorite-entry record. Persisted in the
/// `rtl_tcp_client_favorites` config array as a JSON object per
/// entry. Keeps the stable `key` (hostname:port — see
/// `window.rs::favorite_key`) alongside display metadata the
/// favorites slide-out shows even when the server is offline: the
/// nickname the user last saw, the tuner type and gain-step count
/// from the last mDNS announcement, and a "last seen" wall-clock
/// stamp.
///
/// Optional fields default to `None` so a freshly-starred server
/// with no cached metadata still round-trips correctly, and so
/// legacy bare-string entries (PR #335 schema) can be read back
/// without drift — see `load_favorites`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FavoriteEntry {
    /// Stable identity: `format!("{}:{}", hostname, port)`. Same
    /// value produced by `window.rs::favorite_key` on the live
    /// `DiscoveredServer`. Load-bearing — two entries with the
    /// same key refer to the same endpoint.
    pub key: String,
    /// User-facing label. Preferred source: the mDNS TXT
    /// `nickname`. Fallback: the DNS-SD `instance_name`. For a
    /// migrated legacy entry this is the same string as `key`
    /// until the server re-announces and the user re-stars (or
    /// next-session metadata refresh lands).
    pub nickname: String,
    /// Tuner model from the last-seen `DiscoveredServer` TXT
    /// record, e.g. `"R820T"`. `None` for offline-only entries
    /// we haven't seen since the schema upgrade.
    pub tuner_name: Option<String>,
    /// Gain-step count from the same TXT record. `None` same as
    /// `tuner_name`.
    pub gain_count: Option<u32>,
    /// Unix timestamp (seconds) of the most recent
    /// `ServerAnnounced` event for this `key`. `None` when we
    /// haven't seen the server this session.
    pub last_seen_unix: Option<u64>,
}

/// Load the persisted favorites list. Returns an empty `Vec` on
/// first launch / absent / corrupt config — safe to call
/// unconditionally.
///
/// **Backward compatibility:** accepts two on-disk shapes:
///
/// 1. **Current (PR #315):** `Vec<FavoriteEntry>` — array of JSON
///    objects, each decoded via `serde_json::from_value`. Objects
///    that fail to deserialize are silently skipped (same
///    schema-drift tolerance as `load_last_connected`).
/// 2. **Legacy (PR #335):** `Vec<String>` — array of bare
///    `hostname:port` keys. Upgraded in-place by constructing a
///    `FavoriteEntry` with `nickname = key` and every optional
///    metadata field set to `None`. Those blanks fill in on the
///    next re-announce + re-star, so no user-visible data is lost
///    — just a one-session degraded display until the server is
///    seen again.
pub fn load_favorites(config: &Arc<ConfigManager>) -> Vec<FavoriteEntry> {
    config.read(|v| {
        let Some(arr) = v
            .get(KEY_RTL_TCP_CLIENT_FAVORITES)
            .and_then(serde_json::Value::as_array)
        else {
            return Vec::new();
        };
        arr.iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                if let Some(s) = entry.as_str() {
                    // Legacy bare-string entry. Build a stub
                    // FavoriteEntry so the slide-out still has
                    // something to render while the user waits
                    // for the server to re-announce.
                    Some(FavoriteEntry {
                        key: s.to_string(),
                        nickname: s.to_string(),
                        tuner_name: None,
                        gain_count: None,
                        last_seen_unix: None,
                    })
                } else {
                    // Corrupt object entry — hand-edited JSON or a
                    // shape we don't recognize. Skip the entry so
                    // the rest of the list still loads, but log so
                    // a "my favorite disappeared" bug report
                    // surfaces the parse failure.
                    match serde_json::from_value::<FavoriteEntry>(entry.clone()) {
                        Ok(fav) => Some(fav),
                        Err(err) => {
                            tracing::warn!(
                                entry_index = idx,
                                error = %err,
                                "skipping corrupt rtl_tcp favorite entry",
                            );
                            None
                        }
                    }
                }
            })
            .collect()
    })
}

/// Persist the full favorites list as a JSON array of
/// `FavoriteEntry` objects. Overwrites the config entry — callers
/// pass the current UI state of pinned entries.
pub fn save_favorites(config: &Arc<ConfigManager>, favorites: &[FavoriteEntry]) {
    config.write(|v| {
        v[KEY_RTL_TCP_CLIENT_FAVORITES] =
            serde_json::to_value(favorites).unwrap_or(serde_json::Value::Null);
    });
}

/// Current wall-clock time as Unix seconds. Helper for building
/// `FavoriteEntry::last_seen_unix` on star-toggle / re-announce.
/// Saturating-zero on clock skew (pre-epoch system time) so
/// we never return a garbage very-large value from a
/// `Duration::as_secs` on an error path.
pub fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Load the last-connected server snapshot, if any was recorded.
/// Returns `None` on first launch or when the stored blob fails
/// to deserialize (schema drift, hand-edited config, etc.).
pub fn load_last_connected(config: &Arc<ConfigManager>) -> Option<LastConnectedServer> {
    config.read(|v| {
        v.get(KEY_RTL_TCP_CLIENT_LAST_CONNECTED)
            .and_then(|entry| serde_json::from_value(entry.clone()).ok())
    })
}

/// Persist a `LastConnectedServer` snapshot. Called from the
/// discovery-row Connect handler and from any manual-server
/// connect path once that UI exists.
pub fn save_last_connected(config: &Arc<ConfigManager>, server: &LastConnectedServer) {
    config.write(|v| {
        // Serialize via serde_json::to_value so we don't re-embed
        // JSON-encoded text inside a JSON string (the common
        // round-trip mistake here).
        v[KEY_RTL_TCP_CLIENT_LAST_CONNECTED] =
            serde_json::to_value(server).unwrap_or(serde_json::Value::Null);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time validation that gain constants are consistent.
    const _: () = {
        assert!(MIN_GAIN_DB <= MAX_GAIN_DB);
        assert!(GAIN_STEP_DB > 0.0);
    };

    /// Compile-time validation that port constants are consistent.
    const _: () = {
        assert!(MIN_PORT <= MAX_PORT);
        assert!(DEFAULT_PORT >= MIN_PORT);
        assert!(DEFAULT_PORT <= MAX_PORT);
    };

    /// Compile-time validation that PPM constants are consistent.
    const _: () = {
        assert!(MIN_PPM <= MAX_PPM);
        assert!(DEFAULT_PPM >= MIN_PPM);
        assert!(DEFAULT_PPM <= MAX_PPM);
        assert!(PPM_STEP > 0.0);
    };

    #[test]
    fn device_indices_are_distinct() {
        // Full pairwise distinctness — adding a 5th source type
        // without updating this test would let a collision slip
        // through. The device_row -> SourceType match in window.rs
        // depends on these being unique integer indices.
        assert_ne!(DEVICE_RTLSDR, DEVICE_NETWORK);
        assert_ne!(DEVICE_RTLSDR, DEVICE_FILE);
        assert_ne!(DEVICE_RTLSDR, DEVICE_RTLTCP);
        assert_ne!(DEVICE_NETWORK, DEVICE_FILE);
        assert_ne!(DEVICE_NETWORK, DEVICE_RTLTCP);
        assert_ne!(DEVICE_FILE, DEVICE_RTLTCP);
    }

    #[test]
    fn format_rtl_tcp_state_covers_every_variant() {
        use std::time::Duration;

        // Disconnected → empty-looking but consistent with the const.
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::Disconnected),
            RTL_TCP_STATUS_DISCONNECTED_SUBTITLE
        );
        // Connecting → ellipsis marker (avoids the reader confusing
        // "Connecting" with "Connected" on a cursory glance).
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::Connecting),
            "Connecting…"
        );
        // Connected carries tuner metadata both the user and the
        // debugging eye can parse.
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::Connected {
                tuner_name: "R820T".into(),
                gain_count: 29,
            }),
            "Connected — R820T (29 gains)"
        );
        // Retrying ceils fractional seconds so the row never
        // understates the delay: 250 ms remaining → "1 s", not
        // "0 s" (which would read as "the retry just fired").
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::Retrying {
                attempt: 3,
                retry_in: Duration::from_millis(250),
            }),
            "Retrying in 1 s (attempt 3)"
        );
        // Key regression guard for the ceil semantics — 1.9 s must
        // read as "2 s", never "1 s". Flooring on `as_secs` would
        // silently understate the countdown here.
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::Retrying {
                attempt: 4,
                retry_in: Duration::from_millis(1_900),
            }),
            "Retrying in 2 s (attempt 4)"
        );
        // Exact integer seconds must NOT get bumped by the ceil —
        // 12 s stays at "12 s", not "13 s".
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::Retrying {
                attempt: 5,
                retry_in: Duration::from_secs(12),
            }),
            "Retrying in 12 s (attempt 5)"
        );
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::Failed {
                reason: "bad handshake".into(),
            }),
            "Failed — bad handshake"
        );
    }

    // ---- Client-persistence helpers (favorites + last-connected) ----

    fn make_config() -> Arc<ConfigManager> {
        Arc::new(ConfigManager::in_memory(&serde_json::json!({})))
    }

    #[test]
    fn favorites_round_trip_preserves_rich_metadata() {
        let config = make_config();
        // Fresh config → empty list.
        assert!(load_favorites(&config).is_empty());
        let favs = vec![
            FavoriteEntry {
                key: "shack-pi.local.:1234".into(),
                nickname: "Shack Pi".into(),
                tuner_name: Some("R820T".into()),
                gain_count: Some(29),
                last_seen_unix: Some(1_700_000_000),
            },
            FavoriteEntry {
                key: "attic-pi.local.:1234".into(),
                nickname: "Attic Pi".into(),
                tuner_name: None,
                gain_count: None,
                last_seen_unix: None,
            },
        ];
        save_favorites(&config, &favs);
        let loaded = load_favorites(&config);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].key, "shack-pi.local.:1234");
        assert_eq!(loaded[0].nickname, "Shack Pi");
        assert_eq!(loaded[0].tuner_name.as_deref(), Some("R820T"));
        assert_eq!(loaded[0].gain_count, Some(29));
        assert_eq!(loaded[0].last_seen_unix, Some(1_700_000_000));
        // Second entry has every optional field None → must
        // round-trip as None, NOT as missing / default values.
        assert!(loaded[1].tuner_name.is_none());
        assert!(loaded[1].gain_count.is_none());
        assert!(loaded[1].last_seen_unix.is_none());
    }

    #[test]
    fn favorites_loader_upgrades_legacy_string_entries() {
        // Regression guard for the PR #335 → #315 schema
        // migration. Users who starred servers before #315 have
        // `Vec<String>` persisted; the new loader must synthesize
        // `FavoriteEntry` stubs so those favorites still appear
        // in the slide-out (with degraded metadata until the
        // server re-announces).
        let config = make_config();
        config.write(|v| {
            v[KEY_RTL_TCP_CLIENT_FAVORITES] =
                serde_json::json!(["shack-pi.local.:1234", "attic-pi.local.:1235",]);
        });
        let loaded = load_favorites(&config);
        assert_eq!(loaded.len(), 2);
        // `nickname` falls back to the key so the slide-out has
        // something printable.
        assert_eq!(loaded[0].key, "shack-pi.local.:1234");
        assert_eq!(loaded[0].nickname, "shack-pi.local.:1234");
        // Metadata blanks — filled by next re-announce + re-star.
        assert!(loaded[0].tuner_name.is_none());
        assert!(loaded[0].gain_count.is_none());
        assert!(loaded[0].last_seen_unix.is_none());
    }

    #[test]
    fn favorites_loader_tolerates_non_array_entry() {
        // If someone hand-edits the config file and makes the
        // entry a string (not an array), we shouldn't panic or
        // corrupt state — just return empty and let the user
        // re-pin.
        let config = make_config();
        config.write(|v| {
            v[KEY_RTL_TCP_CLIENT_FAVORITES] = serde_json::json!("not an array");
        });
        assert!(load_favorites(&config).is_empty());
    }

    #[test]
    fn favorites_loader_skips_corrupt_object_entries() {
        // Mixed-array case: a well-formed FavoriteEntry object
        // alongside a JSON blob that doesn't match the schema
        // (e.g. missing required fields). The bad entry is
        // dropped; the good one survives — no "one bad apple
        // spoils the list" failure mode.
        let config = make_config();
        config.write(|v| {
            v[KEY_RTL_TCP_CLIENT_FAVORITES] = serde_json::json!([
                { "key": "shack-pi.local.:1234", "nickname": "Shack Pi" },
                { "this": "is not a FavoriteEntry" },
            ]);
        });
        let loaded = load_favorites(&config);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].key, "shack-pi.local.:1234");
    }

    #[test]
    fn now_unix_seconds_is_monotonic_within_call() {
        // Not a real monotonicity test — just a smoke-test that
        // the helper returns a sensible modern value. Anything
        // past 2020-01-01 (Unix 1_577_836_800) is clearly real
        // wall-clock time and not a clock-skew fallback.
        assert!(now_unix_seconds() > 1_577_836_800);
    }

    #[test]
    fn last_connected_round_trip() {
        let config = make_config();
        assert!(load_last_connected(&config).is_none());
        let server = LastConnectedServer {
            host: "192.168.1.5".to_string(),
            port: 1234,
            nickname: "shack-pi".to_string(),
        };
        save_last_connected(&config, &server);
        let loaded = load_last_connected(&config).expect("loaded");
        assert_eq!(loaded.host, server.host);
        assert_eq!(loaded.port, server.port);
        assert_eq!(loaded.nickname, server.nickname);
    }

    #[test]
    fn last_connected_loader_tolerates_malformed_entry() {
        // Schema drift: an older version persisted a plain string.
        // New loader should return None rather than panic.
        let config = make_config();
        config.write(|v| {
            v[KEY_RTL_TCP_CLIENT_LAST_CONNECTED] = serde_json::json!("shack-pi:1234");
        });
        assert!(load_last_connected(&config).is_none());
    }
}
