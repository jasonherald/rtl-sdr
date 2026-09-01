//! Source device configuration panel — device selector, RTL-SDR /
//! Network / File / RTL-TCP controls.

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_source_rtlsdr::SAMPLE_RATES;
use sdr_types::RtlTcpConnectionState;
/// Decimation factors available in the source panel dropdown.
/// Order is load-bearing: the dropdown row uses the slice index
/// as the persisted combo index, and `DECIMATION_FACTORS[idx]` is
/// the multiplier sent to the DSP. Lives here (not in `window.rs`)
/// so the persistence loader [`load_source_decimation_index`] can
/// bound-check against `DECIMATION_FACTORS.len()` without
/// `window.rs` having to expose the slice. Per `CodeRabbit`
/// round 1 on PR #558.
pub const DECIMATION_FACTORS: &[u32] = &[1, 2, 4, 8, 16];

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

/// Config key for the persisted upconverter offset in Hz. Shared
/// across USB sources — the offset models the antenna chain (one
/// upconverter box), not the dongle behind it. 0.0 = no converter.
/// Per issue #848 phase 4.
pub const KEY_SOURCE_CONVERTER_OFFSET_HZ: &str = "src_converter_offset_hz";

/// Index 0 of the Airspy unit combo — open the first enumerated
/// device rather than a specific serial.
pub const AIRSPY_FIRST_AVAILABLE_LABEL: &str = "First available";

/// Combo index of the "first available" entry in the Airspy unit
/// selector.
pub const AIRSPY_FIRST_AVAILABLE_INDEX: u32 = 0;

/// Combo index of the FIRST enumerated serial — entries above the
/// "first available" slot map serial N to index N + this offset.
pub const AIRSPY_FIRST_SERIAL_INDEX: u32 = 1;

/// Airspy device serial to open, persisted as the 16-digit upper-hex
/// string [`sdr_source_airspy::format_device_serial`] produces (JSON
/// numbers cannot carry a full u64 without precision loss). Empty /
/// absent = first available device. Per #848 phase 5.
pub const KEY_AIRSPY_SERIAL: &str = "src_airspy_serial";

/// Upconverter-offset `SpinRow` defaults (MHz).
const CONVERTER_OFFSET_DEFAULT_MHZ: f64 = 0.0;
/// Lower bound — negative covers block *down*-converters.
const CONVERTER_OFFSET_MIN_MHZ: f64 = -6000.0;
/// Upper bound — comfortably above any HF upconverter product.
const CONVERTER_OFFSET_MAX_MHZ: f64 = 6000.0;
/// Fine step: 1 kHz, enough to trim a converter's LO error.
const CONVERTER_OFFSET_STEP_MHZ: f64 = 0.001;
/// Page step: 1 MHz.
const CONVERTER_OFFSET_PAGE_MHZ: f64 = 1.0;

/// Config key for persisted manual tuner gain in dB. Only
/// applied when AGC is `Off` (hardware/software AGC overrides
/// manual gain). Default `0.0` matches the spin row's initial
/// value. Per issue `#551`.
pub const KEY_SOURCE_RTL_GAIN_DB: &str = "src_rtl_gain_db";

/// Config key for persisted PPM frequency correction. Default
/// `0` (no correction). Per issue `#551`.
pub const KEY_SOURCE_RTL_PPM: &str = "src_rtl_ppm";

/// Config key for persisted RTL2832 direct-sampling mode.
/// Stored as the same `i32` mode value the
/// `rtlsdr_set_direct_sampling` C ABI expects — `0` disables
/// direct sampling (normal tuner path), `1` selects the I
/// branch and `2` selects the Q branch (both bypass the tuner
/// for HF reception). Default `0`. Per issue #538.
pub const KEY_SOURCE_RTL_DIRECT_SAMPLING_MODE: &str = "src_rtl_direct_sampling";

