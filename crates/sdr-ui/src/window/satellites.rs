//! Satellites activity wiring: pass scheduling, auto-record, viewer
//! plumbing, SSTV batch export, and the Doppler tracker.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::{
    AppState, CtcssMode, Duration, PendingSstvExport, Rc, RefCell, SidebarPanels, StatusBar,
    UiToDsp, adw, gio, glib, plain_toast, sidebar, spectrum,
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn connect_satellites_panel(
    panels: &SidebarPanels,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    state: &Rc<AppState>,
    toast_overlay: &adw::ToastOverlay,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    tune_to_satellite: &Rc<dyn Fn(u64, sdr_types::DemodMode, u32)>,
    set_playing: &Rc<dyn Fn(bool)>,
    status_bar: &Rc<StatusBar>,
) {
    use sdr_sat::{GroundStation, KNOWN_SATELLITES, Pass, TleCache};
    use sidebar::satellites_notify::{Action as NotifyAction, NotifyScheduler};
    use sidebar::satellites_panel::{
        AutoRecordQuality, KEY_STATION_ALT_M, KEY_STATION_LAT_DEG, KEY_STATION_LON_DEG,
        SatellitesPanelWeak, enumerate_upcoming_passes, format_downlink_mhz, format_last_refresh,
        format_pass_subtitle, format_pass_title, load_auto_record_apt, load_auto_record_audio,
        load_auto_record_composites, load_auto_record_quality, load_notify_lead_min,
        load_station_alt_m, load_station_lat_deg, load_station_lon_deg, load_watched_satellites,
        norad_id_for_pass, save_auto_record_apt, save_auto_record_audio,
        save_auto_record_composites, save_f64, save_tle_last_refresh, save_watched_satellites,
        tune_target_for_pass,
    };
    use sidebar::satellites_recorder::{
        Action as RecorderAction, AutoRecorder, SavedTune, ToastKind,
    };

    // One pass row + its source `Pass` so the 1 Hz ticker can
    // refresh the title without re-running pass enumeration. The
    // optional bell `ToggleButton` is held so the watch-toggle
    // handler can mirror the active state across every row whose
    // satellite matches — multiple NOAA 19 passes in the visible
    // list must all reflect the same subscription state. `None`
    // for off-catalog passes (no NORAD id → no notify).
    struct DisplayedPass {
        row: adw::ActionRow,
        pass: Pass,
        bell_btn: Option<gtk4::ToggleButton>,
    }

    // Borrow the panel for synchronous setup, then capture only
    // weak refs in long-lived closures. Cloning the strong panel
    // into a closure stored on its own widget creates a refcount
    // cycle (widget → handler → closure → cloned panel → widget)
    // that prevents teardown — see `SatellitesPanelWeak`'s doc for
    // the full chain.
    let panel = &panels.satellites;
    let panel_weak: SatellitesPanelWeak = panel.downgrade();

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

    {
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

    // `Option<Arc<TleCache>>`. `None` means the platform refused us
    // a cache directory (rare; sandboxed minimal environments).
    // Disable TLE-specific UI but keep ground-station persistence,
    // ZIP lookup, and the auto-record toggle wired — those don't
    // depend on TLEs and shouldn't go inert just because the cache
    // is gone.
    let cache: Option<std::sync::Arc<TleCache>> = match TleCache::new() {
        Ok(c) => Some(std::sync::Arc::new(c)),
        Err(e) => {
            tracing::warn!("Satellites panel: TLE cache unavailable — {e}");
            panel.refresh_button.set_sensitive(false);
            panel
                .last_refresh_row
                .set_subtitle("Cache directory unavailable");
            None
        }
    };

    let displayed: Rc<RefCell<Vec<DisplayedPass>>> = Rc::new(RefCell::new(Vec::new()));

    // #510 — per-satellite watched-set + notify scheduler. Loaded
    // from config so the user's selections survive restarts. The
    // set is mutated from two sites: (a) the bell toggle on each
    // pass row (write-through to config); (b) read-only by the
    // 1 Hz tick that drives the scheduler.
    let watched: Rc<RefCell<std::collections::HashSet<u32>>> =
        Rc::new(RefCell::new(load_watched_satellites(config)));
    let notify_scheduler: Rc<RefCell<NotifyScheduler>> =
        Rc::new(RefCell::new(NotifyScheduler::new()));

    // `recompute` is built unconditionally — when the cache is
    // unavailable it's a no-op so the lat/lon/alt notify handlers
    // can call it without branching. When the cache is available
    // it does the real pass-enumeration + row-rebuild work.
    let recompute: Rc<dyn Fn()> = if let Some(cache) = cache.as_ref() {
        let cache_recompute = std::sync::Arc::clone(cache);
        let panel_weak_recompute = panel_weak.clone();
        let displayed_recompute = Rc::clone(&displayed);
        let tune_for_recompute = Rc::clone(tune_to_satellite);
        let watched_for_recompute = Rc::clone(&watched);
        let config_for_recompute = std::sync::Arc::clone(config);
        Rc::new(move || {
            let Some(panel) = panel_weak_recompute.upgrade() else {
                return;
            };
            // Drop the previous pass rows — these are throwaway,
            // built fresh per recompute.
            for entry in displayed_recompute.borrow_mut().drain(..) {
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
            let passes = enumerate_upcoming_passes(&cache_recompute, &station, now);

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
                if let Some((freq_hz, mode, bw_hz, _protocol, _norad_id)) =
                    tune_target_for_pass(&pass)
                {
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
                    // Tooltips aren't read by screen readers — set
                    // the accessible label too, matching the
                    // project rule for icon-only buttons.
                    let a11y_label = format!("Tune to {} downlink", pass.satellite);
                    play_btn
                        .update_property(&[gtk4::accessible::Property::Label(a11y_label.as_str())]);
                    let tune_for_click = Rc::clone(&tune_for_recompute);
                    play_btn.connect_clicked(move |_| {
                        tune_for_click(freq_hz, mode, bw_hz);
                    });
                    row.add_suffix(&play_btn);
                }
                // 🔔 watch-toggle (#510) — per-satellite, NOT
                // per-pass. Toggling on row N flips the user's
                // subscription for THIS satellite. Mirrored across
                // sibling rows in the toggle handler so two rows
                // of the same satellite (NOAA 19 typically has 4-6
                // passes per day) stay in sync. `None` for
                // off-catalog passes — no NORAD id, no
                // notification target, no button.
                let bell_btn = if let Some(norad_id) = norad_id_for_pass(&pass) {
                    let initial_active = watched_for_recompute.borrow().contains(&norad_id);
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
                    bell_btn
                        .update_property(&[gtk4::accessible::Property::Label(a11y_label.as_str())]);
                    let watched_for_toggle = Rc::clone(&watched_for_recompute);
                    let config_for_toggle = std::sync::Arc::clone(&config_for_recompute);
                    // `Weak` (not strong `Rc`) breaks the cycle:
                    // bell_btn → handler closure → Rc → Vec
                    // <DisplayedPass> → bell_btn. With a strong ref
                    // here, removing rows from `passes_group`
                    // wouldn't drop the bell_btn, which would keep
                    // the closure (and the Vec) pinned forever.
                    let displayed_for_toggle = Rc::downgrade(&displayed_recompute);
                    bell_btn.connect_toggled(move |b| {
                        let active = b.is_active();
                        {
                            let mut set = watched_for_toggle.borrow_mut();
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
                                save_watched_satellites(&config_for_toggle, &set);
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
                        let Some(displayed) = displayed_for_toggle.upgrade() else {
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
                    });
                    row.add_suffix(&bell_btn);
                    Some(bell_btn)
                } else {
                    None
                };
                panel.passes_group.add(&row);
                new_rows.push(DisplayedPass {
                    row,
                    pass,
                    bell_btn,
                });
            }
            *displayed_recompute.borrow_mut() = new_rows;
        })
    } else {
        // No cache → no enumeration. Lat/lon/alt notify handlers
        // still call this on every change; making it a no-op
        // keeps the call sites branch-free.
        Rc::new(|| {})
    };

    // Initial paint — show passes immediately if we already have
    // cached TLEs from a prior session. (No-op if cache is None.)
    recompute();

    // Lat / lon / alt — persist on change and re-run pass
    // enumeration. Cheap: a single SGP4 sweep across ~7
    // satellites takes well under a millisecond.
    {
        let config_lat = std::sync::Arc::clone(config);
        let recompute_lat = Rc::clone(&recompute);
        panel.lat_row.connect_value_notify(move |row| {
            save_f64(&config_lat, KEY_STATION_LAT_DEG, row.value());
            recompute_lat();
        });
    }
    {
        let config_lon = std::sync::Arc::clone(config);
        let recompute_lon = Rc::clone(&recompute);
        panel.lon_row.connect_value_notify(move |row| {
            save_f64(&config_lon, KEY_STATION_LON_DEG, row.value());
            recompute_lon();
        });
    }
    {
        let config_alt = std::sync::Arc::clone(config);
        let recompute_alt = Rc::clone(&recompute);
        panel.alt_row.connect_value_notify(move |row| {
            save_f64(&config_alt, KEY_STATION_ALT_M, row.value());
            recompute_alt();
        });
    }

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
    restore_doppler_switch(panels, config);
    if let Some(cache_doppler) = cache.as_ref() {
        connect_doppler_tracker(panels, state, cache_doppler, status_bar);
    }

    // Refresh button — re-download every known satellite's TLE on
    // a worker thread, update the timestamp row, and rebuild the
    // pass list. Same `spawn_future_local` + `spawn_blocking`
    // pattern as the RadioReference search button. Wired only
    // when the cache is available; otherwise the button was
    // already disabled above.
    if let Some(cache_outer) = cache.as_ref() {
        let cache_refresh = std::sync::Arc::clone(cache_outer);
        let config_refresh = std::sync::Arc::clone(config);
        let panel_weak_refresh = panel_weak.clone();
        let recompute_refresh = Rc::clone(&recompute);
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
                let result = gio::spawn_blocking(move || {
                    // `force_refresh` — NOT `tle_text` — because the
                    // user clicked Refresh and a fresh-cache fast-path
                    // would let us mark "Last refreshed: now" without
                    // any actual network fetch. `force_refresh` always
                    // hits the network and never falls back to a stale
                    // cache, so a successful return means a real round
                    // trip happened. A per-satellite failure is logged
                    // and skipped so a single decommissioned /
                    // rate-limited entry can't break the whole
                    // refresh: the user still gets the rest.
                    let mut last_err: Option<sdr_sat::TleCacheError> = None;
                    let mut succeeded = 0usize;
                    for known in KNOWN_SATELLITES {
                        match cache_task.force_refresh(known.norad_id) {
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
                        // Every fetch failed — surface the last error so
                        // the UI can show it. (If at least one
                        // succeeded, treat the refresh as "done" so
                        // the user sees the timestamp tick forward.)
                        Err(last_err.unwrap_or_else(|| {
                            sdr_sat::TleCacheError::Fetch(
                                "refresh produced no successful fetches".to_string(),
                            )
                        }))
                    } else {
                        Ok(())
                    }
                })
                .await;

                let Some(panel) = panel_weak_done.upgrade() else {
                    return;
                };
                panel.refresh_spinner.stop();
                panel.refresh_spinner.set_visible(false);
                panel.refresh_button.set_sensitive(true);

                match result {
                    Ok(Ok(())) => {
                        let now = chrono::Utc::now();
                        save_tle_last_refresh(&config_done, now);
                        panel
                            .last_refresh_row
                            .set_subtitle(&format_last_refresh(&config_done));
                        recompute_done();
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
            });
        });
    }

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
    {
        let in_flight: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
        let run_lookup: Rc<dyn Fn(adw::EntryRow)> = {
            let panel_weak_zip = panel_weak.clone();
            let in_flight_run = Rc::clone(&in_flight);
            Rc::new(move |entry: adw::EntryRow| {
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
                let in_flight_done = Rc::clone(&in_flight_run);
                let zip_for_task = zip.clone();
                glib::spawn_future_local(async move {
                    // Chain the two lookups on the worker thread so the
                    // UI side just gets one result. ZIP failure is fatal
                    // for this run; elevation failure is logged and
                    // demoted to `Ok(_, None)` — altitude is best-effort
                    // since it barely matters for pass prediction
                    // anyway, and we'd rather populate lat/lon than
                    // leave the user staring at an error toast.
                    let result = gio::spawn_blocking(move || -> Result<
                    (sdr_sat::PostalLocation, Result<f64, String>),
                    sdr_sat::PostalLookupError,
                > {
                    let loc = sdr_sat::lookup_us_zip(&zip_for_task)?;
                    // Elevation lookup is best-effort — a failure here
                    // shouldn't fail the whole flow, but we DO want
                    // the provider error to reach the UI so the user
                    // can see why altitude didn't update. Pass it
                    // back as `Err(String)` (cheap to send across
                    // thread, decoupled from `ElevationLookupError`'s
                    // type, and the log already scrubbed lat/lon).
                    let elevation = match sdr_sat::lookup_elevation_m(loc.lat_deg, loc.lon_deg)
                    {
                        Ok(m) => Ok(m),
                        Err(e) => {
                            // Don't include lat/lon in the log — that's user
                            // location data. The error message itself is
                            // safe (it's the upstream HTTP error / parse
                            // error / dataset-coverage error).
                            tracing::warn!("elevation lookup failed: {e}");
                            Err(e.to_string())
                        }
                    };
                    Ok((loc, elevation))
                })
                .await;

                    in_flight_done.set(false);
                    let Some(panel) = panel_weak_done.upgrade() else {
                        return;
                    };
                    panel.zip_row.set_sensitive(true);

                    match result {
                        Ok(Ok((loc, elevation))) => {
                            // Order matters slightly: setting lat/lon/alt
                            // fires `value-notify`, which persists the
                            // value and triggers `recompute`. Three
                            // recomputes back-to-back is fine —
                            // sub-millisecond each.
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
                                Err(e) => {
                                    // Leave altitude alone but
                                    // surface the provider error
                                    // so the user knows what to
                                    // try next (e.g. retry on a
                                    // bad network).
                                    format!("Resolved: {where_text} (altitude unchanged: {e})")
                                }
                            };
                            panel.zip_status_row.set_title(&status);
                        }
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

    // 1 Hz countdown ticker. Only scheduled when the cache is
    // available — without it `displayed` stays empty forever and
    // the timer would tick uselessly. Captures the panel weakly
    // so the source returns `ControlFlow::Break` once any panel
    // widget has been dropped (otherwise GLib runs it forever,
    // holding a strong chain into the `displayed` vec and its
    // widgets).
    // Auto-record-on-pass state machine (#482b). Driven from the
    // same 1 Hz tick that updates pass-row countdowns — no second
    // GLib source. The recorder itself is pure (returns
    // `Vec<RecorderAction>`); the closure below interprets each
    // action against the live UI / DSP / filesystem.
    let recorder: Rc<RefCell<AutoRecorder>> = Rc::new(RefCell::new(AutoRecorder::new()));

    // Parent-window resolver for the auto-open-viewer side effect.
    // Walks up the widget tree from the satellites page; falls
    // back to `None` if the widget has been detached, in which
    // case the open is silently skipped. Holds a `WeakRef` so the
    // 1 Hz timer's `panel_weak.upgrade() == None` exit gate can
    // actually fire — a strong clone here would keep the panel
    // widget alive and the timer would never break.
    let parent_provider_for_recorder: Rc<dyn Fn() -> Option<gtk4::Window>> = {
        let widget_weak = panel.widget.downgrade();
        Rc::new(move || {
            widget_weak
                .upgrade()
                .and_then(|w| w.root())
                .and_then(|r| r.downcast::<gtk4::Window>().ok())
        })
    };

    let interpret_action: Rc<dyn Fn(RecorderAction)> = {
        let state_a = Rc::clone(state);
        let tune_a = Rc::clone(tune_to_satellite);
        let set_playing_a = Rc::clone(set_playing);
        // Optional TLE cache — used by the SavePng wiring to compute
        // `is_ascending` for the rotate-180 flag (B2 of the noaa-apt
        // parity work). `None` when the host platform refused us a
        // cache directory; the rotate path falls back to "no rotation"
        // in that case.
        let cache_a: Option<std::sync::Arc<TleCache>> = cache.as_ref().map(std::sync::Arc::clone);
        // Weak ref for the same lifecycle reason as
        // `parent_provider_for_recorder` — strong clone would pin
        // the toast overlay alive past window close.
        let toast_overlay_weak = toast_overlay.downgrade();
        let parent_provider_a = Rc::clone(&parent_provider_for_recorder);
        // Scanner master switch handle for the LOS-side restore.
        // Set-active here fires the switch's `connect_active_notify`
        // handler, which re-dispatches `SetScannerEnabled(true)` and
        // re-arms the engine — same path the user takes when they
        // flip the switch by hand.
        let scanner_switch_a = panels.scanner.master_switch.clone();
        // Audio-chain widgets — force-disabled at AOS, restored at
        // LOS via the same `set_*` notify chain that any user flip
        // takes. Per #555 (squelch + CTCSS) and #556 (FM IF NR).
        let squelch_enabled_row_a = panels.radio.squelch_enabled_row.clone();
        let auto_squelch_row_a = panels.radio.auto_squelch_row.clone();
        let squelch_level_row_a = panels.radio.squelch_level_row.clone();
        let ctcss_row_a = panels.radio.ctcss_row.clone();
        let fm_if_nr_row_a = panels.radio.fm_if_nr_row.clone();
        // AF-chain widgets that also have to be disabled at AOS for
        // the satellite-image decoders. The 2400 Hz APT subcarrier
        // sits in the rolloff of US 75 µs / EU 50 µs deemphasis
        // (cutoff ~2122 Hz / ~3183 Hz respectively); a notch
        // anywhere in the 1.5-3 kHz audio band would also kill the
        // subcarrier. Like the gates above, these flip via the
        // widget's `notify` handler so the DSP gets a normal
        // `SetDeemphasis(None)` / `SetNotchEnabled(false)` and the
        // LOS restore-from-config path runs the same way it does for
        // squelch / CTCSS / FM-IF-NR. Per silent-fail investigation
        // following the NOAA 15 pass.
        let deemphasis_row_a = panels.radio.deemphasis_row.clone();
        let notch_enabled_row_a = panels.radio.notch_enabled_row.clone();
        // Doppler tracker master switch — also disabled at AOS for
        // the duration of an imaging-protocol pass. The 4 Hz tick
        // re-dispatches `SetVfoOffset(predicted_doppler)` which
        // can disrupt QPSK Costas lock (LRPT) and APT's line-rate
        // clock. The ±3.5 kHz worst-case Doppler shift at 137 MHz
        // / 145 MHz is well inside all three imaging-protocol
        // channel filters (APT 38 kHz, LRPT 144 kHz, SSTV
        // 12.5 kHz), so disabling for the pass loses no functional
        // value. Per silent-fail investigation following the
        // NOAA 15 pass.
        let doppler_switch_a = panels.satellites.doppler_switch.clone();
        // Composite-save toggle — read at LOS by the
        // `SaveLrptPass` handler. Strong clone so the closure
        // doesn't need to upgrade a weak ref against panel
        // teardown ordering: every other panel widget the
        // closure captures is a strong clone too. Per #547.
        let auto_record_composites_switch_a = panel.auto_record_composites_switch.clone();
        let post_toast = move |overlay_weak: &glib::WeakRef<adw::ToastOverlay>, msg: &str| {
            if let Some(overlay) = overlay_weak.upgrade() {
                overlay.add_toast(plain_toast(msg));
            }
        };
        // Compute the rotate-180 flag for the currently-recording APT
        // pass: `true` when the satellite is on the ascending leg of
        // its orbit, which means the assembled image is upside-down +
        // mirrored east/west (per `sdr_radio::apt_image::rotate_180_per_channel`).
        // Falls back to `false` (no rotation) on any failure — TLE
        // cache miss, parse failure, propagation error, or
        // recording-pass info missing. The default is safe: NOAA
        // satellites are sun-synchronous, so the descending pass is
        // the typical case for daytime captures and no-rotation
        // preserves north-at-top.
        // Takes the recording-pass tuple directly (`norad_id`, `aos`)
        // rather than reading it from `AppState`, so callers compute
        // rotation against an explicit snapshot. Without this, the
        // SavePng handler could read a freshly-overwritten slot if a
        // back-to-back AOS landed between this pass's LOS dispatch
        // and the rotation compute — exporting the older pass's
        // image with the newer pass's orientation. Per CR round 6
        // on PR #571.
        let compute_apt_rotate_180_for_pass = {
            let cache_a = cache_a.clone();
            move |norad_id: u32, aos: chrono::DateTime<chrono::Utc>| -> bool {
                let Some(cache) = cache_a.as_ref() else {
                    return false;
                };
                // Look up by stable NORAD id (not display name) so
                // a catalog rename doesn't silently break this
                // path. Per CR round 2 on PR #571.
                let Some(known) = sdr_sat::KNOWN_SATELLITES
                    .iter()
                    .find(|s| s.norad_id == norad_id)
                else {
                    tracing::debug!(
                        norad_id,
                        "APT rotate-180: satellite not in catalog; defaulting to no rotation",
                    );
                    return false;
                };
                let (line1, line2) = match cache.cached_tle_for(known.norad_id) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!(
                            norad_id,
                            error = %e,
                            "APT rotate-180: TLE unavailable; defaulting to no rotation",
                        );
                        return false;
                    }
                };
                let parsed = match sdr_sat::Satellite::from_tle(known.name, &line1, &line2) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!(
                            norad_id,
                            error = %e,
                            "APT rotate-180: TLE parse failed; defaulting to no rotation",
                        );
                        return false;
                    }
                };
                match sdr_sat::is_ascending(&parsed, aos) {
                    Ok(asc) => asc,
                    Err(e) => {
                        tracing::debug!(
                            norad_id,
                            error = %e,
                            "APT rotate-180: SGP4 propagate failed; defaulting to no rotation",
                        );
                        false
                    }
                }
            }
        };
        Rc::new(move |action: RecorderAction| match action {
            RecorderAction::StartAutoRecord {
                satellite,
                norad_id,
                freq_hz,
                mode,
                bandwidth_hz,
                protocol,
            } => {
                tracing::info!(
                    "auto-record AOS: tuning to {satellite} @ {freq_hz} Hz, BW {bandwidth_hz} Hz, protocol {protocol:?}",
                );
                // ACARS-engaged gating happens at the recorder
                // tick site (the for-loop calling
                // `interpret_action`), not here — that's the
                // only level that has visibility into the
                // entire `Vec<RecorderAction>` batch and can
                // defer it as a unit. CR round 1 on PR #591
                // flagged the gap: stashing only this single
                // `StartAutoRecord` while iterating the rest
                // of the batch left `StartAutoAudioRecord` etc.
                // running while ACARS was still engaged.
                // Per-protocol viewer dispatch. Adding a new
                // protocol means adding a match arm here +
                // flipping `imaging_protocol` on the catalog
                // entry — no recorder change needed. Per #514.
                //
                // **Fail closed on unsupported protocols.** All
                // AOS side effects (set_playing, tune, zero VFO,
                // open viewer) live INSIDE each arm rather than
                // unconditionally before the match. Per CR
                // round 1 on PR #541: if a catalog entry is
                // flipped to `Some(Lrpt)` ahead of Task 7 wiring
                // the LRPT viewer, the user's tune state must
                // NOT be hijacked just to land in a no-op
                // branch.
                // Audio-chain force-off helper. Flips the three
                // user-visible widgets to disabled; each one's
                // `connect_*_notify` handler then dispatches the
                // corresponding `UiToDsp::Set*` message — same
                // path a manual flip takes. Per #555 (squelch +
                // CTCSS) / #556 (FM IF NR). Idempotent: a
                // `set_active(false)` on an already-false switch
                // is a no-op (no notify fired, no redundant
                // dispatch).
                //
                // Called inside each protocol arm rather than
                // before the match so an unsupported protocol
                // doesn't hijack the user's audio chain (mirrors
                // the AOS side-effect-isolation rule from CR
                // round 1 on PR #541).
                let force_audio_chain_off = || {
                    if squelch_enabled_row_a.is_active() {
                        squelch_enabled_row_a.set_active(false);
                    }
                    if auto_squelch_row_a.is_active() {
                        auto_squelch_row_a.set_active(false);
                    }
                    let off_idx =
                        sidebar::radio_panel::RadioPanel::ctcss_index_from_mode(CtcssMode::Off);
                    if ctcss_row_a.selected() != off_idx {
                        ctcss_row_a.set_selected(off_idx);
                    }
                    if fm_if_nr_row_a.is_active() {
                        fm_if_nr_row_a.set_active(false);
                    }
                    // De-emphasis ComboRow index 0 = "None". Both
                    // US 75 µs (cutoff ~2122 Hz) and EU 50 µs
                    // (cutoff ~3183 Hz) attenuate the 2400 Hz APT
                    // subcarrier that lives in the demodulated
                    // audio. Force to "None" so the AF chain
                    // passes the subcarrier through flat. Per
                    // silent-fail investigation following the
                    // NOAA 15 pass.
                    let deemp_off_idx: u32 = 0;
                    if deemphasis_row_a.selected() != deemp_off_idx {
                        deemphasis_row_a.set_selected(deemp_off_idx);
                    }
                    // Notch: a user-set notch frequency anywhere in
                    // the 1.5-3 kHz audio band (the typical "remove
                    // a hum" use case) would null the APT subcarrier.
                    // Force off; LOS restore reapplies the persisted
                    // value via the same notify chain.
                    if notch_enabled_row_a.is_active() {
                        notch_enabled_row_a.set_active(false);
                    }
                    // Doppler tracker: 4 Hz `SetVfoOffset` ticks can
                    // disrupt QPSK Costas lock and the APT line-rate
                    // clock. The ±3.5 kHz worst-case shift fits
                    // inside every imaging-protocol channel filter,
                    // so disabling for the pass loses no functional
                    // value and removes a known disruption source.
                    // Per silent-fail investigation following the
                    // NOAA 15 pass.
                    if doppler_switch_a.is_active() {
                        doppler_switch_a.set_active(false);
                    }
                };
                match protocol {
                    sdr_sat::ImagingProtocol::Apt => {
                        // **Order is load-bearing.** Force the
                        // audio chain off BEFORE start/tune so
                        // the very first samples through the
                        // freshly-tuned demod aren't gated by a
                        // stale squelch / CTCSS / FM-IF-NR
                        // setting. Otherwise low-SNR AOS rows
                        // race against the change-notify chain
                        // dispatching the SetX messages and the
                        // first few scan lines could be silenced
                        // before the demod even sees them. Per
                        // CR round 1 on PR #557.
                        force_audio_chain_off();
                        // Drive Start through the header play
                        // button — its `connect_toggled` handler
                        // is the single place that updates
                        // `state.is_running`, dispatches
                        // `UiToDsp::Start`, and swaps the
                        // play/stop icon. `set_active` is a
                        // no-op when the radio is already
                        // running, so this is safe to call
                        // unconditionally without a duplicate
                        // Start. The pre-AOS `was_running` flag
                        // (captured in `SavedTune` by the 1 Hz
                        // tick before this action fires) drives
                        // the corresponding LOS-side stop.
                        set_playing_a(true);
                        tune_a(freq_hz, mode, bandwidth_hz);
                        // Zero the live VFO offset for the
                        // auto-record pass. The user's pre-AOS
                        // offset (a manual VFO drag away from
                        // centre) is preserved in `SavedTune`
                        // for the LOS restore, but during the
                        // pass the demod must align *exactly*
                        // with the satellite's downlink —
                        // otherwise we'd demod at `freq_hz +
                        // saved_offset` and the APT subcarrier
                        // would land outside the channel
                        // filter. The DSP's
                        // `DspToUi::VfoOffsetChanged` echo
                        // updates the spectrum widget, freq
                        // selector, and status bar; no manual
                        // mirror needed.
                        state_a.dispatch_vfo_offset(0.0);
                        crate::apt_viewer::open_apt_viewer_if_needed(&parent_provider_a, &state_a);
                        // Clear the canvas at AOS so a back-to-back
                        // pass (e.g. NOAA 18 → NOAA 19 with
                        // overlapping viewer sessions) starts on a
                        // clean image. The viewer was either just
                        // opened above (already empty) or carried
                        // over from a previous pass — either way,
                        // an explicit clear keeps the image we're
                        // about to save scoped to *this* pass.
                        if let Some(view) = state_a.apt_viewer.borrow().as_ref() {
                            view.clear();
                        }
                        // Stash recording-pass info for the LOS-side
                        // SavePng wiring to compute the auto-rotation
                        // flag (B2 of the noaa-apt parity work). The
                        // exact AOS time matters less than "around
                        // when" — `is_ascending` checks the lat
                        // derivative over a 30 s window, which is
                        // valid anywhere mid-pass. NORAD id arrives
                        // pre-resolved on the recorder action — no
                        // name → catalog lookup at this layer means
                        // no silent rotation breakage if the catalog
                        // ever picks up alias drift. Per CR round 3
                        // on PR #571.
                        let aos = chrono::Utc::now();
                        *state_a.apt_recording_pass.borrow_mut() = Some((norad_id, aos));
                        // Push the rotate-180 flag down to the
                        // renderer so the toolbar's manual `Export
                        // PNG` button matches the auto-record
                        // orientation. Without this, manual exports
                        // of ascending passes come out upside-down
                        // even though the auto-record save rotates
                        // them. Per CR round 1 on PR #571.
                        let rotate_180 = compute_apt_rotate_180_for_pass(norad_id, aos);
                        if let Some(view) = state_a.apt_viewer.borrow().as_ref() {
                            view.set_rotate_180(rotate_180);
                        }
                    }
                    sdr_sat::ImagingProtocol::Lrpt => {
                        // **Order is load-bearing.** Reset the
                        // shared image AND the viewer canvas
                        // BEFORE starting playback / retuning
                        // so the freshly-tuned LRPT decoder
                        // can't push pass-1 leftover rows (or
                        // race the clear and erase the first
                        // few rows of the new pass). Per
                        // CodeRabbit round 8 on PR #543.
                        //
                        // The viewer is opened first so the
                        // `view.clear()` call below can target
                        // it — the open path also sends
                        // `UiToDsp::SetLrptImage(handle)` to
                        // the DSP thread, which lazy-inits the
                        // decoder against the (now-cleared)
                        // shared image. Catalog's
                        // `demod_mode: Lrpt` lines up the
                        // controller's IF rate (144 ksps) with
                        // the QPSK demod's expected sample rate
                        // — without that, `radio_input` would
                        // be at the wrong rate and the demod's
                        // resampler would sit at the wrong
                        // setpoint. Per epic #469 task 7.

                        // Tell the DSP thread which Meteor
                        // downlink profile to use for this pass
                        // — METEOR-M N2 was QPSK with
                        // differential precoding; the active
                        // METEOR-M2 3 / M2-4 are plain OQPSK
                        // (#730). Sent
                        // BEFORE `open_lrpt_viewer_if_needed`
                        // (which triggers lazy decoder init)
                        // so the freshly-built decoder uses
                        // the right inner chain. Per #662.
                        //
                        // Always send — even when the catalog
                        // lookup misses or returns
                        // `lrpt_modulation = None`. Otherwise
                        // the previous pass's modulation leaks
                        // into the next decoder init (the
                        // controller stash defaults to OQPSK at
                        // startup but mutates per
                        // `SetLrptDownlink`, so a Qpsk-then-
                        // None sequence would silently keep the
                        // Qpsk chain). Fallback is `Qpsk`
                        // because that's the standards-default
                        // LRPT modulation; a future LRPT
                        // satellite that turns up uncatalogued
                        // is more likely to be standard-spec
                        // than Meteor-style OQPSK. Per CR
                        // round 1 on PR #663.
                        // Profile first, then the canvas wipe on the
                        // DSP thread — the order matters (see
                        // `lrpt_pass_start_commands`).
                        for command in crate::lrpt_viewer::lrpt_pass_start_commands(
                            norad_id,
                            &state_a.lrpt_image,
                        ) {
                            state_a.send_dsp(command);
                        }

                        crate::lrpt_viewer::open_lrpt_viewer_if_needed(
                            &parent_provider_a,
                            &state_a,
                        );
                        if let Some(view) = state_a.lrpt_viewer.borrow().as_ref() {
                            view.clear();
                        }
                        // Force audio chain off BEFORE start/tune
                        // so the freshly-tuned demod isn't gated
                        // by a stale squelch / CTCSS / FM-IF-NR
                        // setting. Per CR round 1 on PR #557 —
                        // see APT arm above for the full
                        // rationale.
                        force_audio_chain_off();
                        // Now safe to start playback + retune;
                        // any decoded rows from this point
                        // forward land in the cleared image.
                        set_playing_a(true);
                        tune_a(freq_hz, mode, bandwidth_hz);
                        state_a.dispatch_vfo_offset(0.0);
                        // Mirror into AppState so is_recording() (used by
                        // the close-to-tray Quit confirmation modal)
                        // reflects an in-progress LRPT pass. Per #512.
                        // The `(norad_id, aos)` tuple lets the LOS
                        // completion path snapshot-and-compare so an
                        // overlapping pass-N+1 AOS that starts during
                        // pass-N's encode doesn't have its `is_recording`
                        // flag clobbered when pass-N's completion
                        // fires. Mirrors the APT
                        // `apt_recording_pass` pattern from PR #571
                        // round 4. Per CR round 2 on PR #575.
                        let aos = chrono::Utc::now();
                        *state_a.lrpt_recording_pass.borrow_mut() = Some((norad_id, aos));
                    }
                    sdr_sat::ImagingProtocol::Sstv => {
                        // **Order is load-bearing.** Open the viewer
                        // (which sends `UiToDsp::SetSstvImage`) and
                        // clear both the shared handle and the canvas
                        // BEFORE starting playback / retuning so no
                        // leftover rows from a previous pass land in
                        // the fresh image buffer.  Mirrors the LRPT
                        // arm's clear-before-start discipline from
                        // CR round 8 on PR #543.
                        crate::sstv_viewer::open_sstv_viewer_if_needed(
                            &parent_provider_a,
                            &state_a,
                        );
                        state_a.sstv_image.clear();
                        // Failed-pass images now live in
                        // `sstv_pending_export` keyed by their
                        // original pass directory (moved there at
                        // `SaveSstvPass` time, not here at AOS).
                        // The current-pass buffer is the round-4
                        // simple clear. Per CR round 6 #21 on
                        // PR #599 (refines round 5 #20).
                        state_a.sstv_completed_images.borrow_mut().clear();
                        if let Some(view) = state_a.sstv_viewer.borrow().as_ref() {
                            view.clear();
                        }
                        // ISS SSTV is audible NFM — do NOT force the
                        // audio chain off.  The user's squelch /
                        // CTCSS / FM-IF-NR settings are correct for
                        // the mode; we only suppress them for
                        // silent-passthrough decoders (LRPT) and the
                        // APT path (subcarrier noise). Per CLAUDE.md
                        // and step 8 of epic #472 task spec.
                        set_playing_a(true);
                        tune_a(freq_hz, mode, bandwidth_hz);
                        state_a.dispatch_vfo_offset(0.0);
                        let aos = chrono::Utc::now();
                        *state_a.sstv_recording_pass.borrow_mut() = Some((norad_id, aos));
                    }
                }
            }
            RecorderAction::StartAutoAudioRecord(path) => {
                tracing::info!("auto-record AOS: opening WAV writer at {path:?}");
                state_a.send_dsp(UiToDsp::StartAudioRecording(path));
            }
            RecorderAction::StopAutoAudioRecord => {
                tracing::info!("auto-record LOS: closing WAV writer");
                state_a.send_dsp(UiToDsp::StopAudioRecording);
            }
            RecorderAction::ResetImagingDecoders => {
                // Between-pass decoder flush. The state machine
                // emits this at every `Recording → Finalizing`
                // transition (LOS), AFTER the save action's
                // snapshot read of the shared `LrptImage`. When
                // `was_running == true` pre-AOS this is the only
                // hook between passes — `RestoreTune` keeps the
                // source open across LOS → AOS, so the
                // source-stop reset never fires. When
                // `was_running == false` the subsequent
                // `set_playing(false)` in `RestoreTune` triggers
                // the source-stop path which resets again —
                // idempotent (`reset_imaging_decoders` only
                // touches in-flight buffers), so the
                // double-reset is harmless. Per issue #544.
                tracing::info!("auto-record LOS: resetting imaging decoders");
                state_a.send_dsp(UiToDsp::ResetImagingDecoders);
            }
            RecorderAction::SavePng(path) => {
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
                let exported_pass = *state_a.apt_recording_pass.borrow();
                // Compute the rotate-180 flag for ascending passes
                // (B2 of the noaa-apt parity work) FROM THE SNAPSHOT,
                // not from the live `state_a.apt_recording_pass`. The
                // helper resolves the satellite's TLE from the cache
                // and calls `sdr_sat::is_ascending` at the snapshotted
                // AOS sample point. Defaults to `false` (no rotation)
                // if any step fails — descending-pass orientation is
                // the safer default since it preserves north-at-top.
                let rotate_180 = exported_pass
                    .is_some_and(|(norad_id, aos)| compute_apt_rotate_180_for_pass(norad_id, aos));
                let mode = sdr_radio::apt_image::BrightnessMode::default();
                // Async export: snapshot the AptImage on the main
                // thread NOW, hand the snapshot to a worker via
                // `gio::spawn_blocking`. The encode for a 1500-line
                // pass is multi-hundred-ms — synchronously running
                // it here would freeze GTK during LOS, exactly when
                // the user wants to see the toast and have the
                // window auto-close cleanly. Per CR round 1 on PR
                // #571.
                let view_opt = state_a.apt_viewer.borrow().as_ref().cloned();
                let Some(view) = view_opt else {
                    tracing::warn!(
                        "auto-record SavePng but no APT viewer is open (user closed mid-pass)",
                    );
                    post_toast(
                        &toast_overlay_weak,
                        "Pass complete, but the APT viewer was closed — no image saved",
                    );
                    // Same overlap-guard as the async-callback path:
                    // only clear the slot if it still holds the pass
                    // we entered this branch with. If a new AOS
                    // wrote a fresh tuple in the meantime, leave it
                    // alone.
                    {
                        let mut slot = state_a.apt_recording_pass.borrow_mut();
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
                let toast_overlay_for_complete = toast_overlay_weak.clone();
                let state_for_complete = Rc::clone(&state_a);
                // Snapshot the *current* viewer-window WeakRef BEFORE
                // spawning the worker. If the user closes the viewer
                // mid-export and reopens it, `state.apt_viewer_window`
                // will point at the new window by the time the
                // callback fires; reading from there could close the
                // wrong window. Cloning the WeakRef pins the
                // identity of the window we'll attempt to close, while
                // staying weak so a closed/dropped window upgrades to
                // None and we no-op. Per CR round 3 on PR #571.
                let exported_window_weak = state_a.apt_viewer_window.borrow().as_ref().cloned();
                view.export_png_full_async(path_for_export, mode, rotate_180, move |result| {
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
                                format!(
                                    "Pass complete — image saved to {}",
                                    path_for_msg.display()
                                ),
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
                    post_toast(&toast_overlay_for_complete, &msg);
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
                        if let Some(window) = exported_window_weak
                            .as_ref()
                            .and_then(glib::WeakRef::upgrade)
                        {
                            tracing::info!(
                                "auto-record LOS: closing APT viewer window after PNG save",
                            );
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
                        let mut slot = state_for_complete.apt_recording_pass.borrow_mut();
                        if *slot == exported_pass {
                            *slot = None;
                        }
                    }
                });
            }
            RecorderAction::SaveLrptPass(dir) => {
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
                let snapshots: Vec<(u16, sdr_lrpt::image::ChannelBuffer)> = {
                    let mut sorted = state_a.lrpt_image.channel_apids();
                    sorted.sort_unstable();
                    sorted
                        .into_iter()
                        .filter_map(|apid| {
                            state_a
                                .lrpt_image
                                .snapshot_channel(apid)
                                .filter(|s| s.lines > 0)
                                .map(|s| (apid, s))
                        })
                        .collect()
                };
                // Composite snapshots — only when the user opted in
                // via the panel toggle. Each entry is the recipe +
                // a [`CompositeSnapshot`] (cloned source channel
                // pixel buffers + truncated height). Built on the
                // GTK main thread but the assembler lock is held
                // only long enough to memcpy the source channels
                // (~5 ms per recipe for full-pass data, vs ~30 ms
                // for the full RGB interleave that happens in the
                // worker). The expensive per-pixel interleave +
                // PNG encode runs inside `gio::spawn_blocking`
                // below. Per CR round 1 on PR #575.
                let composites_on = auto_record_composites_switch_a.is_active();
                let composite_snapshots: Vec<(
                    crate::lrpt_viewer::CompositeRecipe,
                    sdr_lrpt::image::CompositeSnapshot,
                )> = if composites_on {
                    state_a.lrpt_image.with_assembler(|a| {
                        crate::lrpt_viewer::COMPOSITE_CATALOG
                            .iter()
                            .filter_map(|recipe| {
                                a.clone_channels_for_composite(
                                    recipe.r_apid,
                                    recipe.g_apid,
                                    recipe.b_apid,
                                )
                                .map(|snap| (*recipe, snap))
                            })
                            .collect()
                    })
                } else {
                    Vec::new()
                };
                let toast_overlay_weak_for_save = toast_overlay_weak.clone();
                // Clone state for the post-save viewer-close
                // — we need to read `state.lrpt_recording_pass`
                // after the spawn_blocking completes, which
                // requires capturing state into the future.
                let state_lrpt_close = Rc::clone(&state_a);
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
                let exported_lrpt_window_weak =
                    state_a.lrpt_viewer_window.borrow().as_ref().cloned();
                // Snapshot the recording-pass tuple FIRST so the
                // post-save clear is gated on "this is still the
                // pass we entered with". An overlapping pass-N+1
                // AOS that starts while pass-N is still encoding
                // would otherwise have its slot clobbered when
                // pass-N's completion callback fires `*slot =
                // None`. Same shape as the APT compare-and-clear
                // at `RecorderAction::SavePng`. Per CR round 2 on
                // PR #575.
                let exported_lrpt_pass = *state_a.lrpt_recording_pass.borrow();
                // Capture "no APIDs decoded" up front. This case has
                // no in-memory imagery to retry — the viewer is empty
                // — so the LOS close gate should fire even though
                // `save_ok` will be false. Without this, the viewer
                // would sit open with a blank canvas across silent
                // Meteor passes (Russian sats are intermittent;
                // many passes produce no LRPT). Per silent-pass
                // diagnosis 2026-05-08.
                let pass_decoded_nothing = snapshots.is_empty();
                // Diagnostic: warn if the satellite delivered some
                // APIDs but not the full per-satellite expected set.
                // Catches schedule changes (e.g. Roscosmos flipping
                // M2-3 between summer-mode c1/c2/c3 and standard
                // c1/c2/c4) as a single log line instead of the user
                // wondering why some composite recipes silently
                // produced nothing. Silent passes are skipped — they're
                // a different failure mode handled by
                // `pass_decoded_nothing` above. Per #645.
                if !pass_decoded_nothing
                    && let Some((norad_id, _aos)) = exported_lrpt_pass
                    && let Some(sat) = sdr_sat::KNOWN_SATELLITES
                        .iter()
                        .find(|s| s.norad_id == norad_id)
                {
                    let received_apids: Vec<u16> =
                        snapshots.iter().map(|(apid, _)| *apid).collect();
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
                    let (result_msg, save_ok) = gio::spawn_blocking(move || {
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
                        if let Err(e) = std::fs::create_dir_all(&dir) {
                            // Per-pass directory created up
                            // front so a disk-full / permissions
                            // failure surfaces as a single
                            // observable error rather than `N`
                            // per-channel warnings. Per
                            // CodeRabbit round 1 on PR #543.
                            tracing::warn!(
                                "auto-record SaveLrptPass: failed to create directory {dir:?}: {e}",
                            );
                            return (
                                format!("Pass complete but couldn't create {}: {e}", dir.display()),
                                false,
                            );
                        }
                        let mut saved = 0_usize;
                        let mut errors: Vec<String> = Vec::new();
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
                                    saved += 1;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "auto-record LRPT export for APID {apid} to {path:?} failed: {e}",
                                    );
                                    errors.push(format!("APID {apid}: {e}"));
                                }
                            }
                        }
                        // Composite PNGs alongside the per-APID
                        // files. Filename is `composite-{slug}.png`
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
                            let slug = recipe
                                .name
                                .replace(' ', "-")
                                .replace(['/', '\\'], "_");
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
                                    saved += 1;
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
                    })
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
                        tracing::warn!(
                            "auto-record SaveLrptPass: worker thread panicked: {e:?}",
                        );
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
            RecorderAction::SaveSstvPass(dir) => {
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
                    std::mem::take(&mut *state_a.sstv_pending_export.borrow_mut());
                let current_images: Vec<sdr_radio::sstv_image::CompletedSstvImage> = state_a
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
                let toast_overlay_weak_for_save = toast_overlay_weak.clone();
                let state_sstv_close = Rc::clone(&state_a);
                // Snapshot the WeakRef BEFORE spawning so a
                // viewer reopen during the async save can't
                // trick us into closing the wrong window.
                // Mirrors the LRPT pattern from CR round 2 on
                // PR #575.
                let exported_sstv_window_weak =
                    state_a.sstv_viewer_window.borrow().as_ref().cloned();
                // Snapshot the recording-pass tuple for
                // compare-and-clear on completion — mirrors the
                // LRPT and APT patterns from PR #571 / #575.
                let exported_sstv_pass = *state_a.sstv_recording_pass.borrow();
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
                    let join = gio::spawn_blocking(move || {
                        save_sstv_batches(pending_batches, current_images, dir)
                    })
                    .await;
                    let SstvSaveOutcome {
                        message,
                        current_ok,
                        retained,
                    } = join.unwrap_or_else(|e| {
                        tracing::warn!("auto-record SaveSstvPass: worker thread panicked: {e:?}",);
                        // Re-construct the full retain list from
                        // the backups: prior pending batches
                        // (preserved as-is) plus the current pass
                        // re-keyed to its dir, so neither is
                        // silently dropped by the failure-path
                        // drain below. Per CR round 7 #25 on PR
                        // #599.
                        let mut retained = pending_batches_backup;
                        if !current_images_backup.is_empty() {
                            retained.push(PendingSstvExport {
                                dir: dir_backup.clone(),
                                // Original attempt would have
                                // started at index 0; on panic
                                // retry we reuse that start so
                                // filenames remain stable. Per
                                // CR round 8 #27 on PR #599.
                                start_index: 0,
                                images: current_images_backup,
                            });
                        }
                        SstvSaveOutcome {
                            message: format!(
                                "Pass complete but PNG worker panicked (target was {})",
                                dir_for_msg.display()
                            ),
                            current_ok: false,
                            retained,
                        }
                    });
                    post_toast(&toast_overlay_weak_for_save, &message);
                    // Restore retained batches (pending that still
                    // failed + the current batch if it failed) into
                    // `sstv_pending_export`. New pending items
                    // queued by a parallel AOS slip in *after* the
                    // retained set so retry order honours
                    // chronological pass start.
                    if !retained.is_empty() {
                        let mut pending = state_sstv_close.sstv_pending_export.borrow_mut();
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
                    let mut slot = state_sstv_close.sstv_recording_pass.borrow_mut();
                    if *slot == exported_sstv_pass {
                        if current_ok {
                            let mut completed = state_sstv_close.sstv_completed_images.borrow_mut();
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
                                    dir_for_msg.display()
                                );
                                state_sstv_close.sstv_pending_export.borrow_mut().push(
                                    PendingSstvExport {
                                        dir: dir_for_msg.clone(),
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
                                    },
                                );
                            }
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
                            let mut completed = state_sstv_close.sstv_completed_images.borrow_mut();
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
                        && state_sstv_close.sstv_completed_images.borrow().is_empty()
                        && let Some(window) = exported_sstv_window_weak
                            .as_ref()
                            .and_then(glib::WeakRef::upgrade)
                    {
                        tracing::info!(
                            "auto-record LOS: closing SSTV viewer window after PNG save"
                        );
                        window.close();
                    }
                });
            }
            RecorderAction::RestoreTune(saved) => {
                tracing::info!(
                    "auto-record LOS: restoring tune to {} Hz (offset {} Hz), BW {} Hz",
                    saved.freq_hz,
                    saved.vfo_offset_hz,
                    saved.bandwidth_hz,
                );
                #[allow(
                    clippy::cast_sign_loss,
                    clippy::cast_possible_truncation,
                    reason = "saved freq came from the same widget we're feeding back; \
                              non-negative and well within u64"
                )]
                let freq_hz = saved.freq_hz as u64;
                tune_a(freq_hz, saved.mode, saved.bandwidth_hz);
                // Replay the user's pre-AOS VFO offset so a
                // dragged-from-centre carrier comes back. The
                // existing `DspToUi::VfoOffsetChanged` handler
                // updates the spectrum + freq selector + status
                // bar when the DSP echoes the change, so we
                // don't have to mirror those widgets manually.
                state_a.dispatch_vfo_offset(saved.vfo_offset_hz);
                // Re-engage ACARS if it was on before the pass.
                // Goes after the tune so the airband lock retunes
                // from the user's just-restored freq rather than
                // racing against it. Symmetric with the AOS arm.
                if state_a.acars_was_engaged_pre_pass.replace(false) {
                    tracing::info!("auto-record LOS: re-engaging ACARS (was on pre-pass)");
                    state_a.send_dsp(UiToDsp::SetAcarsEnabled(true));
                }
                // If the user had playback off pre-AOS, we
                // started the radio at AOS to make audio flow —
                // honour that round trip and stop it now. A user
                // who explicitly turned playback off during the
                // pass (rare) loses that intent here, but the
                // expected case (set-and-forget overnight) gets
                // the right behaviour. Routed through
                // `set_playing` (header play button) so the icon,
                // `state.is_running`, and DSP all move together.
                if !saved.was_running {
                    tracing::info!("auto-record: stopping source (was stopped pre-AOS)");
                    set_playing_a(false);
                }
                // **Order is load-bearing.** Restore the audio-
                // chain settings BEFORE re-arming the scanner.
                // A scanner that wakes back up while the audio
                // chain is still in its forced-off pass state
                // would briefly run with wrong squelch / CTCSS /
                // FM-IF-NR — wasting the first dwell or two on
                // un-gated noise. Per CR round 1 on PR #557.
                //
                // Same `set_*`-fires-notify pattern the rest of
                // this handler uses (and that the AOS-side
                // `force_audio_chain_off` mirrors). Squelch
                // level is restored unconditionally (cheap,
                // idempotent on equal value); the toggles are
                // guarded against no-op writes for cleaner
                // tracing logs.
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "saved.squelch_db came from the same SpinRow we're \
                              feeding back into; round-trip preserves precision \
                              well within the row's range"
                )]
                let squelch_db_f64 = f64::from(saved.squelch_db);
                squelch_level_row_a.set_value(squelch_db_f64);
                if saved.squelch_enabled != squelch_enabled_row_a.is_active() {
                    squelch_enabled_row_a.set_active(saved.squelch_enabled);
                }
                if saved.auto_squelch_enabled != auto_squelch_row_a.is_active() {
                    auto_squelch_row_a.set_active(saved.auto_squelch_enabled);
                }
                let saved_ctcss_idx =
                    sidebar::radio_panel::RadioPanel::ctcss_index_from_mode(saved.ctcss_mode);
                if ctcss_row_a.selected() != saved_ctcss_idx {
                    ctcss_row_a.set_selected(saved_ctcss_idx);
                }
                if saved.fm_if_nr_enabled != fm_if_nr_row_a.is_active() {
                    fm_if_nr_row_a.set_active(saved.fm_if_nr_enabled);
                }
                // Restore the AF-chain widgets the AOS path
                // forced off — deemphasis (US 75 µs / EU 50 µs
                // attenuates the 2400 Hz APT subcarrier) and
                // notch (a user-set notch in the 1.5-3 kHz band
                // nulls the subcarrier). Same notify-fires-set
                // pattern as the gate restores above. Per
                // silent-fail investigation following the NOAA
                // 15 pass.
                if saved.deemphasis_idx != deemphasis_row_a.selected() {
                    deemphasis_row_a.set_selected(saved.deemphasis_idx);
                }
                if saved.notch_enabled != notch_enabled_row_a.is_active() {
                    notch_enabled_row_a.set_active(saved.notch_enabled);
                }
                // Doppler tracker — restore the user's pre-AOS
                // master-switch preference. The AOS-side
                // `force_audio_chain_off` flipped it off for the
                // pass; this brings it back so the next pass (if
                // they have Doppler on as their default) gets
                // automatic correction again.
                if saved.doppler_enabled != doppler_switch_a.is_active() {
                    doppler_switch_a.set_active(saved.doppler_enabled);
                }
                // Re-arm the scanner if it was running pre-AOS.
                // The AOS-side `tune_a` call goes through
                // `tune_to_satellite`, which fires
                // `ScannerForceDisable::trigger("satellite tune")`
                // as a manual-tune side effect — without this
                // restore, an active pre-AOS scan would be left
                // permanently off after the pass. Same idiom as
                // `was_running`: snapshot the user's pre-AOS
                // state, return them to it. `set_active(true)`
                // fires the switch's notify handler, which
                // dispatches `SetScannerEnabled(true)` to the
                // engine — same path a manual flip takes.
                if saved.scanner_running && !scanner_switch_a.is_active() {
                    tracing::info!("auto-record: re-arming scanner (was running pre-AOS)");
                    scanner_switch_a.set_active(true);
                }
            }
            RecorderAction::Toast { message, kind } => {
                if matches!(kind, ToastKind::Warn) {
                    // No dedicated warn styling on AdwToast; the
                    // message itself carries the severity. Tracing
                    // captures it for the log either way.
                    tracing::warn!("auto-record: {message}");
                }
                post_toast(&toast_overlay_weak, &message);
            }
        })
    };

    // Stash a `Weak` handle to the interpreter on AppState so
    // the `AcarsEnabledChanged(Ok(false))` arm in
    // `handle_dsp_message` can replay deferred AOS actions
    // without needing the closure plumbed through its parameter
    // list. Stored weakly to avoid an `AppState` ↔ closure
    // retain cycle (the closure captures `Rc<AppState>`
    // transitively); the strong owner is the recorder tick
    // `glib::timeout_add_local`. Issue #589 / CR round 1 on
    // PR #591.
    *state.recorder_action_interpreter.borrow_mut() = Some(Rc::downgrade(&interpret_action));

    if cache.is_some() {
        let panel_weak_tick = panel_weak.clone();
        let state_for_recorder = Rc::clone(state);
        let displayed_tick = Rc::clone(&displayed);
        let recompute_tick = Rc::clone(&recompute);
        let recorder_tick = Rc::clone(&recorder);
        let interpret_tick = Rc::clone(&interpret_action);
        let state_tick = Rc::clone(state);
        let bandwidth_row_tick = panels.radio.bandwidth_row.clone();
        let spectrum_tick = Rc::clone(spectrum_handle);
        // #510 — notify scheduler + watched-set + lead time. The
        // lead time is read fresh from config on every tick so a
        // user edit (once we expose a setting) takes effect
        // immediately without restarting the timer.
        let watched_tick = Rc::clone(&watched);
        let notify_scheduler_tick = Rc::clone(&notify_scheduler);
        let config_tick = std::sync::Arc::clone(config);
        // Scanner master switch — read for the per-tick snapshot so
        // SavedTune carries scanner state across AOS → LOS, written
        // by `interpret_action::RestoreTune` to re-arm the scanner
        // if it was running pre-AOS. Strong clone because the tick
        // already captures other panel widgets (bandwidth_row);
        // when the panel is dropped the tick's `panel_weak.upgrade`
        // returns None and we Break, dropping the chain.
        let scanner_switch_tick = panels.scanner.master_switch.clone();
        // Audio-chain widgets snapshotted into SavedTune so the
        // pre-AOS state can be restored at LOS. AOS force-disables
        // these because they're destructive to data-bearing FM
        // signals (squelch / CTCSS gate audio; FM IF NR zeros the
        // sidebands the APT subcarrier lives in). Per #555 / #556.
        let squelch_enabled_row_tick = panels.radio.squelch_enabled_row.clone();
        let auto_squelch_row_tick = panels.radio.auto_squelch_row.clone();
        let squelch_level_row_tick = panels.radio.squelch_level_row.clone();
        let ctcss_row_tick = panels.radio.ctcss_row.clone();
        let fm_if_nr_row_tick = panels.radio.fm_if_nr_row.clone();
        // AF-chain widgets that the satellite-image AOS path
        // also force-disables (deemphasis attenuates the 2400 Hz
        // APT subcarrier; a notch in the 1.5-3 kHz band nulls
        // it). Snapshotted here on the same 1-Hz tick as the
        // gate widgets so the LOS restore path can return them
        // to their pre-AOS values.
        let deemphasis_row_tick = panels.radio.deemphasis_row.clone();
        let notch_enabled_row_tick = panels.radio.notch_enabled_row.clone();
        // Doppler tracker master switch — also force-disabled at
        // AOS for the duration of an imaging-protocol pass.
        // Snapshotted here so the LOS restore path can return
        // the user's pre-AOS preference.
        let doppler_switch_tick = panels.satellites.doppler_switch.clone();
        let _ = glib::timeout_add_local(SATELLITES_COUNTDOWN_TICK, move || {
            let Some(panel) = panel_weak_tick.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let now = chrono::Utc::now();
            let mut needs_recompute = false;
            for entry in displayed_tick.borrow().iter() {
                if entry.pass.end <= now {
                    needs_recompute = true;
                    continue;
                }
                entry.row.set_title(&format_pass_title(&entry.pass, now));
            }
            // Drive the auto-record state machine. Snapshot the
            // pass list (cloned out of the displayed vec to keep
            // the borrow short) and the current tune so the
            // recorder gets a consistent view. Capture the VFO
            // offset alongside centre frequency — a user-dragged
            // carrier position needs to survive the AOS→LOS round
            // trip.
            let passes_snapshot: Vec<Pass> = displayed_tick
                .borrow()
                .iter()
                .map(|e| e.pass.clone())
                .collect();
            let auto_record_on = panel.auto_record_switch.is_active();
            // Per #533: the "also save audio" toggle is sampled
            // exclusively at AOS by the state machine; flipping
            // it mid-pass does NOT retroactively start or stop
            // recording (matches `auto_record_on`'s
            // "in-flight pass keeps running" semantics).
            let audio_record_on = panel.auto_record_audio_switch.is_active();
            // Round f64 SpinRow value to u32 at the snapshot
            // boundary so SavedTune carries a clean integer for
            // the eventual restore — no per-restore rounding.
            #[allow(
                clippy::cast_sign_loss,
                clippy::cast_possible_truncation,
                reason = "user-set bandwidth is non-negative and \
                          fits in u32 for any realistic SDR channel \
                          width; the SpinRow's own min is positive"
            )]
            let bandwidth_hz_u32 = bandwidth_row_tick.value().round() as u32;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "squelch SpinRow value is in dBFS, bounded by the row's \
                          configured min/max (well within f32 range)"
            )]
            let squelch_db_f32 = squelch_level_row_tick.value() as f32;
            let now_tune = SavedTune {
                freq_hz: state_tick.center_frequency.get(),
                vfo_offset_hz: spectrum_tick.vfo_offset_hz(),
                mode: state_tick.demod_mode.get(),
                bandwidth_hz: bandwidth_hz_u32,
                was_running: state_tick.is_running.get(),
                scanner_running: scanner_switch_tick.is_active(),
                squelch_enabled: squelch_enabled_row_tick.is_active(),
                auto_squelch_enabled: auto_squelch_row_tick.is_active(),
                squelch_db: squelch_db_f32,
                ctcss_mode: sidebar::radio_panel::RadioPanel::ctcss_mode_from_index(
                    ctcss_row_tick.selected(),
                ),
                fm_if_nr_enabled: fm_if_nr_row_tick.is_active(),
                deemphasis_idx: deemphasis_row_tick.selected(),
                notch_enabled: notch_enabled_row_tick.is_active(),
                doppler_enabled: doppler_switch_tick.is_active(),
            };
            // Read the user's selected quality tier on every
            // tick — cheap (just a ComboRow.selected() call), and
            // means a mid-pass change applies immediately to the
            // next eligible pass without a restart. Per #511.
            let min_elev_deg =
                AutoRecordQuality::from_index(panel.auto_record_quality_row.selected())
                    .min_elev_deg();
            let actions = recorder_tick.borrow_mut().tick(
                now,
                &passes_snapshot,
                auto_record_on,
                audio_record_on,
                min_elev_deg,
                now_tune,
            );
            // ACARS-disengage gate (issue #589): if any action
            // in this tick is `StartAutoRecord` AND ACARS is
            // currently engaged, stash the **whole batch** and
            // dispatch `SetAcarsEnabled(false)`. The
            // `AcarsEnabledChanged(Ok(false))` arm in
            // `handle_dsp_message` will drain the batch and
            // replay every action through `interpret_tick` once
            // the controller acks the disengage.
            //
            // Stashing the whole batch (not just
            // `StartAutoRecord`) makes the disengage ack a real
            // gate: same-tick siblings like
            // `StartAutoAudioRecord` and `ResetImagingDecoders`
            // would otherwise execute while the source was
            // still on airband geometry, capturing audio from
            // the wrong frequency until the disengage lands.
            // CR round 1 on PR #591.
            let needs_acars_gate = state_for_recorder.acars_enabled.get()
                && actions
                    .iter()
                    .any(|a| matches!(a, RecorderAction::StartAutoRecord { .. }));
            if needs_acars_gate {
                tracing::info!(
                    "auto-record AOS: gating {} action(s) on ACARS disengage ack",
                    actions.len()
                );
                state_for_recorder.acars_was_engaged_pre_pass.set(true);
                *state_for_recorder.pending_aos_actions.borrow_mut() = Some(actions);
                state_for_recorder.send_dsp(UiToDsp::SetAcarsEnabled(false));
            } else {
                for action in actions {
                    interpret_tick(action);
                }
            }

            // #510 — pre-pass desktop alerts. Walk the displayed
            // pass list, map each to (norad_id, &Pass), feed the
            // scheduler. Pure function in / pure actions out;
            // notification I/O happens in the action loop below.
            let lead_min = load_notify_lead_min(&config_tick);
            let lead = chrono::Duration::minutes(i64::from(lead_min));
            let watched_snapshot = watched_tick.borrow().clone();
            let notify_actions = {
                let displayed_borrow = displayed_tick.borrow();
                let pairs: Vec<(u32, &Pass)> = displayed_borrow
                    .iter()
                    .filter_map(|e| norad_id_for_pass(&e.pass).map(|id| (id, &e.pass)))
                    .collect();
                notify_scheduler_tick
                    .borrow_mut()
                    .tick(now, lead, lead_min, pairs, |id| {
                        watched_snapshot.contains(&id)
                    })
            };
            for action in notify_actions {
                match action {
                    NotifyAction::Fire {
                        norad_id,
                        pass,
                        lead_min,
                    } => {
                        crate::notify::send_pass_alert(&pass, norad_id, lead_min);
                    }
                }
            }

            if needs_recompute {
                recompute_tick();
            }
            glib::ControlFlow::Continue
        });
    }
}

