//! LOS save paths for APT and SSTV passes: async PNG export with
//! snapshot-pinned viewer close (APT), and the per-pass SSTV batch
//! writer with panic recovery and late-frame retry queueing. Split
//! out of `window/satellites.rs` per the Codacy 500-NLOC file gate on
//! PR #844.

use gtk4::prelude::*;

use super::super::{AppState, PendingSstvExport, Rc, adw, gio, glib};
use super::post_toast;
use super::recorder::RecorderDeps;

/// Outcome of `save_sstv_batches` reported back to the GTK main
/// thread. Used by the `RecorderAction::SaveSstvPass` arm. Per CR
/// round 6 #21 on PR #599.
pub(in crate::window) struct SstvSaveOutcome {
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
pub(in crate::window) fn save_sstv_batches(
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
pub(in crate::window) fn save_sstv_batch(
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

/// LOS save for an APT pass: rotate per orbit leg, encode the PNG on
/// a blocking thread, and toast the outcome.
/// Split out of [`interpret_recorder_action`] per the 50-NLOC gate
/// (#817).
#[allow(clippy::too_many_arguments)]
pub(in crate::window) fn on_save_apt_png(deps: &RecorderDeps, path: std::path::PathBuf) {
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
    /// window; cloning the `WeakRef` pins the identity while staying
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
pub(in crate::window) fn on_save_sstv_pass(deps: &RecorderDeps, dir: std::path::PathBuf) {
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
pub(in crate::window) fn compute_apt_rotate_180_for_pass(
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
