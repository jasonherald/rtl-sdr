//! Satellites activity wiring: pass scheduling, auto-record, viewer
//! plumbing, SSTV batch export, and the Doppler tracker.

use gtk4::prelude::*;
use libadwaita::prelude::*;

mod doppler;
mod saves;
mod saves_lrpt;
pub(super) use saves::{compute_apt_rotate_180_for_pass, on_save_apt_png, on_save_sstv_pass};
pub(super) use saves_lrpt::on_save_lrpt_pass;
mod recorder;
mod tick;
use doppler::wire_doppler;
use tick::{SatWiring, wire_recorder};

use super::{
    Duration, Rc, RefCell, SidebarPanels, TuneCtx, TuneFn, adw, gio, glib, plain_toast, sidebar,
};

/// Cadence of the Satellites panel's countdown ticker — 1 line/sec
/// is the smallest interval that produces a visible change in the
/// pass-row title (which renders to 1-minute granularity for far
/// passes and to seconds only inside the "starting now" window).
/// Smaller would burn cycles for no visible benefit.
pub(super) const SATELLITES_COUNTDOWN_TICK: Duration = Duration::from_secs(1);

/// One pass row + its source `Pass` so the 1 Hz ticker can refresh
/// the title without re-running pass enumeration. The optional bell
/// `ToggleButton` is held so the watch-toggle handler can mirror the
/// active state across every row whose satellite matches — multiple
/// NOAA 19 passes in the visible list must all reflect the same
/// subscription state. `None` for off-catalog passes (no NORAD id →
/// no notify).
struct DisplayedPass {
    row: adw::ActionRow,
    pass: sdr_sat::Pass,
    bell_btn: Option<gtk4::ToggleButton>,
}

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

/// Build the pass-list recompute closure. Built unconditionally —
/// when the TLE cache is unavailable it's a no-op so the lat/lon/alt
/// notify handlers can call it without branching; with a cache it does
/// the real pass-enumeration + row-rebuild work.
fn build_recompute(
    cache: Option<&std::sync::Arc<sdr_sat::TleCache>>,
    panel_weak: &sidebar::satellites_panel::SatellitesPanelWeak,
    displayed: &Rc<RefCell<Vec<DisplayedPass>>>,
    tune_to_satellite: &Rc<TuneFn>,
    watched: &Rc<RefCell<std::collections::HashSet<u32>>>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) -> Rc<dyn Fn()> {
    // `recompute` is built unconditionally — when the cache is
    // unavailable it's a no-op so the lat/lon/alt notify handlers
    // can call it without branching. When the cache is available
    // it does the real pass-enumeration + row-rebuild work.
    let recompute: Rc<dyn Fn()> = if let Some(cache) = cache {
        let cache_recompute = std::sync::Arc::clone(cache);
        let panel_weak_recompute = panel_weak.clone();
        let displayed_recompute = Rc::clone(displayed);
        let tune_for_recompute = Rc::clone(tune_to_satellite);
        let watched_for_recompute = Rc::clone(watched);
        let config_for_recompute = std::sync::Arc::clone(config);
        Rc::new(move || {
            let Some(panel) = panel_weak_recompute.upgrade() else {
                return;
            };
            rebuild_pass_rows(
                &panel,
                &cache_recompute,
                &displayed_recompute,
                &tune_for_recompute,
                &watched_for_recompute,
                &config_for_recompute,
            );
        })
    } else {
        // No cache → no enumeration. Lat/lon/alt notify handlers
        // still call this on every change; making it a no-op
        // keeps the call sites branch-free.
        Rc::new(|| {})
    };
    // Initial paint — show passes immediately if we already have
    // cached TLEs from a prior session. (No-op when cache is None.)
    recompute();
    recompute
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

// Refresh button — re-download every known satellite's TLE on
// a worker thread, update the timestamp row, and rebuild the
// pass list. Same `spawn_future_local` + `spawn_blocking`
// pattern as the RadioReference search button. Wired only
// when the cache is available; otherwise the button was
// already disabled above.
fn wire_tle_refresh_button(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    cache: Option<&std::sync::Arc<sdr_sat::TleCache>>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    panel_weak: &sidebar::satellites_panel::SatellitesPanelWeak,
    recompute: &Rc<dyn Fn()>,
) {
    let Some(cache_outer) = cache else {
        // No cache — the button was already disabled by
        // `init_tle_cache`; nothing to wire.
        return;
    };
    let cache_refresh = std::sync::Arc::clone(cache_outer);
    let config_refresh = std::sync::Arc::clone(config);
    let panel_weak_refresh = panel_weak.clone();
    let recompute_refresh = Rc::clone(recompute);
    panel.refresh_button.connect_clicked(move |_| {
        let Some(panel) = panel_weak_refresh.upgrade() else {
            return;
        };
        panel.refresh_spinner.set_visible(true);
        panel.refresh_spinner.start();
        panel.refresh_button.set_sensitive(false);

        let cache_task = std::sync::Arc::clone(&cache_refresh);
        let config_done = std::sync::Arc::clone(&config_refresh);
        let panel_weak_done = panel_weak_refresh.clone();
        let recompute_done = Rc::clone(&recompute_refresh);

        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || force_refresh_all_tles(&cache_task)).await;

            let Some(panel) = panel_weak_done.upgrade() else {
                return;
            };
            panel.refresh_spinner.stop();
            panel.refresh_spinner.set_visible(false);
            panel.refresh_button.set_sensitive(true);

            finish_tle_refresh(&panel, &config_done, &recompute_done, result);
        });
    });
}