/// Cadence of the Doppler tracker's trigger re-evaluation —
/// 1 Hz. Spec §2's overhead-and-frequency-match test only
/// needs to flip on horizon crossing / dial change, which is
/// always slower than 1 s. Cheap: one SGP4 propagate per
/// catalog entry within the ±20 kHz window — typically zero
/// or one sat at a time.
pub(super) const DOPPLER_TRIGGER_TICK: Duration = Duration::from_secs(1);

/// Cadence of the Doppler tracker's offset recompute — 4 Hz
/// (250 ms). Per spec §3, fast enough that the residual
/// frequency error between updates stays inside the channel
/// filter, slow enough that the bus + status-bar updates
/// don't hammer GTK.
pub(super) const DOPPLER_RECOMPUTE_TICK: Duration = Duration::from_millis(250);

/// Minimum |Δoffset| (Hz) before re-dispatching `SetVfoOffset`
/// from the 4 Hz recompute tick. Sub-5-Hz changes are below
/// the channel filter's pass-band granularity for any LEO
/// imaging downlink we care about, so suppressing them is
/// pure bus-traffic relief.
pub(super) const DOPPLER_DISPATCH_THRESHOLD_HZ: f64 = 5.0;

/// Restore the persisted Doppler master-switch state to the
/// widget and wire change-notify to save back. Always called,
/// regardless of TLE-cache availability — the user's preference
/// must survive a launch where the cache happened to be
/// unavailable. The behavioral wiring (timers + tracker) lives
/// in [`connect_doppler_tracker`] and is gated separately.
/// Per CR round 1 on PR #554.
pub(super) fn restore_doppler_switch(
    panels: &SidebarPanels,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    let persisted = sidebar::satellites_panel::load_doppler_tracking_enabled(config);
    panels.satellites.doppler_switch.set_active(persisted);

    let config = std::sync::Arc::clone(config);
    panels
        .satellites
        .doppler_switch
        .connect_active_notify(move |row| {
            sidebar::satellites_panel::save_doppler_tracking_enabled(&config, row.is_active());
        });
}

