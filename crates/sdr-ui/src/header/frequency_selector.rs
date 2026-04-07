//! Frequency selector widget — 12-digit display with scroll/click/keyboard interaction.
//!
//! Displays a frequency in the format `DDD.DDD.DDD.DDD` Hz, where each digit
//! can be individually scrolled, clicked, or typed to adjust the frequency.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

/// Type alias for the frequency-changed callback.
type FrequencyCallback = Rc<RefCell<Option<Box<dyn Fn(u64)>>>>;

/// Total number of digits in the frequency display.
const NUM_DIGITS: usize = 12;

/// Default frequency in Hz (100 MHz).
const DEFAULT_FREQUENCY_HZ: u64 = 100_000_000;

/// Maximum displayable frequency in Hz (~999 GHz).
const MAX_FREQUENCY_HZ: u64 = 999_999_999_999;

/// Number of digits per visual group (separated by dots).
const DIGITS_PER_GROUP: usize = 3;

/// Frequency selector widget composed of 12 digit labels and 3 separator dots.
#[derive(Clone)]
pub struct FrequencySelector {
    /// The container widget to pack into the header bar.
    pub widget: gtk4::Box,
    /// Current frequency in Hz (shared with event handlers).
    frequency: Rc<Cell<u64>>,
    /// Index of the currently selected digit (shared with event handlers).
    selected_digit: Rc<Cell<usize>>,
    /// The 12 digit labels (shared with event handlers).
    digit_labels: Vec<gtk4::Label>,
    /// Optional callback fired on frequency changes.
    on_changed: FrequencyCallback,
}

impl FrequencySelector {
    /// Return the current frequency in Hz.
    #[must_use]
    pub fn frequency(&self) -> u64 {
        self.frequency.get()
    }

    /// Return the index of the currently selected digit.
    #[must_use]
    pub fn selected_digit(&self) -> usize {
        self.selected_digit.get()
    }

    /// Return a reference to the digit labels.
    #[must_use]
    pub fn digit_labels(&self) -> &[gtk4::Label] {
        &self.digit_labels
    }

    /// Register a callback invoked whenever the frequency changes from user interaction.
    pub fn connect_frequency_changed<F: Fn(u64) + 'static>(&self, f: F) {
        *self.on_changed.borrow_mut() = Some(Box::new(f));
    }

    /// Programmatically set the frequency and update the display.
    ///
    /// Does NOT fire the frequency-changed callback — callers are responsible
    /// for sending DSP commands and updating the status bar.
    pub fn set_frequency(&self, freq: u64) {
        let clamped = freq.min(MAX_FREQUENCY_HZ);
        self.frequency.set(clamped);
        let digits = frequency_to_digits(clamped);
        update_labels_and_styles(&self.digit_labels, &digits, self.selected_digit.get());
    }
}

/// Split a frequency in Hz into 12 individual digits (most significant first).
///
/// The leftmost digit (index 0) represents hundreds of GHz,
/// the rightmost digit (index 11) represents Hz.
fn frequency_to_digits(freq: u64) -> [u8; NUM_DIGITS] {
    let mut digits = [0u8; NUM_DIGITS];
    let mut remaining = freq;
    for digit in digits.iter_mut().rev() {
        *digit = (remaining % 10) as u8;
        remaining /= 10;
    }
    digits
}

/// Reconstruct a frequency in Hz from 12 individual digits.
fn digits_to_frequency(digits: &[u8; NUM_DIGITS]) -> u64 {
    let mut freq: u64 = 0;
    for &d in digits {
        freq = freq * 10 + u64::from(d);
    }
    freq
}

/// Return the step size (in Hz) for a given digit position.
///
/// Position 0 (leftmost) = 10^11 Hz, position 11 (rightmost) = 10^0 = 1 Hz.
///
/// # Panics
///
/// Panics if `position >= NUM_DIGITS` (only reachable from a bug in this module).
fn digit_step(position: usize) -> u64 {
    /// Precomputed powers of 10 for each digit position (index 0..12).
    const STEPS: [u64; NUM_DIGITS] = [
        100_000_000_000, // position 0:  100 GHz
        10_000_000_000,  // position 1:  10 GHz
        1_000_000_000,   // position 2:  1 GHz
        100_000_000,     // position 3:  100 MHz
        10_000_000,      // position 4:  10 MHz
        1_000_000,       // position 5:  1 MHz
        100_000,         // position 6:  100 kHz
        10_000,          // position 7:  10 kHz
        1_000,           // position 8:  1 kHz
        100,             // position 9:  100 Hz
        10,              // position 10: 10 Hz
        1,               // position 11: 1 Hz
    ];
    STEPS[position]
}