/// Wire the auto-record machinery: the pure [`AutoRecorder`] state
/// machine, its action interpreter (stashed weakly on `AppState` so
/// the `AcarsEnabledChanged(Ok(false))` arm in `handle_dsp_message`
/// can replay deferred AOS actions — issue #589 / CR round 1 on
/// PR #591; the strong owner is the tick source), and the 1 Hz tick
/// that drives it. The tick is only armed when a TLE cache exists —
/// without one `displayed` stays empty forever and the timer would
/// tick uselessly.

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

/// ZIP code → lat/lon shortcut. Both `apply` (apply-button click /
/// Enter when the apply button is sensitive) and `entry_activated`
/// (Enter, unconditional) are wired to the same closure, deduped by
/// an in-flight flag. Split out per the 50-NLOC gate (#817).
fn wire_zip_lookup(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    panel_weak: &sidebar::satellites_panel::SatellitesPanelWeak,
) {
    // ZIP code → lat/lon shortcut. We wire BOTH `apply` (apply
    // button click / Enter when apply-button is sensitive) and
    // `entry_activated` (Enter, unconditional), then dedupe by an
    // "in-flight" flag — `apply` won't fire if AdwEntryRow's
    // internal "has the text been edited?" tracking is in a state
    // where the apply button is insensitive, but `entry_activated`
    // fires on Enter regardless. Belt-and-braces is cheaper than
    // chasing libadwaita's internal sensitivity rules.
    //
    // Result text goes to `zip_status_row` (AdwEntryRow has no
    // subtitle slot of its own). Wired regardless of TLE cache
    // availability — the ZIP lookup is independent.
    let in_flight: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
    let run_lookup: Rc<dyn Fn(adw::EntryRow)> = {
        let panel_weak_zip = panel_weak.clone();
        let in_flight_run = Rc::clone(&in_flight);
        Rc::new(move |entry: adw::EntryRow| {
            on_zip_lookup_requested(&entry, &panel_weak_zip, &in_flight_run);
        })
    };
    // Wire both signal paths to the same closure: `apply` for
    // the apply button click (when libadwaita has flagged the
    // text as edited), `entry-activated` for raw Enter keys
    // (always fires, regardless of edit state).
    {
        let run = Rc::clone(&run_lookup);
        panel.zip_row.connect_apply(move |entry| run(entry.clone()));
    }
    {
        let run = Rc::clone(&run_lookup);
        panel
            .zip_row
            .connect_entry_activated(move |entry| run(entry.clone()));
    }
}

