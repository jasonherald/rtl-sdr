//! Source device configuration panel — device selector, RTL-SDR /
//! Network / File / RTL-TCP controls.

use std::sync::Arc;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;
use sdr_types::RtlTcpConnectionState;

/// Config key for the persisted list of favorited `rtl_tcp`
/// servers. Stored as a JSON array of [`FavoriteEntry`] objects,
/// each keyed by the stable `hostname:port` identity produced by
/// `window.rs::favorite_key` (NOT by DNS-SD `instance_name` —
/// operators can rename the mDNS nickname, which would otherwise
/// silently drop the star on any rename). The read path
/// ([`load_favorites`]) also accepts legacy bare-string
/// `hostname:port` entries from the PR #335 schema for backward
/// compatibility; unknown / stale objects are skipped with a
/// `tracing::warn!` so schema drift stays diagnosable.
pub const KEY_RTL_TCP_CLIENT_FAVORITES: &str = "rtl_tcp_client_favorites";
/// Config key for the persisted last-connected server. Stored as
/// a JSON object `{ host, port, nickname }` so we can repopulate
/// the hostname / port rows on app launch without waiting for
/// mDNS to rediscover.
pub const KEY_RTL_TCP_CLIENT_LAST_CONNECTED: &str = "rtl_tcp_client_last_connected";

/// Config key for the persisted AGC type selection (Off /
/// Hardware / Software). Written by the source panel's AGC
/// combo on every user change, read back at startup so the
/// combo repopulates with the user's last choice instead of
/// the fresh-install default.
///
/// Legacy compat: pre-#354 builds persisted a boolean under
/// `rtl_sdr_agc_enabled` representing "hardware AGC on/off".
/// [`load_agc_type`] migrates it to `Hardware` (true) or `Off`
/// (false) on first read when the new key is absent.
pub const KEY_AGC_TYPE: &str = "rtl_sdr_agc_type";
/// Legacy config key for the pre-#354 boolean AGC switch.
/// Read-only now — the new `KEY_AGC_TYPE` supersedes it on
/// write. Preserved so users upgrading from an older version
/// don't lose their AGC setting on first launch.
pub const KEY_LEGACY_AGC_ENABLED: &str = "rtl_sdr_agc_enabled";

/// Config key for the persisted bias-T toggle. Powers an
/// inline LNA over the dongle's coax connector — only
/// meaningful on RTL-SDR hardware that has the bias-T circuit
/// (V3+; older clones don't). Default `false` so users without
/// powered antennas don't accidentally feed 5 V into a passive
/// LNA. Per issue #537.
pub const KEY_SOURCE_RTL_BIAS_TEE: &str = "src_rtl_bias_tee";

/// Config key for persisted manual tuner gain in dB. Only
/// applied when AGC is `Off` (hardware/software AGC overrides
/// manual gain). Default `0.0` matches the spin row's initial
/// value. Per issue `#551`.
pub const KEY_SOURCE_RTL_GAIN_DB: &str = "src_rtl_gain_db";

/// Config key for persisted PPM frequency correction. Default
/// `0` (no correction). Per issue `#551`.
pub const KEY_SOURCE_RTL_PPM: &str = "src_rtl_ppm";

// ─── Source-panel persistence keys (#552) ───────────────────────────
// Top-level + frontend + per-source-type config rows that today
// reset to widget defaults across restart. Mechanical mirror of
// the `KEY_SOURCE_RTL_*` pattern for tuner-specific settings —
// each key has a matching `load_*` / `save_*` pair below and a
// `connect_source_panel` restore-then-wire block in `window.rs`.

/// Currently-selected source type (RTL-SDR / Network / File /
/// RTL-TCP). Stored as the combo-row index per the `DEVICE_*`
/// constants (`DEVICE_RTLSDR` / `DEVICE_NETWORK` / etc.).
/// Default `DEVICE_RTLSDR` (`0`). Per issue `#552`.
pub const KEY_SOURCE_DEVICE_INDEX: &str = "src_device_index";
/// Sample rate dropdown index (into `SAMPLE_RATES`). Default
/// matches the widget's initial selection. Per issue `#552`.
pub const KEY_SOURCE_SAMPLE_RATE_INDEX: &str = "src_sample_rate_index";
/// Decimation dropdown index (into `DECIMATION_FACTORS`).
/// Default `0` (1× / no decimation). Per issue `#552`.
pub const KEY_SOURCE_DECIMATION_INDEX: &str = "src_decimation_index";
/// DC blocking toggle on the IQ frontend. Default `true`
/// matches the widget's initial state. Per issue `#552`.
pub const KEY_SOURCE_DC_BLOCKING: &str = "src_dc_blocking";
/// IQ DC correction toggle. Default `false`. Per issue `#552`.
pub const KEY_SOURCE_IQ_CORRECTION: &str = "src_iq_correction";
/// IQ swap toggle. Default `false`. Per issue `#552`.
pub const KEY_SOURCE_IQ_INVERSION: &str = "src_iq_inversion";
/// Raw-Network source hostname. Default `"localhost"`. Note:
/// the `rtl_tcp` client maintains its own per-server hostname/port
/// state via `KEY_RTL_TCP_CLIENT_FAVORITES` — this key is for
/// the raw IQ-stream Network source only. Per issue `#552`.
pub const KEY_SOURCE_NETWORK_HOSTNAME: &str = "src_network_hostname";
/// Raw-Network source port. Default `1234`. Per issue `#552`.
pub const KEY_SOURCE_NETWORK_PORT: &str = "src_network_port";
/// Raw-Network protocol dropdown index (into TCP/UDP). Default
/// `NETWORK_PROTOCOL_TCPCLIENT_IDX`. Per issue `#552`.
pub const KEY_SOURCE_NETWORK_PROTOCOL_INDEX: &str = "src_network_protocol_index";
/// File source playback path. Default empty. Per issue `#552`.
pub const KEY_SOURCE_FILE_PATH: &str = "src_file_path";

/// Device selector index for RTL-SDR.
pub const DEVICE_RTLSDR: u32 = 0;
/// Device selector index for Network.
pub const DEVICE_NETWORK: u32 = 1;
/// Device selector index for File.
pub const DEVICE_FILE: u32 = 2;
/// Device selector index for RTL-TCP (rtl_tcp-protocol network client).
pub const DEVICE_RTLTCP: u32 = 3;