/// Build and return a fully-wired `FrequencySelector`.
pub fn build_frequency_selector() -> FrequencySelector {
    let container = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(0)
        .css_classes(["frequency-selector"])
        .focusable(true)
        .build();

    let frequency = Rc::new(Cell::new(DEFAULT_FREQUENCY_HZ));
    let selected_digit = Rc::new(Cell::new(NUM_DIGITS - 1)); // start on rightmost digit
    let on_changed: FrequencyCallback = Rc::new(RefCell::new(None));
    let mut digit_labels = Vec::with_capacity(NUM_DIGITS);

    let initial_digits = frequency_to_digits(DEFAULT_FREQUENCY_HZ);

    // Build 12 digit labels with separator dots between groups.
    for (i, &digit_val) in initial_digits.iter().enumerate() {
        let label = gtk4::Label::builder()
            .label(digit_val.to_string())
            .css_classes(["digit"])
            .build();

        digit_labels.push(label.clone());
        container.append(&label);

        // Add a separator dot after every 3 digits, except the last group.
        let next = i + 1;
        if next < NUM_DIGITS && next % DIGITS_PER_GROUP == 0 {
            let sep = gtk4::Label::builder()
                .label(".")
                .css_classes(["separator"])
                .build();
            container.append(&sep);
        }
    }

    // Apply initial styling (leading zeros, default selection).
    update_digit_styles(&digit_labels, &initial_digits, selected_digit.get());

    // --- Attach event controllers to each digit label ---
    for i in 0..NUM_DIGITS {
        attach_digit_scroll(
            &digit_labels[i],
            i,
            &frequency,
            &selected_digit,
            &digit_labels,
            &on_changed,
        );
        attach_digit_click(
            &digit_labels[i],
            i,
            &selected_digit,
            &digit_labels,
            &frequency,
            &container,
        );
    }

    // --- Attach keyboard controller to the container ---
    attach_keyboard_controller(
        &container,
        &frequency,
        &selected_digit,
        &digit_labels,
        &on_changed,
    );

    FrequencySelector {
        widget: container,
        frequency,
        selected_digit,
        digit_labels,
        on_changed,
    }
}

/// Attach a scroll event controller to a single digit label.
fn attach_digit_scroll(
    label: &gtk4::Label,
    digit_index: usize,
    frequency: &Rc<Cell<u64>>,
    selected_digit: &Rc<Cell<usize>>,
    digit_labels: &[gtk4::Label],
    on_changed: &FrequencyCallback,
) {
    let scroll = gtk4::EventControllerScroll::new(
        gtk4::EventControllerScrollFlags::VERTICAL | gtk4::EventControllerScrollFlags::DISCRETE,
    );

    let freq = Rc::clone(frequency);
    let sel = Rc::clone(selected_digit);
    let labels: Vec<gtk4::Label> = digit_labels.to_vec();
    let cb = Rc::clone(on_changed);

    scroll.connect_scroll(move |_ctrl, _dx, dy| {
        // Select the digit being scrolled.
        sel.set(digit_index);

        let step = digit_step(digit_index);
        let current = freq.get();

        // dy > 0 = scroll down = decrease; dy < 0 = scroll up = increase.
        let new_freq = if dy < 0.0 {
            current.saturating_add(step).min(MAX_FREQUENCY_HZ)
        } else {
            current.saturating_sub(step)
        };

        freq.set(new_freq);
        let digits = frequency_to_digits(new_freq);
        update_labels_and_styles(&labels, &digits, sel.get());
        notify_frequency_changed(&cb, new_freq);

        glib::Propagation::Stop
    });

    label.add_controller(scroll);
}

/// Attach a click gesture to a single digit label.
fn attach_digit_click(
    label: &gtk4::Label,
    digit_index: usize,
    selected_digit: &Rc<Cell<usize>>,
    digit_labels: &[gtk4::Label],
    frequency: &Rc<Cell<u64>>,
    container: &gtk4::Box,
) {
    let click = gtk4::GestureClick::new();

    let sel = Rc::clone(selected_digit);
    let labels: Vec<gtk4::Label> = digit_labels.to_vec();
    let freq = Rc::clone(frequency);
    let container_ref = container.downgrade();

    click.connect_pressed(move |_gesture, _n_press, _x, _y| {
        sel.set(digit_index);
        let digits = frequency_to_digits(freq.get());
        update_digit_styles(&labels, &digits, sel.get());

        // Grab focus on the container so keyboard events work.
        if let Some(c) = container_ref.upgrade() {
            c.grab_focus();
        }
    });

    label.add_controller(click);
}

