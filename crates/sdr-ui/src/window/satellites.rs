//! Satellites activity wiring: pass scheduling, auto-record, viewer
//! plumbing, SSTV batch export, and the Doppler tracker.

use gtk4::prelude::*;
use libadwaita::prelude::*;

mod doppler;
mod recorder;
mod tick;
use doppler::wire_doppler;
use recorder::RecorderDeps;
use tick::{SatWiring, wire_recorder};

use super::{
    AppState, Duration, PendingSstvExport, Rc, RefCell, SidebarPanels, TuneCtx, TuneFn, adw, gio,
    glib, plain_toast, sidebar,
};

/// Cadence of the Satellites panel's countdown ticker — 1 line/sec
/// is the smallest interval that produces a visible change in the
/// pass-row title (which renders to 1-minute granularity for far
/// passes and to seconds only inside the "starting now" window).
/// Smaller would burn cycles for no visible benefit.
pub(super) const SATELLITES_COUNTDOWN_TICK: Duration = Duration::from_secs(1);

/// Outcome of `save_sstv_batches` reported back to the GTK main
/// thread. Used by the `RecorderAction::SaveSstvPass` arm. Per CR
/// round 6 #21 on PR #599.
pub(super) struct SstvSaveOutcome {
    /// User-facing toast text summarising the per-batch save
    /// results.
    message: String,
    /// `true` iff every image in the *current* pass batch saved
    /// cleanly. Drives the compare-and-clear of
    /// `state.sstv_completed_images` and viewer auto-close.
    current_ok: bool,
    /// Batches that still need to be saved on a future attempt:
    /// any prior pending batch where at least one image failed,
    /// plus the current batch if it had any failures (re-keyed
    /// to the *current* `dir`). On the next `SaveSstvPass` each
    /// retained batch is retried against its own preserved `dir`
    /// — never the new pass's directory.
    retained: Vec<PendingSstvExport>,
}

/// Worker-thread save routine: iterate prior failed batches first
/// (each into its own original `dir`), then save the current
/// pass's images into `current_dir`. Retain any batch that had
/// any per-image failures so the next `SaveSstvPass` can retry it
/// in its own folder. Per CR round 6 #21 on PR #599.
pub(super) fn save_sstv_batches(
    pending_batches: Vec<PendingSstvExport>,
    current_images: Vec<sdr_radio::sstv_image::CompletedSstvImage>,
    current_dir: std::path::PathBuf,
) -> SstvSaveOutcome {
    let mut retained: Vec<PendingSstvExport> = Vec::new();
    let mut total_saved = 0_usize;
    let mut total_failed = 0_usize;
    let mut error_summary: Vec<String> = Vec::new();

    // Save each previously-retained batch to its own directory,
    // honouring its `start_index` so a late-tail retry doesn't
    // overwrite the prefix that already saved successfully on the
    // first attempt. Per CR round 8 #27 on PR #599.
    for batch in pending_batches {
        let (saved, errs) = save_sstv_batch(&batch.dir, &batch.images, batch.start_index);
        total_saved += saved;
        let failed = errs.len();
        total_failed += failed;
        if failed > 0 {
            error_summary.extend(errs.iter().map(|e| format!("{}: {e}", batch.dir.display())));
            retained.push(batch);
        }
    }

    // Save the current pass.
    let current_dir_display = current_dir.display().to_string();
    let current_image_count = current_images.len();
    let (cur_saved, cur_errs) = save_sstv_batch(&current_dir, &current_images, 0);
    total_saved += cur_saved;
    total_failed += cur_errs.len();
    let current_ok = cur_errs.is_empty() && (cur_saved > 0 || current_image_count == 0);
    if !cur_errs.is_empty() {
        error_summary.extend(
            cur_errs
                .iter()
                .map(|e| format!("{current_dir_display}: {e}")),
        );
        retained.push(PendingSstvExport {
            dir: current_dir,
            // Original attempt started at index 0; on retry we
            // re-attempt the entire batch from the same start to
            // keep filenames stable. Per CR round 8 #27 on PR #599.
            start_index: 0,
            images: current_images,
        });
    }

    let message = sstv_save_summary(
        total_saved,
        total_failed,
        &error_summary,
        &current_dir_display,
    );

    SstvSaveOutcome {
        message,
        current_ok,
        retained,
    }
}

/// Toast text for a completed SSTV save sweep. The zero/zero case is
/// a pass that produced no imagery — same warn-and-skip semantics the
/// inline version had.
fn sstv_save_summary(
    total_saved: usize,
    total_failed: usize,
    error_summary: &[String],
    current_dir_display: &str,
) -> String {
    if total_saved == 0 && total_failed == 0 {
        tracing::warn!(
            "auto-record SaveSstvPass but no SSTV images were decoded — pass produced no imagery",
        );
        format!(
            "Pass complete, but no SSTV images decoded — nothing saved to {current_dir_display}"
        )
    } else if total_failed == 0 {
        format!("Pass complete — {total_saved} SSTV image(s) saved")
    } else {
        format!(
            "Pass complete — {total_saved} image(s) saved, {total_failed} failed: {}",
            error_summary.join("; ")
        )
    }
}

