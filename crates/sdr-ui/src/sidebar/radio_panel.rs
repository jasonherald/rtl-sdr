//! Radio / demodulator configuration panel — bandwidth, squelch, de-emphasis.

use std::cell::Cell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_dsp::propagation::{fspl_distance_m, watts_to_dbm};
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

// ─── Distance estimator (FSPL) tuning ────────────────────────
//
// Config keys for the two user-settable inputs (ticket #164).
// Persisted as top-level entries in the main JSON config rather
// than inside a bookmark's `TuningProfile` because ERP and
// receiver calibration are properties of the station + receiver
// setup, not per-channel: a user's antenna/receiver chain has one
// calibration offset, and typical usage is to dial in a known
// transmitter's power once and estimate distances for whatever
// channel they're on.

/// Config key for the FSPL distance estimator's transmitter ERP,
/// stored as watts. Public so `window.rs` can persist the row's
/// value without re-typing the literal.
pub const KEY_RADIO_DISTANCE_ERP_WATTS: &str = "radio_distance_erp_watts";

/// Config key for the FSPL distance estimator's receiver
/// calibration offset, stored as dB.
pub const KEY_RADIO_DISTANCE_CALIBRATION_DB: &str = "radio_distance_calibration_db";

// Transmitter effective radiated power (ERP) bounds. 25 W is a
// reasonable default — most mobile public-safety radios (police,
// fire, EMS) ship at 25-50 W, handhelds at 1-5 W, broadcast
// transmitters up to ~100 kW. The spin row covers the useful
// range without restricting experimentation.

/// Default transmitter ERP in watts.
const DEFAULT_ERP_WATTS: f64 = 25.0;
/// Minimum ERP. Below this a user is probably mis-typing.
const MIN_ERP_WATTS: f64 = 0.001;
/// Maximum ERP — covers high-power FM broadcast.
const MAX_ERP_WATTS: f64 = 100_000.0;
/// Step size for small-knob tuning.
const ERP_STEP_WATTS: f64 = 1.0;
/// Page step for the scroll / page-down keys.
const ERP_PAGE_WATTS: f64 = 10.0;

/// Default receiver-chain calibration offset in dB. The FSPL
/// formula assumes the received level in dBm is calibrated — most
/// RTL-SDRs report relative dBFS with an arbitrary reference, so
/// this slider lets the user dial in an offset until a known
/// reference signal's distance reads correctly.
const DEFAULT_CALIBRATION_DB: f64 = 0.0;
/// Minimum calibration offset. ±30 dB covers every reasonable
/// RTL-SDR reference-level scenario we've seen.
const MIN_CALIBRATION_DB: f64 = -30.0;
/// Maximum calibration offset.
const MAX_CALIBRATION_DB: f64 = 30.0;
/// Calibration offset step.
const CALIBRATION_STEP_DB: f64 = 0.5;
/// Calibration offset page step.
const CALIBRATION_PAGE_DB: f64 = 5.0;

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
    /// The `AdwPreferencesPage` widget packed into the Radio
    /// activity stack slot. The page hosts five titled
    /// `AdwPreferencesGroup`s (Bandwidth / Squelch / Filters /
    /// De-emphasis / CTCSS) — see [`build_radio_panel`].
    pub widget: adw::PreferencesPage,
    /// De-emphasis section group. Stored as a handle so
    /// [`apply_demod_visibility`] can show/hide the whole section
    /// instead of the single row inside it; cleaner visual rhythm
    /// than a titled group with one hidden child taking up a row
    /// of whitespace on AM / SSB / CW.
    ///
    /// [`apply_demod_visibility`]: Self::apply_demod_visibility
    pub deemphasis_group: adw::PreferencesGroup,
    /// CTCSS section group — NFM-only. Hidden as a group for the
    /// same reason as [`deemphasis_group`].
    ///
    /// [`deemphasis_group`]: Self::deemphasis_group
    pub ctcss_group: adw::PreferencesGroup,
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
    /// Transmitter effective radiated power (watts) — input to the
    /// FSPL distance estimator. Persisted to config.
    pub erp_row: adw::SpinRow,
    /// Receiver calibration offset (dB). Shifts the raw signal
    /// level before computing path loss. Persisted to config.
    pub calibration_row: adw::SpinRow,
    /// Read-only display row whose subtitle shows the current
    /// distance estimate. Value set by [`Self::update_distance_display`].
    pub distance_row: adw::ActionRow,
    /// Cached most-recent signal level (dB). Used by the ERP /
    /// calibration value-changed handlers so the distance display
    /// refreshes immediately when the user tweaks a knob, even if
    /// no new `SignalLevel` message arrives in between.
    ///
    /// `Rc<Cell<_>>` (not plain `Cell<_>`) so that cloning
    /// `RadioPanel` shares the cache across clones — the derive
    /// on plain `Cell` would produce disconnected caches.
    pub distance_last_signal_db: Rc<Cell<Option<f32>>>,
    /// Cached most-recent tuner centre frequency (Hz). Same
    /// rationale as `distance_last_signal_db`.
    pub distance_last_frequency_hz: Rc<Cell<Option<f64>>>,
}

