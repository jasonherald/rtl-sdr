//! Keyboard shortcuts and help overlay for the SDR-RS application.

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::header::demod_selector::DEMOD_MODE_COUNT;

/// Set up keyboard shortcuts on the application window.
///
/// Registers shortcuts for play/stop toggle, demod cycling, sidebar toggle,
/// and attaches a help overlay for `Ctrl+?`.
pub fn setup_shortcuts(
    window: &adw::ApplicationWindow,
    play_button: &gtk4::ToggleButton,
    sidebar_toggle: &gtk4::ToggleButton,
    bookmarks_toggle: &gtk4::ToggleButton,
    demod_dropdown: &gtk4::DropDown,
    scanner_switch: &gtk4::Switch,
) {
    let controller = gtk4::ShortcutController::new();
    controller.set_scope(gtk4::ShortcutScope::Managed);

    // Space: Play/Stop toggle
    let play_button_weak = play_button.downgrade();
    let trigger_space = gtk4::ShortcutTrigger::parse_string("space");
    if let Some(trigger) = trigger_space {
        let action = gtk4::CallbackAction::new(move |_widget, _args| {
            if let Some(btn) = play_button_weak.upgrade() {
                btn.set_active(!btn.is_active());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        let shortcut = gtk4::Shortcut::new(Some(trigger), Some(action));
        controller.add_shortcut(shortcut);
    }

    // M: Cycle demod mode (by advancing the dropdown index — its
    // `connect_selected_notify` handler dispatches the DSP command).
    let demod_dropdown_weak = demod_dropdown.downgrade();
    let trigger_m = gtk4::ShortcutTrigger::parse_string("m");
    if let Some(trigger) = trigger_m {
        let action = gtk4::CallbackAction::new(move |_widget, _args| {
            if let Some(dd) = demod_dropdown_weak.upgrade() {
                let sel = dd.selected();
                if sel == gtk4::INVALID_LIST_POSITION {
                    return glib::Propagation::Proceed;
                }
                let next_idx = (sel + 1) % DEMOD_MODE_COUNT;
                dd.set_selected(next_idx);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        let shortcut = gtk4::Shortcut::new(Some(trigger), Some(action));
        controller.add_shortcut(shortcut);
    }

    // F9: Toggle sidebar visibility (via the toggle button, which drives
    // the split view through its signal handler).
    let sidebar_toggle_weak = sidebar_toggle.downgrade();
    let trigger_f9 = gtk4::ShortcutTrigger::parse_string("F9");
    if let Some(trigger) = trigger_f9 {
        let action = gtk4::CallbackAction::new(move |_widget, _args| {
            if let Some(btn) = sidebar_toggle_weak.upgrade() {
                btn.set_active(!btn.is_active());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        let shortcut = gtk4::Shortcut::new(Some(trigger), Some(action));
        controller.add_shortcut(shortcut);
    }

    // Ctrl+B: Toggle bookmarks flyout. Routed through the header
    // toggle button so the button's visual state stays in sync
    // with the flyout — same indirection pattern as F9 /
    // sidebar_toggle above. "B" is the bookmarks mnemonic; F10
    // was considered but conflicts with the GNOME menu
    // convention some shell extensions bind.
    let bookmarks_toggle_weak = bookmarks_toggle.downgrade();
    let trigger_ctrl_b = gtk4::ShortcutTrigger::parse_string("<Ctrl>b");
    if let Some(trigger) = trigger_ctrl_b {
        let action = gtk4::CallbackAction::new(move |_widget, _args| {
            if let Some(btn) = bookmarks_toggle_weak.upgrade() {
                btn.set_active(!btn.is_active());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        let shortcut = gtk4::Shortcut::new(Some(trigger), Some(action));
        controller.add_shortcut(shortcut);
    }

    // F8: Toggle scanner master switch. The master switch is
    // wired via `connect_active_notify` in `connect_scanner_panel`
    // — `set_active` changes the active property and fires that
    // notify, which dispatches `SetScannerEnabled` to the engine.
    // (Earlier iterations of this code claimed `set_active`
    // triggers `state-set` on programmatic changes; that's
    // binding-version-dependent, so we sidestep the ambiguity by
    // listening to notify::active instead.)
    let scanner_switch_weak = scanner_switch.downgrade();
    let trigger_f8 = gtk4::ShortcutTrigger::parse_string("F8");
    if let Some(trigger) = trigger_f8 {
        let action = gtk4::CallbackAction::new(move |_widget, _args| {
            if let Some(sw) = scanner_switch_weak.upgrade() {
                sw.set_active(!sw.is_active());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        let shortcut = gtk4::Shortcut::new(Some(trigger), Some(action));
        controller.add_shortcut(shortcut);
    }

    window.add_controller(controller);
}

/// Shortcut catalog — single source of truth for the help dialog.
const SHORTCUT_CATALOG: &[(&str, &[(&str, &str)])] = &[
    (
        "Playback",
        &[("Space", "Play / Stop"), ("M", "Cycle demod mode")],
    ),
    (
        "Navigation",
        &[
            ("F9", "Toggle sidebar"),
            ("Ctrl+B", "Toggle bookmarks panel"),
            ("F8", "Toggle scanner"),
        ],
    ),
    (
        "Application",
        &[
            ("Ctrl+/", "Keyboard shortcuts"),
            ("Ctrl+Q", "Quit"),
            ("F1", "About"),
        ],
    ),
];

/// Dialog layout constants.
const DIALOG_CONTENT_WIDTH: i32 = 400;
const DIALOG_CONTENT_HEIGHT: i32 = 400;
const DIALOG_SPACING: i32 = 16;
const DIALOG_MARGIN: i32 = 12;
const DIALOG_MARGIN_SIDE: i32 = 24;

/// Show the keyboard shortcuts dialog as a modal `AdwDialog`.
pub fn show_shortcuts_dialog(parent: &impl gtk4::prelude::IsA<gtk4::Widget>) {
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(DIALOG_SPACING)
        .margin_top(DIALOG_MARGIN)
        .margin_bottom(DIALOG_MARGIN)
        .margin_start(DIALOG_MARGIN_SIDE)
        .margin_end(DIALOG_MARGIN_SIDE)
        .build();

    for (group_name, entries) in SHORTCUT_CATALOG {
        let group_label = gtk4::Label::builder()
            .label(*group_name)
            .css_classes(["heading"])
            .halign(gtk4::Align::Start)
            .build();
        content.append(&group_label);

        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        for (key, description) in *entries {
            let row = adw::ActionRow::builder().title(*description).build();
            let key_label = gtk4::Label::builder()
                .label(*key)
                .css_classes(["dim-label"])
                .build();
            row.add_suffix(&key_label);
            list.append(&row);
        }

        content.append(&list);
    }

    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&content)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scrolled));

    let dialog = adw::Dialog::builder()
        .title("Keyboard Shortcuts")
        .content_width(DIALOG_CONTENT_WIDTH)
        .content_height(DIALOG_CONTENT_HEIGHT)
        .build();
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check that `DEMOD_MODE_COUNT` is usable for modulo cycling.
    const _: () = assert!(DEMOD_MODE_COUNT > 0);
}
