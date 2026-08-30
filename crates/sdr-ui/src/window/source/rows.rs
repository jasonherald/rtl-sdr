//! Source-panel row wiring: device/rate/source-type selection and
//! RTL-SDR frontend toggles (DC block, bias-T, converter offset,
//! sampling mode, IQ inversion, decimation). Split out of
//! `window/source.rs` per the Codacy large-file gate (#846).
//! AGC-related rows live in `rows/agc.rs`; network / `rtl_tcp`-client
//! rows and Airspy unit selection live in `rows/network.rs`.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{
    AppState, DECIMATION_FACTORS, DEVICE_FILE, DEVICE_NETWORK, DEVICE_RTLSDR, DEVICE_RTLTCP, Rc,
    RefCell, SAMPLE_RATES, SidebarPanels, SourceType, UiToDsp, adw, glib, plain_toast, sidebar,
};
mod agc;
mod network;

pub(super) use agc::wire_gain_and_agc_rows;
pub(super) use network::{
    wire_airspy_device_row, wire_iq_ppm_and_restart_rows, wire_network_source_rows,
    wire_role_and_server_key_rows, wire_rtl_tcp_client_rows,
};

/// Interval for refreshing the source combo's RTL-SDR slot label
/// against the live USB bus. Low-frequency enough to be
/// negligible CPU-wise; fast enough that a user plugging in their
/// dongle after app launch sees the slot update to the real
/// device name within a few seconds without having to restart.
///
/// Previously shared cadence with a server-panel hotplug poll that
/// drove panel visibility — that poll was removed when Share became
/// its own activity icon, but this source-combo poller's 3 s cadence
/// was tuned for the same reason (user plugs in a dongle, sees the
/// slot update by the time they reach for the sidebar) so the value
/// remains a good fit on its own.
pub(super) const SOURCE_RTLSDR_PROBE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(3);

/// Install a hotplug poller on the source panel that keeps the
/// RTL-SDR slot label (`device_row` entry 0) in sync with the
/// live USB bus. Seeded once at build-time (inside
/// `build_source_panel`); this helper adds the ongoing refresh.
///
/// Compared against a cached last-seen label so the `splice` fires
/// only on real edges — plugging in, unplugging, or USB string
/// changing. Without the edge gate we'd churn the combo's model
/// every 3 s and risk transient selection flicker (though GTK's
/// `ComboRow` is robust to same-value splices, the no-op is
/// cheaper to skip than to perform).
///
/// Weak ref on the source panel's `widget` so the poller tears
/// down cleanly on window close — upgrade returns `None` and the
/// `ControlFlow::Break` arm fires.
pub(in crate::window) fn connect_source_rtlsdr_probe(panels: &SidebarPanels) {
    let widget_weak = panels.source.widget.downgrade();
    let model_weak = panels.source.device_model.downgrade();
    // Cached label from the last tick so we only rewrite on a
    // real edge. Seed from the model's current `DEVICE_RTLSDR`
    // entry — NOT from a fresh probe — so we're comparing
    // subsequent probes against what the UI is actually showing.
    //
    // A second probe here would race the USB state: if the user
    // unplugs their dongle between `build_source_panel` (which
    // ran the initial probe + seed) and this wiring point, a
    // second probe would read the new bus state, cache it as
    // `last_label`, and then every subsequent tick's probe would
    // match the cache — the combo would stay on the stale plugged-
    // in name forever (or until the NEXT plug / unplug edge
    // briefly desynced them again). Reading the model directly
    // guarantees first-tick reconciliation.
    let seed_label = panels
        .source
        .device_model
        .string(DEVICE_RTLSDR)
        .map_or_else(String::new, |s| s.to_string());
    let last_label: Rc<RefCell<String>> = Rc::new(RefCell::new(seed_label));
    let _ = glib::timeout_add_local(SOURCE_RTLSDR_PROBE_INTERVAL, move || {
        if widget_weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        let Some(model) = model_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let current = sidebar::source_panel::probe_rtlsdr_device_label();
        let mut last = last_label.borrow_mut();
        if *last != current {
            tracing::debug!(
                previous = %*last,
                current = %current,
                "source panel: RTL-SDR slot label updated",
            );
            // Replace the RTL-SDR slot in the StringList.
            // `splice(pos, n, additions)` removes `n` items at
            // `pos` and inserts `additions` — so `(DEVICE_RTLSDR,
            // 1, &[&current])` is a single-entry in-place swap.
            // Using the shared `DEVICE_RTLSDR` constant instead
            // of a literal `0` keeps the probe aligned with the
            // rest of the source-row selection logic; all four
            // `DEVICE_*` indices are the one source of truth for
            // slot positions. Leaves Network / File / RTL-TCP
            // entries untouched.
            model.splice(DEVICE_RTLSDR, 1, &[&current]);
            *last = current;
        }
        glib::ControlFlow::Continue
    });
}

