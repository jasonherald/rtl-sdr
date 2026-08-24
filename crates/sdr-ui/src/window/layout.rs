//! Window chrome: split-view layout, activity bars, resize handles,
//! header bar, and the favorites popover shell.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::{
    AppState, DEFAULT_WIDTH, Rc, SidebarPanels, StatusBar, UiToDsp, adw, gio, glib, header,
    sidebar, spectrum, status_bar,
};

/// Sidebar collapse breakpoint width in pixels.
pub(super) const SIDEBAR_BREAKPOINT_PX: f64 = 800.0;

/// Minimum left-panel width in pixels — narrower than this makes
/// `AdwPreferencesGroup` content wrap awkwardly (design doc §4.4).
pub(super) const LEFT_SIDEBAR_MIN_WIDTH: f64 = 220.0;

/// Minimum right-panel width. The transcript panel's controls
/// (model combo, VAD slider, auto-break sliders) need more breathing
/// room than a preferences row — below this they stack awkwardly
/// and the transcript text view loses usable line width.
pub(super) const RIGHT_SIDEBAR_MIN_WIDTH: f64 = 360.0;

/// Default left-panel width — matches today's sidebar width.
pub(super) const LEFT_SIDEBAR_DEFAULT_WIDTH: f64 = 320.0;

/// Default right-panel width — gives the transcript panel room for
/// its wider controls without the user having to resize on every
/// launch.
pub(super) const RIGHT_SIDEBAR_DEFAULT_WIDTH: f64 = 420.0;

/// How much wider than its default a sidebar may be dragged. 2× the
/// default feels natural — "a little bigger" and "a lot bigger"
/// without letting the panel overrun the spectrum.
pub(super) const SIDEBAR_MAX_WIDTH_MULTIPLIER: f64 = 2.0;

/// Minimum `sidebar-width-fraction` we write. Guards against the
/// `AdwOverlaySplitView` pspec's rejection of exactly 0 and the
/// animator's visual collapse at very small values.
pub(super) const SIDEBAR_FRACTION_MIN: f64 = 0.01;

/// Maximum `sidebar-width-fraction` — symmetric sibling of
/// [`SIDEBAR_FRACTION_MIN`]. Prevents the content area from being
/// squeezed to zero even if a pixel clamp miscomputes.
pub(super) const SIDEBAR_FRACTION_MAX: f64 = 0.99;

/// Handles returned by [`build_layout`] for downstream wiring. Bundled
/// into a struct rather than a tuple because the return list grew past
/// the clippy threshold during the activity-bar scaffolding migration.
pub(super) struct LayoutHandles {
    /// Root horizontal container for the whole window content area.
    pub(super) root: gtk4::Box,
    /// Outer split view — sidebar hosts the left activity stack,
    /// content hosts the nested right split view.
    pub(super) left_split_view: adw::OverlaySplitView,
    /// Inner split view — sidebar hosts the right activity stack
    /// (`sidebar_position=End`), content hosts spectrum + status
    /// + the legacy bookmarks revealer.
    pub(super) right_split_view: adw::OverlaySplitView,
    /// Left activity bar widget + per-entry toggle buttons.
    pub(super) left_activity_bar: sidebar::ActivityBar,
    /// Right activity bar widget + per-entry toggle buttons.
    pub(super) right_activity_bar: sidebar::ActivityBar,
    /// Left panel content switcher — 5 children keyed by entry name.
    pub(super) left_stack: gtk4::Stack,
    /// Right panel content switcher — 1 child keyed `"transcript"`.
    pub(super) right_stack: gtk4::Stack,
    pub(super) panels: SidebarPanels,
    pub(super) spectrum_handle: spectrum::SpectrumHandle,
    pub(super) status_bar: StatusBar,
    pub(super) transcript_panel: sidebar::transcript_panel::TranscriptPanel,
    /// General activity panel — landing view. Hosts band presets
    /// and source as flat `AdwPreferencesGroup`s on an
    /// `AdwPreferencesPage`. Bookmarks live in the right activity
    /// stack (not here); `rtl_tcp` share controls live in the Share
    /// left activity (not here).
    pub(super) general_panel: sidebar::GeneralPanel,
}

