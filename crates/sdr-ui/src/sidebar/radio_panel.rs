//! Radio / demodulator configuration panel — bandwidth, squelch, de-emphasis.

use libadwaita as adw;
use libadwaita::prelude::*;

/// Default bandwidth in Hz.
const DEFAULT_BANDWIDTH_HZ: f64 = 12_500.0;
/// Minimum bandwidth in Hz.
const MIN_BANDWIDTH_HZ: f64 = 100.0;
/// Maximum bandwidth in Hz.
const MAX_BANDWIDTH_HZ: f64 = 250_000.0;
/// Bandwidth step in Hz.
const BANDWIDTH_STEP_HZ: f64 = 100.0;

/// Default squelch level in dB.
const DEFAULT_SQUELCH_DB: f64 = -100.0;
/// Minimum squelch level in dB.
const MIN_SQUELCH_DB: f64 = -160.0;
/// Maximum squelch level in dB.
const MAX_SQUELCH_DB: f64 = 0.0;
/// Squelch step in dB.
const SQUELCH_STEP_DB: f64 = 1.0;

/// Radio / demodulator configuration panel with references to interactive rows.
pub struct RadioPanel {
    /// The `AdwPreferencesGroup` widget to pack into the sidebar.
    pub widget: adw::PreferencesGroup,
    /// Bandwidth control.
    pub bandwidth_row: adw::SpinRow,
    /// Squelch enable toggle.
    pub squelch_enabled_row: adw::SwitchRow,
    /// Squelch level control.
    pub squelch_level_row: adw::SpinRow,
    /// De-emphasis filter selector.
    pub deemphasis_row: adw::ComboRow,
    /// Noise blanker toggle.
    pub noise_blanker_row: adw::SwitchRow,
    /// FM IF noise reduction toggle (visible only for FM modes).
    pub fm_if_nr_row: adw::SwitchRow,
}

impl RadioPanel {
    /// Show or hide FM-specific controls based on the current demod mode.
    ///
    /// Call this when the demod mode changes to show FM IF NR only for FM modes
    /// (WFM and NFM).
    pub fn set_fm_controls_visible(&self, visible: bool) {
        self.deemphasis_row.set_visible(visible);
        self.fm_if_nr_row.set_visible(visible);
    }
}

/// Build the radio / demodulator configuration panel.
pub fn build_radio_panel() -> RadioPanel {
    let group = adw::PreferencesGroup::builder()
        .title("Radio")
        .description("Demodulator settings")
        .build();

    // --- Bandwidth ---
    let bandwidth_adj = gtk4::Adjustment::new(
        DEFAULT_BANDWIDTH_HZ,
        MIN_BANDWIDTH_HZ,
        MAX_BANDWIDTH_HZ,
        BANDWIDTH_STEP_HZ,
        1_000.0,
        0.0,
    );
    let bandwidth_row = adw::SpinRow::builder()
        .title("Bandwidth")
        .subtitle("Hz")
        .adjustment(&bandwidth_adj)
        .digits(0)
        .build();

    // --- Squelch ---
    let squelch_enabled_row = adw::SwitchRow::builder().title("Squelch").build();

    let squelch_adj = gtk4::Adjustment::new(
        DEFAULT_SQUELCH_DB,
        MIN_SQUELCH_DB,
        MAX_SQUELCH_DB,
        SQUELCH_STEP_DB,
        10.0,
        0.0,
    );
    let squelch_level_row = adw::SpinRow::builder()
        .title("Squelch Level")
        .subtitle("dB")
        .adjustment(&squelch_adj)
        .digits(0)
        .build();

    // --- De-emphasis ---
    let deemphasis_model =
        gtk4::StringList::new(&["None", "50 \u{00b5}s (EU)", "75 \u{00b5}s (US)"]);
    let deemphasis_row = adw::ComboRow::builder()
        .title("De-emphasis")
        .model(&deemphasis_model)
        .build();

    // --- Noise Blanker ---
    let noise_blanker_row = adw::SwitchRow::builder().title("Noise Blanker").build();

    // --- FM IF Noise Reduction ---
    let fm_if_nr_row = adw::SwitchRow::builder()
        .title("FM IF NR")
        .subtitle("IF noise reduction for FM modes")
        .build();

    group.add(&bandwidth_row);
    group.add(&squelch_enabled_row);
    group.add(&squelch_level_row);
    group.add(&deemphasis_row);
    group.add(&noise_blanker_row);
    group.add(&fm_if_nr_row);

    // TODO: Connect all rows to DSP pipeline (PR #7)

    RadioPanel {
        widget: group,
        bandwidth_row,
        squelch_enabled_row,
        squelch_level_row,
        deemphasis_row,
        noise_blanker_row,
        fm_if_nr_row,
    }
}

#[cfg(test)]
mod tests {
    /// Compile-time validation that bandwidth constants are consistent.
    const _: () = {
        assert!(super::MIN_BANDWIDTH_HZ <= super::MAX_BANDWIDTH_HZ);
        assert!(super::DEFAULT_BANDWIDTH_HZ >= super::MIN_BANDWIDTH_HZ);
        assert!(super::DEFAULT_BANDWIDTH_HZ <= super::MAX_BANDWIDTH_HZ);
        assert!(super::BANDWIDTH_STEP_HZ > 0.0);
    };

    /// Compile-time validation that squelch constants are consistent.
    const _: () = {
        assert!(super::MIN_SQUELCH_DB <= super::MAX_SQUELCH_DB);
        assert!(super::DEFAULT_SQUELCH_DB >= super::MIN_SQUELCH_DB);
        assert!(super::DEFAULT_SQUELCH_DB <= super::MAX_SQUELCH_DB);
        assert!(super::SQUELCH_STEP_DB > 0.0);
    };
}
