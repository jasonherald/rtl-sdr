//! Config persistence for the Server (Share) panel (issue #819):
//! the restore-then-subscribe wiring in
//! [`connect_server_panel_persistence`]. Split out of
//! `server_panel.rs` per the file-size pass.

use std::sync::Arc;

use libadwaita::prelude::*;
use sdr_config::ConfigManager;

use super::{
    BIND_ALL_INTERFACES_IDX, BIND_LOOPBACK_IDX, COMPRESSION_COUNT, KEY_SERVER_ADVERTISE,
    KEY_SERVER_BIND_IDX, KEY_SERVER_COMPRESSION_IDX, KEY_SERVER_DEFAULT_BIAS_TEE,
    KEY_SERVER_DEFAULT_DIRECT_SAMPLING, KEY_SERVER_DEFAULT_FREQ_HZ, KEY_SERVER_DEFAULT_GAIN_DB,
    KEY_SERVER_DEFAULT_PPM, KEY_SERVER_DEFAULT_SR_IDX, KEY_SERVER_LISTENER_CAP,
    KEY_SERVER_NICKNAME, KEY_SERVER_PORT, KEY_SERVER_REQUIRE_AUTH, MAX_CENTER_FREQ_HZ,
    MAX_LISTENER_CAP, MAX_SERVER_GAIN_DB, MAX_SERVER_PORT, MAX_SERVER_PPM, MIN_CENTER_FREQ_HZ,
    MIN_LISTENER_CAP, MIN_SERVER_GAIN_DB, MIN_SERVER_PORT, MIN_SERVER_PPM, SAMPLE_RATE_COUNT,
    ServerPanel,
};

/// Load saved server-panel values from `config` and wire every
/// editable row to re-persist on change. Called from `window.rs`
/// after the panel is built. Two-phase:
///
/// 1. **Restore** — read each key, fall back to the widget's
///    existing default if the key is absent or of the wrong type.
///    Unknown / corrupt types are silently dropped (the restore
///    path is fire-and-forget — `serde_json`'s `as_*` helpers
///    return `None` on a type mismatch, the `if let Some` guard
///    skips the apply, and the widget keeps its build-time
///    default). No panic path.
/// 2. **Subscribe** — install a notify handler on each editable
///    widget that writes its current value back to `config`. The
///    config manager's auto-save thread picks up the change on
///    its ~1 s tick.
///
/// `GObject` weak refs on the capture side would over-complicate
/// this signal-handler block; `clone()` is fine here because the
/// panel's widgets are all held strongly by the sidebar (= window)
/// lifetime anyway, and the notify handlers only fire on user
pub fn connect_server_panel_persistence(panel: &ServerPanel, config: &Arc<ConfigManager>) {
    // ---- Phase 1: restore ----
    config.read(|v| {
        restore_connection_rows(panel, v);
        restore_device_default_rows(panel, v);
        restore_sharing_policy_rows(panel, v);
    });

    // ---- Phase 2: subscribe ----
    subscribe_connection_rows(panel, config);
    subscribe_device_default_rows(panel, config);
    subscribe_device_frontend_rows(panel, config);
    subscribe_sharing_policy_rows(panel, config);
}