/// Build the `AdwOverlaySplitView` with sidebar configuration panels,
/// content, and status bar, returning the full [`LayoutHandles`] set.
pub(super) fn build_layout(
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) -> LayoutHandles {
    // Sidebar panels — constructed flat; each lives in its own
    // activity stack child (no shared scroll wrapper). The
    // General activity composes band presets + source into an
    // `AdwPreferencesPage`; Radio / Audio / Display / Scanner /
    // Share host their respective panel widgets directly until
    // sub-tickets #423-#426 refactor each into the expander-row
    // layout. Bookmarks lives in the right activity stack.
    let panels = sidebar::build_panels();
    sidebar::server_panel::connect_server_panel_persistence(&panels.server, config);

    let general_panel = sidebar::build_general_panel(&panels.navigation, &panels.source);

    // Spectrum display (FFT + waterfall) + status bar.
    let (spectrum_view, spectrum_handle) = spectrum::build_spectrum_view(state.ui_tx.clone());
    spectrum_view.add_css_class("spectrum-area");
    let status_bar = status_bar::build_status_bar();

    let content_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    content_box.append(&spectrum_view);
    content_box.append(&status_bar.widget);

    // Transcript panel — the real widget, not a placeholder. Its
    // root is already an `AdwPreferencesGroup` so it slots straight
    // into the page wrapper in the right stack, inheriting the same
    // chrome as every other activity panel.
    let transcript_panel = sidebar::transcript_panel::build_transcript_panel(config);

    let (left_stack, right_stack) = build_panel_stacks(&panels, &general_panel, &transcript_panel);

    // Explicitly pin the initial visible child so a future
    // additional right-activity inserted before transcript doesn't
    // silently shift what the first `Ctrl+Shift+1` press (or the
    // header transcript button's click) shows. Matches the contract
    // `wire_activity_bar_clicks(..., "transcript")` relies on below.
    right_stack.set_visible_child_name("transcript");

    build_split_views(
        config,
        SplitViewParts {
            panels,
            general_panel,
            transcript_panel,
            left_stack,
            right_stack,
            spectrum_handle,
            status_bar,
        },
        &content_box,
    )
}

/// Drag gesture of [`build_resize_handle`]: width follows the pointer
/// from the drag-begin fraction, clamped to `[min_px, max_px]`, and
/// the final width persists at drag-end. Split out per the 50-NLOC
/// gate (#817).
#[allow(clippy::too_many_arguments)]
fn wire_resize_drag_gesture(
    handle: &gtk4::Box,
    split_view: &adw::OverlaySplitView,
    direction: ResizeDirection,
    min_px: f64,
    max_px: f64,
    start_fraction: &std::rc::Rc<std::cell::Cell<f64>>,
    save_width_px: &std::rc::Rc<dyn Fn(u32)>,
) {
    let drag_gesture = gtk4::GestureDrag::new();

    let split_view_weak = split_view.downgrade();
    let start_fraction_begin = std::rc::Rc::clone(start_fraction);
    drag_gesture.connect_drag_begin(move |_, _, _| {
        if let Some(sv) = split_view_weak.upgrade() {
            start_fraction_begin.set(sv.sidebar_width_fraction());
        }
    });

    let split_view_weak = split_view.downgrade();
    let start_fraction_update = std::rc::Rc::clone(start_fraction);
    drag_gesture.connect_drag_update(move |_, offset_x, _| {
        let Some(sv) = split_view_weak.upgrade() else {
            return;
        };
        let sv_w = f64::from(sv.width());
        if sv_w <= 0.0 {
            return;
        }
        let start_px = start_fraction_update.get() * sv_w;
        let signed_offset = match direction {
            ResizeDirection::RightGrowsSidebar => offset_x,
            ResizeDirection::LeftGrowsSidebar => -offset_x,
        };
        let new_px = (start_px + signed_offset).clamp(min_px, max_px);
        // Fraction pspec is `[0, 1]`; guard against 0 which the
        // widget treats as "collapsed" at the animator level.
        let new_fraction = (new_px / sv_w).clamp(SIDEBAR_FRACTION_MIN, SIDEBAR_FRACTION_MAX);
        sv.set_sidebar_width_fraction(new_fraction);
    });

    wire_resize_drag_end(&drag_gesture, split_view, save_width_px);
    handle.add_controller(drag_gesture);
}

/// Drag-end persistence of the resize gesture. Split out per the
/// 50-NLOC gate (#817).
fn wire_resize_drag_end(
    drag_gesture: &gtk4::GestureDrag,
    split_view: &adw::OverlaySplitView,
    save_width_px: &std::rc::Rc<dyn Fn(u32)>,
) {
    let split_view_weak = split_view.downgrade();
    let save_end = std::rc::Rc::clone(save_width_px);
    drag_gesture.connect_drag_end(move |_, _, _| {
        let Some(sv) = split_view_weak.upgrade() else {
            return;
        };
        let sv_w = f64::from(sv.width());
        if sv_w <= 0.0 {
            return;
        }
        let final_px = sv.sidebar_width_fraction() * sv_w;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let px = final_px.round().max(0.0) as u32;
        save_end(px);
    });
}

/// Double-click-to-reset gesture of [`build_resize_handle`] — the GTK
/// paned-divider convention. Split out per the 50-NLOC gate (#817).
fn wire_resize_double_click(
    handle: &gtk4::Box,
    split_view: &adw::OverlaySplitView,
    default_px: f64,
    save_width_px: &std::rc::Rc<dyn Fn(u32)>,
) {
    // Double-click = reset to default width. Matches the GTK paned-
    // divider convention users expect ("I messed up my drag, take
    // me back"). A single click does nothing — the drag gesture
    // already handles press/release.
    let click_gesture = gtk4::GestureClick::new();
    click_gesture.set_button(gtk4::gdk::BUTTON_PRIMARY);
    let split_view_weak = split_view.downgrade();
    let save_click = std::rc::Rc::clone(save_width_px);
    click_gesture.connect_released(move |_, n_press, _, _| {
        if n_press != 2 {
            return;
        }
        let Some(sv) = split_view_weak.upgrade() else {
            return;
        };
        let sv_w = f64::from(sv.width());
        if sv_w <= 0.0 {
            return;
        }
        let fraction = (default_px / sv_w).clamp(SIDEBAR_FRACTION_MIN, SIDEBAR_FRACTION_MAX);
        sv.set_sidebar_width_fraction(fraction);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let px = default_px.round().max(0.0) as u32;
        save_click(px);
    });
    handle.add_controller(click_gesture);
}