/// Attach a keyboard event controller to the container widget.
fn attach_keyboard_controller(
    container: &gtk4::Box,
    frequency: &Rc<Cell<u64>>,
    selected_digit: &Rc<Cell<usize>>,
    digit_labels: &[gtk4::Label],
    on_changed: &FrequencyCallback,
) {
    let key_ctrl = gtk4::EventControllerKey::new();

    let freq = Rc::clone(frequency);
    let sel = Rc::clone(selected_digit);
    let labels: Vec<gtk4::Label> = digit_labels.to_vec();
    let cb = Rc::clone(on_changed);

    key_ctrl.connect_key_pressed(move |_ctrl, keyval, _keycode, _state| {
        match keyval {
            // Arrow keys: Up/Down adjust frequency, Left/Right move selection.
            v if v == gtk4::gdk::Key::Up => {
                let step = digit_step(sel.get());
                let new_freq = freq.get().saturating_add(step).min(MAX_FREQUENCY_HZ);
                freq.set(new_freq);
                let digits = frequency_to_digits(new_freq);
                update_labels_and_styles(&labels, &digits, sel.get());
                notify_frequency_changed(&cb, new_freq);
                glib::Propagation::Stop
            }
            v if v == gtk4::gdk::Key::Down => {
                let step = digit_step(sel.get());
                let new_freq = freq.get().saturating_sub(step);
                freq.set(new_freq);
                let digits = frequency_to_digits(new_freq);
                update_labels_and_styles(&labels, &digits, sel.get());
                notify_frequency_changed(&cb, new_freq);
                glib::Propagation::Stop
            }
            v if v == gtk4::gdk::Key::Left => {
                let current = sel.get();
                if current > 0 {
                    sel.set(current - 1);
                    let digits = frequency_to_digits(freq.get());
                    update_digit_styles(&labels, &digits, sel.get());
                }
                glib::Propagation::Stop
            }
            v if v == gtk4::gdk::Key::Right => {
                let current = sel.get();
                if current < NUM_DIGITS - 1 {
                    sel.set(current + 1);
                    let digits = frequency_to_digits(freq.get());
                    update_digit_styles(&labels, &digits, sel.get());
                }
                glib::Propagation::Stop
            }
            // Digit keys 0-9: set the digit value and advance selection.
            v => {
                let digit_value = match v {
                    k if k == gtk4::gdk::Key::_0 || k == gtk4::gdk::Key::KP_0 => Some(0u8),
                    k if k == gtk4::gdk::Key::_1 || k == gtk4::gdk::Key::KP_1 => Some(1),
                    k if k == gtk4::gdk::Key::_2 || k == gtk4::gdk::Key::KP_2 => Some(2),
                    k if k == gtk4::gdk::Key::_3 || k == gtk4::gdk::Key::KP_3 => Some(3),
                    k if k == gtk4::gdk::Key::_4 || k == gtk4::gdk::Key::KP_4 => Some(4),
                    k if k == gtk4::gdk::Key::_5 || k == gtk4::gdk::Key::KP_5 => Some(5),
                    k if k == gtk4::gdk::Key::_6 || k == gtk4::gdk::Key::KP_6 => Some(6),
                    k if k == gtk4::gdk::Key::_7 || k == gtk4::gdk::Key::KP_7 => Some(7),
                    k if k == gtk4::gdk::Key::_8 || k == gtk4::gdk::Key::KP_8 => Some(8),
                    k if k == gtk4::gdk::Key::_9 || k == gtk4::gdk::Key::KP_9 => Some(9),
                    _ => None,
                };

                if let Some(val) = digit_value {
                    let pos = sel.get();
                    let mut digits = frequency_to_digits(freq.get());
                    digits[pos] = val;
                    let new_freq = digits_to_frequency(&digits).min(MAX_FREQUENCY_HZ);
                    freq.set(new_freq);

                    // Re-derive digits in case clamping changed something.
                    let clamped_digits = frequency_to_digits(new_freq);

                    // Advance to next digit (stay at rightmost if already there).
                    if pos < NUM_DIGITS - 1 {
                        sel.set(pos + 1);
                    }

                    update_labels_and_styles(&labels, &clamped_digits, sel.get());
                    notify_frequency_changed(&cb, new_freq);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        }
    });

    container.add_controller(key_ctrl);
}

/// Update label text and styling for all digits.
fn update_labels_and_styles(labels: &[gtk4::Label], digits: &[u8; NUM_DIGITS], selected: usize) {
    for (i, label) in labels.iter().enumerate() {
        label.set_label(&digits[i].to_string());
    }
    update_digit_styles(labels, digits, selected);
}

/// Update CSS classes on digit labels: `.leading-zero` for leading zeros,
/// `.selected` for the currently selected digit.
fn update_digit_styles(labels: &[gtk4::Label], digits: &[u8; NUM_DIGITS], selected: usize) {
    // Find the first non-zero digit.
    let first_nonzero = digits.iter().position(|&d| d != 0).unwrap_or(NUM_DIGITS);

    for (i, label) in labels.iter().enumerate() {
        // Leading zero styling.
        if i < first_nonzero {
            label.add_css_class("leading-zero");
        } else {
            label.remove_css_class("leading-zero");
        }

        // Selected digit styling.
        if i == selected {
            label.add_css_class("selected");
        } else {
            label.remove_css_class("selected");
        }
    }
}

/// Invoke the frequency-changed callback, if one has been registered.
fn notify_frequency_changed(on_changed: &FrequencyCallback, new_freq: u64) {
    if let Some(cb) = on_changed.borrow().as_ref() {
        cb(new_freq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_to_digits_zero() {
        let digits = frequency_to_digits(0);
        assert_eq!(digits, [0; NUM_DIGITS]);
    }

    #[test]
    fn frequency_to_digits_default() {
        // 100 MHz = 100_000_000 Hz
        // As 12 digits: 000.100.000.000
        let digits = frequency_to_digits(DEFAULT_FREQUENCY_HZ);
        assert_eq!(digits, [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn frequency_to_digits_max() {
        let digits = frequency_to_digits(MAX_FREQUENCY_HZ);
        assert_eq!(digits, [9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9]);
    }

    #[test]
    fn digits_to_frequency_roundtrip() {
        let test_values = [0, 1, 42, 100_000_000, 1_234_567_890, MAX_FREQUENCY_HZ];
        for &freq in &test_values {
            let digits = frequency_to_digits(freq);
            let reconstructed = digits_to_frequency(&digits);
            assert_eq!(reconstructed, freq, "roundtrip failed for {freq}");
        }
    }

    #[test]
    fn digit_step_positions() {
        // Position 11 (rightmost) = 1 Hz
        assert_eq!(digit_step(11), 1);
        // Position 10 = 10 Hz
        assert_eq!(digit_step(10), 10);
        // Position 9 = 100 Hz
        assert_eq!(digit_step(9), 100);
        // Position 6 = 100_000 Hz (100 kHz)
        assert_eq!(digit_step(6), 100_000);
        // Position 3 = 100_000_000 Hz (100 MHz)
        assert_eq!(digit_step(3), 100_000_000);
        // Position 0 (leftmost) = 100_000_000_000 Hz (100 GHz)
        assert_eq!(digit_step(0), 100_000_000_000);
    }

    #[test]
    fn default_frequency_is_100_mhz() {
        assert_eq!(DEFAULT_FREQUENCY_HZ, 100_000_000);
    }

    #[test]
    fn frequency_clamping_to_max() {
        let over_max = MAX_FREQUENCY_HZ + 1000;
        let clamped = over_max.min(MAX_FREQUENCY_HZ);
        assert_eq!(clamped, MAX_FREQUENCY_HZ);
    }

    #[test]
    fn frequency_saturating_sub_at_zero() {
        let freq: u64 = 0;
        let step = digit_step(6); // 100 kHz
        let new_freq = freq.saturating_sub(step);
        assert_eq!(new_freq, 0);
    }

    #[test]
    fn digit_step_all_positions() {
        let expected_steps: [u64; NUM_DIGITS] = [
            100_000_000_000,
            10_000_000_000,
            1_000_000_000,
            100_000_000,
            10_000_000,
            1_000_000,
            100_000,
            10_000,
            1_000,
            100,
            10,
            1,
        ];
        for (i, &expected) in expected_steps.iter().enumerate() {
            assert_eq!(digit_step(i), expected, "digit_step({i}) wrong");
        }
    }
}
