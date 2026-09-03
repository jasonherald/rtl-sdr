//! Bookmark model + persistence for the Navigation panel — the
//! [`Bookmark`] / [`TuningProfile`] structs, demod-mode string
//! conversion, the JSON load/save pair, and the shared frequency
//! formatter. Split out of `navigation_panel.rs` per the
//! file-size pass (issue #819).

use gtk4::glib;
use sdr_types::DemodMode;

// ---------------------------------------------------------------------------
// Bookmarks — user-saved frequencies with JSON persistence
// ---------------------------------------------------------------------------

/// Snapshot of tuning-profile settings captured from the UI.
///
/// Passed to [`Bookmark::with_profile`] to populate the optional fields.
/// Using a struct avoids long parameter lists and the clippy
/// `fn_params_excessive_bools` lint.
#[allow(clippy::struct_excessive_bools)]
pub struct TuningProfile {
    pub squelch_enabled: bool,
    pub auto_squelch_enabled: bool,
    pub squelch_level: f32,
    pub gain: f64,
    /// Three-way AGC selection — replaces the pre-#354 `agc: bool`.
    /// The save path populates both the new `agc_type` and the
    /// legacy `agc: Option<bool>` on the persisted `Bookmark` so
    /// older builds loading the file still get a sensible
    /// (if reduced) restore.
    pub agc_type: crate::sidebar::source_panel::AgcType,
    pub volume: Option<f32>,
    pub deemphasis: u32,
    pub nb_enabled: bool,
    pub nb_level: f32,
    pub fm_if_nr: bool,
    pub wfm_stereo: bool,
    pub high_pass: Option<bool>,
    /// CTCSS sub-audible tone squelch mode. `None` means "don't
    /// touch the current setting on restore" — bookmarks saved
    /// before PR 3 are all `None` after deserialization so they
    /// preserve the user's current CTCSS setting when loaded.
    pub ctcss_mode: Option<sdr_radio::af_chain::CtcssMode>,
    /// CTCSS detection threshold (normalized magnitude, `(0, 1]`).
    /// Same backward-compat semantics as `ctcss_mode`.
    pub ctcss_threshold: Option<f32>,
    /// Voice-activity squelch mode — tagged enum carrying its
    /// threshold inline. Same backward-compat contract as CTCSS:
    /// `None` on restore means leave the current voice squelch
    /// setting alone.
    pub voice_squelch_mode: Option<sdr_dsp::voice_squelch::VoiceSquelchMode>,
}

