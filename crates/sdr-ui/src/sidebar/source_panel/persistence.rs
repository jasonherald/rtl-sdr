//! Config load/save helpers for the Source panel's persisted rows
//! (issue #819): AGC type (with the pre-#354 legacy-boolean
//! migration), the RTL frontend toggles, the Airspy serial, and the
//! #552 top-level/source-type persistence block. Split out of
//! `source_panel.rs` per the file-size pass.

use std::sync::Arc;

use sdr_config::ConfigManager;
use sdr_source_rtlsdr::SAMPLE_RATES;

use super::{
    AgcType, DECIMATION_FACTORS, DEFAULT_SAMPLE_RATE_INDEX, DEVICE_AIRSPY, DEVICE_RTLSDR,
    DIRECT_SAMPLING_DISABLED_IDX, DIRECT_SAMPLING_MAX_IDX, KEY_AGC_TYPE, KEY_AIRSPY_SERIAL,
    KEY_LEGACY_AGC_ENABLED, KEY_SOURCE_CONVERTER_OFFSET_HZ, KEY_SOURCE_DC_BLOCKING,
    KEY_SOURCE_DECIMATION_INDEX, KEY_SOURCE_DEVICE_INDEX, KEY_SOURCE_FILE_PATH,
    KEY_SOURCE_IQ_CORRECTION, KEY_SOURCE_IQ_INVERSION, KEY_SOURCE_NETWORK_HOSTNAME,
    KEY_SOURCE_NETWORK_PORT, KEY_SOURCE_NETWORK_PROTOCOL_INDEX, KEY_SOURCE_RTL_BIAS_TEE,
    KEY_SOURCE_RTL_DIRECT_SAMPLING_MODE, KEY_SOURCE_RTL_GAIN_DB, KEY_SOURCE_RTL_OFFSET_TUNING,
    KEY_SOURCE_RTL_PPM, KEY_SOURCE_SAMPLE_RATE_INDEX, NETWORK_PROTOCOL_TCPCLIENT_IDX,
    NETWORK_PROTOCOL_UDP_IDX,
};

/// Load the persisted AGC type selection. Returns
/// [`AgcType::DEFAULT`] on first launch / absent / corrupt
/// config. Falls back to the legacy `KEY_LEGACY_AGC_ENABLED`
/// boolean when the new key is absent — mapping `true →
/// Hardware` and `false → Off` so users upgrading from a
/// pre-#354 build keep their AGC setting on first startup.
pub fn load_agc_type(config: &Arc<ConfigManager>) -> AgcType {
    config.read(|v| {
        if let Some(entry) = v.get(KEY_AGC_TYPE) {
            // New key present — trust it.
            if let Ok(t) = serde_json::from_value::<AgcType>(entry.clone()) {
                return t;
            }
        }
        // Fall back to the legacy boolean, then to the default
        // if that's absent too.
        v.get(KEY_LEGACY_AGC_ENABLED)
            .and_then(serde_json::Value::as_bool)
            .map_or(AgcType::DEFAULT, |on| {
                if on { AgcType::Hardware } else { AgcType::Off }
            })
    })
}

/// Persist the AGC type selection. Written on every
/// `agc_row.connect_selected_notify` event in `window.rs`.
/// Does NOT write the legacy `KEY_LEGACY_AGC_ENABLED` key —
/// that one is read-only from here on, so a downgrade to a
/// pre-#354 build would see a stale legacy value, but we
/// accept that trade-off rather than maintaining two keys in
/// lockstep forever.
pub fn save_agc_type(config: &Arc<ConfigManager>, agc_type: AgcType) {
    config.write(|v| {
        v[KEY_AGC_TYPE] = serde_json::to_value(agc_type).unwrap_or(serde_json::Value::Null);
    });
}