/// Sample-rate selector + bandwidth advisory + device selector (restore-then-wire, #552).
/// Split out per the 50-NLOC gate (#817).
pub(super) fn wire_sample_rate_and_device_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Sample rate selector + bandwidth advisory re-render.
    // The advisory visibility depends on BOTH the sample-rate
    // selection AND the device-type selection (only network paths
    // care about wire bandwidth). We clone the helper closure into
    // both notify handlers so either trigger re-evaluates.
    // All three widgets the advisory closure touches are weak-
    // ref'd. The closure is attached to both `sample_rate_row` and
    // `device_row`'s `connect_selected_notify` — strong captures
    // here would create the same self-cycle pattern flagged in
    // `connect_share_switch` / `connect_server_status_polling`:
    // `row → closure → row.clone()` keeps the widget alive forever.
    let advisory_row_weak = panels.source.bandwidth_advisory_row.downgrade();
    let device_row_weak = panels.source.device_row.downgrade();
    let sample_rate_row_weak = panels.source.sample_rate_row.downgrade();
    let apply_source_bandwidth_advisory = {
        let advisory_row_weak = advisory_row_weak.clone();
        let device_row_weak = device_row_weak.clone();
        let sample_rate_row_weak = sample_rate_row_weak.clone();
        move || {
            // Any missing widget means the window has been torn
            // down; skip the render — subsequent notify events
            // won't fire against dead widgets.
            let (Some(advisory), Some(device_row), Some(sample_rate_row)) = (
                advisory_row_weak.upgrade(),
                device_row_weak.upgrade(),
                sample_rate_row_weak.upgrade(),
            ) else {
                return;
            };
            // Raw Network (TCP/UDP IQ) has the same wire-bandwidth
            // cost profile as rtl_tcp — a high-sample-rate pull
            // across the network will saturate a 100 Mbit link
            // either way. The advisory applies equally to both
            // network-backed source types.
            let is_network_path = matches!(device_row.selected(), DEVICE_NETWORK | DEVICE_RTLTCP);
            // Bounds-check the sample-rate index: transient
            // out-of-range values from widget-model churn would
            // otherwise satisfy the `>= threshold` compare and
            // flash the advisory visible with no legal selection.
            // Same safety pattern as the server-panel advisory
            // above.
            let selected = sample_rate_row.selected();
            let is_high_rate = (selected as usize) < SAMPLE_RATES.len()
                && selected >= crate::sidebar::source_panel::HIGH_BANDWIDTH_SAMPLE_RATE_IDX;
            advisory.set_visible(is_network_path && is_high_rate);
        }
    };
    // Seed the advisory visibility once at wire-up. Without this,
    // the caption stays hidden until the user nudges one of the
    // two rows — which hides it even when the restored config
    // already has RTL-TCP + a high sample rate selected.
    apply_source_bandwidth_advisory();

    wire_sample_rate_selector(
        panels,
        state,
        config,
        apply_source_bandwidth_advisory.clone(),
    );
}

