//! Aviation (ACARS) panel wiring.

use gtk4::prelude::*;
use libadwaita::prelude::*;

mod viewer_rows;
use viewer_rows::render_acars_channel_row;
pub(in crate::window) use viewer_rows::{format_relative_age, try_collapse_into_existing};

use super::{AppState, Rc, adw, glib, sidebar};

/// Wire the Aviation sidebar panel: toggle switch → DSP, 4 Hz tick
/// for status/channel-row refresh, the open-viewer button, and the
/// output-formatter controls (station ID, JSONL log, network feeder).
pub(super) fn connect_aviation_panel(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    toast_overlay: &adw::ToastOverlay,
) {
    // ─── Region selector seed + signal (issue #581 / #592) ───
    wire_region_row(panel, state, config);
    wire_custom_channels_row(panel, state, config, toast_overlay);
    wire_acars_enable_switch(panel, state);
    wire_acars_status_tick(panel, state);
    wire_acars_output_rows(panel, state, config);
}

/// Region combo + custom-channels CSV row (seed-then-wire).
/// Split out per the 50-NLOC gate (#817).
fn wire_region_row(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    use crate::sidebar::aviation_panel::{rebuild_channel_rows, region_combo_index};

    // Read the persisted region, dispatch it to DSP at startup,
    // and seed the combo index BEFORE wiring the change handler
    // — otherwise the seed itself would fire a redundant
    // dispatch + persist round-trip.
    let initial_region = load_initial_region(config);
    panel
        .region_row
        .set_selected(region_combo_index(&initial_region));
    // Rebuild channel rows to match the seeded region's channel
    // count (predefined = 6; Custom = saved list len, possibly 0).
    rebuild_channel_rows(panel, initial_region.channels().len());
    // Hydrate the Custom EntryRow text from saved config so the
    // user sees their last entry next to the (now-visible) row.
    {
        let saved_chans = crate::acars_config::read_acars_custom_channels(config);
        if !saved_chans.is_empty() {
            let csv = saved_chans
                .iter()
                .map(|hz| format!("{:.3}", hz / 1_000_000.0))
                .collect::<Vec<_>>()
                .join(", ");
            panel.custom_channels_row.set_text(&csv);
        }
    }
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsRegion(initial_region));
    {
        let state = Rc::clone(state);
        let config = std::sync::Arc::clone(config);
        // Capture the panel widgets needed by the rebuild path.
        // Each widget already holds a strong ref into the GTK
        // tree, so cloning here is the same cheap GObject ref
        // bump the rest of `connect_aviation_panel` uses. The
        // `channel_rows` Rc is the SAME inner cell the 4 Hz
        // tick reads — this is what keeps that tick from
        // operating on a stale row snapshot.
        let channels_group = panel.channels_group.clone();
        let channel_rows_cell = std::rc::Rc::clone(&panel.channel_rows);
        let custom_row_for_dispatch = panel.custom_channels_row.clone();
        panel.region_row.connect_selected_notify(move |row| {
            on_acars_region_selected(
                row,
                &state,
                &config,
                &channels_group,
                &channel_rows_cell,
                &custom_row_for_dispatch,
            );
        });
    }

    // ─── Custom-channels apply handler (issue #592) ───
}

/// Resolve the persisted ACARS region from config. `"custom"` is a
/// two-key load: read channels, validate, then build `Custom` — or
/// fall back to the default region on bad/stale config. `Custom([])`
/// would reach the engage guard as invalid (`validate_custom_channels`
/// rejects empty), so it must never be dispatched at startup.
fn load_initial_region(
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) -> sdr_core::acars_airband_lock::AcarsRegion {
    use sdr_core::acars_airband_lock::{AcarsRegion, validate_custom_channels};

    let saved_region_id = crate::acars_config::read_acars_channel_set(config);
    if saved_region_id == "custom" {
        let saved_chans = crate::acars_config::read_acars_custom_channels(config);
        match validate_custom_channels(&saved_chans) {
            Ok(()) => AcarsRegion::Custom(saved_chans.into_boxed_slice()),
            Err(e) => {
                tracing::warn!(
                    "saved custom ACARS channels invalid ({e}); \
                     falling back to default region"
                );
                AcarsRegion::default()
            }
        }
    } else {
        AcarsRegion::from_config_id(saved_region_id.as_str())
    }
}

