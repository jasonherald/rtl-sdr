//! Main window construction — header bar, split view, breakpoints.

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::header;
use crate::sidebar;
use crate::spectrum;

/// Default window width in pixels.
const DEFAULT_WIDTH: i32 = 1200;
/// Default window height in pixels.
const DEFAULT_HEIGHT: i32 = 800;
/// Sidebar collapse breakpoint width in pixels.
const SIDEBAR_BREAKPOINT_PX: f64 = 800.0;

/// Build and present the main application window.
pub fn build_window(app: &adw::Application) {
    let split_view = build_split_view();
    let sidebar_toggle = build_sidebar_toggle(&split_view);
    let header = build_header_bar(&sidebar_toggle);
    let toolbar_view = build_toolbar_view(&header, &split_view);
    let breakpoint = build_breakpoint(&split_view);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("SDR-RS")
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .content(&toolbar_view)
        .build();

    window.add_breakpoint(breakpoint);

    setup_app_actions(app, &window);

    window.present();
}

/// Build the `AdwOverlaySplitView` with sidebar configuration panels and content.
fn build_split_view() -> adw::OverlaySplitView {
    // Sidebar — configuration panels.
    let (sidebar_scroll, _panels) = sidebar::build_sidebar();
    // TODO: Store `_panels` for DSP bridge signal wiring (PR #7)

    // Main content area — spectrum display (FFT plot + waterfall).
    let spectrum_view = spectrum::build_spectrum_view();
    spectrum_view.add_css_class("spectrum-area");

    let content_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    content_box.append(&spectrum_view);

    adw::OverlaySplitView::builder()
        .sidebar(&sidebar_scroll)
        .content(&content_box)
        .show_sidebar(true)
        .build()
}

/// Build the sidebar toggle button bound to the split view.
fn build_sidebar_toggle(split_view: &adw::OverlaySplitView) -> gtk4::ToggleButton {
    let toggle = gtk4::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Toggle sidebar")
        .active(true)
        .build();

    // Bind toggle state to sidebar visibility
    toggle.connect_toggled(glib::clone!(
        #[weak]
        split_view,
        move |btn| {
            split_view.set_show_sidebar(btn.is_active());
        }
    ));

    toggle
}

/// Build the `AdwHeaderBar` with controls.
fn build_header_bar(sidebar_toggle: &gtk4::ToggleButton) -> adw::HeaderBar {
    // Play/stop button (non-functional placeholder)
    let play_button = gtk4::ToggleButton::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text("Start / Stop")
        .css_classes(["play-button"])
        .build();

    // Frequency selector as the title widget
    let freq_selector = header::build_frequency_selector();
    freq_selector.connect_frequency_changed(|freq| {
        tracing::debug!(frequency_hz = freq, "frequency changed");
        // TODO: Send UiToDsp::Tune(freq as f64) to DSP pipeline (PR #7)
    });

    // App menu
    let menu_button = build_menu_button();

    let header = adw::HeaderBar::builder()
        .title_widget(&freq_selector.widget)
        .build();

    header.pack_start(sidebar_toggle);
    header.pack_start(&play_button);
    header.pack_end(&menu_button);

    header
}

/// Build the app menu button with About / Keyboard Shortcuts / Quit actions.
fn build_menu_button() -> gtk4::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("_About SDR-RS"), Some("app.about"));
    menu.append(Some("_Quit"), Some("app.quit"));

    gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main menu")
        .build()
}

/// Wrap header and content in an `AdwToolbarView`.
fn build_toolbar_view(
    header: &adw::HeaderBar,
    content: &adw::OverlaySplitView,
) -> adw::ToolbarView {
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(header);
    toolbar_view.set_content(Some(content));
    toolbar_view
}

/// Create a breakpoint that collapses the sidebar below `SIDEBAR_BREAKPOINT_PX`.
fn build_breakpoint(split_view: &adw::OverlaySplitView) -> adw::Breakpoint {
    let condition = adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        SIDEBAR_BREAKPOINT_PX,
        adw::LengthUnit::Px,
    );

    let breakpoint = adw::Breakpoint::new(condition);
    breakpoint.add_setter(split_view, "collapsed", Some(&true.into()));

    breakpoint
}

/// Register application-level actions (About, Quit).
fn setup_app_actions(app: &adw::Application, window: &adw::ApplicationWindow) {
    // Quit action
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate(glib::clone!(
        #[weak]
        window,
        move |_, _| {
            window.close();
        }
    ));
    app.add_action(&quit_action);
    app.set_accels_for_action("app.quit", &["<Ctrl>q"]);

    // About action
    let about_action = gio::SimpleAction::new("about", None);
    about_action.connect_activate(glib::clone!(
        #[weak]
        window,
        move |_, _| {
            let about = adw::AboutDialog::builder()
                .application_name("SDR-RS")
                .developer_name("Jason Herald")
                .version(env!("CARGO_PKG_VERSION"))
                .application_icon("audio-radio-symbolic")
                .license_type(gtk4::License::MitX11)
                .website("https://github.com/jasonherald/rtl-sdr")
                .build();
            about.present(Some(&window));
        }
    ));
    app.add_action(&about_action);
}