/// DC blocking / bias-T / direct sampling / offset tuning / IQ inversion / decimation rows (restore-then-wire).
/// Split out per the 50-NLOC gate (#817).
pub(super) fn wire_rtl_frontend_toggles(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    toast_overlay: &adw::ToastOverlay,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // DC blocking toggle. Restore-then-wire (#552). Same idiom
    // as bias-T / gain / PPM: programmatic `set_active` fires
    // `connect_active_notify`, which would re-save the loaded
    // value AND re-dispatch `SetDcBlocking` — both cheap, but
    // the duplicate dispatch in tracing logs is misleading. So
    // restore first, then wire.
    {
        let persisted = sidebar::source_panel::load_source_dc_blocking(config);
        panels.source.dc_blocking_row.set_active(persisted);
        state.send_dsp(UiToDsp::SetDcBlocking(persisted));
    }
    let state_dc_block = Rc::clone(state);
    let config_dc_block = std::sync::Arc::clone(config);
    panels
        .source
        .dc_blocking_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_dc_blocking(&config_dc_block, enabled);
            state_dc_block.send_dsp(UiToDsp::SetDcBlocking(enabled));
        });

    // Bias-T toggle (#537). Powers an inline LNA over the
    // RTL-SDR's coax. The startup restore must run BEFORE
    // wiring the change-notify handler — same idiom as the
    // satellites-panel auto-record toggle: a programmatic
    // `set_active` fires `connect_active_notify`, which would
    // otherwise re-save the just-loaded value (cheap) AND
    // dispatch a redundant `SetBiasTee` (also cheap, but
    // misleading in tracing logs).
    {
        let persisted = sidebar::source_panel::load_source_rtl_bias_tee(config);
        panels.source.bias_tee_row.set_active(persisted);
        // Dispatch the persisted value once at startup so the
        // dongle's GPIO matches the UI from the first source
        // open, not just after the user toggles. The
        // `SetBiasTee` handler stores the value in `DspState`
        // up-front, and `open_source` re-applies it to the
        // freshly-opened RTL-SDR source — so this dispatch
        // works regardless of whether a source is open at
        // startup. Per CR on PR #550.
        state.send_dsp(UiToDsp::SetBiasTee(persisted));
    }
    let state_bias_tee = Rc::clone(state);
    let config_bias_tee = std::sync::Arc::clone(config);
    panels
        .source
        .bias_tee_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_rtl_bias_tee(&config_bias_tee, enabled);
            state_bias_tee.send_dsp(UiToDsp::SetBiasTee(enabled));
        });

    wire_converter_offset_row(panels, state, config);

    wire_sampling_mode_rows(panels, state, toast_overlay, config);

    wire_iq_inversion_and_decimation(panels, state, config);
}

/// Upconverter offset row (#848 phase 4). Same restore-then-wire +
/// startup-dispatch idiom as bias-T: the `SetConverterOffset`
/// handler stores the value in `DspState` up-front and the
/// pre-start settings replay it onto every freshly opened USB
/// source. The row edits in MHz; the wire value is Hz.
fn wire_converter_offset_row(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    /// The row edits MHz; config + wire carry Hz.
    const HZ_PER_MHZ: f64 = 1_000_000.0;
    {
        let persisted_hz = sidebar::source_panel::load_source_converter_offset_hz(config);
        panels
            .source
            .converter_offset_row
            .set_value(persisted_hz / HZ_PER_MHZ);
        state.send_dsp(UiToDsp::SetConverterOffset(persisted_hz));
    }
    let state_offset = Rc::clone(state);
    let config_offset = std::sync::Arc::clone(config);
    panels
        .source
        .converter_offset_row
        .connect_value_notify(move |row| {
            let offset_hz = row.value() * HZ_PER_MHZ;
            sidebar::source_panel::save_source_converter_offset_hz(&config_offset, offset_hz);
            state_offset.send_dsp(UiToDsp::SetConverterOffset(offset_hz));
        });
}