/// Phase-1 restore for the connection rows (nickname / port / bind /
/// advertise). Split out of [`connect_server_panel_persistence`] per
/// the 50-NLOC gate (#819, PR #880 Codacy precedent).
#[allow(
    clippy::cast_precision_loss,
    reason = "persisted numeric fields (port / freq Hz / ppm) fit well below f64's 52-bit mantissa; the spin rows clamp to u16/u32 ranges at the widget level"
)]
fn restore_connection_rows(panel: &ServerPanel, v: &serde_json::Value) {
    if let Some(nickname) = v
        .get(KEY_SERVER_NICKNAME)
        .and_then(serde_json::Value::as_str)
    {
        panel.nickname_row.set_text(nickname);
    }
    if let Some(port) = v.get(KEY_SERVER_PORT).and_then(serde_json::Value::as_u64) {
        let clamped = (port as f64).clamp(MIN_SERVER_PORT, MAX_SERVER_PORT);
        panel.port_row.set_value(clamped);
    }
    if let Some(bind_idx) = v
        .get(KEY_SERVER_BIND_IDX)
        .and_then(serde_json::Value::as_u64)
    {
        // Accept only the legal indices; anything else falls
        // back to loopback (safest default — never silently
        // widens exposure).
        let idx = u32::try_from(bind_idx).unwrap_or(BIND_LOOPBACK_IDX);
        let legal = if idx == BIND_ALL_INTERFACES_IDX {
            BIND_ALL_INTERFACES_IDX
        } else {
            BIND_LOOPBACK_IDX
        };
        panel.bind_row.set_selected(legal);
    }
    if let Some(advertise) = v
        .get(KEY_SERVER_ADVERTISE)
        .and_then(serde_json::Value::as_bool)
    {
        panel.advertise_row.set_active(advertise);
    }
}

/// Phase-1 restore for the device-defaults rows (freq / sample rate /
/// gain / PPM / bias-T / direct sampling). Split out per the 50-NLOC
/// gate (#819).
#[allow(
    clippy::cast_precision_loss,
    reason = "persisted numeric fields (port / freq Hz / ppm) fit well below f64's 52-bit mantissa; the spin rows clamp to u16/u32 ranges at the widget level"
)]
fn restore_device_default_rows(panel: &ServerPanel, v: &serde_json::Value) {
    if let Some(freq) = v
        .get(KEY_SERVER_DEFAULT_FREQ_HZ)
        .and_then(serde_json::Value::as_u64)
    {
        let clamped = (freq as f64).clamp(MIN_CENTER_FREQ_HZ, MAX_CENTER_FREQ_HZ);
        panel.center_freq_row.set_value(clamped);
    }
    if let Some(idx) = v
        .get(KEY_SERVER_DEFAULT_SR_IDX)
        .and_then(serde_json::Value::as_u64)
        && let Ok(idx_u32) = u32::try_from(idx)
        && idx_u32 < SAMPLE_RATE_COUNT
    {
        // Strict bounds check on the stored index: anything
        // past the StringList's last entry is discarded (not
        // silently clamped) so a corrupt config leaves the
        // widget on its build-time default instead of flipping
        // to an arbitrary rate. Same policy as `bind_row`.
        panel.sample_rate_row.set_selected(idx_u32);
    }
    if let Some(gain) = v
        .get(KEY_SERVER_DEFAULT_GAIN_DB)
        .and_then(serde_json::Value::as_f64)
    {
        let clamped = gain.clamp(MIN_SERVER_GAIN_DB, MAX_SERVER_GAIN_DB);
        panel.gain_row.set_value(clamped);
    }
    if let Some(ppm) = v
        .get(KEY_SERVER_DEFAULT_PPM)
        .and_then(serde_json::Value::as_i64)
    {
        let clamped = (ppm as f64).clamp(MIN_SERVER_PPM, MAX_SERVER_PPM);
        panel.ppm_row.set_value(clamped);
    }
    if let Some(bias_tee) = v
        .get(KEY_SERVER_DEFAULT_BIAS_TEE)
        .and_then(serde_json::Value::as_bool)
    {
        panel.bias_tee_row.set_active(bias_tee);
    }
    if let Some(ds) = v
        .get(KEY_SERVER_DEFAULT_DIRECT_SAMPLING)
        .and_then(serde_json::Value::as_bool)
    {
        panel.direct_sampling_row.set_active(ds);
    }
}