/// One ZIP-lookup run: in-flight dedupe, trim, kick the async chain,
/// and re-enable the row + paint the status when the result lands.
/// Split out per the 50-NLOC gate (#817).
fn on_zip_lookup_requested(
    entry: &adw::EntryRow,
    panel_weak_zip: &sidebar::satellites_panel::SatellitesPanelWeak,
    in_flight_run: &Rc<std::cell::Cell<bool>>,
) {
    if in_flight_run.get() {
        tracing::debug!("Satellites: ZIP lookup ignored — already in flight");
        return;
    }
    let Some(panel) = panel_weak_zip.upgrade() else {
        return;
    };
    // Trim once, here, so the trimmed value is what
    // flows through the lookup. `lookup_us_zip` does its
    // own trim internally too, but a paste of "  24068 "
    // showing up as `length=8` in the debug log reads
    // worse than `length=5`.
    let zip = entry.text().trim().to_string();
    if zip.is_empty() {
        // Empty entry — nothing to do; treat as a no-op so a
        // stray Enter doesn't reset the status row.
        return;
    }
    in_flight_run.set(true);
    tracing::debug!("Satellites: ZIP lookup triggered (length={})", zip.len());
    entry.set_sensitive(false);
    panel.zip_status_row.set_title("Looking up…");

    let panel_weak_done = panel_weak_zip.clone();
    let in_flight_done = Rc::clone(in_flight_run);
    let zip_for_task = zip.clone();
    glib::spawn_future_local(async move {
        // Chain the two lookups on the worker thread so the
        // UI side just gets one result. ZIP failure is fatal
        // for this run; elevation failure is logged and
        // demoted to `Ok(_, None)` — altitude is best-effort
        // since it barely matters for pass prediction
        // anyway, and we'd rather populate lat/lon than
        // leave the user staring at an error toast.
        let result = gio::spawn_blocking(move || lookup_zip_and_elevation(&zip_for_task)).await;

        in_flight_done.set(false);
        let Some(panel) = panel_weak_done.upgrade() else {
            return;
        };
        panel.zip_row.set_sensitive(true);

        match result {
            Ok(Ok((loc, elevation))) => apply_zip_result(&panel, loc, elevation),
            Ok(Err(e)) => {
                // Don't include the ZIP in the log — user
                // location data, already surfaced inline in
                // the status row. Provider error alone is
                // enough.
                tracing::warn!("ZIP lookup failed: {e}");
                panel.zip_status_row.set_title(&e.to_string());
            }
            Err(_) => {
                tracing::warn!("ZIP lookup task panicked");
                panel
                    .zip_status_row
                    .set_title("Lookup failed: background task panicked");
            }
        }
    });
}

/// Chain the ZIP → lat/lon and elevation lookups on the worker
/// thread so the UI side gets one result. ZIP failure is fatal for
/// the run; elevation failure is logged (without lat/lon — user
/// location data) and demoted to `Err(String)` so the provider error
/// still reaches the UI while lat/lon populate anyway — altitude is
/// best-effort and barely matters for pass prediction.
#[allow(clippy::type_complexity)]
fn lookup_zip_and_elevation(
    zip: &str,
) -> Result<(sdr_sat::PostalLocation, Result<f64, String>), sdr_sat::PostalLookupError> {
    let loc = sdr_sat::lookup_us_zip(zip)?;
    let elevation = match sdr_sat::lookup_elevation_m(loc.lat_deg, loc.lon_deg) {
        Ok(m) => Ok(m),
        Err(e) => {
            tracing::warn!("elevation lookup failed: {e}");
            Err(e.to_string())
        }
    };
    Ok((loc, elevation))
}

