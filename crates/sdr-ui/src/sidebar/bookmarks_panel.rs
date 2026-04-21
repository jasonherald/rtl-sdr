//! Right-side bookmarks slide-out panel (#339).
//!
//! Complementary to the left sidebar's `NavigationPanel`: the sidebar
//! keeps the quick-add controls (name entry + Add button) so the user
//! can stash a bookmark without opening the flyout, while this panel
//! is the browse / search / manage surface for the full bookmark
//! list. Toggled from the header bar bookmark icon or `Ctrl+B`.
//!
//! The widget built here is packed into a `gtk4::Revealer` alongside
//! the transcript revealer in `window.rs::build_split_view`. Slide
//! transition matches the transcript pattern (200 ms slide from the
//! right edge of the content area).
//!
//! Owns the bookmark list state: the `Rc<RefCell<Vec<Bookmark>>>`
//! backing store, active-bookmark highlight, navigate / save
//! callbacks. `NavigationPanel`'s Add button wires into this state
//! via shared `Rc` clones — both panels render views of the same
//! underlying list.
//!
//! Commits that land on this file:
//! - Layout scaffolding (prior commit).
//! - List + row actions relocated from `NavigationPanel` (prior commit).
//! - Filter / search row (prior commit).
//! - Category grouping via `AdwExpanderRow` (prior commit).
//! - Persist flyout open/closed state across restarts (THIS commit).

use gtk4::prelude::*;
use libadwaita as adw;

use super::navigation_panel::{
    ActiveBookmark, Bookmark, NavigationCallback, SaveCallback, load_bookmarks,
    rebuild_bookmark_list,
};

/// Default width of the bookmarks flyout, in pixels. Wide enough for
/// a long bookmark nickname + a recall button + a delete affordance
/// without wrapping. Slightly wider than the transcript revealer
/// (320 px) because bookmark rows carry more suffix widgets (active
/// indicator, save button, delete button) than transcript items.
pub const BOOKMARKS_PANEL_WIDTH_PX: i32 = 360;

/// Config key for whether the bookmarks flyout was open at last
/// shutdown. Read once on startup to restore the reveal state +
/// the header toggle button's visual pressed state; written on
/// every toggle change.
pub const CONFIG_KEY_FLYOUT_OPEN: &str = "bookmarks_flyout_open";

/// Widget handles + shared state exposed from the bookmarks flyout.
///
/// Fields that external callers reach into:
/// - `widget` — root container packed into the revealer.
/// - `bookmark_list` / `bookmark_scroll` — list widget + scroll,
///   used by the `rebuild_bookmark_list` helper when the backing
///   store mutates (add, delete, import from `RadioReference`).
/// - `bookmarks` / `active_bookmark` / `on_navigate` / `on_save` —
///   shared `Rc`-backed state. `NavigationPanel`'s Add button
///   mutates `bookmarks` and calls `rebuild_bookmark_list`; the
///   preset row clears `active_bookmark` when a band preset is
///   selected. The `connect_navigate` / `connect_save` methods
///   register the callbacks the list rows invoke on click.
pub struct BookmarksPanel {
    /// Root container for the flyout — packed into the revealer.
    pub widget: gtk4::Box,
    /// Bookmark list widget. Rebuilt in place on every add /
    /// delete / import via [`rebuild_bookmark_list`].
    pub bookmark_list: gtk4::ListBox,
    /// Scrolled window wrapping `bookmark_list`. The rebuild
    /// helper uses it to enforce a max visible height so the
    /// panel grows only up to a cap before scrolling kicks in.
    pub bookmark_scroll: gtk4::ScrolledWindow,
    /// Shared bookmark backing store. Loaded once on startup via
    /// [`load_bookmarks`]; every mutation is followed by a
    /// [`save_bookmarks`](super::navigation_panel::save_bookmarks)
    /// persist.
    pub bookmarks: std::rc::Rc<std::cell::RefCell<Vec<Bookmark>>>,
    /// Identity of the bookmark currently loaded into the tuning
    /// state, used for the in-list "active" highlight + save
    /// button. Cleared by band-preset selection.
    pub active_bookmark: std::rc::Rc<std::cell::RefCell<ActiveBookmark>>,
    /// Callback invoked when the user clicks a list row — fed
    /// the `Bookmark` so window.rs can dispatch tune / bandwidth
    /// / profile-restore commands.
    pub on_navigate: std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>>,
    /// Callback invoked when the user clicks the "save over
    /// active" button on the active list row. Captures current
    /// tuning state at call time.
    pub on_save: SaveCallback,
    /// Current search needle (lowercased). Shared between the
    /// search-entry `search-changed` handler and the list
    /// rebuild path — the rebuild consults this on every call
    /// and omits non-matching rows. Stored on the panel so
    /// rebuilds triggered by external mutations (add, delete,
    /// `RadioReference` import) respect the active filter.
    pub filter_text: std::rc::Rc<std::cell::RefCell<String>>,
    /// Category titles the user has manually expanded, tracked
    /// across rebuilds. Distinct from "currently expanded
    /// widgets" because the search path force-opens every
    /// expander — snapshotting widget state on every rebuild
    /// would treat those search-forced opens as manual intent
    /// and keep them open after the search clears. Only updated
    /// when the user toggles an expander while no filter is
    /// active (see `expanded-notify` handler in
    /// `rebuild_bookmark_list`).
    pub manual_expanded: std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
}