/// Right (inner) split view + its resize handle and sidebar wrap.
/// Split out of [`build_split_views`] per the 50-NLOC gate (#817).
fn build_right_split(
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    right_stack: &gtk4::Stack,
    content_box: &gtk4::Box,
) -> adw::OverlaySplitView {
    let right_split_view = adw::OverlaySplitView::builder()
        .sidebar_position(gtk4::PackType::End)
        .content(content_box)
        .show_sidebar(false)
        .min_sidebar_width(RIGHT_SIDEBAR_MIN_WIDTH)
        .max_sidebar_width(RIGHT_SIDEBAR_DEFAULT_WIDTH * SIDEBAR_MAX_WIDTH_MULTIPLIER)
        .sidebar_width_fraction(RIGHT_SIDEBAR_DEFAULT_WIDTH / f64::from(DEFAULT_WIDTH))
        .build();

    // Compose the right sidebar with its resize handle on the
    // leading edge (the boundary with the content area). Dragging
    // the handle LEFT widens the sidebar; drag-end persists the
    // new pixel width; double-click resets to the default.
    let config_right_resize = std::sync::Arc::clone(config);
    let save_right_width: std::rc::Rc<dyn Fn(u32)> = std::rc::Rc::new(move |px| {
        sidebar::activity_bar::save_right_width_px(&config_right_resize, px);
    });
    let right_handle = build_resize_handle(
        &right_split_view,
        ResizeDirection::LeftGrowsSidebar,
        RIGHT_SIDEBAR_MIN_WIDTH,
        RIGHT_SIDEBAR_DEFAULT_WIDTH * SIDEBAR_MAX_WIDTH_MULTIPLIER,
        RIGHT_SIDEBAR_DEFAULT_WIDTH,
        &save_right_width,
    );
    let right_sidebar_wrap = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .hexpand(true)
        .vexpand(true)
        .build();
    right_sidebar_wrap.append(&right_handle);
    right_sidebar_wrap.append(right_stack);
    right_split_view.set_sidebar(Some(&right_sidebar_wrap));

    right_split_view
}

/// Wrap an `AdwPreferencesGroup` in its own `AdwPreferencesPage`
/// so every activity stack child inherits the same margin/spacing
/// rhythm as the General panel (Apple-style header padding + group
/// titles). `AdwPreferencesPage` is itself scrollable internally,
/// so no extra `GtkScrolledWindow` wrapper is needed.
pub(super) fn page_from_group(group: &adw::PreferencesGroup) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.add(group);
    page
}

/// Apply a pixel width to an `AdwOverlaySplitView` sidebar after
/// the split view has a real allocation. A single `notify::width`
/// handler fires once the first non-zero width lands, converts
/// the target pixels into the `[0, 1]` fraction the widget
/// accepts, applies it, and then disarms (`applied` flag) so
/// subsequent width notifications (window resize) leave the
/// sidebar's fractional preference alone.
///
/// `saved_px == Some(px)` uses the persisted value; `None` falls
/// back to `default_px`. Both cases go through the same
/// post-allocation conversion so the advertised pixel default
/// actually lands — builder-time fractions are derived from
/// `DEFAULT_WIDTH` and evaluate against the split view's
/// narrower-than-window allocation, so without this the fresh-
/// session defaults under-shoot their targets.
pub(super) fn apply_sidebar_width(
    split_view: &adw::OverlaySplitView,
    saved_px: Option<u32>,
    default_px: u32,
) {
    let target_px = saved_px.unwrap_or(default_px);
    let applied: std::rc::Rc<std::cell::Cell<bool>> = std::rc::Rc::new(std::cell::Cell::new(false));
    split_view.connect_notify_local(Some("width"), move |sv, _| {
        if applied.get() {
            return;
        }
        let sv_w = f64::from(sv.width());
        if sv_w <= 0.0 {
            return;
        }
        let fraction =
            (f64::from(target_px) / sv_w).clamp(SIDEBAR_FRACTION_MIN, SIDEBAR_FRACTION_MAX);
        sv.set_sidebar_width_fraction(fraction);
        applied.set(true);
    });
}

/// Width of the invisible drag strip at a sidebar's inner edge
/// (design doc §4.4 calls for "thin (4–6 px)"). 6 px gives the
/// user a forgiving hit target without stealing pixels from the
/// panel content.
pub(super) const RESIZE_HANDLE_WIDTH_PX: i32 = 6;

