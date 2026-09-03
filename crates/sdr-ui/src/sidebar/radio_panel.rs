//! Radio / demodulator configuration panel — bandwidth, squelch, de-emphasis.

use std::cell::Cell;
use std::rc::Rc;

use libadwaita as adw;

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

/// CTCSS combo index for "Off" — indices `1..=51` map to
/// [`sdr_dsp::tone_detect::CTCSS_TONES_HZ`] entries (see
/// `modes.rs::ctcss_mode_from_index`). Named alongside the
/// voice-squelch indices per `CodeRabbit` round 1 on PR #887.
pub(crate) const CTCSS_OFF_IDX: u32 = 0;
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

/// Combo indices of the De-emphasis `StringList` model
/// (`["None", "50 µs (EU)", "75 µs (US)"]`). Keep in lock-step with
/// the model built in `build_radio_panel`.
pub(crate) const DEEMPHASIS_NONE_IDX: u32 = 0;
/// 50 µs (EU) entry of the De-emphasis combo model.
pub(crate) const DEEMPHASIS_EU50_IDX: u32 = 1;
/// 75 µs (US) entry of the De-emphasis combo model.
pub(crate) const DEEMPHASIS_US75_IDX: u32 = 2;
/// Number of entries in the De-emphasis combo model.
pub(crate) const DEEMPHASIS_MODEL_LEN: u32 = 3;

/// Radio / demodulator configuration panel with references to interactive rows.
#[derive(Clone)]
pub struct RadioPanel {
    /// The `AdwPreferencesPage` widget packed into the Radio
    /// activity stack slot. The page hosts six titled
    /// `AdwPreferencesGroup`s (Bandwidth / Squelch / Filters /
    /// De-emphasis / CTCSS / Distance Estimator) — see
    /// [`build_radio_panel`].
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

mod build;
mod distance;
mod modes;

pub use build::build_radio_panel;

#[cfg(test)]
mod tests;
