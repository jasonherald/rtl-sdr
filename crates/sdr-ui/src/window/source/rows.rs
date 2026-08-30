//! Source-panel row wiring: device/rate/gain/AGC/network/`rtl_tcp`
//! client rows and their restore-then-wire startup sequences.
//! Split out of `window/source.rs` per the Codacy large-file
//! gate (#846).

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{
    AppState, DECIMATION_FACTORS, DEVICE_FILE, DEVICE_NETWORK, DEVICE_RTLSDR, DEVICE_RTLTCP,
    NETWORK_PROTOCOL_TCPCLIENT_IDX, NETWORK_PROTOCOL_UDP_IDX, Rc, RefCell, SAMPLE_RATES,
    SidebarPanels, SourceType, UiToDsp, adw, glib, plain_toast, recording_path, sidebar,
};
use super::FavoritesMap;
use super::connect::{
    invalidate_rtl_tcp_active_server_on_edit, persist_role_preference, restore_rtl_tcp_client_state,
};

/// Subtitle text shown on AGC-mutexed rows in the grayed-out
/// state so the reason for the lock is inline — without it, an
/// insensitive row is easy to mistake for a bug rather than
/// intentional behavior.
pub(super) const AGC_MUTEX_SUBTITLE: &str = "Disabled while AGC is on";

/// Enforce the tuner AGC ↔ manual gain mutual exclusion on the UI
/// side: when AGC is on, the gain spin row becomes insensitive
/// (grayed out, non-interactive). When AGC is off, the row is
/// fully editable.
///
/// The mutex exists because librtlsdr's `rtlsdr_set_tuner_gain`
/// silently no-ops when AGC mode is active on most RTL variants,
/// and on some oscillates between the manual target and the AGC
/// target in a loop that produces audible artifacts. Preventing
/// the user from editing the control while it would silently fail
/// is the discoverable fix (see #332). Bookmarks restore the full
/// tuning profile with AGC-first-then-gain ordering already, so
/// the restore path still updates `gain_row.set_value` cleanly
/// even when the row is insensitive — the value displays but the
/// user can't edit it until AGC is turned off.
pub(super) fn apply_agc_gain_mutex(gain_row: &adw::SpinRow, agc_active: bool) {
    gain_row.set_sensitive(!agc_active);
    gain_row.set_subtitle(if agc_active { AGC_MUTEX_SUBTITLE } else { "" });
}

/// Enforce the tuner AGC ↔ squelch mutual exclusion on the UI
/// side: when AGC is on, the squelch controls (manual enable,
/// manual level, auto-squelch enable) become insensitive.
///
/// The mutex exists because RTL-SDR's hardware tuner AGC auto-
/// normalizes the IF signal amplitude — the tuner's internal
/// VGA pushes toward a target level regardless of actual RF
/// input. `PowerSquelch` reads mean IF amplitude and gates
/// against a threshold, so with AGC on every signal (including
/// noise on an empty channel) looks like "above threshold" and
/// the gate stays open. Users see this as "all static all the
/// time" the moment they enable AGC while squelch is on.
///
/// Same UX pattern as `apply_agc_gain_mutex`: gray the rows,
/// set a subtitle on the first row explaining why, restore
/// sensitivity when AGC turns off. Both mutexes share the
/// `AGC_MUTEX_SUBTITLE` string so the explanation reads
/// identically across the panel.
pub(super) fn apply_agc_squelch_mutex(
    squelch_enabled_row: &adw::SwitchRow,
    squelch_level_row: &adw::SpinRow,
    auto_squelch_row: &adw::SwitchRow,
    agc_active: bool,
) {
    squelch_enabled_row.set_sensitive(!agc_active);
    squelch_level_row.set_sensitive(!agc_active);
    auto_squelch_row.set_sensitive(!agc_active);
    // Only one subtitle — the squelch-enabled row is the
    // "header" of this group in the Radio panel, so that's
    // where the explanation lands. The other two rows stay
    // grayed without extra text to avoid repeating the
    // message three times in a row.
    squelch_enabled_row.set_subtitle(if agc_active { AGC_MUTEX_SUBTITLE } else { "" });
}

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

