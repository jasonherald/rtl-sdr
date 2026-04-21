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
//! transition matches the transcript pattern (300 ms slide from the
//! right edge of the content area).
//!
//! This is a scaffolding stub — the bookmark list itself still lives
//! in `NavigationPanel` for now. Subsequent commits on this branch
//! migrate the list + row actions over and add search + category
//! grouping.

use gtk4::prelude::*;

/// Default width of the bookmarks flyout, in pixels. Wide enough for
/// a long bookmark nickname + a recall button + a delete affordance
/// without wrapping. Matches the transcript revealer's width request
/// so the two side panels have consistent visual weight when both
/// are open.
pub const BOOKMARKS_PANEL_WIDTH_PX: i32 = 360;

/// Widget handles exposed from the bookmarks flyout — the root
/// container for packing into the revealer, and references that
/// the window wiring layer needs (list widget for search/filter
/// rebinds in subsequent commits, active-bookmark state for
/// highlight).
pub struct BookmarksPanel {
    /// Root container for the flyout — packed into the revealer.
    pub widget: gtk4::Box,
}

/// Build an empty bookmarks flyout panel. The list content is still
/// rendered in the left-sidebar `NavigationPanel` for the duration
/// of this scaffolding commit; the next commit on this branch
/// relocates it here.
#[must_use]
pub fn build_bookmarks_panel() -> BookmarksPanel {
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

    // Placeholder caption while the list migration is pending.
    // Removed when the list lands in the follow-up commit.
    let placeholder = gtk4::Label::builder()
        .label("Bookmark list moves here in the next commit.")
        .css_classes(["dim-label"])
        .wrap(true)
        .halign(gtk4::Align::Start)
        .build();
    widget.append(&placeholder);

    BookmarksPanel { widget }
}