impl BookmarksPanel {
    /// Register a callback invoked when the user selects a bookmark.
    pub fn connect_navigate<F: Fn(&Bookmark) + 'static>(&self, f: F) {
        *self.on_navigate.borrow_mut() = Some(Box::new(f));
    }

    /// Register a callback invoked when the user clicks save on the active bookmark.
    pub fn connect_save<F: Fn() + 'static>(&self, f: F) {
        *self.on_save.borrow_mut() = Some(Box::new(f));
    }

    /// Rebuild the flyout's bookmark list from the backing store,
    /// honoring the current search filter. Preferred over calling
    /// [`rebuild_bookmark_list`](super::navigation_panel::rebuild_bookmark_list)
    /// directly — packs up all the shared `Rc` state owned by
    /// this panel so callers only need to hand in the
    /// `NavigationPanel`-owned `name_entry` reference.
    pub fn rebuild(&self, name_entry: &adw::EntryRow) {
        super::navigation_panel::rebuild_bookmark_list(
            &self.bookmark_list,
            &self.bookmark_scroll,
            &self.bookmarks,
            &self.on_navigate,
            &self.active_bookmark,
            name_entry,
            &self.on_save,
            &self.filter_text,
            &self.manual_expanded,
        );
    }
}

/// Build the bookmarks flyout panel.
///
/// Takes a reference to the left sidebar's `name_entry` so the list
/// rebuild path — which lives in this module and is called from
/// both panels — can keep the entry field in sync with the active
/// bookmark. The entry is **create-only context** for the Add
/// Bookmark button: recall populates it as an informational
/// reminder of which bookmark is loaded (and cleared on
/// delete-of-active), but the Add button always pushes a new
/// `Bookmark` — there's no in-place rename path. The entry field
/// belongs to `NavigationPanel` because the Add button is packed
/// with it; this panel just holds a reference.
#[must_use]
pub fn build_bookmarks_panel(name_entry: &adw::EntryRow) -> BookmarksPanel {
    let widget = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .width_request(BOOKMARKS_PANEL_WIDTH_PX)
        .build();

    let heading = gtk4::Label::builder()
        .label("Bookmarks")
        .css_classes(["title-2"])
        .halign(gtk4::Align::Start)
        .build();
    widget.append(&heading);

    // Search entry — live-filters the list via
    // `ListBox::set_filter_func` below. Case-insensitive substring
    // match against the row's title (bookmark name) and subtitle
    // (demod + frequency). `gtk4::SearchEntry` already handles the
    // clear-button affordance and Ctrl+F keyboard shortcut.
    let search_entry = gtk4::SearchEntry::builder()
        .placeholder_text("Search bookmarks")
        .build();
    widget.append(&search_entry);

    let bookmark_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    let bookmark_scroll = gtk4::ScrolledWindow::builder()
        .child(&bookmark_list)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .build();
    widget.append(&bookmark_scroll);

    let bookmarks = std::rc::Rc::new(std::cell::RefCell::new(load_bookmarks()));
    let on_navigate: std::rc::Rc<std::cell::RefCell<Option<NavigationCallback>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let active_bookmark = std::rc::Rc::new(std::cell::RefCell::new(ActiveBookmark::default()));
    let on_save: SaveCallback = std::rc::Rc::new(std::cell::RefCell::new(None));
    let filter_text = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let manual_expanded = std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashSet::<
        String,
    >::new()));

    // Search-changed → update needle + rebuild. We rebuild the
    // list instead of using `ListBox::set_filter_func` because
    // the categorized view uses nested `AdwExpanderRow` widgets:
    // the outer `ListBox` filter function only sees expanders,
    // not the child rows inside them, so it cannot drive child
    // visibility. Rebuilding on keystroke is cheap (dozens of
    // rows in practice) and keeps the filter + grouping logic
    // in one place.
    let filter_for_entry = std::rc::Rc::clone(&filter_text);
    let list_for_entry = bookmark_list.clone();
    let scroll_for_entry = bookmark_scroll.clone();
    let bookmarks_for_entry = std::rc::Rc::clone(&bookmarks);
    let on_navigate_for_entry = std::rc::Rc::clone(&on_navigate);
    let active_for_entry = std::rc::Rc::clone(&active_bookmark);
    let on_save_for_entry = std::rc::Rc::clone(&on_save);
    let manual_expanded_for_entry = std::rc::Rc::clone(&manual_expanded);
    let name_entry_for_entry = name_entry.clone();
    search_entry.connect_search_changed(move |entry| {
        *filter_for_entry.borrow_mut() = entry.text().to_lowercase();
        rebuild_bookmark_list(
            &list_for_entry,
            &scroll_for_entry,
            &bookmarks_for_entry,
            &on_navigate_for_entry,
            &active_for_entry,
            &name_entry_for_entry,
            &on_save_for_entry,
            &filter_for_entry,
            &manual_expanded_for_entry,
        );
    });

    // Seed the list with the restored bookmarks.
    rebuild_bookmark_list(
        &bookmark_list,
        &bookmark_scroll,
        &bookmarks,
        &on_navigate,
        &active_bookmark,
        name_entry,
        &on_save,
        &filter_text,
        &manual_expanded,
    );

    BookmarksPanel {
        widget,
        bookmark_list,
        bookmark_scroll,
        bookmarks,
        active_bookmark,
        on_navigate,
        on_save,
        filter_text,
        manual_expanded,
    }
}