/// Region-combo notify body: resolve the region (Custom pulls the
/// saved channel list), persist + dispatch, and rebuild the channel
/// rows. Split out per the 50-NLOC gate (#817).
fn on_acars_region_selected(
    row: &adw::ComboRow,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    channels_group: &adw::PreferencesGroup,
    channel_rows_cell: &Rc<std::cell::RefCell<Vec<adw::ActionRow>>>,
    custom_row_for_dispatch: &adw::EntryRow,
) {
    use crate::sidebar::aviation_panel::{region_combo_index, region_from_combo_index};
    use sdr_core::acars_airband_lock::AcarsRegion;

    // Guard against transient ComboRow indices. The model
    // briefly emits selection notifications during
    // teardown / repopulate that don't correspond to a
    // real `REGION_OPTIONS` slot. If the round-trip
    // through `region_from_combo_index` lands on a
    // different index than the selector reported, the
    // value is transient — skip persistence + dispatch
    // so a UI quirk doesn't churn config or fire a
    // needless DSP command. CR round 1 on PR #593.
    let selected = row.selected();
    let mut region = region_from_combo_index(selected);
    if region_combo_index(&region) != selected {
        tracing::debug!(
            selected,
            "acars region combo emitted transient index; ignoring"
        );
        return;
    }
    // Custom slot: hydrate from saved config. With no saved
    // channels yet, dispatching `Custom([])` would be stored by
    // the (disabled) DSP side and then reject the next enable
    // request with `AcarsEnableError::ChannelBankInit` — so keep
    // the previous region live and only reveal the EntryRow; the
    // apply handler dispatches once a validated non-empty list
    // exists. Per CR round 1 on PR #844.
    if matches!(region, AcarsRegion::Custom(_)) {
        // Make the EntryRow visible immediately on
        // selection — the visibility-binding closure in
        // the panel builder also fires on this same
        // notify, but invoking the binding side-effect
        // here would require ordering guarantees we
        // don't have. Cheap idempotent set is fine.
        custom_row_for_dispatch.set_visible(true);
        let saved = crate::acars_config::read_acars_custom_channels(config);
        if saved.is_empty() {
            tracing::info!(
                "acars region: Custom selected with no saved channels — awaiting EntryRow apply"
            );
            return;
        }
        region = AcarsRegion::Custom(saved.into_boxed_slice());
    }
    crate::acars_config::save_acars_channel_set(config, region.config_id());
    // Rebuild channel rows to match the new region's
    // channel count — borrow_mut + adw mutation must
    // happen before send_dsp because the 4 Hz tick will
    // start consulting the new row count almost
    // immediately.
    let new_count = region.channels().len();
    // Inline the rebuild (the helper takes
    // `&AviationPanel`, but we only have the
    // individual fields here in the closure).
    crate::sidebar::aviation_panel::rebuild_channel_rows_parts(
        channels_group,
        channel_rows_cell,
        new_count,
    );
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsRegion(region));
}

/// ACARS enable switch + 4 Hz status/channel mirror tick.
/// Split out per the 50-NLOC gate (#817).
fn wire_acars_enable_switch(panel: &sidebar::aviation_panel::AviationPanel, state: &Rc<AppState>) {
    // Set `acars_pending` BEFORE dispatching so the 4 Hz mirror
    // tick (below) skips the switch-state mirror until the
    // AcarsEnabledChanged ack lands. Without this, the tick can
    // see the not-yet-updated `acars_enabled` cell, flip the
    // switch back to its old state, and re-enter this same
    // notify handler with the inverse SetAcarsEnabled —
    // racing the original request.
    {
        let state = Rc::clone(state);
        panel.enable_switch.connect_active_notify(move |row| {
            state.acars_pending.set(true);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsEnabled(
                row.is_active(),
            ));
        });
    }

    // ─── 4 Hz tick: AppState → switch row + status subtitle + per-channel rows ───
}