/// Wire the [`DopplerTracker`](crate::doppler_tracker::DopplerTracker):
/// 1 Hz trigger re-evaluation tick, 4 Hz offset-recompute
/// tick, status-bar update, [`UiToDsp::SetVfoOffset`] dispatch
/// (rate-limited to changes >`DOPPLER_DISPATCH_THRESHOLD_HZ`).
/// Per #521 and the design spec at
/// `docs/superpowers/specs/2026-04-26-doppler-correction-design.md`.
///
/// Master-switch persistence + initial restore happens in
/// [`restore_doppler_switch`], which is called unconditionally
/// from [`connect_satellites_panel`]. This function adds a
/// **second** change-notify handler on the same widget that
/// drives the tracker model — multiple GTK signal handlers on
/// one widget fire independently, no conflict. Wired only when
/// the TLE cache is available; without TLEs the trigger
/// re-evaluate has no candidate to engage.
#[allow(
    clippy::too_many_lines,
    reason = "three chained closures (master-switch handler + two timers) all \
              live in one function so they share the `tracker` and \
              `last_dispatched` Rcs by direct clone; splitting would mean \
              hoisting those onto AppState, which the design spec §4 \
              already explicitly defers"
)]
pub(super) fn connect_doppler_tracker(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    cache: &std::sync::Arc<sdr_sat::TleCache>,
    status_bar: &Rc<StatusBar>,
) {
    use crate::doppler_tracker::DopplerTracker;

    // Read the widget's current state — it was already restored
    // (and a persistence handler wired) by `restore_doppler_switch`,
    // which runs unconditionally before we enter this cache-gated
    // path. Per CR round 1 on PR #554.
    let initial = panels.satellites.doppler_switch.is_active();

    let tracker: Rc<RefCell<DopplerTracker>> = Rc::new(RefCell::new(DopplerTracker::new(initial)));

    // The dispatch baseline lives on `AppState` as
    // `last_dispatched_vfo_offset_hz` — written by the
    // `connect_vfo_offset_changed` callback, which fires from
    // BOTH the DSP echo (`DspToUi::VfoOffsetChanged`) and direct
    // user-drag dispatches. The tracker reads from there for
    // its rate-limit gate, so external writes (auto-record AOS
    // reset, spectrum drag) keep the baseline in sync — no
    // stale local value to worry about. Per CR round 7 on PR
    // #554. The fallback paths below also write the baseline
    // directly when they dispatch a `SetVfoOffset(user_ref)`
    // flush, so re-engagement within `DOPPLER_DISPATCH_THRESHOLD_HZ`
    // of the prior live value isn't suppressed.

    // Master-switch handler that drives the tracker. (A separate
    // change-notify handler in `restore_doppler_switch` already
    // persists the value — multiple GTK signal handlers fire
    // independently, no conflict.) On disable, `set_master_enabled`
    // atomically clears `active`, captures and resets
    // `user_reference_offset_hz`, and returns the captured value
    // for us to flush to DSP.
    wire_doppler_master_switch(panels, &tracker, state, status_bar);

    // 1 Hz trigger re-evaluation tick: rebuild the candidate
    // list from catalog × frequency match × ground station ×
    // cached TLEs, run `pick_active_satellite`, and call
    // `set_active` on the tracker. On a transition to None
    // (e.g. user retunes off the satellite, or the satellite
    // sets), dispatch a final SetVfoOffset(user_reference) and
    // clear the status bar — same teardown the master-switch
    // handler does for the off-while-active case.
    {
        let tracker = Rc::clone(&tracker);
        let cache = std::sync::Arc::clone(cache);
        let state = Rc::clone(state);
        let status_bar = Rc::clone(status_bar);
        let panel_weak = panels.satellites.downgrade();
        let _ = glib::timeout_add_local(DOPPLER_TRIGGER_TICK, move || {
            let Some(panel) = panel_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            doppler_trigger_tick(&panel, &tracker, &cache, &state, &status_bar)
        });
    }

    // 4 Hz offset-recompute tick: while a satellite is active,
    // recompute the Doppler shift and dispatch a SetVfoOffset
    // (rate-limited to changes >DOPPLER_DISPATCH_THRESHOLD_HZ
    // to avoid spamming the bus). Update the status-bar label
    // every tick — the kHz/0.1 rounded format already hides
    // sub-100-Hz wobble, no further suppression needed.
    {
        let tracker = Rc::clone(&tracker);
        let cache = std::sync::Arc::clone(cache);
        let state = Rc::clone(state);
        let status_bar = Rc::clone(status_bar);
        let panel_weak = panels.satellites.downgrade();
        let _ = glib::timeout_add_local(DOPPLER_RECOMPUTE_TICK, move || {
            let Some(panel) = panel_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            doppler_recompute_tick(&panel, &tracker, &cache, &state, &status_bar)
        });
    }
}

