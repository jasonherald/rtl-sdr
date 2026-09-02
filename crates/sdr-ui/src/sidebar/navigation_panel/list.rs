//! Bookmark-list rendering for the bookmarks flyout — the
//! category-grouped / flat rebuild, per-row build with recall,
//! save, delete, scan-enable, and priority controls, and the
//! search filter predicate. Split out of `navigation_panel.rs`
//! per the file-size pass (issue #819).

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use super::bookmarks::save_bookmarks;
use super::{ActiveBookmark, Bookmark, BookmarksMutatedCallback, NavigationCallback, SaveCallback};

/// Approximate height of one `AdwActionRow` with subtitle in pixels.
const BOOKMARK_ROW_HEIGHT: i32 = 56;
/// Maximum visible bookmark rows before scrolling.
const MAX_VISIBLE_BOOKMARKS: i32 = 3;

/// Sentinel category title for bookmarks without an `rr_category`.
/// Only shown when the list contains a mix of categorized and
/// uncategorized bookmarks — pure-uncategorized lists render
/// flat, skipping expander grouping entirely.
const UNCATEGORIZED_LABEL: &str = "Uncategorized";

/// Shared handles every bookmark-list rebuild and row action needs
/// — a named bundle replacing the former ten-parameter
/// [`rebuild_bookmark_list`] signature per the 8-parameter gate
/// (#819; `SpectrumShared` on PR #883 and `ClientSetupDeps` on
/// PR #880 are the precedents). Every field is an `Rc` or a
/// `GObject` handle, so `Clone` is a cheap refcount bump: row
/// closures capture a clone, and the recursive rebuild threads
/// the same context back through.
#[derive(Clone)]
pub struct BookmarkListCtx {
    /// The backing bookmark store.
    pub bookmarks: std::rc::Rc<std::cell::RefCell<Vec<Bookmark>>>,
    /// Navigate callback fired on recall / preset selection.
    pub on_navigate: std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>>,
    /// Identity of the currently active bookmark.
    pub active: std::rc::Rc<std::cell::RefCell<ActiveBookmark>>,
    /// Shared quick-add name entry (owned by the Navigation
    /// panel; the flyout borrows it for recall / delete resets).
    pub name_entry: adw::EntryRow,
    /// Save-over-active callback for the active row's 💾 button.
    pub on_save: SaveCallback,
    /// Lowercased search needle.
    pub filter_text: std::rc::Rc<std::cell::RefCell<String>>,
    /// Categories the user manually expanded (see the rebuild's
    /// expansion-state commentary).
    pub manual_expanded: std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
    /// Scanner re-projection callback fired on list mutations.
    pub on_mutated: BookmarksMutatedCallback,
}

/// Does the bookmark's name, subtitle, or category contain the
/// (already-lowercased) search needle? Empty needle matches
/// everything. Category matching lets users filter by the
/// `rr_category` label (e.g., typing "dispatch" or "fire")
/// when bookmarks are imported from `RadioReference`, not just
/// by name or demod/frequency.
pub(super) fn bookmark_matches_filter(bm: &Bookmark, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let name_lc = bm.name.to_lowercase();
    let subtitle_lc = bm.settings_subtitle().to_lowercase();
    let category_lc = bm.rr_category.as_deref().unwrap_or_default().to_lowercase();
    name_lc.contains(needle) || subtitle_lc.contains(needle) || category_lc.contains(needle)
}

/// Rebuild the bookmark `ListBox` from the current bookmark list.
///
/// Honors the search filter in `ctx.filter_text` by delegating to
/// [`bookmark_matches_filter`] — lowercase substring match
/// against the bookmark's name, subtitle (demod + frequency),
/// and `rr_category`. Emits an `AdwExpanderRow` per unique
/// `rr_category` when any bookmark is categorized (see
/// [`rebuild_categorized`]); falls back to a flat list when all
/// bookmarks are uncategorized so users who don't import from
/// `RadioReference` keep the original single-level view.
pub fn rebuild_bookmark_list(
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    ctx: &BookmarkListCtx,
) {
    // Remove all existing rows.
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    let bm_list = ctx.bookmarks.borrow();
    let current_active = ctx.active.borrow().clone();
    let needle = ctx.filter_text.borrow().clone();
    let uses_categories = bm_list.iter().any(|b| b.rr_category.is_some());

    if uses_categories {
        rebuild_categorized(list_box, scroll, ctx, &bm_list, &current_active, &needle);
    } else {
        for bm in bm_list.iter() {
            if !bookmark_matches_filter(bm, &needle) {
                continue;
            }
            let row = build_bookmark_row(bm, &current_active, list_box, scroll, ctx);
            list_box.append(&row);
        }
    }

    apply_list_sizing(scroll, uses_categories, &bm_list, &needle);
}

