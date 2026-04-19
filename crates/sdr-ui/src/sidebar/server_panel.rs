//! Server panel — "Share over network" controls exposing a local
//! RTL-SDR dongle to remote `rtl_tcp` clients.
//!
//! This panel is hidden by default. It becomes visible when a local
//! RTL-SDR dongle is detected AND it's not currently the active
//! source — exposing the same dongle over `rtl_tcp` while also
//! receiving from it locally would cause a USB-device double-open,
//! so the UI gates the panel on the incompatible state.
//!
//! The panel itself only builds widgets; the wire-up (start/stop,
//! stats polling, activity log) lives in `window.rs` alongside the
//! rest of the DSP/UI bridge. Keeping this file widget-only mirrors
//! the pattern in `source_panel.rs` / `audio_panel.rs` / etc.

use libadwaita as adw;
use libadwaita::prelude::*;

/// Default TCP port for `rtl_tcp`. Matches upstream `rtl_tcp.c` and
/// every ecosystem client's default. Changing it means users have to
/// know the custom port on every client — keep as a knob but default
/// to the well-known value.
pub const DEFAULT_SERVER_PORT: f64 = 1234.0;
/// Lowest TCP port we'll accept. 1023 and below are privileged on
/// Unix and require `CAP_NET_BIND_SERVICE` / root — we're not going
/// to run as root, so refuse up front.
pub const MIN_SERVER_PORT: f64 = 1024.0;
/// Highest legal TCP port (16-bit unsigned max).
pub const MAX_SERVER_PORT: f64 = 65_535.0;
/// Spin-row per-click step for the port field.
const SERVER_PORT_STEP: f64 = 1.0;
/// Spin-row page step (`PgUp` / `PgDn`) for the port field.
const SERVER_PORT_PAGE: f64 = 100.0;

/// Bind-address selector index: loopback-only (127.0.0.1). The
/// default — limits exposure to clients running on the same machine
/// until the user opts into broader access.
pub const BIND_LOOPBACK_IDX: u32 = 0;
/// Bind-address selector index: all interfaces (0.0.0.0).
pub const BIND_ALL_INTERFACES_IDX: u32 = 1;

/// Server device-defaults: center frequency default (Hz) applied on
/// start, before the first client connects. Upstream `rtl_tcp.c:389`
/// default. Clients typically tune immediately after connecting, so
/// this only affects the "waiting for client" idle state and any
/// client that doesn't send `SetCenterFreq` before reading data.
const DEFAULT_CENTER_FREQ_HZ: f64 = 100_000_000.0;
/// Minimum tunable frequency (Hz). Real RTL-SDR dongles go lower
/// (~24 MHz native, down to DC in direct-sampling mode), but for
/// defaults-on-start the UI caps at 24 MHz to stay in the dongle's
/// documented range.
const MIN_CENTER_FREQ_HZ: f64 = 24_000_000.0;
/// Maximum tunable frequency (Hz). R820T / R828D top out ~1.7 GHz
/// depending on the tuner; 1.766 GHz is the driver's stated ceiling.
const MAX_CENTER_FREQ_HZ: f64 = 1_766_000_000.0;
/// Frequency spin-row step (1 kHz per click).
const CENTER_FREQ_STEP_HZ: f64 = 1_000.0;
/// Frequency spin-row page step (1 MHz per PgUp/PgDn).
const CENTER_FREQ_PAGE_HZ: f64 = 1_000_000.0;

/// Server device-defaults: sample-rate selector index (2.4 MHz = 7).
/// Same ordering as `source_panel::build_rtlsdr_rows` so keyboard
/// muscle memory matches.
const DEFAULT_SERVER_SAMPLE_RATE_INDEX: u32 = 7;

/// Server device-defaults: gain default (dB). 0.0 dB matches
/// upstream's "auto" gain interpretation when the CLI passes `-g 0`.
/// UI treats 0.0 as auto; any positive value is a manual setting.
const DEFAULT_SERVER_GAIN_DB: f64 = 0.0;
/// Minimum server-gain spin-row value (dB).
const MIN_SERVER_GAIN_DB: f64 = 0.0;
/// Maximum server-gain spin-row value (dB) — widest R820T range.
const MAX_SERVER_GAIN_DB: f64 = 49.6;
/// Server-gain spin-row step (dB).
const SERVER_GAIN_STEP_DB: f64 = 0.1;
/// Server-gain spin-row page step (dB).
const SERVER_GAIN_PAGE_DB: f64 = 1.0;