/// Master-switch handler for the Doppler tracker: flips the tracker's
/// enabled state and, when a satellite was actively tracked, restores
/// the user's reference offset + clears the status-bar badge. Split
/// out per the 50-NLOC gate (#817).
fn wire_doppler_master_switch(
    panels: &SidebarPanels,
    tracker: &Rc<RefCell<crate::doppler_tracker::DopplerTracker>>,
    state: &Rc<AppState>,
    status_bar: &Rc<StatusBar>,
) {
    let tracker = Rc::clone(&tracker);
    let state = Rc::clone(state);
    let status_bar = Rc::clone(status_bar);
    panels
        .satellites
        .doppler_switch
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            let mut t = tracker.borrow_mut();
            let was_active = t.active().is_some();
            let final_offset = t.set_master_enabled(enabled);
            drop(t);
            // Only dispatch the fallback `SetVfoOffset` when
            // a satellite was actually being tracked. Without
            // this guard, toggling Doppler off while no
            // satellite is engaged would still send
            // `SetVfoOffset(0.0)` and clobber any non-zero
            // VFO offset the user had set independently. Per
            // CR round 3 on PR #554.
            if was_active && let Some(offset) = final_offset {
                state.dispatch_vfo_offset(offset);
                status_bar.update_doppler(None);
            }
        });
}

