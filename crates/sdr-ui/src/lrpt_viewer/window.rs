//! Window chrome + action wiring for the LRPT viewer (issue
//! #819): the non-modal viewer window with its channel/composite
//! dropdown, Pause and Export PNG header buttons, the
//! `app.lrpt-open` action (`Ctrl+Shift+L`), and the
//! open-if-needed flow shared with the auto-record path. Split
//! out of `lrpt_viewer.rs` per the file-size pass.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use sdr_lrpt::image::IMAGE_WIDTH;
use sdr_radio::lrpt_image::LrptImage;

use super::composite::{COMPOSITE_CATALOG, CompositeRecipe};
use super::export::{
    ExportSnapshot, composite_export_path, default_export_path, write_greyscale_png, write_rgb_png,
};
use super::view::LrptImageView;
use super::{DROPDOWN_REFRESH_INTERVAL_MS, VIEWER_WINDOW_HEIGHT, VIEWER_WINDOW_WIDTH};
use crate::messages::UiToDsp;
use crate::viewer::{plain_toast, show_toast_in};

// ─── Non-modal viewer window ───────────────────────────────────────────

/// One row in the dropdown — either a single APID, or a
/// composite recipe. Pulled out as a tagged enum (rather than
/// the previous parallel `Vec<u16>`) so the
/// `connect_selected_notify` handler can dispatch straight off
/// the index without index-arithmetic against a "where do
/// composites start" boundary that drifted any time the APID
/// list changed. Per #547.
#[derive(Clone, Copy, Debug)]
enum DropdownEntry {
    Apid(u16),
    Composite(CompositeRecipe),
}