/// Category-grouped rebuild path: bucket the filtered bookmarks
/// per `rr_category` (deterministic alphabetical order via
/// `BTreeMap`, one `Uncategorized` bucket for loose entries),
/// then emit one expander per non-empty bucket. Split out of
/// [`rebuild_bookmark_list`] per the 50-NLOC gate (#819).
fn rebuild_categorized(
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    ctx: &BookmarkListCtx,
    bm_list: &[Bookmark],
    current_active: &ActiveBookmark,
    needle: &str,
) {
    // Read manual expansion state separately from widget state.
    // We can't snapshot widget expansion on every rebuild: the
    // search path force-opens every expander, and if the user
    // then clears the search, we'd treat those forced opens as
    // manual intent. Instead `manual_expanded` is only mutated
    // by the `expanded-notify` handler below, and only when no
    // filter is active — so programmatic expansions (search,
    // active-category restore, initial apply on rebuild) never
    // pollute it.
    let manual_open: std::collections::HashSet<String> = ctx.manual_expanded.borrow().clone();

    // Collect bookmarks into category buckets, preserving
    // within-category insertion order. `BTreeMap` gives
    // deterministic alphabetical category ordering; use
    // a single `Uncategorized` bucket for loose bookmarks.
    let mut groups: std::collections::BTreeMap<String, Vec<&Bookmark>> =
        std::collections::BTreeMap::new();
    for bm in bm_list {
        if !bookmark_matches_filter(bm, needle) {
            continue;
        }
        let cat = bm
            .rr_category
            .clone()
            .unwrap_or_else(|| UNCATEGORIZED_LABEL.to_string());
        groups.entry(cat).or_default().push(bm);
    }
    // The category containing the active bookmark, so we can
    // guarantee it stays expanded even if the user hadn't
    // opened it before the recall (e.g., clicking through
    // search results into a previously-collapsed section).
    let active_category = bm_list
        .iter()
        .find(|b| b.name == current_active.name && b.frequency == current_active.frequency)
        .map(|b| {
            b.rr_category
                .clone()
                .unwrap_or_else(|| UNCATEGORIZED_LABEL.to_string())
        });
    for (cat, items) in groups {
        if items.is_empty() {
            continue;
        }
        // Expand when: a filter is active (so matches
        // surface without a manual click); the user
        // manually expanded this category before (preserved
        // across rebuilds via `manual_expanded`); or this
        // category holds the active bookmark (so recall
        // keeps its section open).
        let keep_expanded = !needle.is_empty()
            || manual_open.contains(&cat)
            || active_category.as_deref() == Some(cat.as_str());
        let expander = build_category_expander(&cat, items.len(), keep_expanded, ctx);
        for bm in items {
            let row = build_bookmark_row(bm, current_active, list_box, scroll, ctx);
            expander.add_row(&row);
        }
        list_box.append(&expander);
    }
}

/// One category `AdwExpanderRow` with its expansion state applied
/// and the manual-expansion tracking wired. Split out per the
/// 50-NLOC gate (#819).
fn build_category_expander(
    cat: &str,
    item_count: usize,
    keep_expanded: bool,
    ctx: &BookmarkListCtx,
) -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder()
        .title(cat)
        .subtitle(format!(
            "{} bookmark{}",
            item_count,
            if item_count == 1 { "" } else { "s" }
        ))
        .build();
    expander.set_expanded(keep_expanded);

    // Track user-driven expansion toggles. Connects
    // *after* the initial `set_expanded` above so the
    // programmatic apply doesn't fire the handler —
    // GLib signals only reach handlers connected at the
    // time of emission. Gated on filter being empty
    // because during search all expanders are force-
    // open; a user toggle under those conditions
    // reflects "show/hide matches in this category
    // right now", not lasting intent.
    let manual_for_notify = std::rc::Rc::clone(&ctx.manual_expanded);
    let filter_for_notify = std::rc::Rc::clone(&ctx.filter_text);
    let cat_for_notify = cat.to_string();
    expander.connect_expanded_notify(move |row| {
        if !filter_for_notify.borrow().is_empty() {
            return;
        }
        if row.is_expanded() {
            manual_for_notify
                .borrow_mut()
                .insert(cat_for_notify.clone());
        } else {
            manual_for_notify.borrow_mut().remove(&cat_for_notify);
        }
    });
    expander
}