/// AGC type for the source panel's three-way selector. Users pick
/// between the tuner's hardware AGC (overshoots on strong signals,
/// see #332), the pure-DSP software AGC on the IQ stream (well-
/// behaved, see #354), or Off (manual gain). Fresh installs
/// default to `Software` — hardware AGC is the documented-problem
/// path, so new users get the well-behaved option out of the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgcType {
    /// No AGC — manual gain is the user's sole control.
    Off,
    /// RTL-SDR hardware tuner AGC (VGA auto-mode via
    /// `rtlsdr_set_tuner_gain_mode(false)`).
    Hardware,
    /// Pure-DSP envelope follower on IQ inside `IfChain`.
    Software,
}

impl AgcType {
    /// Default AGC type for fresh installs. Software-first rule
    /// described on the enum docstring — hardware AGC is the
    /// documented-problem path, software is the smooth default.
    pub const DEFAULT: Self = Self::Software;
}

/// Combo row index for `AgcType::Off`. Load-bearing — must match
/// the order of the `StringList` entries built in
/// `build_rtlsdr_rows`, and the `agc_type_from_selected` /
/// `selected_from_agc_type` helpers.
pub const AGC_TYPE_OFF_IDX: u32 = 0;
/// Combo row index for `AgcType::Hardware`.
pub const AGC_TYPE_HARDWARE_IDX: u32 = 1;
/// Combo row index for `AgcType::Software`.
pub const AGC_TYPE_SOFTWARE_IDX: u32 = 2;

/// Translate a combo row `selected()` index into an `AgcType`.
/// Returns `None` on an unknown index — GTK can emit transient
/// out-of-range values (e.g. `gtk4::INVALID_LIST_POSITION`)
/// during widget teardown or model-swap churn. Returning
/// `Option` forces call sites to make an explicit choice rather
/// than silently coercing a transient to [`AgcType::DEFAULT`]
/// and dispatching it as a real mode change (which would
/// corrupt the persisted config).
#[must_use]
pub fn agc_type_from_selected(idx: u32) -> Option<AgcType> {
    match idx {
        AGC_TYPE_OFF_IDX => Some(AgcType::Off),
        AGC_TYPE_HARDWARE_IDX => Some(AgcType::Hardware),
        AGC_TYPE_SOFTWARE_IDX => Some(AgcType::Software),
        _ => None,
    }
}

/// Inverse of [`agc_type_from_selected`]: translate an `AgcType`
/// into the combo row index to call `set_selected` with.
#[must_use]
pub fn selected_from_agc_type(agc_type: AgcType) -> u32 {
    match agc_type {
        AgcType::Off => AGC_TYPE_OFF_IDX,
        AgcType::Hardware => AGC_TYPE_HARDWARE_IDX,
        AgcType::Software => AGC_TYPE_SOFTWARE_IDX,
    }
}

/// Label shown in the source combo's RTL-SDR slot when no dongle
/// is detected on the USB bus. Kept as a pub const so the hotplug
/// poller and the probe helper render identical text.
pub const RTLSDR_ABSENT_LABEL: &str = "No RTL-SDR device found";

/// Probe the USB bus for an RTL-SDR dongle and return the label
/// to show in the source combo's RTL-SDR slot (index 0).
///
/// Returns the librtlsdr device name of the first matching device
/// when present (e.g. `"Generic RTL2832U OEM"`), or
/// [`RTLSDR_ABSENT_LABEL`] when the bus has no dongle. Cheap
/// enough to call from a 3 s hotplug poller on the GTK main
/// thread — `sdr_rtlsdr::get_device_count` is a libusb enumerate
/// filtered by vendor/product ID, and `get_device_name` re-runs
/// the same enumerate to reach the Nth match.
///
/// When `get_device_count` reports > 0 but `get_device_name`
/// returns an empty string (shouldn't happen outside of a race
/// where the device was unplugged between the two enumerate
/// calls), we fall back to the generic "RTL-SDR" label so the
/// UI stays usable rather than rendering an empty combo entry.
pub fn probe_rtlsdr_device_label() -> String {
    if sdr_rtlsdr::get_device_count() == 0 {
        return RTLSDR_ABSENT_LABEL.to_string();
    }
    let name = sdr_rtlsdr::get_device_name(0);
    if name.is_empty() {
        "RTL-SDR".to_string()
    } else {
        name
    }
}

/// Default subtitle for the RTL-TCP status row before any
/// `DspToUi::RtlTcpConnectionState` event has arrived (or after a
/// Disconnect). Kept as a const so the empty-at-startup and
/// empty-after-disconnect paths render identical text.
pub const RTL_TCP_STATUS_DISCONNECTED_SUBTITLE: &str = "Disconnected";

/// Combo-row index for `Role::Control` on `rtl_tcp_role_row`.
/// Load-bearing — must match the `StringList` order built in
/// `build_source_panel`. Per issue #396.
pub const RTL_TCP_ROLE_CONTROL_IDX: u32 = 0;
/// Combo-row index for `Role::Listen` on `rtl_tcp_role_row`.
pub const RTL_TCP_ROLE_LISTEN_IDX: u32 = 1;