/// Gain row + AGC type selector with mutex + restore ordering (#551).
/// Split out per the 50-NLOC gate (#817).
pub(super) fn wire_gain_and_agc_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Gain control. Sensitivity is gated by AGC — see the `AGC
    // toggle` handler below and `apply_agc_gain_mutex` for the
    // reasoning (librtlsdr silently ignores gain writes when
    // tuner AGC is on; some variants also oscillate between
    // manual and AGC targets on mixed writes).
    //
    // The notify handler checks the AGC state and skips the
    // DSP dispatch when AGC is not Off. `set_sensitive(false)`
    // blocks user interaction but does NOT suppress the notify
    // signal on programmatic `set_value` calls (bookmark
    // restore, future preset-apply paths, etc.), so a pure-
    // sensitivity gate would still let a stream of no-op
    // `SetGain` commands hit the DSP every time a non-Off-AGC
    // bookmark loads. The AGC-state check short-circuits those
    // at the source — both hardware and software AGC
    // renormalize the signal, so any gain write during those
    // modes is discarded downstream anyway.
    // Restore persisted manual gain BEFORE wiring the notify
    // handler — otherwise the programmatic `set_value` fires
    // `connect_value_notify` and the in-flight `set_value`
    // re-dispatches with the freshly-loaded value redundantly.
    // Same idiom as the bias-T restore. Per #551.
    {
        let persisted_gain = sidebar::source_panel::load_source_rtl_gain_db(config);
        panels.source.gain_row.set_value(persisted_gain);
        state.send_dsp(UiToDsp::SetGain(persisted_gain));
    }
    let state_gain = Rc::clone(state);
    let agc_row_for_gain = panels.source.agc_row.downgrade();
    let config_gain = std::sync::Arc::clone(config);
    panels.source.gain_row.connect_value_notify(move |row| {
        // Persist the slider value even when AGC is on — the
        // user's last manual gain should survive an AGC-on /
        // restart / AGC-off cycle. Per #551.
        sidebar::source_panel::save_source_rtl_gain_db(&config_gain, row.value());
        if let Some(agc_row) = agc_row_for_gain.upgrade() {
            let agc_type = sidebar::source_panel::agc_type_from_selected(agc_row.selected());
            if !matches!(agc_type, Some(sidebar::source_panel::AgcType::Off)) {
                return;
            }
        }
        state_gain.send_dsp(UiToDsp::SetGain(row.value()));
    });

    wire_agc_type_selector(panels, state, config);
}

/// `rtl_tcp` client role + auth-key rows with last-good-bytes cache and per-server restore (#396).
/// Split out per the 50-NLOC gate (#817).
pub(super) fn wire_rtl_tcp_client_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    last_good_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
) {
    restore_rtl_tcp_client_state(panels, state, config, last_good_auth_key);
}

/// IQ correction, PPM, and the stop/start buttons.
/// Split out per the 50-NLOC gate (#817).
pub(super) fn wire_iq_ppm_and_restart_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // IQ correction toggle. Restore-then-wire (#552).
    {
        let persisted = sidebar::source_panel::load_source_iq_correction(config);
        panels.source.iq_correction_row.set_active(persisted);
        state.send_dsp(UiToDsp::SetIqCorrection(persisted));
    }
    let state_iq_corr = Rc::clone(state);
    let config_iq_corr = std::sync::Arc::clone(config);
    panels
        .source
        .iq_correction_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_iq_correction(&config_iq_corr, enabled);
            state_iq_corr.send_dsp(UiToDsp::SetIqCorrection(enabled));
        });

    // PPM correction. Restore persisted value before wiring
    // the notify handler — same idiom as bias-T / gain. Per
    // #551.
    {
        let persisted_ppm = sidebar::source_panel::load_source_rtl_ppm(config);
        panels.source.ppm_row.set_value(f64::from(persisted_ppm));
        state.send_dsp(UiToDsp::SetPpmCorrection(persisted_ppm));
    }
    let state_ppm = Rc::clone(state);
    let config_ppm = std::sync::Arc::clone(config);
    panels.source.ppm_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation)]
        let ppm = row.value() as i32;
        sidebar::source_panel::save_source_rtl_ppm(&config_ppm, ppm);
        state_ppm.send_dsp(UiToDsp::SetPpmCorrection(ppm));
    });

    // rtl_tcp connection controls — Disconnect + Retry now.
    // Both route to the DSP controller which owns the active
    // Source and performs the stop/start teardown. Buttons are
    // sensitive-gated by the state-change handler in
    // `handle_dsp_message`, so clicks should only ever reach here
    // on legal transitions.
    let state_disconnect = Rc::clone(state);
    panels
        .source
        .rtl_tcp_disconnect_button
        .connect_clicked(move |_| {
            state_disconnect.send_dsp(UiToDsp::DisconnectRtlTcp);
        });
    let state_retry = Rc::clone(state);
    panels
        .source
        .rtl_tcp_retry_button
        .connect_clicked(move |_| {
            state_retry.send_dsp(UiToDsp::RetryRtlTcpNow);
        });
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
            // Exclusivity guard: can't re-enter the local-source
            // world while the rtl_tcp server has the dongle claimed.
            if selected == DEVICE_RTLSDR && server_running.get() {
                if let Some(overlay) = toast_overlay_weak.upgrade() {
                    overlay.add_toast(plain_toast(
                        "Stop the network server first before switching to local RTL-SDR.",
                    ));
                }
                reverting.set(true);
                row.set_selected(last_legal_selection.get());
                reverting.set(false);
                return;
            }
            let source_type = match selected {
                DEVICE_RTLSDR => SourceType::RtlSdr,
                DEVICE_NETWORK => SourceType::Network,
                DEVICE_FILE => SourceType::File,
                DEVICE_RTLTCP => SourceType::RtlTcp,
                sidebar::source_panel::DEVICE_AIRSPY => SourceType::Airspy,
                _ => return, // ignore transient indices
            };
            last_legal_selection.set(selected);
            state_source.send_dsp(UiToDsp::SetSourceType(source_type));
            reset_device_scoped_state(selected, &state_source);
            // Swap the rate combo to the device's table and re-seed
            // a legal selection: the persisted index when it fits,
            // else the table's first entry. The dispatch keeps the
            // DSP's configured rate in the new device's range so the
            // next Play doesn't need the start()-side clamp. Per
            // #848.
            if let Some(gain_row) = gain_row_for_device.upgrade() {
                sidebar::source_panel::apply_device_gain_row(&gain_row, selected);
            }
            if let Some(rate_row) = sample_rate_row_for_device.upgrade() {
                sidebar::source_panel::repopulate_sample_rate_model(&rate_row, selected);
                let rates = sidebar::source_panel::sample_rates_for_device(selected);
                let persisted =
                    sidebar::source_panel::load_source_sample_rate_index(&config_for_device_rates);
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
        });
}