impl RadioPanel {
    /// Update mode-specific control visibility for the given demod mode.
    ///
    /// Centralizes FM/WFM visibility policy so startup and mode-switch
    /// handlers stay in sync.
    pub fn apply_demod_visibility(&self, mode: sdr_types::DemodMode) {
        let is_fm = matches!(mode, sdr_types::DemodMode::Wfm | sdr_types::DemodMode::Nfm);
        // De-emphasis group: hide the whole section on AM / SSB /
        // CW. The per-row `deemphasis_row.set_visible(...)` is
        // retained as a belt for screen readers (the row stays
        // hidden even if a future refactor moves the group
        // around).
        self.deemphasis_group.set_visible(is_fm);
        self.deemphasis_row.set_visible(is_fm);
        self.fm_if_nr_row.set_visible(is_fm);
        self.stereo_row
            .set_visible(mode == sdr_types::DemodMode::Wfm);
        // CTCSS is an NFM-only feature — WFM / AM / SSB / CW
        // either don't carry sub-audible tones or don't use them
        // as a squelch keying mechanism in practice. Hide the
        // whole group; individual `set_visible` kept for the
        // same defensive reason as de-emphasis.
        let ctcss_allowed = mode == sdr_types::DemodMode::Nfm;
        self.ctcss_group.set_visible(ctcss_allowed);
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

    /// Cache a fresh signal level and recompute the distance
    /// estimate. Called from the `DspToUi::SignalLevel` handler
    /// in `window.rs` on every level update (~10 Hz).
    pub fn update_distance_from_signal(&self, signal_db: f32, frequency_hz: f64) {
        self.distance_last_signal_db.set(Some(signal_db));
        self.distance_last_frequency_hz.set(Some(frequency_hz));
        self.refresh_distance_display();
    }

    /// Cache a new tuner frequency and recompute the distance
    /// estimate. Called from the tuner-change handler in
    /// `window.rs`.
    pub fn update_distance_frequency(&self, frequency_hz: f64) {
        self.distance_last_frequency_hz.set(Some(frequency_hz));
        self.refresh_distance_display();
    }

    /// Recompute and render the distance display from the cached
    /// signal/frequency and the current ERP / calibration row
    /// values. Called by the setters above and by the ERP /
    /// calibration spin-row value-notify handlers.
    pub fn refresh_distance_display(&self) {
        let state = DistanceDisplay::compute(
            self.distance_last_signal_db.get(),
            self.distance_last_frequency_hz.get(),
            self.erp_row.value(),
            self.calibration_row.value(),
        );
        self.distance_row.set_subtitle(&state.format());
    }
}

/// Standalone version of [`RadioPanel::refresh_distance_display`]
/// usable from inside `build_radio_panel` before the `RadioPanel`
/// struct has been materialised — wired to the ERP / calibration
/// value-notify signals so a knob twiddle refreshes the display
/// immediately. Both variants route through [`DistanceDisplay`]
/// so the state machine stays in one place.
fn refresh_distance_display_standalone(
    erp_row: &adw::SpinRow,
    calibration_row: &adw::SpinRow,
    distance_row: &adw::ActionRow,
    last_signal_db: Option<f32>,
    last_frequency_hz: Option<f64>,
) {
    let state = DistanceDisplay::compute(
        last_signal_db,
        last_frequency_hz,
        erp_row.value(),
        calibration_row.value(),
    );
    distance_row.set_subtitle(&state.format());
}

/// Maximum distance in metres the formatter will print as a
/// number. The longest great-circle path on Earth is ~20,015 km;
/// above this the FSPL math is producing a physically
/// meaningless result (almost always because path loss is
/// implying a distance bigger than any RF can meaningfully
/// travel from a terrestrial source).
const MAX_MEANINGFUL_DISTANCE_M: f64 = 20_000_000.0;

/// Calibrated received-power threshold (dBm) below which we
/// assume there is no active signal to estimate a distance from.
/// Slightly above the theoretical MDS of a sensitive narrowband
/// receiver (~-130 dBm for a commercial VHF/UHF set, a bit
/// better for lab gear). Anything below this is dominated by
/// noise or is the pipeline reporting a squelch-gated floor —
/// we label it "no active signal" rather than stretching FSPL
/// into sci-fi territory.
const NO_ACTIVE_SIGNAL_DBM: f64 = -130.0;

/// At or above this distance (metres) the distance display
/// switches from "N m" to "N.N km". Mirrors the naming pattern
/// in `antenna.rs` for unit-scaling thresholds.
const KM_THRESHOLD_M: f64 = 1_000.0;

/// At or above this distance (metres) single-km precision is
/// meaningless — FSPL idealisation swamps any third-significant-
/// digit stability — so the display rounds to the nearest 10 km.
const MEGAMETRE_THRESHOLD_M: f64 = 1_000_000.0;

/// Rounding granularity (in km) for distances at or above
/// `MEGAMETRE_THRESHOLD_M`. Kept as a named constant so the
/// "why round to 10 km" rationale lives next to the value.
const MEGAMETRE_ROUND_KM: f64 = 10.0;

/// Kilometres per metre, factored out so the formatter's intent
/// reads cleanly without magic literals sharing the numeric
/// literal `1_000.0` with `KM_THRESHOLD_M`.
const METRES_PER_KM: f64 = 1_000.0;

/// Distinct visual states the distance display can be in.
/// Split out so the logic is explicit and test-covered rather
/// than buried in a single formatter function that had to
/// overload "—" for several semantically different cases.
#[derive(Debug, PartialEq)]
enum DistanceDisplay {
    /// No signal level has ever flowed yet — source not running
    /// or panel freshly constructed.
    NoData,
    /// Calibrated received level is below receiver sensitivity,
    /// so there is nothing real to measure. Typical cause:
    /// squelch gated, source pointed at an empty channel, or
    /// hardware disconnected.
    NoActiveSignal,
    /// Received level ≥ transmitted ERP — physically impossible
    /// under FSPL. The user has a calibration problem (receiver
    /// cal offset too large, or ERP set too low for the actual
    /// transmitter).
    CheckCalibration,
    /// A signal is present above the sensitivity threshold but
    /// path loss implies a distance greater than Earth's great-
    /// circle maximum — the estimator has saturated.
    TooWeak,
    /// Meaningful distance in metres, safe to print as a number.
    Value(f64),
}

impl DistanceDisplay {
    /// Decide the display state from the live inputs. All four
    /// fields-that-matter (last signal, last frequency, ERP,
    /// calibration offset) get threaded through explicitly so
    /// tests can pin every transition without constructing a
    /// full `RadioPanel`.
    fn compute(
        signal_db: Option<f32>,
        frequency_hz: Option<f64>,
        erp_watts: f64,
        cal_db: f64,
    ) -> Self {
        let (Some(raw_signal_db), Some(freq)) = (signal_db, frequency_hz) else {
            return Self::NoData;
        };
        let received_dbm = f64::from(raw_signal_db) + cal_db;
        if !received_dbm.is_finite() || received_dbm < NO_ACTIVE_SIGNAL_DBM {
            return Self::NoActiveSignal;
        }
        let erp_dbm = watts_to_dbm(erp_watts);
        let d = fspl_distance_m(erp_dbm, received_dbm, freq);
        if !d.is_finite() || d < f64::EPSILON {
            // `fspl_distance_m` returns 0.0 when received ≥ ERP
            // (i.e., the user is receiving stronger than the
            // transmitter putatively radiates — only reachable
            // by miscalibrated inputs).
            return Self::CheckCalibration;
        }
        if d > MAX_MEANINGFUL_DISTANCE_M {
            return Self::TooWeak;
        }
        Self::Value(d)
    }