/// Load the persisted bias-T toggle. Defaults to `false` —
/// users without powered antennas should never have 5 V on
/// the coax accidentally on first launch. Per issue #537.
#[must_use]
pub fn load_source_rtl_bias_tee(config: &Arc<ConfigManager>) -> bool {
    config.read(|v| {
        v.get(KEY_SOURCE_RTL_BIAS_TEE)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}

/// Persist the bias-T toggle. Written on every
/// `bias_tee_row.connect_active_notify` event in
/// `window.rs::connect_source_panel`. Per issue #537.
pub fn save_source_rtl_bias_tee(config: &Arc<ConfigManager>, enabled: bool) {
    config.write(|v| {
        v[KEY_SOURCE_RTL_BIAS_TEE] = serde_json::json!(enabled);
    });
}

/// Read the persisted upconverter offset in Hz (0.0 when unset).
#[must_use]
pub fn load_source_converter_offset_hz(config: &Arc<ConfigManager>) -> f64 {
    config.read(|v| {
        v.get(KEY_SOURCE_CONVERTER_OFFSET_HZ)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0)
    })
}

/// Persist the upconverter offset in Hz.
pub fn save_source_converter_offset_hz(config: &Arc<ConfigManager>, offset_hz: f64) {
    config.write(|v| {
        v[KEY_SOURCE_CONVERTER_OFFSET_HZ] = serde_json::json!(offset_hz);
    });
}

/// Load the persisted Airspy serial; `None` = first available.
pub fn load_airspy_serial(config: &Arc<ConfigManager>) -> Option<u64> {
    config.read(|v| {
        v.get(KEY_AIRSPY_SERIAL)
            .and_then(serde_json::Value::as_str)
            .and_then(sdr_source_airspy::parse_device_serial)
    })
}

/// Persist the Airspy serial selection (`None` clears to "first
/// available").
pub fn save_airspy_serial(config: &Arc<ConfigManager>, serial: Option<u64>) {
    config.write(|v| {
        v[KEY_AIRSPY_SERIAL] = serde_json::json!(
            serial
                .map(sdr_source_airspy::format_device_serial)
                .unwrap_or_default()
        );
    });
}

/// Load the persisted RTL2832 direct-sampling combo index.
/// Defaults to [`DIRECT_SAMPLING_DISABLED_IDX`] when the key is
/// missing, the value isn't numeric, or the parsed value falls
/// outside the legal range `0..=DIRECT_SAMPLING_MAX_IDX` (e.g.
/// a future build added more modes and the user rolled back).
/// Per issue #538.
#[must_use]
pub fn load_source_rtl_direct_sampling_mode(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
        v.get(KEY_SOURCE_RTL_DIRECT_SAMPLING_MODE)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&idx| idx <= DIRECT_SAMPLING_MAX_IDX)
            .unwrap_or(DIRECT_SAMPLING_DISABLED_IDX)
    })
}

/// Persist the direct-sampling combo index. Written on every
/// `direct_sampling_row.connect_selected_notify` event in
/// `window.rs::connect_source_panel`. Per issue #538.
pub fn save_source_rtl_direct_sampling_mode(config: &Arc<ConfigManager>, mode: u32) {
    config.write(|v| {
        v[KEY_SOURCE_RTL_DIRECT_SAMPLING_MODE] = serde_json::json!(mode);
    });
}