/// Config key for persisted RTL-SDR offset-tuning toggle.
/// Default `false` — support varies by tuner/driver (E4000
/// honors it; R820T/R828D reject with `InvalidParameter`; the
/// driver also rejects while direct sampling is enabled), so
/// the default keeps tuning behavior predictable across
/// hardware variants. Per issue #539.
pub const KEY_SOURCE_RTL_OFFSET_TUNING: &str = "src_rtl_offset_tuning";

/// Direct-sampling combo indices. Order is load-bearing —
/// matches the `rtlsdr_set_direct_sampling` mode argument the
/// driver expects (0/1/2). The combo's user-visible label list
/// in [`build_rtlsdr_rows`] is built in the same order so the
/// selected index can be cast straight to the driver mode.
pub const DIRECT_SAMPLING_DISABLED_IDX: u32 = 0;
pub const DIRECT_SAMPLING_I_BRANCH_IDX: u32 = 1;
pub const DIRECT_SAMPLING_Q_BRANCH_IDX: u32 = 2;
/// Highest valid direct-sampling combo index, used by the
/// loader to bound-check persisted values.
pub const DIRECT_SAMPLING_MAX_IDX: u32 = DIRECT_SAMPLING_Q_BRANCH_IDX;

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
/// Airspy R2 / Mini USB receiver (`libairspy-rs`). Appended per the
/// stable-index rule — existing entries are config keys. Per #848.
pub const DEVICE_AIRSPY: u32 = 4;

/// Display labels for the RTL-SDR sample-rate table — index-aligned
/// with [`SAMPLE_RATES`].
pub const RTL_SAMPLE_RATE_LABELS: &[&str] = &[
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
];

/// Display labels for the Airspy IQ sample-rate table — index-aligned
/// with [`sdr_source_airspy::DEFAULT_SAMPLE_RATES`] (the R2 firmware
/// table; a Mini's extra rates arrive with the rate-list event in
/// #848 follow-up scope).
pub const AIRSPY_SAMPLE_RATE_LABELS: &[&str] = &["2.5 MHz", "10 MHz"];

/// The sample-rate table backing the rate combo for a device-combo
/// index. Sources without a tunable rate row fall back to the RTL
/// table (their row is hidden anyway).
#[must_use]
pub fn sample_rates_for_device(device: u32) -> &'static [f64] {
    match device {
        DEVICE_AIRSPY => sdr_source_airspy::DEFAULT_SAMPLE_RATES,
        _ => SAMPLE_RATES,
    }
}

/// Index-aligned labels for [`sample_rates_for_device`].
#[must_use]
pub fn sample_rate_labels_for_device(device: u32) -> &'static [&'static str] {
    match device {
        DEVICE_AIRSPY => AIRSPY_SAMPLE_RATE_LABELS,
        _ => RTL_SAMPLE_RATE_LABELS,
    }
}

/// Reconfigure the gain row for the selected device. Airspy's gain
/// is the unitless 0-21 composite linearity ladder in whole steps
/// (encoded x10 through the tenths-based dispatch), not a dB value —
/// leaving the RTL adjustment in place would display "30.0 dB" while
/// the source clamps to step 21. The live `GainList` event still
/// refines the bounds after Play; this keeps the row honest at
/// selection time. Per CR round 1 on PR #850.
/// Bottom of the Airspy composite linearity ladder.
const AIRSPY_GAIN_MIN_STEP: f64 = 0.0;
/// Top of the ladder, derived from the source crate's step count so
/// the UI bound cannot drift from the gain contract.
const AIRSPY_GAIN_MAX_STEP: f64 = (sdr_source_airspy::LINEARITY_GAIN_STEPS - 1) as f64;
/// The ladder moves in whole steps.
const AIRSPY_GAIN_STEP: f64 = 1.0;
/// Page increment — a few steps per click, mirroring the RTL row's
/// step/page ratio.
const AIRSPY_GAIN_PAGE: f64 = 3.0;