/// Build the dynamic channel-picker dropdown for the viewer
/// header. APIDs aren't known at open time, so the dropdown
/// starts dimmed-but-visible and a 1 Hz `glib` timer rebuilds
/// its model whenever new APIDs appear in `view`. A parallel
/// `Vec<DropdownEntry>` lets us decode the dropdown's numeric
/// `selected` index back into either an APID or a composite
/// recipe without parsing the display string.
///
/// The model is laid out as: per-APID entries first (sorted),
/// then every recipe in [`COMPOSITE_CATALOG`] in catalog order.
/// Composite rows are listed unconditionally even when the
/// underlying APIDs aren't all present yet — picking one in
/// that state shows a black canvas with a debug log, and the
/// dropdown's drain tick re-issues `set_composite` on every
/// poll so the image populates the moment the missing channel
/// arrives. Per #547.
fn build_channel_dropdown(view: &LrptImageView) -> gtk4::DropDown {
    // Seed with the static composite catalog so users can pick
    // a composite from the moment the viewer opens — no waiting
    // for the first 1 Hz refresh tick. APIDs prepend to this
    // list as they're decoded (the tick rebuilds the model
    // sorted-APIDs-first, then composites). Per CR round 6 on
    // PR #575: previously the dropdown started empty + dimmed
    // until the tick fired, which made the new composite rows
    // invisible for up to a second every viewer open.
    let seed_entries: Vec<DropdownEntry> = COMPOSITE_CATALOG
        .iter()
        .copied()
        .map(DropdownEntry::Composite)
        .collect();
    let seed_labels: Vec<String> = seed_entries.iter().map(|e| entry_label(*e)).collect();
    let seed_label_refs: Vec<&str> = seed_labels.iter().map(String::as_str).collect();
    let model = gtk4::StringList::new(&seed_label_refs);
    let dropdown = gtk4::DropDown::builder()
        .model(&model)
        .tooltip_text("Which AVHRR channel (APID) or composite to display")
        // Seeded with composites — user can pick before any
        // APID arrives. Per CR round 6 on PR #575.
        .sensitive(true)
        .build();
    dropdown.update_property(&[gtk4::accessible::Property::Label("LRPT channel selector")]);
    // GTK4 `DropDown` defaults `selected` to `0` for a
    // non-empty model, which would silently activate composite
    // #0 the moment the dropdown is built (the
    // `selected_notify` handler can't distinguish "user picked"
    // from "GTK auto-selected at construction"). Pin it to
    // `INVALID_LIST_POSITION` so the renderer's auto-select
    // path (`push_line` on first APID line received) drives
    // the initial selection instead — matching the pre-CR-6
    // behavior where opening the viewer didn't activate
    // anything until data flowed. Per CR round 6 on PR #575.
    dropdown.set_selected(gtk4::INVALID_LIST_POSITION);
    let dropdown_entries: Rc<RefCell<Vec<DropdownEntry>>> = Rc::new(RefCell::new(seed_entries));

    // Selection → renderer. Per-APID picks route to
    // `set_active_apid` and clear any active composite so the
    // single-channel canvas paints; composite picks call
    // `set_composite`, which builds the cached ARGB32 surface
    // from the named source APIDs. Per #547.
    {
        let view = view.clone();
        let dropdown_entries = Rc::clone(&dropdown_entries);
        dropdown.connect_selected_notify(move |dd| {
            let idx = dd.selected() as usize;
            let entries = dropdown_entries.borrow();
            let Some(&entry) = entries.get(idx) else {
                return;
            };
            // Drop the borrow before any view mutation that
            // might re-enter the dropdown handler (e.g. via
            // a future `set_selected` call inside the view).
            drop(entries);
            match entry {
                DropdownEntry::Apid(apid) => {
                    view.clear_composite();
                    let _ = view.set_active_apid(apid);
                }
                DropdownEntry::Composite(recipe) => {
                    let _ = view.set_composite(recipe);
                }
            }
        });
    }

    // Refresh tick — runs at 1 Hz (channel discovery is rare;
    // a faster cadence would burn CPU on idle string compares).
    // Register the source on the view so `LrptImageView::shutdown`
    // can cancel it when the window closes; otherwise the closure's
    // `view.clone()` would keep the view + ~51 MB-per-channel
    // surfaces alive forever. The tick's three jobs and its
    // RefCell borrow-scoping invariants are documented on
    // [`dropdown_refresh_tick`] (split out per the 50-NLOC gate,
    // #819).
    let view_for_tick = view.clone();
    let dropdown_clone = dropdown.clone();
    let refresh_id = glib::timeout_add_local(
        std::time::Duration::from_millis(u64::from(DROPDOWN_REFRESH_INTERVAL_MS)),
        move || dropdown_refresh_tick(&view_for_tick, &dropdown_clone, &model, &dropdown_entries),
    );
    view.register_source(refresh_id);

    dropdown
}

/// Display label for one dropdown entry. Shared between the seed
/// list and the refresh tick's model rebuild so the two can't
/// drift. Split out per the 50-NLOC gate (#819).
fn entry_label(entry: DropdownEntry) -> String {
    match entry {
        DropdownEntry::Apid(apid) => format!("APID {apid}"),
        DropdownEntry::Composite(recipe) => format!("Composite — {}", recipe.name),
    }
}

