//! Widget construction for the Source panel (issue #819): the
//! per-source-type row builders, the device-visibility policy and
//! its change-notify wiring, and [`build_source_panel`] itself.
//! Split out of `source_panel.rs` per the file-size pass.

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

mod visibility;
use visibility::seed_and_wire_visibility;

use super::{
    AIRSPY_FIRST_AVAILABLE_LABEL, AgcType, CONVERTER_OFFSET_DEFAULT_MHZ, CONVERTER_OFFSET_MAX_MHZ,
    CONVERTER_OFFSET_MIN_MHZ, CONVERTER_OFFSET_PAGE_MHZ, CONVERTER_OFFSET_STEP_MHZ,
    DEFAULT_GAIN_DB, DEFAULT_PORT, DEFAULT_PPM, DEFAULT_SAMPLE_RATE_INDEX,
    DIRECT_SAMPLING_DISABLED_IDX, GAIN_PAGE_DB, GAIN_STEP_DB, HIGH_BANDWIDTH_ADVISORY_SUBTITLE,
    HIGH_BANDWIDTH_ADVISORY_TITLE, MAX_GAIN_DB, MAX_PORT, MAX_PPM, MIN_GAIN_DB, MIN_PORT, MIN_PPM,
    PORT_PAGE, PORT_STEP, PPM_PAGE, PPM_STEP, RTL_SAMPLE_RATE_LABELS, RTL_TCP_ROLE_CONTROL_IDX,
    RTL_TCP_STATUS_DISCONNECTED_SUBTITLE, SourcePanel, probe_rtlsdr_device_label,
    selected_from_agc_type,
};

/// RF-frontend rows shared by the USB tuner sources: bias-T,
/// upconverter offset, direct sampling, offset tuning. Split out of
/// `build_rtlsdr_rows` per the 50-NLOC gate.
fn build_rtl_frontend_rows() -> (adw::SwitchRow, adw::SpinRow, adw::ComboRow, adw::SwitchRow) {
    // Bias tee — powers an inline LNA over the coax. Off by
    // default so users without powered antennas don't drive
    // unexpected current into a passive antenna's centre
    // conductor. Per issue #537.
    let bias_tee_row = adw::SwitchRow::builder()
        .title("Bias-T")
        .subtitle("Power an inline LNA over the antenna coax")
        .active(false)
        .build();

    // Upconverter offset — hardware tunes display + offset. 120 MHz
    // for a SpyVerter, 125 MHz for a Ham-It-Up, 0 for none. Range
    // covers down-converters too (negative offsets). Per #848
    // phase 4.
    let converter_offset_adj = gtk4::Adjustment::new(
        CONVERTER_OFFSET_DEFAULT_MHZ,
        CONVERTER_OFFSET_MIN_MHZ,
        CONVERTER_OFFSET_MAX_MHZ,
        CONVERTER_OFFSET_STEP_MHZ,
        CONVERTER_OFFSET_PAGE_MHZ,
        0.0,
    );
    let converter_offset_row = adw::SpinRow::builder()
        .title("Upconverter Offset")
        .subtitle("MHz added to the displayed frequency at the tuner (SpyVerter: 120)")
        .adjustment(&converter_offset_adj)
        .digits(3)
        .build();

    // Direct sampling — Disabled / I branch / Q branch. Order is
    // load-bearing: the combo's selected index is cast straight
    // to the `rtlsdr_set_direct_sampling` `mode` argument
    // (0/1/2). Disabled is the default — only RTL-SDR Blog v3+
    // dongles benefit, and only when the user is tuning HF.
    // Per issue #538.
    let direct_sampling_model = gtk4::StringList::new(&["Disabled", "I branch", "Q branch"]);
    let direct_sampling_row = adw::ComboRow::builder()
        .title("Direct Sampling")
        .subtitle("Bypass the tuner for HF reception (RTL-SDR Blog v3+)")
        .model(&direct_sampling_model)
        .selected(DIRECT_SAMPLING_DISABLED_IDX)
        .build();

    // Offset tuning — pushes the LO away from the requested
    // centre frequency to dodge the DC spike that lives at the
    // LO position. Most relevant on E4000 tuners; support
    // varies by tuner and driver. R820T/R828D reject the
    // request with `InvalidParameter` (surfaced as a
    // `TuneFailed` toast by the wiring in `window.rs`). Off by
    // default to keep behavior predictable across hardware. Per
    // issue #539.
    let offset_tuning_row = adw::SwitchRow::builder()
        .title("Offset Tuning")
        .subtitle("Shift LO off the tuned freq to avoid the DC spike")
        .active(false)
        .build();

    (
        bias_tee_row,
        converter_offset_row,
        direct_sampling_row,
        offset_tuning_row,
    )
}