/// Save a single batch of SSTV images into `dir`, naming files
/// `img{start_index}.png`, `img{start_index+1}.png`, … . Returns
/// `(saved_count, per_image_error_messages)`. A directory-creation
/// failure surfaces as one error covering the whole batch; image
/// write failures surface per image.
///
/// `start_index` lets a late-tail retry append after the prefix
/// that already saved on the first attempt (round-7 #26 left late
/// frames in `sstv_completed_images`; we now move them into a
/// retry batch keyed to the same dir, with `start_index =
/// exported_image_count` so the retry's `img12.png` doesn't
/// clobber a successfully-saved `img0.png`). Per CR round 8 #27
/// on PR #599.
pub(super) fn save_sstv_batch(
    dir: &std::path::Path,
    images: &[sdr_radio::sstv_image::CompletedSstvImage],
    start_index: usize,
) -> (usize, Vec<String>) {
    if images.is_empty() {
        return (0, Vec::new());
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("auto-record SaveSstvPass: failed to create directory {dir:?}: {e}",);
        return (0, vec![format!("create_dir_all failed: {e}")]);
    }
    let mut saved = 0_usize;
    let mut errors: Vec<String> = Vec::new();
    for (offset, img) in images.iter().enumerate() {
        let idx = start_index + offset;
        let path = dir.join(format!("img{idx}.png"));
        match crate::sstv_viewer::write_sstv_rgb_png(&path, &img.pixels, img.width, img.height) {
            Ok(()) => {
                tracing::info!(
                    ?path,
                    width = img.width,
                    height = img.height,
                    "auto-record SSTV image saved",
                );
                saved += 1;
            }
            Err(e) => {
                tracing::warn!("auto-record SSTV export img{idx} to {path:?} failed: {e}",);
                errors.push(format!("img{idx}: {e}"));
            }
        }
    }
    (saved, errors)
}

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

/// LOS save for an APT pass: rotate per orbit leg, encode the PNG on
/// a blocking thread, and toast the outcome.
/// Split out of [`interpret_recorder_action`] per the 50-NLOC gate
/// (#817).
#[allow(clippy::too_many_arguments)]
fn on_save_apt_png(deps: &RecorderDeps, path: std::path::PathBuf) {
    // Snapshot the recording-pass tuple FIRST so every
    // pass-derived value (rotation flag, slot-clear
    // check) reads from this stable view. If a new AOS
    // overwrites the slot between this dispatch and the
    // export — a back-to-back-pass race — we must use
    // the snapshot, not the live slot, otherwise the
    // older pass's image gets exported with the newer
    // pass's orientation. Per CR round 6 on PR #571.
    //
    // The same snapshot also drives the "only clear if
    // still-equal" guard on `apt_recording_pass` in both
    // the early-return path below and the async-callback
    // completion. Per CR rounds 4 and 5 on PR #571.
    let exported_pass = *deps.state.apt_recording_pass.borrow();
    // Compute the rotate-180 flag for ascending passes
    // (B2 of the noaa-apt parity work) FROM THE SNAPSHOT,
    // not from the live `deps.state.apt_recording_pass`. The
    // helper resolves the satellite's TLE from the cache
    // and calls `sdr_sat::is_ascending` at the snapshotted
    // AOS sample point. Defaults to `false` (no rotation)
    // if any step fails — descending-pass orientation is
    // the safer default since it preserves north-at-top.
    let rotate_180 = exported_pass.is_some_and(|(norad_id, aos)| {
        compute_apt_rotate_180_for_pass(deps.cache.as_ref(), norad_id, aos)
    });
    let mode = sdr_radio::apt_image::BrightnessMode::default();
    // Async export: snapshot the AptImage on the main
    // thread NOW, hand the snapshot to a worker via
    // `gio::spawn_blocking`. The encode for a 1500-line
    // pass is multi-hundred-ms — synchronously running
    // it here would freeze GTK during LOS, exactly when
    // the user wants to see the toast and have the
    // window auto-close cleanly. Per CR round 1 on PR
    // #571.
    let view_opt = deps.state.apt_viewer.borrow().as_ref().cloned();
    let Some(view) = view_opt else {
        tracing::warn!("auto-record SavePng but no APT viewer is open (user closed mid-pass)",);
        post_toast(
            &deps.toast_overlay,
            "Pass complete, but the APT viewer was closed — no image saved",
        );
        // Same overlap-guard as the async-callback path:
        // only clear the slot if it still holds the pass
        // we entered this branch with. If a new AOS
        // wrote a fresh tuple in the meantime, leave it
        // alone.
        {
            let mut slot = deps.state.apt_recording_pass.borrow_mut();
            if *slot == exported_pass {
                *slot = None;
            }
        }
        return;
    };
    // Capture state needed by the async on_complete
    // callback (the rest can be moved into the closure).
    let path_for_msg = path.clone();
    let path_for_export = path;
    let toast_overlay_for_complete = deps.toast_overlay.clone();
    let state_for_complete = Rc::clone(&deps.state);
    // Snapshot the *current* viewer-window WeakRef BEFORE
    // spawning the worker. If the user closes the viewer
    // mid-export and reopens it, `state.apt_viewer_window`
    // will point at the new window by the time the
    // callback fires; reading from there could close the
    // wrong window. Cloning the WeakRef pins the
    // identity of the window we'll attempt to close, while
    // staying weak so a closed/dropped window upgrades to
    // None and we no-op. Per CR round 3 on PR #571.
    let done = AptExportDone {
        path: path_for_msg,
        rotate_180,
        mode,
        window_weak: deps.state.apt_viewer_window.borrow().as_ref().cloned(),
        pass: exported_pass,
    };
    view.export_png_full_async(path_for_export, mode, rotate_180, move |result| {
        on_apt_export_complete(
            result,
            &done,
            &toast_overlay_for_complete,
            &state_for_complete,
        );
    });
}

