//! Satellites activity wiring: pass scheduling, auto-record, viewer
//! plumbing, SSTV batch export, and the Doppler tracker.

use gtk4::prelude::*;
use libadwaita::prelude::*;

mod doppler;
mod heard;
mod passes;
use passes::{DisplayedPass, build_recompute, wire_tle_refresh_button, wire_zip_lookup};
mod saves;
mod saves_lrpt;
pub(super) use saves::{compute_apt_rotate_180_for_pass, on_save_apt_png, on_save_sstv_pass};
pub(super) use saves_lrpt::on_save_lrpt_pass;
mod recorder;
mod tick;
use doppler::wire_doppler;
use tick::{SatWiring, wire_recorder};

use super::{
    Duration, Rc, RefCell, SidebarPanels, TuneCtx, TuneFn, adw, glib, plain_toast, sidebar,
};

/// Cadence of the Satellites panel's countdown ticker — 1 line/sec
/// is the smallest interval that produces a visible change in the
/// pass-row title (which renders to 1-minute granularity for far
/// passes and to seconds only inside the "starting now" window).
/// Smaller would burn cycles for no visible benefit.
pub(super) const SATELLITES_COUNTDOWN_TICK: Duration = Duration::from_secs(1);

/// Restore persisted panel values BEFORE the caller wires
/// change-notify handlers, matching the scanner-panel pattern:
/// `set_value` on a `SpinRow` fires `value-changed`, so wiring first
/// would trigger spurious saves + recomputes during window
/// construction.
fn seed_persisted_panel_values(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    use sidebar::satellites_panel::{
        format_last_refresh, load_auto_record_apt, load_auto_record_audio,
        load_auto_record_composites, load_auto_record_quality, load_notify_lead_min,
        load_station_alt_m, load_station_lat_deg, load_station_lon_deg,
    };
    // Restore persisted values BEFORE wiring change-notify handlers,
    // matching the scanner-panel pattern: `set_value` on a SpinRow
    // fires `value-changed`, so wiring first would trigger spurious
    // saves + recomputes during window construction.
    panel.lat_row.set_value(load_station_lat_deg(config));
    panel.lon_row.set_value(load_station_lon_deg(config));
    panel.alt_row.set_value(load_station_alt_m(config));
    panel
        .notify_lead_row
        .set_value(f64::from(load_notify_lead_min(config)));
    panel
        .auto_record_switch
        .set_active(load_auto_record_apt(config));
    panel
        .auto_record_audio_switch
        .set_active(load_auto_record_audio(config));
    panel
        .auto_record_composites_switch
        .set_active(load_auto_record_composites(config));
    let initial_quality = load_auto_record_quality(config);
    panel
        .auto_record_quality_row
        .set_selected(initial_quality.to_index());
    // Sensitivity is wired in `build_satellites_panel` via the
    // auto-record switch's `connect_active_notify` handler — it
    // fires on every toggle, including the one triggered above
    // by `auto_record_switch.set_active(load_auto_record_apt(...))`,
    // so the persisted switch state propagates to the combo's
    // sensitivity automatically. No re-sync needed here. Per CR
    // round 2 on PR #574.
    panel
        .last_refresh_row
        .set_subtitle(&format_last_refresh(config));
}

/// Persist the notify-lead `SpinRow` on change.
fn wire_notify_lead_persistence(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    let config_lead = std::sync::Arc::clone(config);
    panel.notify_lead_row.connect_value_notify(move |row| {
        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "SpinRow is bounded NOTIFY_LEAD_MIN_LOWER..=UPPER \
                      (positive, < u32::MAX)"
        )]
        let value = row.value().round() as u32;
        sidebar::satellites_panel::save_notify_lead_min(&config_lead, value);
    });
}

