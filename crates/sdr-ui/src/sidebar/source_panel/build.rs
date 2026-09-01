//! Widget construction for the Source panel (issue #819): the
//! per-source-type row builders, the device-visibility policy and
//! its change-notify wiring, and [`build_source_panel`] itself.
//! Split out of `source_panel.rs` per the file-size pass.

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use super::{
    AIRSPY_FIRST_AVAILABLE_LABEL, AgcType, CONVERTER_OFFSET_DEFAULT_MHZ, CONVERTER_OFFSET_MAX_MHZ,
    CONVERTER_OFFSET_MIN_MHZ, CONVERTER_OFFSET_PAGE_MHZ, CONVERTER_OFFSET_STEP_MHZ,
    DEFAULT_GAIN_DB, DEFAULT_PORT, DEFAULT_PPM, DEFAULT_SAMPLE_RATE_INDEX, DEVICE_AIRSPY,
    DEVICE_FILE, DEVICE_NETWORK, DEVICE_RTLSDR, DEVICE_RTLTCP, DIRECT_SAMPLING_DISABLED_IDX,
    GAIN_PAGE_DB, GAIN_STEP_DB, HIGH_BANDWIDTH_ADVISORY_SUBTITLE, HIGH_BANDWIDTH_ADVISORY_TITLE,
    MAX_GAIN_DB, MAX_PORT, MAX_PPM, MIN_GAIN_DB, MIN_PORT, MIN_PPM, PORT_PAGE, PORT_STEP, PPM_PAGE,
    PPM_STEP, RTL_SAMPLE_RATE_LABELS, RTL_TCP_ROLE_CONTROL_IDX,
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

/// Borrowed row handles for the per-device visibility pass. One
/// bundle instead of a dozen positional widget params — both callers
/// (initial render and the change-notify handler) build it from the
/// rows they already hold.
struct SourceVisibilityRows<'a> {
    sample_rate: &'a adw::ComboRow,
    gain: &'a adw::SpinRow,
    agc: &'a adw::ComboRow,
    ppm: &'a adw::SpinRow,
    bias_tee: &'a adw::SwitchRow,
    converter_offset: &'a adw::SpinRow,
    airspy_device: &'a adw::ComboRow,
    direct_sampling: &'a adw::ComboRow,
    offset_tuning: &'a adw::SwitchRow,
    hostname: &'a adw::EntryRow,
    port: &'a adw::SpinRow,
    protocol: &'a adw::ComboRow,
    file_path: &'a adw::EntryRow,
}