/// Apply a successful ZIP lookup to the station rows. Order matters
/// slightly: setting lat/lon/alt fires `value-notify`, which persists
/// the value and triggers `recompute`; three recomputes back-to-back
/// are sub-millisecond each. An elevation failure leaves altitude
/// alone but surfaces the provider error so the user knows what to
/// try next.
fn apply_zip_result(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    loc: sdr_sat::PostalLocation,
    elevation: Result<f64, String>,
) {
    panel.lat_row.set_value(loc.lat_deg);
    panel.lon_row.set_value(loc.lon_deg);
    let where_text = if loc.region.is_empty() {
        loc.place
    } else {
        format!("{place}, {region}", place = loc.place, region = loc.region)
    };
    let status = match elevation {
        Ok(alt_m) => {
            panel.alt_row.set_value(alt_m);
            format!("Resolved: {where_text} ({alt_m:.0} m)")
        }
        Err(e) => format!("Resolved: {where_text} (altitude unchanged: {e})"),
    };
    panel.zip_status_row.set_title(&status);
}

/// Force-refresh every catalog satellite's TLE. `force_refresh` — NOT
/// `tle_text` — because the user clicked Refresh and a fresh-cache
/// fast-path would let us mark "Last refreshed: now" without any
/// actual network fetch; a successful return means a real round trip
/// happened. A per-satellite failure is logged and skipped so a
/// single decommissioned / rate-limited entry can't break the whole
/// refresh; only an all-fail sweep surfaces the last error.
fn force_refresh_all_tles(
    cache: &std::sync::Arc<sdr_sat::TleCache>,
) -> Result<(), sdr_sat::TleCacheError> {
    use sdr_sat::KNOWN_SATELLITES;

    let mut last_err: Option<sdr_sat::TleCacheError> = None;
    let mut succeeded = 0usize;
    for known in KNOWN_SATELLITES {
        match cache.force_refresh(known.norad_id) {
            Ok(_) => succeeded += 1,
            Err(e) => {
                tracing::warn!(
                    "TLE refresh for {} (NORAD {}) failed: {e}",
                    known.name,
                    known.norad_id,
                );
                last_err = Some(e);
            }
        }
    }
    if succeeded == 0 {
        Err(last_err.unwrap_or_else(|| {
            sdr_sat::TleCacheError::Fetch("refresh produced no successful fetches".to_string())
        }))
    } else {
        Ok(())
    }
}

/// Completion side of a TLE refresh: persist + show the new timestamp
/// and re-enumerate passes on success; surface the failure (or a
/// panicked background task) on the last-refresh row otherwise.
fn finish_tle_refresh(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    recompute: &Rc<dyn Fn()>,
    result: Result<Result<(), sdr_sat::TleCacheError>, Box<dyn std::any::Any + Send>>,
) {
    use sidebar::satellites_panel::{format_last_refresh, save_tle_last_refresh};

    match result {
        Ok(Ok(())) => {
            let now = chrono::Utc::now();
            save_tle_last_refresh(config, now);
            panel
                .last_refresh_row
                .set_subtitle(&format_last_refresh(config));
            recompute();
        }
        Ok(Err(e)) => {
            tracing::warn!("TLE refresh failed: {e}");
            panel
                .last_refresh_row
                .set_subtitle(&format!("Refresh failed: {e}"));
        }
        Err(_) => {
            tracing::warn!("TLE refresh task panicked");
            panel
                .last_refresh_row
                .set_subtitle("Refresh failed: background task panicked");
        }
    }
}