/// Wire the persist-only auto-record handlers: APT master toggle,
/// "also save audio" (#533), false-colour composites (#547), and the
/// quality threshold combo (validated through
/// `AutoRecordQuality::from_index` per CR round 1 on PR #574). The
/// recorder samples these widgets at AOS/LOS; flipping mid-pass does
/// not retroactively start or stop anything.
fn wire_auto_record_persistence(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    use sidebar::satellites_panel::{
        save_auto_record_apt, save_auto_record_audio, save_auto_record_composites,
    };
    // Auto-record toggle — persist only. The actual "tune the
    // radio + start APT decoding when a NOAA pass starts" wiring
    // lands in #482 and reads from the same config key.
    {
        let config_auto = std::sync::Arc::clone(config);
        panel.auto_record_switch.connect_active_notify(move |sw| {
            save_auto_record_apt(&config_auto, sw.is_active());
        });
    }

    // "Also save audio" toggle — persist only. The recorder's
    // 1 Hz tick samples this switch's `is_active()` at AOS.
    // Per #533.
    {
        let config_audio = std::sync::Arc::clone(config);
        panel
            .auto_record_audio_switch
            .connect_active_notify(move |sw| {
                save_auto_record_audio(&config_audio, sw.is_active());
            });
    }

    // "Save false-colour composites" toggle — persist only. The
    // `RecorderAction::SaveLrptPass` handler reads
    // `panel.auto_record_composites_switch.is_active()` at LOS
    // (mirrors the audio-save sampling pattern — flipping
    // mid-pass doesn't retroactively start or stop anything).
    // Per #547.
    {
        let config_comp = std::sync::Arc::clone(config);
        panel
            .auto_record_composites_switch
            .connect_active_notify(move |sw| {
                save_auto_record_composites(&config_comp, sw.is_active());
                tracing::info!(on = sw.is_active(), "auto_record_composites persisted");
            });
    }

    // Persist the quality threshold on change via the symmetric
    // writer. Validating through `AutoRecordQuality::from_index`
    // before the write protects the config against transient
    // out-of-range indices that GTK can emit during model churn —
    // an unrecognized value would round-trip back to the default
    // tier on the next read otherwise. Per CR round 1 on PR #574.
    {
        let config_quality = std::sync::Arc::clone(config);
        panel
            .auto_record_quality_row
            .connect_selected_notify(move |row| {
                let raw = row.selected();
                let quality = crate::sidebar::satellites_panel::AutoRecordQuality::from_index(raw);
                if quality.to_index() != raw {
                    // Transient model-churn value (e.g. mid-rebuild
                    // selection-cleared). Skip the write so we don't
                    // overwrite a valid persisted index with garbage.
                    tracing::debug!(raw, "auto_record_quality: ignoring transient combo index");
                    return;
                }
                crate::sidebar::satellites_panel::save_auto_record_quality(
                    &config_quality,
                    quality,
                );
                tracing::info!(idx = quality.to_index(), "auto_record_quality persisted");
            });
    }
}

/// Build the shared TLE cache. `None` means the platform refused us a
/// cache directory (rare; sandboxed minimal environments) — the caller
/// disables TLE-specific UI but keeps ground-station persistence, ZIP
/// lookup, and the auto-record toggle wired, since those don't depend
/// on TLEs.
fn init_tle_cache(
    panel: &sidebar::satellites_panel::SatellitesPanel,
) -> Option<std::sync::Arc<sdr_sat::TleCache>> {
    // `Option<Arc<TleCache>>`. `None` means the platform refused us
    // a cache directory (rare; sandboxed minimal environments).
    // Disable TLE-specific UI but keep ground-station persistence,
    // ZIP lookup, and the auto-record toggle wired — those don't
    // depend on TLEs and shouldn't go inert just because the cache
    // is gone.
    match sdr_sat::TleCache::new() {
        Ok(c) => Some(std::sync::Arc::new(c)),
        Err(e) => {
            tracing::warn!("Satellites panel: TLE cache unavailable — {e}");
            panel.refresh_button.set_sensitive(false);
            panel
                .last_refresh_row
                .set_subtitle("Cache directory unavailable");
            None
        }
    }
}

/// Lat / lon / alt — persist on change and re-run pass enumeration.
/// Cheap: a single SGP4 sweep across ~7 satellites takes well under a
/// millisecond.
fn wire_station_rows(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    recompute: &Rc<dyn Fn()>,
) {
    use sidebar::satellites_panel::{
        KEY_STATION_ALT_M, KEY_STATION_LAT_DEG, KEY_STATION_LON_DEG, save_f64,
    };
    // Lat / lon / alt — persist on change and re-run pass
    // enumeration. Cheap: a single SGP4 sweep across ~7
    // satellites takes well under a millisecond.
    {
        let config_lat = std::sync::Arc::clone(config);
        let recompute_lat = Rc::clone(recompute);
        panel.lat_row.connect_value_notify(move |row| {
            save_f64(&config_lat, KEY_STATION_LAT_DEG, row.value());
            recompute_lat();
        });
    }
    {
        let config_lon = std::sync::Arc::clone(config);
        let recompute_lon = Rc::clone(recompute);
        panel.lon_row.connect_value_notify(move |row| {
            save_f64(&config_lon, KEY_STATION_LON_DEG, row.value());
            recompute_lon();
        });
    }
    {
        let config_alt = std::sync::Arc::clone(config);
        let recompute_alt = Rc::clone(recompute);
        panel.alt_row.connect_value_notify(move |row| {
            save_f64(&config_alt, KEY_STATION_ALT_M, row.value());
            recompute_alt();
        });
    }
}

