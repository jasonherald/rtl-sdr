//! Widget construction for the Server (Share) panel (issue #819):
//! the status / activity-log / connected-clients expander builders,
//! the device-defaults rows, and [`build_server_panel`] itself.
//! Split out of `server_panel.rs` per the file-size pass.

use libadwaita as adw;
use libadwaita::prelude::*;

use super::{
    ACTIVITY_LOG_EMPTY_SUBTITLE, ACTIVITY_LOG_MAX_HEIGHT_PX, AUTH_KEY_MASKED_PLACEHOLDER,
    BIND_LOOPBACK_IDX, CENTER_FREQ_PAGE_HZ, CENTER_FREQ_STEP_HZ, CLIENTS_LIST_EMPTY_SUBTITLE,
    CLIENTS_LIST_MAX_HEIGHT_PX, COMPRESSION_OFF_IDX, DEFAULT_CENTER_FREQ_HZ, DEFAULT_NICKNAME,
    DEFAULT_SERVER_GAIN_DB, DEFAULT_SERVER_PORT, DEFAULT_SERVER_PPM,
    DEFAULT_SERVER_SAMPLE_RATE_INDEX, LISTENER_CAP_PAGE, LISTENER_CAP_STEP, MAX_CENTER_FREQ_HZ,
    MAX_LISTENER_CAP, MAX_SERVER_GAIN_DB, MAX_SERVER_PORT, MAX_SERVER_PPM, MIN_CENTER_FREQ_HZ,
    MIN_LISTENER_CAP, MIN_SERVER_GAIN_DB, MIN_SERVER_PORT, MIN_SERVER_PPM, SERVER_GAIN_PAGE_DB,
    SERVER_GAIN_STEP_DB, SERVER_PORT_PAGE, SERVER_PORT_STEP, SERVER_PPM_PAGE, SERVER_PPM_STEP,
    STATUS_IDLE_VALUE_SUBTITLE, STATUS_WAITING_FOR_CLIENT_SUBTITLE, ServerPanel,
};

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

/// Build the "Connected clients" expander + its inner `ListBox`.
/// Mirrors `build_activity_log_row`'s scroll-wrapping pattern
/// so a server with a dozen listeners doesn't balloon the
/// sidebar height. Per issue #395.
fn build_clients_row() -> (adw::ExpanderRow, gtk4::ListBox) {
    let row = adw::ExpanderRow::builder()
        .title("Connected clients")
        .subtitle(CLIENTS_LIST_EMPTY_SUBTITLE)
        .expanded(true)
        .visible(false)
        .build();
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(CLIENTS_LIST_MAX_HEIGHT_PX)
        .child(&list)
        .build();
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

/// Center-frequency + sample-rate halves of the device-defaults
/// expander. Split out of [`build_device_defaults_rows`] per the
/// 50-NLOC gate (#819, PR #880 Codacy precedent).
fn build_defaults_freq_rate_rows() -> (adw::SpinRow, adw::ComboRow) {
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

    (center_freq_row, sample_rate_row)
}

fn build_device_defaults_rows() -> DeviceDefaultsRows {
    let (center_freq_row, sample_rate_row) = build_defaults_freq_rate_rows();

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

/// Always-visible share-configuration rows (switch, nickname, port,
/// bind, advertise, compression). Grouped per the
/// [`DeviceDefaultsRows`] precedent so [`build_server_panel`] stays
/// inside the 50-NLOC gate (#819, PR #880 Codacy precedent).
#[allow(
    clippy::struct_field_names,
    reason = "all fields are GTK *Row widgets — the shared suffix matches the rest of sidebar/ and reads clearly at call sites"
)]
struct ShareConfigRows {
    share_row: adw::SwitchRow,
    nickname_row: adw::EntryRow,
    port_row: adw::SpinRow,
    bind_row: adw::ComboRow,
    advertise_row: adw::SwitchRow,
    compression_row: adw::ComboRow,
}

/// The #394/#395 auth-control widgets: master switch, key-display
/// row, and its reveal / copy / regenerate suffix buttons. Grouped
/// per the [`DeviceDefaultsRows`] precedent (50-NLOC gate, #819).
#[allow(
    clippy::struct_field_names,
    reason = "the shared auth_ prefix mirrors the ServerPanel field names these map onto"
)]
struct AuthRows {
    auth_require_row: adw::SwitchRow,
    auth_key_row: adw::ActionRow,
    auth_key_reveal_button: gtk4::Button,
    auth_key_copy_button: gtk4::Button,
    auth_key_regenerate_button: gtk4::Button,
}