/// A user-saved frequency bookmark with optional tuning profile fields.
///
/// The optional fields use `#[serde(default)]` so existing `bookmarks.json`
/// files (which lack these keys) deserialize without error — the missing
/// fields simply become `None`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Bookmark {
    pub name: String,
    pub frequency: u64,
    pub demod_mode: String,
    pub bandwidth: f64,
    // --- Full tuning profile (all optional for backward compat) ---
    #[serde(default)]
    pub squelch_enabled: Option<bool>,
    #[serde(default)]
    pub auto_squelch_enabled: Option<bool>,
    #[serde(default)]
    pub squelch_level: Option<f32>,
    #[serde(default)]
    pub gain: Option<f64>,
    /// Legacy hardware-AGC flag — `Some(true)` meant tuner AGC
    /// on, `Some(false)` meant manual gain. Preserved for read-
    /// path compatibility with bookmarks saved before #354
    /// landed; superseded by `agc_type` for new bookmarks. Save
    /// path writes both fields when AGC is `Off` or `Hardware`
    /// so older builds loading a new bookmark still get a
    /// sensible (if reduced) restore.
    #[serde(default)]
    pub agc: Option<bool>,
    /// Three-way AGC selection (Off / Hardware / Software).
    /// Added with #354 / #356. Pre-existing bookmarks deserialize
    /// to `None`; the restore path falls back to the legacy
    /// `agc: Option<bool>` field mapping `true → Hardware` and
    /// `false → Off`. When both fields are present the new
    /// `agc_type` wins.
    #[serde(default)]
    pub agc_type: Option<crate::sidebar::source_panel::AgcType>,
    #[serde(default)]
    pub volume: Option<f32>,
    #[serde(default)]
    pub deemphasis: Option<u32>,
    #[serde(default)]
    pub nb_enabled: Option<bool>,
    #[serde(default)]
    pub nb_level: Option<f32>,
    #[serde(default)]
    pub fm_if_nr: Option<bool>,
    #[serde(default)]
    pub wfm_stereo: Option<bool>,
    #[serde(default)]
    pub high_pass: Option<bool>,
    /// `RadioReference` category (e.g., "Law Dispatch"). Metadata for future
    /// bookmark tree organization.
    #[serde(default)]
    pub rr_category: Option<String>,
    /// `RadioReference` frequency ID for duplicate detection and future sync.
    #[serde(default)]
    pub rr_import_id: Option<String>,
    /// CTCSS sub-audible tone squelch mode. Added in PR 3 of #269.
    /// Serialized as the tagged form `{"kind":"off"}` or
    /// `{"kind":"tone","hz":100.0}`. Pre-PR-3 bookmarks lack this
    /// key and deserialize to `None`, which the restore path
    /// interprets as "leave the current CTCSS setting alone".
    #[serde(default)]
    pub ctcss_mode: Option<sdr_radio::af_chain::CtcssMode>,
    /// CTCSS detection threshold in `(0, 1]`. Same backward-compat
    /// semantics as `ctcss_mode`.
    #[serde(default)]
    pub ctcss_threshold: Option<f32>,
    /// Voice-activity squelch mode — tagged `VoiceSquelchMode`
    /// enum carrying its threshold inline. Added in the voice-
    /// squelch PR; pre-PR bookmarks deserialize to `None` which
    /// the restore path interprets as "leave the current voice
    /// squelch setting alone."
    #[serde(default)]
    pub voice_squelch_mode: Option<sdr_dsp::voice_squelch::VoiceSquelchMode>,
    /// Include in scanner rotation. Default false so existing
    /// bookmarks don't start getting scanned without opt-in.
    #[serde(default)]
    pub scan_enabled: bool,
    /// Priority tier. 0 = normal, 1 = priority (checked more
    /// often). Higher tiers reserved for future phases.
    #[serde(default)]
    pub priority: u8,
    /// Per-channel dwell override in ms. `None` → resolved to the
    /// UI-side default at `ScannerChannel` projection time (scanner
    /// itself doesn't own a default; timing defaults live in the
    /// UI layer per the design).
    #[serde(default)]
    pub dwell_ms_override: Option<u32>,
    /// Per-channel hang override in ms. `None` → resolved to the
    /// UI-side default at `ScannerChannel` projection time (same
    /// ownership contract as `dwell_ms_override`).
    #[serde(default)]
    pub hang_ms_override: Option<u32>,
}

impl Bookmark {
    /// Create a bookmark with only the core tuning state (backward compat).
    pub fn new(name: &str, frequency: u64, demod_mode: DemodMode, bandwidth: f64) -> Self {
        Self {
            name: name.to_string(),
            frequency,
            demod_mode: demod_mode_to_string(demod_mode),
            bandwidth,
            squelch_enabled: None,
            auto_squelch_enabled: None,
            squelch_level: None,
            gain: None,
            agc: None,
            agc_type: None,
            volume: None,
            deemphasis: None,
            nb_enabled: None,
            nb_level: None,
            fm_if_nr: None,
            wfm_stereo: None,
            high_pass: None,
            rr_category: None,
            rr_import_id: None,
            ctcss_mode: None,
            ctcss_threshold: None,
            voice_squelch_mode: None,
            scan_enabled: false,
            priority: 0,
            dwell_ms_override: None,
            hang_ms_override: None,
        }
    }

