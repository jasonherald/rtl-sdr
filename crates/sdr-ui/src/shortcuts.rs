//! Keyboard shortcuts and help overlay for the SDR-RS application.

use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use sdr_types::DemodMode;

use crate::header::demod_selector;
use crate::messages::UiToDsp;
use crate::state::AppState;

/// Total number of demod modes in the cycle order.
const DEMOD_MODE_COUNT: u32 = 8;

/// Demod mode cycle order matching the dropdown: NFM -> WFM -> AM -> USB -> LSB -> DSB -> CW -> RAW -> NFM.
/// The dropdown order is WFM(0), NFM(1), AM(2), USB(3), LSB(4), DSB(5), CW(6), RAW(7).
/// Cycling goes through the dropdown order: 0 -> 1 -> 2 -> ... -> 7 -> 0.
const CYCLE_ORDER: [DemodMode; 8] = [
    DemodMode::Wfm,
    DemodMode::Nfm,
    DemodMode::Am,
    DemodMode::Usb,
    DemodMode::Lsb,
    DemodMode::Dsb,
    DemodMode::Cw,
    DemodMode::Raw,
];

/// Set up keyboard shortcuts on the application window.
///
/// Registers shortcuts for play/stop toggle, demod cycling, sidebar toggle,
/// and attaches a help overlay for `Ctrl+?`.
pub fn setup_shortcuts(
    window: &adw::ApplicationWindow,
    state: &Rc<AppState>,
    play_button: &gtk4::ToggleButton,
    split_view: &adw::OverlaySplitView,
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

    // M: Cycle demod mode
    let state_demod = Rc::clone(state);
    let demod_dropdown_weak = demod_dropdown.downgrade();
    let trigger_m = gtk4::ShortcutTrigger::parse_string("m");
    if let Some(trigger) = trigger_m {
        let action = gtk4::CallbackAction::new(move |_widget, _args| {
            let current = state_demod.demod_mode.get();
            let next = cycle_demod_mode(current);
            state_demod.demod_mode.set(next);
            state_demod.send_dsp(UiToDsp::SetDemodMode(next));
            tracing::debug!(?next, "demod mode cycled via shortcut");

            // Update the dropdown to reflect the new mode.
            if let Some(dd) = demod_dropdown_weak.upgrade()
                && let Some(idx) = demod_selector::demod_mode_to_index(next)
            {
                dd.set_selected(idx);
            }

            glib::Propagation::Stop
        });
        let shortcut = gtk4::Shortcut::new(Some(trigger), Some(action));
        controller.add_shortcut(shortcut);
    }

    // F9: Toggle sidebar visibility
    let split_view_weak = split_view.downgrade();
    let trigger_f9 = gtk4::ShortcutTrigger::parse_string("F9");
    if let Some(trigger) = trigger_f9 {
        let action = gtk4::CallbackAction::new(move |_widget, _args| {
            if let Some(sv) = split_view_weak.upgrade() {
                sv.set_show_sidebar(!sv.shows_sidebar());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        let shortcut = gtk4::Shortcut::new(Some(trigger), Some(action));
        controller.add_shortcut(shortcut);
    }

    window.add_controller(controller);
}

/// Cycle to the next demod mode in sequence.
fn cycle_demod_mode(current: DemodMode) -> DemodMode {
    let current_idx = CYCLE_ORDER.iter().position(|&m| m == current).unwrap_or(0);
    #[allow(clippy::cast_possible_truncation)]
    let next_idx = ((current_idx as u32 + 1) % DEMOD_MODE_COUNT) as usize;
    CYCLE_ORDER[next_idx]
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

    #[test]
    fn cycle_demod_wraps_around() {
        assert_eq!(cycle_demod_mode(DemodMode::Wfm), DemodMode::Nfm);
        assert_eq!(cycle_demod_mode(DemodMode::Nfm), DemodMode::Am);
        assert_eq!(cycle_demod_mode(DemodMode::Am), DemodMode::Usb);
        assert_eq!(cycle_demod_mode(DemodMode::Usb), DemodMode::Lsb);
        assert_eq!(cycle_demod_mode(DemodMode::Lsb), DemodMode::Dsb);
        assert_eq!(cycle_demod_mode(DemodMode::Dsb), DemodMode::Cw);
        assert_eq!(cycle_demod_mode(DemodMode::Cw), DemodMode::Raw);
        assert_eq!(cycle_demod_mode(DemodMode::Raw), DemodMode::Wfm);
    }

    #[test]
    fn cycle_order_has_correct_count() {
        assert_eq!(CYCLE_ORDER.len(), DEMOD_MODE_COUNT as usize);
    }
}