/// Exclusivity guard for the source-type selector: reverts the
/// combo when the user tries to re-select local RTL-SDR while the
/// `rtl_tcp` server still has the dongle claimed. Returns `true`
/// when it reverted (caller should stop processing this notify).
/// Split out of `wire_source_type_guard` per the 50-NLOC gate
/// (#846).
fn revert_device_row_if_server_running(
    row: &adw::ComboRow,
    selected: u32,
    server_running: &Rc<std::cell::Cell<bool>>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    reverting: &Rc<std::cell::Cell<bool>>,
    last_legal_selection: &Rc<std::cell::Cell<u32>>,
) -> bool {
    // Exclusivity guard: can't re-enter the local-source world
    // while the rtl_tcp server has the dongle claimed.
    if selected != DEVICE_RTLSDR || !server_running.get() {
        return false;
    }
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        overlay.add_toast(plain_toast(
            "Stop the network server first before switching to local RTL-SDR.",
        ));
    }
    reverting.set(true);
    row.set_selected(last_legal_selection.get());
    reverting.set(false);
    true
}

/// Map a `device_row` selected index to its `SourceType`, or `None`
/// for a transient out-of-range index. Split out of
/// `wire_source_type_guard` per the 50-NLOC gate (#846).
fn source_type_for_device_index(selected: u32) -> Option<SourceType> {
    match selected {
        DEVICE_RTLSDR => Some(SourceType::RtlSdr),
        DEVICE_NETWORK => Some(SourceType::Network),
        DEVICE_FILE => Some(SourceType::File),
        DEVICE_RTLTCP => Some(SourceType::RtlTcp),
        sidebar::source_panel::DEVICE_AIRSPY => Some(SourceType::Airspy),
        _ => None,
    }
}

/// Source-type selector guard against transient out-of-range indices.
/// Split out per the 50-NLOC gate (#817).
pub(super) fn wire_source_type_guard(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    toast_overlay: &adw::ToastOverlay,
    server_running: Rc<std::cell::Cell<bool>>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Source type selector — guard against transient out-of-range
    // indices AND enforce mutual exclusivity with the rtl_tcp server
    // (the dongle can only serve one master; re-selecting RTL-SDR
    // while the server's accept thread has the USB device would
    // trigger a double-open at the next Play).
    let state_source = Rc::clone(state);
    let toast_overlay_weak = toast_overlay.downgrade();
    // Last-known legal selection. Seeded from the current row state
    // so the revert path on first illegal transition lands on the
    // value the UI already shows. Updated every time the guard
    // accepts a new selection.
    let last_legal_selection: Rc<std::cell::Cell<u32>> =
        Rc::new(std::cell::Cell::new(panels.source.device_row.selected()));
    // Re-entry guard against our own `set_selected` (the revert).
    // Without it the revert would re-enter this handler, see the
    // previous illegal value as "new", and endlessly toggle.
    let reverting: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
    let sample_rate_row_for_device = panels.source.sample_rate_row.downgrade();
    let gain_row_for_device = panels.source.gain_row.downgrade();
    let config_for_device_rates = std::sync::Arc::clone(config);
    panels
        .source
        .device_row
        .connect_selected_notify(move |row| {
            if reverting.get() {
                // Our own revert fired this notify — drop it.
                return;
            }
            let selected = row.selected();
            if revert_device_row_if_server_running(
                row,
                selected,
                &server_running,
                &toast_overlay_weak,
                &reverting,
                &last_legal_selection,
            ) {
                return;
            }
            let Some(source_type) = source_type_for_device_index(selected) else {
                return; // ignore transient indices
            };
            last_legal_selection.set(selected);
            state_source.send_dsp(UiToDsp::SetSourceType(source_type));
            reset_device_scoped_state(selected, &state_source);
            reseed_rate_and_gain_rows_for_device(
                selected,
                &state_source,
                &config_for_device_rates,
                &gain_row_for_device,
                &sample_rate_row_for_device,
            );
        });
}

