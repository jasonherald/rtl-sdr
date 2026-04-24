//! Custom CSS for the SDR-RS application.

/// Inline CSS for the application.
const APP_CSS: &str = r#"
/* Frequency selector — monospace digit display */
.frequency-selector {
    font-family: "SF Mono", "JetBrains Mono", "Fira Code", monospace;
    font-size: 24px;
    font-weight: 300;
    letter-spacing: 1px;
}

.frequency-selector .digit {
    padding: 2px 1px;
    border-radius: 4px;
    min-width: 16px;
}

.frequency-selector .digit:hover {
    background-color: alpha(@accent_color, 0.1);
}

.frequency-selector .digit.selected {
    background-color: alpha(@accent_color, 0.2);
}

.frequency-selector .digit.leading-zero {
    opacity: 0.3;
}

.frequency-selector .separator {
    opacity: 0.3;
    padding: 0 2px;
}

/* Status bar — subtle padding, smaller font, border-top */
.status-bar {
    padding: 4px 12px;
    font-size: 12px;
    color: alpha(@theme_fg_color, 0.7);
    background-color: alpha(@theme_bg_color, 0.95);
    border-top: 1px solid alpha(@borders, 0.5);
}

.status-bar label {
    margin: 0 8px;
}

.status-bar separator {
    margin: 2px 0;
}

/* Play button — destructive color when active (recording/running) */
.play-button:checked {
    background-color: @error_bg_color;
    color: @error_fg_color;
}

/* Spectrum display area — borderless, transparent background */
.spectrum-area {
    border: none;
    background: transparent;
}

/* Activity bar — narrow strip of icon toggle buttons against window
 * edge. Visually a natural extension of the header bar: no chrome
 * on buttons, icons sit on transparent background, subtle hover tint,
 * and a thin accent strip + tint on the currently-selected icon.
 */
.activity-bar {
    background-color: transparent;
    padding: 4px 0;
}

.activity-bar button.activity-bar-button {
    min-width: 32px;
    min-height: 32px;
    padding: 4px;
    margin: 2px 4px;
    border-radius: 6px;
    background: none;
    border: none;
    box-shadow: none;
    color: alpha(@window_fg_color, 0.7);
}

.activity-bar button.activity-bar-button:hover {
    background-color: alpha(@window_fg_color, 0.08);
    color: @window_fg_color;
}

.activity-bar button.activity-bar-button:checked {
    background-color: alpha(@accent_color, 0.15);
    color: @accent_color;
}

.activity-bar button.activity-bar-button:checked:hover {
    background-color: alpha(@accent_color, 0.2);
}

/* Sidebar resize handle — a thin strip at the sidebar's inner
 * edge that carries the drag gesture + col-resize cursor. The
 * strip is transparent by default (invisible to eye) and tints
 * subtly on hover so the user discovers the affordance when
 * their cursor enters the region. Matches the restrained visual
 * treatment of the activity bar — no chrome unless the user
 * reaches for it.
 */
.sidebar-resize-handle {
    background: transparent;
}

.sidebar-resize-handle:hover {
    background-color: alpha(@accent_color, 0.2);
}
"#;

/// Load the application CSS into the default display's style context.
///
/// Logs a warning and returns early if no display is available.
pub fn load_css() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(APP_CSS);

    let Some(display) = gtk4::gdk::Display::default() else {
        tracing::warn!("no display available — CSS not loaded");
        return;
    };

    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