/// Snapshot taken at APT-export start, consumed by the async
/// completion callback.
struct AptExportDone {
    /// Destination path, kept for the toast / log messages.
    path: std::path::PathBuf,
    rotate_180: bool,
    mode: sdr_radio::apt_image::BrightnessMode,
    /// Viewer window snapshotted at export START — the user closing
    /// and reopening the viewer mid-export must not close the wrong
    /// window; cloning the WeakRef pins the identity while staying
    /// weak so a dropped window upgrades to None and we no-op. Per
    /// CR round 3 on PR #571.
    window_weak: Option<glib::WeakRef<adw::Window>>,
    /// Recording-pass tuple for the compare-and-clear on
    /// completion. Per CR round 4 on PR #571.
    pass: Option<(u32, chrono::DateTime<chrono::Utc>)>,
}

/// Completion side of the async APT PNG export: toast the outcome,
/// close the viewer window we snapshotted at export start on success
/// (a reopen mid-export must not close the wrong window — CR round 3
/// on PR #571), and clear the recording-pass slot only when it still
/// holds the exported pass (a new AOS may own it — CR round 4 on
/// PR #571).
fn on_apt_export_complete(
    result: Result<(), crate::viewer::ViewerError>,
    done: &AptExportDone,
    toast_overlay: &glib::WeakRef<adw::ToastOverlay>,
    state: &Rc<AppState>,
) {
    let AptExportDone {
        path: path_for_msg,
        rotate_180,
        mode,
        window_weak,
        pass: exported_pass,
    } = done;
    let exported_pass = *exported_pass;
    let exported_window_weak = window_weak.as_ref();
    let (export_ok, msg) = match result {
        Ok(()) => {
            tracing::info!(
                rotate_180,
                ?mode,
                "auto-record PNG saved to {}",
                path_for_msg.display()
            );
            (
                true,
                format!("Pass complete — image saved to {}", path_for_msg.display()),
            )
        }
        Err(e) => {
            tracing::warn!(
                "auto-record PNG export to {} failed: {e}",
                path_for_msg.display()
            );
            (false, format!("Pass complete but PNG save failed: {e}"))
        }
    };
    post_toast(toast_overlay, &msg);
    // Close the APT viewer window now that the PNG
    // is on disk — resets the viewer for the next
    // pass instead of carrying stale lines forward.
    // Per a user request during PR #554 live
    // testing.
    //
    // Only close on a successful export — if the
    // save failed (Cairo error, disk full, etc.)
    // the user probably wants to inspect the
    // in-memory image and manually retry the
    // export. Per CR round 9 on PR #554.
    if export_ok {
        // Use the WeakRef we snapshotted at export
        // start (not the current `state.apt_viewer_window`)
        // so a viewer reopen during the async save
        // can't trick us into closing the wrong
        // window. Upgrade-or-skip — if the user
        // already closed it, the upgrade returns None
        // and we simply do nothing. Per CR round 3 on
        // PR #571.
        if let Some(window) = exported_window_weak.and_then(glib::WeakRef::upgrade) {
            tracing::info!("auto-record LOS: closing APT viewer window after PNG save",);
            window.close();
        }
    }
    // Clear the recording-pass info now that the
    // export is done — but ONLY if the slot still
    // holds the same pass we just saved. If a new
    // AOS overwrote it while we were encoding, that
    // new pass owns the slot now and clearing it
    // would silently break the next LOS-side
    // rotate-180 lookup. Per CR round 4 on PR #571.
    {
        let mut slot = state.apt_recording_pass.borrow_mut();
        if *slot == exported_pass {
            *slot = None;
        }
    }
}

/// Log a warning when a Meteor-M pass delivered fewer AVHRR APIDs than
/// the satellite's expected set — the Roscosmos transmission schedule
/// occasionally changes which channels are on (see #645).
fn warn_missing_lrpt_apids(
    snapshots: &[(u16, sdr_lrpt::image::ChannelBuffer)],
    exported_lrpt_pass: Option<(u32, chrono::DateTime<chrono::Utc>)>,
) {
    // Diagnostic: warn if the satellite delivered some
    // APIDs but not the full per-satellite expected set.
    // Catches schedule changes (e.g. Roscosmos flipping
    // M2-3 between summer-mode c1/c2/c3 and standard
    // c1/c2/c4) as a single log line instead of the user
    // wondering why some composite recipes silently
    // produced nothing. Silent passes are skipped — they're
    // a different failure mode handled by
    // `pass_decoded_nothing` above. Per #645.
    if let Some((norad_id, _aos)) = exported_lrpt_pass
        && let Some(sat) = sdr_sat::KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == norad_id)
    {
        let received_apids: Vec<u16> = snapshots.iter().map(|(apid, _)| *apid).collect();
        let missing = sat.missing_lrpt_apids(&received_apids);
        if !missing.is_empty() {
            tracing::warn!(
                "auto-record LOS: {} delivered APIDs {:?} but expected {:?}; \
             missing {:?} — Roscosmos schedule may have changed (see #645)",
                sat.name,
                received_apids,
                sat.expected_lrpt_apids.unwrap_or(&[]),
                missing,
            );
        }
    }
}