/// Phase-1 restore for the sharing-policy rows (compression /
/// listener cap / require-key). Split out per the 50-NLOC gate (#819).
#[allow(
    clippy::cast_precision_loss,
    reason = "persisted numeric fields (port / freq Hz / ppm) fit well below f64's 52-bit mantissa; the spin rows clamp to u16/u32 ranges at the widget level"
)]
fn restore_sharing_policy_rows(panel: &ServerPanel, v: &serde_json::Value) {
    if let Some(idx) = v
        .get(KEY_SERVER_COMPRESSION_IDX)
        .and_then(serde_json::Value::as_u64)
        && let Ok(idx_u32) = u32::try_from(idx)
        && idx_u32 < COMPRESSION_COUNT
    {
        // Strict bounds check: unknown stored indices fall
        // back to the widget's build-time default (`Off`) so
        // a corrupt config can't silently enable compression.
        panel.compression_row.set_selected(idx_u32);
    }
    if let Some(cap) = v
        .get(KEY_SERVER_LISTENER_CAP)
        .and_then(serde_json::Value::as_u64)
    {
        // Clamp to the UI's advertised range on restore. An
        // out-of-range stored value would have been saved by
        // some other client talking to the same config file
        // (e.g. `sdr-rtl-tcp --listener-cap 999`); the widget
        // still needs to be a valid spin-row value so pin it
        // into [MIN_LISTENER_CAP, MAX_LISTENER_CAP]. Per #395.
        let clamped = (cap as f64).clamp(MIN_LISTENER_CAP, MAX_LISTENER_CAP);
        panel.listener_cap_row.set_value(clamped);
    }
    if let Some(require) = v
        .get(KEY_SERVER_REQUIRE_AUTH)
        .and_then(serde_json::Value::as_bool)
    {
        // Restore the "Require key" toggle state. The key
        // itself lives in the OS keyring; window.rs loads /
        // creates it on toggle-on. Just restore the bool
        // here so the widget reflects the user's last
        // choice; window.rs's connect-active handler
        // kicks off the keyring/server wiring if it was on.
        // Per #395.
        panel.auth_require_row.set_active(require);
    }
}

/// Phase-2 subscribe for the connection rows. Split out per the
/// 50-NLOC gate (#819).
fn subscribe_connection_rows(panel: &ServerPanel, config: &Arc<ConfigManager>) {
    // Nickname: AdwEntryRow fires `connect_changed` on every edit.
    let cfg_nick = Arc::clone(config);
    panel.nickname_row.connect_changed(move |row| {
        let text = row.text();
        cfg_nick.write(|v| {
            v[KEY_SERVER_NICKNAME] = serde_json::json!(text.as_str());
        });
    });
    // Port spin row.
    let cfg_port = Arc::clone(config);
    panel.port_row.connect_value_notify(move |row| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "spin row bounded to 1024..=65535 at the widget level"
        )]
        let port = row.value() as u64;
        cfg_port.write(|v| {
            v[KEY_SERVER_PORT] = serde_json::json!(port);
        });
    });
    // Bind-address combo. Only persist legal indices — GTK's
    // ComboRow can emit transient out-of-range values during
    // widget-model churn (e.g. a repopulation mid-drag). Writing
    // those verbatim would corrupt the next startup's restore,
    // which would then silently fall back to loopback and hide
    // the drift. Strict gate here + on the restore side keeps
    // the persisted state well-formed.
    let cfg_bind = Arc::clone(config);
    panel.bind_row.connect_selected_notify(move |row| {
        let selected = row.selected();
        if selected == BIND_LOOPBACK_IDX || selected == BIND_ALL_INTERFACES_IDX {
            cfg_bind.write(|v| {
                v[KEY_SERVER_BIND_IDX] = serde_json::json!(selected);
            });
        }
    });
    // Advertise switch.
    let cfg_adv = Arc::clone(config);
    panel.advertise_row.connect_active_notify(move |row| {
        cfg_adv.write(|v| {
            v[KEY_SERVER_ADVERTISE] = serde_json::json!(row.is_active());
        });
    });
}