/// Tuner gain-chain rows shared by the USB tuner sources: manual
/// gain, AGC selector, PPM correction. Split out of
/// `build_rtlsdr_rows` per the 50-NLOC gate.
fn build_tuner_gain_rows() -> (adw::SpinRow, adw::ComboRow, adw::SpinRow) {
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

    (gain_row, agc_row, ppm_row)
}

/// Build RTL-SDR-specific rows: sample rate, gain, AGC, PPM
/// correction, bias tee, upconverter offset, direct sampling,
/// offset tuning.
fn build_rtlsdr_rows() -> (
    adw::ComboRow,
    adw::SpinRow,
    adw::ComboRow,
    adw::SpinRow,
    adw::SwitchRow,
    adw::SpinRow,
    adw::ComboRow,
    adw::SwitchRow,
) {
    let sample_rate_model = gtk4::StringList::new(RTL_SAMPLE_RATE_LABELS);
    let sample_rate_row = adw::ComboRow::builder()
        .title("Sample Rate")
        .model(&sample_rate_model)
        .selected(DEFAULT_SAMPLE_RATE_INDEX)
        .build();

    let (gain_row, agc_row, ppm_row) = build_tuner_gain_rows();

    let (bias_tee_row, converter_offset_row, direct_sampling_row, offset_tuning_row) =
        build_rtl_frontend_rows();

    (
        sample_rate_row,
        gain_row,
        agc_row,
        ppm_row,
        bias_tee_row,
        converter_offset_row,
        direct_sampling_row,
        offset_tuning_row,
    )
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

/// Device-type selector combo + its backing model. Split out of
/// [`build_source_panel`] per the 50-NLOC gate (#819, PR #880
/// Codacy precedent).
fn build_device_selector() -> (gtk4::StringList, adw::ComboRow) {
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
        "Airspy R2 / Mini",
    ]);
    let device_row = adw::ComboRow::builder()
        .title("Device")
        .model(&device_model)
        .build();
    (device_model, device_row)
}

/// USB-tuner row bundle (RTL-SDR + Airspy). Grouped per the
/// `DeviceDefaultsRows` precedent in `server_panel` so
/// [`build_source_panel`] stays inside the 50-NLOC gate (#819).
struct UsbTunerRows {
    sample_rate: adw::ComboRow,
    gain: adw::SpinRow,
    agc: adw::ComboRow,
    ppm: adw::SpinRow,
    bias_tee: adw::SwitchRow,
    converter_offset: adw::SpinRow,
    airspy_device: adw::ComboRow,
    direct_sampling: adw::ComboRow,
    offset_tuning: adw::SwitchRow,
}

fn build_usb_tuner_rows() -> UsbTunerRows {
    let (sample_rate, gain, agc, ppm, bias_tee, converter_offset, direct_sampling, offset_tuning) =
        build_rtlsdr_rows();
    // Airspy unit selector. Starts with only the "first available"
    // entry; the wiring swaps in enumerated serials when the
    // controller answers `RefreshAirspyDevices`. Per #848 phase 5.
    let airspy_device_model = gtk4::StringList::new(&[AIRSPY_FIRST_AVAILABLE_LABEL]);
    let airspy_device = adw::ComboRow::builder()
        .title("Airspy Unit")
        .subtitle("Which device to open when multiple are connected")
        .model(&airspy_device_model)
        .build();
    UsbTunerRows {
        sample_rate,
        gain,
        agc,
        ppm,
        bias_tee,
        converter_offset,
        airspy_device,
        direct_sampling,
        offset_tuning,
    }
}