/// ACARS output-formatter rows (station ID, JSONL log, network feeder) — seed then wire then initial dispatch.
/// Split out per the 50-NLOC gate (#817).
fn wire_acars_output_rows(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Seed text fields and toggles BEFORE wiring signal handlers so
    // the initial set_text / set_active calls do NOT fire save or
    // dispatch. The explicit send_dsp block at the end delivers the
    // persisted state to the controller unconditionally.

    // Seed text fields first. Station ID is normalized to
    // the same 8-char cap the DSP enforces (see
    // `controller.rs::handle_set_acars_station_id`), and
    // healed back to config when the persisted value is over
    // the cap — otherwise the field, the config, and the
    // DSP would briefly hold three different values until
    // the user touches the row. CR round 5 on PR #595.
    let raw_station_id = crate::acars_config::read_acars_station_id(config);
    let normalized_station_id: String = raw_station_id
        .chars()
        .take(crate::acars_config::ACARS_STATION_ID_MAX_CHARS)
        .collect();
    if normalized_station_id != raw_station_id {
        crate::acars_config::save_acars_station_id(config, &normalized_station_id);
    }
    panel.station_id_row.set_text(&normalized_station_id);
    panel
        .jsonl_path_row
        .set_text(&crate::acars_config::read_acars_jsonl_path(config));
    panel
        .network_addr_row
        .set_text(&crate::acars_config::read_acars_network_addr(config));

    // Seed toggles (bind_property in the panel builder already handles
    // path/addr row visibility, so these calls also show/hide them).
    panel
        .jsonl_enable_row
        .set_active(crate::acars_config::read_acars_jsonl_enabled(config));
    panel
        .network_enable_row
        .set_active(crate::acars_config::read_acars_network_enabled(config));

    // Set subtitle to match current toggle state — signal
    // handlers only fire on user-initiated changes, so we
    // need to seed the subtitle explicitly. CR round 1 on
    // PR #595.
    seed_output_toggle_subtitle(
        &panel.jsonl_enable_row,
        &panel.jsonl_path_row.text(),
        crate::acars_config::ACARS_JSONL_DEFAULT_PATH,
    );
    seed_output_toggle_subtitle(
        &panel.network_enable_row,
        &panel.network_addr_row.text(),
        "feed.airframes.io:5550",
    );

    // Wire station_id_row — per-keystroke save + dispatch.
    // Station ID changes are cheap on the DSP side (just
    // updates a string field, no reopen), so live updates
    // are fine and match the codebase's existing pattern for
    // text-row config (server_panel.rs nickname,
    // source_panel.rs file path). CR round 4 on PR #595 —
    // `connect_apply` only fires on Enter, so it would lose
    // edits committed via focus-out or app close.
    {
        let state = Rc::clone(state);
        let config = std::sync::Arc::clone(config);
        panel.station_id_row.connect_changed(move |row| {
            let value = row.text().to_string();
            crate::acars_config::save_acars_station_id(&config, &value);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsStationId(value));
        });
    }

    wire_jsonl_output_rows(panel, state, config);
    wire_jsonl_path_row(panel, state, config);
    wire_network_output_rows(panel, state, config);
}

/// Seed an output-toggle row's subtitle to match its current state:
/// the configured value (or the default when empty) while active,
/// "Off" otherwise. Shared by the JSONL and network rows.
fn seed_output_toggle_subtitle(enable_row: &adw::SwitchRow, value: &str, default_value: &str) {
    let subtitle = if enable_row.is_active() {
        if value.is_empty() {
            default_value.to_string()
        } else {
            value.to_string()
        }
    } else {
        "Off".to_string()
    };
    enable_row.set_subtitle(&subtitle);
}

