//! Radio / demodulator configuration panel — bandwidth, squelch, de-emphasis.

use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_dsp::tone_detect::{CTCSS_DEFAULT_THRESHOLD, CTCSS_TONES_HZ};
use sdr_dsp::voice_squelch::{
    VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB, VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
    VoiceSquelchMode,
};
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

// ─── Voice squelch UI tuning ──────────────────────────────────
//
// The threshold spin row has to cover two different units (a
// normalized envelope ratio for Syllabic, dB for SNR), so we
// keep per-mode min/max/step/default constants and update the
// adjustment when the mode changes.

/// Combo row indices for the voice-squelch selector. Must match
/// the order of [`VOICE_SQUELCH_MODE_LABELS`] below. `pub(crate)`
/// so `window.rs` can translate the selection back to a
/// [`VoiceSquelchMode`] at `BackendConfig` build time without
/// re-deriving the match.
pub(crate) const VOICE_SQUELCH_OFF_IDX: u32 = 0;
pub(crate) const VOICE_SQUELCH_SYLLABIC_IDX: u32 = 1;
pub(crate) const VOICE_SQUELCH_SNR_IDX: u32 = 2;
/// User-visible combo labels. Order matches the `*_IDX` constants.
const VOICE_SQUELCH_MODE_LABELS: &[&str] = &["Off", "Syllabic", "SNR ratio"];

/// Syllabic threshold range — normalized envelope ratio. The
/// DSP default is `VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD`
/// (0.15). Range picked to cover the useful tuning window:
/// below 0.05 even hiss opens the gate, above 0.5 clear speech
/// is often rejected.
const SYLLABIC_THRESHOLD_MIN: f64 = 0.05;
const SYLLABIC_THRESHOLD_MAX: f64 = 0.50;
const SYLLABIC_THRESHOLD_STEP: f64 = 0.01;
const SYLLABIC_THRESHOLD_PAGE: f64 = 0.05;