/// One firing of the dropdown's 1 Hz refresh tick. The tick has
/// three jobs (per #547):
///   1. Rebuild the entries list when the APID set changes
///      (composite rows are always appended; per-APID rows
///      are sorted).
///   2. Re-sync the dropdown's `selected` to whichever APID
///      the renderer thinks is active (or the first APID if
///      the renderer has no selection yet).
///   3. When composite mode is active, re-issue
///      `view.set_composite(recipe)` so newly-decoded lines
///      from the source APIDs land in the cached composite
///      surface at the same ~1 Hz cadence.
///
/// **Borrow scoping:** GTK4's `gtk4::DropDown::set_selected`
/// emits `notify::selected` SYNCHRONOUSLY inside the setter,
/// which means the `connect_selected_notify` handler re-enters
/// the same `dropdown_entries` `RefCell` to look up the entry for
/// the new index. If any helper held a `borrow_mut()` across
/// `set_selected(...)`, that re-entrance would panic with
/// "already borrowed". Per `CodeRabbit` round 3 on PR #543. The
/// borrows are kept tight: an immutable `borrow()` for the
/// equality compare, a fresh `borrow_mut()` for the
/// `clone_from`, and zero borrows held during the
/// `set_selected` calls (in [`sync_dropdown_selection`]).
/// Split out of the tick closure per the 50-NLOC gate (#819).
fn dropdown_refresh_tick(
    view: &LrptImageView,
    dropdown: &gtk4::DropDown,
    model: &gtk4::StringList,
    dropdown_entries: &Rc<RefCell<Vec<DropdownEntry>>>,
) -> glib::ControlFlow {
    let mut current_apids = view.known_apids();
    current_apids.sort_unstable();
    let desired = desired_dropdown_entries(&current_apids);

    let entries_unchanged = {
        let cur = dropdown_entries.borrow();
        entries_match(&cur, &desired)
    };

    maybe_rebuild_composite(view);

    // If the entries match AND the dropdown's selected entry
    // still aligns with the renderer's active channel, there's
    // nothing else to do this tick.
    let active_apid = view.active_apid();
    let active_composite = view.active_composite();
    #[allow(clippy::cast_possible_truncation)]
    let selected_entry = {
        let entries = dropdown_entries.borrow();
        entries.get(dropdown.selected() as usize).copied()
    };
    let selected_aligned = match (selected_entry, active_composite, active_apid) {
        (Some(DropdownEntry::Composite(s)), Some(a), _) => s == a,
        (Some(DropdownEntry::Apid(s)), None, Some(a)) => s == a,
        _ => false,
    };
    if entries_unchanged && selected_aligned {
        return glib::ControlFlow::Continue;
    }

    if !entries_unchanged {
        model.splice(0, model.n_items(), &[]);
        for entry in &desired {
            model.append(&entry_label(*entry));
        }
        dropdown_entries.borrow_mut().clone_from(&desired);
    }
    // Always sensitive — composite catalog entries are present
    // even before any APID arrives. Picking one pre-decode logs
    // and falls through to the background-painted canvas; the
    // next refresh tick rebuilds once data shows up. Per #547.
    dropdown.set_sensitive(!desired.is_empty());

    sync_dropdown_selection(
        dropdown,
        &desired,
        active_composite,
        active_apid,
        &current_apids,
    );
    glib::ControlFlow::Continue
}

/// Element-wise equality between the dropdown's current entries
/// and the tick's freshly-built desired list. Split out of
/// [`dropdown_refresh_tick`] per the 50-NLOC gate (#819).
fn entries_match(cur: &[DropdownEntry], desired: &[DropdownEntry]) -> bool {
    cur.len() == desired.len()
        && cur.iter().zip(desired.iter()).all(|(a, b)| match (a, b) {
            (DropdownEntry::Apid(x), DropdownEntry::Apid(y)) => x == y,
            (DropdownEntry::Composite(x), DropdownEntry::Composite(y)) => x == y,
            _ => false,
        })
}

/// The desired full entries list for the dropdown model: per-APID
/// entries first (sorted by the caller), then every catalog
/// composite in catalog order. Split out of
/// [`dropdown_refresh_tick`] per the 50-NLOC gate (#819).
fn desired_dropdown_entries(current_apids: &[u16]) -> Vec<DropdownEntry> {
    let mut desired: Vec<DropdownEntry> = current_apids
        .iter()
        .copied()
        .map(DropdownEntry::Apid)
        .collect();
    desired.extend(
        COMPOSITE_CATALOG
            .iter()
            .copied()
            .map(DropdownEntry::Composite),
    );
    desired
}

