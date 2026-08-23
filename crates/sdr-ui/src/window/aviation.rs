//! Aviation (ACARS) panel wiring.

use gtk4::prelude::*;
use gtk4::subclass::prelude::ObjectSubclassIsExt;
use libadwaita::prelude::*;

use super::{AppState, Rc, adw, glib, sidebar};

/// Wire the Aviation sidebar panel: toggle switch → DSP, 4 Hz tick
/// for status/channel-row refresh, the open-viewer button, and the
/// output-formatter controls (station ID, JSONL log, network feeder).
#[allow(clippy::too_many_lines)]
pub(super) fn connect_aviation_panel(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    toast_overlay: &adw::ToastOverlay,
) {
    // ─── Region selector seed + signal (issue #581 / #592) ───
    wire_region_and_custom_rows(panel, state, config, toast_overlay);

    wire_acars_enable_switch(panel, state);

    wire_acars_output_rows(panel, state, config);
}

/// Format a `SystemTime` as a relative age string ("5s ago",
/// "2m ago", "1h ago"). Returns "—" if the timestamp is in the
/// future or unrepresentable.
/// Walk the most recent rows of the viewer store backwards from
/// the end and check for a `(aircraft, mode, label, text)` key
/// match within `ACARS_COLLAPSE_WINDOW`. Returns the matched
/// row's index after bumping its count + `last_seen` in place,
/// or `None` if no in-window match — in which case the caller
/// appends a fresh row. Stops walking as soon as it sees a row
/// older than the recency window (rows are insertion-ordered
/// in the underlying store, oldest at index 0). Issue #586.
pub(super) fn try_collapse_into_existing(
    store: &gtk4::gio::ListStore,
    msg: &sdr_acars::AcarsMessage,
) -> Option<u32> {
    use gtk4::prelude::ListModelExt;
    let n = store.n_items();
    if n == 0 {
        return None;
    }
    let cutoff = msg
        .timestamp
        .checked_sub(crate::acars_viewer::ACARS_COLLAPSE_WINDOW)?;
    let mut idx = n;
    while idx > 0 {
        idx -= 1;
        let Some(item) = store.item(idx) else {
            continue;
        };
        let Some(obj) = item.downcast_ref::<crate::acars_viewer::AcarsMessageObject>() else {
            continue;
        };
        // Skip rows older than the recency window. We can't
        // early-exit here even though insertion order would
        // suggest later rows have even older `last_seen`:
        // `record_duplicate` updates an existing row's
        // `last_seen` IN PLACE (no store reorder), so the
        // "monotonic by index" invariant doesn't hold once any
        // collapse has fired. CR round 1 on PR #591.
        if obj.last_seen() < cutoff {
            continue;
        }
        let inner = obj.imp().inner.borrow();
        let Some(existing) = inner.as_ref() else {
            continue;
        };
        if existing.aircraft == msg.aircraft
            && existing.mode == msg.mode
            && existing.label == msg.label
            && existing.text == msg.text
        {
            // Drop the borrow before mutating via the public
            // API (which doesn't actually need the borrow held,
            // but keeping the scope tight is cleaner).
            drop(inner);
            obj.record_duplicate(msg.timestamp);
            return Some(idx);
        }
    }
    None
}