    /// Render the state as the subtitle text for the
    /// `distance_row`. Keeps wording in one place so changes
    /// don't drift between the panel helpers and test assertions.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn format(&self) -> String {
        match *self {
            Self::NoData => "—".to_string(),
            Self::NoActiveSignal => "No active signal".to_string(),
            Self::CheckCalibration => "Check calibration".to_string(),
            Self::TooWeak => "Too weak to measure".to_string(),
            Self::Value(d) => {
                if d < KM_THRESHOLD_M {
                    format!("{} m", d.round() as u64)
                } else if d < MEGAMETRE_THRESHOLD_M {
                    format!("{:.1} km", d / METRES_PER_KM)
                } else {
                    let km_rounded =
                        (d / METRES_PER_KM / MEGAMETRE_ROUND_KM).round() * MEGAMETRE_ROUND_KM;
                    format!("{km_rounded:.0} km")
                }
            }
        }
    }
}

#[cfg(test)]
mod distance_display_tests {
    use super::{DistanceDisplay, MAX_MEANINGFUL_DISTANCE_M};

    // ERP for the scenarios below: a 25 W public-safety mobile,
    // which is one of the defaults we suggest in the UI. Doesn't
    // affect any of the state-transition boundaries this module
    // tests — just needs to be a realistic value.
    const TEST_ERP_WATTS: f64 = 25.0;
    const TEST_FREQ_HZ: f64 = 155e6;