/// Load the persisted offset-tuning toggle. Defaults to `false`
/// — most R820T-family dongles ignore the setting anyway, and a
/// false default keeps tuning behavior predictable across
/// hardware variants. Per issue #539.
#[must_use]
pub fn load_source_rtl_offset_tuning(config: &Arc<ConfigManager>) -> bool {
    config.read(|v| {
        v.get(KEY_SOURCE_RTL_OFFSET_TUNING)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}

/// Persist the offset-tuning toggle. Written on every
/// `offset_tuning_row.connect_active_notify` event in
/// `window.rs::connect_source_panel`. Per issue #539.
pub fn save_source_rtl_offset_tuning(config: &Arc<ConfigManager>, enabled: bool) {
    config.write(|v| {
        v[KEY_SOURCE_RTL_OFFSET_TUNING] = serde_json::json!(enabled);
    });
}

/// Load the persisted manual tuner gain in dB. Default `0.0` —
/// matches the spin row's initial value. Per issue `#551`.
#[must_use]
pub fn load_source_rtl_gain_db(config: &Arc<ConfigManager>) -> f64 {
    config.read(|v| {
        v.get(KEY_SOURCE_RTL_GAIN_DB)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0)
    })
}

/// Persist the manual tuner gain in dB. Per issue `#551`.
pub fn save_source_rtl_gain_db(config: &Arc<ConfigManager>, gain_db: f64) {
    config.write(|v| {
        v[KEY_SOURCE_RTL_GAIN_DB] = serde_json::json!(gain_db);
    });
}

/// Load the persisted PPM frequency correction. Default `0`.
/// Per issue `#551`.
#[must_use]
pub fn load_source_rtl_ppm(config: &Arc<ConfigManager>) -> i32 {
    config.read(|v| {
        v.get(KEY_SOURCE_RTL_PPM)
            .and_then(serde_json::Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(0)
    })
}

/// Persist the PPM frequency correction. Per issue `#551`.
pub fn save_source_rtl_ppm(config: &Arc<ConfigManager>, ppm: i32) {
    config.write(|v| {
        v[KEY_SOURCE_RTL_PPM] = serde_json::json!(ppm);
    });
}

// ─── #552 source-panel persistence helpers ──────────────────────────
//
// All follow the same shape as `load_source_rtl_*` /
// `save_source_rtl_*`: a tolerant `load` that falls back to a
// safe default on missing-key or wrong-type, paired with an
// idempotent `save`. The wiring layer in
// `window.rs::connect_source_panel` calls each `save_*` from the
// row's change-notify handler and each `load_*` once at panel
// build time (restore-before-wire idiom).

/// Load the persisted source-type combo index. Defaults to
/// [`DEVICE_RTLSDR`] when the key is missing, the value isn't a
/// `u64`, or the parsed index falls outside `0..=DEVICE_AIRSPY`
/// (e.g. a future build added more source types and the user
/// rolled back). Per `CodeRabbit` round 1 on PR #558; bound raised
/// with the Airspy slot on PR #852 after the stale `DEVICE_RTLTCP`
/// clamp silently reverted a persisted Airspy selection to RTL-SDR
/// at startup.
#[must_use]
pub fn load_source_device_index(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
        v.get(KEY_SOURCE_DEVICE_INDEX)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&idx| idx <= DEVICE_AIRSPY)
            .unwrap_or(DEVICE_RTLSDR)
    })
}

pub fn save_source_device_index(config: &Arc<ConfigManager>, index: u32) {
    config.write(|v| {
        v[KEY_SOURCE_DEVICE_INDEX] = serde_json::json!(index);
    });
}

/// Load the persisted sample-rate combo index. Falls back to
/// [`DEFAULT_SAMPLE_RATE_INDEX`] (matches the widget's initial
/// selection at panel build time) when the key is missing, the
/// value isn't numeric, or the parsed index is out of range
/// (`>= SAMPLE_RATES.len()`). Per `CodeRabbit` round 1 on PR #558
/// — the prior literal-`0` fallback would silently downgrade a
/// fresh install to 250 kHz instead of the intended 2.4 MHz.
#[must_use]
pub fn load_source_sample_rate_index(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
        v.get(KEY_SOURCE_SAMPLE_RATE_INDEX)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&idx| (idx as usize) < SAMPLE_RATES.len())
            .unwrap_or(DEFAULT_SAMPLE_RATE_INDEX)
    })
}

pub fn save_source_sample_rate_index(config: &Arc<ConfigManager>, index: u32) {
    config.write(|v| {
        v[KEY_SOURCE_SAMPLE_RATE_INDEX] = serde_json::json!(index);
    });
}

/// Load the persisted decimation combo index. Defaults to `0`
/// (1× decimation, the widget's initial selection) when the key
/// is missing, the value isn't numeric, or the parsed index is
/// out of range (`>= DECIMATION_FACTORS.len()`). Per
/// `CodeRabbit` round 1 on PR #558.
#[must_use]
pub fn load_source_decimation_index(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
        v.get(KEY_SOURCE_DECIMATION_INDEX)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&idx| (idx as usize) < DECIMATION_FACTORS.len())
            .unwrap_or(0)
    })
}

pub fn save_source_decimation_index(config: &Arc<ConfigManager>, index: u32) {
    config.write(|v| {
        v[KEY_SOURCE_DECIMATION_INDEX] = serde_json::json!(index);
    });
}

/// Load the persisted DC-blocking toggle. Defaults to `true`
/// (matches the widget's initial state).
#[must_use]
pub fn load_source_dc_blocking(config: &Arc<ConfigManager>) -> bool {
    config.read(|v| {
        v.get(KEY_SOURCE_DC_BLOCKING)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    })
}

