//! Keyboard shortcuts and help overlay for the SDR-RS application.

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::header::demod_selector::DEMOD_MODE_COUNT;
use crate::sidebar::{ActivityBar, ActivityBarEntry, LEFT_ACTIVITIES, RIGHT_ACTIVITIES};

/// Set up keyboard shortcuts on the application window.
///
/// Registers shortcuts for play/stop toggle, demod cycling, sidebar toggle,
/// and attaches a help overlay for `Ctrl+?`.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "single window-wide shortcut setup — callers already bundle their widgets; the registrations are parallel not chainable"
)]
pub fn setup_shortcuts(
    window: &adw::ApplicationWindow,
    play_button: &gtk4::ToggleButton,
    sidebar_toggle: &gtk4::ToggleButton,
    bookmarks_toggle: &gtk4::ToggleButton,
    demod_dropdown: &gtk4::DropDown,
    scanner_switch: &gtk4::Switch,
    left_activity_bar: &ActivityBar,
    right_activity_bar: &ActivityBar,
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

    // Activity-bar keyboard bindings — iterate the canonical entry
    // lists (single source of truth in `sidebar::activity_bar`) so a
    // rename/reorder/new-entry in one place automatically propagates
    // to both the GTK binding registration and the help-dialog
    // catalog. `emit_clicked` routes through the activity bar's
    // click handler, which manages selection vs. panel-toggle
    // semantics and keeps `:checked` in lockstep with the logical
    // selection.
    register_activity_shortcuts(&controller, LEFT_ACTIVITIES, left_activity_bar);
    register_activity_shortcuts(&controller, RIGHT_ACTIVITIES, right_activity_bar);

    window.add_controller(controller);
}

/// Register `Ctrl+N` / `Ctrl+Shift+N` bindings for every entry in an
/// activity list. Each binding fires `emit_clicked` on the matching
/// `ToggleButton` so the press runs through the same click handler
/// as a real click (preserving the selection-vs-toggle semantic).
fn register_activity_shortcuts(
    controller: &gtk4::ShortcutController,
    entries: &[ActivityBarEntry],
    bar: &ActivityBar,
) {
    for entry in entries {
        let Some(btn) = bar.buttons.get(entry.name) else {
            tracing::warn!(
                "activity shortcut {} has no matching button ({})",
                entry.accelerator,
                entry.name
            );
            continue;
        };
        let Some(trigger) = gtk4::ShortcutTrigger::parse_string(entry.accelerator) else {
            tracing::warn!(
                "activity accelerator {} for {} failed to parse",
                entry.accelerator,
                entry.name
            );
            continue;
        };
        let btn_weak = btn.downgrade();
        let action = gtk4::CallbackAction::new(move |_widget, _args| {
            if let Some(btn) = btn_weak.upgrade() {
                btn.emit_clicked();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        controller.add_shortcut(gtk4::Shortcut::new(Some(trigger), Some(action)));
    }
}

/// Playback shortcuts — stable, no activity-bar dependency.
const PLAYBACK_SHORTCUTS: &[(&str, &str)] = &[("Space", "Play / Stop"), ("M", "Cycle demod mode")];

/// Static navigation shortcuts that don't come from the activity
/// lists — F9/F8/Ctrl+B. Activity-bar `Ctrl+N`/`Ctrl+Shift+N`
/// bindings are spliced in by [`shortcut_catalog`] so the dialog
/// always reflects the canonical entry lists in
/// `sidebar::activity_bar`.
const NAV_STATIC_SHORTCUTS: &[(&str, &str)] = &[
    ("F9", "Toggle left panel"),
    ("Ctrl+B", "Toggle bookmarks panel"),
    ("F8", "Toggle scanner"),
];

/// Application-level shortcuts — stable.
const APPLICATION_SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+/", "Keyboard shortcuts"),
    ("Ctrl+Q", "Quit"),
    ("F1", "About"),
];

/// Build the shortcut catalog shown in the help dialog. Navigation
/// entries are derived from [`LEFT_ACTIVITIES`] / [`RIGHT_ACTIVITIES`]
/// so renaming / reordering / adding an activity in
/// `sidebar::activity_bar` updates the dialog automatically without
/// requiring a separate table edit here.
fn shortcut_catalog() -> Vec<(&'static str, Vec<(String, String)>)> {
    let playback = PLAYBACK_SHORTCUTS
        .iter()
        .map(|(k, d)| ((*k).to_string(), (*d).to_string()))
        .collect::<Vec<_>>();

    let mut navigation: Vec<(String, String)> = Vec::new();
    navigation.push((
        NAV_STATIC_SHORTCUTS[0].0.to_string(),
        NAV_STATIC_SHORTCUTS[0].1.to_string(),
    ));
    for entry in LEFT_ACTIVITIES {
        navigation.push((
            entry.shortcut_label.to_string(),
            format!("{} panel", entry.display_name),
        ));
    }
    for entry in RIGHT_ACTIVITIES {
        navigation.push((
            entry.shortcut_label.to_string(),
            format!("Toggle {} panel", entry.display_name.to_lowercase()),
        ));
    }
    for (key, desc) in &NAV_STATIC_SHORTCUTS[1..] {
        navigation.push(((*key).to_string(), (*desc).to_string()));
    }

    let application = APPLICATION_SHORTCUTS
        .iter()
        .map(|(k, d)| ((*k).to_string(), (*d).to_string()))
        .collect::<Vec<_>>();

    vec![
        ("Playback", playback),
        ("Navigation", navigation),
        ("Application", application),
    ]
}

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

    for (group_name, entries) in shortcut_catalog() {
        let group_label = gtk4::Label::builder()
            .label(group_name)
            .css_classes(["heading"])
            .halign(gtk4::Align::Start)
            .build();
        content.append(&group_label);

        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        for (key, description) in entries {
            let row = adw::ActionRow::builder().title(&description).build();
            let key_label = gtk4::Label::builder()
                .label(&key)
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