/// Composite-cache rebuild gate for the refresh tick. Rebuild
/// when composite mode is active so newly-decoded lines accrue in
/// near-real-time — but skip while the viewer is paused
/// (`set_composite` always queues a redraw, and the pause
/// contract says the canvas freezes until Resume; the next
/// non-paused tick catches up — per #547 + CR round 1 on PR
/// #575), and skip when the source channels' min height hasn't
/// advanced since the last build (LOS reached, decoder stalled,
/// only a non-limiting channel grew — the composite truncates to
/// `min(r, g, b)`, so rebuilding then is pure waste; per CR
/// round 3 on PR #575). Split out per the 50-NLOC gate (#819).
fn maybe_rebuild_composite(view: &LrptImageView) {
    if let Some(recipe) = view.active_composite()
        && !view.is_paused()
    {
        let current = view.current_composite_min_height(recipe);
        let cached = view.cached_composite_min_height();
        if current != cached {
            let _ = view.set_composite(recipe);
        }
    }
}

/// Sync the dropdown's selected index to the renderer's active
/// state. Composite mode wins over per-APID active; with no
/// active selection at all, the first (sorted) APID is picked so
/// the user sees something the moment data arrives — the
/// `selected_notify` handler routes that choice into the
/// renderer. No `dropdown_entries` borrow is held here (the
/// `set_selected` calls re-enter the selection handler
/// synchronously — see [`dropdown_refresh_tick`]'s borrow-scoping
/// note). Split out per the 50-NLOC gate (#819).
fn sync_dropdown_selection(
    dropdown: &gtk4::DropDown,
    desired: &[DropdownEntry],
    active_composite: Option<CompositeRecipe>,
    active_apid: Option<u16>,
    current_apids: &[u16],
) {
    if let Some(recipe) = active_composite {
        if let Some(pos) = desired.iter().position(|e| match e {
            DropdownEntry::Composite(r) => *r == recipe,
            DropdownEntry::Apid(_) => false,
        }) {
            #[allow(clippy::cast_possible_truncation)]
            dropdown.set_selected(pos as u32);
        }
    } else if let Some(active) = active_apid {
        if let Some(pos) = desired.iter().position(|e| match e {
            DropdownEntry::Apid(a) => *a == active,
            DropdownEntry::Composite(_) => false,
        }) {
            #[allow(clippy::cast_possible_truncation)]
            dropdown.set_selected(pos as u32);
        }
    } else if !current_apids.is_empty() {
        dropdown.set_selected(0);
    }
}

/// Build the Pause / Resume toggle for the viewer header.
/// Pull-out so [`open_lrpt_viewer_window`] stays under the
/// 100-line clippy threshold.
fn build_pause_button(view: &LrptImageView) -> gtk4::ToggleButton {
    let btn = gtk4::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text("Pause / resume the live image update")
        .build();
    btn.update_property(&[gtk4::accessible::Property::Label(
        "Pause or resume live image update",
    )]);
    let view = view.clone();
    btn.connect_toggled(move |b| {
        view.set_paused(b.is_active());
    });
    btn
}

/// Open the LRPT viewer in a non-modal transient window. The
/// window holds a header bar with a channel dropdown,
/// Pause / Resume, and Export PNG, plus the drawing-area
/// canvas underneath.
///
/// Non-modal so the user can keep tuning, recording, or
/// otherwise interacting with the main radio window while the
/// LRPT image builds up alongside.
pub fn open_lrpt_viewer_window<W: gtk4::prelude::IsA<gtk4::Window>>(
    parent: &W,
    title: &str,
    image: LrptImage,
) -> (LrptImageView, adw::Window) {
    let view = LrptImageView::new(image);

    let window = adw::Window::builder()
        .title(title)
        .default_width(VIEWER_WINDOW_WIDTH)
        .default_height(VIEWER_WINDOW_HEIGHT)
        .transient_for(parent)
        .modal(false)
        .build();
    // Inherit the parent's GApplication so Wayland's
    // `xdg_toplevel_set_app_id` carries `com.sdr.rs` and the
    // WM can resolve our icon. See apt_viewer.rs for the full
    // rationale.
    window.set_application(parent.application().as_ref());

    let header = adw::HeaderBar::new();

    let channel_dropdown = build_channel_dropdown(&view);
    header.pack_start(&channel_dropdown);

    let pause_btn = build_pause_button(&view);
    header.pack_start(&pause_btn);

    let export_btn = build_export_button(&view, &window);
    header.pack_end(&export_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(view.drawing_area()));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar));

    window.set_content(Some(&toast_overlay));
    window.present();

    (view, window)
}