/// Apply the per-device visibility policy to every row whose
/// visibility depends on the selected source type. Single source
/// of truth for the policy — both [`build_source_panel`]'s
/// initial render block AND [`connect_device_visibility`]'s
/// `connect_selected_notify` handler call this so they can never
/// drift as rows evolve. Per `CodeRabbit` round 1 on PR #559.
///
/// Policy:
/// - **Tune controls** (sample rate, gain, AGC, PPM): visible
///   for local RTL-SDR USB AND remote `rtl_tcp` — both route
///   through the `Source` trait's `set_gain` / `set_gain_mode` /
///   `set_ppm_correction` hooks, and `RtlTcpSource` forwards
///   them as wire commands.
/// - **Bias tee, direct sampling, offset tuning**: visible only
///   for local RTL-SDR USB. The `rtl_tcp` wire protocol exposes
///   equivalents but this panel doesn't surface them today (the
///   server-side panel has its own bias-T toggle); the `Source`
///   trait defaults silently drop these commands on
///   file/network sources.
/// - **Hostname / port**: visible for raw-IQ Network AND
///   `rtl_tcp` — both dial a host:port. **Protocol** (TCP / UDP)
///   only applies to raw Network — `rtl_tcp` always rides on
///   TCP.
/// - **File path**: visible only for File source.
fn apply_source_row_visibility(selected: u32, rows: &SourceVisibilityRows<'_>) {
    let SourceVisibilityRows {
        sample_rate: sample_rate_row,
        gain: gain_row,
        agc: agc_row,
        ppm: ppm_row,
        bias_tee: bias_tee_row,
        converter_offset: converter_offset_row,
        airspy_device: airspy_device_row,
        direct_sampling: direct_sampling_row,
        offset_tuning: offset_tuning_row,
        hostname: hostname_row,
        port: port_row,
        protocol: protocol_row,
        file_path: file_path_row,
    } = rows;
    let is_rtlsdr = selected == DEVICE_RTLSDR;
    let is_network = selected == DEVICE_NETWORK;
    let is_file = selected == DEVICE_FILE;
    let is_rtltcp = selected == DEVICE_RTLTCP;
    let is_airspy = selected == DEVICE_AIRSPY;

    let tune_controls_visible = is_rtlsdr || is_rtltcp || is_airspy;
    sample_rate_row.set_visible(tune_controls_visible);
    gain_row.set_visible(tune_controls_visible);
    agc_row.set_visible(tune_controls_visible);
    // No PPM row for Airspy — 0.5 ppm TCXO, and the source's
    // `set_ppm_correction` is the trait no-op.
    ppm_row.set_visible(is_rtlsdr || is_rtltcp);

    // Bias-T powers a SpyVerter / mast-head LNA on both USB radios.
    bias_tee_row.set_visible(is_rtlsdr || is_airspy);
    converter_offset_row.set_visible(is_rtlsdr || is_airspy);
    airspy_device_row.set_visible(is_airspy);
    direct_sampling_row.set_visible(is_rtlsdr);
    offset_tuning_row.set_visible(is_rtlsdr);

    hostname_row.set_visible(is_network || is_rtltcp);
    port_row.set_visible(is_network || is_rtltcp);
    protocol_row.set_visible(is_network);

    file_path_row.set_visible(is_file);
}

/// Weak counterparts of [`SourceVisibilityRows`] captured by the
/// device-change closure — weak so the handler can't keep the panel
/// widgets alive past window teardown.
struct SourceVisibilityWeak {
    sample_rate: glib::WeakRef<adw::ComboRow>,
    gain: glib::WeakRef<adw::SpinRow>,
    agc: glib::WeakRef<adw::ComboRow>,
    ppm: glib::WeakRef<adw::SpinRow>,
    bias_tee: glib::WeakRef<adw::SwitchRow>,
    converter_offset: glib::WeakRef<adw::SpinRow>,
    airspy_device: glib::WeakRef<adw::ComboRow>,
    direct_sampling: glib::WeakRef<adw::ComboRow>,
    offset_tuning: glib::WeakRef<adw::SwitchRow>,
    hostname: glib::WeakRef<adw::EntryRow>,
    port: glib::WeakRef<adw::SpinRow>,
    protocol: glib::WeakRef<adw::ComboRow>,
    file_path: glib::WeakRef<adw::EntryRow>,
}

impl SourceVisibilityWeak {
    fn from_rows(rows: &SourceVisibilityRows<'_>) -> Self {
        Self {
            sample_rate: rows.sample_rate.downgrade(),
            gain: rows.gain.downgrade(),
            agc: rows.agc.downgrade(),
            ppm: rows.ppm.downgrade(),
            bias_tee: rows.bias_tee.downgrade(),
            converter_offset: rows.converter_offset.downgrade(),
            airspy_device: rows.airspy_device.downgrade(),
            direct_sampling: rows.direct_sampling.downgrade(),
            offset_tuning: rows.offset_tuning.downgrade(),
            hostname: rows.hostname.downgrade(),
            port: rows.port.downgrade(),
            protocol: rows.protocol.downgrade(),
            file_path: rows.file_path.downgrade(),
        }
    }