/// Which direction of drag grows the sidebar. The LEFT split
/// view's sidebar sits on the leading edge — dragging the handle
/// right pushes the sidebar-content boundary right and widens the
/// sidebar. The RIGHT split view's sidebar sits on the trailing
/// edge (`sidebar_position=End`) — the handle is on its leading
/// edge, and dragging LEFT widens the sidebar.
#[derive(Clone, Copy, Debug)]
pub(super) enum ResizeDirection {
    /// Positive `offset_x` widens the sidebar (left split view).
    RightGrowsSidebar,
    /// Negative `offset_x` widens the sidebar (right split view).
    LeftGrowsSidebar,
}

/// Build an invisible drag-handle widget sized to
/// [`RESIZE_HANDLE_WIDTH_PX`] and wire it to resize an
/// `AdwOverlaySplitView` sidebar. Live-resizes during drag,
/// persists the final width on drag-end via `save_width_px`,
/// and resets to `default_px` on a left-button double-click
/// (standard GTK paned-divider pattern).
///
/// `AdwOverlaySplitView` only exposes `sidebar-width-fraction`
/// (range `[0, 1]`); pixel min/max/default are converted to the
/// fraction against the split view's live allocation every time
/// the gesture fires, so the clamp reacts correctly to window
/// resizes.
pub(super) fn build_resize_handle(
    split_view: &adw::OverlaySplitView,
    direction: ResizeDirection,
    min_px: f64,
    max_px: f64,
    default_px: f64,
    save_width_px: &std::rc::Rc<dyn Fn(u32)>,
) -> gtk4::Box {
    let handle = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .width_request(RESIZE_HANDLE_WIDTH_PX)
        .css_classes(["sidebar-resize-handle"])
        .build();
    if let Some(cursor) = gtk4::gdk::Cursor::from_name("col-resize", None) {
        handle.set_cursor(Some(&cursor));
    }

    // Captured at drag-begin so every `drag-update` computes the
    // new width from the stable starting fraction rather than
    // integrating floating-point deltas. Without this the gesture
    // would drift 1–2 px per drag cycle.
    let start_fraction: std::rc::Rc<std::cell::Cell<f64>> =
        std::rc::Rc::new(std::cell::Cell::new(0.0));

    // Gesture closures capture `split_view` via `WeakRef` to
    // break an otherwise-real retain cycle: `split_view` owns
    // `sidebar`, `sidebar` owns `handle`, `handle` owns the
    // gesture controllers, the controllers own their closures,
    // and a strong `split_view.clone()` inside the closures would
    // close the loop and leak the whole sidebar subtree on window
    // teardown. Matches the `glib::WeakRef` idiom used elsewhere
    // in this file (scanner force-disable, RTL-TCP handlers).
    wire_resize_drag_gesture(
        &handle,
        split_view,
        direction,
        min_px,
        max_px,
        &start_fraction,
        save_width_px,
    );
    wire_resize_double_click(&handle, split_view, default_px, save_width_px);

    handle
}

/// Build the sidebar toggle button bound to the split view.
pub(super) fn build_sidebar_toggle(split_view: &adw::OverlaySplitView) -> gtk4::ToggleButton {
    let toggle = gtk4::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Toggle sidebar")
        .active(true)
        .build();

    toggle.connect_toggled(glib::clone!(
        #[weak]
        split_view,
        move |btn| {
            split_view.set_show_sidebar(btn.is_active());
        }
    ));

    toggle
}

/// Handles handed back from `build_header_bar` for the `rtl_tcp`
/// favorites slide-out. The `button` is packed into the header bar
/// and drops its popover on click; the `list` is the scrollable
/// `ListBox` inside that popover — `connect_rtl_tcp_discovery`
/// clears + re-populates it when the favorites map changes. The
/// `empty_label` is shown when the list is empty so the user sees
/// "No pinned servers yet" instead of a blank popover.
pub(super) struct FavoritesHeaderHandle {
    pub(super) button: gtk4::MenuButton,
    pub(super) popover: gtk4::Popover,
    pub(super) list: gtk4::ListBox,
    pub(super) empty_label: gtk4::Label,
}

