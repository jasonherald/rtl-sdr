//! Radio / demodulator configuration panel — bandwidth, squelch, de-emphasis.

use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_dsp::tone_detect::{CTCSS_DEFAULT_THRESHOLD, CTCSS_TONES_HZ};
use sdr_radio::af_chain::CtcssMode;

/// Default bandwidth in Hz.
const DEFAULT_BANDWIDTH_HZ: f64 = 12_500.0;
/// Minimum bandwidth in Hz.
const MIN_BANDWIDTH_HZ: f64 = 100.0;
/// Maximum bandwidth in Hz.
const MAX_BANDWIDTH_HZ: f64 = 250_000.0;
/// Bandwidth step in Hz.
const BANDWIDTH_STEP_HZ: f64 = 100.0;
/// Bandwidth page increment in Hz (scroll/page-up/down).
const BANDWIDTH_PAGE_HZ: f64 = 1_000.0;

/// Default notch filter frequency in Hz (US power line hum).
const DEFAULT_NOTCH_FREQ_HZ: f64 = 60.0;
/// Minimum notch filter frequency in Hz.
const MIN_NOTCH_FREQ_HZ: f64 = 20.0;
/// Maximum notch filter frequency in Hz.
const MAX_NOTCH_FREQ_HZ: f64 = 20_000.0;
/// Notch frequency step in Hz.
const NOTCH_FREQ_STEP_HZ: f64 = 10.0;
/// Notch frequency page increment in Hz.
const NOTCH_FREQ_PAGE_HZ: f64 = 100.0;

/// Default noise blanker level (threshold multiplier).
const DEFAULT_NB_LEVEL: f64 = 5.0;
/// Minimum noise blanker level.
const MIN_NB_LEVEL: f64 = 1.0;
/// Maximum noise blanker level.
const MAX_NB_LEVEL: f64 = 20.0;
/// Noise blanker level step.
const NB_LEVEL_STEP: f64 = 0.5;
/// Noise blanker page increment.
const NB_LEVEL_PAGE: f64 = 1.0;

/// Default CTCSS detection threshold (matches
/// [`sdr_dsp::tone_detect::CTCSS_DEFAULT_THRESHOLD`] = 0.1).
const DEFAULT_CTCSS_THRESHOLD: f64 = 0.1;
/// Minimum CTCSS threshold.
const MIN_CTCSS_THRESHOLD: f64 = 0.05;
/// Maximum CTCSS threshold — the DSP layer accepts up to 1.0 but
/// anything above ~0.5 is effectively unreachable for real tones,
/// so we cap the slider at 0.5 to keep the useful range legible.
const MAX_CTCSS_THRESHOLD: f64 = 0.5;
/// Step for keyboard increment / page-down.
const CTCSS_THRESHOLD_STEP: f64 = 0.01;
/// Page step (scroll / page-down).
const CTCSS_THRESHOLD_PAGE: f64 = 0.05;

/// Default squelch level in dB.
const DEFAULT_SQUELCH_DB: f64 = -100.0;
/// Minimum squelch level in dB.
const MIN_SQUELCH_DB: f64 = -160.0;
/// Maximum squelch level in dB.
const MAX_SQUELCH_DB: f64 = 0.0;
/// Squelch step in dB.
const SQUELCH_STEP_DB: f64 = 1.0;
/// Squelch page increment in dB.
const SQUELCH_PAGE_DB: f64 = 10.0;

