//! Per-device row-visibility policy for the Source panel
//! (issue #819): the borrowed/weak row bundles, the single-source
//! visibility policy, and the change-notify wiring — including the
//! RTL-TCP-specific rows. Split out of `build.rs` per the 500-NLOC
//! file gate.

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use super::super::{
    DEVICE_AIRSPY, DEVICE_FILE, DEVICE_NETWORK, DEVICE_RTLSDR, DEVICE_RTLTCP, SourcePanel,
};

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

/// Seed initial per-device visibility and install the change-notify
/// handlers, reading every row off the finished panel handle.
/// Split out of [`build_source_panel`] per the 50-NLOC gate (#819).
pub(super) fn seed_and_wire_visibility(panel: &SourcePanel) {
    // Derive initial visibility from the selected device. Funnel
    // through the same helper the change-notify handler uses so
    // the policy lives in exactly one place. Per `CodeRabbit`
    // round 1 on PR #559.
    let selected = panel.device_row.selected();
    // One bundle serves both the initial render and the
    // change-notify wiring below — a single construction site keeps
    // the field list in step when rows are added. Per Codacy round 1
    // on PR #852.
    let visibility_rows = SourceVisibilityRows {
        sample_rate: &panel.sample_rate_row,
        gain: &panel.gain_row,
        agc: &panel.agc_row,
        ppm: &panel.ppm_row,
        bias_tee: &panel.bias_tee_row,
        converter_offset: &panel.converter_offset_row,
        airspy_device: &panel.airspy_device_row,
        direct_sampling: &panel.direct_sampling_row,
        offset_tuning: &panel.offset_tuning_row,
        hostname: &panel.hostname_row,
        port: &panel.port_row,
        protocol: &panel.protocol_row,
        file_path: &panel.file_path_row,
    };
    apply_source_row_visibility(selected, &visibility_rows);
    // RTL-TCP-specific rows aren't part of `apply_source_row_
    // visibility` — they're handled by `connect_rtl_tcp_visibility`
    // below for change-notify, but the initial render still
    // needs to seed them. `is_rtltcp` is recomputed locally
    // because the helper consumes `selected` and doesn't return
    // the projection.
    let is_rtltcp = selected == DEVICE_RTLTCP;
    panel.rtl_tcp_discovered_row.set_visible(is_rtltcp);
    panel.rtl_tcp_status_row.set_visible(is_rtltcp);
    panel.rtl_tcp_role_row.set_visible(is_rtltcp);
    // Auth key row stays hidden until a specific signal
    // (mDNS TXT auth_required=true OR saved key exists for the
    // active host:port). Starting hidden avoids prompting users
    // on servers that don't require auth. The wiring in
    // window.rs flips visibility via the discovery / last-
    // connected load paths.

    connect_device_visibility(&panel.device_row, &visibility_rows);
    connect_rtl_tcp_visibility(
        &panel.device_row,
        &panel.rtl_tcp_discovered_row,
        &panel.rtl_tcp_status_row,
        &panel.rtl_tcp_role_row,
        &panel.rtl_tcp_auth_key_row,
    );
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