/// SNR threshold range — dB above the out-of-voice-band noise
/// floor. DSP default is `VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB`
/// (6.0 dB = 2× ratio). Below 0 dB the gate is trivially
/// satisfied by any broadband noise; above 20 dB you need a
/// near-studio-quality signal to open.
const SNR_THRESHOLD_DB_MIN: f64 = 0.0;
const SNR_THRESHOLD_DB_MAX: f64 = 20.0;
const SNR_THRESHOLD_DB_STEP: f64 = 0.5;
const SNR_THRESHOLD_DB_PAGE: f64 = 2.0;

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
    /// "Reset bandwidth to default for current demod mode" button,
    /// packed as a suffix on `bandwidth_row`. Sensitive only when
    /// the current bandwidth differs from the mode's default;
    /// otherwise the button grays out so it doesn't lie about
    /// having something to do. Per issue #341.
    pub bandwidth_reset_button: gtk4::Button,
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
    /// Voice-activity squelch mode selector. Off / Syllabic /
    /// SNR ratio. The threshold spin row below relabels and
    /// re-ranges based on the selection — one row, two units.
    pub voice_squelch_row: adw::ComboRow,
    /// Voice-squelch threshold. Range + subtitle change based
    /// on the mode selected above; see
    /// [`Self::apply_voice_squelch_mode_ui`].
    pub voice_squelch_threshold_row: adw::SpinRow,
    /// Read-only status row for the voice squelch gate. Updated
    /// from `DspToUi::VoiceSquelchOpenChanged` via
    /// [`Self::set_voice_squelch_open`].
    pub voice_squelch_status_row: adw::ActionRow,
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

        // Voice squelch is also NFM-oriented. Syllabic is
        // designed around human speech cadence and Snr keys on
        // a voice-band-centered BPF — neither makes sense on
        // WFM broadcast or SSB where the audio content is
        // structurally different.
        //
        // Unlike CTCSS (which force-clears the combo on leave-
        // NFM), voice squelch PRESERVES the user's selection
        // across non-NFM transitions. The DSP layer has a
        // matching gate in `RadioModule::set_mode` that forces
        // the live AF chain to `Off` for non-NFM modes while
        // keeping the cached mode intact. So on WFM the combo
        // still shows "Syllabic" (or whatever the user picked)
        // but the detector isn't actually running — and on NFM
        // re-entry everything re-arms automatically without the
        // user having to reselect the mode.
        //
        // The rows just hide/show based on demod mode. The
        // combo selection is left alone — the user's
        // configuration survives round-trips through non-NFM
        // bands.
        let voice_squelch_allowed = mode == sdr_types::DemodMode::Nfm;
        self.voice_squelch_row.set_visible(voice_squelch_allowed);
        self.voice_squelch_status_row
            .set_visible(voice_squelch_allowed);
        // Threshold row visibility depends on BOTH the demod
        // mode (must allow voice squelch) AND the current voice
        // squelch mode (must be active, not Off). When the mode
        // is Off the row is hidden even on NFM.
        let voice_squelch_active = self.voice_squelch_row.selected() != VOICE_SQUELCH_OFF_IDX;
        self.voice_squelch_threshold_row
            .set_visible(voice_squelch_allowed && voice_squelch_active);
        // On re-entry to NFM with a cached active voice-squelch
        // mode, the status row subtitle might still say
        // "Signal present — gate open" from the last session if
        // the DSP detector happened to be open when the user
        // last left NFM. The fresh AF chain starts closed, so
        // reset the label to the mode-appropriate "waiting"
        // text. The first real DSP edge after re-entry will
        // override this if the detector actually opens.
        if voice_squelch_allowed && voice_squelch_active {
            self.set_voice_squelch_open(false);
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

    /// Convert a voice-squelch combo index + current threshold
    /// value to a [`VoiceSquelchMode`]. Out-of-range indices
    /// (shouldn't happen with the fixed 3-entry model) fall
    /// back to `Off` — same contract as CTCSS.
    ///
    /// **Important**: the caller must ensure `threshold` is in
    /// the correct units for the target `index`. Syllabic
    /// expects a normalized envelope ratio (~0.05–0.50); Snr
    /// expects a dB value (~0.0–20.0). Passing 0.15 to Snr or
    /// 6.0 to Syllabic would leave the detector far outside its
    /// tuning range and either always-open or never-open.
    ///
    /// Used by two call sites with different threshold sources:
    ///
    /// - **Save path** (bookmark save) — threshold is read from
    ///   the spin row, which is already in the current mode's
    ///   units, so the combo index and threshold are in sync.
    /// - **Restore path** (bookmark load) — threshold is
    ///   extracted from the persisted [`VoiceSquelchMode`] enum
    ///   which carries it inline in the correct units.
    ///
    /// For the **mode-change** path (user flips the combo), the
    /// caller must use [`Self::voice_squelch_default_threshold_for_index`]
    /// to get the target mode's per-variant default, NOT the
    /// current spin-row value from the previous mode. Otherwise
    /// the units don't match.
    #[must_use]
    pub fn voice_squelch_mode_from_index(index: u32, threshold: f32) -> VoiceSquelchMode {
        match index {
            VOICE_SQUELCH_SYLLABIC_IDX => VoiceSquelchMode::Syllabic { threshold },
            VOICE_SQUELCH_SNR_IDX => VoiceSquelchMode::Snr {
                threshold_db: threshold,
            },
            _ => VoiceSquelchMode::Off,
        }
    }

    /// Return the default threshold for a voice-squelch combo
    /// index, in the correct units for that variant. Used by
    /// the combo-change signal handler in `window.rs` to seed
    /// a mode switch with a sane per-mode default rather than
    /// carrying the previous mode's threshold (which would be in
    /// the wrong units).
    ///
    /// Returns 0.0 for `Off` because `Off` has no threshold to
    /// apply — the caller should ignore the value on that path.
    #[must_use]
    pub fn voice_squelch_default_threshold_for_index(index: u32) -> f32 {
        match index {
            VOICE_SQUELCH_SYLLABIC_IDX => VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
            VOICE_SQUELCH_SNR_IDX => VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
            _ => 0.0,
        }
    }

    /// Inverse of [`Self::voice_squelch_mode_from_index`] — used
    /// by bookmark restore to map a persisted mode back to a
    /// combo index.
    #[must_use]
    pub fn voice_squelch_index_from_mode(mode: VoiceSquelchMode) -> u32 {
        match mode {
            VoiceSquelchMode::Off => VOICE_SQUELCH_OFF_IDX,
            VoiceSquelchMode::Syllabic { .. } => VOICE_SQUELCH_SYLLABIC_IDX,
            VoiceSquelchMode::Snr { .. } => VOICE_SQUELCH_SNR_IDX,
        }
    }

    /// Extract the current threshold value from a mode. `Off`
    /// has no threshold; we return the syllabic default so the
    /// caller can plug it into the spin row without a special
    /// case. Syllabic and Snr return their inline value.
    #[must_use]
    pub fn voice_squelch_threshold_from_mode(mode: VoiceSquelchMode) -> f32 {
        match mode {
            VoiceSquelchMode::Off => VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
            VoiceSquelchMode::Syllabic { threshold } => threshold,
            VoiceSquelchMode::Snr { threshold_db } => threshold_db,
        }
    }

    /// Reconfigure the threshold spin row's adjustment for the
    /// given mode — each voice-squelch variant uses different
    /// units (normalized ratio vs dB) and different ranges.
    /// Called on startup and whenever the combo row changes.
    ///
    /// The threshold spin row is hidden in Off mode (nothing to
    /// tune) and shown in Syllabic / Snr mode with the right
    /// subtitle and adjustment range.
    pub fn apply_voice_squelch_mode_ui(&self, mode: VoiceSquelchMode) {
        match mode {
            VoiceSquelchMode::Off => {
                self.voice_squelch_threshold_row.set_visible(false);
                self.voice_squelch_status_row.set_subtitle("Off");
            }
            VoiceSquelchMode::Syllabic { threshold } => {
                self.voice_squelch_threshold_row.set_visible(true);
                self.voice_squelch_threshold_row
                    .set_subtitle("Envelope ratio (0.05 = permissive, 0.5 = strict)");
                let adj = gtk4::Adjustment::new(
                    f64::from(threshold),
                    SYLLABIC_THRESHOLD_MIN,
                    SYLLABIC_THRESHOLD_MAX,
                    SYLLABIC_THRESHOLD_STEP,
                    SYLLABIC_THRESHOLD_PAGE,
                    0.0,
                );
                self.voice_squelch_threshold_row.set_adjustment(Some(&adj));
                self.voice_squelch_threshold_row.set_digits(2);
                self.voice_squelch_status_row
                    .set_subtitle("Waiting for speech");
            }
            VoiceSquelchMode::Snr { threshold_db } => {
                self.voice_squelch_threshold_row.set_visible(true);
                self.voice_squelch_threshold_row
                    .set_subtitle("dB above noise floor (0 = permissive, 20 = strict)");
                let adj = gtk4::Adjustment::new(
                    f64::from(threshold_db),
                    SNR_THRESHOLD_DB_MIN,
                    SNR_THRESHOLD_DB_MAX,
                    SNR_THRESHOLD_DB_STEP,
                    SNR_THRESHOLD_DB_PAGE,
                    0.0,
                );
                self.voice_squelch_threshold_row.set_adjustment(Some(&adj));
                self.voice_squelch_threshold_row.set_digits(1);
                self.voice_squelch_status_row
                    .set_subtitle("Waiting for signal");
            }
        }
    }

    /// Update the voice-squelch status row from a gate edge
    /// event. Off-mode guarded: if the combo currently says Off
    /// we keep the "Off" subtitle regardless of the incoming
    /// bool, matching CTCSS's Off-first pattern.
    pub fn set_voice_squelch_open(&self, open: bool) {
        if self.voice_squelch_row.selected() == VOICE_SQUELCH_OFF_IDX {
            self.voice_squelch_status_row.set_subtitle("Off");
            return;
        }
        self.voice_squelch_status_row.set_subtitle(if open {
            "Signal present — gate open"
        } else {
            match self.voice_squelch_row.selected() {
                VOICE_SQUELCH_SYLLABIC_IDX => "Waiting for speech",
                VOICE_SQUELCH_SNR_IDX => "Waiting for signal",
                _ => "Waiting",
            }
        });
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

    // "Reset bandwidth to default for current demod mode" —
    // packed as a suffix so it sits inline with the spin row.
    // Flat + valign(Center) matches the affordance pattern other
    // sidebar rows use for secondary actions.
    let bandwidth_reset_button = gtk4::Button::builder()
        .icon_name("edit-undo-symbolic")
        .tooltip_text("Reset bandwidth to default for current demod mode")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        // Start insensitive — the initial bandwidth is the
        // mode default. The value-notify + DemodModeChanged
        // handlers in window.rs update sensitivity from here.
        .sensitive(false)
        .build();
    bandwidth_reset_button.update_property(&[gtk4::accessible::Property::Label(
        "Reset bandwidth to default",
    )]);
    bandwidth_row.add_suffix(&bandwidth_reset_button);

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

    // --- Voice squelch ---
    // Three-entry combo: Off / Syllabic / SNR ratio. Threshold
    // spin row + status row start hidden (Off is the default);
    // they're revealed by `apply_voice_squelch_mode_ui` when
    // the combo changes to an active mode.
    let voice_squelch_model = gtk4::StringList::new(VOICE_SQUELCH_MODE_LABELS);
    let voice_squelch_row = adw::ComboRow::builder()
        .title("Voice squelch")
        .subtitle("Speech / signal detector, gates alongside CTCSS")
        .model(&voice_squelch_model)
        // Start hidden — `apply_demod_visibility` reveals it on
        // NFM. Without this the row would flash briefly on the
        // default non-NFM startup path before the visibility
        // handler kicks in, mirroring the CTCSS pattern.
        .visible(false)
        .build();

    // Threshold spin row — starts in syllabic-default range but
    // the adjustment is overwritten by `apply_voice_squelch_mode_ui`
    // whenever the mode changes, so the initial range is just a
    // placeholder.
    let voice_squelch_threshold_adj = gtk4::Adjustment::new(
        f64::from(VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD),
        SYLLABIC_THRESHOLD_MIN,
        SYLLABIC_THRESHOLD_MAX,
        SYLLABIC_THRESHOLD_STEP,
        SYLLABIC_THRESHOLD_PAGE,
        0.0,
    );
    let voice_squelch_threshold_row = adw::SpinRow::builder()
        .title("Voice squelch threshold")
        .subtitle("Select a mode first")
        .adjustment(&voice_squelch_threshold_adj)
        .digits(2)
        .visible(false)
        .build();

    let voice_squelch_status_row = adw::ActionRow::builder()
        .title("Voice squelch status")
        .subtitle("Off")
        .visible(false)
        .build();

    // Sanity-check that the DSP-layer defaults haven't drifted
    // from the UI's tuning range. If someone bumps the DSP
    // default out of the UI range, this debug_assert forces them
    // to update the UI bounds too.
    debug_assert!(
        f64::from(VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD) >= SYLLABIC_THRESHOLD_MIN
            && f64::from(VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD) <= SYLLABIC_THRESHOLD_MAX,
        "syllabic default threshold outside UI range"
    );
    debug_assert!(
        f64::from(VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB) >= SNR_THRESHOLD_DB_MIN
            && f64::from(VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB) <= SNR_THRESHOLD_DB_MAX,
        "SNR default threshold outside UI range"
    );

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
    group.add(&voice_squelch_row);
    group.add(&voice_squelch_threshold_row);
    group.add(&voice_squelch_status_row);

    // All rows connected to DSP pipeline via window.rs

    RadioPanel {
        widget: group,
        bandwidth_row,
        bandwidth_reset_button,
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
        voice_squelch_row,
        voice_squelch_threshold_row,
        voice_squelch_status_row,
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