/// Config key for the persisted last-used connection role.
/// Stored as a `"control"` / `"listen"` string via
/// `FavoriteRole`'s serde representation. Per issue #396.
pub const KEY_RTL_TCP_CLIENT_LAST_ROLE: &str = "rtl_tcp_client_last_role";

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
    /// Backing `StringList` for `device_row`. Exposed so a
    /// hotplug poller can update the RTL-SDR slot's label (entry
    /// index 0) via `splice` when the probed device name or
    /// presence changes, without replacing the whole model (which
    /// would reset the selection).
    pub device_model: gtk4::StringList,
    /// RTL-SDR sample rate selector.
    pub sample_rate_row: adw::ComboRow,
    /// RTL-SDR gain control.
    pub gain_row: adw::SpinRow,
    /// RTL-SDR AGC toggle.
    /// Three-way AGC selector (Off / Hardware / Software). See
    /// [`AgcType`] for the enum shape and `AGC_TYPE_*_IDX` for
    /// the combo indices. Dispatch wiring in `window.rs` reads
    /// `.selected()` and routes to the appropriate `UiToDsp`
    /// message.
    pub agc_row: adw::ComboRow,
    /// RTL-SDR PPM frequency correction.
    pub ppm_row: adw::SpinRow,
    /// RTL-SDR bias tee toggle. Powers an inline LNA over the
    /// coax (V3+ dongles only — older clones lack the circuit
    /// and the driver returns Err on those, surfaced as a
    /// toast). Visibility-gated to local RTL-SDR USB only in
    /// this panel (hidden for Network / File / `rtl_tcp`).
    /// `rtl_tcp` has its own remote-bias-T default in the
    /// share-server panel rather than reusing this row. Per
    /// issue #537.
    pub bias_tee_row: adw::SwitchRow,
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
    /// "Connection role" picker (Control / Listen) shown only
    /// when the RTL-TCP source type is selected. Wire-level
    /// default is Control (#392 back-compat); Listen opts into
    /// concurrent read-only access to a server that already has
    /// a controller. Per issue #396.
    pub rtl_tcp_role_row: adw::ComboRow,
    /// "Server key" password entry shown when the RTL-TCP source
    /// is selected AND the active server either advertises
    /// `auth_required=true` via mDNS TXT or has a saved key in
    /// the keyring. Per issue #396. The key bytes themselves
    /// are persisted to the OS keyring (not this widget's
    /// value), so the `EntryRow` is cleared on source-type change
    /// to avoid leaking the value into widget-tree dumps.
    pub rtl_tcp_auth_key_row: adw::PasswordEntryRow,

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
            codec,
            // The subtitle copy intentionally omits the role —
            // the status-bar badge carries it. Per CodeRabbit
            // round 1 on PR #408.
            granted_role: _,
        } => {
            // Only surface the codec when it's actually compressing —
            // the common "None" case (every legacy server, plus our
            // own server with compression off) stays at the existing
            // short form so the subtitle doesn't grow a meaningless
            // "codec: None" suffix on every connection.
            if codec == "None" {
                format!("Connected — {tuner_name} ({gain_count} gains)")
            } else {
                format!("Connected — {tuner_name} ({gain_count} gains, {codec})")
            }
        }
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
        // Role-denial terminal states (#396). These show short
        // actionable subtitles so the user knows WHY the
        // connection didn't advance — the full toast UX with
        // "Take control" / "Connect as Listener" buttons lives
        // in `window.rs`.
        RtlTcpConnectionState::ControllerBusy => "Controller slot is occupied".to_string(),
        RtlTcpConnectionState::AuthRequired => "Server requires a key".to_string(),
        RtlTcpConnectionState::AuthFailed => "Key rejected".to_string(),
    }
}

/// Default sample rate selector index (2.4 MHz = index 7).
const DEFAULT_SAMPLE_RATE_INDEX: u32 = 7;

