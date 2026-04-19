//! Source device configuration panel — device selector, RTL-SDR /
//! Network / File / RTL-TCP controls.

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

/// Device selector index for RTL-SDR.
pub const DEVICE_RTLSDR: u32 = 0;
/// Device selector index for Network.
pub const DEVICE_NETWORK: u32 = 1;
/// Device selector index for File.
pub const DEVICE_FILE: u32 = 2;
/// Device selector index for RTL-TCP (rtl_tcp-protocol network client).
pub const DEVICE_RTLTCP: u32 = 3;

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
    //
    // Connection-state display (Connecting / Connected / Retrying /
    // Failed) is intentionally deferred to #323 — wiring it requires
    // a new DspToUi event from the controller that polls
    // RtlTcpSource::connection_state(), which is outside this PR's
    // scope. Shipping the row without that plumbing would leave it
    // stuck on "Disconnected" misleadingly.
    let rtl_tcp_discovered_row = adw::ExpanderRow::builder()
        .title("Discovered rtl_tcp servers")
        .subtitle("No servers discovered on the local network yet.")
        .visible(false)
        .build();

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
    connect_rtl_tcp_visibility(&device_row, &rtl_tcp_discovered_row);

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
    }
}

/// Toggle visibility of the RTL-TCP-specific rows based on the device
/// selector. Kept separate from `connect_device_visibility` so the
/// existing function's argument list doesn't grow further.
fn connect_rtl_tcp_visibility(
    device_row: &adw::ComboRow,
    rtl_tcp_discovered_row: &adw::ExpanderRow,
) {
    device_row.connect_selected_notify(glib::clone!(
        #[weak]
        rtl_tcp_discovered_row,
        move |row| {
            let is_rtltcp = row.selected() == DEVICE_RTLTCP;
            rtl_tcp_discovered_row.set_visible(is_rtltcp);
        }
    ));
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
}