/// Airspy unit selector (#848 phase 5): restore-then-wire. The
/// persisted serial is dispatched at startup so the next Play opens
/// the chosen unit even before the combo has real entries; the
/// enumeration answer (`AirspyDeviceList`) later rebuilds the combo
/// and re-selects it. Selection changes persist the serial and
/// dispatch it — taking effect at the next Play, matching how the
/// RTL/Airspy device switch itself behaves.
pub(super) fn wire_airspy_device_row(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    {
        let persisted = sidebar::source_panel::load_airspy_serial(config);
        if persisted.is_some() {
            state.send_dsp(UiToDsp::SetAirspyDeviceSerial(persisted));
        }
        // Enumerate at startup when the panel restores to Airspy so
        // the combo has real entries without a device-switch nudge.
        let device = sidebar::source_panel::load_source_device_index(config);
        if device == sidebar::source_panel::DEVICE_AIRSPY {
            state.send_dsp(UiToDsp::RefreshAirspyDevices);
        }
    }
    let state_serial = Rc::clone(state);
    let config_serial = std::sync::Arc::clone(config);
    panels
        .source
        .airspy_device_row
        .connect_selected_notify(move |row| {
            // Programmatic rebuilds from the device-list event must
            // not round-trip into config writes — see
            // `AppState::suppress_airspy_unit_notify`.
            if state_serial.suppress_airspy_unit_notify.get() {
                return;
            }
            let idx = row.selected();
            // "First available" vs an enumerated serial parsed back
            // from its label. A transient out-of-range index during
            // model churn parses to None and is discarded.
            let serial = if idx == sidebar::source_panel::AIRSPY_FIRST_AVAILABLE_INDEX {
                None
            } else {
                let Some(label) = row
                    .model()
                    .and_then(|m| m.downcast::<gtk4::StringList>().ok())
                    .and_then(|m| m.string(idx))
                else {
                    return;
                };
                let Some(serial) = sdr_source_airspy::parse_device_serial(&label) else {
                    return;
                };
                Some(serial)
            };
            sidebar::source_panel::save_airspy_serial(&config_serial, serial);
            state_serial.send_dsp(UiToDsp::SetAirspyDeviceSerial(serial));
        });
}

/// Raw-Network hostname/port/protocol rows (atomic restore, per-edit dispatch).
/// Split out per the 50-NLOC gate (#817).
pub(super) fn wire_network_source_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Raw-Network source config (hostname / port / protocol).
    // Restore all three widgets atomically BEFORE wiring the
    // change-notify handlers, then dispatch one
    // `SetNetworkConfig` with the loaded values so Play picks up
    // the right destination on first launch. Per #552. (rtl_tcp
    // client maintains its own per-server hostname/port via the
    // favorites list — these keys are for the raw IQ-stream
    // Network source only; on a launch where the user was last
    // on rtl_tcp the favorites system also restores its own
    // hostname/port and the two are independent.)
    {
        let hostname = sidebar::source_panel::load_source_network_hostname(config);
        let port = sidebar::source_panel::load_source_network_port(config);
        let protocol_idx = sidebar::source_panel::load_source_network_protocol_index(config);
        panels.source.hostname_row.set_text(&hostname);
        panels.source.port_row.set_value(f64::from(port));
        if protocol_idx == NETWORK_PROTOCOL_UDP_IDX
            || protocol_idx == NETWORK_PROTOCOL_TCPCLIENT_IDX
        {
            panels.source.protocol_row.set_selected(protocol_idx);
        }
        let protocol = if protocol_idx == NETWORK_PROTOCOL_UDP_IDX {
            sdr_types::Protocol::Udp
        } else {
            sdr_types::Protocol::TcpClient
        };
        state.send_dsp(UiToDsp::SetNetworkConfig {
            hostname,
            port,
            protocol,
        });
    }

    wire_network_hostname_row(panels, state, config);
}