// ─── Live viewer action ────────────────────────────────────────────────

/// Build the Export PNG header button. Snapshots the viewer's
/// current state on the GTK main thread (drains pending rows +
/// clones either the per-channel `Vec<u8>` or the three composite
/// source channels under a brief mutex hold), then hands the
/// heavy PNG encoding + filesystem I/O to
/// [`spawn_export_worker`]'s `gio::spawn_blocking` — same pattern
/// as the LOS `SaveLrptPass` handler; before this, the manual
/// Export PNG button froze the GTK main loop on any large channel
/// (Cairo PNG encoding is O(width × `n_lines`), not negligible at
/// the ≤8192-line cap; per `CodeRabbit` round 10 on PR #543).
///
/// Composite-mode aware: `snapshot_for_export` returns an
/// [`ExportSnapshot::Composite`] when the user has a composite
/// recipe active so the manual Export PNG matches what's on
/// screen (per CR round 2 on PR #575 — before that, exporting
/// while a composite was displayed wrote out the last greyscale
/// APID's surface instead). Split out of
/// [`open_lrpt_viewer_window`] per the 50-NLOC gate (#819).
fn build_export_button(view: &LrptImageView, window: &adw::Window) -> gtk4::Button {
    let export_btn = gtk4::Button::builder()
        .icon_name("document-save-symbolic")
        // Wording covers both per-APID and composite exports — the
        // button writes whatever the dropdown has selected
        // (per-channel greyscale OR a false-colour composite). Per
        // CR round 3 on PR #575.
        .tooltip_text("Export the current LRPT image to PNG")
        .build();
    export_btn.update_property(&[gtk4::accessible::Property::Label(
        "Export current LRPT image to PNG",
    )]);
    let export_view = view.clone();
    let window_for_export = window.downgrade();
    export_btn.connect_clicked(move |_| {
        let Some(window_now) = window_for_export.upgrade() else {
            return;
        };
        let Some(snapshot) = export_view.snapshot_for_export() else {
            // Either nothing is selected, or the active channel /
            // composite has no decoded rows yet. Surface as a
            // clear toast rather than an opaque "no active
            // channel" error.
            show_toast_in(
                &window_now,
                adw::Toast::builder()
                    .title("No LRPT image data to export yet")
                    .build(),
            );
            return;
        };
        // Filename is derived AFTER the snapshot so the resolved
        // APID (or composite recipe slug) lands in it. See
        // `default_export_path` / `composite_export_path` for the
        // disk-layout convention.
        let path = match &snapshot {
            ExportSnapshot::Channel { apid, .. } => default_export_path(Some(*apid)),
            ExportSnapshot::Composite { recipe, .. } => composite_export_path(recipe.name),
        };
        spawn_export_worker(snapshot, path, window_now.downgrade());
    });
    export_btn
}