    #[test]
    fn no_data_when_signal_cache_empty() {
        let s = DistanceDisplay::compute(None, Some(TEST_FREQ_HZ), TEST_ERP_WATTS, 0.0);
        assert_eq!(s, DistanceDisplay::NoData);
        assert_eq!(s.format(), "—");
    }

    #[test]
    fn no_data_when_frequency_missing() {
        let s = DistanceDisplay::compute(Some(-80.0), None, TEST_ERP_WATTS, 0.0);
        assert_eq!(s, DistanceDisplay::NoData);
    }

    #[test]
    fn no_active_signal_below_receiver_sensitivity() {
        // -120 dB raw + -20 dB calibration → -140 dBm received,
        // below our -130 dBm "active signal" threshold. This
        // matches the exact user report of "squelch on, no audio,
        // showing millions of km" (ticket #164 user feedback).
        let s = DistanceDisplay::compute(Some(-120.0), Some(TEST_FREQ_HZ), TEST_ERP_WATTS, -20.0);
        assert_eq!(s, DistanceDisplay::NoActiveSignal);
        assert_eq!(s.format(), "No active signal");
    }

    #[test]
    fn check_calibration_when_received_exceeds_transmitted() {
        // 25 W ERP ≈ 44 dBm. Received at +50 dBm is louder than
        // the transmitter — impossible under FSPL; user needs to
        // check their calibration offset or ERP value.
        let s = DistanceDisplay::compute(Some(50.0), Some(TEST_FREQ_HZ), TEST_ERP_WATTS, 0.0);
        assert_eq!(s, DistanceDisplay::CheckCalibration);
        assert_eq!(s.format(), "Check calibration");
    }

    #[test]
    fn too_weak_when_signal_present_but_fspl_saturates() {
        // Received level above the sensitivity threshold but
        // implying a distance past Earth's great-circle max.
        // -50 dB raw + -75 dB cal → -125 dBm received, just above
        // the -130 dBm threshold; 25W at 155 MHz says distance is
        // about 35,000 km — past the 20,000 km cap.
        let s = DistanceDisplay::compute(Some(-50.0), Some(TEST_FREQ_HZ), TEST_ERP_WATTS, -75.0);
        assert_eq!(s, DistanceDisplay::TooWeak);
        assert_eq!(s.format(), "Too weak to measure");
    }

    #[test]
    fn value_for_plausible_signal() {
        // 25W ERP, -80 dBm received at 155 MHz → 44 dBm path loss
        // of 124 dB → ~244 km FSPL. Hand-computed:
        //   d = 10 ^ ((124 - 163.8 + 147.55) / 20)
        //     = 10 ^ (107.75 / 20)  ≈  243 km
        // Bounds generous (100–1000 km) so micro-drift in the
        // 147.55 constant doesn't fail the test.
        let s = DistanceDisplay::compute(Some(-80.0), Some(TEST_FREQ_HZ), TEST_ERP_WATTS, 0.0);
        let DistanceDisplay::Value(d) = s else {
            unreachable!("expected Value, got {s:?}");
        };
        assert!(
            (100_000.0..1_000_000.0).contains(&d),
            "expected 100-1000 km for -80 dBm received from 25W at 155 MHz, got {d} m"
        );
    }