/// Swap the rate combo to the new device's table and re-seed a
/// legal selection: the persisted index when it fits, else the
/// table's first entry. Also applies the new device's gain-row
/// bounds. The dispatch keeps the DSP's configured rate in the new
/// device's range so the next Play doesn't need the start()-side
/// clamp. Per #848. Split out of `wire_source_type_guard` per the
/// 50-NLOC gate (#846).
fn reseed_rate_and_gain_rows_for_device(
    selected: u32,
    state_source: &Rc<AppState>,
    config_for_device_rates: &std::sync::Arc<sdr_config::ConfigManager>,
    gain_row_for_device: &glib::WeakRef<adw::SpinRow>,
    sample_rate_row_for_device: &glib::WeakRef<adw::ComboRow>,
) {
    if let Some(gain_row) = gain_row_for_device.upgrade() {
        sidebar::source_panel::apply_device_gain_row(&gain_row, selected);
    }
    if let Some(rate_row) = sample_rate_row_for_device.upgrade() {
        sidebar::source_panel::repopulate_sample_rate_model(&rate_row, selected);
        let rates = sidebar::source_panel::sample_rates_for_device(selected);
        let persisted =
            sidebar::source_panel::load_source_sample_rate_index(config_for_device_rates);
        let idx = if (persisted as usize) < rates.len() {
            persisted
        } else {
            0
        };
        rate_row.set_selected(idx);
        if let Some(&rate) = rates.get(idx as usize) {
            state_source.send_dsp(UiToDsp::SetSampleRate(rate));
        }
    }
}

/// Direct-sampling combo + offset-tuning toggle (#538/#539).
/// Split out per the 50-NLOC gate (#817).
fn wire_sampling_mode_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    toast_overlay: &adw::ToastOverlay,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Direct sampling combo (#538). Same restore-then-wire idiom
    // as bias-T above. The persisted value is the combo index
    // (0/1/2), which is also the `rtlsdr_set_direct_sampling`
    // mode argument — cast straight to `i32` for the dispatch.
    {
        let persisted = sidebar::source_panel::load_source_rtl_direct_sampling_mode(config);
        if persisted <= sidebar::source_panel::DIRECT_SAMPLING_MAX_IDX {
            panels.source.direct_sampling_row.set_selected(persisted);
            #[allow(clippy::cast_possible_wrap, reason = "u32 <= 2 fits in i32 trivially")]
            state.send_dsp(UiToDsp::SetDirectSampling(persisted as i32));
        }
    }
    let state_direct = Rc::clone(state);
    let config_direct = std::sync::Arc::clone(config);
    let toast_overlay_direct = toast_overlay.downgrade();
    panels
        .source
        .direct_sampling_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting (mirrors the
            // protocol_row / sample-rate / device / decimation
            // early-return-on-invalid pattern). GTK can briefly
            // emit out-of-range values during widget-model
            // churn; persisting them would leave the next
            // restart pinned to a non-existent direct-sampling
            // mode. Per `CodeRabbit` round 3 on PR #558.
            if idx > sidebar::source_panel::DIRECT_SAMPLING_MAX_IDX {
                return;
            }
            sidebar::source_panel::save_source_rtl_direct_sampling_mode(&config_direct, idx);
            #[allow(clippy::cast_possible_wrap, reason = "idx <= 2 fits in i32 trivially")]
            state_direct.send_dsp(UiToDsp::SetDirectSampling(idx as i32));
            // Surface a tune-guidance toast: enabling direct
            // sampling routes the antenna straight to the ADC,
            // which silences VHF/UHF (the R820T tuner is now
            // bypassed); disabling it puts the tuner back in
            // path, which silences HF. Either direction needs a
            // manual retune to be useful, and a toast saves the
            // user from staring at noise wondering why. Per
            // `CodeRabbit` round 1 on PR #559 / closes #538
            // objective.
            if let Some(overlay) = toast_overlay_direct.upgrade() {
                let msg = if idx == sidebar::source_panel::DIRECT_SAMPLING_DISABLED_IDX {
                    "Direct Sampling off — retune to VHF/UHF."
                } else {
                    // No `<` here: `adw::Toast` titles are Pango markup and
                    // "(< 28 MHz)" failed to parse (GTK-WARNING, blank toast).
                    "Direct Sampling on — retune to an HF frequency (below 28 MHz)."
                };
                overlay.add_toast(plain_toast(msg));
            }
        });

    wire_offset_tuning_toggle(panels, state, config);
}

