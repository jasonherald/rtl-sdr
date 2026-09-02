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
/// Honors the search filter in `filter_text` by delegating to
/// [`bookmark_matches_filter`] — lowercase substring match
/// against the bookmark's name, subtitle (demod + frequency),
/// and `rr_category`. Emits an `AdwExpanderRow` per unique
/// `rr_category` when any bookmark is categorized; falls back
/// to a flat list when all bookmarks are uncategorized so users
/// who don't import from `RadioReference` keep the original
/// single-level view.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::implicit_hasher,
    reason = "manual_expanded is a private handle passed Rc-clone-style between internal callers — the default `RandomState` hasher is fine and genericizing would force every caller site to spell the hasher"
)]
pub fn rebuild_bookmark_list(
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    bookmarks: &std::rc::Rc<std::cell::RefCell<Vec<Bookmark>>>,
    on_navigate: &std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>>,
    active: &std::rc::Rc<std::cell::RefCell<ActiveBookmark>>,
    name_entry: &adw::EntryRow,
    on_save: &SaveCallback,
    filter_text: &std::rc::Rc<std::cell::RefCell<String>>,
    manual_expanded: &std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
    on_mutated: &BookmarksMutatedCallback,
) {
    // Remove all existing rows.
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    let bm_list = bookmarks.borrow();
    let current_active = active.borrow().clone();
    let needle = filter_text.borrow().clone();
    let uses_categories = bm_list.iter().any(|b| b.rr_category.is_some());
    // Read manual expansion state separately from widget state.
    // We can't snapshot widget expansion on every rebuild: the
    // search path force-opens every expander, and if the user
    // then clears the search, we'd treat those forced opens as
    // manual intent. Instead `manual_expanded` is only mutated
    // by the `expanded-notify` handler below, and only when no
    // filter is active — so programmatic expansions (search,
    // active-category restore, initial apply on rebuild) never
    // pollute it.
    let manual_open: std::collections::HashSet<String> = manual_expanded.borrow().clone();

    if uses_categories {
        // Collect bookmarks into category buckets, preserving
        // within-category insertion order. `BTreeMap` gives
        // deterministic alphabetical category ordering; use
        // a single `Uncategorized` bucket for loose bookmarks.
        let mut groups: std::collections::BTreeMap<String, Vec<&Bookmark>> =
            std::collections::BTreeMap::new();
        for bm in bm_list.iter() {
            if !bookmark_matches_filter(bm, &needle) {
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
            let expander = adw::ExpanderRow::builder()
                .title(&cat)
                .subtitle(format!(
                    "{} bookmark{}",
                    items.len(),
                    if items.len() == 1 { "" } else { "s" }
                ))
                .build();
            // Expand when: a filter is active (so matches
            // surface without a manual click); the user
            // manually expanded this category before (preserved
            // across rebuilds via `manual_expanded`); or this
            // category holds the active bookmark (so recall
            // keeps its section open).
            let keep_expanded = !needle.is_empty()
                || manual_open.contains(&cat)
                || active_category.as_deref() == Some(cat.as_str());
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
            let manual_for_notify = std::rc::Rc::clone(manual_expanded);
            let filter_for_notify = std::rc::Rc::clone(filter_text);
            let cat_for_notify = cat.clone();
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

            for bm in items {
                let row = build_bookmark_row(
                    bm,
                    &current_active,
                    list_box,
                    scroll,
                    bookmarks,
                    on_navigate,
                    active,
                    name_entry,
                    on_save,
                    filter_text,
                    manual_expanded,
                    on_mutated,
                );
                expander.add_row(&row);
            }
            list_box.append(&expander);
        }
    } else {
        for bm in bm_list.iter() {
            if !bookmark_matches_filter(bm, &needle) {
                continue;
            }
            let row = build_bookmark_row(
                bm,
                &current_active,
                list_box,
                scroll,
                bookmarks,
                on_navigate,
                active,
                name_entry,
                on_save,
                filter_text,
                manual_expanded,
                on_mutated,
            );
            list_box.append(&row);
        }
    }

    // Left-sidebar legacy sizing only makes sense in flat mode —
    // the flyout is vexpand and doesn't need a min height. Keep
    // the 3-row cap for flat lists (where this function is still
    // called from the sidebar's pre-flyout code path) and skip
    // entirely for expander-grouped views. Use the filtered row
    // count, not the total — when a search is active the scroll
    // region should shrink with the visible list rather than
    // reserving space for filtered-out rows.
    if uses_categories {
        scroll.set_height_request(-1);
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let count = bm_list
            .iter()
            .filter(|bm| bookmark_matches_filter(bm, &needle))
            .count() as i32;
        let visible = count.clamp(0, MAX_VISIBLE_BOOKMARKS);
        let height = visible * BOOKMARK_ROW_HEIGHT;
        scroll.set_height_request(height);
    }
}

/// Build a single bookmark `ActionRow` with its active-highlight
/// prefix, save/delete suffix buttons, and recall-on-activate
/// handler wired up.
///
/// Shared between the flat and categorized rebuild paths so the
/// row's behavior is identical regardless of whether it's a
/// top-level child of the `ListBox` or nested under an
/// `AdwExpanderRow`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn build_bookmark_row(
    bm: &Bookmark,
    current_active: &ActiveBookmark,
    list_box: &gtk4::ListBox,
    scroll: &gtk4::ScrolledWindow,
    bookmarks: &std::rc::Rc<std::cell::RefCell<Vec<Bookmark>>>,
    on_navigate: &std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>>,
    active: &std::rc::Rc<std::cell::RefCell<ActiveBookmark>>,
    name_entry: &adw::EntryRow,
    on_save: &SaveCallback,
    filter_text: &std::rc::Rc<std::cell::RefCell<String>>,
    manual_expanded: &std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
    on_mutated: &BookmarksMutatedCallback,
) -> adw::ActionRow {
    let is_active = bm.name == current_active.name && bm.frequency == current_active.frequency;
    let row = adw::ActionRow::builder()
        .title(&bm.name)
        .subtitle(bm.settings_subtitle())
        .activatable(true)
        .build();

    if is_active {
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
        let save_cb = std::rc::Rc::clone(on_save);
        save_btn.connect_clicked(move |_| {
            if let Some(cb) = save_cb.borrow().as_ref() {
                cb();
            }
        });
        row.add_suffix(&save_btn);
    }

    let delete_btn = gtk4::Button::builder()
        .icon_name("user-trash-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text("Delete bookmark")
        .css_classes(["flat"])
        .build();
    delete_btn.update_property(&[gtk4::accessible::Property::Label("Delete bookmark")]);

    let bm_rc = std::rc::Rc::clone(bookmarks);
    let nav_rc = std::rc::Rc::clone(on_navigate);
    let active_rc = std::rc::Rc::clone(active);
    let save_del = std::rc::Rc::clone(on_save);
    let filter_del = std::rc::Rc::clone(filter_text);
    let manual_expanded_del = std::rc::Rc::clone(manual_expanded);
    let on_mutated_del = std::rc::Rc::clone(on_mutated);
    let list_ref = list_box.downgrade();
    let scroll_ref = scroll.downgrade();
    let entry_del = name_entry.clone();
    let del_name = bm.name.clone();
    let del_freq = bm.frequency;
    delete_btn.connect_clicked(move |_| {
        {
            let active = active_rc.borrow();
            if active.name == del_name && active.frequency == del_freq {
                drop(active);
                *active_rc.borrow_mut() = ActiveBookmark::default();
                entry_del.set_text("");
            }
        }
        // Remove only the first matching bookmark rather than
        // every entry with the same (name, frequency). Quick-add
        // intentionally always creates a new `Bookmark`, so
        // duplicates are a supported state — one click on the
        // trash icon should delete the one row the user pointed
        // at, not wipe the whole set. Stable bookmark IDs will
        // supersede this first-match contract once they land.
        let remove_idx = bm_rc
            .borrow()
            .iter()
            .position(|b| b.name == del_name && b.frequency == del_freq);
        if let Some(idx) = remove_idx {
            bm_rc.borrow_mut().remove(idx);
        }
        save_bookmarks(&bm_rc.borrow());
        // Fire the mutation callback *before* the rebuild so the
        // scanner sees the post-delete channel list in the same
        // tick the UI re-renders — no transient "channel still
        // present" window.
        if let Some(cb) = on_mutated_del.borrow().as_ref() {
            cb();
        }
        if let Some(lb) = list_ref.upgrade()
            && let Some(sc) = scroll_ref.upgrade()
        {
            rebuild_bookmark_list(
                &lb,
                &sc,
                &bm_rc,
                &nav_rc,
                &active_rc,
                &entry_del,
                &save_del,
                &filter_del,
                &manual_expanded_del,
                &on_mutated_del,
            );
        }
    });
    row.add_suffix(&delete_btn);

    // --- Scanner scan-enable checkbox ---
    // Checked = include this bookmark in scanner rotation. Drop
    // the borrow before firing `on_mutated` so the callback can
    // itself borrow the bookmarks list (for projection) without
    // panicking on a nested mutable + immutable borrow.
    let scan_check = gtk4::CheckButton::builder()
        .tooltip_text("Include in scanner")
        .active(bm.scan_enabled)
        .valign(gtk4::Align::Center)
        .build();
    scan_check.update_property(&[gtk4::accessible::Property::Label("Include in scanner")]);
    let bm_scan = std::rc::Rc::clone(bookmarks);
    let on_mutated_scan = std::rc::Rc::clone(on_mutated);
    let scan_name = bm.name.clone();
    let scan_freq = bm.frequency;
    scan_check.connect_toggled(move |btn| {
        let active = btn.is_active();
        {
            let mut bms = bm_scan.borrow_mut();
            if let Some(b) = bms
                .iter_mut()
                .find(|b| b.name == scan_name && b.frequency == scan_freq)
            {
                b.scan_enabled = active;
            }
            save_bookmarks(&bms);
        }
        if let Some(cb) = on_mutated_scan.borrow().as_ref() {
            cb();
        }
    });
    row.add_suffix(&scan_check);

    // --- Scanner priority star toggle ---
    // Toggled = priority 1 (checked more often by the scanner).
    // Phase 1 is binary — higher tiers are reserved for later
    // phases, so the UI exposes on/off rather than a spinner.
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
    let bm_pri = std::rc::Rc::clone(bookmarks);
    let on_mutated_pri = std::rc::Rc::clone(on_mutated);
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
            let mut bms = bm_pri.borrow_mut();
            if let Some(b) = bms
                .iter_mut()
                .find(|b| b.name == pri_name && b.frequency == pri_freq)
            {
                b.priority = u8::from(active);
            }
            save_bookmarks(&bms);
        }
        if let Some(cb) = on_mutated_pri.borrow().as_ref() {
            cb();
        }
    });
    row.add_suffix(&pri_btn);

    let recall_bookmark = bm.clone();
    let on_nav_recall = std::rc::Rc::clone(on_navigate);
    let active_recall = std::rc::Rc::clone(active);
    let save_recall = std::rc::Rc::clone(on_save);
    let filter_recall = std::rc::Rc::clone(filter_text);
    let manual_expanded_recall = std::rc::Rc::clone(manual_expanded);
    let on_mutated_recall = std::rc::Rc::clone(on_mutated);
    let bm_recall = std::rc::Rc::clone(bookmarks);
    let list_recall = list_box.downgrade();
    let scroll_recall = scroll.downgrade();
    let entry_recall = name_entry.clone();
    row.connect_activated(move |_| {
        *active_recall.borrow_mut() = ActiveBookmark {
            name: recall_bookmark.name.clone(),
            frequency: recall_bookmark.frequency,
        };
        entry_recall.set_text(&recall_bookmark.name);

        if let Some(cb) = on_nav_recall.borrow().as_ref() {
            cb(&recall_bookmark);
        }

        if let Some(lb) = list_recall.upgrade()
            && let Some(sc) = scroll_recall.upgrade()
        {
            let saved_scroll = sc.vadjustment().value();
            rebuild_bookmark_list(
                &lb,
                &sc,
                &bm_recall,
                &on_nav_recall,
                &active_recall,
                &entry_recall,
                &save_recall,
                &filter_recall,
                &manual_expanded_recall,
                &on_mutated_recall,
            );
            let adj = sc.vadjustment();
            glib::idle_add_local_once(move || {
                adj.set_value(saved_scroll);
            });
        }
    });

    row
}