/// Connection-role picker + server-key entry (#394/#396).
/// Split out per the 50-NLOC gate (#817).
pub(super) fn wire_role_and_server_key_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    favorites: &FavoritesMap,
    last_good_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
) {
    // Connection-role picker (#396). The selector flips between
    // `Role::Control` (index 0) and `Role::Listen` (index 1); we
    // dispatch a fresh `SetRtlTcpClientConfig` with the new role
    // plus the current auth key (unchanged by a role flip). The
    // role takes effect on the NEXT connect — already-running
    // sessions keep their admitted role because the wire
    // protocol ties role to the hello and doesn't support
    // mid-stream role changes. Persistence has two tiers:
    //
    // - Global `KEY_RTL_TCP_CLIENT_LAST_ROLE` — fallback default
    //   for NEW servers that haven't been favorited yet. The
    //   Connect-from-discovery path reads this to seed the
    //   picker before the user has expressed a per-server
    //   preference. Pre-CodeRabbit round 1 on PR #408 this was
    //   the ONLY persistence tier, which meant changing
    //   Server B's role clobbered Server A's preference.
    // - Per-favorite `FavoriteEntry.requested_role` — wins for
    //   favorited servers. When the current server identity
    //   matches a favorite key, update that entry's role and
    //   save_favorites so the next connect from this favorite
    //   restores the right picker state without touching other
    //   servers.
    let state_role = Rc::clone(state);
    let auth_key_for_role = panels.source.rtl_tcp_auth_key_row.clone();
    let config_for_role = std::sync::Arc::clone(config);
    let hostname_for_role = panels.source.hostname_row.clone();
    let port_for_role = panels.source.port_row.clone();
    let favorites_for_role = Rc::clone(favorites);
    let last_good_for_role = Rc::clone(last_good_auth_key);
    panels
        .source
        .rtl_tcp_role_row
        .connect_selected_notify(move |row| {
            on_rtl_tcp_role_selected(
                row,
                &state_role,
                &auth_key_for_role,
                &config_for_role,
                &hostname_for_role,
                &port_for_role,
                &favorites_for_role,
                &last_good_for_role,
            );
        });

    wire_server_key_entry(panels, state, config, last_good_auth_key);
}