/// Off-main-thread half of the Export PNG button: write the
/// snapshot to `path` inside `gio::spawn_blocking` (per-channel
/// greyscale via [`write_greyscale_png`]; composite via
/// `assemble_rgb_composite` + [`write_rgb_png`]) and toast the
/// outcome back into the viewer window. Split out per the 50-NLOC
/// gate (#819).
fn spawn_export_worker(
    snapshot: ExportSnapshot,
    path: std::path::PathBuf,
    window_weak: glib::WeakRef<adw::Window>,
) {
    glib::spawn_future_local(async move {
        let path_for_msg = path.clone();
        let result = gio::spawn_blocking(move || match snapshot {
            ExportSnapshot::Channel { buffer, .. } => {
                write_greyscale_png(&path, &buffer.pixels, IMAGE_WIDTH, buffer.lines)
            }
            ExportSnapshot::Composite { snapshot, .. } => {
                let rgb = sdr_lrpt::image::assemble_rgb_composite(
                    &snapshot.r_pixels,
                    &snapshot.g_pixels,
                    &snapshot.b_pixels,
                    snapshot.height,
                );
                write_rgb_png(&path, &rgb, IMAGE_WIDTH, snapshot.height)
            }
        })
        .await;
        let toast = match result {
            Ok(Ok(())) => plain_toast(&format!("Saved {}", path_for_msg.display())),
            Ok(Err(e)) => plain_toast(&format!("PNG export failed: {e}")),
            Err(e) => {
                // Worker thread panicked. `Box<dyn Any>`
                // doesn't implement Display — log via Debug,
                // surface a generic message.
                tracing::warn!("manual LRPT export worker panicked: {e:?}");
                adw::Toast::builder()
                    .title("PNG export worker panicked")
                    .build()
            }
        };
        if let Some(window) = window_weak.upgrade() {
            show_toast_in(&window, toast);
        }
    });
}

/// Wire the `app.lrpt-open` action onto `app`. Activating it
/// (via the app menu, the `Ctrl+Shift+L` accelerator, or future
/// activity-bar entry) opens a non-modal LRPT viewer window
/// and informs the DSP controller about the shared image
/// handle so the LRPT decoder tap starts pushing scan lines
/// into it. Closing the window clears the `AppState` slot
/// (the GTK widget tree drops with the window) but leaves the
/// DSP-side decoder + shared image attached so an in-flight
/// auto-record pass keeps capturing — see the close-request
/// comment in [`open_lrpt_viewer_if_needed`] and the
/// module-level docs above for the lifecycle rationale.
pub fn connect_lrpt_action(
    app: &adw::Application,
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    let action = gio::SimpleAction::new("lrpt-open", None);
    let parent_provider = Rc::clone(parent_provider);
    let state_for_action = Rc::clone(state);
    action.connect_activate(move |_, _| {
        open_lrpt_viewer_if_needed(&parent_provider, &state_for_action);
    });
    app.add_action(&action);
    app.set_accels_for_action("app.lrpt-open", &["<Ctrl><Shift>l"]);
}