/// One recompute-tick of the Doppler tracker: re-propagate the active
/// satellite's position and retune the VFO offset when the predicted
/// Doppler shift moved past the dispatch threshold. Split out per the
/// 50-NLOC gate (#817).
fn doppler_recompute_tick(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    tracker: &Rc<RefCell<crate::doppler_tracker::DopplerTracker>>,
    cache: &std::sync::Arc<sdr_sat::TleCache>,
    state: &Rc<AppState>,
    status_bar: &Rc<StatusBar>,
) -> glib::ControlFlow {
    use crate::doppler_tracker::{FREQ_MATCH_TOLERANCE_HZ, should_tick};

    // Lifecycle gate: master + running. The status-bar
    // badge clears on the first not-running tick so the
    // user gets immediate "Doppler is idle" feedback when
    // they press Stop — `update_doppler(None)` is
    // idempotent (set_visible(false) on an already-hidden
    // label is a no-op). Per #567.
    if !should_tick(tracker.borrow().master_enabled(), state.is_running.get()) {
        status_bar.update_doppler(None);
        return glib::ControlFlow::Continue;
    }
    let active_sat = tracker.borrow().active();
    let Some(sat) = active_sat else {
        return glib::ControlFlow::Continue;
    };
    // Has the user retuned away from the active satellite?
    // If so, disengage NOW rather than wait up to 1 s for
    // the trigger tick — otherwise stale Doppler keeps
    // dispatching against the new center frequency for up
    // to a full second. Per CR round 5 on PR #554.
    #[allow(
        clippy::cast_precision_loss,
        reason = "catalog downlinks sit in the 100s of MHz, well \
                  below f64's 2^53 mantissa ceiling"
    )]
    let downlink = sat.downlink_hz as f64;
    let current_freq = state.center_frequency.get();
    if (downlink - current_freq).abs() > FREQ_MATCH_TOLERANCE_HZ {
        abandon_doppler_tracking(tracker, state, status_bar);
        return glib::ControlFlow::Continue;
    }
    let Some(doppler) = predicted_doppler_hz(panel, cache, sat) else {
        return glib::ControlFlow::Continue;
    };
    let live = tracker.borrow().live_offset_hz(doppler);
    // Status bar updates every tick — the kHz/0.1
    // format hides sub-100-Hz jitter naturally.
    status_bar.update_doppler(Some(doppler));
    // SetVfoOffset is rate-limited to material changes.
    // Baseline lives on `AppState` and is kept in sync by
    // the `connect_vfo_offset_changed` callback (fires on
    // both DSP echo and direct user-drag dispatches). Per
    // CR round 7 on PR #554. We also write it eagerly at
    // dispatch so a fast back-to-back tick before the
    // echo round-trip doesn't over-dispatch — the echo
    // arrives later with the same value, harmless.
    let baseline = state.last_dispatched_vfo_offset_hz.get();
    if (live - baseline).abs() > DOPPLER_DISPATCH_THRESHOLD_HZ {
        state.dispatch_vfo_offset(live);
    }
    glib::ControlFlow::Continue
}