/// LOS save for an LRPT pass: one PNG per APID (plus the optional
/// RGB composite) into the pass directory, off the main loop.
/// Split out of [`interpret_recorder_action`] per the 50-NLOC gate
/// (#817).
fn on_save_lrpt_pass(deps: &RecorderDeps, dir: std::path::PathBuf) {
    // Walk every APID present in the SHARED `LrptImage`
    // (the DSP-side decoder's destination — the source
    // of truth) and write one PNG per channel into the
    // per-pass directory (creating it lazily). Decoupled
    // from the live viewer in `CodeRabbit` round 7 on
    // PR #543: the previous implementation went through
    // `state.lrpt_viewer` and produced "no image saved"
    // toasts whenever the user dismissed the live
    // window mid-pass — even though the DSP had been
    // happily decoding into the shared image the
    // whole time. Reading directly from
    // `state.lrpt_image` makes the LOS save robust
    // against viewer close: the decoder runs as long
    // as the demod mode is `Lrpt`, and the captured
    // imagery survives any number of viewer cycles.
    // Snapshot every non-empty APID's pixel buffer
    // on the main thread (cheap — `snapshot_channel`
    // clones the per-channel `Vec<u8>` under a brief
    // mutex hold), then move the encoding + file
    // I/O off to a worker via `gio::spawn_blocking`.
    // PNG encoding for a full multi-channel pass is
    // multiple MB per APID and can take seconds; doing
    // it inline on the 1 Hz countdown tick would
    // freeze the UI right when the auto-record toast
    // and tune-restore should be landing. Per
    // CodeRabbit round 8 on PR #543. Established
    // pattern in this file (TLE refresh @ 8678,
    // bookmark import @ 8805).
    let snapshots = snapshot_lrpt_channels(deps);
    let composite_snapshots = snapshot_lrpt_composites(deps);
    let toast_overlay_weak_for_save = deps.toast_overlay.clone();
    // Clone state for the post-save viewer-close
    // — we need to read `state.lrpt_recording_pass`
    // after the spawn_blocking completes, which
    // requires capturing state into the future.
    let state_lrpt_close = Rc::clone(&deps.state);
    // Snapshot the *current* viewer-window WeakRef BEFORE
    // spawning the worker, mirroring the APT path's
    // pattern from PR #571 round 3. If the user closes
    // the LRPT viewer mid-export and reopens it,
    // `state.lrpt_viewer_window` will point at the new
    // window by the time the callback fires; reading
    // from there could close the wrong window. Cloning
    // the WeakRef pins the identity of the window we'll
    // attempt to close, while staying weak so a
    // closed/dropped window upgrades to None and we
    // no-op. Per CR round 2 on PR #575.
    let exported_lrpt_window_weak = deps.state.lrpt_viewer_window.borrow().as_ref().cloned();
    // Snapshot the recording-pass tuple FIRST so the
    // post-save clear is gated on "this is still the
    // pass we entered with". An overlapping pass-N+1
    // AOS that starts while pass-N is still encoding
    // would otherwise have its slot clobbered when
    // pass-N's completion callback fires `*slot =
    // None`. Same shape as the APT compare-and-clear
    // at `RecorderAction::SavePng`. Per CR round 2 on
    // PR #575.
    let exported_lrpt_pass = *deps.state.lrpt_recording_pass.borrow();
    // Capture "no APIDs decoded" up front. This case has
    // no in-memory imagery to retry — the viewer is empty
    // — so the LOS close gate should fire even though
    // `save_ok` will be false. Without this, the viewer
    // would sit open with a blank canvas across silent
    // Meteor passes (Russian sats are intermittent;
    // many passes produce no LRPT). Per silent-pass
    // diagnosis 2026-05-08.
    let pass_decoded_nothing = snapshots.is_empty();
    if !pass_decoded_nothing {
        warn_missing_lrpt_apids(&snapshots, exported_lrpt_pass);
    }
    glib::spawn_future_local(async move {
        let dir_for_msg = dir.clone();
        // Tuple return: (toast message, saved-at-least-one).
        // The flag gates the post-save viewer close —
        // we keep the viewer open ONLY on real save
        // failures (disk full, dir create errored,
        // worker panicked) where in-memory imagery
        // exists and a manual retry is possible. The
        // "no APIDs decoded" branch is closed via
        // `pass_decoded_nothing` below, since there's
        // nothing to retry. Per CR round 9 on PR #554
        // + silent-pass cleanup 2026-05-08.
        let (result_msg, save_ok) =
            gio::spawn_blocking(move || save_lrpt_files(&dir, snapshots, composite_snapshots))
                .await
                .unwrap_or_else(|e| {
                    // `gio::spawn_blocking`'s join error is a
                    // panic payload (`Box<dyn Any + Send>`),
                    // which doesn't implement `Display`.
                    // Format via `Debug` on the worker side
                    // and just report a generic message to
                    // the user — a panicking PNG encoder is
                    // a logic bug, not something the user
                    // can act on.
                    tracing::warn!("auto-record SaveLrptPass: worker thread panicked: {e:?}",);
                    (
                        format!(
                            "Pass complete but PNG worker panicked (target was {})",
                            dir_for_msg.display()
                        ),
                        false,
                    )
                });
        post_toast(&toast_overlay_weak_for_save, &result_msg);
        // Mark the LRPT pass as no-longer-recording for
        // the close-to-tray Quit-confirmation predicate.
        // We clear regardless of save_ok — the pass itself
        // is over (LOS already happened); save_ok only
        // controls whether to close the viewer. Per #512.
        //
        // **Compare-and-clear:** only clear the slot if
        // it still holds the same pass we entered this
        // branch with. If a new AOS overwrote it
        // mid-export (overlapping passes can happen now
        // that composites widen the LOS window), that
        // new pass owns the slot — wiping it would lie
        // to the close-to-tray predicate about the
        // in-flight pass. Mirrors the APT
        // `apt_recording_pass` compare-and-clear from
        // PR #571 round 4. Per CR round 2 on PR #575.
        {
            let mut slot = state_lrpt_close.lrpt_recording_pass.borrow_mut();
            if *slot == exported_lrpt_pass {
                *slot = None;
            }
        }
        // Close the LRPT viewer window now that the
        // PNGs are on disk — resets the viewer for
        // the next pass instead of carrying stale
        // APIDs forward. Per a user request during
        // PR #554 live testing.
        //
        // Only close when at least one channel
        // actually saved (`save_ok`) — on total-
        // failure outcomes (no APIDs decoded, dir
        // create failed, every channel errored, or
        // worker panicked) keep the viewer open so
        // the user can inspect the in-memory image
        // and manually retry the export. Per CR
        // round 9 on PR #554.
        //
        // Runs on the GLib main loop (we re-entered
        // it via `spawn_future_local`), so the weak
        // upgrade + `.close()` is main-thread-safe.
        // Weak-ref upgrade fails closed: if the user
        // already dismissed the window, there's
        // nothing to close. The close-request
        // handler in `open_lrpt_viewer_if_needed`
        // clears the AppState slots so the next AOS
        // opens a fresh viewer.
        //
        // Use the WeakRef we snapshotted at export
        // start (not the current
        // `state.lrpt_viewer_window`) so a viewer
        // reopen during the async save can't trick us
        // into closing the wrong window — same shape
        // as the APT path's snapshot pattern. Per CR
        // round 2 on PR #575.
        // Close-gate logic:
        //
        //   save_ok               → close (PNGs are on disk;
        //                            nothing to keep viewer
        //                            open for)
        //   pass_decoded_nothing  → close (no imagery to
        //                            retry; viewer canvas
        //                            is empty — common on
        //                            silent Russian Meteor
        //                            passes)
        //   !save_ok && !pass_decoded_nothing
        //                         → keep open (real save
        //                            failure with in-memory
        //                            imagery — user can
        //                            inspect + retry export)
        //
        // Both close branches log the reason so the
        // overnight pass log answers "did the viewer
        // reset properly between passes?" with a single
        // grep.
        let should_close = save_ok || pass_decoded_nothing;
        if should_close
            && let Some(window) = exported_lrpt_window_weak
                .as_ref()
                .and_then(glib::WeakRef::upgrade)
        {
            let reason = if save_ok {
                "PNGs saved"
            } else {
                "no APIDs decoded — nothing to retry"
            };
            tracing::info!("auto-record LOS: closing LRPT viewer window ({reason})");
            window.close();
        }
    });
}