/// Network / file / IQ-frontend / recording rows shared across
/// source types. Grouped per the 50-NLOC gate (#819).
struct IoRows {
    hostname: adw::EntryRow,
    port: adw::SpinRow,
    protocol: adw::ComboRow,
    file_path: adw::EntryRow,
    dc_blocking: adw::SwitchRow,
    iq_correction: adw::SwitchRow,
    iq_inversion: adw::SwitchRow,
    decimation: adw::ComboRow,
    record_iq: adw::SwitchRow,
}

fn build_io_rows() -> IoRows {
    let (hostname, port, protocol) = build_network_rows();
    let file_path = adw::EntryRow::builder()
        .title("File Path")
        .text("")
        .visible(false)
        .build();
    let (dc_blocking, iq_correction, iq_inversion, decimation) = build_common_rows();
    let record_iq = adw::SwitchRow::builder()
        .title("Record IQ")
        .subtitle("Raw IQ samples to WAV")
        .build();
    IoRows {
        hostname,
        port,
        protocol,
        file_path,
        dc_blocking,
        iq_correction,
        iq_inversion,
        decimation,
        record_iq,
    }
}

/// RTL-TCP client rows (#396): the discovered-servers expander with
/// its favorites entry point, the connection status row + buttons,
/// the role picker, and the server-key entry. Grouped per the
/// 50-NLOC gate (#819).
struct RtlTcpClientRows {
    discovered: adw::ExpanderRow,
    manage_favorites_button: gtk4::Button,
    status: adw::ActionRow,
    disconnect_button: gtk4::Button,
    retry_button: gtk4::Button,
    role: adw::ComboRow,
    auth_key: adw::PasswordEntryRow,
}

fn build_rtl_tcp_client_rows() -> RtlTcpClientRows {
    // RTL-TCP-specific rows. Built always, shown only when the RTL-TCP
    // source type is selected (see connect_device_visibility + the
    // initial-visibility block below).
    let discovered = adw::ExpanderRow::builder()
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
    discovered.add_suffix(&manage_favorites_button);

    // Connection-state status row — subtitle updated by the DSP
    // bridge via `DspToUi::RtlTcpConnectionState`. Suffix buttons
    // let the user tear down or force-retry the connection without
    // leaving the RTL-TCP source type.
    let status = adw::ActionRow::builder()
        .title("Connection")
        .subtitle(RTL_TCP_STATUS_DISCONNECTED_SUBTITLE)
        .visible(false)
        .build();
    let disconnect_button = gtk4::Button::with_label("Disconnect");
    disconnect_button.set_valign(gtk4::Align::Center);
    disconnect_button.set_sensitive(false);
    let retry_button = gtk4::Button::with_label("Retry now");
    retry_button.set_valign(gtk4::Align::Center);
    retry_button.add_css_class("suggested-action");
    retry_button.set_sensitive(false);
    status.add_suffix(&disconnect_button);
    status.add_suffix(&retry_button);

    // Connection-role picker (#396). AdwComboRow with two
    // entries: "Control" (index 0) and "Listen" (index 1).
    // Default Control matches the pre-#392 single-client flow
    // every legacy rtl_tcp client / server assumes.
    let role_model = gtk4::StringList::new(&["Control", "Listen"]);
    let role = adw::ComboRow::builder()
        .title("Connection role")
        .subtitle("Control drives tuning; Listen receives IQ read-only")
        .model(&role_model)
        .selected(RTL_TCP_ROLE_CONTROL_IDX)
        .visible(false)
        .build();

    // Server key entry (#394 + #396). Password-purpose entry
    // row — masked by default, revealable via AdwPasswordEntryRow's
    // built-in "peek" button. Kept separate from the main hostname
    // / port block so the user sees it only when a key is
    // actually needed (server advertises auth_required=true OR
    // there's a saved key for the active host:port).
    let auth_key = adw::PasswordEntryRow::builder()
        .title("Server key")
        .visible(false)
        .build();

    RtlTcpClientRows {
        discovered,
        manage_favorites_button,
        status,
        disconnect_button,
        retry_button,
        role,
        auth_key,
    }
}

