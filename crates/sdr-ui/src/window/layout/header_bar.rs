//! Header-bar construction: the `AdwHeaderBar` and its buttons
//! (play/stop, volume, sidebar toggles, favorites popover, app menu).
//! Split out of `window/layout.rs` per the Codacy 500-NLOC file gate
//! on PR #844.

use gtk4::prelude::*;

use super::super::{AppState, Rc, UiToDsp, adw, gio, glib, header};

/// Build the sidebar toggle button bound to the split view.
pub(in crate::window) fn build_sidebar_toggle(
    split_view: &adw::OverlaySplitView,
) -> gtk4::ToggleButton {
    let toggle = gtk4::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Toggle sidebar")
        .active(true)
        .build();
    toggle.update_property(&[gtk4::accessible::Property::Label("Toggle sidebar")]);

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
pub(in crate::window) struct FavoritesHeaderHandle {
    pub(in crate::window) button: gtk4::MenuButton,
    pub(in crate::window) popover: gtk4::Popover,
    pub(in crate::window) list: gtk4::ListBox,
    pub(in crate::window) empty_label: gtk4::Label,
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
/// Named handles out of [`build_header_bar`]. A struct rather than a
/// positional tuple so an insertion or reorder can't silently swap
/// two same-typed widgets (`screenshot_button` and `rr_button` are
/// both `gtk4::Button`). Per CR round 2 on PR #844.
pub(in crate::window) struct HeaderBarHandles {
    pub(in crate::window) header: adw::HeaderBar,
    pub(in crate::window) play_button: gtk4::ToggleButton,
    pub(in crate::window) demod_dropdown: gtk4::DropDown,
    pub(in crate::window) freq_selector: header::frequency_selector::FrequencySelector,
    pub(in crate::window) screenshot_button: gtk4::Button,
    pub(in crate::window) rr_button: gtk4::Button,
    pub(in crate::window) volume_button: gtk4::ScaleButton,
    pub(in crate::window) favorites_handle: FavoritesHeaderHandle,
}

pub(in crate::window) fn build_header_bar(
    sidebar_toggle: &gtk4::ToggleButton,
    state: &Rc<AppState>,
) -> HeaderBarHandles {
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
    // Explicit accessibility label — tooltips alone aren't announced
    // reliably by screen readers for icon-only controls. Per CR
    // round 2 on PR #844 (same idiom as the volume / favorites
    // buttons below).
    screenshot_button
        .update_property(&[gtk4::accessible::Property::Label("Export waterfall to PNG")]);

    // RadioReference frequency browser button
    let rr_button = gtk4::Button::builder()
        .icon_name("network-wireless-symbolic")
        .tooltip_text("RadioReference Frequency Browser")
        .visible(crate::preferences::accounts_page::has_rr_credentials())
        .build();
    rr_button.update_property(&[gtk4::accessible::Property::Label(
        "RadioReference frequency browser",
    )]);

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

    HeaderBarHandles {
        header,
        play_button,
        demod_dropdown: demod_dropdown.clone(),
        freq_selector,
        screenshot_button,
        rr_button,
        volume_button,
        favorites_handle,
    }
}

/// Width of the favorites popover's scrollable list. Wide enough
/// for a `rtl_tcp://hostname.local.:12345 — R820T (29 gains)`
/// subtitle without wrapping.
pub(in crate::window) const FAVORITES_POPOVER_WIDTH_PX: i32 = 420;

/// Max height of the favorites popover's scrollable list. Caps the
/// popover so a large favorites set doesn't paint past the bottom
/// of the window; the internal `ScrolledWindow` handles overflow.
pub(in crate::window) const FAVORITES_POPOVER_HEIGHT_PX: i32 = 360;

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
pub(in crate::window) fn build_favorites_header() -> FavoritesHeaderHandle {
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
pub(in crate::window) fn build_menu_button() -> gtk4::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("_Preferences"), Some("app.preferences"));
    menu.append(Some("_Keyboard Shortcuts"), Some("win.show-help-overlay"));
    menu.append(Some("_About SDR-RS"), Some("app.about"));
    menu.append(Some("_Quit"), Some("app.quit"));

    let menu_button = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main menu")
        .build();
    menu_button.update_property(&[gtk4::accessible::Property::Label("Main menu")]);
    menu_button
}

/// Wrap header and content in an `AdwToolbarView`.
pub(in crate::window) fn build_toolbar_view(
    header: &adw::HeaderBar,
    content: &gtk4::Box,
) -> adw::ToolbarView {
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(header);
    toolbar_view.set_content(Some(content));
    toolbar_view
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
    play_button.update_property(&[gtk4::accessible::Property::Label("Start or stop")]);

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