/// Snapshot the per-APID channel buffers out of the shared LRPT
/// assembler (cheap memcpys under the lock; encode happens later on
/// a worker).
fn snapshot_lrpt_channels(deps: &RecorderDeps) -> Vec<(u16, sdr_lrpt::image::ChannelBuffer)> {
    let mut sorted = deps.state.lrpt_image.channel_apids();
    sorted.sort_unstable();
    sorted
        .into_iter()
        .filter_map(|apid| {
            deps.state
                .lrpt_image
                .snapshot_channel(apid)
                .filter(|s| s.lines > 0)
                .map(|s| (apid, s))
        })
        .collect()
}

/// Snapshot the enabled composite recipes' channel triples — empty
/// when the composites switch is off.
#[allow(clippy::type_complexity)]
fn snapshot_lrpt_composites(
    deps: &RecorderDeps,
) -> Vec<(
    crate::lrpt_viewer::CompositeRecipe,
    sdr_lrpt::image::CompositeSnapshot,
)> {
    if deps.auto_record_composites_switch.is_active() {
        deps.state.lrpt_image.with_assembler(|a| {
            crate::lrpt_viewer::COMPOSITE_CATALOG
                .iter()
                .filter_map(|recipe| {
                    a.clone_channels_for_composite(recipe.r_apid, recipe.g_apid, recipe.b_apid)
                        .map(|snap| (*recipe, snap))
                })
                .collect()
        })
    } else {
        Vec::new()
    }
}

/// Save one LRPT pass to disk: per-pass directory, one greyscale PNG
/// per decoded APID, and the enabled RGB composites alongside.
/// Returns the toast message plus a "close-worthy" success flag —
/// at least one file saved counts (partial-success outcomes still
/// produced disk artifacts the user can inspect). Runs inside
/// `gio::spawn_blocking`: the ~30 ms per-recipe RGB interleave and
/// all PNG encoding stay off the GTK main thread (CR round 1 on
/// PR #575).
fn save_lrpt_files(
    dir: &std::path::Path,
    snapshots: Vec<(u16, sdr_lrpt::image::ChannelBuffer)>,
    composite_snapshots: Vec<(
        crate::lrpt_viewer::CompositeRecipe,
        sdr_lrpt::image::CompositeSnapshot,
    )>,
) -> (String, bool) {
    if snapshots.is_empty() {
        tracing::warn!(
            "auto-record SaveLrptPass but no APIDs were decoded — pass produced no imagery",
        );
        return (
            format!(
                "Pass complete, but no LRPT channels decoded — nothing saved to {}",
                dir.display()
            ),
            false,
        );
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        // Per-pass directory created up
        // front so a disk-full / permissions
        // failure surfaces as a single
        // observable error rather than `N`
        // per-channel warnings. Per
        // CodeRabbit round 1 on PR #543.
        tracing::warn!("auto-record SaveLrptPass: failed to create directory {dir:?}: {e}",);
        return (
            format!("Pass complete but couldn't create {}: {e}", dir.display()),
            false,
        );
    }
    let mut saved = 0_usize;
    let mut errors: Vec<String> = Vec::new();
    save_lrpt_channels(dir, snapshots, &mut saved, &mut errors);
    save_lrpt_composites(dir, composite_snapshots, &mut saved, &mut errors);
    let msg = if errors.is_empty() {
        format!(
            "Pass complete — {saved} LRPT file(s) saved to {}",
            dir.display()
        )
    } else {
        format!(
            "Pass complete — {saved} file(s) saved, {} failed: {}",
            errors.len(),
            errors.join("; ")
        )
    };
    // Treat "at least one channel saved" as
    // success for close-purposes — partial-
    // success outcomes still produced disk
    // artifacts the user can inspect.
    (msg, saved > 0)
}

