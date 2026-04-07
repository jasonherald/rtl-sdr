//! Source device configuration panel — device selector, RTL-SDR / Network controls.

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

/// Device selector index for RTL-SDR.
const DEVICE_RTLSDR: u32 = 0;
/// Device selector index for Network.
const DEVICE_NETWORK: u32 = 1;
/// Device selector index for File.
const DEVICE_FILE: u32 = 2;

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
}

/// Default sample rate selector index (2.4 MHz = index 7).
const DEFAULT_SAMPLE_RATE_INDEX: u32 = 7;

/// Build RTL-SDR-specific rows: sample rate, gain, AGC.
fn build_rtlsdr_rows() -> (adw::ComboRow, adw::SpinRow, adw::SwitchRow) {
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

    (sample_rate_row, gain_row, agc_row)
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

            sample_rate_row.set_visible(is_rtlsdr);
            gain_row.set_visible(is_rtlsdr);
            agc_row.set_visible(is_rtlsdr);

            hostname_row.set_visible(is_network);
            port_row.set_visible(is_network);
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

    let device_model = gtk4::StringList::new(&["RTL-SDR", "Network", "File"]);
    let device_row = adw::ComboRow::builder()
        .title("Device")
        .model(&device_model)
        .build();

    let (sample_rate_row, gain_row, agc_row) = build_rtlsdr_rows();
    let (hostname_row, port_row, protocol_row) = build_network_rows();
    let file_path_row = adw::EntryRow::builder()
        .title("File Path")
        .text("")
        .visible(false)
        .build();
    let (dc_blocking_row, iq_correction_row, iq_inversion_row, decimation_row) =
        build_common_rows();

    // Add all rows to the group.
    group.add(&device_row);
    group.add(&sample_rate_row);
    group.add(&gain_row);
    group.add(&agc_row);
    group.add(&hostname_row);
    group.add(&port_row);
    group.add(&protocol_row);
    group.add(&file_path_row);
    group.add(&dc_blocking_row);
    group.add(&iq_correction_row);
    group.add(&iq_inversion_row);
    group.add(&decimation_row);

    // Derive initial visibility from the selected device.
    let selected = device_row.selected();
    let is_rtlsdr = selected == DEVICE_RTLSDR;
    let is_network = selected == DEVICE_NETWORK;
    let is_file = selected == DEVICE_FILE;
    sample_rate_row.set_visible(is_rtlsdr);
    gain_row.set_visible(is_rtlsdr);
    agc_row.set_visible(is_rtlsdr);
    hostname_row.set_visible(is_network);
    port_row.set_visible(is_network);
    protocol_row.set_visible(is_network);
    file_path_row.set_visible(is_file);

    connect_device_visibility(
        &device_row,
        &sample_rate_row,
        &gain_row,
        &agc_row,
        &hostname_row,
        &port_row,
        &protocol_row,
        &file_path_row,
    );

    // Controls connected to DSP pipeline via window.rs

    SourcePanel {
        widget: group,
        device_row,
        sample_rate_row,
        gain_row,
        agc_row,
        hostname_row,
        port_row,
        protocol_row,
        file_path_row,
        dc_blocking_row,
        iq_correction_row,
        iq_inversion_row,
        decimation_row,
    }
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

    #[test]
    fn device_indices_are_distinct() {
        assert_ne!(DEVICE_RTLSDR, DEVICE_NETWORK);
    }
}