/// Radio / demodulator configuration panel with references to interactive rows.
#[derive(Clone)]
pub struct RadioPanel {
    /// The `AdwPreferencesGroup` widget to pack into the sidebar.
    pub widget: adw::PreferencesGroup,
    /// Bandwidth control.
    pub bandwidth_row: adw::SpinRow,
    /// Squelch enable toggle.
    pub squelch_enabled_row: adw::SwitchRow,
    /// Squelch level control.
    pub squelch_level_row: adw::SpinRow,
    /// Auto-squelch toggle (noise floor tracking).
    pub auto_squelch_row: adw::SwitchRow,
    /// De-emphasis filter selector.
    pub deemphasis_row: adw::ComboRow,
    /// Noise blanker toggle.
    pub noise_blanker_row: adw::SwitchRow,
    /// Noise blanker level control.
    pub nb_level_row: adw::SpinRow,
    /// FM IF noise reduction toggle (visible only for FM modes).
    pub fm_if_nr_row: adw::SwitchRow,
    /// WFM stereo decode toggle (visible only for WFM mode).
    pub stereo_row: adw::SwitchRow,
    /// Notch filter enable toggle.
    pub notch_enabled_row: adw::SwitchRow,
    /// Notch filter frequency control.
    pub notch_freq_row: adw::SpinRow,
    /// CTCSS tone squelch selector. Entry 0 is "Off"; entries
    /// 1..=51 map directly to [`CTCSS_TONES_HZ`] one-to-one.
    /// Visible only when the demod mode is NFM — CTCSS is a
    /// sub-audible tone-squelch feature used exclusively on
    /// narrowband FM in practice.
    pub ctcss_row: adw::ComboRow,
    /// CTCSS detection threshold (`(0, 1]` normalized magnitude).
    /// Visible alongside `ctcss_row`.
    pub ctcss_threshold_row: adw::SpinRow,
    /// Read-only status indicator row that shows whether the
    /// detector's sustained gate is currently open. Updated from
    /// `DspToUi::CtcssSustainedChanged` messages via
    /// [`Self::set_ctcss_sustained`].
    pub ctcss_status_row: adw::ActionRow,
}

impl RadioPanel {
    /// Update mode-specific control visibility for the given demod mode.
    ///
    /// Centralizes FM/WFM visibility policy so startup and mode-switch
    /// handlers stay in sync.
    pub fn apply_demod_visibility(&self, mode: sdr_types::DemodMode) {
        let is_fm = matches!(mode, sdr_types::DemodMode::Wfm | sdr_types::DemodMode::Nfm);
        self.deemphasis_row.set_visible(is_fm);
        self.fm_if_nr_row.set_visible(is_fm);
        self.stereo_row
            .set_visible(mode == sdr_types::DemodMode::Wfm);
        // CTCSS is an NFM-only feature — WFM / AM / SSB / CW
        // either don't carry sub-audible tones or don't use them
        // as a squelch keying mechanism in practice.
        let ctcss_allowed = mode == sdr_types::DemodMode::Nfm;
        self.ctcss_row.set_visible(ctcss_allowed);
        self.ctcss_threshold_row.set_visible(ctcss_allowed);
        self.ctcss_status_row.set_visible(ctcss_allowed);

        // Leaving NFM must force the combo back to "Off" (index 0).
        // Without this, switching from NFM-with-a-tone to WFM would
        // hide the combo row while the AF chain continues to gate
        // the speaker path on the now-inapplicable detector — the
        // user sees "no audio" with no way to clear the state
        // because the control is hidden. Setting the combo to 0
        // fires the `selected-notify` signal wired in
        // `connect_radio_panel`, which sends `SetCtcssMode(Off)`
        // through to the DSP controller. GTK only emits the signal
        // on actual value change, so this is a no-op when CTCSS
        // was already Off.
        if !ctcss_allowed {
            self.ctcss_row.set_selected(0);
        }
    }

    /// Convert a combo-row selection index to a
    /// [`CtcssMode`]. Index 0 is `Off`; indices `1..=51` map to
    /// [`CTCSS_TONES_HZ`] entries. Out-of-range indices
    /// (shouldn't happen with the fixed 52-entry model) fall back
    /// to `Off`.
    #[must_use]
    pub fn ctcss_mode_from_index(index: u32) -> CtcssMode {
        if index == 0 {
            CtcssMode::Off
        } else if let Some(&hz) = CTCSS_TONES_HZ.get((index - 1) as usize) {
            CtcssMode::Tone(hz)
        } else {
            CtcssMode::Off
        }
    }

    /// Convert a [`CtcssMode`] back to a combo-row index. Used by
    /// the bookmark-restore path. `Tone(_)` with a non-table
    /// frequency (shouldn't happen, but serde lets anyone build
    /// the enum) falls back to `Off` (index 0).
    #[must_use]
    pub fn ctcss_index_from_mode(mode: CtcssMode) -> u32 {
        match mode {
            CtcssMode::Off => 0,
            CtcssMode::Tone(hz) => CTCSS_TONES_HZ
                .iter()
                .position(|&t| (t - hz).abs() < 0.01)
                .and_then(|i| u32::try_from(i + 1).ok())
                .unwrap_or(0),
        }
    }