/// Custom-channels CSV apply row (Enter / focus-out): parse, validate, persist + dispatch.
/// Split out per the 50-NLOC gate (#817).
fn wire_custom_channels_row(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    toast_overlay: &adw::ToastOverlay,
) {
    use sdr_core::acars_airband_lock::AcarsRegion;
    use sdr_core::acars_airband_lock::validate_custom_channels;

    // Fires on Enter or focus-out. Parses CSV → multiplies by
    // 1e6 → validates via `validate_custom_channels`. On success
    // persists + dispatches a `Custom` variant with real
    // frequencies. On failure: toast naming the problem + add
    // the `error` CSS class (cleared on success).
    {
        let state = Rc::clone(state);
        let config = std::sync::Arc::clone(config);
        let toast_overlay = toast_overlay.clone();
        let channels_group = panel.channels_group.clone();
        let channel_rows_cell = std::rc::Rc::clone(&panel.channel_rows);
        panel.custom_channels_row.connect_apply(move |row| {
            // Stage 1: parse CSV → Vec<f64> (Hz). Stage 2:
            // domain validation (size, finite, span).
            let chans = match parse_custom_channels_csv(&row.text()) {
                Ok(v) => v,
                Err(e) => {
                    show_custom_channels_error(
                        row,
                        &toast_overlay,
                        &format!("Invalid custom channels: {e}"),
                    );
                    return;
                }
            };
            if let Err(e) = validate_custom_channels(&chans) {
                show_custom_channels_error(row, &toast_overlay, &e.to_string());
                return;
            }
            // Validated — clear any prior error styling and
            // commit (persist + rebuild rows + dispatch).
            row.remove_css_class("error");
            crate::acars_config::save_acars_custom_channels(&config, &chans);
            // Rebuild channel rows to match the new custom-
            // channel count — same inline pattern as the
            // region-change handler above.
            let new_count = chans.len();
            crate::sidebar::aviation_panel::rebuild_channel_rows_parts(
                &channels_group,
                &channel_rows_cell,
                new_count,
            );
            let region = AcarsRegion::Custom(chans.into_boxed_slice());
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsRegion(region));
        });
    }

    // ─── Toggle: switch-row → SetAcarsEnabled ───
}

/// Parse the Custom-channels CSV into Hz values. Empty entries (e.g.
/// a trailing comma) are silently skipped; a truly empty list is left
/// for `validate_custom_channels` to reject.
fn parse_custom_channels_csv(text: &str) -> Result<Vec<f64>, String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<f64>()
                .map(|mhz| mhz * 1_000_000.0)
                .map_err(|e| format!("'{s}': {e}"))
        })
        .collect()
}

/// Mark the Custom-channels row invalid and toast the reason. The
/// `error` CSS class is cleared by the success path.
fn show_custom_channels_error(row: &adw::EntryRow, toast_overlay: &adw::ToastOverlay, msg: &str) {
    row.add_css_class("error");
    let toast = adw::Toast::builder().title(msg).timeout(5).build();
    toast_overlay.add_toast(toast);
}

/// JSONL log toggle + path row.
/// Split out per the 50-NLOC gate (#817).
fn wire_jsonl_output_rows(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Wire jsonl_enable_row toggle.
    {
        let state = Rc::clone(state);
        let config = std::sync::Arc::clone(config);
        let path_row = panel.jsonl_path_row.clone();
        panel.jsonl_enable_row.connect_active_notify(move |row| {
            let active = row.is_active();
            let path = path_row.text().to_string();
            // Persist + dispatch the latest path on EVERY
            // toggle edge so an edit-then-toggle-off sequence
            // doesn't leave config / DSP holding the old
            // value. The user's typed text is always the
            // source of truth. CR round 3 on PR #595.
            crate::acars_config::save_acars_jsonl_path(&config, &path);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsJsonlPath(path.clone()));
            crate::acars_config::save_acars_jsonl_enabled(&config, active);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsJsonlEnabled(active));
            // Subtitle reflects current path or "Off".
            let subtitle = if active {
                if path.is_empty() {
                    crate::acars_config::ACARS_JSONL_DEFAULT_PATH.to_string()
                } else {
                    path
                }
            } else {
                "Off".to_string()
            };
            row.set_subtitle(&subtitle);
        });
    }
}