/// Open the LRPT viewer window if it isn't already open.
/// Registers the new view in `state.lrpt_viewer`, hands the
/// shared image to the DSP thread, and wires `close-request`
/// to cancel the view's `glib` timers + drop the `AppState` slot.
/// The DSP capture (decoder + shared image) intentionally
/// outlives the window — see the close-request body for why.
///
/// Pulled out of [`connect_lrpt_action`] so the auto-record
/// path (Task 7.5) can fire the same open flow at AOS without
/// going through the GIO action system. Mirrors the APT
/// viewer's [`crate::apt_viewer::open_apt_viewer_if_needed`].
pub fn open_lrpt_viewer_if_needed(
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    if state.lrpt_viewer.borrow().is_some() {
        // Defensive re-attach: if a future code path ever
        // detaches the DSP-side image (today nothing sends
        // `ClearLrptImage`, but a future refactor might), the
        // existing-viewer fast-path would silently leave the
        // tap muted. Re-sending `SetLrptImage` is idempotent
        // — the controller's handler no longer drops the
        // decoder on attach (round 11 paired change), so
        // mid-pass decoder state survives the round-trip. Per
        // `CodeRabbit` round 11 on PR #543.
        state.send_dsp(UiToDsp::SetLrptImage(state.lrpt_image.clone()));
        // Raise the existing window so `Ctrl+Shift+L` actually
        // surfaces a buried / minimised viewer instead of being
        // a silent no-op. Weak-ref upgrade fails closed: if the
        // window is gone but the AppState slot wasn't cleared
        // yet (close-request race), we just skip — the slot
        // will clear momentarily anyway. Per `CodeRabbit`
        // round 13 on PR #543.
        if let Some(window) = state
            .lrpt_viewer_window
            .borrow()
            .as_ref()
            .and_then(glib::WeakRef::upgrade)
        {
            window.present();
        }
        return;
    }
    let Some(parent) = parent_provider() else {
        tracing::warn!("lrpt-open invoked with no main window available");
        return;
    };
    let image = state.lrpt_image.clone();
    let (view, window) = open_lrpt_viewer_window(&parent, "Meteor-M LRPT", image.clone());
    *state.lrpt_viewer.borrow_mut() = Some(view);
    *state.lrpt_viewer_window.borrow_mut() = Some(window.downgrade());
    state.send_dsp(UiToDsp::SetLrptImage(image));

    let state_for_close = Rc::clone(state);
    window.connect_close_request(move |_| {
        // Cancel the view's drain + dropdown-refresh timeouts
        // BEFORE we drop the AppState slot; otherwise their
        // closures' `Rc<view>` clones keep the view + ~51 MB-
        // per-channel surfaces alive until the application
        // exits. Per `CodeRabbit` round 1 on PR #543.
        if let Some(view) = state_for_close.lrpt_viewer.borrow().as_ref() {
            view.shutdown();
        }
        *state_for_close.lrpt_viewer.borrow_mut() = None;
        *state_for_close.lrpt_viewer_window.borrow_mut() = None;
        // Deliberately NOT sending `UiToDsp::ClearLrptImage`
        // here — the DSP-side decoder + shared image stay
        // attached so the DSP keeps decoding into the shared
        // image regardless of viewer presence. Closing the
        // viewer mid-pass used to drop all subsequent rows
        // and break the LOS `SaveLrptPass` save (the recorder
        // would post "no image saved" even though decoding
        // was still feasible). Now the recorder reads the
        // shared image directly at LOS, so viewer close is
        // purely a UI teardown. The decoder remains gated by
        // `current_mode == Lrpt` and the source-stop cleanup
        // path, so closing the viewer in manual LRPT mode
        // doesn't burn CPU forever — switching demod or
        // stopping the source still tears it down. Per
        // `CodeRabbit` round 7 on PR #543.
        glib::Propagation::Proceed
    });
}

#[cfg(test)]
mod tests {
    use super::{COMPOSITE_CATALOG, DropdownEntry, desired_dropdown_entries, entries_match};

    #[test]
    fn desired_entries_are_apids_in_order_then_full_catalog() {
        let desired = desired_dropdown_entries(&[68, 65]);
        assert_eq!(desired.len(), 2 + COMPOSITE_CATALOG.len());
        assert!(matches!(desired[0], DropdownEntry::Apid(68)));
        assert!(matches!(desired[1], DropdownEntry::Apid(65)));
        for (entry, recipe) in desired[2..].iter().zip(COMPOSITE_CATALOG.iter()) {
            assert!(matches!(entry, DropdownEntry::Composite(r) if r == recipe));
        }
    }

    #[test]
    fn entries_match_rejects_length_and_variant_mismatches() {
        let a = desired_dropdown_entries(&[68]);
        let b = desired_dropdown_entries(&[68]);
        assert!(entries_match(&a, &b));
        // Different APID set → mismatch.
        let c = desired_dropdown_entries(&[68, 65]);
        assert!(!entries_match(&a, &c));
        // Same length, different variant in slot 0 → mismatch.
        let mut d = b.clone();
        d[0] = DropdownEntry::Composite(COMPOSITE_CATALOG[0]);
        assert!(!entries_match(&a, &d));
    }
}