/// Drop the active satellite and restore the user's pre-tracking VFO
/// offset + a cleared status-bar badge (retune-away and off-while-
/// active both land here).
fn abandon_doppler_tracking(
    tracker: &Rc<RefCell<crate::doppler_tracker::DopplerTracker>>,
    state: &Rc<AppState>,
    status_bar: &Rc<StatusBar>,
) {
    let mut t = tracker.borrow_mut();
    let prior_user_ref = t.user_reference_offset_hz();
    let _ = t.set_active(None);
    drop(t);
    state.dispatch_vfo_offset(prior_user_ref);
    status_bar.update_doppler(None);
}

/// Propagate the active satellite and return its predicted Doppler
/// shift, or `None` when the TLE was evicted between trigger
/// evaluation and recompute (the next 1 Hz trigger tick drops the
/// active sat since `cached_tle_for` fails there too) or SGP4
/// propagation fails.
fn predicted_doppler_hz(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    cache: &std::sync::Arc<sdr_sat::TleCache>,
    sat: &sdr_sat::KnownSatellite,
) -> Option<f64> {
    use crate::doppler_tracker::compute_doppler_offset_hz;
    use sdr_sat::{GroundStation, Satellite};

    let station = GroundStation::new(
        panel.lat_row.value(),
        panel.lon_row.value(),
        panel.alt_row.value(),
    );
    let (line1, line2) = cache.cached_tle_for(sat.norad_id).ok()?;
    let parsed = Satellite::from_tle(sat.name, &line1, &line2).ok()?;
    let now = chrono::Utc::now();
    #[allow(
        clippy::cast_precision_loss,
        reason = "catalog downlinks sit in the 100s of MHz, well \
                  below f64's 2^53 mantissa ceiling"
    )]
    let carrier = sat.downlink_hz as f64;
    compute_doppler_offset_hz(&parsed, &station, now, carrier)
        .map_err(|_| {
            tracing::debug!(
                satellite = sat.name,
                "Doppler recompute: SGP4 propagate failed; skipping tick"
            );
        })
        .ok()
}

