//! Demod-mode visibility policy plus the CTCSS and voice-squelch
//! selector logic for the Radio panel — index/mode converters,
//! status-row renderers, and the per-mode threshold-row
//! reconfiguration. Split out of `radio_panel.rs` per the
//! file-size pass (issue #819).

use libadwaita::prelude::*;
use sdr_dsp::tone_detect::CTCSS_TONES_HZ;
use sdr_dsp::voice_squelch::{
    VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB, VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
    VoiceSquelchMode,
};
use sdr_radio::af_chain::CtcssMode;

use super::{
    RadioPanel, SNR_THRESHOLD_DB_MAX, SNR_THRESHOLD_DB_MIN, SNR_THRESHOLD_DB_PAGE,
    SNR_THRESHOLD_DB_STEP, SYLLABIC_THRESHOLD_MAX, SYLLABIC_THRESHOLD_MIN, SYLLABIC_THRESHOLD_PAGE,
    SYLLABIC_THRESHOLD_STEP, VOICE_SQUELCH_OFF_IDX, VOICE_SQUELCH_SNR_IDX,
    VOICE_SQUELCH_SYLLABIC_IDX,
};

/// Pure visibility policy for a demod mode — which mode-specific
/// control clusters the Radio panel shows. Extracted from
/// [`RadioPanel::apply_demod_visibility`] so the policy (including
/// the CTCSS hide-implies-force-clear rule that fixed a real
/// "no audio with no visible control" bug) is unit-testable without
/// a GTK harness; the widget application below stays a thin
/// consumer. Per the Codacy AI review on PR #887.
pub(super) struct DemodVisibility {
    /// De-emphasis group/row + FM IF NR: any FM mode.
    pub(super) fm_controls: bool,
    /// Stereo decode: WFM broadcast only.
    pub(super) stereo: bool,
    /// CTCSS cluster: NFM only. When `false` the combo is also
    /// force-cleared to "Off" by the applier — hiding an armed
    /// detector would gate the speaker with no visible control.
    pub(super) ctcss: bool,
}

impl DemodVisibility {
    pub(super) fn for_mode(mode: sdr_types::DemodMode) -> Self {
        let is_fm = matches!(mode, sdr_types::DemodMode::Wfm | sdr_types::DemodMode::Nfm);
        Self {
            fm_controls: is_fm,
            stereo: mode == sdr_types::DemodMode::Wfm,
            ctcss: mode == sdr_types::DemodMode::Nfm,
        }
    }
}

impl RadioPanel {
    /// Update mode-specific control visibility for the given demod mode.
    ///
    /// Centralizes FM/WFM visibility policy so startup and mode-switch
    /// handlers stay in sync.
    pub fn apply_demod_visibility(&self, mode: sdr_types::DemodMode) {
        let policy = DemodVisibility::for_mode(mode);
        let is_fm = policy.fm_controls;
        // De-emphasis group: hide the whole section on AM / SSB /
        // CW. The per-row `deemphasis_row.set_visible(...)` is
        // retained as a belt for screen readers (the row stays
        // hidden even if a future refactor moves the group
        // around).
        self.deemphasis_group.set_visible(is_fm);
        self.deemphasis_row.set_visible(is_fm);
        self.fm_if_nr_row.set_visible(is_fm);
        self.stereo_row.set_visible(policy.stereo);
        // CTCSS is an NFM-only feature — WFM / AM / SSB / CW
        // either don't carry sub-audible tones or don't use them
        // as a squelch keying mechanism in practice. Hide the
        // whole group; individual `set_visible` kept for the
        // same defensive reason as de-emphasis.
        let ctcss_allowed = policy.ctcss;
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
            self.ctcss_row.set_selected(super::CTCSS_OFF_IDX);
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
        let text = if self.ctcss_row.selected() == super::CTCSS_OFF_IDX {
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