/// Per-APID greyscale saves for an LRPT pass.
fn save_lrpt_channels(
    dir: &std::path::Path,
    snapshots: Vec<(u16, sdr_lrpt::image::ChannelBuffer)>,
    saved: &mut usize,
    errors: &mut Vec<String>,
) {
    for (apid, snap) in snapshots {
        let path = dir.join(format!("apid{apid}.png"));
        match crate::lrpt_viewer::write_greyscale_png(
            &path,
            &snap.pixels,
            sdr_lrpt::image::IMAGE_WIDTH,
            snap.lines,
        ) {
            Ok(()) => {
                tracing::info!(
                    ?path,
                    apid,
                    lines = snap.lines,
                    "auto-record LRPT channel saved",
                );
                *saved += 1;
            }
            Err(e) => {
                tracing::warn!("auto-record LRPT export for APID {apid} to {path:?} failed: {e}",);
                errors.push(format!("APID {apid}: {e}"));
            }
        }
    }
}

/// Composite RGB saves for an LRPT pass. Filename is
/// `composite-{slug}.png` where `slug` is the recipe name with spaces
/// replaced by `-` and path separators by `_` so the disk layout is
/// portable across filesystems. The RGB interleave runs here — inside
/// the `gio::spawn_blocking` worker — so the ~30 ms per-recipe
/// per-pixel walk doesn't block the GTK main thread; the assembler
/// lock was already released after the cheap channel memcpy in the
/// snapshot phase (CR round 1 on PR #575).
fn save_lrpt_composites(
    dir: &std::path::Path,
    composite_snapshots: Vec<(
        crate::lrpt_viewer::CompositeRecipe,
        sdr_lrpt::image::CompositeSnapshot,
    )>,
    saved: &mut usize,
    errors: &mut Vec<String>,
) {
    // Composite PNGs alongside the per-APID
    // where `slug` is the recipe name with
    // spaces replaced by `-` and path
    // separators replaced by `_` so the disk
    // layout is portable across filesystems.
    //
    // The RGB interleave runs HERE — inside the
    // `gio::spawn_blocking` worker — so the
    // ~30 ms per-recipe per-pixel walk doesn't
    // block the GTK main thread. The assembler
    // lock was released after the cheap channel
    // memcpy in the snapshot phase above. Per
    // CR round 1 on PR #575.
    for (recipe, snap) in composite_snapshots {
        let rgb = sdr_lrpt::image::assemble_rgb_composite(
            &snap.r_pixels,
            &snap.g_pixels,
            &snap.b_pixels,
            snap.height,
        );
        let width = sdr_lrpt::image::IMAGE_WIDTH;
        let height = snap.height;
        let slug = recipe.name.replace(' ', "-").replace(['/', '\\'], "_");
        let path = dir.join(format!("composite-{slug}.png"));
        match crate::lrpt_viewer::write_rgb_png(&path, &rgb, width, height) {
            Ok(()) => {
                tracing::info!(
                    ?path,
                    recipe = recipe.name,
                    width,
                    height,
                    "auto-record LRPT composite saved",
                );
                *saved += 1;
            }
            Err(e) => {
                tracing::warn!(
                    "auto-record LRPT composite {} to {path:?} failed: {e}",
                    recipe.name,
                );
                errors.push(format!("Composite {}: {e}", recipe.name));
            }
        }
    }
}

/// Fallback [`SstvSaveOutcome`] when the `spawn_blocking` PNG worker
/// panics. Re-constructs the full retain list from the backups: prior
/// pending batches (preserved as-is) plus the current pass re-keyed to
/// its dir, so neither is silently dropped by the failure-path drain.
/// Per CR round 7 #25 on PR #599.
fn sstv_panic_outcome(
    e: &(dyn std::any::Any + Send),
    pending_batches_backup: Vec<PendingSstvExport>,
    current_images_backup: Vec<sdr_radio::sstv_image::CompletedSstvImage>,
    dir: &std::path::Path,
) -> SstvSaveOutcome {
    tracing::warn!("auto-record SaveSstvPass: worker thread panicked: {e:?}",);
    let mut retained = pending_batches_backup;
    if !current_images_backup.is_empty() {
        retained.push(PendingSstvExport {
            dir: dir.to_path_buf(),
            // Original attempt would have started at index 0; on panic
            // retry we reuse that start so filenames remain stable. Per
            // CR round 8 #27 on PR #599.
            start_index: 0,
            images: current_images_backup,
        });
    }
    SstvSaveOutcome {
        message: format!(
            "Pass complete but PNG worker panicked (target was {})",
            dir.display()
        ),
        current_ok: false,
        retained,
    }
}