/// Build the `AdwHeaderBar` with play/stop, frequency selector, demod selector,
/// and volume control.
///
/// Returns the header bar, play button, demod dropdown, and frequency selector
/// (for shortcuts, status bar wiring, and frequency change callbacks).
#[allow(
    clippy::too_many_lines,
    reason = "widget-assembly — splitting scatters one-time wire-up across helpers without readability win"
)]
pub(super) fn build_header_bar(
    sidebar_toggle: &gtk4::ToggleButton,
    state: &Rc<AppState>,
) -> (
    adw::HeaderBar,
    gtk4::ToggleButton,
    gtk4::DropDown,
    header::frequency_selector::FrequencySelector,
    gtk4::Button,
    gtk4::Button,
    gtk4::ScaleButton,
    FavoritesHeaderHandle,
) {
    let play_button = build_play_button(state);

    // Frequency selector as the title widget.
    // NOTE: The frequency-changed callback is connected later in `build_window`
    // so it can also update the status bar.
    let freq_selector = header::build_frequency_selector();

    // Demod selector dropdown. The DSP-dispatch handler used to
    // live here, but it would race the scanner force-disable
    // that runs from build_window's handler — scanner would hear
    // SetDemodMode first, then the stop command. Dispatch wiring
    // moved to build_window so force-disable + send_dsp can run
    // in a single handler in the right order.
    let (demod_dropdown, _demod_mode_cell) = header::build_demod_selector();

    let volume_button = build_volume_button();

    let menu_button = build_menu_button();

    let header = adw::HeaderBar::builder()
        .title_widget(&freq_selector.widget)
        .build();

    header.pack_start(sidebar_toggle);
    header.pack_start(&play_button);
    header.pack_start(&demod_dropdown);
    // Waterfall screenshot button
    let screenshot_button = gtk4::Button::builder()
        .icon_name("camera-photo-symbolic")
        .tooltip_text("Export waterfall to PNG")
        .build();

    // RadioReference frequency browser button
    let rr_button = gtk4::Button::builder()
        .icon_name("network-wireless-symbolic")
        .tooltip_text("RadioReference Frequency Browser")
        .visible(crate::preferences::accounts_page::has_rr_credentials())
        .build();

    // Favorites slide-out button — opens a popover listing the
    // user's pinned `rtl_tcp` servers. Entries populated
    // dynamically by `connect_rtl_tcp_discovery`. MenuButton
    // auto-toggles and handles click-outside dismissal.
    let favorites_handle = build_favorites_header();

    header.pack_end(&menu_button);
    header.pack_end(&volume_button);
    header.pack_end(&rr_button);
    header.pack_end(&screenshot_button);
    header.pack_end(&favorites_handle.button);

    (
        header,
        play_button,
        demod_dropdown.clone(),
        freq_selector,
        screenshot_button,
        rr_button,
        volume_button,
        favorites_handle,
    )
}

/// Width of the favorites popover's scrollable list. Wide enough
/// for a `rtl_tcp://hostname.local.:12345 — R820T (29 gains)`
/// subtitle without wrapping.
pub(super) const FAVORITES_POPOVER_WIDTH_PX: i32 = 420;

/// Max height of the favorites popover's scrollable list. Caps the
/// popover so a large favorites set doesn't paint past the bottom
/// of the window; the internal `ScrolledWindow` handles overflow.
pub(super) const FAVORITES_POPOVER_HEIGHT_PX: i32 = 360;

/// Favorites list + scroll + empty-state label of
/// [`build_favorites_header`]. Split out per the 50-NLOC gate (#817).
fn build_favorites_list() -> (gtk4::ListBox, gtk4::ScrolledWindow, gtk4::Label) {
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(6)
        .margin_end(6)
        .margin_bottom(6)
        .build();

    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .max_content_height(FAVORITES_POPOVER_HEIGHT_PX)
        .propagate_natural_height(true)
        .child(&list)
        .build();

    let empty_label = gtk4::Label::builder()
        .label("No pinned servers yet.\n\nStar a discovered server to pin it here.")
        .justify(gtk4::Justification::Center)
        .wrap(true)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .css_classes(["dim-label"])
        .build();

    (list, scroll, empty_label)
}

/// Build the header-bar favorites button + its popover contents.
/// The popover hosts a `ListBox` (populated by
/// `connect_rtl_tcp_discovery` whenever the favorites map mutates)
/// wrapped in a capped `ScrolledWindow`. The empty-state label is
/// shown when the list is empty and hidden when it's populated —
/// callers are responsible for that toggle alongside row rebuilds.
pub(super) fn build_favorites_header() -> FavoritesHeaderHandle {
    let popover = gtk4::Popover::builder()
        .autohide(true)
        .has_arrow(true)
        .width_request(FAVORITES_POPOVER_WIDTH_PX)
        .build();
    popover.add_css_class("menu");

    let title = gtk4::Label::builder()
        .label("Pinned servers")
        .halign(gtk4::Align::Start)
        .margin_start(12)
        .margin_top(12)
        .margin_bottom(6)
        .css_classes(["heading"])
        .build();

    let (list, scroll, empty_label) = build_favorites_list();

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(0)
        .build();
    content.append(&title);
    content.append(&empty_label);
    content.append(&scroll);
    popover.set_child(Some(&content));

    let button = gtk4::MenuButton::builder()
        .icon_name("starred-symbolic")
        .tooltip_text("Pinned rtl_tcp servers")
        .popover(&popover)
        .build();
    // Screen-reader name. Tooltips aren't announced by most
    // ATs — icon-only controls need an explicit accessible
    // label via the GtkAccessible `Label` property.
    button.update_property(&[gtk4::accessible::Property::Label("Pinned servers menu")]);

    FavoritesHeaderHandle {
        button,
        popover,
        list,
        empty_label,
    }
}