/// Network feeder toggle + address row + initial dispatch.
/// Split out per the 50-NLOC gate (#817).
fn wire_network_output_rows(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Wire network_enable_row toggle.
    {
        let state = Rc::clone(state);
        let config = std::sync::Arc::clone(config);
        let addr_row = panel.network_addr_row.clone();
        panel.network_enable_row.connect_active_notify(move |row| {
            let active = row.is_active();
            let addr = addr_row.text().to_string();
            // Persist + dispatch the latest addr on EVERY
            // toggle edge — same pattern as jsonl above.
            // CR round 3 on PR #595.
            crate::acars_config::save_acars_network_addr(&config, &addr);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsNetworkAddr(
                addr.clone(),
            ));
            crate::acars_config::save_acars_network_enabled(&config, active);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsNetworkEnabled(active));
            // Subtitle reflects current addr or "Off".
            let subtitle = if active {
                if addr.is_empty() {
                    "feed.airframes.io:5550".to_string()
                } else {
                    addr
                }
            } else {
                "Off".to_string()
            };
            row.set_subtitle(&subtitle);
        });
    }

    wire_network_addr_row(panel, state, config);
}

/// 4 Hz ACARS status/channel-row mirror tick + open-viewer button.
/// Split out per the 50-NLOC gate (#817).
fn wire_acars_status_tick(panel: &sidebar::aviation_panel::AviationPanel, state: &Rc<AppState>) {
    use crate::sidebar::aviation_panel::SIDEBAR_STATUS_REFRESH_MS;

    // Hold weak refs only so closing the window drops the panel
    // widgets and the timer self-cancels via Break on the next
    // fire. Strong refs would keep the panel + AppState alive
    // for the rest of the process. The channel-rows view is an
    // `Rc<RefCell<…>>` clone so the rebuild handler can swap the
    // row list under us without the tick needing a re-snapshot.
    // We hold a STRONG ref to the cell here — the cell itself
    // is small (one `Vec<ActionRow>` plus a borrow flag) and
    // the rows it owns are dropped in lock-step with the panel
    // widgets when the window closes; the switch/status weak
    // refs gate Break on the next fire either way.
    let switch_weak = panel.enable_switch.downgrade();
    let status_weak = panel.status_row.downgrade();
    let channel_rows_for_tick = std::rc::Rc::clone(&panel.channel_rows);
    let state_for_tick = Rc::clone(state);
    glib::timeout_add_local(
        std::time::Duration::from_millis(SIDEBAR_STATUS_REFRESH_MS),
        move || {
            let (Some(switch), Some(status)) = (switch_weak.upgrade(), status_weak.upgrade())
            else {
                return glib::ControlFlow::Break;
            };

            let enabled = state_for_tick.acars_enabled.get();

            // Mirror Cell→switch one direction only — but only
            // when no SetAcarsEnabled is in flight. While
            // pending, the user's just-typed switch value is
            // authoritative; mirroring back from the still-old
            // `enabled` Cell would race the in-flight command.
            // Cleared by every AcarsEnabledChanged arm.
            if !state_for_tick.acars_pending.get() && switch.is_active() != enabled {
                switch.set_active(enabled);
            }

            status.set_subtitle(&acars_status_subtitle(&state_for_tick, enabled));

            // Per-channel rows. Read the LIVE row list each tick
            // so a region swap (which drops + recreates rows
            // under `channel_rows_for_tick`) is reflected
            // immediately — a snapshot taken at wire time would
            // hold weak refs into the OLD generation of rows.
            // `channel_stats` is a `Vec` sized from the active
            // region (Task 9 migration); the two lengths can
            // transiently differ around a region swap (panel
            // rebuild lags the next DSP-side stats emission).
            // Zip over the shorter of the two so neither side
            // panics during the window. Issue #592.
            let channel_stats = state_for_tick.acars_channel_stats.borrow();
            let rows = channel_rows_for_tick.borrow();
            for (row, ch) in rows.iter().zip(channel_stats.iter()) {
                render_acars_channel_row(row, ch);
            }
            glib::ControlFlow::Continue
        },
    );

    wire_open_viewer_button(panel, state);

    // ─── Output-formatter widget seed + wiring (issue #578) ───
}

/// Status-row subtitle for the 4 Hz mirror tick: decode count + age
/// of the newest message while enabled, "Disabled" otherwise.
fn acars_status_subtitle(state: &Rc<AppState>, enabled: bool) -> String {
    let total = state.acars_total_count.get();
    let last_label = state
        .acars_recent
        .borrow()
        .back()
        .map(|m| format!("Last: {}", format_relative_age(m.timestamp)));
    if enabled {
        match last_label {
            Some(s) => format!("Decoded {total} · {s}"),
            None => format!("Decoded {total} · Awaiting first message"),
        }
    } else {
        "Disabled".to_string()
    }
}