/// One 1 Hz trigger-tick of the Doppler tracker: rebuild the
/// overhead-candidate list from fresh TLE propagation, elect the
/// active satellite, and apply the activation / deactivation edge to
/// the VFO + status bar. Split out per the 50-NLOC gate (#817).
fn doppler_trigger_tick(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    tracker: &Rc<RefCell<crate::doppler_tracker::DopplerTracker>>,
    cache: &std::sync::Arc<sdr_sat::TleCache>,
    state: &Rc<AppState>,
    status_bar: &Rc<StatusBar>,
) -> glib::ControlFlow {
    use crate::doppler_tracker::{
        Candidate, FREQ_MATCH_TOLERANCE_HZ, pick_active_satellite, should_tick,
    };
    use sdr_sat::{GroundStation, KNOWN_SATELLITES, Satellite, track};

    let mut t = tracker.borrow_mut();
    // Lifecycle gate: master + running. While stopped,
    // no candidate rebuild + no `set_active` transition,
    // so a satellite setting below the horizon mid-stop
    // doesn't fire a spurious disengage dispatch into a
    // stopped DSP. On resume, this tick re-evaluates and
    // engages / disengages naturally against the live
    // geometry. Per #567.
    if !should_tick(t.master_enabled(), state.is_running.get()) {
        return glib::ControlFlow::Continue;
    }
    // Build the ground station from the live panel
    // values — the user can edit lat/lon/alt mid-pass
    // and the tracker should follow.
    let station = GroundStation::new(
        panel.lat_row.value(),
        panel.lon_row.value(),
        panel.alt_row.value(),
    );
    let now = chrono::Utc::now();
    let current_freq = state.center_frequency.get();

    // Build the candidate list: every catalog entry
    // whose downlink is within ±FREQ_MATCH_TOLERANCE_HZ
    // of the radio's current centre frequency, paired
    // with its currently-evaluated elevation. Iterate
    // in `KNOWN_SATELLITES` order so the spec §2
    // tie-break (earlier entry wins) is deterministic.
    let mut candidates: Vec<Candidate> = Vec::new();
    for sat in KNOWN_SATELLITES {
        #[allow(
            clippy::cast_precision_loss,
            reason = "catalog downlinks sit in the 100s of MHz, well \
                      below f64's 2^53 mantissa ceiling"
        )]
        let downlink = sat.downlink_hz as f64;
        if (downlink - current_freq).abs() > FREQ_MATCH_TOLERANCE_HZ {
            continue;
        }
        let Ok((line1, line2)) = cache.cached_tle_for(sat.norad_id) else {
            continue;
        };
        let Ok(parsed) = Satellite::from_tle(sat.name, &line1, &line2) else {
            continue;
        };
        let Ok(track) = track(&station, &parsed, now) else {
            continue;
        };
        candidates.push(Candidate {
            satellite: sat,
            elevation_deg: track.elevation_deg,
        });
    }

    let new_active = pick_active_satellite(t.master_enabled(), &candidates);
    // Capture pre-`set_active` state so we can:
    //   1. Flush back to the prior user reference on a
    //      Some → None disengage (`set_active` resets
    //      `user_reference_offset_hz` to 0 on any change,
    //      so reading it AFTER would always give 0).
    //   2. Decide whether this is a fresh engagement
    //      (None → Some) vs. a satellite swap
    //      (Some(A) → Some(B)) — only the former should
    //      seed `user_reference_offset_hz` from the live
    //      spectrum offset. On a swap, the live offset
    //      is `prior_user_ref + prior_doppler`; reseeding
    //      with that would copy the previous pass's
    //      Doppler into the new pass's baseline (a
    //      double-count). Per CR round 4 on PR #554.
    let prior_user_ref = t.user_reference_offset_hz();
    let prior_active_some = t.active().is_some();
    let changed = t.set_active(new_active);
    if changed {
        if new_active.is_some() {
            if prior_active_some {
                // Some(A) → Some(B) satellite swap.
                // Restore the pre-swap user_reference
                // (which `set_active` just reset to 0)
                // so it survives the satellite change.
                // Per CR round 5 on PR #554.
                t.set_user_reference_offset_hz(prior_user_ref);
            } else {
                // None → Some fresh engagement. Seed
                // `user_reference_offset_hz` from the
                // synchronously-tracked DSP baseline on
                // `AppState` so this pass's Doppler tracks
                // ON TOP of any offset the user had set
                // before AOS — and so disengage at LOS
                // restores that exact value via the
                // Some → None flush path.
                //
                // Round 6 deferred this seed because the
                // only available source was `spectrum.vfo_offset_hz()`,
                // which lags DSP echoes — auto-record's
                // AOS-side `SetVfoOffset(0.0)` would not yet
                // be reflected when the trigger tick fired,
                // so we'd capture the stale pre-AOS value.
                // Round 7 added `state.last_dispatched_vfo_offset_hz`,
                // which the `connect_vfo_offset_changed`
                // callback updates on every DSP echo (and
                // every direct user-drag dispatch). That
                // gives us the synchronously-tracked source
                // of truth the deferral was waiting for.
                // Per CR round 9 on PR #554.
                let baseline = state.last_dispatched_vfo_offset_hz.get();
                t.set_user_reference_offset_hz(baseline);
            }
            // No dispatch here — the next 4 Hz tick will
            // dispatch `live = user_reference + doppler`.
        } else {
            // Disengaged — flush the live offset back to
            // the pre-engage user reference (captured
            // before `set_active` reset it) and clear
            // the status badge.
            //
            // We don't need to explicitly clear the
            // tracker's `user_reference_offset_hz` here
            // — `set_active(None)` already did it on
            // line 216 of `doppler_tracker.rs` (the
            // `if changed { self.user_reference_offset_hz = 0.0; }`
            // branch), and the
            // `satellite_to_none_resets_user_reference_offset`
            // unit test pins that invariant. The
            // `prior_user_ref` we dispatch is the value
            // captured pre-`set_active`, so DSP gets the
            // user's pre-engage baseline; the tracker's
            // own field is already 0 for the next
            // engagement. Per CR round 8 on PR #554.
            drop(t);
            state.dispatch_vfo_offset(prior_user_ref);
            status_bar.update_doppler(None);
        }
    }
    glib::ControlFlow::Continue
}