/// Phase-2 subscribe for the device-defaults rows. Split out per the
/// 50-NLOC gate (#819).
fn subscribe_device_default_rows(panel: &ServerPanel, config: &Arc<ConfigManager>) {
    // Center frequency spin row (device default).
    let cfg_freq = Arc::clone(config);
    panel.center_freq_row.connect_value_notify(move |row| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "spin row bounded to u32-valid Hz range at the widget level"
        )]
        let hz = row.value() as u64;
        cfg_freq.write(|v| {
            v[KEY_SERVER_DEFAULT_FREQ_HZ] = serde_json::json!(hz);
        });
    });
    // Sample-rate combo (device default). Same strict-gate policy
    // as `bind_row` — don't persist transient out-of-range values
    // from GTK widget-model churn.
    let cfg_sr = Arc::clone(config);
    panel.sample_rate_row.connect_selected_notify(move |row| {
        let selected = row.selected();
        if selected < SAMPLE_RATE_COUNT {
            cfg_sr.write(|v| {
                v[KEY_SERVER_DEFAULT_SR_IDX] = serde_json::json!(selected);
            });
        }
    });
    // Gain spin row (device default).
    let cfg_gain = Arc::clone(config);
    panel.gain_row.connect_value_notify(move |row| {
        cfg_gain.write(|v| {
            v[KEY_SERVER_DEFAULT_GAIN_DB] = serde_json::json!(row.value());
        });
    });
    // PPM spin row (device default).
    let cfg_ppm = Arc::clone(config);
    panel.ppm_row.connect_value_notify(move |row| {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "spin row bounded to ±200 at the widget level"
        )]
        let ppm = row.value() as i64;
        cfg_ppm.write(|v| {
            v[KEY_SERVER_DEFAULT_PPM] = serde_json::json!(ppm);
        });
    });
}

/// Phase-2 subscribe for the bias-T / direct-sampling frontend
/// switches. Split out of the device-defaults subscriber per the
/// 50-NLOC gate (#819).
fn subscribe_device_frontend_rows(panel: &ServerPanel, config: &Arc<ConfigManager>) {
    // Bias-tee switch.
    let cfg_bt = Arc::clone(config);
    panel.bias_tee_row.connect_active_notify(move |row| {
        cfg_bt.write(|v| {
            v[KEY_SERVER_DEFAULT_BIAS_TEE] = serde_json::json!(row.is_active());
        });
    });
    // Direct-sampling switch.
    let cfg_ds = Arc::clone(config);
    panel.direct_sampling_row.connect_active_notify(move |row| {
        cfg_ds.write(|v| {
            v[KEY_SERVER_DEFAULT_DIRECT_SAMPLING] = serde_json::json!(row.is_active());
        });
    });
}

/// Phase-2 subscribe for the sharing-policy rows. Split out per the
/// 50-NLOC gate (#819).
fn subscribe_sharing_policy_rows(panel: &ServerPanel, config: &Arc<ConfigManager>) {
    // Compression codec combo. Same strict-gate policy as
    // `bind_row` / `sample_rate_row` — only persist in-range
    // indices so widget-model churn can't corrupt the stored value.
    let cfg_comp = Arc::clone(config);
    panel.compression_row.connect_selected_notify(move |row| {
        let selected = row.selected();
        if selected < COMPRESSION_COUNT {
            cfg_comp.write(|v| {
                v[KEY_SERVER_COMPRESSION_IDX] = serde_json::json!(selected);
            });
        }
    });
    // Listener cap spin row. Persist on every change so the next
    // session restores the same cap. Applying the new value to a
    // running server (`Server::set_listener_cap`) is wired
    // separately in `window.rs` where the live `Server` handle
    // lives. Per #395.
    let cfg_cap = Arc::clone(config);
    panel.listener_cap_row.connect_value_notify(move |row| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "spin row bounded to [MIN_LISTENER_CAP, MAX_LISTENER_CAP] at the widget level"
        )]
        let cap = row.value() as u64;
        cfg_cap.write(|v| {
            v[KEY_SERVER_LISTENER_CAP] = serde_json::json!(cap);
        });
    });
    // "Require key" switch — persist the bool to sdr_config. The
    // key bytes themselves live in the OS keyring, managed by
    // window.rs. Per #395.
    let cfg_auth = Arc::clone(config);
    panel.auth_require_row.connect_active_notify(move |row| {
        cfg_auth.write(|v| {
            v[KEY_SERVER_REQUIRE_AUTH] = serde_json::json!(row.is_active());
        });
    });
}