/// Success-path drain after an SSTV pass save: remove the exported
/// prefix from the current-pass buffer and queue any late-arrived tail
/// (frames pushed by `DspToUi::SstvImageComplete` while the worker
/// ran) into `sstv_pending_export` keyed to this pass's `dir`, so the
/// next `SaveSstvPass` retries them into the correct folder. Per CR
/// round 7 #26 on PR #599.
fn drain_saved_sstv_images(
    state: &Rc<AppState>,
    exported_image_count: usize,
    dir: &std::path::Path,
) {
    let mut completed = state.sstv_completed_images.borrow_mut();
    let to_drain = exported_image_count.min(completed.len());
    completed.drain(..to_drain);
    // Late frames pushed by
    // `DspToUi::SstvImageComplete` while
    // the worker was running stay in
    // `completed`. Without further action
    // they'd survive the export, then get
    // wiped by the next AOS — breaking the
    // per-pass auto-save contract. Move
    // them into `sstv_pending_export`
    // keyed to *this* pass's `dir` so the
    // next `SaveSstvPass` retries them
    // into the correct folder. Per CR
    // round 7 #26 on PR #599.
    if !completed.is_empty() {
        let late_tail: Vec<_> = completed.drain(..).collect();
        tracing::info!(
            "auto-record SaveSstvPass: queueing {} late SSTV frame(s) for retry into {}",
            late_tail.len(),
            dir.display()
        );
        state
            .sstv_pending_export
            .borrow_mut()
            .push(PendingSstvExport {
                dir: dir.to_path_buf(),
                // Late frames belong AFTER
                // the prefix that already
                // saved successfully on this
                // pass — `exported_image_count`
                // images went out at indices
                // 0..exported_image_count, so
                // the retry starts at that
                // index. Per CR round 8 #27
                // on PR #599.
                start_index: exported_image_count,
                images: late_tail,
            });
    }
}

/// Completion side of the async SSTV pass save: toast the outcome,
/// restore retained batches for retry, drain the exported prefix of
/// the current-pass buffer (queueing any late-arrived tail), clear the
/// recording-pass slot compare-and-clear style, and close the viewer
/// window we snapshotted at export start when the save fully landed.
#[allow(clippy::too_many_arguments)]
fn on_sstv_save_complete(
    state: &Rc<AppState>,
    toast_overlay: &glib::WeakRef<adw::ToastOverlay>,
    outcome: SstvSaveOutcome,
    exported_image_count: usize,
    exported_sstv_pass: Option<(u32, chrono::DateTime<chrono::Utc>)>,
    exported_sstv_window_weak: Option<&glib::WeakRef<adw::Window>>,
    dir: &std::path::Path,
) {
    let SstvSaveOutcome {
        message,
        current_ok,
        retained,
    } = outcome;
    post_toast(toast_overlay, &message);
    // Restore retained batches (pending that still
    // failed + the current batch if it failed) into
    // `sstv_pending_export`. New pending items
    // queued by a parallel AOS slip in *after* the
    // retained set so retry order honours
    // chronological pass start.
    if !retained.is_empty() {
        let mut pending = state.sstv_pending_export.borrow_mut();
        let mut combined = retained;
        combined.append(&mut pending);
        *pending = combined;
    }
    // Drain only the current-pass images we
    // actually snapshotted. Late frames pushed
    // while the worker was running stay buffered
    // for the next save cycle. Compare-and-clear
    // by the recording-pass tuple so an
    // overlapping pass's buffer/slot isn't wiped
    // by a late completion callback. Per CR round
    // 4 on PR #599.
    let mut slot = state.sstv_recording_pass.borrow_mut();
    if *slot == exported_sstv_pass {
        if current_ok {
            drain_saved_sstv_images(state, exported_image_count, dir);
            *slot = None;
        } else {
            // Failure path: clear the slot so the
            // recorder isn't stuck in a permanent
            // "pass in flight" state. The current
            // images are already in `retained`
            // (queued for retry under their own
            // `dir`), so the buffer can be safely
            // drained too — keeping them would
            // duplicate-save on the next attempt.
            let mut completed = state.sstv_completed_images.borrow_mut();
            let to_drain = exported_image_count.min(completed.len());
            completed.drain(..to_drain);
            *slot = None;
        }
    }
    drop(slot);
    // Close the viewer on successful save AND only
    // when the buffer is empty — if late frames
    // arrived while saving, keep the viewer open
    // so the user can see them rather than burying
    // a tail. On failure: also keep open so the
    // user can inspect the in-memory image and
    // retry. Mirrors LRPT semantics from CR round
    // 9 on PR #554, refined per CR round 4 #18 on
    // PR #599.
    if current_ok
        && state.sstv_completed_images.borrow().is_empty()
        && let Some(window) = exported_sstv_window_weak.and_then(glib::WeakRef::upgrade)
    {
        tracing::info!("auto-record LOS: closing SSTV viewer window after PNG save");
        window.close();
    }
}