pub fn save_source_dc_blocking(config: &Arc<ConfigManager>, enabled: bool) {
    config.write(|v| {
        v[KEY_SOURCE_DC_BLOCKING] = serde_json::json!(enabled);
    });
}

/// Load the persisted IQ-correction toggle. Defaults to `false`.
#[must_use]
pub fn load_source_iq_correction(config: &Arc<ConfigManager>) -> bool {
    config.read(|v| {
        v.get(KEY_SOURCE_IQ_CORRECTION)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}

pub fn save_source_iq_correction(config: &Arc<ConfigManager>, enabled: bool) {
    config.write(|v| {
        v[KEY_SOURCE_IQ_CORRECTION] = serde_json::json!(enabled);
    });
}

/// Load the persisted IQ-swap toggle. Defaults to `false`.
#[must_use]
pub fn load_source_iq_inversion(config: &Arc<ConfigManager>) -> bool {
    config.read(|v| {
        v.get(KEY_SOURCE_IQ_INVERSION)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}

pub fn save_source_iq_inversion(config: &Arc<ConfigManager>, enabled: bool) {
    config.write(|v| {
        v[KEY_SOURCE_IQ_INVERSION] = serde_json::json!(enabled);
    });
}

/// Load the persisted raw-Network hostname. Defaults to
/// [`super::DEFAULT_NETWORK_HOSTNAME`] — the same constant the
/// widget's initial value uses, so the two can't drift.
#[must_use]
pub fn load_source_network_hostname(config: &Arc<ConfigManager>) -> String {
    config.read(|v| {
        v.get(KEY_SOURCE_NETWORK_HOSTNAME)
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || super::DEFAULT_NETWORK_HOSTNAME.to_string(),
                ToString::to_string,
            )
    })
}

pub fn save_source_network_hostname(config: &Arc<ConfigManager>, hostname: &str) {
    config.write(|v| {
        v[KEY_SOURCE_NETWORK_HOSTNAME] = serde_json::json!(hostname);
    });
}

/// Load the persisted raw-Network port. Defaults to
/// [`super::DEFAULT_PORT_U16`] — the wire form `DEFAULT_PORT`
/// itself derives from, so widget and fallback can't drift.
#[must_use]
pub fn load_source_network_port(config: &Arc<ConfigManager>) -> u16 {
    config.read(|v| {
        v.get(KEY_SOURCE_NETWORK_PORT)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .unwrap_or(super::DEFAULT_PORT_U16)
    })
}

pub fn save_source_network_port(config: &Arc<ConfigManager>, port: u16) {
    config.write(|v| {
        v[KEY_SOURCE_NETWORK_PORT] = serde_json::json!(port);
    });
}

/// Load the persisted raw-Network protocol combo index. Defaults
/// to [`NETWORK_PROTOCOL_TCPCLIENT_IDX`] when the key is missing,
/// the value isn't numeric, or the parsed index falls outside
/// `0..=NETWORK_PROTOCOL_UDP_IDX`. Per `CodeRabbit` round 1 on
/// PR #558.
#[must_use]
pub fn load_source_network_protocol_index(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
        v.get(KEY_SOURCE_NETWORK_PROTOCOL_INDEX)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&idx| idx <= NETWORK_PROTOCOL_UDP_IDX)
            .unwrap_or(NETWORK_PROTOCOL_TCPCLIENT_IDX)
    })
}

pub fn save_source_network_protocol_index(config: &Arc<ConfigManager>, index: u32) {
    config.write(|v| {
        v[KEY_SOURCE_NETWORK_PROTOCOL_INDEX] = serde_json::json!(index);
    });
}

/// Load the persisted File-source playback path. Defaults to
/// the empty string (no file selected).
#[must_use]
pub fn load_source_file_path(config: &Arc<ConfigManager>) -> String {
    config.read(|v| {
        v.get(KEY_SOURCE_FILE_PATH)
            .and_then(serde_json::Value::as_str)
            .map_or_else(String::new, ToString::to_string)
    })
}

pub fn save_source_file_path(config: &Arc<ConfigManager>, path: &str) {
    config.write(|v| {
        v[KEY_SOURCE_FILE_PATH] = serde_json::json!(path);
    });
}