/// Build the app menu button with Preferences / Keyboard Shortcuts / About / Quit actions.
pub(super) fn build_menu_button() -> gtk4::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("_Preferences"), Some("app.preferences"));
    menu.append(Some("_Keyboard Shortcuts"), Some("win.show-help-overlay"));
    menu.append(Some("_About SDR-RS"), Some("app.about"));
    menu.append(Some("_Quit"), Some("app.quit"));

    gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main menu")
        .build()
}

/// Wrap header and content in an `AdwToolbarView`.
pub(super) fn build_toolbar_view(header: &adw::HeaderBar, content: &gtk4::Box) -> adw::ToolbarView {
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(header);
    toolbar_view.set_content(Some(content));
    toolbar_view
}

/// Wire click handlers on every button of a multi-activity bar so:
///
/// - Clicking a *different* button swaps the stack's visible child
///   and forces the split view's sidebar open.
/// - Clicking the *currently-selected* button keeps that button
///   visually selected (design doc §4.2 — the user's mental model is
///   "I'm still in Radio, I just closed the panel for a second") and
///   toggles the split view's sidebar show/hide.
///
/// The `:checked` CSS pseudo-class (driven by `ToggleButton::active`)
/// renders the accent tint — no manual CSS class juggling needed.
///
/// Mutual exclusion is enforced manually rather than via
/// `ToggleButton::set_group`; see `sidebar::activity_bar` module docs.
///
/// Only suitable for bars with more than one entry. Single-button
/// bars (like the right transcript bar today) wire `active` directly
/// to `show_sidebar` — there's no "select vs. toggle panel"
/// distinction to preserve.
pub(super) fn wire_activity_bar_clicks(
    bar: &sidebar::ActivityBar,
    stack: &gtk4::Stack,
    split_view: &adw::OverlaySplitView,
) {
    for (&name, btn) in &bar.buttons {
        let bar_buttons: Vec<(&'static str, glib::WeakRef<gtk4::ToggleButton>)> = bar
            .buttons
            .iter()
            .map(|(n, b)| (*n, b.downgrade()))
            .collect();
        let stack_weak = stack.downgrade();
        let split_view_weak = split_view.downgrade();
        btn.connect_clicked(move |clicked_btn| {
            // The stack itself is the single source of truth for
            // "which activity is selected" — no shadow copy to
            // drift when another code path (keyboard shortcut,
            // header button) swaps the visible child. Per CR
            // round 1 on PR #844 and the crate's no-Rc<RefCell>
            // guidance.
            let already_selected = stack_weak
                .upgrade()
                .and_then(|stk| stk.visible_child_name())
                .is_some_and(|current| current == name);
            if already_selected {
                // Clicking the already-selected icon toggles the
                // panel open/closed. The icon's `active` property
                // tracks the panel's NEW visibility — active
                // when shown, inactive when hidden — so the
                // highlight always reflects "this panel is on
                // screen right now". Per issue #518: the earlier
                // `set_active(true)` unconditionally re-asserted
                // the highlight even after a close, which was
                // misleading (icon glowed but panel was gone).
                //
                // GTK's default click handler already flipped
                // `active`, so we'd otherwise see two flips per
                // click. Setting it explicitly here pins the
                // icon state to the resolved sidebar visibility
                // regardless of GTK's intermediate flip.
                if let Some(sv) = split_view_weak.upgrade() {
                    let new_shown = !sv.shows_sidebar();
                    sv.set_show_sidebar(new_shown);
                    clicked_btn.set_active(new_shown);
                } else {
                    // Split view torn down — preserve the
                    // previous "icon stays active" behaviour
                    // since we can't observe panel state.
                    clicked_btn.set_active(true);
                }
            } else {
                // Click on a different activity — deselect siblings,
                // swap stack child, open panel.
                for (other_name, weak) in &bar_buttons {
                    if let Some(other) = weak.upgrade()
                        && *other_name != name
                        && other.is_active()
                    {
                        other.set_active(false);
                    }
                }
                clicked_btn.set_active(true);
                if let Some(stk) = stack_weak.upgrade() {
                    stk.set_visible_child_name(name);
                }
                if let Some(sv) = split_view_weak.upgrade() {
                    sv.set_show_sidebar(true);
                }
            }
        });
    }
}

/// Wire a `connect_show_sidebar_notify` that keeps the activity
/// bar's icon active state in sync with the sidebar's visibility
/// regardless of who toggled it. Companion to
/// [`wire_activity_bar_clicks`] — that handles the user-clicks-
/// the-icon path; this handles every OTHER way `show-sidebar`
/// can flip (header sidebar button, F9 keyboard shortcut, the
/// breakpoint collapsing the sidebar at narrow widths, future
/// programmatic toggles).
///
/// Without this, an external toggle would leave the icon stale —
/// e.g. user opens panel via icon (icon active), closes panel
/// via header button (sidebar gone, icon still active). Per
/// issue #518.
pub(super) fn sync_activity_bar_to_sidebar_visibility(
    split_view: &adw::OverlaySplitView,
    bar: &sidebar::ActivityBar,
    stack: &gtk4::Stack,
) {
    let buttons: Vec<(&'static str, glib::WeakRef<gtk4::ToggleButton>)> = bar
        .buttons
        .iter()
        .map(|(n, b)| (*n, b.downgrade()))
        .collect();
    let stack_weak = stack.downgrade();
    split_view.connect_show_sidebar_notify(move |sv| {
        let shown = sv.shows_sidebar();
        let visible_name = stack_weak
            .upgrade()
            .and_then(|s| s.visible_child_name().map(|gs| gs.to_string()));
        for (name, weak) in &buttons {
            let Some(btn) = weak.upgrade() else { continue };
            let should_be_active = shown && visible_name.as_deref() == Some(*name);
            if btn.is_active() != should_be_active {
                btn.set_active(should_be_active);
            }
        }
    });
}

/// Create a breakpoint that collapses both sidebars below
/// `SIDEBAR_BREAKPOINT_PX`. Both split views flip to overlay mode at
/// narrow widths so the spectrum keeps its minimum real estate.
pub(super) fn build_breakpoint(
    left_split_view: &adw::OverlaySplitView,
    right_split_view: &adw::OverlaySplitView,
) -> adw::Breakpoint {
    let condition = adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        SIDEBAR_BREAKPOINT_PX,
        adw::LengthUnit::Px,
    );

    let breakpoint = adw::Breakpoint::new(condition);
    breakpoint.add_setter(left_split_view, "collapsed", Some(&true.into()));
    breakpoint.add_setter(right_split_view, "collapsed", Some(&true.into()));

    breakpoint
}

/// Left/right activity panel stacks (names are config keys; design doc §5).
/// Split out per the 50-NLOC gate (#817).
fn build_panel_stacks(
    panels: &SidebarPanels,
    general_panel: &sidebar::GeneralPanel,
    transcript_panel: &sidebar::transcript_panel::TranscriptPanel,
) -> (gtk4::Stack, gtk4::Stack) {
    // Left panel stack — one real panel widget per activity. General
    // hosts the composed `GeneralPanel` (band presets + bookmarks +
    // source + rtl_tcp share as expander rows); Radio / Audio /
    // Display / Scanner host their existing panel widget wrapped in
    // a scroll so long pages can scroll internally without resizing
    // the panel's width (design doc §2.4). Sub-tickets #423-#426
    // later refactor each of those widgets into the expander-row
    // layout the General panel demonstrates; the `name` strings MUST
    // remain stable because they're the config-persistence keys
    // (§5 of the design doc).
    let left_stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::None)
        .hexpand(true)
        .vexpand(true)
        .build();
    left_stack.add_named(&general_panel.widget, Some("general"));
    left_stack.add_named(&panels.radio.widget, Some("radio"));
    left_stack.add_named(&panels.audio.widget, Some("audio"));
    left_stack.add_named(&panels.display.widget, Some("display"));
    left_stack.add_named(&panels.scanner.widget, Some("scanner"));
    left_stack.add_named(&page_from_group(&panels.server.widget), Some("share"));
    left_stack.add_named(&panels.satellites.widget, Some("satellites"));
    left_stack.add_named(&panels.aviation.widget, Some("aviation"));

    // Right panel stack — single child today, hosts the real
    // transcript widget (not a placeholder) so transcription keeps
    // working during the migration window.
    let right_stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::None)
        .hexpand(true)
        .vexpand(true)
        .build();
    right_stack.add_named(
        &page_from_group(&transcript_panel.widget),
        Some("transcript"),
    );
    right_stack.add_named(
        &page_from_group(&panels.bookmarks.widget),
        Some("bookmarks"),
    );

    (left_stack, right_stack)
}