/// Build RTL-SDR-specific rows: sample rate, gain, AGC, PPM correction.
fn build_rtlsdr_rows() -> (
    adw::ComboRow,
    adw::SpinRow,
    adw::ComboRow,
    adw::SpinRow,
    adw::SwitchRow,
) {
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

    // AGC type selector: Off / Hardware / Software. Labels are
    // terse but qualify each option so the user understands what
    // "Hardware" and "Software" mean in context. Order is load-
    // bearing — must match the `AGC_TYPE_*_IDX` constants and
    // the `agc_type_from_selected` mapping.
    let agc_type_model = gtk4::StringList::new(&["Off", "Hardware (tuner)", "Software (IQ)"]);
    let agc_row = adw::ComboRow::builder()
        .title("AGC")
        .subtitle("Automatic gain control")
        .model(&agc_type_model)
        .selected(selected_from_agc_type(AgcType::DEFAULT))
        .build();

    let ppm_adj = gtk4::Adjustment::new(DEFAULT_PPM, MIN_PPM, MAX_PPM, PPM_STEP, PPM_PAGE, 0.0);
    let ppm_row = adw::SpinRow::builder()
        .title("PPM Correction")
        .subtitle("Crystal frequency offset")
        .adjustment(&ppm_adj)
        .digits(0)
        .build();

    // Bias tee — powers an inline LNA over the coax. Off by
    // default so users without powered antennas don't drive
    // unexpected current into a passive antenna's centre
    // conductor. Per issue #537.
    let bias_tee_row = adw::SwitchRow::builder()
        .title("Bias-T")
        .subtitle("Power an inline LNA over the antenna coax")
        .active(false)
        .build();

    (sample_rate_row, gain_row, agc_row, ppm_row, bias_tee_row)
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
    agc_row: &adw::ComboRow,
    ppm_row: &adw::SpinRow,
    bias_tee_row: &adw::SwitchRow,
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
        bias_tee_row,
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
            // Bias tee is local-RTL-SDR only — see the
            // initial-visibility block for the same gating
            // rationale. Per issue #537.
            bias_tee_row.set_visible(is_rtlsdr);

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
    // Initial label for the RTL-SDR slot — probed against the USB
    // bus so a first-launch user without a dongle sees "No RTL-SDR
    // device found" instead of the app lying that a dongle is
    // present. Kept in sync with hotplug events by the probe
    // poller wired in `connect_source_rtlsdr_probe` in window.rs.
    let device_model = gtk4::StringList::new(&[
        &probe_rtlsdr_device_label(),
        "Network",
        "File",
        "RTL-TCP (network)",
    ]);
    let device_row = adw::ComboRow::builder()
        .title("Device")
        .model(&device_model)
        .build();

    let (sample_rate_row, gain_row, agc_row, ppm_row, bias_tee_row) = build_rtlsdr_rows();
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

    // Connection-role picker (#396). AdwComboRow with two
    // entries: "Control" (index 0) and "Listen" (index 1).
    // Default Control matches the pre-#392 single-client flow
    // every legacy rtl_tcp client / server assumes.
    let rtl_tcp_role_model = gtk4::StringList::new(&["Control", "Listen"]);
    let rtl_tcp_role_row = adw::ComboRow::builder()
        .title("Connection role")
        .subtitle("Control drives tuning; Listen receives IQ read-only")
        .model(&rtl_tcp_role_model)
        .selected(RTL_TCP_ROLE_CONTROL_IDX)
        .visible(false)
        .build();

    // Server key entry (#394 + #396). Password-purpose entry
    // row — masked by default, revealable via AdwPasswordEntryRow's
    // built-in "peek" button. Kept separate from the main hostname
    // / port block so the user sees it only when a key is
    // actually needed (server advertises auth_required=true OR
    // there's a saved key for the active host:port).
    let rtl_tcp_auth_key_row = adw::PasswordEntryRow::builder()
        .title("Server key")
        .visible(false)
        .build();

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
    group.add(&bias_tee_row);
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
    group.add(&rtl_tcp_role_row);
    group.add(&rtl_tcp_auth_key_row);
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
    // Bias tee is local-RTL-SDR only — the rtl_tcp wire
    // protocol exposes a bias-T command but the rtl_tcp client
    // doesn't currently surface it in this panel (the
    // server-side panel has its own toggle). Keep this row
    // hidden on RTL-TCP to match. Per issue #537.
    bias_tee_row.set_visible(is_rtlsdr);
    hostname_row.set_visible(is_network || is_rtltcp);
    port_row.set_visible(is_network || is_rtltcp);
    protocol_row.set_visible(is_network);
    file_path_row.set_visible(is_file);
    rtl_tcp_discovered_row.set_visible(is_rtltcp);
    rtl_tcp_status_row.set_visible(is_rtltcp);
    rtl_tcp_role_row.set_visible(is_rtltcp);
    // Auth key row stays hidden until a specific signal
    // (mDNS TXT auth_required=true OR saved key exists for the
    // active host:port). Starting hidden avoids prompting users
    // on servers that don't require auth. The wiring in
    // window.rs flips visibility via the discovery / last-
    // connected load paths.

    connect_device_visibility(
        &device_row,
        &sample_rate_row,
        &gain_row,
        &agc_row,
        &ppm_row,
        &bias_tee_row,
        &hostname_row,
        &port_row,
        &protocol_row,
        &file_path_row,
    );
    connect_rtl_tcp_visibility(
        &device_row,
        &rtl_tcp_discovered_row,
        &rtl_tcp_status_row,
        &rtl_tcp_role_row,
        &rtl_tcp_auth_key_row,
    );

    // Controls connected to DSP pipeline via window.rs

    SourcePanel {
        widget: group,
        device_row,
        device_model,
        sample_rate_row,
        gain_row,
        agc_row,
        ppm_row,
        bias_tee_row,
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
        rtl_tcp_role_row,
        rtl_tcp_auth_key_row,
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
    rtl_tcp_role_row: &adw::ComboRow,
    rtl_tcp_auth_key_row: &adw::PasswordEntryRow,
) {
    device_row.connect_selected_notify(glib::clone!(
        #[weak]
        rtl_tcp_discovered_row,
        #[weak]
        rtl_tcp_status_row,
        #[weak]
        rtl_tcp_role_row,
        #[weak]
        rtl_tcp_auth_key_row,
        move |row| {
            let is_rtltcp = row.selected() == DEVICE_RTLTCP;
            rtl_tcp_discovered_row.set_visible(is_rtltcp);
            rtl_tcp_status_row.set_visible(is_rtltcp);
            rtl_tcp_role_row.set_visible(is_rtltcp);
            // Auth key row stays hidden until the discovery /
            // last-connected layer in window.rs flips it on via
            // the `auth_required` hint or a saved-key lookup.
            // Flipping to a non-RTLX source type always hides
            // it AND clears the entry so the value doesn't
            // linger in the widget tree for other source types
            // that don't use it. Per #396.
            if !is_rtltcp {
                rtl_tcp_auth_key_row.set_visible(false);
                rtl_tcp_auth_key_row.set_text("");
            }
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
    /// Last-used role against this server: `"control"` or
    /// `"listen"`. Stored as a string (via
    /// `serde(rename_all = "snake_case")` on the enum) rather
    /// than the raw enum so the JSON is human-readable and a
    /// future enum-variant rename doesn't silently break
    /// deserialization. `None` until the user explicitly picks
    /// a role for this server; the connect path defaults to
    /// Control when `None`. Per issue #396.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_role: Option<FavoriteRole>,
    /// Whether the most recent mDNS TXT for this server
    /// advertised `auth_required=true`. Pre-populated from
    /// discovery events so the UI can reveal the Server key
    /// field BEFORE the user clicks Connect (saves a round
    /// trip through the `AuthRequired` error path). `None`
    /// means "unknown" — either we've never seen a TXT, or the
    /// record didn't carry the field (older server, non-sdr-rs
    /// server). Per issue #396.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_required: Option<bool>,
}

/// Favorite-entry serialized form of a client's preferred role
/// for a given server. `snake_case` so the JSON surface reads
/// as `"control"` / `"listen"` — easier to hand-edit and
/// more forgiving across future enum changes. Per #396.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FavoriteRole {
    Control,
    Listen,
}

impl FavoriteRole {
    /// Translate to the wire-level `sdr_server_rtltcp::extension::Role`
    /// the client hello will carry. Kept as a separate crate
    /// boundary so `FavoriteEntry` doesn't force a dep on
    /// `sdr-server-rtltcp` at every call site that reads a
    /// serialized favorite.
    pub fn as_wire_role(self) -> sdr_server_rtltcp::extension::Role {
        match self {
            Self::Control => sdr_server_rtltcp::extension::Role::Control,
            Self::Listen => sdr_server_rtltcp::extension::Role::Listen,
        }
    }

    /// Inverse: build a `FavoriteRole` from a wire-level
    /// `Role`. Used when persisting a newly-chosen role back to
    /// the favorite entry after a successful connect.
    pub fn from_wire_role(role: sdr_server_rtltcp::extension::Role) -> Self {
        match role {
            sdr_server_rtltcp::extension::Role::Control => Self::Control,
            sdr_server_rtltcp::extension::Role::Listen => Self::Listen,
        }
    }
}

/// Load the persisted favorites list. Returns an empty `Vec` on
/// first launch / absent / corrupt config — safe to call
/// unconditionally.
///
/// **Backward compatibility:** accepts two on-disk shapes:
///
/// 1. **Current (PR #315):** `Vec<FavoriteEntry>` — array of JSON
///    objects, each decoded via `serde_json::from_value`. Objects
///    that fail to deserialize are skipped AND logged at
///    `tracing::warn!` with the offending entry index and the
///    serde error, so schema drift is diagnosable in bug reports
///    instead of silently eating favorites.
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
                    // for the server to re-announce. Role and
                    // auth-required default to `None` — the
                    // connect path treats both as "unknown"
                    // (role defaults to Control, auth_required
                    // is decided by the server on first
                    // connect).
                    Some(FavoriteEntry {
                        key: s.to_string(),
                        nickname: s.to_string(),
                        tuner_name: None,
                        gain_count: None,
                        last_seen_unix: None,
                        requested_role: None,
                        auth_required: None,
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

/// Load the persisted AGC type selection. Returns
/// [`AgcType::DEFAULT`] on first launch / absent / corrupt
/// config. Falls back to the legacy `KEY_LEGACY_AGC_ENABLED`
/// boolean when the new key is absent — mapping `true →
/// Hardware` and `false → Off` so users upgrading from a
/// pre-#354 build keep their AGC setting on first startup.
pub fn load_agc_type(config: &Arc<ConfigManager>) -> AgcType {
    config.read(|v| {
        if let Some(entry) = v.get(KEY_AGC_TYPE) {
            // New key present — trust it.
            if let Ok(t) = serde_json::from_value::<AgcType>(entry.clone()) {
                return t;
            }
        }
        // Fall back to the legacy boolean, then to the default
        // if that's absent too.
        v.get(KEY_LEGACY_AGC_ENABLED)
            .and_then(serde_json::Value::as_bool)
            .map_or(AgcType::DEFAULT, |on| {
                if on { AgcType::Hardware } else { AgcType::Off }
            })
    })
}

/// Persist the AGC type selection. Written on every
/// `agc_row.connect_selected_notify` event in `window.rs`.
/// Does NOT write the legacy `KEY_LEGACY_AGC_ENABLED` key —
/// that one is read-only from here on, so a downgrade to a
/// pre-#354 build would see a stale legacy value, but we
/// accept that trade-off rather than maintaining two keys in
/// lockstep forever.
pub fn save_agc_type(config: &Arc<ConfigManager>, agc_type: AgcType) {
    config.write(|v| {
        v[KEY_AGC_TYPE] = serde_json::to_value(agc_type).unwrap_or(serde_json::Value::Null);
    });
}

/// Load the persisted bias-T toggle. Defaults to `false` —
/// users without powered antennas should never have 5 V on
/// the coax accidentally on first launch. Per issue #537.
#[must_use]
pub fn load_source_rtl_bias_tee(config: &Arc<ConfigManager>) -> bool {
    config.read(|v| {
        v.get(KEY_SOURCE_RTL_BIAS_TEE)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}

/// Persist the bias-T toggle. Written on every
/// `bias_tee_row.connect_active_notify` event in
/// `window.rs::connect_source_panel`. Per issue #537.
pub fn save_source_rtl_bias_tee(config: &Arc<ConfigManager>, enabled: bool) {
    config.write(|v| {
        v[KEY_SOURCE_RTL_BIAS_TEE] = serde_json::json!(enabled);
    });
}

/// Load the persisted manual tuner gain in dB. Default `0.0` —
/// matches the spin row's initial value. Per issue `#551`.
#[must_use]
pub fn load_source_rtl_gain_db(config: &Arc<ConfigManager>) -> f64 {
    config.read(|v| {
        v.get(KEY_SOURCE_RTL_GAIN_DB)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0)
    })
}

/// Persist the manual tuner gain in dB. Per issue `#551`.
pub fn save_source_rtl_gain_db(config: &Arc<ConfigManager>, gain_db: f64) {
    config.write(|v| {
        v[KEY_SOURCE_RTL_GAIN_DB] = serde_json::json!(gain_db);
    });
}

/// Load the persisted PPM frequency correction. Default `0`.
/// Per issue `#551`.
#[must_use]
pub fn load_source_rtl_ppm(config: &Arc<ConfigManager>) -> i32 {
    config.read(|v| {
        v.get(KEY_SOURCE_RTL_PPM)
            .and_then(serde_json::Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(0)
    })
}

/// Persist the PPM frequency correction. Per issue `#551`.
pub fn save_source_rtl_ppm(config: &Arc<ConfigManager>, ppm: i32) {
    config.write(|v| {
        v[KEY_SOURCE_RTL_PPM] = serde_json::json!(ppm);
    });
}

// ─── #552 source-panel persistence helpers ──────────────────────────
//
// All follow the same shape as `load_source_rtl_*` /
// `save_source_rtl_*`: a tolerant `load` that falls back to a
// safe default on missing-key or wrong-type, paired with an
// idempotent `save`. The wiring layer in
// `window.rs::connect_source_panel` calls each `save_*` from the
// row's change-notify handler and each `load_*` once at panel
// build time (restore-before-wire idiom).

/// Load the persisted source-type combo index. Defaults to
/// [`DEVICE_RTLSDR`].
#[must_use]
pub fn load_source_device_index(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
        v.get(KEY_SOURCE_DEVICE_INDEX)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(DEVICE_RTLSDR)
    })
}

pub fn save_source_device_index(config: &Arc<ConfigManager>, index: u32) {
    config.write(|v| {
        v[KEY_SOURCE_DEVICE_INDEX] = serde_json::json!(index);
    });
}

/// Load the persisted sample-rate combo index. Defaults to `0`
/// (the first entry in `SAMPLE_RATES`, matching the widget's
/// initial selection at panel build time).
#[must_use]
pub fn load_source_sample_rate_index(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
        v.get(KEY_SOURCE_SAMPLE_RATE_INDEX)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    })
}

pub fn save_source_sample_rate_index(config: &Arc<ConfigManager>, index: u32) {
    config.write(|v| {
        v[KEY_SOURCE_SAMPLE_RATE_INDEX] = serde_json::json!(index);
    });
}

/// Load the persisted decimation combo index. Defaults to `0`
/// (1× decimation).
#[must_use]
pub fn load_source_decimation_index(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
        v.get(KEY_SOURCE_DECIMATION_INDEX)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    })
}

pub fn save_source_decimation_index(config: &Arc<ConfigManager>, index: u32) {
    config.write(|v| {
        v[KEY_SOURCE_DECIMATION_INDEX] = serde_json::json!(index);
    });
}

/// Load the persisted DC-blocking toggle. Defaults to `true`
/// (matches the widget's initial state).
#[must_use]
pub fn load_source_dc_blocking(config: &Arc<ConfigManager>) -> bool {
    config.read(|v| {
        v.get(KEY_SOURCE_DC_BLOCKING)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    })
}

pub fn save_source_dc_blocking(config: &Arc<ConfigManager>, enabled: bool) {
    config.write(|v| {
        v[KEY_SOURCE_DC_BLOCKING] = serde_json::json!(enabled);
    });
}

/// Load the persisted IQ-correction toggle. Defaults to `false`.
#[must_use]
pub fn load_source_iq_correction(config: &Arc<ConfigManager>) -> bool {
    config.read(|v| {
        v.get(KEY_SOURCE_IQ_CORRECTION)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}

pub fn save_source_iq_correction(config: &Arc<ConfigManager>, enabled: bool) {
    config.write(|v| {
        v[KEY_SOURCE_IQ_CORRECTION] = serde_json::json!(enabled);
    });
}

/// Load the persisted IQ-swap toggle. Defaults to `false`.
#[must_use]
pub fn load_source_iq_inversion(config: &Arc<ConfigManager>) -> bool {
    config.read(|v| {
        v.get(KEY_SOURCE_IQ_INVERSION)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}

pub fn save_source_iq_inversion(config: &Arc<ConfigManager>, enabled: bool) {
    config.write(|v| {
        v[KEY_SOURCE_IQ_INVERSION] = serde_json::json!(enabled);
    });
}

/// Load the persisted raw-Network hostname. Defaults to
/// `"localhost"` (matches the widget's initial value).
#[must_use]
pub fn load_source_network_hostname(config: &Arc<ConfigManager>) -> String {
    config.read(|v| {
        v.get(KEY_SOURCE_NETWORK_HOSTNAME)
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "localhost".to_string(), ToString::to_string)
    })
}

pub fn save_source_network_hostname(config: &Arc<ConfigManager>, hostname: &str) {
    config.write(|v| {
        v[KEY_SOURCE_NETWORK_HOSTNAME] = serde_json::json!(hostname);
    });
}

/// Load the persisted raw-Network port. Defaults to `1234`
/// (matches `DEFAULT_PORT`).
#[must_use]
pub fn load_source_network_port(config: &Arc<ConfigManager>) -> u16 {
    config.read(|v| {
        v.get(KEY_SOURCE_NETWORK_PORT)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .unwrap_or(1234)
    })
}

pub fn save_source_network_port(config: &Arc<ConfigManager>, port: u16) {
    config.write(|v| {
        v[KEY_SOURCE_NETWORK_PORT] = serde_json::json!(port);
    });
}

/// Load the persisted raw-Network protocol combo index.
/// Defaults to [`NETWORK_PROTOCOL_TCPCLIENT_IDX`].
#[must_use]
pub fn load_source_network_protocol_index(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
        v.get(KEY_SOURCE_NETWORK_PROTOCOL_INDEX)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(NETWORK_PROTOCOL_TCPCLIENT_IDX)
    })
}

pub fn save_source_network_protocol_index(config: &Arc<ConfigManager>, index: u32) {
    config.write(|v| {
        v[KEY_SOURCE_NETWORK_PROTOCOL_INDEX] = serde_json::json!(index);
    });
}

/// Load the persisted File-source playback path. Defaults to
/// the empty string (no file selected).
#[must_use]
pub fn load_source_file_path(config: &Arc<ConfigManager>) -> String {
    config.read(|v| {
        v.get(KEY_SOURCE_FILE_PATH)
            .and_then(serde_json::Value::as_str)
            .map_or_else(String::new, ToString::to_string)
    })
}

pub fn save_source_file_path(config: &Arc<ConfigManager>, path: &str) {
    config.write(|v| {
        v[KEY_SOURCE_FILE_PATH] = serde_json::json!(path);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed Unix timestamp used in the favorites round-trip test
    /// to pin the `last_seen_unix` field. Value is arbitrary (from
    /// November 2023) but deliberately chosen to be well past any
    /// clock-skew-fallback sentinel and well before `u32::MAX`
    /// seconds so overflow edges aren't in play.
    const TEST_LAST_SEEN_UNIX: u64 = 1_700_000_000;
    /// Unix timestamp for 2020-01-01T00:00:00Z. Used by the
    /// `now_unix_seconds` smoke test as a "modern wall-clock"
    /// floor — anything past this is clearly real time and not a
    /// clock-skew fallback returning 0.
    const MODERN_UNIX_FLOOR: u64 = 1_577_836_800;

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
        // Connected with `None` codec keeps the short form — the
        // common path every legacy server hits, and the default for
        // our own client. Adding a "(None)" suffix here would noise
        // up every single connection in exchange for zero signal.
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::Connected {
                tuner_name: "R820T".into(),
                gain_count: 29,
                codec: "None".into(),
                granted_role: Some(true),
            }),
            "Connected — R820T (29 gains)"
        );
        // Connected with a non-`None` codec gets an extra suffix so
        // the user can see which codec actually landed. Signals
        // that compression is active without forcing them to hunt
        // through logs.
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::Connected {
                tuner_name: "R820T".into(),
                gain_count: 29,
                codec: "LZ4".into(),
                granted_role: Some(true),
            }),
            "Connected — R820T (29 gains, LZ4)"
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
        // Role-denial states (#396) get their own short
        // subtitles — no reason string needed because the
        // variant itself IS the reason. Lock in each copy
        // against accidental drift; a typo here would ship
        // to users without CI catching it otherwise.
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::ControllerBusy),
            "Controller slot is occupied",
        );
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::AuthRequired),
            "Server requires a key",
        );
        assert_eq!(
            format_rtl_tcp_state(&RtlTcpConnectionState::AuthFailed),
            "Key rejected",
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
                last_seen_unix: Some(TEST_LAST_SEEN_UNIX),
                requested_role: Some(FavoriteRole::Listen),
                auth_required: Some(true),
            },
            FavoriteEntry {
                key: "attic-pi.local.:1234".into(),
                nickname: "Attic Pi".into(),
                tuner_name: None,
                gain_count: None,
                last_seen_unix: None,
                requested_role: None,
                auth_required: None,
            },
        ];
        save_favorites(&config, &favs);
        let loaded = load_favorites(&config);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].key, "shack-pi.local.:1234");
        assert_eq!(loaded[0].nickname, "Shack Pi");
        assert_eq!(loaded[0].tuner_name.as_deref(), Some("R820T"));
        assert_eq!(loaded[0].gain_count, Some(29));
        assert_eq!(loaded[0].last_seen_unix, Some(TEST_LAST_SEEN_UNIX));
        // Role + auth_required round-trip on the opt-in side.
        // Per #396: the JSON surface carries these through the
        // serde `snake_case` rename and skip-if-none attributes.
        assert_eq!(loaded[0].requested_role, Some(FavoriteRole::Listen));
        assert_eq!(loaded[0].auth_required, Some(true));
        // Second entry has every optional field None → must
        // round-trip as None, NOT as missing / default values.
        assert!(loaded[1].tuner_name.is_none());
        assert!(loaded[1].gain_count.is_none());
        assert!(loaded[1].last_seen_unix.is_none());
        assert!(loaded[1].requested_role.is_none());
        assert!(loaded[1].auth_required.is_none());
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
        // Role + auth-required fields are #396 additions and
        // must also default to `None` for legacy bare-string
        // entries — the connect path treats `None` as
        // "unknown, default to Control / don't pre-reveal the
        // auth row." A regression that silently wrote `Some`
        // defaults here would change the UX for every
        // pre-#396 favorite on the first launch after upgrade.
        assert!(loaded[0].requested_role.is_none());
        assert!(loaded[0].auth_required.is_none());
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
        // past `MODERN_UNIX_FLOOR` (2020-01-01T00:00:00Z) is
        // clearly real wall-clock time and not a clock-skew
        // fallback returning 0.
        assert!(now_unix_seconds() > MODERN_UNIX_FLOOR);
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

    // --- AGC type persistence tests (#356) ---

    /// Fresh config with no AGC key and no legacy key returns
    /// the default (Software). Matches the "fresh-install user
    /// gets the well-behaved path" contract from the issue.
    #[test]
    fn load_agc_type_defaults_to_software_on_fresh_config() {
        let config = make_config();
        assert_eq!(load_agc_type(&config), AgcType::Software);
        assert_eq!(AgcType::DEFAULT, AgcType::Software);
    }

    /// Round-trip: save each variant, load returns it. Pins the
    /// serde representation against future rename / enum
    /// reordering.
    #[test]
    fn agc_type_save_load_round_trips_all_variants() {
        for variant in [AgcType::Off, AgcType::Hardware, AgcType::Software] {
            let config = make_config();
            save_agc_type(&config, variant);
            assert_eq!(
                load_agc_type(&config),
                variant,
                "round-trip failed for {variant:?}"
            );
        }
    }

    /// Legacy migration: a pre-#354 config has only the boolean
    /// `rtl_sdr_agc_enabled` key. Loader maps `true → Hardware`,
    /// `false → Off`. Preserves the user's upgrade path without
    /// a one-shot migration job.
    #[test]
    fn load_agc_type_migrates_legacy_boolean_on() {
        let config = make_config();
        config.write(|v| {
            v[KEY_LEGACY_AGC_ENABLED] = serde_json::json!(true);
        });
        assert_eq!(load_agc_type(&config), AgcType::Hardware);
    }

    #[test]
    fn load_agc_type_migrates_legacy_boolean_off() {
        let config = make_config();
        config.write(|v| {
            v[KEY_LEGACY_AGC_ENABLED] = serde_json::json!(false);
        });
        assert_eq!(load_agc_type(&config), AgcType::Off);
    }

    /// When both the new key and the legacy key are present,
    /// the new key wins. Guards against a mis-migration that
    /// could silently revert a user's post-upgrade selection.
    #[test]
    fn load_agc_type_new_key_wins_over_legacy_key() {
        let config = make_config();
        config.write(|v| {
            v[KEY_LEGACY_AGC_ENABLED] = serde_json::json!(true);
        });
        save_agc_type(&config, AgcType::Software);
        assert_eq!(load_agc_type(&config), AgcType::Software);
    }

    /// Corrupt `agc_type` value (e.g. renamed-since enum variant
    /// that's no longer recognized) falls back to the legacy
    /// key then the default, without panicking.
    #[test]
    fn load_agc_type_tolerates_corrupt_new_key() {
        let config = make_config();
        config.write(|v| {
            v[KEY_AGC_TYPE] = serde_json::json!("this_is_not_a_valid_variant");
            v[KEY_LEGACY_AGC_ENABLED] = serde_json::json!(true);
        });
        // Corrupt new key skipped → fall through to legacy →
        // Hardware.
        assert_eq!(load_agc_type(&config), AgcType::Hardware);
    }

    /// `agc_type_from_selected` round-trips each legal index
    /// and returns `None` on unknown indices so callers can
    /// reject transient GTK teardown values instead of
    /// silently dispatching a fallback as a real user choice.
    #[test]
    fn agc_type_selected_index_helpers_round_trip() {
        for variant in [AgcType::Off, AgcType::Hardware, AgcType::Software] {
            let idx = selected_from_agc_type(variant);
            assert_eq!(agc_type_from_selected(idx), Some(variant));
        }
        // Unknown index → `None`. Notify handler early-returns
        // on this to avoid corrupting persisted config.
        assert_eq!(agc_type_from_selected(99), None);
        // `u32::MAX` is the `gtk4::INVALID_LIST_POSITION`
        // sentinel; make sure we don't panic or coerce.
        assert_eq!(agc_type_from_selected(u32::MAX), None);
    }

    // --- Bias-T persistence tests (#537) ---

    /// Fresh config (no key present) defaults to `false`.
    /// "Safe by default" — a user without a powered antenna
    /// shouldn't get 5 V on the coax on first launch.
    #[test]
    fn load_source_rtl_bias_tee_defaults_to_off() {
        let config = make_config();
        assert!(!load_source_rtl_bias_tee(&config));
    }

    /// Round-trip: write `true`, read back `true`.
    #[test]
    fn save_and_load_source_rtl_bias_tee_round_trip() {
        let config = make_config();
        save_source_rtl_bias_tee(&config, true);
        assert!(load_source_rtl_bias_tee(&config));
        save_source_rtl_bias_tee(&config, false);
        assert!(!load_source_rtl_bias_tee(&config));
    }

    /// Corrupt value (wrong JSON type) falls back to default
    /// rather than panicking. Mirrors the
    /// `load_agc_type_tolerates_corrupt_new_key` resilience
    /// pattern.
    #[test]
    fn load_source_rtl_bias_tee_tolerates_non_bool() {
        let config = make_config();
        config.write(|v| {
            v[KEY_SOURCE_RTL_BIAS_TEE] = serde_json::json!("not a bool");
        });
        assert!(!load_source_rtl_bias_tee(&config));
    }

    /// #551 persistence: gain in dB.
    #[test]
    fn source_rtl_gain_db_round_trip_and_default() {
        let config = make_config();
        assert!((load_source_rtl_gain_db(&config) - 0.0).abs() < f64::EPSILON);
        save_source_rtl_gain_db(&config, 35.5);
        assert!((load_source_rtl_gain_db(&config) - 35.5).abs() < f64::EPSILON);
        config.write(|v| v[KEY_SOURCE_RTL_GAIN_DB] = serde_json::json!("not a number"));
        assert!((load_source_rtl_gain_db(&config) - 0.0).abs() < f64::EPSILON);
    }

    /// #551 persistence: PPM correction.
    #[test]
    fn source_rtl_ppm_round_trip_and_default() {
        let config = make_config();
        assert_eq!(load_source_rtl_ppm(&config), 0);
        save_source_rtl_ppm(&config, -25);
        assert_eq!(load_source_rtl_ppm(&config), -25);
        save_source_rtl_ppm(&config, 50);
        assert_eq!(load_source_rtl_ppm(&config), 50);
        config.write(|v| v[KEY_SOURCE_RTL_PPM] = serde_json::json!("not a number"));
        assert_eq!(load_source_rtl_ppm(&config), 0);
    }

    // ─── #552 persistence round-trips ─────────────────────────

    #[test]
    fn source_device_index_round_trip_and_default() {
        let config = make_config();
        assert_eq!(load_source_device_index(&config), DEVICE_RTLSDR);
        save_source_device_index(&config, DEVICE_NETWORK);
        assert_eq!(load_source_device_index(&config), DEVICE_NETWORK);
        config.write(|v| v[KEY_SOURCE_DEVICE_INDEX] = serde_json::json!("nope"));
        assert_eq!(load_source_device_index(&config), DEVICE_RTLSDR);
    }

    #[test]
    fn source_sample_rate_index_round_trip_and_default() {
        let config = make_config();
        assert_eq!(load_source_sample_rate_index(&config), 0);
        save_source_sample_rate_index(&config, 3);
        assert_eq!(load_source_sample_rate_index(&config), 3);
        config.write(|v| v[KEY_SOURCE_SAMPLE_RATE_INDEX] = serde_json::json!("nope"));
        assert_eq!(load_source_sample_rate_index(&config), 0);
    }

    #[test]
    fn source_decimation_index_round_trip_and_default() {
        let config = make_config();
        assert_eq!(load_source_decimation_index(&config), 0);
        save_source_decimation_index(&config, 2);
        assert_eq!(load_source_decimation_index(&config), 2);
        config.write(|v| v[KEY_SOURCE_DECIMATION_INDEX] = serde_json::json!("nope"));
        assert_eq!(load_source_decimation_index(&config), 0);
    }

    #[test]
    fn source_dc_blocking_round_trip_and_default() {
        let config = make_config();
        assert!(load_source_dc_blocking(&config));
        save_source_dc_blocking(&config, false);
        assert!(!load_source_dc_blocking(&config));
        save_source_dc_blocking(&config, true);
        assert!(load_source_dc_blocking(&config));
        config.write(|v| v[KEY_SOURCE_DC_BLOCKING] = serde_json::json!("nope"));
        assert!(load_source_dc_blocking(&config));
    }

    #[test]
    fn source_iq_correction_round_trip_and_default() {
        let config = make_config();
        assert!(!load_source_iq_correction(&config));
        save_source_iq_correction(&config, true);
        assert!(load_source_iq_correction(&config));
        config.write(|v| v[KEY_SOURCE_IQ_CORRECTION] = serde_json::json!("nope"));
        assert!(!load_source_iq_correction(&config));
    }

    #[test]
    fn source_iq_inversion_round_trip_and_default() {
        let config = make_config();
        assert!(!load_source_iq_inversion(&config));
        save_source_iq_inversion(&config, true);
        assert!(load_source_iq_inversion(&config));
        config.write(|v| v[KEY_SOURCE_IQ_INVERSION] = serde_json::json!("nope"));
        assert!(!load_source_iq_inversion(&config));
    }

    #[test]
    fn source_network_hostname_round_trip_and_default() {
        let config = make_config();
        assert_eq!(load_source_network_hostname(&config), "localhost");
        save_source_network_hostname(&config, "shack-pi.local");
        assert_eq!(load_source_network_hostname(&config), "shack-pi.local");
        config.write(|v| v[KEY_SOURCE_NETWORK_HOSTNAME] = serde_json::json!(42));
        assert_eq!(load_source_network_hostname(&config), "localhost");
    }

    #[test]
    fn source_network_port_round_trip_and_default() {
        let config = make_config();
        assert_eq!(load_source_network_port(&config), 1234);
        save_source_network_port(&config, 8888);
        assert_eq!(load_source_network_port(&config), 8888);
        // out-of-range u16 falls back
        config.write(|v| v[KEY_SOURCE_NETWORK_PORT] = serde_json::json!(70_000));
        assert_eq!(load_source_network_port(&config), 1234);
        config.write(|v| v[KEY_SOURCE_NETWORK_PORT] = serde_json::json!("nope"));
        assert_eq!(load_source_network_port(&config), 1234);
    }

    #[test]
    fn source_network_protocol_index_round_trip_and_default() {
        let config = make_config();
        assert_eq!(
            load_source_network_protocol_index(&config),
            NETWORK_PROTOCOL_TCPCLIENT_IDX
        );
        save_source_network_protocol_index(&config, NETWORK_PROTOCOL_UDP_IDX);
        assert_eq!(
            load_source_network_protocol_index(&config),
            NETWORK_PROTOCOL_UDP_IDX
        );
        config.write(|v| v[KEY_SOURCE_NETWORK_PROTOCOL_INDEX] = serde_json::json!("nope"));
        assert_eq!(
            load_source_network_protocol_index(&config),
            NETWORK_PROTOCOL_TCPCLIENT_IDX
        );
    }

    #[test]
    fn source_file_path_round_trip_and_default() {
        let config = make_config();
        assert_eq!(load_source_file_path(&config), "");
        save_source_file_path(&config, "/tmp/iq.wav");
        assert_eq!(load_source_file_path(&config), "/tmp/iq.wav");
        config.write(|v| v[KEY_SOURCE_FILE_PATH] = serde_json::json!(42));
        assert_eq!(load_source_file_path(&config), "");
    }
}