/// Server device-defaults: PPM correction default. 0 is "no
/// correction" — the user can override if they know their crystal
/// offset.
const DEFAULT_SERVER_PPM: f64 = 0.0;
/// Minimum server PPM correction.
const MIN_SERVER_PPM: f64 = -200.0;
/// Maximum server PPM correction.
const MAX_SERVER_PPM: f64 = 200.0;
/// PPM spin-row step.
const SERVER_PPM_STEP: f64 = 1.0;
/// PPM spin-row page step.
const SERVER_PPM_PAGE: f64 = 10.0;

/// Default server nickname shown until the user edits it. Kept
/// generic — a hostname is substituted at `Server::start()` time in
/// `window.rs`, mirroring the CLI's `sdr-rtl-tcp` default-nickname
/// logic in `sdr-server-rtltcp/src/bin/sdr-rtl-tcp.rs`.
const DEFAULT_NICKNAME: &str = "sdr-rtl-tcp";

// Compile-time invariants for the port and frequency bounds. Moves
// "did I accidentally flip min/max or push the port into privileged
// space" checks from runtime-only test assertions (clippy flags them
// as tautologies on consts) to build-time hard errors.
const _: () = {
    assert!(
        MIN_SERVER_PORT >= 1024.0,
        "server port must be unprivileged"
    );
    assert!(MAX_SERVER_PORT <= 65_535.0, "server port must fit in a u16");
    assert!(MIN_SERVER_PORT <= DEFAULT_SERVER_PORT);
    assert!(DEFAULT_SERVER_PORT <= MAX_SERVER_PORT);
    assert!(MIN_CENTER_FREQ_HZ <= DEFAULT_CENTER_FREQ_HZ);
    assert!(DEFAULT_CENTER_FREQ_HZ <= MAX_CENTER_FREQ_HZ);
    assert!(BIND_LOOPBACK_IDX != BIND_ALL_INTERFACES_IDX);
};

/// Server-panel widget handles — packed into the sidebar as an
/// `AdwPreferencesGroup` and handed to `window.rs` for signal
/// wiring.
///
/// Every row except `widget` / `device_defaults_row` is a leaf
/// control; `window.rs` reads their values at `Server::start()`
/// time and disables them while the server is running so the user
/// can't mutate config out from under a live session.
pub struct ServerPanel {
    /// The `AdwPreferencesGroup` widget to pack into the sidebar.
    /// Hidden by default — `window.rs` toggles visibility based on
    /// USB hotplug + active-source state.
    pub widget: adw::PreferencesGroup,
    /// Master share-over-network switch. On → start Server. Off →
    /// stop Server.
    pub share_row: adw::SwitchRow,
    /// User-editable server nickname. Becomes the mDNS TXT
    /// `nickname` field when advertising is on.
    pub nickname_row: adw::EntryRow,
    /// TCP port the server binds to (1024-65535, default 1234).
    pub port_row: adw::SpinRow,
    /// Bind address selector (Loopback / All interfaces).
    pub bind_row: adw::ComboRow,
    /// Whether to announce the running server over mDNS. Defaults
    /// on; the user can turn it off to run locally without LAN
    /// advertisement.
    pub advertise_row: adw::SwitchRow,
    /// Collapsible group of device-defaults (freq / sample rate /
    /// gain / PPM / bias tee / direct sampling) applied on server
    /// start. Clients override these live via the `rtl_tcp` command
    /// channel — these are just the "before first client" defaults.
    pub device_defaults_row: adw::ExpanderRow,
    /// Center-frequency default applied on server open.
    pub center_freq_row: adw::SpinRow,
    /// Sample-rate default applied on server open.
    pub sample_rate_row: adw::ComboRow,
    /// Tuner-gain default applied on server open. 0.0 = auto.
    pub gain_row: adw::SpinRow,
    /// PPM frequency-correction default applied on server open.
    pub ppm_row: adw::SpinRow,
    /// Bias-tee power-output toggle applied on server open.
    pub bias_tee_row: adw::SwitchRow,
    /// Direct-sampling toggle (Q-branch) applied on server open.
    /// Only useful for HF experimentation; off for normal use.
    pub direct_sampling_row: adw::SwitchRow,
    /// Collapsible "Server status" expander shown only while the
    /// server is running. Children below render the live state
    /// pulled from `ServerStats` every
    /// `STATUS_POLL_INTERVAL`.
    pub status_row: adw::ExpanderRow,
    /// "Client: …" — connected peer socket address or "Waiting for
    /// client" when the accept loop is idle.
    pub status_client_row: adw::ActionRow,
    /// "Uptime: …" — wall-clock time since the current client
    /// connected. Hidden when no client.
    pub status_uptime_row: adw::ActionRow,
    /// "Data rate: …" — rolling Mbps computed from `bytes_sent`
    /// deltas between status polls.
    pub status_data_rate_row: adw::ActionRow,
    /// "Tuned to: …" — reflects the client's most recent
    /// `SetCenterFreq` / `SetSampleRate` / `SetTunerGain` commands.
    pub status_commanded_row: adw::ActionRow,
    /// Stop button packed as a suffix on the expander row. Flips
    /// the master `share_row` switch off, which is the same control
    /// path the user would hit to stop manually.
    pub status_stop_button: gtk4::Button,
    /// Collapsible "Activity log" expander, listing the last
    /// `sdr_server_rtltcp::RECENT_COMMANDS_CAPACITY` commands the
    /// server has received with timestamps. Hidden while the
    /// server isn't running.
    pub activity_log_row: adw::ExpanderRow,
    /// `ListBox` child of `activity_log_row` where individual
    /// activity entries are appended. Held separately from the
    /// expander so the stats poller can rebuild it on updates
    /// without walking the expander's `AdwActionRow` children.
    pub activity_log_list: gtk4::ListBox,
}