/// Nested `AdwOverlaySplitViews` + resize handles + activity bars around the content box.
/// Split out per the 50-NLOC gate (#817).
/// Owned widgets `build_layout` hands to [`build_split_views`] —
/// the pieces that end up inside [`LayoutHandles`] after the split
/// views wrap them.
struct SplitViewParts {
    panels: SidebarPanels,
    general_panel: sidebar::GeneralPanel,
    transcript_panel: sidebar::transcript_panel::TranscriptPanel,
    left_stack: gtk4::Stack,
    right_stack: gtk4::Stack,
    spectrum_handle: spectrum::SpectrumHandle,
    status_bar: StatusBar,
}

fn build_split_views(
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    parts: SplitViewParts,
    content_box: &gtk4::Box,
) -> LayoutHandles {
    let SplitViewParts {
        panels,
        general_panel,
        transcript_panel,
        left_stack,
        right_stack,
        spectrum_handle,
        status_bar,
    } = parts;
    // Inner (right) split view — sidebar sits on the trailing edge
    // so the right activity bar is the rightmost element on-screen.
    //
    // `sidebar_width_fraction` is `[0, 1]` regardless of the
    // `sidebar-width-unit` we set; the unit only changes how
    // `min`/`max-sidebar-width` are interpreted. Passing a pixel
    // value as the fraction panics at property-set even with
    // `unit = Px` (verified on libadwaita 1.9). So the default
    // here is a fraction; under nested splits its pixel result
    // is approximate, and `min-sidebar-width` clamps the transcript
    // panel up to its 360 px floor when the math would otherwise
    // leave it narrower. User-driven resize + persistence come from
    // the drag handle wired below (#429).
    let right_split_view = build_right_split(config, &right_stack, content_box);

    let left_split_view = build_left_split(config, &left_stack, &right_split_view);

    let left_activity_bar =
        sidebar::build_activity_bar(sidebar::LEFT_ACTIVITIES, sidebar::ActivityBarSide::Left);
    let right_activity_bar =
        sidebar::build_activity_bar(sidebar::RIGHT_ACTIVITIES, sidebar::ActivityBarSide::Right);

    let root = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .hexpand(true)
        .vexpand(true)
        .build();
    root.append(&left_activity_bar.widget);
    root.append(&left_split_view);
    root.append(&right_activity_bar.widget);

    LayoutHandles {
        root,
        left_split_view,
        right_split_view,
        left_activity_bar,
        right_activity_bar,
        left_stack,
        right_stack,
        panels,
        spectrum_handle,
        status_bar,
        transcript_panel,
        general_panel,
    }
}