/// Rebuild the upcoming-passes list: drop the previous rows, run a
/// fresh SGP4 enumeration for the panel's ground station, and build
/// one row (title/subtitle + play + bell) per pass. Split out per the
/// 50-NLOC gate (#817).
fn rebuild_pass_rows(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    cache: &std::sync::Arc<sdr_sat::TleCache>,
    displayed: &Rc<RefCell<Vec<DisplayedPass>>>,
    tune: &Rc<TuneFn>,
    watched: &Rc<RefCell<std::collections::HashSet<u32>>>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    use sdr_sat::GroundStation;
    use sidebar::satellites_panel::{
        enumerate_upcoming_passes, format_pass_subtitle, format_pass_title,
    };

    // Drop the previous pass rows — these are throwaway,
    // built fresh per recompute.
    for entry in displayed.borrow_mut().drain(..) {
        panel.passes_group.remove(&entry.row);
    }
    // `passes_status_row` is the always-present empty-state
    // placeholder. We toggle its *visibility* rather than
    // detach + reattach. Once a widget is unparented, its
    // last strong ref lives only on the SatellitesPanel
    // struct — and that struct is dropped when
    // `build_window` returns. Toggling visibility keeps the
    // row parented (and therefore alive) for the lifetime
    // of the window.
    let station = GroundStation::new(
        panel.lat_row.value(),
        panel.lon_row.value(),
        panel.alt_row.value(),
    );
    let now = chrono::Utc::now();
    let passes = enumerate_upcoming_passes(cache, &station, now);

    if passes.is_empty() {
        panel.passes_status_row.set_visible(true);
        return;
    }

    panel.passes_status_row.set_visible(false);
    let mut new_rows = Vec::with_capacity(passes.len());
    for pass in passes {
        let row = adw::ActionRow::builder()
            .title(format_pass_title(&pass, now))
            .subtitle(format_pass_subtitle(&pass))
            .build();
        // Per-row play button — one-click tune to the
        // satellite's downlink with the right demod / BW.
        // Skipped when the satellite isn't in the catalog
        // (impossible in practice but the lookup type is
        // `Option`, so we fail closed — no button rather
        // than a button that does nothing).
        // Per-row play button: ignore the 4th element
        // (`Option<ImagingProtocol>`) — manual tune is a
        // user-initiated action and works on any catalog
        // entry. Only the auto-record path filters on
        // `Some(protocol)`.
        attach_pass_play_button(&row, &pass, tune);
        // 🔔 watch-toggle (#510) — per-satellite, NOT
        // per-pass. Toggling on row N flips the user's
        // subscription for THIS satellite. Mirrored across
        // sibling rows in the toggle handler so two rows
        // of the same satellite (NOAA 19 typically has 4-6
        // passes per day) stay in sync. `None` for
        // off-catalog passes — no NORAD id, no
        // notification target, no button.
        let bell_btn = build_pass_bell_button(&row, &pass, watched, config, displayed);
        panel.passes_group.add(&row);
        new_rows.push(DisplayedPass {
            row,
            pass,
            bell_btn,
        });
    }
    *displayed.borrow_mut() = new_rows;
}

/// 🔔 watch-toggle (#510) — per-satellite, NOT per-pass. Toggling on
/// row N flips the user's subscription for THIS satellite; the toggle
/// handler mirrors the active state across sibling rows (NOAA 19
/// typically has 4-6 passes per day) so they stay in sync. Returns
/// `None` for off-catalog passes — no NORAD id, no notification
/// target, no button.
fn build_pass_bell_button(
    row: &adw::ActionRow,
    pass: &sdr_sat::Pass,
    watched: &Rc<RefCell<std::collections::HashSet<u32>>>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    displayed: &Rc<RefCell<Vec<DisplayedPass>>>,
) -> Option<gtk4::ToggleButton> {
    use sidebar::satellites_panel::norad_id_for_pass;

    let norad_id = norad_id_for_pass(pass)?;
    let initial_active = watched.borrow().contains(&norad_id);
    let bell_btn = gtk4::ToggleButton::builder()
        .icon_name("alarm-symbolic")
        .active(initial_active)
        .tooltip_text(format!(
            "Notify before {} passes (T-pre-pass alert)",
            pass.satellite,
        ))
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    let a11y_label = format!("Notify before {} passes", pass.satellite);
    bell_btn.update_property(&[gtk4::accessible::Property::Label(a11y_label.as_str())]);
    let watched_for_toggle = Rc::clone(watched);
    let config_for_toggle = std::sync::Arc::clone(config);
    // `Weak` (not strong `Rc`) breaks the cycle:
    // bell_btn → handler closure → Rc → Vec
    // <DisplayedPass> → bell_btn. With a strong ref
    // here, removing rows from `passes_group`
    // wouldn't drop the bell_btn, which would keep
    // the closure (and the Vec) pinned forever.
    let displayed_for_toggle = Rc::downgrade(displayed);
    bell_btn.connect_toggled(move |b| {
        on_pass_bell_toggled(
            b,
            norad_id,
            &watched_for_toggle,
            &config_for_toggle,
            &displayed_for_toggle,
        );
    });
    row.add_suffix(&bell_btn);
    Some(bell_btn)
}