/// Legacy left-sidebar scroll sizing — 3-row cap for flat lists,
/// natural height for expander-grouped views. Uses the filtered
/// row count, not the total — when a search is active the scroll
/// region should shrink with the visible list rather than
/// reserving space for filtered-out rows. Split out per the
/// 50-NLOC gate (#819).
fn apply_list_sizing(
    scroll: &gtk4::ScrolledWindow,
    uses_categories: bool,
    bm_list: &[Bookmark],
    needle: &str,
) {
    if uses_categories {
        scroll.set_height_request(-1);
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let count = bm_list
            .iter()
            .filter(|bm| bookmark_matches_filter(bm, needle))
            .count() as i32;
        let visible = count.clamp(0, MAX_VISIBLE_BOOKMARKS);
        let height = visible * BOOKMARK_ROW_HEIGHT;
        scroll.set_height_request(height);
    }
}

/// Build a single bookmark `ActionRow` with its active-highlight
/// prefix, save/delete suffix buttons, scanner controls, and
/// recall-on-activate handler wired up.
///
/// Shared between the flat and categorized rebuild paths so the
/// row's behavior is identical regardless of whether it's a
/// top-level child of the `ListBox` or nested under an
/// `AdwExpanderRow`. Orchestrator over one wiring helper per
/// control per the 50-NLOC gate (#819); suffix order (save →
/// delete → scan checkbox → priority star) matches the pre-split
/// attachment sequence.
fn build_bookmark_row(
    bm: &Bookmark,
    current_active: &ActiveBookmark,
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    ctx: &BookmarkListCtx,
) -> adw::ActionRow {
    let is_active = bm.name == current_active.name && bm.frequency == current_active.frequency;
    let row = adw::ActionRow::builder()
        .title(&bm.name)
        .subtitle(bm.settings_subtitle())
        .activatable(true)
        .build();

    if is_active {
        add_active_affordances(&row, ctx);
    }
    wire_delete_button(&row, bm, list_box, scroll, ctx);
    wire_scan_check(&row, bm, ctx);
    wire_priority_toggle(&row, bm, ctx);
    wire_recall(&row, bm, list_box, scroll, ctx);
    row
}