/// Role-picker dispatch (#396): resolve the auth key (empty → None,
/// valid hex → bytes, malformed → last-good cache), push the new role
/// to DSP, then persist the two-tier role preference (global default
/// + per-favorite override). Split out per the 50-NLOC gate (#817).
#[allow(clippy::too_many_arguments)]
fn on_rtl_tcp_role_selected(
    row: &adw::ComboRow,
    state_role: &Rc<AppState>,
    auth_key_for_role: &adw::PasswordEntryRow,
    config_for_role: &std::sync::Arc<sdr_config::ConfigManager>,
    hostname_for_role: &adw::EntryRow,
    port_for_role: &adw::SpinRow,
    favorites_for_role: &FavoritesMap,
    last_good_for_role: &Rc<RefCell<Option<Vec<u8>>>>,
) {
    use crate::sidebar::source_panel::{
        FavoriteRole, RTL_TCP_ROLE_CONTROL_IDX, RTL_TCP_ROLE_LISTEN_IDX,
    };
    let fav_role = match row.selected() {
        RTL_TCP_ROLE_CONTROL_IDX => FavoriteRole::Control,
        RTL_TCP_ROLE_LISTEN_IDX => FavoriteRole::Listen,
        _ => return, // transient out-of-range indices
    };
    let requested_role = fav_role.as_wire_role();
    // Resolve the auth_key for this dispatch:
    // - Empty text → `None` (intentional clear).
    // - Valid hex → `Some(bytes)`.
    // - Malformed non-empty text → the cached last-good
    //   bytes (which the auth handler maintains). This
    //   means a role flip with bad hex in the auth field
    //   still pushes the new role to DSP — pre-
    //   `CodeRabbit` round 9 on PR #408 we'd skip the
    //   dispatch entirely, so a user could switch to
    //   Listener, hit Retry / ControllerBusy-toast-
    //   Takeover, and still end up as Controller because
    //   DSP never saw the new role. The auth_key-row
    //   handler still drives the `error` CSS class on
    //   the row so the user sees the malformed input.
    let key_text = auth_key_for_role.text().to_string();
    let auth_key: Option<Vec<u8>> = if key_text.is_empty() {
        None
    } else if let Some(bytes) = crate::sidebar::server_panel::auth_key_from_hex(&key_text) {
        Some(bytes)
    } else {
        last_good_for_role.borrow().clone()
    };
    state_role.send_dsp(UiToDsp::SetRtlTcpClientConfig {
        requested_role,
        auth_key,
    });
    persist_role_preference(
        state_role,
        config_for_role,
        hostname_for_role,
        port_for_role,
        favorites_for_role,
        fav_role,
    );
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

/// Network port + protocol rows.
/// Split out per the 50-NLOC gate (#817).
fn wire_network_port_and_protocol(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Network port
    let state_port = Rc::clone(state);
    let config_port = std::sync::Arc::clone(config);
    let host_for_port = panels.source.hostname_row.clone();
    let proto_for_port = panels.source.protocol_row.clone();
    let port_row_for_port = panels.source.port_row.clone();
    let auth_key_for_port = panels.source.rtl_tcp_auth_key_row.clone();
    panels.source.port_row.connect_value_notify(move |row| {
        // Skip the invalidation during RTL-TCP hydration; see
        // hostname handler above for the rationale. Per
        // `CodeRabbit` round 3 on PR #558.
        if !state_port.rtl_tcp_hydration_in_progress.get() {
            invalidate_rtl_tcp_active_server_on_edit(
                &state_port,
                &host_for_port,
                &port_row_for_port,
                &auth_key_for_port,
            );
        }
        let hostname = host_for_port.text().to_string();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let port = row.value() as u16;
        // Skip the raw-Network disk-write during RTL-TCP
        // hydration; see hostname handler above. Per CodeRabbit
        // round 1 on PR #558.
        if !state_port.rtl_tcp_hydration_in_progress.get() {
            sidebar::source_panel::save_source_network_port(&config_port, port);
        }
        let protocol = if proto_for_port.selected() == NETWORK_PROTOCOL_UDP_IDX {
            sdr_types::Protocol::Udp
        } else {
            sdr_types::Protocol::TcpClient
        };
        // Suppress per-edit dispatch during hydration; see
        // hostname handler above. Per `CodeRabbit` round 2 on
        // PR #558.
        if !state_port.rtl_tcp_hydration_in_progress.get() {
            state_port.send_dsp(UiToDsp::SetNetworkConfig {
                hostname,
                port,
                protocol,
            });
        }
    });

    wire_network_protocol_row(panels, state, config);
}

/// Server key entry (#394/#396): per-edit config rebuild + last-good cache.
/// Split out per the 50-NLOC gate (#817).
fn wire_server_key_entry(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    last_good_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
) {
    // Server key entry (#394 + #396). On every edit we rebuild
    // the `SetRtlTcpClientConfig` message with the current role
    // + the new key bytes, so the NEXT connect carries the
    // latest value. The entry accepts hex input (matching what
    // `openssl rand -hex 32` produces and what the server UI's
    // Copy button writes to the clipboard); an empty field
    // clears the key (`auth_key: None`). The key is also saved
    // to the per-server keyring on a successful auth-required
    // connect (wired in the toast-flow commit) — this handler
    // only threads the current-session value through to the
    // DSP.
    let state_auth = Rc::clone(state);
    let role_for_auth = panels.source.rtl_tcp_role_row.clone();
    let last_good_for_auth = Rc::clone(last_good_auth_key);
    panels
        .source
        .rtl_tcp_auth_key_row
        .connect_changed(move |row| {
            use crate::sidebar::source_panel::{
                FavoriteRole, RTL_TCP_ROLE_CONTROL_IDX, RTL_TCP_ROLE_LISTEN_IDX,
            };
            // Transient out-of-range indices on `ComboRow` can
            // occur during widget teardown; fall back to the
            // legacy-safe `Control` default in that case (same
            // treatment the role_row handler gives with an
            // `early return`, but auth_key edits happen often
            // enough that swallowing one rare transient is
            // fine).
            #[allow(
                clippy::match_same_arms,
                reason = "explicit catch-all matches the Control default"
            )]
            let fav_role = match role_for_auth.selected() {
                RTL_TCP_ROLE_CONTROL_IDX => FavoriteRole::Control,
                RTL_TCP_ROLE_LISTEN_IDX => FavoriteRole::Listen,
                _ => FavoriteRole::Control,
            };
            let text = row.text().to_string();
            // Malformed hex must NOT collapse to `auth_key: None`.
            // Pre-`CodeRabbit` round 7 on PR #408 a bad paste fell
            // into the `auth_key_from_hex(..) -> None` branch and
            // silently cleared DSP auth state — the next Retry /
            // Play would then dispatch an unauthenticated connect,
            // bounce through `AuthRequired`, and the user had to
            // fix the text before realizing the previous saved key
            // had been clobbered. Three cases now:
            //
            // - Empty text: intentional clear. Drop the error
            //   class, dispatch `auth_key: None`, cache `None`.
            // - Valid hex: parsed bytes. Drop the error class,
            //   dispatch `Some(bytes)`, cache `Some(bytes)`.
            // - Malformed non-empty text: add the libadwaita
            //   `error` CSS class so the row reads as invalid,
            //   and RETURN without dispatching or updating the
            //   cache — keeping DSP's last-good auth state
            //   (and the `last_good_auth_key` cache the role
            //   handler reads from) intact until the user
            //   either fixes the text or clears the field.
            //
            // `auth_key_from_hex` treats empty as `None` too, but
            // we handle the empty branch explicitly above so the
            // malformed case is cleanly separable.
            let auth_key: Option<Vec<u8>> = if text.is_empty() {
                row.remove_css_class("error");
                None
            } else if let Some(bytes) = crate::sidebar::server_panel::auth_key_from_hex(&text) {
                row.remove_css_class("error");
                Some(bytes)
            } else {
                row.add_css_class("error");
                return;
            };
            // Update the last-good cache alongside the dispatch
            // so the role handler's fallback path (malformed
            // hex at role-flip time) has a coherent value to
            // dispatch. See `last_good_auth_key` declaration
            // above. Per `CodeRabbit` round 9 on PR #408.
            last_good_for_auth.borrow_mut().clone_from(&auth_key);
            state_auth.send_dsp(UiToDsp::SetRtlTcpClientConfig {
                requested_role: fav_role.as_wire_role(),
                auth_key,
            });
        });

    wire_file_path_row(panels, state, config);
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