    /// Update the CTCSS status row subtitle from the current
    /// combo selection and an explicit sustained-gate hint.
    ///
    /// Three states — in priority order:
    ///
    /// 1. **CTCSS combo is "Off"** → `"Off"` regardless of
    ///    `sustained`. This is load-bearing: the detector can
    ///    emit a `CtcssSustainedChanged(false)` edge when the
    ///    mode flips from `Tone` back to `Off` (because the
    ///    previous state was `true`), and the handler for that
    ///    edge calls right back into this method. Without the
    ///    Off-first guard we'd overwrite "Off" with
    ///    "Waiting for tone" and mislead the user into thinking
    ///    the detector is still running.
    /// 2. **Combo names a tone and `sustained == true`** →
    ///    `"Tone detected — gate open"`.
    /// 3. **Combo names a tone and `sustained == false`** →
    ///    `"Waiting for tone"`.
    ///
    /// Called from both the combo-change handler (with
    /// `sustained = false` — mode switches reset the detector)
    /// and the `DspToUi::CtcssSustainedChanged` edge handler
    /// (with the actual bool from the message).
    pub fn set_ctcss_sustained(&self, sustained: bool) {
        let text = if self.ctcss_row.selected() == 0 {
            "Off"
        } else if sustained {
            "Tone detected — gate open"
        } else {
            "Waiting for tone"
        };
        self.ctcss_status_row.set_subtitle(text);
    }
}

