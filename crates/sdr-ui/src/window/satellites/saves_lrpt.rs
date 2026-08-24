//! LOS save path for a Meteor-M LRPT pass: per-APID PNGs plus
//! optional false-colour composites into the pass directory, off the
//! GTK main loop. Split out of `window/satellites.rs` per the Codacy
//! 500-NLOC file gate on PR #844.

use gtk4::prelude::*;

use super::super::{Rc, gio, glib};
use super::post_toast;
use super::recorder::RecorderDeps;

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
pub(in crate::window) fn on_save_lrpt_pass(deps: &RecorderDeps, dir: std::path::PathBuf) {
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