/// Subtitle shown on `status_client_row` when the accept loop is
/// idle. Kept as a const so future i18n can swap every occurrence
/// at once and the "no client yet" vs "some degraded state" render
/// can't drift.
pub const STATUS_WAITING_FOR_CLIENT_SUBTITLE: &str = "Waiting for client";
/// Subtitle shown on data-rate / uptime / commanded rows when the
/// accept loop is idle — same no-client state, different row.
pub const STATUS_IDLE_VALUE_SUBTITLE: &str = "—";

/// Subtitle shown on the activity-log expander when no commands
/// have been received yet. Empty-state text that distinguishes
/// "nothing to show" from "the ring buffer cleared after disconnect"
/// (which also renders as empty but is a different journey).
pub const ACTIVITY_LOG_EMPTY_SUBTITLE: &str = "No commands received yet";

/// Max height the activity-log `ScrolledWindow` grows before
/// scrolling kicks in. Small enough to fit inside the sidebar
/// without dominating it; the expander is collapsed by default so
/// users opt in to seeing the log at all.
const ACTIVITY_LOG_MAX_HEIGHT_PX: i32 = 240;

/// Aggregated status rows rendered under the "Server status"
/// expander. Grouped so the builder stays readable and the
/// top-level `build_server_panel` stays inside clippy's
/// `too_many_lines` limit.
#[allow(
    clippy::struct_field_names,
    reason = "all fields are GTK *Row widgets — shared suffix reads clearly at the call sites"
)]
struct StatusRows {
    expander: adw::ExpanderRow,
    client_row: adw::ActionRow,
    uptime_row: adw::ActionRow,
    data_rate_row: adw::ActionRow,
    commanded_row: adw::ActionRow,
    stop_button: gtk4::Button,
}

/// Build the "Activity log" expander plus its scrollable child
/// `ListBox`. The `ListBox` is wrapped in a `ScrolledWindow` with
/// an `ACTIVITY_LOG_MAX_HEIGHT_PX` cap so the expander doesn't grow
/// the sidebar taller than the viewport when the ring fills up.
fn build_activity_log_row() -> (adw::ExpanderRow, gtk4::ListBox) {
    let row = adw::ExpanderRow::builder()
        .title("Activity log")
        .subtitle(ACTIVITY_LOG_EMPTY_SUBTITLE)
        .visible(false)
        .build();
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(ACTIVITY_LOG_MAX_HEIGHT_PX)
        .child(&list)
        .build();
    // Wrap the scroll in an ActionRow so the expander's layout
    // machinery (which expects rows) renders it correctly. Empty
    // title/subtitle pushes the scroll widget into the row body.
    let wrapper = adw::ActionRow::builder().activatable(false).build();
    wrapper.add_prefix(&scroll);
    row.add_row(&wrapper);
    (row, list)
}