/// Build the radio / demodulator configuration panel.
#[allow(clippy::too_many_lines)]
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
        BANDWIDTH_PAGE_HZ,
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
        SQUELCH_PAGE_DB,
        0.0,
    );
    let squelch_level_row = adw::SpinRow::builder()
        .title("Squelch Level")
        .subtitle("dB")
        .adjustment(&squelch_adj)
        .digits(0)
        .build();

    // --- Auto-squelch ---
    let auto_squelch_row = adw::SwitchRow::builder()
        .title("Auto Squelch")
        .subtitle("Track noise floor automatically")
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

    // --- Noise Blanker Level ---
    let nb_level_adj = gtk4::Adjustment::new(
        DEFAULT_NB_LEVEL,
        MIN_NB_LEVEL,
        MAX_NB_LEVEL,
        NB_LEVEL_STEP,
        NB_LEVEL_PAGE,
        0.0,
    );
    let nb_level_row = adw::SpinRow::builder()
        .title("NB Level")
        .subtitle("Threshold multiplier")
        .adjustment(&nb_level_adj)
        .digits(1)
        .build();

    // --- FM IF Noise Reduction ---
    let fm_if_nr_row = adw::SwitchRow::builder()
        .title("FM IF NR")
        .subtitle("IF noise reduction for FM modes")
        .build();

    // --- WFM Stereo ---
    let stereo_row = adw::SwitchRow::builder()
        .title("Stereo")
        .subtitle("WFM stereo decode")
        .visible(false) // Only shown in WFM mode
        .build();

    // --- Notch Filter ---
    let notch_enabled_row = adw::SwitchRow::builder()
        .title("Notch Filter")
        .subtitle("Remove interference tones")
        .build();

    let notch_freq_adj = gtk4::Adjustment::new(
        DEFAULT_NOTCH_FREQ_HZ,
        MIN_NOTCH_FREQ_HZ,
        MAX_NOTCH_FREQ_HZ,
        NOTCH_FREQ_STEP_HZ,
        NOTCH_FREQ_PAGE_HZ,
        0.0,
    );
    let notch_freq_row = adw::SpinRow::builder()
        .title("Notch Frequency")
        .subtitle("Hz")
        .adjustment(&notch_freq_adj)
        .digits(0)
        .build();

    // --- CTCSS tone squelch ---
    // Build the combo model with "Off" followed by the 51 CTCSS
    // tones. Each tone is labelled to one decimal place (matching
    // the hardware convention — e.g. "100.0 Hz", "151.4 Hz").
    let mut ctcss_labels: Vec<String> = Vec::with_capacity(CTCSS_TONES_HZ.len() + 1);
    ctcss_labels.push("Off".to_string());
    for &tone in CTCSS_TONES_HZ {
        ctcss_labels.push(format!("{tone:.1} Hz"));
    }
    let ctcss_label_refs: Vec<&str> = ctcss_labels.iter().map(String::as_str).collect();
    let ctcss_model = gtk4::StringList::new(&ctcss_label_refs);
    let ctcss_row = adw::ComboRow::builder()
        .title("CTCSS Tone Squelch")
        .subtitle("Sub-audible tone required to open squelch")
        .model(&ctcss_model)
        .visible(false) // NFM-only; startup mode sets it
        .build();

    let ctcss_threshold_adj = gtk4::Adjustment::new(
        DEFAULT_CTCSS_THRESHOLD,
        MIN_CTCSS_THRESHOLD,
        MAX_CTCSS_THRESHOLD,
        CTCSS_THRESHOLD_STEP,
        CTCSS_THRESHOLD_PAGE,
        0.0,
    );
    let ctcss_threshold_row = adw::SpinRow::builder()
        .title("CTCSS Threshold")
        .subtitle("Higher = more conservative (fewer false triggers)")
        .adjustment(&ctcss_threshold_adj)
        .digits(2)
        .visible(false)
        .build();
    // Debug assert the default matches the DSP layer at startup —
    // a future bump to CTCSS_DEFAULT_THRESHOLD should be
    // accompanied by a bump to DEFAULT_CTCSS_THRESHOLD so the
    // slider and the detector agree on the un-tuned default.
    debug_assert!(
        (DEFAULT_CTCSS_THRESHOLD - f64::from(CTCSS_DEFAULT_THRESHOLD)).abs() < 1e-6,
        "UI default CTCSS threshold diverged from DSP default"
    );

    let ctcss_status_row = adw::ActionRow::builder()
        .title("CTCSS Status")
        .subtitle("Off")
        .visible(false)
        .build();

    group.add(&bandwidth_row);
    group.add(&squelch_enabled_row);
    group.add(&squelch_level_row);
    group.add(&auto_squelch_row);
    group.add(&deemphasis_row);
    group.add(&noise_blanker_row);
    group.add(&nb_level_row);
    group.add(&fm_if_nr_row);
    group.add(&stereo_row);
    group.add(&notch_enabled_row);
    group.add(&notch_freq_row);
    group.add(&ctcss_row);
    group.add(&ctcss_threshold_row);
    group.add(&ctcss_status_row);

    // All rows connected to DSP pipeline via window.rs

    RadioPanel {
        widget: group,
        bandwidth_row,
        squelch_enabled_row,
        squelch_level_row,
        auto_squelch_row,
        deemphasis_row,
        noise_blanker_row,
        nb_level_row,
        fm_if_nr_row,
        stereo_row,
        notch_enabled_row,
        notch_freq_row,
        ctcss_row,
        ctcss_threshold_row,
        ctcss_status_row,
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

    /// Compile-time validation that NB level constants are consistent.
    const _: () = {
        assert!(super::MIN_NB_LEVEL >= 1.0); // NoiseBlanker requires >= 1.0
        assert!(super::MIN_NB_LEVEL <= super::MAX_NB_LEVEL);
        assert!(super::DEFAULT_NB_LEVEL >= super::MIN_NB_LEVEL);
        assert!(super::DEFAULT_NB_LEVEL <= super::MAX_NB_LEVEL);
        assert!(super::NB_LEVEL_STEP > 0.0);
        assert!(super::NB_LEVEL_PAGE > 0.0);
    };

    /// Compile-time validation that notch frequency constants are consistent.
    const _: () = {
        assert!(super::MIN_NOTCH_FREQ_HZ <= super::MAX_NOTCH_FREQ_HZ);
        assert!(super::DEFAULT_NOTCH_FREQ_HZ >= super::MIN_NOTCH_FREQ_HZ);
        assert!(super::DEFAULT_NOTCH_FREQ_HZ <= super::MAX_NOTCH_FREQ_HZ);
        assert!(super::NOTCH_FREQ_STEP_HZ > 0.0);
        assert!(super::NOTCH_FREQ_PAGE_HZ > 0.0);
    };
}