/// Open-viewer button of the Aviation panel. Split out per the
/// 50-NLOC gate (#817).
fn wire_open_viewer_button(panel: &sidebar::aviation_panel::AviationPanel, state: &Rc<AppState>) {
    // ─── Open ACARS window button ───
    {
        let state = Rc::clone(state);
        panel.open_viewer_button.connect_clicked(move |_| {
            crate::acars_viewer::open_acars_viewer_if_needed(&state);
        });
    }
}

/// JSONL path row: per-keystroke save + Enter/focus-out dispatch.
/// Split out per the 50-NLOC gate (#817).
fn wire_jsonl_path_row(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Wire jsonl_path_row — per-keystroke save to config so
    // edits never get lost on focus-out / app close, plus
    // explicit-commit DSP dispatch on Enter (apply) so we
    // don't reopen the writer 17 times while the user types
    // a path. Toggle handlers read the live row text
    // separately, so toggling on after typing without Enter
    // dispatches the latest value too. CR round 4 on PR #595.
    {
        let config = std::sync::Arc::clone(config);
        panel.jsonl_path_row.connect_changed(move |row| {
            crate::acars_config::save_acars_jsonl_path(&config, &row.text());
        });
    }
    {
        let state = Rc::clone(state);
        let enable_row = panel.jsonl_enable_row.clone();
        panel.jsonl_path_row.connect_apply(move |row| {
            let value = row.text().to_string();
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsJsonlPath(
                value.clone(),
            ));
            // Keep the enable-row subtitle in sync — when the
            // toggle is on, the subtitle shows the current
            // path. CR round 2 on PR #595.
            if enable_row.is_active() {
                let subtitle = if value.is_empty() {
                    crate::acars_config::ACARS_JSONL_DEFAULT_PATH.to_string()
                } else {
                    value
                };
                enable_row.set_subtitle(&subtitle);
            }
        });
    }
}

/// Network address row + the initial persisted-value dispatch.
/// Split out per the 50-NLOC gate (#817).
fn wire_network_addr_row(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Wire network_addr_row — same split as jsonl_path_row:
    // per-keystroke save to config (no DNS/dial spam), DSP
    // dispatch on apply. CR round 4 on PR #595.
    {
        let config = std::sync::Arc::clone(config);
        panel.network_addr_row.connect_changed(move |row| {
            crate::acars_config::save_acars_network_addr(&config, &row.text());
        });
    }
    {
        let state = Rc::clone(state);
        let enable_row = panel.network_enable_row.clone();
        panel.network_addr_row.connect_apply(move |row| {
            let value = row.text().to_string();
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsNetworkAddr(
                value.clone(),
            ));
            // Keep the enable-row subtitle in sync — when the
            // toggle is on, the subtitle shows the current
            // addr. CR round 2 on PR #595.
            if enable_row.is_active() {
                let subtitle = if value.is_empty() {
                    "feed.airframes.io:5550".to_string()
                } else {
                    value
                };
                enable_row.set_subtitle(&subtitle);
            }
        });
    }

    // Initial dispatch — deliver persisted values to the controller so
    // writers reopen on launch if previously enabled. Path/addr/station_id
    // are dispatched BEFORE enabled so the controller's handler finds the
    // pending paths already set when it processes the enabled flags.
    // Use the (already-normalized) row text rather than the
    // raw config value so the initial DSP-side state matches
    // exactly what the user sees. CR round 5 on PR #595.
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsStationId(
        panel.station_id_row.text().to_string(),
    ));
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsJsonlPath(
        crate::acars_config::read_acars_jsonl_path(config),
    ));
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsNetworkAddr(
        crate::acars_config::read_acars_network_addr(config),
    ));
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsJsonlEnabled(
        crate::acars_config::read_acars_jsonl_enabled(config),
    ));
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsNetworkEnabled(
        crate::acars_config::read_acars_network_enabled(config),
    ));
}