fn build_status_rows() -> StatusRows {
    let expander = adw::ExpanderRow::builder()
        .title("Server status")
        .subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE)
        .expanded(true)
        .visible(false)
        .build();
    let client_row = adw::ActionRow::builder()
        .title("Client")
        .subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE)
        .build();
    let uptime_row = adw::ActionRow::builder()
        .title("Uptime")
        .subtitle(STATUS_IDLE_VALUE_SUBTITLE)
        .build();
    let data_rate_row = adw::ActionRow::builder()
        .title("Data rate")
        .subtitle(STATUS_IDLE_VALUE_SUBTITLE)
        .build();
    let commanded_row = adw::ActionRow::builder()
        .title("Tuned to")
        .subtitle(STATUS_IDLE_VALUE_SUBTITLE)
        .build();
    let stop_button = gtk4::Button::with_label("Stop");
    stop_button.add_css_class("destructive-action");
    stop_button.set_valign(gtk4::Align::Center);
    expander.add_suffix(&stop_button);
    expander.add_row(&client_row);
    expander.add_row(&uptime_row);
    expander.add_row(&data_rate_row);
    expander.add_row(&commanded_row);
    StatusRows {
        expander,
        client_row,
        uptime_row,
        data_rate_row,
        commanded_row,
        stop_button,
    }
}

/// Rows applied-on-start that live inside the "Device defaults"
/// expander. Return tuple grouped so the top-level builder stays
/// under clippy's `too_many_lines` limit.
#[allow(
    clippy::struct_field_names,
    reason = "all fields are GTK *Row widgets — the shared suffix matches the rest of sidebar/ and reads clearly at call sites"
)]
struct DeviceDefaultsRows {
    center_freq_row: adw::SpinRow,
    sample_rate_row: adw::ComboRow,
    gain_row: adw::SpinRow,
    ppm_row: adw::SpinRow,
    bias_tee_row: adw::SwitchRow,
    direct_sampling_row: adw::SwitchRow,
}

fn build_device_defaults_rows() -> DeviceDefaultsRows {
    let freq_adj = gtk4::Adjustment::new(
        DEFAULT_CENTER_FREQ_HZ,
        MIN_CENTER_FREQ_HZ,
        MAX_CENTER_FREQ_HZ,
        CENTER_FREQ_STEP_HZ,
        CENTER_FREQ_PAGE_HZ,
        0.0,
    );
    let center_freq_row = adw::SpinRow::builder()
        .title("Center frequency (Hz)")
        .adjustment(&freq_adj)
        .digits(0)
        .build();

    // Sample-rate list mirrors the client-side source panel so a
    // user familiar with one knows the other. 2.4 MHz is the default
    // to stay inside RTL-SDR's stable-without-dropouts range.
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
        .title("Sample rate")
        .model(&sample_rate_model)
        .selected(DEFAULT_SERVER_SAMPLE_RATE_INDEX)
        .build();

    let gain_adj = gtk4::Adjustment::new(
        DEFAULT_SERVER_GAIN_DB,
        MIN_SERVER_GAIN_DB,
        MAX_SERVER_GAIN_DB,
        SERVER_GAIN_STEP_DB,
        SERVER_GAIN_PAGE_DB,
        0.0,
    );
    let gain_row = adw::SpinRow::builder()
        .title("Tuner gain (dB)")
        .subtitle("0 = auto gain")
        .adjustment(&gain_adj)
        .digits(1)
        .build();

    let ppm_adj = gtk4::Adjustment::new(
        DEFAULT_SERVER_PPM,
        MIN_SERVER_PPM,
        MAX_SERVER_PPM,
        SERVER_PPM_STEP,
        SERVER_PPM_PAGE,
        0.0,
    );
    let ppm_row = adw::SpinRow::builder()
        .title("Frequency correction (PPM)")
        .adjustment(&ppm_adj)
        .digits(0)
        .build();

    let bias_tee_row = adw::SwitchRow::builder()
        .title("Bias tee")
        .subtitle("Power remote LNA via antenna connector")
        .build();

    let direct_sampling_row = adw::SwitchRow::builder()
        .title("Direct sampling (Q branch)")
        .subtitle("HF mode — bypasses the tuner")
        .build();

    DeviceDefaultsRows {
        center_freq_row,
        sample_rate_row,
        gain_row,
        ppm_row,
        bias_tee_row,
        direct_sampling_row,
    }
}