/// Active-row affordances: the playing prefix icon and the
/// save-current-settings suffix button. Split out per the 50-NLOC
/// gate (#819).
fn add_active_affordances(row: &adw::ActionRow, ctx: &BookmarkListCtx) {
    let icon = gtk4::Image::from_icon_name("media-playback-start-symbolic");
    icon.set_valign(gtk4::Align::Center);
    row.add_prefix(&icon);

    let save_btn = gtk4::Button::builder()
        .icon_name("media-floppy-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text("Save current settings to this bookmark")
        .css_classes(["flat"])
        .build();
    save_btn.update_property(&[gtk4::accessible::Property::Label(
        "Save current settings to this bookmark",
    )]);
    let save_cb = std::rc::Rc::clone(&ctx.on_save);
    save_btn.connect_clicked(move |_| {
        if let Some(cb) = save_cb.borrow().as_ref() {
            cb();
        }
    });
    row.add_suffix(&save_btn);
}

/// Delete-button suffix: removes the first matching bookmark,
/// clears the active highlight when it pointed here, persists,
/// fires the mutation callback, and rebuilds. Split out per the
/// 50-NLOC gate (#819).
fn wire_delete_button(
    row: &adw::ActionRow,
    bm: &Bookmark,
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    ctx: &BookmarkListCtx,
) {
    let delete_btn = gtk4::Button::builder()
        .icon_name("user-trash-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text("Delete bookmark")
        .css_classes(["flat"])
        .build();
    delete_btn.update_property(&[gtk4::accessible::Property::Label("Delete bookmark")]);

    let ctx_del = ctx.clone();
    let list_ref = list_box.downgrade();
    let scroll_ref = scroll.downgrade();
    let del_name = bm.name.clone();
    let del_freq = bm.frequency;
    delete_btn.connect_clicked(move |_| {
        {
            let active = ctx_del.active.borrow();
            if active.name == del_name && active.frequency == del_freq {
                drop(active);
                *ctx_del.active.borrow_mut() = ActiveBookmark::default();
                ctx_del.name_entry.set_text("");
            }
        }
        // Remove only the first matching bookmark rather than
        // every entry with the same (name, frequency). Quick-add
        // intentionally always creates a new `Bookmark`, so
        // duplicates are a supported state — one click on the
        // trash icon should delete the one row the user pointed
        // at, not wipe the whole set. Stable bookmark IDs will
        // supersede this first-match contract once they land.
        let remove_idx = ctx_del
            .bookmarks
            .borrow()
            .iter()
            .position(|b| b.name == del_name && b.frequency == del_freq);
        if let Some(idx) = remove_idx {
            ctx_del.bookmarks.borrow_mut().remove(idx);
        }
        save_bookmarks(&ctx_del.bookmarks.borrow());
        // Fire the mutation callback *before* the rebuild so the
        // scanner sees the post-delete channel list in the same
        // tick the UI re-renders — no transient "channel still
        // present" window.
        if let Some(cb) = ctx_del.on_mutated.borrow().as_ref() {
            cb();
        }
        if let Some(lb) = list_ref.upgrade()
            && let Some(sc) = scroll_ref.upgrade()
        {
            rebuild_bookmark_list(&lb, &sc, &ctx_del);
        }
    });
    row.add_suffix(&delete_btn);
}

/// Scanner scan-enable checkbox suffix. Checked = include this
/// bookmark in scanner rotation. Drops the borrow before firing
/// `on_mutated` so the callback can itself borrow the bookmarks
/// list (for projection) without panicking on a nested mutable +
/// immutable borrow. Split out per the 50-NLOC gate (#819).
fn wire_scan_check(row: &adw::ActionRow, bm: &Bookmark, ctx: &BookmarkListCtx) {
    let scan_check = gtk4::CheckButton::builder()
        .tooltip_text("Include in scanner")
        .active(bm.scan_enabled)
        .valign(gtk4::Align::Center)
        .build();
    scan_check.update_property(&[gtk4::accessible::Property::Label("Include in scanner")]);
    let ctx_scan = ctx.clone();
    let scan_name = bm.name.clone();
    let scan_freq = bm.frequency;
    scan_check.connect_toggled(move |btn| {
        let active = btn.is_active();
        {
            let mut bms = ctx_scan.bookmarks.borrow_mut();
            if let Some(b) = bms
                .iter_mut()
                .find(|b| b.name == scan_name && b.frequency == scan_freq)
            {
                b.scan_enabled = active;
            }
            save_bookmarks(&bms);
        }
        if let Some(cb) = ctx_scan.on_mutated.borrow().as_ref() {
            cb();
        }
    });
    row.add_suffix(&scan_check);
}

/// Scanner priority star toggle suffix. Toggled = priority 1
/// (checked more often by the scanner). Phase 1 is binary —
/// higher tiers are reserved for later phases, so the UI exposes
/// on/off rather than a spinner. Split out per the 50-NLOC gate
/// (#819).
fn wire_priority_toggle(row: &adw::ActionRow, bm: &Bookmark, ctx: &BookmarkListCtx) {
    let pri_btn = gtk4::ToggleButton::builder()
        .icon_name(if bm.priority >= 1 {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        })
        .tooltip_text("Scanner priority channel")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .active(bm.priority >= 1)
        .build();
    pri_btn.update_property(&[gtk4::accessible::Property::Label(
        "Scanner priority channel",
    )]);
    let ctx_pri = ctx.clone();
    let pri_name = bm.name.clone();
    let pri_freq = bm.frequency;
    pri_btn.connect_toggled(move |btn| {
        let active = btn.is_active();
        btn.set_icon_name(if active {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });
        {
            let mut bms = ctx_pri.bookmarks.borrow_mut();
            if let Some(b) = bms
                .iter_mut()
                .find(|b| b.name == pri_name && b.frequency == pri_freq)
            {
                b.priority = u8::from(active);
            }
            save_bookmarks(&bms);
        }
        if let Some(cb) = ctx_pri.on_mutated.borrow().as_ref() {
            cb();
        }
    });
    row.add_suffix(&pri_btn);
}

/// Recall-on-activate handler: sets the active bookmark, fills
/// the name entry, fires the navigate callback, and rebuilds with
/// the scroll position preserved across the rebuild. Split out
/// per the 50-NLOC gate (#819).
fn wire_recall(
    row: &adw::ActionRow,
    bm: &Bookmark,
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    ctx: &BookmarkListCtx,
) {
    let recall_bookmark = bm.clone();
    let ctx_recall = ctx.clone();
    let list_recall = list_box.downgrade();
    let scroll_recall = scroll.downgrade();
    row.connect_activated(move |_| {
        *ctx_recall.active.borrow_mut() = ActiveBookmark {
            name: recall_bookmark.name.clone(),
            frequency: recall_bookmark.frequency,
        };
        ctx_recall.name_entry.set_text(&recall_bookmark.name);

        if let Some(cb) = ctx_recall.on_navigate.borrow().as_ref() {
            cb(&recall_bookmark);
        }

        if let Some(lb) = list_recall.upgrade()
            && let Some(sc) = scroll_recall.upgrade()
        {
            let saved_scroll = sc.vadjustment().value();
            rebuild_bookmark_list(&lb, &sc, &ctx_recall);
            let adj = sc.vadjustment();
            glib::idle_add_local_once(move || {
                adj.set_value(saved_scroll);
            });
        }
    });
}