    /// Upgrade the whole set atomically; `None` when any widget is
    /// gone (window teardown) so the caller drops the event.
    fn apply(&self, selected: u32) -> Option<()> {
        apply_source_row_visibility(
            selected,
            &SourceVisibilityRows {
                sample_rate: &self.sample_rate.upgrade()?,
                gain: &self.gain.upgrade()?,
                agc: &self.agc.upgrade()?,
                ppm: &self.ppm.upgrade()?,
                bias_tee: &self.bias_tee.upgrade()?,
                converter_offset: &self.converter_offset.upgrade()?,
                airspy_device: &self.airspy_device.upgrade()?,
                direct_sampling: &self.direct_sampling.upgrade()?,
                offset_tuning: &self.offset_tuning.upgrade()?,
                hostname: &self.hostname.upgrade()?,
                port: &self.port.upgrade()?,
                protocol: &self.protocol.upgrade()?,
                file_path: &self.file_path.upgrade()?,
            },
        );
        Some(())
    }
}

/// Wire the device selector to show/hide source-specific rows.
fn connect_device_visibility(device_row: &adw::ComboRow, rows: &SourceVisibilityRows<'_>) {
    let weak = SourceVisibilityWeak::from_rows(rows);
    device_row.connect_selected_notify(move |row| {
        let selected = row.selected();
        let _ = weak.apply(selected);
        tracing::debug!(device = selected, "source device changed");
    });
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
        "Airspy R2 / Mini",
    ]);
    let device_row = adw::ComboRow::builder()
        .title("Device")
        .model(&device_model)
        .build();

    let (
        sample_rate_row,
        gain_row,
        agc_row,
        ppm_row,
        bias_tee_row,
        converter_offset_row,
        direct_sampling_row,
        offset_tuning_row,
    ) = build_rtlsdr_rows();
    // Airspy unit selector. Starts with only the "first available"
    // entry; the wiring swaps in enumerated serials when the
    // controller answers `RefreshAirspyDevices`. Per #848 phase 5.
    let airspy_device_model = gtk4::StringList::new(&[AIRSPY_FIRST_AVAILABLE_LABEL]);
    let airspy_device_row = adw::ComboRow::builder()
        .title("Airspy Unit")
        .subtitle("Which device to open when multiple are connected")
        .model(&airspy_device_model)
        .build();

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
    group.add(&converter_offset_row);
    group.add(&airspy_device_row);
    group.add(&direct_sampling_row);
    group.add(&offset_tuning_row);
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

    // Derive initial visibility from the selected device. Funnel
    // through the same helper the change-notify handler uses so
    // the policy lives in exactly one place. Per `CodeRabbit`
    // round 1 on PR #559.
    let selected = device_row.selected();
    // One bundle serves both the initial render and the
    // change-notify wiring below — a single construction site keeps
    // the field list in step when rows are added. Per Codacy round 1
    // on PR #852.
    let visibility_rows = SourceVisibilityRows {
        sample_rate: &sample_rate_row,
        gain: &gain_row,
        agc: &agc_row,
        ppm: &ppm_row,
        bias_tee: &bias_tee_row,
        converter_offset: &converter_offset_row,
        airspy_device: &airspy_device_row,
        direct_sampling: &direct_sampling_row,
        offset_tuning: &offset_tuning_row,
        hostname: &hostname_row,
        port: &port_row,
        protocol: &protocol_row,
        file_path: &file_path_row,
    };
    apply_source_row_visibility(selected, &visibility_rows);
    // RTL-TCP-specific rows aren't part of `apply_source_row_
    // visibility` — they're handled by `connect_rtl_tcp_visibility`
    // below for change-notify, but the initial render still
    // needs to seed them. `is_rtltcp` is recomputed locally
    // because the helper consumes `selected` and doesn't return
    // the projection.
    let is_rtltcp = selected == DEVICE_RTLTCP;
    rtl_tcp_discovered_row.set_visible(is_rtltcp);
    rtl_tcp_status_row.set_visible(is_rtltcp);
    rtl_tcp_role_row.set_visible(is_rtltcp);
    // Auth key row stays hidden until a specific signal
    // (mDNS TXT auth_required=true OR saved key exists for the
    // active host:port). Starting hidden avoids prompting users
    // on servers that don't require auth. The wiring in
    // window.rs flips visibility via the discovery / last-
    // connected load paths.

    connect_device_visibility(&device_row, &visibility_rows);
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
        converter_offset_row,
        airspy_device_row,
        direct_sampling_row,
        offset_tuning_row,
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
