//! Custom CSS for the SDR-RS application.

/// Inline CSS for the application.
const APP_CSS: &str = r"
/* Frequency selector — monospace, accent color (used in PR 4) */
.frequency-selector {
    font-family: monospace;
    font-size: 1.4em;
    color: @accent_color;
}

/* Status bar — subtle padding, smaller font, border-top */
.status-bar {
    padding: 4px 8px;
    font-size: 0.85em;
    border-top: 1px solid @borders;
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
";

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