/// AGC type selector (Off/Hardware/Software) with restore ordering.
/// Split out per the 50-NLOC gate (#817).
fn wire_agc_type_selector(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // AGC type selector (Off / Hardware / Software). Dispatches
    // the right `UiToDsp::SetAgc` / `UiToDsp::SetSoftwareAgc`
    // pair on every selection and also fires two mutexes so
    // the UI doesn't lie about controls that EITHER AGC type
    // disables:
    //
    // 1. Gain row — `rtlsdr_set_tuner_gain` silently no-ops on
    //    most RTL variants when hardware AGC is on; software
    //    AGC makes manual gain pointless because the DSP stage
    //    would renormalize it immediately.
    // 2. Squelch rows — both AGC types auto-normalize IF
    //    amplitude, so amplitude-based squelch can't distinguish
    //    signal from noise and the gate just stays open. Without
    //    this mutex users see "all static all the time" the
    //    moment they enable AGC with squelch on.
    //
    wire_agc_notify_handler(panels, state, config);
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

/// AGC notify handler (registered before the restore so the seed dispatches).
/// Split out per the 50-NLOC gate (#817).
fn wire_agc_notify_handler(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Register the AGC notify handler BEFORE restoring the
    // persisted selection. `set_selected` only fires
    // `selected-notify` when the new index differs from the
    // current one, so the startup-restore path relies on the
    // handler being registered first to dispatch the persisted
    // mode. Without this ordering, fresh installs (persisted
    // matches build-time default) or config match would leave
    // DSP stuck in its all-off default state until the user
    // touched the selector.
    //
    // Handler drops transient out-of-range indices —
    // `agc_type_from_selected` now returns `Option<AgcType>`
    // and we early-return on `None` rather than coercing them
    // to a fallback and persisting a bogus config write during
    // widget-teardown churn.
    let state_agc = Rc::clone(state);
    let config_for_agc = std::sync::Arc::clone(config);
    let gain_row_for_agc = panels.source.gain_row.clone();
    let squelch_enabled_for_agc = panels.radio.squelch_enabled_row.clone();
    let squelch_level_for_agc = panels.radio.squelch_level_row.clone();
    let auto_squelch_for_agc = panels.radio.auto_squelch_row.clone();
    panels.source.agc_row.connect_selected_notify(move |row| {
        let Some(agc_type) = sidebar::source_panel::agc_type_from_selected(row.selected()) else {
            // Transient GTK value (e.g., `INVALID_LIST_POSITION`
            // during model swap). Skip dispatch AND persistence
            // — we'll pick up the next real selection from the
            // follow-up notify event.
            tracing::trace!(
                selected = row.selected(),
                "AGC combo notify with out-of-range index, ignoring"
            );
            return;
        };

        // Dispatch both messages every time so exactly one
        // enable path is active and the other is cleanly off.
        // The engine treats hardware and software AGC as
        // independent flags; the UI is the policy layer that
        // mutually excludes them.
        let (hw, sw) = match agc_type {
            sidebar::source_panel::AgcType::Off => (false, false),
            sidebar::source_panel::AgcType::Hardware => (true, false),
            sidebar::source_panel::AgcType::Software => (false, true),
        };
        state_agc.send_dsp(UiToDsp::SetAgc(hw));
        state_agc.send_dsp(UiToDsp::SetSoftwareAgc(sw));

        // Persist the new selection so the choice sticks
        // across restarts. Cheap — `ConfigManager::write` is an
        // in-memory update with a debounced flush to disk.
        sidebar::source_panel::save_agc_type(&config_for_agc, agc_type);

        let agc_active = !matches!(agc_type, sidebar::source_panel::AgcType::Off);
        apply_agc_gain_mutex(&gain_row_for_agc, agc_active);
        apply_agc_squelch_mutex(
            &squelch_enabled_for_agc,
            &squelch_level_for_agc,
            &auto_squelch_for_agc,
            agc_active,
        );
    });

    restore_agc_type_selection(panels, state, config);
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

fn wire_sample_rate_selector(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    apply_source_bandwidth_advisory: impl Fn() + Clone + 'static,
) {
    // Sample rate selector. Restore-then-wire (#552).
    {
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

/// Network hostname — per-edit dispatch so Play always has the current value.
/// Split out per the 50-NLOC gate (#817).
fn wire_network_hostname_row(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Network hostname — send on every edit so Play always has current value
    let state_host = Rc::clone(state);
    let config_host = std::sync::Arc::clone(config);
    let port_for_host = panels.source.port_row.clone();
    let proto_for_host = panels.source.protocol_row.clone();
    let hostname_for_host = panels.source.hostname_row.clone();
    let auth_key_for_host = panels.source.rtl_tcp_auth_key_row.clone();
    panels.source.hostname_row.connect_changed(move |row| {
        // Invalidate the cached `rtl_tcp_active_server` when
        // the widget no longer matches the cached stable id
        // (typically a manual edit; harmless no-op for
        // `apply_rtl_tcp_connect`'s programmatic writes when
        // those match the cache). Per CodeRabbit round 4 on
        // PR #408.
        //
        // Skip the invalidation during RTL-TCP hydration: the
        // startup hydration in `connect_rtl_tcp_discovery`
        // rewrites this row from the last-connected RTL-TCP
        // server (only when the persisted source type is
        // RTL-TCP), and `apply_rtl_tcp_connect` writes the
        // cache *after* the row writes — so an unguarded
        // invalidate would clear the cache the hydration just
        // restored AND blank the auth row before the auth-row
        // handler had a chance to push the saved key. The
        // `apply_rtl_tcp_connect` path handles cache and auth
        // row deterministically itself; we just need to stay
        // out of its way here. Per `CodeRabbit` round 3 on PR
        // #558.
        if !state_host.rtl_tcp_hydration_in_progress.get() {
            invalidate_rtl_tcp_active_server_on_edit(
                &state_host,
                &hostname_for_host,
                &port_for_host,
                &auth_key_for_host,
            );
        }
        let hostname = row.text().to_string();
        // Skip the raw-Network disk-write when this change came
        // from an RTL-TCP hydration. The user's independent
        // raw-Network hostname stays in `KEY_SOURCE_NETWORK_*`
        // and round-trips across restart on its own. Per
        // CodeRabbit round 1 on PR #558.
        if !state_host.rtl_tcp_hydration_in_progress.get() {
            sidebar::source_panel::save_source_network_hostname(&config_host, &hostname);
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let port = port_for_host.value() as u16;
        let protocol = if proto_for_host.selected() == NETWORK_PROTOCOL_UDP_IDX {
            sdr_types::Protocol::Udp
        } else {
            sdr_types::Protocol::TcpClient
        };
        // Suppress per-edit `SetNetworkConfig` dispatch while a
        // hydration is rewriting all three rows in sequence. The
        // sequence would otherwise cause three intermediate
        // reconnect attempts (one per row), each against a
        // partially-rewritten triple. `apply_rtl_tcp_connect`
        // dispatches a single canonical `SetNetworkConfig` after
        // clearing the flag, so the final state still reaches
        // the DSP. Per `CodeRabbit` round 2 on PR #558.
        if !state_host.rtl_tcp_hydration_in_progress.get() {
            state_host.send_dsp(UiToDsp::SetNetworkConfig {
                hostname,
                port,
                protocol,
            });
        }
    });

    wire_network_port_and_protocol(panels, state, config);
}

/// Network protocol selector.
/// Split out per the 50-NLOC gate (#817).
fn wire_network_protocol_row(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Network protocol
    let state_proto = Rc::clone(state);
    let config_proto = std::sync::Arc::clone(config);
    let host_for_proto = panels.source.hostname_row.clone();
    let port_for_proto = panels.source.port_row.clone();
    panels
        .source
        .protocol_row
        .connect_selected_notify(move |row| {
            let hostname = host_for_proto.text().to_string();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let port = port_for_proto.value() as u16;
            let selected = row.selected();
            // Validate the selected index BEFORE persisting so a
            // transient out-of-range value during widget churn
            // can't land in config (matches the sample-rate /
            // device / decimation handlers' early-return pattern).
            // Per `CodeRabbit` round 3 on PR #558.
            let protocol = match selected {
                NETWORK_PROTOCOL_TCPCLIENT_IDX => sdr_types::Protocol::TcpClient,
                NETWORK_PROTOCOL_UDP_IDX => sdr_types::Protocol::Udp,
                _ => return, // ignore transient indices
            };
            // Skip the raw-Network disk-write during RTL-TCP
            // hydration; see hostname handler above. Per
            // `CodeRabbit` round 1 on PR #558.
            if !state_proto.rtl_tcp_hydration_in_progress.get() {
                sidebar::source_panel::save_source_network_protocol_index(&config_proto, selected);
            }
            // Suppress per-edit dispatch during hydration; see
            // hostname handler above. Per `CodeRabbit` round 2 on
            // PR #558.
            if !state_proto.rtl_tcp_hydration_in_progress.get() {
                state_proto.send_dsp(UiToDsp::SetNetworkConfig {
                    hostname,
                    port,
                    protocol,
                });
            }
        });
}

/// File path — per-edit dispatch.
/// Split out per the 50-NLOC gate (#817).
fn wire_file_path_row(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // File path — send on every edit so Play always has current
    // value. Restore-then-wire (#552). Empty saved string is the
    // default and means "no file selected" — re-set the widget
    // to empty too so the placeholder stays correct.
    {
        let persisted = sidebar::source_panel::load_source_file_path(config);
        panels.source.file_path_row.set_text(&persisted);
        state.send_dsp(UiToDsp::SetFilePath(std::path::PathBuf::from(&persisted)));
    }
    let state_file = Rc::clone(state);
    let config_file = std::sync::Arc::clone(config);
    panels.source.file_path_row.connect_changed(move |row| {
        let text = row.text().to_string();
        sidebar::source_panel::save_source_file_path(&config_file, &text);
        state_file.send_dsp(UiToDsp::SetFilePath(std::path::PathBuf::from(text)));
    });

    // IQ recording toggle
    let state_iq_rec = Rc::clone(state);
    panels
        .source
        .record_iq_row
        .connect_active_notify(move |row| {
            if row.is_active() {
                let path = recording_path("iq");
                tracing::info!(?path, "starting IQ recording");
                state_iq_rec.send_dsp(UiToDsp::StartIqRecording(path));
            } else {
                tracing::info!("stopping IQ recording");
                state_iq_rec.send_dsp(UiToDsp::StopIqRecording);
            }
        });
}

/// Persisted AGC-type restore (runs after the handler registration so the seed dispatches).
/// Split out per the 50-NLOC gate (#817).
fn restore_agc_type_selection(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Restore persisted AGC type from config now that the
    // notify handler is wired up. Two scenarios:
    //
    // 1. Persisted index differs from the combo's build-time
    //    default (Software) — `set_selected` fires
    //    `selected-notify`, the handler runs, DSP is
    //    dispatched, mutexes applied.
    // 2. Persisted index matches the default (fresh install
    //    or user previously selected Software) —
    //    `set_selected` is a no-op and `selected-notify`
    //    does NOT fire. We explicitly dispatch so DSP still
    //    gets the initial-state sync and mutexes are applied
    //    against the seeded selection.
    //
    // Both paths run the same dispatch logic; the explicit
    // post-`set_selected` call is idempotent with the notify
    // handler (both `SetAgc` and `SetSoftwareAgc` are
    // idempotent at the controller), so the double-dispatch
    // in scenario 1 is cheap and correct.
    {
        let persisted = sidebar::source_panel::load_agc_type(config);
        panels
            .source
            .agc_row
            .set_selected(sidebar::source_panel::selected_from_agc_type(persisted));

        let (hw, sw) = match persisted {
            sidebar::source_panel::AgcType::Off => (false, false),
            sidebar::source_panel::AgcType::Hardware => (true, false),
            sidebar::source_panel::AgcType::Software => (false, true),
        };
        state.send_dsp(UiToDsp::SetAgc(hw));
        state.send_dsp(UiToDsp::SetSoftwareAgc(sw));
        let agc_active = !matches!(persisted, sidebar::source_panel::AgcType::Off);
        apply_agc_gain_mutex(&panels.source.gain_row, agc_active);
        apply_agc_squelch_mutex(
            &panels.radio.squelch_enabled_row,
            &panels.radio.squelch_level_row,
            &panels.radio.auto_squelch_row,
            agc_active,
        );
    }
}