fn build_share_config_rows() -> ShareConfigRows {
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

    // Compression model — order matches COMPRESSION_OFF_IDX / _LZ4_IDX.
    // Default is Off to stay wire-compatible with every existing
    // rtl_tcp client on the LAN; opting in is a deliberate click,
    // and even then vanilla clients fall through to uncompressed
    // via the mutual-codec intersection on the server side.
    let compression_model = gtk4::StringList::new(&["Off", "LZ4 (if client supports)"]);
    let compression_row = adw::ComboRow::builder()
        .title("Compression")
        .subtitle("Negotiated per client — legacy clients always get uncompressed")
        .model(&compression_model)
        .selected(COMPRESSION_OFF_IDX)
        .build();

    ShareConfigRows {
        share_row,
        nickname_row,
        port_row,
        bind_row,
        advertise_row,
        compression_row,
    }
}

/// Listener-cap spin row (#395). Split out per the 50-NLOC gate (#819).
fn build_listener_cap_row() -> adw::SpinRow {
    // Listener cap — per #395. Default pulled from the backend's
    // `DEFAULT_LISTENER_CAP` so a UI-backend drift would surface as
    // a test / build failure rather than a quiet divergence. The
    // `usize` → `f64` cast is lossless on every realistic value
    // (cap is always < 32, and f64 is exact for integers up to
    // `2^53`), but clippy's `cast_precision_loss` lint fires on
    // any `usize as f64` conversion regardless — allow inline
    // with a reason rather than adding a workspace-wide exception.
    #[allow(
        clippy::cast_precision_loss,
        reason = "listener cap is bounded << 2^53, f64 represents it exactly"
    )]
    let default_cap = sdr_server_rtltcp::DEFAULT_LISTENER_CAP as f64;
    let listener_cap_adj = gtk4::Adjustment::new(
        default_cap,
        MIN_LISTENER_CAP,
        MAX_LISTENER_CAP,
        LISTENER_CAP_STEP,
        LISTENER_CAP_PAGE,
        0.0,
    );
    adw::SpinRow::builder()
        .title("Listener cap")
        .subtitle(
            "Max simultaneous Listen clients — 0 disables listeners, change applies on next client",
        )
        .adjustment(&listener_cap_adj)
        .numeric(true)
        .snap_to_ticks(true)
        .build()
}

fn build_auth_rows() -> AuthRows {
    // Auth-key controls (#394/#395). Three widgets: master
    // "Require key" switch, a key-display row that only shows
    // when auth is on, and three suffix buttons for
    // reveal / copy / regenerate. State (current key bytes,
    // currently-revealed flag) lives in `window.rs` where the
    // running `Server` + keyring store are accessible.
    let auth_require_row = adw::SwitchRow::builder()
        .title("Require key")
        .subtitle("Clients must present a pre-shared key to connect — LAN-grade only, not WAN-safe")
        .active(false)
        .build();

    // Auth-key display row — hidden until `auth_require_row` is
    // on. `subtitle_selectable(true)` lets users triple-click the
    // revealed key to copy it without using the Copy button.
    let auth_key_row = adw::ActionRow::builder()
        .title("Key")
        .subtitle(AUTH_KEY_MASKED_PLACEHOLDER)
        .subtitle_selectable(true)
        .visible(false)
        .build();

    // Reveal-toggle button. Icon starts as `view-reveal-symbolic`
    // (masked → click to reveal); window.rs flips it to
    // `view-conceal-symbolic` when the subtitle shows the real
    // key. `.flat()` keeps it visually aligned with the row.
    let auth_key_reveal_button = gtk4::Button::builder()
        .icon_name("view-reveal-symbolic")
        .tooltip_text("Reveal key")
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    // Icon-only buttons need an explicit accessible label —
    // screen readers read the label, not the tooltip. The reveal
    // button's label flips in `window.rs` alongside icon_name when
    // toggled. Matches the established pattern in this crate
    // (source_panel, navigation_panel, radio_panel). Per
    // `CodeRabbit` round 1 on PR #406.
    auth_key_reveal_button.update_property(&[gtk4::accessible::Property::Label("Reveal key")]);
    let auth_key_copy_button = gtk4::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy key to clipboard")
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    auth_key_copy_button
        .update_property(&[gtk4::accessible::Property::Label("Copy key to clipboard")]);
    let auth_key_regenerate_button = gtk4::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Regenerate key — old key stops working for future reconnects")
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    auth_key_regenerate_button
        .update_property(&[gtk4::accessible::Property::Label("Regenerate key")]);
    auth_key_row.add_suffix(&auth_key_reveal_button);
    auth_key_row.add_suffix(&auth_key_copy_button);
    auth_key_row.add_suffix(&auth_key_regenerate_button);

    AuthRows {
        auth_require_row,
        auth_key_row,
        auth_key_reveal_button,
        auth_key_copy_button,
        auth_key_regenerate_button,
    }
}