    #[test]
    fn value_for_strong_signal() {
        // Anchors the user-reported "9 km" scenario. For 25W at
        // 155 MHz to produce a ~9 km estimate the received level
        // has to be roughly -51 dBm — a very strong nearby
        // station. The test just checks the same
        // state-machine path returns `Value` and a modest
        // (single-to-tens-of-km) distance.
        let s = DistanceDisplay::compute(Some(-51.0), Some(TEST_FREQ_HZ), TEST_ERP_WATTS, 0.0);
        let DistanceDisplay::Value(d) = s else {
            unreachable!("expected Value, got {s:?}");
        };
        assert!(
            (1_000.0..50_000.0).contains(&d),
            "expected 1-50 km for -51 dBm received from 25W at 155 MHz, got {d} m"
        );
    }

    #[test]
    fn exactly_at_sensitivity_threshold_still_evaluates() {
        // Boundary: exactly at the threshold should NOT trip
        // `NoActiveSignal` — we use `<` not `<=` so legitimate
        // edge-of-sensitivity receptions still get measured.
        // The threshold is -130 dBm; explicit literal rather than
        // casting `NO_ACTIVE_SIGNAL_DBM` because `as f32` would
        // trip clippy's `cast_possible_truncation` even though
        // -130.0 is exactly representable.
        let signal_at_threshold_db: f32 = -130.0;
        let s = DistanceDisplay::compute(
            Some(signal_at_threshold_db),
            Some(TEST_FREQ_HZ),
            TEST_ERP_WATTS,
            0.0,
        );
        assert_ne!(s, DistanceDisplay::NoActiveSignal);
    }

    // ─── format() rendering tests ─────────────────────────────

    #[test]
    fn format_sub_kilometre_shows_metres() {
        assert_eq!(DistanceDisplay::Value(1.4).format(), "1 m");
        assert_eq!(DistanceDisplay::Value(837.5).format(), "838 m");
        assert_eq!(DistanceDisplay::Value(999.0).format(), "999 m");
    }

    #[test]
    fn format_kilometre_range_has_one_decimal() {
        assert_eq!(DistanceDisplay::Value(1_000.0).format(), "1.0 km");
        assert_eq!(DistanceDisplay::Value(12_345.0).format(), "12.3 km");
        assert_eq!(DistanceDisplay::Value(999_000.0).format(), "999.0 km");
    }

    #[test]
    fn format_large_distances_round_to_10_km() {
        assert_eq!(DistanceDisplay::Value(1_234_000.0).format(), "1230 km");
        assert_eq!(DistanceDisplay::Value(1_500_000.0).format(), "1500 km");
    }

    #[test]
    fn format_exactly_at_cap_still_shown_as_number() {
        // Boundary case: the cap value itself is still a real
        // distance (half of Earth's circumference). Only values
        // STRICTLY above it transition to `TooWeak`.
        let formatted = DistanceDisplay::Value(MAX_MEANINGFUL_DISTANCE_M).format();
        assert!(formatted.ends_with(" km"));
    }
}