/// Build the server-panel widgets. The panel is hidden by default;
/// `window.rs` toggles `widget.set_visible(true)` once a local dongle
/// is detected and the active source is not RTL-SDR.
pub fn build_server_panel() -> ServerPanel {
    let widget = adw::PreferencesGroup::builder()
        .title("Share over network")
        .description("Expose this machine's RTL-SDR dongle to remote rtl_tcp clients")
        .visible(false)
        .build();

    let share_row = adw::SwitchRow::builder()
        .title("Share over network")
        .subtitle("Start the rtl_tcp server and advertise it on the LAN")
        .build();

    let nickname_row = adw::EntryRow::builder()
        .title("Server nickname")
        .text(DEFAULT_NICKNAME)
        .build();

    let port_adj = gtk4::Adjustment::new(
        DEFAULT_SERVER_PORT,
        MIN_SERVER_PORT,
        MAX_SERVER_PORT,
        SERVER_PORT_STEP,
        SERVER_PORT_PAGE,
        0.0,
    );
    let port_row = adw::SpinRow::builder()
        .title("Port")
        .adjustment(&port_adj)
        .digits(0)
        .build();

    // Order is load-bearing — matches `BIND_LOOPBACK_IDX` /
    // `BIND_ALL_INTERFACES_IDX`. A third "specific interface" option
    // is deferred to #323 because it needs an interface enumerator
    // we haven't wired up yet.
    let bind_model = gtk4::StringList::new(&["Loopback only", "All interfaces"]);
    let bind_row = adw::ComboRow::builder()
        .title("Bind address")
        .model(&bind_model)
        .selected(BIND_LOOPBACK_IDX)
        .build();

    let advertise_row = adw::SwitchRow::builder()
        .title("Announce via mDNS")
        .subtitle("Let LAN clients discover this server by name")
        .active(true)
        .build();

    let device_defaults_row = adw::ExpanderRow::builder()
        .title("Device defaults")
        .subtitle("Applied when the server opens the dongle — clients override live")
        .build();

    let DeviceDefaultsRows {
        center_freq_row,
        sample_rate_row,
        gain_row,
        ppm_row,
        bias_tee_row,
        direct_sampling_row,
    } = build_device_defaults_rows();

    device_defaults_row.add_row(&center_freq_row);
    device_defaults_row.add_row(&sample_rate_row);
    device_defaults_row.add_row(&gain_row);
    device_defaults_row.add_row(&ppm_row);
    device_defaults_row.add_row(&bias_tee_row);
    device_defaults_row.add_row(&direct_sampling_row);

    let StatusRows {
        expander: status_row,
        client_row: status_client_row,
        uptime_row: status_uptime_row,
        data_rate_row: status_data_rate_row,
        commanded_row: status_commanded_row,
        stop_button: status_stop_button,
    } = build_status_rows();

    let (activity_log_row, activity_log_list) = build_activity_log_row();

    widget.add(&share_row);
    widget.add(&nickname_row);
    widget.add(&port_row);
    widget.add(&bind_row);
    widget.add(&advertise_row);
    widget.add(&device_defaults_row);
    widget.add(&status_row);
    widget.add(&activity_log_row);

    ServerPanel {
        widget,
        share_row,
        nickname_row,
        port_row,
        bind_row,
        advertise_row,
        device_defaults_row,
        center_freq_row,
        sample_rate_row,
        gain_row,
        ppm_row,
        bias_tee_row,
        direct_sampling_row,
        status_row,
        status_client_row,
        status_uptime_row,
        status_data_rate_row,
        status_commanded_row,
        status_stop_button,
        activity_log_row,
        activity_log_list,
    }
}