/// Bandwidth advisory row — hidden by default. Visibility is
/// toggled by the sample-rate and device-type notify handlers
/// in window.rs. Title + subtitle copy come from shared consts
/// so the source and server panels render identical text.
/// Split out per the 50-NLOC gate (#819).
fn build_source_bandwidth_advisory_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(HIGH_BANDWIDTH_ADVISORY_TITLE)
        .subtitle(HIGH_BANDWIDTH_ADVISORY_SUBTITLE)
        .visible(false)
        .build();
    row.add_prefix(&gtk4::Image::from_icon_name("dialog-information-symbolic"));
    row
}

/// Pack every row into the panel's `PreferencesGroup` in display
/// order. Runs after the [`SourcePanel`] literal is built so the
/// add-sequence reads off the panel handle instead of two dozen
/// positional parameters. Split out per the 50-NLOC gate (#819).
fn attach_source_rows(panel: &SourcePanel) {
    let g = &panel.widget;
    g.add(&panel.device_row);
    g.add(&panel.sample_rate_row);
    g.add(&panel.gain_row);
    g.add(&panel.agc_row);
    g.add(&panel.ppm_row);
    g.add(&panel.bias_tee_row);
    g.add(&panel.converter_offset_row);
    g.add(&panel.airspy_device_row);
    g.add(&panel.direct_sampling_row);
    g.add(&panel.offset_tuning_row);
    g.add(&panel.hostname_row);
    g.add(&panel.port_row);
    g.add(&panel.protocol_row);
    g.add(&panel.file_path_row);
    g.add(&panel.dc_blocking_row);
    g.add(&panel.iq_correction_row);
    g.add(&panel.iq_inversion_row);
    g.add(&panel.decimation_row);
    g.add(&panel.record_iq_row);
    g.add(&panel.rtl_tcp_discovered_row);
    g.add(&panel.rtl_tcp_status_row);
    g.add(&panel.rtl_tcp_role_row);
    g.add(&panel.rtl_tcp_auth_key_row);
    g.add(&panel.bandwidth_advisory_row);
}

/// Build the source device configuration panel.
pub fn build_source_panel() -> SourcePanel {
    let group = adw::PreferencesGroup::builder()
        .title("Source")
        .description("Device and input configuration")
        .build();

    let (device_model, device_row) = build_device_selector();
    let usb = build_usb_tuner_rows();
    let io = build_io_rows();
    let rtl_tcp = build_rtl_tcp_client_rows();

    // Field-by-field moves from the grouped builders straight into
    // the flat `SourcePanel` handle window.rs consumes — no
    // intermediate destructuring, so the function stays inside the
    // 50-NLOC gate while the panel's public shape is unchanged.
    // Controls are connected to the DSP pipeline via window.rs.
    let panel = SourcePanel {
        widget: group,
        device_row,
        device_model,
        sample_rate_row: usb.sample_rate,
        gain_row: usb.gain,
        agc_row: usb.agc,
        ppm_row: usb.ppm,
        bias_tee_row: usb.bias_tee,
        converter_offset_row: usb.converter_offset,
        airspy_device_row: usb.airspy_device,
        direct_sampling_row: usb.direct_sampling,
        offset_tuning_row: usb.offset_tuning,
        hostname_row: io.hostname,
        port_row: io.port,
        protocol_row: io.protocol,
        file_path_row: io.file_path,
        dc_blocking_row: io.dc_blocking,
        iq_correction_row: io.iq_correction,
        iq_inversion_row: io.iq_inversion,
        decimation_row: io.decimation,
        record_iq_row: io.record_iq,
        rtl_tcp_discovered_row: rtl_tcp.discovered,
        manage_favorites_button: rtl_tcp.manage_favorites_button,
        rtl_tcp_status_row: rtl_tcp.status,
        rtl_tcp_disconnect_button: rtl_tcp.disconnect_button,
        rtl_tcp_retry_button: rtl_tcp.retry_button,
        rtl_tcp_role_row: rtl_tcp.role,
        rtl_tcp_auth_key_row: rtl_tcp.auth_key,
        bandwidth_advisory_row: build_source_bandwidth_advisory_row(),
    };
    attach_source_rows(&panel);
    seed_and_wire_visibility(&panel);
    panel
}