/// IQ inversion toggle + decimation selector (restore-then-wire).
/// Split out per the 50-NLOC gate (#817).
fn wire_iq_inversion_and_decimation(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // IQ inversion toggle. Restore-then-wire (#552).
    {
        let persisted = sidebar::source_panel::load_source_iq_inversion(config);
        panels.source.iq_inversion_row.set_active(persisted);
        state.send_dsp(UiToDsp::SetIqInversion(persisted));
    }
    let state_iq_inv = Rc::clone(state);
    let config_iq_inv = std::sync::Arc::clone(config);
    panels
        .source
        .iq_inversion_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_iq_inversion(&config_iq_inv, enabled);
            state_iq_inv.send_dsp(UiToDsp::SetIqInversion(enabled));
        });

    // Decimation selector. Restore-then-wire (#552). The
    // decimation index also feeds the bandwidth-advisory
    // recompute via `apply_source_bandwidth_advisory`, so
    // restoring here BEFORE wiring keeps the advisory pristine
    // on first launch.
    {
        let persisted_idx = sidebar::source_panel::load_source_decimation_index(config);
        if (persisted_idx as usize) < DECIMATION_FACTORS.len() {
            panels.source.decimation_row.set_selected(persisted_idx);
            if let Some(&factor) = DECIMATION_FACTORS.get(persisted_idx as usize) {
                state.send_dsp(UiToDsp::SetDecimation(factor));
            }
        }
    }
    let state_decim = Rc::clone(state);
    let config_decim = std::sync::Arc::clone(config);
    panels
        .source
        .decimation_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting (same rationale as the
            // sample-rate row above). Per CodeRabbit round 1 on
            // PR #558.
            let Some(&factor) = DECIMATION_FACTORS.get(idx as usize) else {
                return;
            };
            sidebar::source_panel::save_source_decimation_index(&config_decim, idx);
            state_decim.send_dsp(UiToDsp::SetDecimation(factor));
        });
}

/// Source-type (device) selector (restore-then-wire, #552).
/// Split out per the 50-NLOC gate (#817).
fn wire_device_selector(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    apply_source_bandwidth_advisory: impl Fn() + 'static,
) {
    // Source-type (device) selector. Restore-then-wire (#552).
    // The restore SETs the row's selected index, which fires
    // `connect_selected_notify` and thus re-applies the bandwidth
    // advisory; that's intentional (it wires up the correct
    // visibility for the persisted source type at startup). The
    // source-type swap itself is handled by an UPSTREAM
    // `connect_selected_notify` (around the per-source-type
    // visibility block); this handler only wires the persistence
    // save + bandwidth-advisory refresh. The dedicated swap
    // dispatch lives at the end of `connect_source_panel`.
    {
        let persisted_idx = sidebar::source_panel::load_source_device_index(config);
        // Bound check via `DEVICE_AIRSPY` (the highest valid
        // index) — fails closed if a stale config carries an
        // out-of-range value (e.g. a future build added more
        // source types and the user rolled back).
        if persisted_idx <= sidebar::source_panel::DEVICE_AIRSPY {
            panels.source.device_row.set_selected(persisted_idx);
            // Dispatch the restored source type to the DSP so a
            // saved Network / File / RTL-TCP selection takes
            // effect at startup. The change-notify handler that
            // dispatches `SetSourceType` from user clicks is
            // wired AFTER this restore block runs, and even if it
            // were wired first, programmatic `set_selected` to a
            // value that already matches the row's default (0 =
            // RTL-SDR) wouldn't fire it. Explicit dispatch closes
            // both gaps. Per CodeRabbit round 1 on PR #558.
            let source_type = match persisted_idx {
                sidebar::source_panel::DEVICE_RTLSDR => Some(SourceType::RtlSdr),
                sidebar::source_panel::DEVICE_NETWORK => Some(SourceType::Network),
                sidebar::source_panel::DEVICE_FILE => Some(SourceType::File),
                sidebar::source_panel::DEVICE_RTLTCP => Some(SourceType::RtlTcp),
                sidebar::source_panel::DEVICE_AIRSPY => Some(SourceType::Airspy),
                _ => None,
            };
            if let Some(source_type) = source_type {
                state.send_dsp(UiToDsp::SetSourceType(source_type));
            }
        }
    }
    let config_device = std::sync::Arc::clone(config);
    let apply_on_device = apply_source_bandwidth_advisory;
    panels
        .source
        .device_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting (same rationale as the
            // sample-rate row above). `DEVICE_AIRSPY` is the
            // highest valid index. Per CodeRabbit round 1 on
            // PR #558; bound raised for Airspy per CR round 1 on
            // PR #852 (the stale RTLTCP bound silently dropped
            // the Airspy selection from config).
            if idx > sidebar::source_panel::DEVICE_AIRSPY {
                return;
            }
            sidebar::source_panel::save_source_device_index(&config_device, idx);
            apply_on_device();
        });
}