    /// Create a bookmark capturing the full tuning profile.
    pub fn with_profile(
        name: &str,
        frequency: u64,
        demod_mode: DemodMode,
        bandwidth: f64,
        profile: &TuningProfile,
    ) -> Self {
        Self {
            name: name.to_string(),
            frequency,
            demod_mode: demod_mode_to_string(demod_mode),
            bandwidth,
            squelch_enabled: Some(profile.squelch_enabled),
            auto_squelch_enabled: Some(profile.auto_squelch_enabled),
            squelch_level: Some(profile.squelch_level),
            gain: Some(profile.gain),
            // Populate both the new `agc_type` AND the legacy
            // `agc` field so a bookmark saved on a post-#354
            // build still round-trips through a pre-#354 build
            // as a sensible (if reduced) setting. Software AGC
            // has no legacy representation — map it to `false`
            // (AGC off) on the legacy path, which is the safer
            // default than "hardware on" for users who haven't
            // opted into either AGC type.
            agc: Some(matches!(
                profile.agc_type,
                crate::sidebar::source_panel::AgcType::Hardware
            )),
            agc_type: Some(profile.agc_type),
            volume: profile.volume,
            deemphasis: Some(profile.deemphasis),
            nb_enabled: Some(profile.nb_enabled),
            nb_level: Some(profile.nb_level),
            fm_if_nr: Some(profile.fm_if_nr),
            wfm_stereo: Some(profile.wfm_stereo),
            high_pass: profile.high_pass,
            rr_category: None,
            rr_import_id: None,
            ctcss_mode: profile.ctcss_mode,
            ctcss_threshold: profile.ctcss_threshold,
            voice_squelch_mode: profile.voice_squelch_mode,
            scan_enabled: false,
            priority: 0,
            dwell_ms_override: None,
            hang_ms_override: None,
        }
    }

    /// Build a compact subtitle: "NFM 495.300 MHz"
    pub fn settings_subtitle(&self) -> String {
        format!("{} {}", self.demod_mode, format_frequency(self.frequency))
    }
}

pub fn demod_mode_to_string(mode: DemodMode) -> String {
    match mode {
        DemodMode::Wfm => "WFM",
        DemodMode::Nfm => "NFM",
        DemodMode::Am => "AM",
        DemodMode::Usb => "USB",
        DemodMode::Lsb => "LSB",
        DemodMode::Dsb => "DSB",
        DemodMode::Cw => "CW",
        DemodMode::Raw => "RAW",
        DemodMode::Lrpt => "LRPT",
    }
    .to_string()
}

/// Parse a demod mode string back to a `DemodMode` enum value.
///
/// Unrecognized strings default to `Nfm`.
pub fn parse_demod_mode(s: &str) -> DemodMode {
    string_to_demod_mode(s)
}

pub(super) fn string_to_demod_mode(s: &str) -> DemodMode {
    match s {
        "WFM" => DemodMode::Wfm,
        "AM" => DemodMode::Am,
        "USB" => DemodMode::Usb,
        "LSB" => DemodMode::Lsb,
        "DSB" => DemodMode::Dsb,
        "CW" => DemodMode::Cw,
        "RAW" => DemodMode::Raw,
        // Per CodeRabbit round 1 on PR #543: this arm was
        // missing, so a Meteor bookmark saved as "LRPT" parsed
        // back as `Nfm` on next load — silently demoting the
        // user's catalog-driven LRPT tune. Mirrors the
        // serializer's `DemodMode::Lrpt => "LRPT"`.
        "LRPT" => DemodMode::Lrpt,
        // "NFM" and any unrecognized string default to NFM.
        _ => DemodMode::Nfm,
    }
}

/// Default bookmark file location.
fn bookmarks_path() -> std::path::PathBuf {
    let mut path = glib::user_config_dir();
    path.push("sdr-rs");
    path.push("bookmarks.json");
    path
}

pub fn load_bookmarks() -> Vec<Bookmark> {
    let path = bookmarks_path();
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str(&data) {
        Ok(bookmarks) => bookmarks,
        Err(e) => {
            tracing::warn!(?path, "failed to parse bookmarks, starting fresh: {e}");
            Vec::new()
        }
    }
}

pub fn save_bookmarks(bookmarks: &[Bookmark]) {
    let path = bookmarks_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(bookmarks) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                tracing::warn!("failed to save bookmarks: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to serialize bookmarks: {e}"),
    }
}

/// Format a frequency as a human-readable string (e.g., "98.100 MHz").
pub fn format_frequency(freq: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let freq_f64 = freq as f64;
    if freq >= 1_000_000_000 {
        format!("{:.3} GHz", freq_f64 / 1_000_000_000.0)
    } else if freq >= 1_000_000 {
        format!("{:.3} MHz", freq_f64 / 1_000_000.0)
    } else if freq >= 1_000 {
        format!("{:.1} kHz", freq_f64 / 1_000.0)
    } else {
        format!("{freq} Hz")
    }
}
