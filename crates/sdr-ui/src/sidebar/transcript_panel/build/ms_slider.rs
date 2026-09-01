//! The persisted-milliseconds `AdwSpinRow` builder shared by the
//! three Auto Break timing sliders (issue #819): [`MsSliderSpec`]
//! and [`build_persisted_ms_slider`]. Split out of `build.rs` per
//! the 500-NLOC file gate.

#[cfg(feature = "sherpa")]
use std::sync::Arc;

#[cfg(feature = "sherpa")]
use libadwaita as adw;
#[cfg(feature = "sherpa")]
use libadwaita::prelude::*;
#[cfg(feature = "sherpa")]
use sdr_config::ConfigManager;

#[cfg(feature = "sherpa")]
use super::super::{AUTO_BREAK_MS_PAGE, AUTO_BREAK_MS_STEP};

// `pub(super)` (not private) throughout: the spec literals and the
// builder's call sites live in the parent `build.rs`, which
// constructs one `MsSliderSpec` per Auto Break slider.
/// Specification for a persisted-millisecond `AdwSpinRow`. Used by
/// [`build_persisted_ms_slider`] to construct the three Auto Break
/// timing sliders from one code path.
#[cfg(feature = "sherpa")]
pub(super) struct MsSliderSpec {
    /// Config-file JSON key where the value is persisted (`u64` ms).
    pub(super) key: &'static str,
    /// User-visible row title (e.g. "Auto Break: min open (ms)").
    pub(super) title: &'static str,
    /// User-visible row subtitle explaining what the knob does.
    pub(super) subtitle: &'static str,
    /// Inclusive minimum allowed slider value.
    pub(super) min: f64,
    /// Inclusive maximum allowed slider value.
    pub(super) max: f64,
    /// Default value shown when the config key is missing or invalid.
    pub(super) default: f64,
}

/// Build a sherpa-only persisted-milliseconds `AdwSpinRow` from a
/// [`MsSliderSpec`]. Shared shape for the three Auto Break timing
/// sliders (`min_open`, `tail`, `min_segment`) which all follow the
/// same load/clamp/build/persist pattern.
///
/// The `u64 ↔ f64` casts are bounded by `spec.min`/`spec.max` (both
/// well under 2^52 for any realistic slider range) so the conversions
/// are lossless in practice. Allows are scoped tight to this helper.
#[cfg(feature = "sherpa")]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(super) fn build_persisted_ms_slider(
    group: &adw::PreferencesGroup,
    config: &Arc<ConfigManager>,
    spec: &MsSliderSpec,
) -> adw::SpinRow {
    let saved = config.read(|v| {
        v.get(spec.key)
            .and_then(serde_json::Value::as_u64)
            .map_or(spec.default, |val| (val as f64).clamp(spec.min, spec.max))
    });

    let row = adw::SpinRow::builder()
        .title(spec.title)
        .subtitle(spec.subtitle)
        .adjustment(&gtk4::Adjustment::new(
            saved,
            spec.min,
            spec.max,
            AUTO_BREAK_MS_STEP,
            AUTO_BREAK_MS_PAGE,
            0.0,
        ))
        .digits(0)
        .build();
    group.add(&row);

    // Capture `spec.key` by Copy (it's `&'static str`) so the
    // GLib closure can own it without borrowing the spec.
    let cfg_clone = Arc::clone(config);
    let key = spec.key;
    row.connect_value_notify(move |r| {
        let val = r.value() as u64;
        cfg_clone.write(|v| {
            v[key] = serde_json::json!(val);
        });
    });
    row
}
