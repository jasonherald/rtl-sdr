//! Shared UI utility functions.

use gtk4::prelude::*;

/// Show a status label with success (green) or error (red) styling.
///
/// Uses Adwaita CSS classes for theme-aware colors.
pub fn show_status(label: &gtk4::Label, text: &str, success: bool) {
    label.set_visible(true);
    label.set_text(text);
    label.set_css_classes(if success { &["success"] } else { &["error"] });
}