/// Build the server-panel widgets. Always visible — the Share
/// activity icon in the left activity bar is the user's opt-in
/// gesture, so the panel no longer hides itself based on hotplug
/// state. When no dongle is plugged in the Start switch errors
/// gracefully; the panel's presence under its dedicated icon is
/// the right UX regardless of current dongle availability.
pub fn build_server_panel() -> ServerPanel {
    let widget = adw::PreferencesGroup::builder()
        .title("Share over network")
        .description("Expose this machine's RTL-SDR dongle to remote rtl_tcp clients")
        .build();

    let share = build_share_config_rows();
    let auth = build_auth_rows();
    let defaults = build_device_defaults_rows();
    let device_defaults_row = build_defaults_expander(&defaults);
    let status = build_status_rows();
    let (activity_log_row, activity_log_list) = build_activity_log_row();
    let (clients_row, clients_list) = build_clients_row();

    // Field-by-field moves from the grouped builders straight into
    // the flat `ServerPanel` handle `window.rs` consumes — no
    // intermediate destructuring, so the function stays inside the
    // 50-NLOC gate while the panel's public shape is unchanged.
    let panel = ServerPanel {
        widget,
        share_row: share.share_row,
        nickname_row: share.nickname_row,
        port_row: share.port_row,
        bind_row: share.bind_row,
        advertise_row: share.advertise_row,
        compression_row: share.compression_row,
        listener_cap_row: build_listener_cap_row(),
        auth_require_row: auth.auth_require_row,
        auth_key_row: auth.auth_key_row,
        auth_key_reveal_button: auth.auth_key_reveal_button,
        auth_key_copy_button: auth.auth_key_copy_button,
        auth_key_regenerate_button: auth.auth_key_regenerate_button,
        device_defaults_row,
        center_freq_row: defaults.center_freq_row,
        sample_rate_row: defaults.sample_rate_row,
        gain_row: defaults.gain_row,
        ppm_row: defaults.ppm_row,
        bias_tee_row: defaults.bias_tee_row,
        direct_sampling_row: defaults.direct_sampling_row,
        status_row: status.expander,
        status_client_row: status.client_row,
        status_uptime_row: status.uptime_row,
        status_data_rate_row: status.data_rate_row,
        status_commanded_row: status.commanded_row,
        status_stop_button: status.stop_button,
        activity_log_row,
        activity_log_list,
        clients_row,
        clients_list,
        bandwidth_advisory_row: build_bandwidth_advisory_row(),
    };
    attach_server_rows(&panel);
    panel
}

/// Pack every top-level row into the panel's `PreferencesGroup`, in
/// display order. Runs after the [`ServerPanel`] literal is built so
/// the add-sequence reads off the panel handle instead of fourteen
/// positional parameters. Split out of [`build_server_panel`] per
/// the 50-NLOC gate (#819, PR #880 Codacy precedent). Order is the
/// user-visible row order.
fn attach_server_rows(panel: &ServerPanel) {
    let w = &panel.widget;
    w.add(&panel.share_row);
    w.add(&panel.nickname_row);
    w.add(&panel.port_row);
    w.add(&panel.bind_row);
    w.add(&panel.advertise_row);
    w.add(&panel.compression_row);
    w.add(&panel.listener_cap_row);
    w.add(&panel.auth_require_row);
    w.add(&panel.auth_key_row);
    w.add(&panel.device_defaults_row);
    w.add(&panel.status_row);
    w.add(&panel.clients_row);
    w.add(&panel.activity_log_row);
    w.add(&panel.bandwidth_advisory_row);
}

/// "Device defaults" expander with its six child rows attached.
/// Split out of [`build_server_panel`] per the 50-NLOC gate (#819).
fn build_defaults_expander(rows: &DeviceDefaultsRows) -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder()
        .title("Device defaults")
        .subtitle("Applied when the server opens the dongle — clients override live")
        .build();
    expander.add_row(&rows.center_freq_row);
    expander.add_row(&rows.sample_rate_row);
    expander.add_row(&rows.gain_row);
    expander.add_row(&rows.ppm_row);
    expander.add_row(&rows.bias_tee_row);
    expander.add_row(&rows.direct_sampling_row);
    expander
}

/// Bandwidth advisory — hidden initially. Visibility is toggled
/// on sample-rate changes via the wiring in window.rs, mirroring
/// the source-panel path. Copy is intentionally identical to the
/// source-panel version (shared consts) so users see the same
/// warning wording no matter which side they're configuring.
/// Split out of [`build_server_panel`] per the 50-NLOC gate (#819).
fn build_bandwidth_advisory_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(crate::sidebar::source_panel::HIGH_BANDWIDTH_ADVISORY_TITLE)
        .subtitle(crate::sidebar::source_panel::HIGH_BANDWIDTH_ADVISORY_SUBTITLE)
        .visible(false)
        .build();
    row.add_prefix(&gtk4::Image::from_icon_name("dialog-information-symbolic"));
    row
}