/// Offset-tuning toggle (#539), restore-then-wire; only a persisted true dispatches.
/// Split out per the 50-NLOC gate (#817).
fn wire_offset_tuning_toggle(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Offset tuning toggle (#539). Same restore-then-wire idiom
    // as bias-T above. The controller bridge
    // (`UiToDsp::SetOffsetTuning`) was already plumbed; only
    // wiring is new here.
    //
    // Only DISPATCH the persisted value when it's `true`. The
    // librtlsdr R820T-family branch returns `InvalidParameter`
    // for every `set_offset_tuning` call regardless of value —
    // dispatching `false` at startup (the default for users
    // who've never touched the toggle) generates a spurious
    // "Offset tuning failed" toast on the vast majority of
    // dongles. The driver default already matches `false`, so
    // skipping the dispatch is semantically a no-op. Per issue
    // #564.
    {
        let persisted = sidebar::source_panel::load_source_rtl_offset_tuning(config);
        panels.source.offset_tuning_row.set_active(persisted);
        if persisted {
            state.send_dsp(UiToDsp::SetOffsetTuning(true));
        }
    }
    let state_offset = Rc::clone(state);
    let config_offset = std::sync::Arc::clone(config);
    panels
        .source
        .offset_tuning_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_rtl_offset_tuning(&config_offset, enabled);
            state_offset.send_dsp(UiToDsp::SetOffsetTuning(enabled));
        });
}

/// Sample-rate selector, restore-then-wire (#552), advisory re-render on change.
/// Split out per the 50-NLOC gate (#817).
/// The rate combo's live index→Hz mapping: the device-reported list
/// (`SampleRateList` event) when one is active, else the static
/// per-device table. Per #848 phase 5.
fn current_rate_table(state: &Rc<AppState>, device: u32) -> Vec<f64> {
    state
        .live_source_rates
        .borrow()
        .clone()
        .unwrap_or_else(|| sidebar::source_panel::sample_rates_for_device(device).to_vec())
}

/// Per-device state reset on a source-type switch: the
/// device-reported rate list belonged to the previous source (back
/// to the static table until the next start reports the new
/// device's), and a switch to Airspy kicks off unit enumeration so
/// the serial combo fills without a manual nudge. Per #848 phase 5.
fn reset_device_scoped_state(selected: u32, state: &Rc<AppState>) {
    *state.live_source_rates.borrow_mut() = None;
    if selected == sidebar::source_panel::DEVICE_AIRSPY {
        state.send_dsp(UiToDsp::RefreshAirspyDevices);
    }
}