/// Volume `ScaleButton` with a11y label (value wiring lives in
/// `build_window`). Split out per the 50-NLOC gate (#817).
fn build_volume_button() -> gtk4::ScaleButton {
    // Volume button (ScaleButton with audio icons)
    let volume_button = gtk4::ScaleButton::new(
        0.0,
        1.0,
        0.05,
        &[
            "audio-volume-muted-symbolic",
            "audio-volume-low-symbolic",
            "audio-volume-medium-symbolic",
            "audio-volume-high-symbolic",
        ],
    );
    // Initial value + `connect_value_changed` handler are wired in
    // `build_window` after `connect_audio_panel` runs, so the
    // persistence + audio-panel mirror rely on the full handle set.
    volume_button.set_tooltip_text(Some("Volume"));
    // Explicit accessibility label — tooltip text alone isn't
    // announced reliably by screen readers for icon-only header
    // controls (same idiom as the bookmarks / transcript / pinned-
    // servers buttons).
    volume_button.update_property(&[gtk4::accessible::Property::Label("Volume")]);

    volume_button
}

/// Play/stop toggle + its DSP dispatch. Split out per the 50-NLOC gate (#817).
fn build_play_button(state: &Rc<AppState>) -> gtk4::ToggleButton {
    // Play/stop button
    let play_button = gtk4::ToggleButton::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text("Start / Stop")
        .css_classes(["play-button"])
        .build();

    // Connect play/stop button to DSP
    let state_play = Rc::clone(state);
    play_button.connect_toggled(move |btn| {
        if btn.is_active() {
            btn.set_icon_name("media-playback-stop-symbolic");
            state_play.is_running.set(true);
            state_play.send_dsp(UiToDsp::Start);
        } else {
            btn.set_icon_name("media-playback-start-symbolic");
            state_play.is_running.set(false);
            state_play.send_dsp(UiToDsp::Stop);
        }
    });

    play_button
}

/// Left (outer) split view + its resize handle and sidebar wrap.
/// Split out per the 50-NLOC gate (#817).
fn build_left_split(
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    left_stack: &gtk4::Stack,
    right_split_view: &adw::OverlaySplitView,
) -> adw::OverlaySplitView {
    // Outer (left) split view — sidebar hosts the left activity
    // stack. Starts open with "general" visible so a fresh launch
    // lands on the General panel instead of an empty frame.
    let left_split_view = adw::OverlaySplitView::builder()
        .content(right_split_view)
        .show_sidebar(true)
        .min_sidebar_width(LEFT_SIDEBAR_MIN_WIDTH)
        .max_sidebar_width(LEFT_SIDEBAR_DEFAULT_WIDTH * SIDEBAR_MAX_WIDTH_MULTIPLIER)
        .sidebar_width_fraction(LEFT_SIDEBAR_DEFAULT_WIDTH / f64::from(DEFAULT_WIDTH))
        .build();

    // Compose the left sidebar with its resize handle on the
    // trailing edge. Dragging the handle RIGHT widens the sidebar.
    let config_left_resize = std::sync::Arc::clone(config);
    let save_left_width: std::rc::Rc<dyn Fn(u32)> = std::rc::Rc::new(move |px| {
        sidebar::activity_bar::save_left_width_px(&config_left_resize, px);
    });
    let left_handle = build_resize_handle(
        &left_split_view,
        ResizeDirection::RightGrowsSidebar,
        LEFT_SIDEBAR_MIN_WIDTH,
        LEFT_SIDEBAR_DEFAULT_WIDTH * SIDEBAR_MAX_WIDTH_MULTIPLIER,
        LEFT_SIDEBAR_DEFAULT_WIDTH,
        &save_left_width,
    );
    let left_sidebar_wrap = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .hexpand(true)
        .vexpand(true)
        .build();
    left_sidebar_wrap.append(left_stack);
    left_sidebar_wrap.append(&left_handle);
    left_split_view.set_sidebar(Some(&left_sidebar_wrap));
    left_stack.set_visible_child_name("general");

    left_split_view
}