pub fn apply_device_gain_row(gain_row: &adw::SpinRow, device: u32) {
    let adj = gain_row.adjustment();
    if device == DEVICE_AIRSPY {
        gain_row.set_subtitle("Linearity step");
        gain_row.set_digits(0);
        adj.set_lower(AIRSPY_GAIN_MIN_STEP);
        adj.set_upper(AIRSPY_GAIN_MAX_STEP);
        adj.set_step_increment(AIRSPY_GAIN_STEP);
        adj.set_page_increment(AIRSPY_GAIN_PAGE);
    } else {
        gain_row.set_subtitle("dB");
        gain_row.set_digits(1);
        adj.set_lower(MIN_GAIN_DB);
        adj.set_upper(MAX_GAIN_DB);
        adj.set_step_increment(GAIN_STEP_DB);
        adj.set_page_increment(GAIN_PAGE_DB);
    }
}

/// Swap the rate combo's `StringList` contents to the device's label
/// set. Splice keeps the model object (the `ComboRow` holds it), so
/// only the rows change; the caller re-seeds the selection afterwards
/// (the splice itself fires transient `selected` notifies that the
/// out-of-range guards in the notify handler discard).
pub fn repopulate_sample_rate_model(sample_rate_row: &adw::ComboRow, device: u32) {
    let Some(model) = sample_rate_row
        .model()
        .and_then(|m| m.downcast::<gtk4::StringList>().ok())
    else {
        tracing::warn!("sample-rate combo model is not a StringList; skipping repopulate");
        return;
    };
    let labels = sample_rate_labels_for_device(device);
    model.splice(0, model.n_items(), labels);
}

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
/// thread — `librtlsdr_rs::get_device_count` is a libusb enumerate
/// filtered by vendor/product ID, and `get_device_name` re-runs
/// the same enumerate to reach the Nth match.
///
/// When `get_device_count` reports > 0 but `get_device_name`
/// returns an empty string (shouldn't happen outside of a race
/// where the device was unplugged between the two enumerate
/// calls), we fall back to the generic "RTL-SDR" label so the
/// UI stays usable rather than rendering an empty combo entry.
pub fn probe_rtlsdr_device_label() -> String {
    if librtlsdr_rs::get_device_count() == 0 {
        return RTLSDR_ABSENT_LABEL.to_string();
    }
    let name = librtlsdr_rs::get_device_name(0);
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

/// Default raw-Network port, wire form. Single source of truth —
/// the `f64` [`DEFAULT_PORT`] the `gtk4::Adjustment` consumes and
/// the persistence loader's fallback both derive from it, so the
/// widget default and the persisted fallback can't drift
/// (`CodeRabbit` round 1 on PR #886).
const DEFAULT_PORT_U16: u16 = 1234;
/// Default network port ([`DEFAULT_PORT_U16`] in the `f64` form
/// `gtk4::Adjustment` requires).
const DEFAULT_PORT: f64 = DEFAULT_PORT_U16 as f64;
/// Default raw-Network hostname, shared by the widget's initial
/// value and the persistence fallback (`CodeRabbit` round 1 on
/// PR #886).
const DEFAULT_NETWORK_HOSTNAME: &str = "localhost";
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
    /// Upconverter offset `SpinRow` in MHz (hardware = display +
    /// offset). Visible for the USB tuner sources. Per #848 phase 4.
    pub converter_offset_row: adw::SpinRow,
    /// Airspy unit selector: "First available" plus one entry per
    /// enumerated serial (populated by the `AirspyDeviceList`
    /// event). Visibility-gated to the Airspy device. Per #848
    /// phase 5.
    pub airspy_device_row: adw::ComboRow,
    /// RTL-SDR direct-sampling combo (Disabled / I branch /
    /// Q branch). Q branch is how RTL-SDR Blog v3+ dongles tune
    /// below 28 MHz — the R820T tuner cuts off there, but the
    /// RTL2832 ADC can sample I or Q directly when the tuner is
    /// bypassed. Visibility-gated to local RTL-SDR USB only in
    /// this panel (hidden for Network / File / `rtl_tcp`). Per
    /// issue #538.
    pub direct_sampling_row: adw::ComboRow,
    /// RTL-SDR offset-tuning toggle. Pushes the LO away from
    /// the tuned frequency to dodge the DC spike. Most relevant
    /// on E4000 tuners; support varies by tuner and driver, and
    /// unsupported hardware (R820T / R828D) rejects the request
    /// with `InvalidParameter`. The change-notify handler in
    /// `connect_source_panel` surfaces driver rejections as a
    /// `TuneFailed` toast so the user gets a clear "your tuner
    /// doesn't support this" rather than a silent no-op. Same
    /// visibility gate as the rest of the RTL-SDR-specific
    /// rows. Per issue #539.
    pub offset_tuning_row: adw::SwitchRow,
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
/// Shared with [`load_source_sample_rate_index`] so the loader's
/// fallback matches the widget's initial selection.
pub(crate) const DEFAULT_SAMPLE_RATE_INDEX: u32 = 7;

/// Format a sample rate in Hz as the combo label the static tables
/// use ("2.5 MHz", "10 MHz"). Trailing `.0` is trimmed so integer
/// megahertz read clean. Per #848 phase 5.
#[must_use]
pub fn format_rate_label(rate_hz: f64) -> String {
    const HZ_PER_MHZ: f64 = 1_000_000.0;
    let mhz = rate_hz / HZ_PER_MHZ;
    let text = format!("{mhz:.3}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    format!("{trimmed} MHz")
}

/// Index of the entry nearest `current_hz` in a device-reported
/// rate list; `None` on an empty list. The source clamps requests to
/// its table, so an exact match is the norm — nearest keeps the UI
/// honest if rounding ever drifts. Per #848 phase 5.
#[must_use]
pub fn nearest_rate_index(rates: &[f64], current_hz: f64) -> Option<usize> {
    rates
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (*a - current_hz).abs().total_cmp(&(*b - current_hz).abs()))
        .map(|(i, _)| i)
}

/// Swap the rate combo's labels to a device-reported rate list (the
/// dynamic counterpart of [`repopulate_sample_rate_model`]). The
/// caller re-seeds the selection afterwards.
pub fn repopulate_sample_rate_model_from_rates(sample_rate_row: &adw::ComboRow, rates: &[f64]) {
    let Some(model) = sample_rate_row
        .model()
        .and_then(|m| m.downcast::<gtk4::StringList>().ok())
    else {
        tracing::warn!("sample-rate combo model is not a StringList; skipping repopulate");
        return;
    };
    let labels: Vec<String> = rates.iter().map(|&r| format_rate_label(r)).collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    model.splice(0, model.n_items(), &label_refs);
}

mod build;
mod favorites;
mod persistence;

pub use build::build_source_panel;
pub use favorites::{
    FavoriteEntry, FavoriteRole, LastConnectedServer, load_favorites, load_last_connected,
    now_unix_seconds, save_favorites, save_last_connected,
};
pub use persistence::{
    load_agc_type, load_airspy_serial, load_source_converter_offset_hz, load_source_dc_blocking,
    load_source_decimation_index, load_source_device_index, load_source_file_path,
    load_source_iq_correction, load_source_iq_inversion, load_source_network_hostname,
    load_source_network_port, load_source_network_protocol_index, load_source_rtl_bias_tee,
    load_source_rtl_direct_sampling_mode, load_source_rtl_gain_db, load_source_rtl_offset_tuning,
    load_source_rtl_ppm, load_source_sample_rate_index, save_agc_type, save_airspy_serial,
    save_source_converter_offset_hz, save_source_dc_blocking, save_source_decimation_index,
    save_source_device_index, save_source_file_path, save_source_iq_correction,
    save_source_iq_inversion, save_source_network_hostname, save_source_network_port,
    save_source_network_protocol_index, save_source_rtl_bias_tee,
    save_source_rtl_direct_sampling_mode, save_source_rtl_gain_db, save_source_rtl_offset_tuning,
    save_source_rtl_ppm, save_source_sample_rate_index,
};

#[cfg(test)]
mod tests;