/// Build the radio / demodulator configuration panel.
///
/// Lays out as an `AdwPreferencesPage` with five titled sections —
/// Bandwidth / Squelch / Filters / De-emphasis / CTCSS — matching
/// the activity-bar redesign's Apple-style section rhythm (design
/// doc §3.2). Section groups are flat rather than `AdwExpanderRow`
/// — same call as the General panel: the expander-row inset +
/// group-title inset stacked to a double-indent that read cluttered
/// once sections were populated, so we pin "expanded by default"
/// into "always visible" and give the user scroll instead of
/// collapse as the focus affordance.
#[allow(clippy::too_many_lines)]
pub fn build_radio_panel() -> RadioPanel {
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

    // --- Sectioned preferences page ---
    //
    // Individual row-level `.visible(false)` flags set at
    // construction above (stereo, CTCSS rows, voice-squelch rows)
    // are preserved — they keep the startup state correct before
    // `apply_demod_visibility` runs, and the group-level hide in
    // `apply_demod_visibility` is a second line of defence for
    // screen-reader users.
    // Section groups — `title` + `description` pattern mirrors the
    // other panels (Audio / Display / Source / etc.) so header
    // spacing + typography stays consistent across activities.
    // Descriptions double as plain-English hints for users new to
    // SDR jargon.
    let bandwidth_group = adw::PreferencesGroup::builder()
        .title("Bandwidth")
        .description("Filter width around the tuned frequency")
        .build();
    bandwidth_group.add(&bandwidth_row);

    let squelch_group = adw::PreferencesGroup::builder()
        .title("Squelch")
        .description("Mute audio when the signal is too weak")
        .build();
    squelch_group.add(&squelch_enabled_row);
    squelch_group.add(&squelch_level_row);
    squelch_group.add(&auto_squelch_row);
    squelch_group.add(&voice_squelch_row);
    squelch_group.add(&voice_squelch_threshold_row);
    squelch_group.add(&voice_squelch_status_row);

    let filters_group = adw::PreferencesGroup::builder()
        .title("Filters")
        .description("Clean up interference and noise")
        .build();
    filters_group.add(&noise_blanker_row);
    filters_group.add(&nb_level_row);
    filters_group.add(&fm_if_nr_row);
    filters_group.add(&stereo_row);
    filters_group.add(&notch_enabled_row);
    filters_group.add(&notch_freq_row);

    let deemphasis_group = adw::PreferencesGroup::builder()
        .title("De-emphasis")
        .description("Restore high-frequency audio on FM")
        .build();
    deemphasis_group.add(&deemphasis_row);

    let ctcss_group = adw::PreferencesGroup::builder()
        .title("CTCSS")
        .description("Open audio only when a matching tone is present")
        .build();
    ctcss_group.add(&ctcss_row);
    ctcss_group.add(&ctcss_threshold_row);
    ctcss_group.add(&ctcss_status_row);

    // --- Distance Estimator (FSPL, ticket #164) ---
    let erp_adj = gtk4::Adjustment::new(
        DEFAULT_ERP_WATTS,
        MIN_ERP_WATTS,
        MAX_ERP_WATTS,
        ERP_STEP_WATTS,
        ERP_PAGE_WATTS,
        0.0,
    );
    let erp_row = adw::SpinRow::builder()
        .title("Transmitter Power")
        .subtitle("Effective radiated power, in watts (handheld ~5, mobile ~25-50)")
        .adjustment(&erp_adj)
        .digits(3)
        .build();

    let cal_adj = gtk4::Adjustment::new(
        DEFAULT_CALIBRATION_DB,
        MIN_CALIBRATION_DB,
        MAX_CALIBRATION_DB,
        CALIBRATION_STEP_DB,
        CALIBRATION_PAGE_DB,
        0.0,
    );
    let calibration_row = adw::SpinRow::builder()
        .title("Receiver Calibration")
        .subtitle("dB offset applied to raw level before computing path loss")
        .adjustment(&cal_adj)
        .digits(1)
        .build();

    let distance_row = adw::ActionRow::builder()
        .title("Estimated Distance")
        .subtitle("—")
        .selectable(false)
        .activatable(false)
        .build();

    let distance_group = adw::PreferencesGroup::builder()
        .title("Distance Estimator")
        .description(
            "Rough line-of-sight (FSPL) estimate — read as an upper bound, not precision ranging",
        )
        .build();
    distance_group.add(&erp_row);
    distance_group.add(&calibration_row);
    distance_group.add(&distance_row);

    // Internal state shared across the panel clone surface — see
    // the field docs on `RadioPanel` for why this is `Rc<Cell>`
    // rather than plain `Cell`.
    let distance_last_signal_db: Rc<Cell<Option<f32>>> = Rc::new(Cell::new(None));
    let distance_last_frequency_hz: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));

    // Wire ERP and calibration spin-row changes to trigger a
    // distance refresh using the cached signal/frequency. Config
    // persistence and any DSP plumbing that cares about these
    // values is wired separately in `window.rs` on the same
    // signal — both handlers run on value change.
    {
        let last_signal = Rc::clone(&distance_last_signal_db);
        let last_freq = Rc::clone(&distance_last_frequency_hz);
        let erp_row_for_signal = erp_row.clone();
        let cal_row_for_signal = calibration_row.clone();
        let distance_row_for_signal = distance_row.clone();
        let refresh = move || {
            refresh_distance_display_standalone(
                &erp_row_for_signal,
                &cal_row_for_signal,
                &distance_row_for_signal,
                last_signal.get(),
                last_freq.get(),
            );
        };
        let refresh_for_erp = refresh.clone();
        erp_row.connect_value_notify(move |_| refresh_for_erp());
        calibration_row.connect_value_notify(move |_| refresh());
    }

    let page = adw::PreferencesPage::new();
    page.add(&bandwidth_group);
    page.add(&squelch_group);
    page.add(&filters_group);
    page.add(&deemphasis_group);
    page.add(&ctcss_group);
    page.add(&distance_group);

    // All rows connected to DSP pipeline via window.rs

    RadioPanel {
        widget: page,
        deemphasis_group,
        ctcss_group,
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
        erp_row,
        calibration_row,
        distance_row,
        distance_last_signal_db,
        distance_last_frequency_hz,
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