/// Restore-then-wire seed for the sample-rate selector: resolve the
/// persisted device (fail-closed to RTL-SDR), apply its gain-row
/// bounds, repopulate the rate combo's model, and dispatch the
/// persisted rate index when it's still in range. Split out of
/// `wire_sample_rate_selector` per the 50-NLOC gate (#846).
fn restore_sample_rate_selection(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // The rate table (and the combo's label model) depend on the
    // persisted DEVICE selection. Read it from config rather than
    // the widget: this block runs before `wire_device_selector`
    // restores the row, so `device_row.selected()` would still be
    // the construction default and a persisted Airspy selection
    // would start with the RTL rate model + an RTL rate dispatch.
    // Same fail-closed bound rule as the device restore itself.
    // Per CR round 1 on PR #850.
    let persisted_device = sidebar::source_panel::load_source_device_index(config);
    let device = if persisted_device <= sidebar::source_panel::DEVICE_AIRSPY {
        persisted_device
    } else {
        sidebar::source_panel::DEVICE_RTLSDR
    };
    sidebar::source_panel::apply_device_gain_row(&panels.source.gain_row, device);
    sidebar::source_panel::repopulate_sample_rate_model(&panels.source.sample_rate_row, device);
    let rates = sidebar::source_panel::sample_rates_for_device(device);
    let persisted_idx = sidebar::source_panel::load_source_sample_rate_index(config);
    if (persisted_idx as usize) < rates.len() {
        panels.source.sample_rate_row.set_selected(persisted_idx);
        if let Some(&rate) = rates.get(persisted_idx as usize) {
            state.send_dsp(UiToDsp::SetSampleRate(rate));
        }
    }
}

fn wire_sample_rate_selector(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    apply_source_bandwidth_advisory: impl Fn() + Clone + 'static,
) {
    // Sample rate selector. Restore-then-wire (#552).
    restore_sample_rate_selection(panels, state, config);
    let state_sr = Rc::clone(state);
    let config_sr = std::sync::Arc::clone(config);
    let device_row_for_rates = panels.source.device_row.downgrade();
    let apply_on_sr = apply_source_bandwidth_advisory.clone();
    panels
        .source
        .sample_rate_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting. GTK can briefly emit
            // out-of-range values during widget-model churn (e.g.
            // teardown / rebuild on style changes); persisting
            // those would corrupt the config file across restart.
            // Mirror the protocol_row pattern further down: bail
            // when the index doesn't map to a real sample rate.
            // Per CodeRabbit round 1 on PR #558.
            let device = device_row_for_rates
                .upgrade()
                .map_or(DEVICE_RTLSDR, |d| d.selected());
            let rates = current_rate_table(&state_sr, device);
            let Some(&rate) = rates.get(idx as usize) else {
                return;
            };
            sidebar::source_panel::save_source_sample_rate_index(&config_sr, idx);
            state_sr.send_dsp(UiToDsp::SetSampleRate(rate));
            apply_on_sr();
        });
    wire_device_selector(
        panels,
        state,
        config,
        apply_source_bandwidth_advisory.clone(),
    );
}

#[cfg(test)]
mod tests {
    use super::{
        DEVICE_FILE, DEVICE_NETWORK, DEVICE_RTLSDR, DEVICE_RTLTCP, SourceType, sidebar,
        source_type_for_device_index,
    };

    #[test]
    fn maps_every_known_device_index_to_its_source_type() {
        // Table test over the full index -> SourceType mapping,
        // including the out-of-range None case (transient GTK
        // ComboRow indices during widget-model churn).
        let device_airspy = sidebar::source_panel::DEVICE_AIRSPY;
        let cases = [
            (DEVICE_RTLSDR, Some(SourceType::RtlSdr)),
            (DEVICE_NETWORK, Some(SourceType::Network)),
            (DEVICE_FILE, Some(SourceType::File)),
            (DEVICE_RTLTCP, Some(SourceType::RtlTcp)),
            (device_airspy, Some(SourceType::Airspy)),
            (device_airspy + 1, None),
            (u32::MAX, None),
        ];
        for (index, expected) in cases {
            assert_eq!(
                source_type_for_device_index(index),
                expected,
                "index {index} mapped incorrectly"
            );
        }
    }
}