/// LOS save for an SSTV pass: drain completed frames (+ retained
/// prior batches) into per-pass directories.
/// Split out of [`interpret_recorder_action`] per the 50-NLOC gate
/// (#817).
fn on_save_sstv_pass(deps: &RecorderDeps, dir: std::path::PathBuf) {
    // Per-pass auto-record save. Each pass's images are
    // written into their own `sstv-iss-{ts}` directory.
    // Failed-pass batches are kept in
    // `sstv_pending_export` keyed by their *original*
    // `dir`, then retried separately against that dir
    // at the next LOS — they never bleed into the
    // current pass's directory. Per CR round 6 #21 on
    // PR #599.
    //
    // Reading from `state.sstv_completed_images`
    // (rather than the shared `SstvImage` handle)
    // mirrors the LRPT design from CodeRabbit round 7
    // on PR #543: the save path is decoupled from the
    // live viewer so closing the viewer window
    // mid-pass doesn't lose the imagery.
    //
    // Encoding + file I/O is offloaded to
    // `gio::spawn_blocking` so multi-image PNG encoding
    // doesn't freeze the UI right when the auto-record
    // toast is landing. Per CodeRabbit #9 on PR #599.
    let pending_batches: Vec<PendingSstvExport> =
        std::mem::take(&mut *deps.state.sstv_pending_export.borrow_mut());
    let current_images: Vec<sdr_radio::sstv_image::CompletedSstvImage> = deps
        .state
        .sstv_completed_images
        .borrow()
        .iter()
        .cloned()
        .collect();
    // Snapshot count of the *current* pass so the
    // success path can drain only those — late frames
    // pushed by `DspToUi::SstvImageComplete` while we
    // were awaiting the worker stay buffered for the
    // next save cycle. Per CR round 4 on PR #599.
    let exported_image_count = current_images.len();
    let toast_overlay_weak_for_save = deps.toast_overlay.clone();
    let state_sstv_close = Rc::clone(&deps.state);
    // Snapshot the WeakRef BEFORE spawning so a
    // viewer reopen during the async save can't
    // trick us into closing the wrong window.
    // Mirrors the LRPT pattern from CR round 2 on
    // PR #575.
    let exported_sstv_window_weak = deps.state.sstv_viewer_window.borrow().as_ref().cloned();
    // Snapshot the recording-pass tuple for
    // compare-and-clear on completion — mirrors the
    // LRPT and APT patterns from PR #571 / #575.
    let exported_sstv_pass = *deps.state.sstv_recording_pass.borrow();
    // Clone the worker inputs so a `spawn_blocking`
    // panic doesn't lose the imagery we already drained
    // from `sstv_pending_export`. The originals move
    // into the worker; the backups feed the panic
    // fallback's `retained` list. Per CR round 7 #25 on
    // PR #599.
    let pending_batches_backup = pending_batches.clone();
    let current_images_backup = current_images.clone();
    let dir_backup = dir.clone();
    glib::spawn_future_local(async move {
        let dir_for_msg = dir_backup.clone();
        let join =
            gio::spawn_blocking(move || save_sstv_batches(pending_batches, current_images, dir))
                .await;
        let SstvSaveOutcome {
            message,
            current_ok,
            retained,
        } = join.unwrap_or_else(|e| {
            sstv_panic_outcome(
                &*e,
                pending_batches_backup,
                current_images_backup,
                &dir_backup,
            )
        });
        on_sstv_save_complete(
            &state_sstv_close,
            &toast_overlay_weak_for_save,
            SstvSaveOutcome {
                message,
                current_ok,
                retained,
            },
            exported_image_count,
            exported_sstv_pass,
            exported_sstv_window_weak.as_ref(),
            &dir_for_msg,
        );
    });
}

/// Toast through a weak overlay handle — no-op when the window is
/// gone.
fn post_toast(overlay_weak: &glib::WeakRef<adw::ToastOverlay>, msg: &str) {
    if let Some(overlay) = overlay_weak.upgrade() {
        overlay.add_toast(plain_toast(msg));
    }
}

/// Compute the rotate-180 flag for the currently-recording APT pass:
/// `true` when the satellite is on the ascending leg of its orbit,
/// which means the assembled image is upside-down + mirrored
/// east/west (per `sdr_radio::apt_image::rotate_180_per_channel`).
/// Falls back to `false` (no rotation) on any failure — TLE cache
/// miss, parse failure, propagation error, or recording-pass info
/// missing. The default is safe: NOAA satellites are sun-synchronous,
/// so the descending pass is the typical case for daytime captures
/// and no-rotation preserves north-at-top. Takes the pass tuple
/// directly (`norad_id`, `aos`) so callers compute rotation against
/// an explicit snapshot — reading `AppState` here could race a
/// back-to-back AOS and export the older pass's image with the newer
/// pass's orientation (CR round 6 on PR #571).
fn compute_apt_rotate_180_for_pass(
    cache: Option<&std::sync::Arc<sdr_sat::TleCache>>,
    norad_id: u32,
    aos: chrono::DateTime<chrono::Utc>,
) -> bool {
    cache
        .and_then(|c| apt_pass_is_ascending(c, norad_id, aos))
        .unwrap_or(false)
}

/// Fallible core of [`compute_apt_rotate_180_for_pass`]. Looks the
/// satellite up by stable NORAD id (not display name) so a catalog
/// rename doesn't silently break this path (CR round 2 on PR #571);
/// each failure logs its own debug line before propagating `None`.
fn apt_pass_is_ascending(
    cache: &std::sync::Arc<sdr_sat::TleCache>,
    norad_id: u32,
    aos: chrono::DateTime<chrono::Utc>,
) -> Option<bool> {
    let known = sdr_sat::KNOWN_SATELLITES
        .iter()
        .find(|s| s.norad_id == norad_id)
        .or_else(|| {
            tracing::debug!(
                norad_id,
                "APT rotate-180: satellite not in catalog; defaulting to no rotation",
            );
            None
        })?;
    let (line1, line2) = cache
        .cached_tle_for(known.norad_id)
        .inspect_err(|e| {
            tracing::debug!(
                norad_id,
                error = %e,
                "APT rotate-180: TLE unavailable; defaulting to no rotation",
            );
        })
        .ok()?;
    let parsed = sdr_sat::Satellite::from_tle(known.name, &line1, &line2)
        .inspect_err(|e| {
            tracing::debug!(
                norad_id,
                error = %e,
                "APT rotate-180: TLE parse failed; defaulting to no rotation",
            );
        })
        .ok()?;
    sdr_sat::is_ascending(&parsed, aos)
        .inspect_err(|e| {
            tracing::debug!(
                norad_id,
                error = %e,
                "APT rotate-180: SGP4 propagate failed; defaulting to no rotation",
            );
        })
        .ok()
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