/// Wire the Satellites scheduler panel to its config-persistence
/// layer, the [`sdr_sat::TleCache`], and a 1 Hz countdown timer.
///
/// Two pieces of shared state plumb the handlers together:
///
/// * `displayed: Rc<RefCell<Vec<DisplayedPass>>>` — the list of
///   pass rows currently in `passes_group`. Walked by the 1 Hz
///   ticker (to update title-line countdowns) and rebuilt by
///   `recompute` whenever lat/lon/alt changes or a TLE refresh
///   completes.
/// * `cache: Arc<TleCache>` — `Arc` (not `Rc`) because the
///   refresh button hands a clone to `gio::spawn_blocking`, which
///   requires `Send`. `TleCache` is `Send + Sync`.
///
/// The 1 Hz timer holds a `glib::WeakRef<adw::PreferencesGroup>`
/// to the passes group so it returns `ControlFlow::Break` once
/// the window is destroyed; same lifecycle pattern as the DSP
/// poll loop.
#[allow(clippy::too_many_arguments)]
pub(super) fn connect_satellites_panel(
    panels: &SidebarPanels,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    tune_ctx: &TuneCtx,
    toast_overlay: &adw::ToastOverlay,
    tune_to_satellite: &Rc<TuneFn>,
    set_playing: &Rc<dyn Fn(bool)>,
) {
    use sidebar::satellites_panel::{SatellitesPanelWeak, load_watched_satellites};

    let state = &tune_ctx.state;
    let status_bar = &tune_ctx.status_bar;

    // Borrow the panel for synchronous setup, then capture only
    // weak refs in long-lived closures. Cloning the strong panel
    // into a closure stored on its own widget creates a refcount
    // cycle (widget → handler → closure → cloned panel → widget)
    // that prevents teardown — see `SatellitesPanelWeak`'s doc for
    // the full chain.
    let panel = &panels.satellites;
    let panel_weak: SatellitesPanelWeak = panel.downgrade();

    seed_persisted_panel_values(panel, config);

    wire_notify_lead_persistence(panel, config);

    let cache = init_tle_cache(panel);

    let displayed: Rc<RefCell<Vec<DisplayedPass>>> = Rc::new(RefCell::new(Vec::new()));

    // #510 — per-satellite watched-set + notify scheduler. Loaded
    // from config so the user's selections survive restarts. The
    // set is mutated from two sites: (a) the bell toggle on each
    // pass row (write-through to config); (b) read-only by the
    // 1 Hz tick that drives the scheduler.
    let watched: Rc<RefCell<std::collections::HashSet<u32>>> =
        Rc::new(RefCell::new(load_watched_satellites(config)));

    let recompute = build_recompute(
        cache.as_ref(),
        &panel_weak,
        &displayed,
        tune_to_satellite,
        &watched,
        config,
    );

    // Initial paint — show passes immediately if we already have
    // cached TLEs from a prior session. (No-op if cache is None.)
    wire_station_rows(panel, config, &recompute);

    wire_auto_record_persistence(panel, config);

    // Doppler-correction tracker (#521).
    //
    // Two-layer wiring:
    //   1. `restore_doppler_switch` runs ALWAYS — restores the
    //      persisted master-switch value to the widget and
    //      wires its change-notify to save back. This way the
    //      user's preference survives a launch even when the
    //      TLE cache is unavailable. Per CR round 1 on PR #554.
    //   2. `connect_doppler_tracker` runs only when the TLE
    //      cache is available — without TLEs we can't propagate
    //      SGP4 to evaluate the trigger or compute the offset,
    //      so there's nothing for the *behavior* to do.
    wire_doppler(panels, state, config, cache.as_ref(), status_bar);

    wire_tle_refresh_button(panel, cache.as_ref(), config, &panel_weak, &recompute);

    wire_zip_lookup(panel, &panel_weak);

    heard::wire_heard_group(&panel_weak, state);

    let wiring = SatWiring {
        panel_weak,
        displayed,
        recompute,
        watched,
        cache,
    };
    wire_recorder(
        panels,
        tune_ctx,
        config,
        toast_overlay,
        tune_to_satellite,
        set_playing,
        &wiring,
    );
}

/// Toast through a weak overlay handle — no-op when the window is
/// gone.
fn post_toast(overlay_weak: &glib::WeakRef<adw::ToastOverlay>, msg: &str) {
    if let Some(overlay) = overlay_weak.upgrade() {
        overlay.add_toast(plain_toast(msg));
    }
}