/// Toggle body of a pass bell: flip the per-satellite subscription
/// (persisting only on real membership change — sibling mirroring
/// re-enters this handler; CR round 3 on PR #568), then mirror the
/// state across sibling rows by NORAD id (CR round 1 on PR #568).
fn on_pass_bell_toggled(
    b: &gtk4::ToggleButton,
    norad_id: u32,
    watched: &Rc<RefCell<std::collections::HashSet<u32>>>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    displayed: &std::rc::Weak<RefCell<Vec<DisplayedPass>>>,
) {
    use sidebar::satellites_panel::{norad_id_for_pass, save_watched_satellites};

    let active = b.is_active();
    {
        let mut set = watched.borrow_mut();
        // `HashSet::insert` / `HashSet::remove`
        // return whether membership actually
        // changed. Skip the config write when it
        // didn't — sibling-mirror re-enters this
        // handler for every other row of the
        // same satellite, and without the guard
        // every mirror would issue an identical
        // save_watched_satellites call. Per CR
        // round 3 on PR #568.
        let changed = if active {
            set.insert(norad_id)
        } else {
            set.remove(&norad_id)
        };
        if changed {
            save_watched_satellites(config, &set);
        }
    }
    // Mirror across sibling rows. `set_active`
    // is a no-op when the state already matches,
    // so the recursion terminates after one
    // round-trip per sibling. The pointer
    // compare keeps us from re-entering THIS
    // button's own handler. If the displayed
    // Vec has already been dropped (window
    // teardown), the upgrade fails and we
    // simply skip mirroring — the watched-set
    // write above is the only persistent
    // effect that matters at that point.
    //
    // Match siblings by NORAD id, not display
    // name: the watched set is keyed by id, and
    // any future catalog drift where two entries
    // share a label (alternate names, alias
    // entries) would otherwise toggle the wrong
    // satellite's bells. Per CR round 1 on PR
    // #568.
    let Some(displayed) = displayed.upgrade() else {
        return;
    };
    for entry in displayed.borrow().iter() {
        if norad_id_for_pass(&entry.pass) == Some(norad_id)
            && let Some(other) = &entry.bell_btn
            && other.as_ptr() != b.as_ptr()
            && other.is_active() != active
        {
            other.set_active(active);
        }
    }
}

/// Per-row play button — one-click tune to the satellite's downlink
/// with the right demod / BW. Skipped when the satellite isn't in the
/// catalog (impossible in practice but the lookup type is `Option`,
/// so we fail closed — no button rather than a button that does
/// nothing). The 4th tuple element (`Option<ImagingProtocol>`) is
/// ignored — manual tune is user-initiated and works on any catalog
/// entry; only the auto-record path filters on `Some(protocol)`.
fn attach_pass_play_button(row: &adw::ActionRow, pass: &sdr_sat::Pass, tune: &Rc<TuneFn>) {
    use sidebar::satellites_panel::{format_downlink_mhz, tune_target_for_pass};

    let Some((freq_hz, mode, bw_hz, _protocol, _norad_id)) = tune_target_for_pass(pass) else {
        return;
    };
    let play_btn = gtk4::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text(format!(
            "Tune to {} ({})",
            pass.satellite,
            format_downlink_mhz(freq_hz),
        ))
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    // Tooltips aren't read by screen readers — set the accessible
    // label too, matching the project rule for icon-only buttons.
    let a11y_label = format!("Tune to {} downlink", pass.satellite);
    play_btn.update_property(&[gtk4::accessible::Property::Label(a11y_label.as_str())]);
    let tune_for_click = Rc::clone(tune);
    play_btn.connect_clicked(move |_| {
        tune_for_click(freq_hz, mode, bw_hz);
    });
    row.add_suffix(&play_btn);
}