pub(super) fn format_relative_age(ts: std::time::SystemTime) -> String {
    let Ok(elapsed) = ts.elapsed() else {
        return "—".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Region combo + custom-channels CSV row (seed-then-wire).
/// Split out per the 50-NLOC gate (#817).
fn wire_region_and_custom_rows(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    toast_overlay: &adw::ToastOverlay,
) {
    use crate::sidebar::aviation_panel::{
        rebuild_channel_rows, region_combo_index, region_from_combo_index,
    };
    use sdr_core::acars_airband_lock::{AcarsRegion, validate_custom_channels};

    // Read the persisted region, dispatch it to DSP at startup,
    // and seed the combo index BEFORE wiring the change handler
    // — otherwise the seed itself would fire a redundant
    // dispatch + persist round-trip.
    let saved_region_id = crate::acars_config::read_acars_channel_set(config);
    let initial_region = if saved_region_id == "custom" {
        // Two-key load: read channels, validate, then build
        // Custom — or fall back to default on bad/stale config.
        // `Custom([])` would reach the engage guard as invalid
        // (validate_custom_channels rejects empty), so we must
        // not dispatch it at startup.
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
    };
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
    wire_custom_channels_row(panel, state, config, toast_overlay);
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
    use crate::sidebar::aviation_panel::{
        rebuild_channel_rows, region_combo_index, region_from_combo_index,
    };
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
    // Custom slot: hydrate from saved config (or the
    // current EntryRow text if the user just typed
    // something but hasn't pressed Enter yet). Empty
    // list is fine — the variant is dispatched as
    // `Custom([])` and the apply handler later
    // replaces it with a real list.
    if matches!(region, AcarsRegion::Custom(_)) {
        let saved = crate::acars_config::read_acars_custom_channels(&config);
        region = AcarsRegion::Custom(saved.into_boxed_slice());
        // Make the EntryRow visible immediately on
        // selection — the visibility-binding closure in
        // the panel builder also fires on this same
        // notify, but invoking the binding side-effect
        // here would require ordering guarantees we
        // don't have. Cheap idempotent set is fine.
        custom_row_for_dispatch.set_visible(true);
    }
    crate::acars_config::save_acars_channel_set(&config, region.config_id());
    // Rebuild channel rows to match the new region's
    // channel count — borrow_mut + adw mutation must
    // happen before send_dsp because the 4 Hz tick will
    // start consulting the new row count almost
    // immediately.
    let new_count = region.channels().len();
    // Inline the rebuild (the helper takes
    // `&AviationPanel`, but we only have the
    // individual fields here in the closure).
    {
        let mut rows = channel_rows_cell.borrow_mut();
        for row in rows.iter() {
            channels_group.remove(row);
        }
        rows.clear();
        for _ in 0..new_count {
            let row = adw::ActionRow::builder().title("—").subtitle("—").build();
            channels_group.add(&row);
            rows.push(row);
        }
    }
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
    wire_acars_status_tick(panel, state);
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
    let normalized_station_id: String = raw_station_id.chars().take(8).collect();
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
    {
        let path = panel.jsonl_path_row.text();
        let active = panel.jsonl_enable_row.is_active();
        let subtitle = if active {
            if path.is_empty() {
                "~/sdr-recordings/acars.jsonl".to_string()
            } else {
                path.to_string()
            }
        } else {
            "Off".to_string()
        };
        panel.jsonl_enable_row.set_subtitle(&subtitle);
    }
    {
        let addr = panel.network_addr_row.text();
        let active = panel.network_enable_row.is_active();
        let subtitle = if active {
            if addr.is_empty() {
                "feed.airframes.io:5550".to_string()
            } else {
                addr.to_string()
            }
        } else {
            "Off".to_string()
        };
        panel.network_enable_row.set_subtitle(&subtitle);
    }

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

    wire_network_output_rows(panel, state, config);
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
            let text = row.text();
            // Stage 1: parse CSV → Vec<f64> (Hz). Empty entries
            // (e.g. trailing comma) are silently skipped; a
            // truly empty list will be caught in stage 2 by
            // `validate_custom_channels`.
            let parsed: Result<Vec<f64>, String> = text
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    s.parse::<f64>()
                        .map(|mhz| mhz * 1_000_000.0)
                        .map_err(|e| format!("'{s}': {e}"))
                })
                .collect();
            let chans = match parsed {
                Ok(v) => v,
                Err(e) => {
                    row.add_css_class("error");
                    let toast = adw::Toast::builder()
                        .title(format!("Invalid custom channels: {e}"))
                        .timeout(5)
                        .build();
                    toast_overlay.add_toast(toast);
                    return;
                }
            };
            // Stage 2: domain validation (size, finite, span).
            if let Err(e) = validate_custom_channels(&chans) {
                row.add_css_class("error");
                let toast = adw::Toast::builder()
                    .title(e.to_string())
                    .timeout(5)
                    .build();
                toast_overlay.add_toast(toast);
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
            {
                let mut rows = channel_rows_cell.borrow_mut();
                for row in rows.iter() {
                    channels_group.remove(row);
                }
                rows.clear();
                for _ in 0..new_count {
                    let r = adw::ActionRow::builder().title("—").subtitle("—").build();
                    channels_group.add(&r);
                    rows.push(r);
                }
            }
            let region = AcarsRegion::Custom(chans.into_boxed_slice());
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsRegion(region));
        });
    }

    // ─── Toggle: switch-row → SetAcarsEnabled ───
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
                    "~/sdr-recordings/acars.jsonl".to_string()
                } else {
                    path
                }
            } else {
                "Off".to_string()
            };
            row.set_subtitle(&subtitle);
        });
    }

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
                    "~/sdr-recordings/acars.jsonl".to_string()
                } else {
                    value
                };
                enable_row.set_subtitle(&subtitle);
            }
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

/// 4 Hz ACARS status/channel-row mirror tick + open-viewer button.
/// Split out per the 50-NLOC gate (#817).
fn wire_acars_status_tick(panel: &sidebar::aviation_panel::AviationPanel, state: &Rc<AppState>) {
    use crate::sidebar::aviation_panel::GLYPH_IDLE;
    use crate::sidebar::aviation_panel::GLYPH_LOCKED;
    use crate::sidebar::aviation_panel::GLYPH_SIGNAL;
    use crate::sidebar::aviation_panel::SIDEBAR_STATUS_REFRESH_MS;
    use sdr_acars::ChannelLockState;

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

            // Status subtitle.
            let total = state_for_tick.acars_total_count.get();
            let last_label = state_for_tick
                .acars_recent
                .borrow()
                .back()
                .map(|m| format!("Last: {}", format_relative_age(m.timestamp)));
            let subtitle = if enabled {
                match last_label {
                    Some(s) => format!("Decoded {total} · {s}"),
                    None => format!("Decoded {total} · Awaiting first message"),
                }
            } else {
                "Disabled".to_string()
            };
            status.set_subtitle(&subtitle);

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
                let glyph = match ch.lock_state {
                    ChannelLockState::Locked => GLYPH_LOCKED,
                    ChannelLockState::Idle => GLYPH_IDLE,
                    ChannelLockState::Signal => GLYPH_SIGNAL,
                };
                row.set_title(&format!("{glyph}  {:.3} MHz", ch.freq_hz / 1_000_000.0));
                row.set_subtitle(&format!(
                    "{} msgs · {:.1} dB · {}",
                    ch.msg_count,
                    ch.level_db,
                    ch.last_msg_at
                        .map_or_else(|| "—".to_string(), format_relative_age)
                ));
            }
            glib::ControlFlow::Continue
        },
    );

    // ─── Open ACARS window button ───
    {
        let state = Rc::clone(state);
        panel.open_viewer_button.connect_clicked(move |_| {
            crate::acars_viewer::open_acars_viewer_if_needed(&state);
        });
    }

    // ─── Output-formatter widget seed + wiring (issue #578) ───
}
