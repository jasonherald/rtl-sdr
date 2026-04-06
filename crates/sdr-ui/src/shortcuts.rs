//! Keyboard shortcuts and help overlay for the SDR-RS application.

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

use crate::header::demod_selector::DEMOD_MODE_COUNT;

/// Set up keyboard shortcuts on the application window.
///
/// Registers shortcuts for play/stop toggle, demod cycling, sidebar toggle,
/// and attaches a help overlay for `Ctrl+?`.
pub fn setup_shortcuts(
    window: &adw::ApplicationWindow,
    play_button: &gtk4::ToggleButton,
    sidebar_toggle: &gtk4::ToggleButton,
    demod_dropdown: &gtk4::DropDown,
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

    window.add_controller(controller);
}

/// Build the keyboard shortcuts help window.
///
/// Returns a `ShortcutsWindow` that lists all available keyboard shortcuts,
/// suitable for use as the window's help overlay.
pub fn build_shortcuts_window() -> gtk4::ShortcutsWindow {
    // --- Playback group ---
    let play_stop = gtk4::ShortcutsShortcut::builder()
        .shortcut_type(gtk4::ShortcutType::Accelerator)
        .title("Play / Stop")
        .accelerator("space")
        .build();

    let cycle_demod = gtk4::ShortcutsShortcut::builder()
        .shortcut_type(gtk4::ShortcutType::Accelerator)
        .title("Cycle demod mode")
        .accelerator("m")
        .build();

    let playback_group = gtk4::ShortcutsGroup::builder().title("Playback").build();
    // ShortcutsGroup extends Box — use append() (pre-v4.14 API).
    playback_group.append(&play_stop);
    playback_group.append(&cycle_demod);

    // --- Navigation group ---
    let toggle_sidebar = gtk4::ShortcutsShortcut::builder()
        .shortcut_type(gtk4::ShortcutType::Accelerator)
        .title("Toggle sidebar")
        .accelerator("F9")
        .build();

    let nav_group = gtk4::ShortcutsGroup::builder().title("Navigation").build();
    nav_group.append(&toggle_sidebar);

    // --- Application group ---
    let shortcuts_help = gtk4::ShortcutsShortcut::builder()
        .shortcut_type(gtk4::ShortcutType::Accelerator)
        .title("Keyboard shortcuts")
        .accelerator("<Ctrl>question")
        .build();

    let quit = gtk4::ShortcutsShortcut::builder()
        .shortcut_type(gtk4::ShortcutType::Accelerator)
        .title("Quit")
        .accelerator("<Ctrl>q")
        .build();

    let app_group = gtk4::ShortcutsGroup::builder().title("Application").build();
    app_group.append(&shortcuts_help);
    app_group.append(&quit);

    // --- Section and window ---
    // ShortcutsSection extends Box — use append() (pre-v4.14 API).
    let section = gtk4::ShortcutsSection::builder()
        .title("SDR-RS")
        .section_name("shortcuts")
        .build();
    section.append(&playback_group);
    section.append(&nav_group);
    section.append(&app_group);

    // ShortcutsWindow uses set_child to accept the section.
    let shortcuts_window = gtk4::ShortcutsWindow::builder()
        .modal(true)
        .resizable(true)
        .build();
    shortcuts_window.set_child(Some(&section));

    shortcuts_window
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check that `DEMOD_MODE_COUNT` is usable for modulo cycling.
    const _: () = assert!(DEMOD_MODE_COUNT > 0);
}
