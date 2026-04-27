//! Transcript sidebar panel — displays live transcription results.

use std::sync::Arc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;

#[cfg(feature = "whisper")]
/// Config key for the persisted Whisper model index.
const KEY_MODEL: &str = "transcription_model";
#[cfg(feature = "whisper")]
/// Config key for the silence threshold after spectral denoising.
/// Whisper-only — Sherpa uses native endpoint detection.
const KEY_SILENCE_THRESHOLD: &str = "transcription_silence_threshold";
/// Config key for the spectral noise gate ratio.
const KEY_NOISE_GATE: &str = "transcription_noise_gate";
#[cfg(feature = "sherpa")]
/// Config key for the persisted Sherpa model index.
///
/// `pub(crate)` so `window.rs` can write to it from the reload
/// handler's `InitEvent::Ready` branch — sherpa persistence is
/// deferred until the recognizer swap succeeds, so a failed reload
/// can't leave a broken model index in config and wedge next
/// startup's `init_sherpa_host`.
pub(crate) const KEY_SHERPA_MODEL: &str = "transcription_sherpa_model";
#[cfg(feature = "sherpa")]
/// Config key for the persisted transcript display mode.
/// Values: `"live"` (default) or `"final"`.
const KEY_DISPLAY_MODE: &str = "transcription_display_mode";
#[cfg(feature = "sherpa")]
/// Config key for the persisted Silero VAD threshold.
/// Sherpa-only — only meaningful for offline models (Moonshine, Parakeet).
const KEY_SHERPA_VAD_THRESHOLD: &str = "sherpa_vad_threshold";
#[cfg(feature = "sherpa")]
/// Config key for persisting the Auto Break segmentation preference.
/// When true, offline sherpa sessions use squelch edges as utterance
/// boundaries instead of Silero VAD. Default false (preserve existing
/// behavior for existing config files).
pub(crate) const KEY_AUTO_BREAK_ENABLED: &str = "transcription_auto_break_enabled";
/// Config key for the persisted Auto Break "minimum open duration"
/// threshold. Squelch opens shorter than this are discarded as noise
/// spikes. Persisted in config as `u64` milliseconds (the on-disk
/// JSON representation widens `BackendConfig`'s u32 field through the
/// `SpinRow`'s f64 adjustment).
#[cfg(feature = "sherpa")]
const KEY_AUTO_BREAK_MIN_OPEN_MS: &str = "transcription_auto_break_min_open_ms";
/// Config key for the persisted Auto Break tail-capture window.
/// Continue buffering audio for this long after squelch closes so
/// the last syllable isn't chopped. Persisted in config as `u64`
/// milliseconds.
#[cfg(feature = "sherpa")]
const KEY_AUTO_BREAK_TAIL_MS: &str = "transcription_auto_break_tail_ms";
/// Config key for the persisted Auto Break minimum segment length.
/// Segments shorter than this are discarded instead of decoded (sub-
/// word fragments make offline sherpa models hallucinate). Persisted
/// in config as `u64` milliseconds.
#[cfg(feature = "sherpa")]
const KEY_AUTO_BREAK_MIN_SEGMENT_MS: &str = "transcription_auto_break_min_segment_ms";

/// Config key for the persisted audio-enhancement mode. Values are
/// the `AudioEnhancement::as_config_str` strings: `"voice_band"`,
/// `"broadband"`, or `"off"`. Default `"voice_band"`. Applies to
/// both whisper and sherpa — the audio enhancement dispatcher lives
/// in `sdr-transcription::denoise` and all four recognizer call
/// sites route through it. Added in PR for issue #281 so users who
/// hit voice-band preprocessor issues (notably Moonshine) have a
/// user-visible workaround without rebuilding.
const KEY_AUDIO_ENHANCEMENT: &str = "transcription_audio_enhancement";

/// Combo row indices for the audio-enhancement selector. Order
/// matches [`AUDIO_ENHANCEMENT_LABELS`] below. `pub(crate)` so
/// `window.rs` can match on them at `BackendConfig` construction
/// time without re-deriving the parse logic.
pub(crate) const AUDIO_ENHANCEMENT_VOICE_BAND_IDX: u32 = 0;
pub(crate) const AUDIO_ENHANCEMENT_BROADBAND_IDX: u32 = 1;
pub(crate) const AUDIO_ENHANCEMENT_OFF_IDX: u32 = 2;
/// User-visible labels for the audio-enhancement combo row. Order
/// must match the `AUDIO_ENHANCEMENT_*_IDX` constants above.
const AUDIO_ENHANCEMENT_LABELS: &[&str] = &["Voice-band (default)", "Broadband", "Off"];

#[cfg(feature = "sherpa")]
const DISPLAY_MODE_LIVE_IDX: u32 = 0;
/// `pub(crate)` so `window.rs` can gate the `Partial` handler on it.
#[cfg(feature = "sherpa")]
pub(crate) const DISPLAY_MODE_FINAL_IDX: u32 = 1;
#[cfg(feature = "sherpa")]
const DISPLAY_MODE_LABELS: &[&str] = &["Live captions", "Final only"];

// Silence threshold slider defaults and range. Whisper-only — Sherpa
// uses native endpoint detection so the slider isn't shown.
#[cfg(feature = "whisper")]
const DEFAULT_SILENCE_THRESHOLD: f64 = 0.007;
#[cfg(feature = "whisper")]
const SILENCE_THRESHOLD_MIN: f64 = 0.001;
#[cfg(feature = "whisper")]
const SILENCE_THRESHOLD_MAX: f64 = 0.100;
#[cfg(feature = "whisper")]
const SILENCE_THRESHOLD_STEP: f64 = 0.001;
#[cfg(feature = "whisper")]
const SILENCE_THRESHOLD_PAGE: f64 = 0.01;

// Noise gate slider defaults and range.
const DEFAULT_NOISE_GATE: f64 = 3.0;

// VAD threshold slider defaults and range. Sherpa-only — only matters
// for offline models (Moonshine, Parakeet) which use Silero VAD to
// detect utterance boundaries. Default 0.5 matches sherpa-onnx's
// upstream Silero default. Lower for noisy NFM/scanner audio; higher
// for clean broadcast.
// UI slider values are f64 (adw::SpinRow takes f64). The canonical
// f32 constants live in `sdr_transcription::backend` — these are
// widened casts so the slider can't drift from the backend defaults.
#[cfg(feature = "sherpa")]
const DEFAULT_SHERPA_VAD_THRESHOLD: f64 = sdr_transcription::VAD_THRESHOLD_DEFAULT as f64;
#[cfg(feature = "sherpa")]
const SHERPA_VAD_THRESHOLD_MIN: f64 = sdr_transcription::VAD_THRESHOLD_MIN as f64;
#[cfg(feature = "sherpa")]
const SHERPA_VAD_THRESHOLD_MAX: f64 = sdr_transcription::VAD_THRESHOLD_MAX as f64;
#[cfg(feature = "sherpa")]
const SHERPA_VAD_THRESHOLD_STEP: f64 = 0.05;
#[cfg(feature = "sherpa")]
const SHERPA_VAD_THRESHOLD_PAGE: f64 = 0.10;

// Auto Break timing parameters (sherpa-only, offline-only, NFM-only).
// Defaults and bounds come from `sdr_transcription::backend` as u32
// constants; the UI widens them to f64 because `adw::SpinRow` takes
// f64 adjustments. All three sliders step in 10 ms increments
// (`AUTO_BREAK_MS_STEP` below) because finer precision has no
// perceptible effect on segmentation behavior.
#[cfg(feature = "sherpa")]
const AUTO_BREAK_MIN_OPEN_MS_MIN: f64 = sdr_transcription::AUTO_BREAK_MIN_OPEN_MS_MIN as f64;
#[cfg(feature = "sherpa")]
const AUTO_BREAK_MIN_OPEN_MS_MAX: f64 = sdr_transcription::AUTO_BREAK_MIN_OPEN_MS_MAX as f64;
#[cfg(feature = "sherpa")]
const AUTO_BREAK_MIN_OPEN_MS_DEFAULT: f64 =
    sdr_transcription::AUTO_BREAK_MIN_OPEN_MS_DEFAULT as f64;
#[cfg(feature = "sherpa")]
const AUTO_BREAK_TAIL_MS_MIN: f64 = sdr_transcription::AUTO_BREAK_TAIL_MS_MIN as f64;
#[cfg(feature = "sherpa")]
const AUTO_BREAK_TAIL_MS_MAX: f64 = sdr_transcription::AUTO_BREAK_TAIL_MS_MAX as f64;
#[cfg(feature = "sherpa")]
const AUTO_BREAK_TAIL_MS_DEFAULT: f64 = sdr_transcription::AUTO_BREAK_TAIL_MS_DEFAULT as f64;
#[cfg(feature = "sherpa")]
const AUTO_BREAK_MIN_SEGMENT_MS_MIN: f64 = sdr_transcription::AUTO_BREAK_MIN_SEGMENT_MS_MIN as f64;
#[cfg(feature = "sherpa")]
const AUTO_BREAK_MIN_SEGMENT_MS_MAX: f64 = sdr_transcription::AUTO_BREAK_MIN_SEGMENT_MS_MAX as f64;
#[cfg(feature = "sherpa")]
const AUTO_BREAK_MIN_SEGMENT_MS_DEFAULT: f64 =
    sdr_transcription::AUTO_BREAK_MIN_SEGMENT_MS_DEFAULT as f64;
/// All three Auto Break sliders step in 10 ms increments. Sub-10 ms
/// tuning has no perceptible effect on segmentation behavior.
#[cfg(feature = "sherpa")]
const AUTO_BREAK_MS_STEP: f64 = 10.0;
#[cfg(feature = "sherpa")]
const AUTO_BREAK_MS_PAGE: f64 = 50.0;

const NOISE_GATE_MIN: f64 = 1.0;
const NOISE_GATE_MAX: f64 = 10.0;
const NOISE_GATE_STEP: f64 = 0.5;
const NOISE_GATE_PAGE: f64 = 1.0;

/// Transcript panel with toggle switch, model picker, tuning sliders,
/// status label, progress bar, scrolling transcript log, and clear button.
pub struct TranscriptPanel {
    /// The `AdwPreferencesGroup` widget to pack into the sidebar.
    pub widget: adw::PreferencesGroup,
    /// Toggle to enable/disable live transcription.
    pub enable_row: adw::SwitchRow,
    /// Model size selector — shows Whisper or Sherpa models based on
    /// which cargo feature was compiled in.
    pub model_row: adw::ComboRow,
    /// Silence threshold spin row. Whisper-only — Sherpa hides this
    /// because it uses native endpoint detection.
    #[cfg(feature = "whisper")]
    pub silence_row: adw::SpinRow,
    /// Noise gate spin row.
    pub noise_gate_row: adw::SpinRow,
    /// Audio enhancement mode selector (Voice-band / Broadband /
    /// Off). Applies to both whisper and sherpa backends — the
    /// dispatcher in `sdr-transcription::denoise` routes every
    /// recognizer call site through the user's choice. Default is
    /// Voice-band, which matches the pre-#281 behavior.
    pub audio_enhancement_row: adw::ComboRow,
    /// Display-mode selector (Live captions vs Final only). Sherpa-only —
    /// Whisper has no `Partial` events to render.
    #[cfg(feature = "sherpa")]
    pub display_mode_row: adw::ComboRow,
    /// VAD threshold spin row. Sherpa-only — only visible when an
    /// offline model (Moonshine, Parakeet) is selected. Online
    /// models (Zipformer) don't use Silero VAD.
    #[cfg(feature = "sherpa")]
    pub vad_threshold_row: adw::SpinRow,
    /// Auto Break toggle. Sherpa-only — when enabled, uses squelch
    /// edges as utterance boundaries instead of Silero VAD. NFM only.
    #[cfg(feature = "sherpa")]
    pub auto_break_row: adw::SwitchRow,
    /// Auto Break minimum-open-duration slider. Sherpa-only — only
    /// visible alongside the Auto Break toggle when Auto Break is on.
    #[cfg(feature = "sherpa")]
    pub auto_break_min_open_row: adw::SpinRow,
    /// Auto Break tail-capture slider. Sherpa-only — only visible
    /// alongside the Auto Break toggle when Auto Break is on.
    #[cfg(feature = "sherpa")]
    pub auto_break_tail_row: adw::SpinRow,
    /// Auto Break minimum-segment-length slider. Sherpa-only — only
    /// visible alongside the Auto Break toggle when Auto Break is on.
    #[cfg(feature = "sherpa")]
    pub auto_break_min_segment_row: adw::SpinRow,
    /// Dimmed italic label below the text view that renders in-progress
    /// Sherpa partials. Sherpa-only.
    #[cfg(feature = "sherpa")]
    pub live_line_label: gtk4::Label,
    /// Status label (downloading, listening, error).
    pub status_label: gtk4::Label,
    /// Model download progress bar.
    pub progress_bar: gtk4::ProgressBar,
    /// Scrolling transcript text display.
    pub text_view: gtk4::TextView,
    /// Scroll container for the text view.
    pub scroll: gtk4::ScrolledWindow,
    /// Button to clear the transcript log.
    pub clear_button: gtk4::Button,
}

/// Append a channel-marker divider line to a transcript
/// `gtk4::TextView`. Called from the wiring layer's
/// `DspToUi::ScannerActiveChannelChanged` handler whenever the
/// scanner switches to a non-idle channel — gives the reader a
/// navigation anchor so transcribed text from different channels
/// doesn't bleed together visually. Per issue #517.
///
/// Format: `─── HH:MM:SS · {channel_name} ───`. Styled dim +
/// italic via a lazily-installed `channel_marker` `TextTag` on
/// the buffer's tag table so it stands apart from regular
/// transcript rows.
///
/// Freestanding rather than a method on `TranscriptPanel` so the
/// call site in `window.rs::handle_dsp_message` only has to
/// thread a `&gtk4::TextView` clone (cheap — GTK widgets are
/// Rc-internal) instead of the whole panel.
pub fn push_channel_marker(text_view: &gtk4::TextView, channel_name: &str) {
    let buf = text_view.buffer();
    let tag_table = buf.tag_table();
    let tag = tag_table.lookup("channel_marker").unwrap_or_else(|| {
        let new_tag = gtk4::TextTag::builder()
            .name("channel_marker")
            .style(gtk4::pango::Style::Italic)
            .foreground("#888888")
            .build();
        tag_table.add(&new_tag);
        new_tag
    });

    let timestamp = chrono::Local::now().format("%H:%M:%S");
    let marker_text = format!("─── {timestamp} · {channel_name} ───\n");

    let start_offset = buf.end_iter().offset();
    let mut end_iter = buf.end_iter();
    buf.insert(&mut end_iter, &marker_text);
    let start_iter = buf.iter_at_offset(start_offset);
    let end_iter = buf.end_iter();
    buf.apply_tag(&tag, &start_iter, &end_iter);

    // Auto-scroll to the bottom so the new marker is visible
    // even when the user hasn't manually scrolled along. Same
    // idiom as the regular transcript-text insert.
    let mark = buf.create_mark(None, &buf.end_iter(), false);
    text_view.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
    buf.delete_mark(&mark);
}

/// Specification for a persisted-millisecond `AdwSpinRow`. Used by
/// [`build_persisted_ms_slider`] to construct the three Auto Break
/// timing sliders from one code path.
#[cfg(feature = "sherpa")]
struct MsSliderSpec {
    /// Config-file JSON key where the value is persisted (`u64` ms).
    key: &'static str,
    /// User-visible row title (e.g. "Auto Break: min open (ms)").
    title: &'static str,
    /// User-visible row subtitle explaining what the knob does.
    subtitle: &'static str,
    /// Inclusive minimum allowed slider value.
    min: f64,
    /// Inclusive maximum allowed slider value.
    max: f64,
    /// Default value shown when the config key is missing or invalid.
    default: f64,
}

/// Build a sherpa-only persisted-milliseconds `AdwSpinRow` from a
/// [`MsSliderSpec`]. Shared shape for the three Auto Break timing
/// sliders (`min_open`, `tail`, `min_segment`) which all follow the
/// same load/clamp/build/persist pattern.
///
/// The `u64 ↔ f64` casts are bounded by `spec.min`/`spec.max` (both
/// well under 2^52 for any realistic slider range) so the conversions
/// are lossless in practice. Allows are scoped tight to this helper.
#[cfg(feature = "sherpa")]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn build_persisted_ms_slider(
    group: &adw::PreferencesGroup,
    config: &Arc<ConfigManager>,
    spec: &MsSliderSpec,
) -> adw::SpinRow {
    let saved = config.read(|v| {
        v.get(spec.key)
            .and_then(serde_json::Value::as_u64)
            .map_or(spec.default, |val| (val as f64).clamp(spec.min, spec.max))
    });

    let row = adw::SpinRow::builder()
        .title(spec.title)
        .subtitle(spec.subtitle)
        .adjustment(&gtk4::Adjustment::new(
            saved,
            spec.min,
            spec.max,
            AUTO_BREAK_MS_STEP,
            AUTO_BREAK_MS_PAGE,
            0.0,
        ))
        .digits(0)
        .build();
    group.add(&row);

    // Capture `spec.key` by Copy (it's `&'static str`) so the
    // GLib closure can own it without borrowing the spec.
    let cfg_clone = Arc::clone(config);
    let key = spec.key;
    row.connect_value_notify(move |r| {
        let val = r.value() as u64;
        cfg_clone.write(|v| {
            v[key] = serde_json::json!(val);
        });
    });
    row
}

/// Build the transcript sidebar panel.
#[allow(clippy::too_many_lines)]
pub fn build_transcript_panel(config: &Arc<ConfigManager>) -> TranscriptPanel {
    let group = adw::PreferencesGroup::builder()
        .title("Transcript")
        .description("Live speech-to-text")
        .build();

    let enable_row = adw::SwitchRow::builder()
        .title("Enable Transcription")
        .build();
    group.add(&enable_row);

    // --- Model selector ---
    //
    // Whisper and Sherpa are mutually exclusive cargo features (see
    // sdr-transcription/src/lib.rs compile_error guards). The model picker
    // shows whichever backend was compiled in. The user picks the build
    // they want at install time:
    //
    //   make install CARGO_FLAGS="--release --features whisper-cuda"     # Whisper + CUDA
    //   make install CARGO_FLAGS="--release --no-default-features --features sherpa-cpu"  # Sherpa CPU
    #[cfg(feature = "whisper")]
    let (model_labels, max_model_idx, key_for_persistence): (Vec<&'static str>, u32, &str) = {
        let labels: Vec<&'static str> = sdr_transcription::WhisperModel::ALL
            .iter()
            .map(|m| m.label())
            .collect();
        #[allow(clippy::cast_possible_truncation)]
        let max = sdr_transcription::WhisperModel::ALL.len() as u32;
        (labels, max, KEY_MODEL)
    };
    #[cfg(feature = "sherpa")]
    let (model_labels, max_model_idx, key_for_persistence): (Vec<&'static str>, u32, &str) = {
        let labels: Vec<&'static str> = sdr_transcription::SherpaModel::ALL
            .iter()
            .map(|m| m.label())
            .collect();
        #[allow(clippy::cast_possible_truncation)]
        let max = sdr_transcription::SherpaModel::ALL.len() as u32;
        (labels, max, KEY_SHERPA_MODEL)
    };

    let model_list = gtk4::StringList::new(&model_labels);

    let saved_model_idx = config.read(|v| {
        v.get(key_for_persistence)
            .and_then(serde_json::Value::as_u64)
            .and_then(|idx| u32::try_from(idx).ok())
            .filter(|&idx| idx < max_model_idx)
            .unwrap_or(0)
    });

    #[cfg(feature = "whisper")]
    let model_title = "Whisper Model";
    #[cfg(feature = "sherpa")]
    let model_title = "Sherpa Model";

    let model_row = adw::ComboRow::builder()
        .title(model_title)
        .model(&model_list)
        .selected(saved_model_idx)
        .build();
    group.add(&model_row);

    // Persist model selection on change.
    //
    // Whisper persists immediately: Whisper has no runtime model swap,
    // so the selection only matters at next launch. Saving it now is
    // harmless even if the user later picks a broken model.
    //
    // Sherpa does NOT persist here. The reload handler in `window.rs`
    // writes `KEY_SHERPA_MODEL` to config only after `InitEvent::Ready`
    // fires — deferring persistence until the recognizer swap actually
    // succeeds. If the reload fails, the previous (working) model
    // stays in config, and next startup's `init_sherpa_host` won't
    // retry a known-broken selection.
    #[cfg(feature = "whisper")]
    {
        let config_model = Arc::clone(config);
        model_row.connect_selected_notify(move |row| {
            let idx = row.selected();
            if idx < max_model_idx {
                config_model.write(|v| {
                    v[key_for_persistence] = serde_json::json!(idx);
                });
            }
        });
    }
    #[cfg(feature = "sherpa")]
    {
        // Reference the local so it's not flagged as unused in sherpa
        // builds — the sherpa reload handler in window.rs owns the
        // persistence logic for this key.
        let _ = key_for_persistence;
    }

    // --- Tuning sliders ---

    // Silence threshold slider — Whisper-only because Sherpa has
    // native endpoint detection (see SherpaBackend::build_recognizer_config).
    #[cfg(feature = "whisper")]
    let silence_row = {
        let saved_silence = config.read(|v| {
            v.get(KEY_SILENCE_THRESHOLD)
                .and_then(serde_json::Value::as_f64)
                .map_or(DEFAULT_SILENCE_THRESHOLD, |val| {
                    val.clamp(SILENCE_THRESHOLD_MIN, SILENCE_THRESHOLD_MAX)
                })
        });

        let row = adw::SpinRow::builder()
            .title("Silence threshold")
            .adjustment(&gtk4::Adjustment::new(
                saved_silence,
                SILENCE_THRESHOLD_MIN,
                SILENCE_THRESHOLD_MAX,
                SILENCE_THRESHOLD_STEP,
                SILENCE_THRESHOLD_PAGE,
                0.0,
            ))
            .digits(3)
            .build();
        group.add(&row);

        let config_silence = Arc::clone(config);
        row.connect_value_notify(move |r| {
            let val = r.value();
            config_silence.write(|v| {
                v[KEY_SILENCE_THRESHOLD] = serde_json::json!(val);
            });
        });

        row
    };

    let saved_noise_gate = config.read(|v| {
        v.get(KEY_NOISE_GATE)
            .and_then(serde_json::Value::as_f64)
            .map_or(DEFAULT_NOISE_GATE, |val| {
                val.clamp(NOISE_GATE_MIN, NOISE_GATE_MAX)
            })
    });

    let noise_gate_row = adw::SpinRow::builder()
        .title("Noise gate")
        .subtitle("Spectral gate ratio")
        .adjustment(&gtk4::Adjustment::new(
            saved_noise_gate,
            NOISE_GATE_MIN,
            NOISE_GATE_MAX,
            NOISE_GATE_STEP,
            NOISE_GATE_PAGE,
            0.0,
        ))
        .digits(1)
        .build();
    group.add(&noise_gate_row);

    let config_noise = Arc::clone(config);
    noise_gate_row.connect_value_notify(move |row| {
        let val = row.value();
        config_noise.write(|v| {
            v[KEY_NOISE_GATE] = serde_json::json!(val);
        });
    });

    // --- Audio enhancement mode selector ---
    //
    // Applies to all recognizer backends (whisper + both sherpa
    // paths). Default Voice-band matches the pre-#281 behavior;
    // users hitting voice-band preprocessor issues (e.g. Moonshine
    // silently returning empty text on NFM speech) can switch to
    // Broadband or Off as a workaround. Persisted as a stable
    // string id via `AudioEnhancement::as_config_str` so future
    // schema migrations don't rely on u32 index stability.
    let audio_enhancement_row = {
        let list = gtk4::StringList::new(AUDIO_ENHANCEMENT_LABELS);

        let saved_idx = config.read(|v| {
            let s = v
                .get(KEY_AUDIO_ENHANCEMENT)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("voice_band");
            match sdr_transcription::denoise::AudioEnhancement::from_config_str(s) {
                sdr_transcription::denoise::AudioEnhancement::VoiceBand => {
                    AUDIO_ENHANCEMENT_VOICE_BAND_IDX
                }
                sdr_transcription::denoise::AudioEnhancement::Broadband => {
                    AUDIO_ENHANCEMENT_BROADBAND_IDX
                }
                sdr_transcription::denoise::AudioEnhancement::Off => AUDIO_ENHANCEMENT_OFF_IDX,
            }
        });

        let row = adw::ComboRow::builder()
            .title("Audio enhancement")
            .subtitle(
                "Voice-band (recommended) • Broadband if your recognizer returns no text \
                 • Off for pristine source audio",
            )
            .model(&list)
            .selected(saved_idx)
            .build();
        group.add(&row);

        let config_enhancement = Arc::clone(config);
        row.connect_selected_notify(move |r| {
            // Map combo index → AudioEnhancement → stable config
            // string. Only persist if the index matches one of
            // the three known-valid values. GTK `ComboRow` emits
            // `selected-notify` with transient out-of-range
            // indices during intermediate widget state changes
            // (e.g. during model repopulation), and the lenient
            // "fall through to VoiceBand" pattern the ACTIVE
            // dispatch path uses is dangerous here — it would
            // silently overwrite a user's Broadband or Off
            // workaround with the default on a spurious signal.
            // Runtime dispatch (window.rs BackendConfig build) can
            // still be lenient because it reads the current value
            // once at session start; this persistence handler is
            // the one that cares about transient signals.
            let Some(value) = (match r.selected() {
                AUDIO_ENHANCEMENT_VOICE_BAND_IDX => {
                    Some(sdr_transcription::denoise::AudioEnhancement::VoiceBand)
                }
                AUDIO_ENHANCEMENT_BROADBAND_IDX => {
                    Some(sdr_transcription::denoise::AudioEnhancement::Broadband)
                }
                AUDIO_ENHANCEMENT_OFF_IDX => {
                    Some(sdr_transcription::denoise::AudioEnhancement::Off)
                }
                _ => None,
            }) else {
                return;
            };
            config_enhancement.write(|v| {
                v[KEY_AUDIO_ENHANCEMENT] = serde_json::json!(value.as_config_str());
            });
        });

        row
    };

    // --- VAD threshold slider (Sherpa + offline models only) ---
    //
    // Only visible when an offline model (Moonshine, Parakeet) is selected.
    // The Silero VAD's default 0.5 threshold is too strict for noisy NFM
    // scanner audio — this slider lets the user tune it per source.
    // Visibility is toggled in the sherpa block below alongside display_mode_row.
    #[cfg(feature = "sherpa")]
    let vad_threshold_row = {
        let saved_vad_threshold = config.read(|v| {
            v.get(KEY_SHERPA_VAD_THRESHOLD)
                .and_then(serde_json::Value::as_f64)
                .map_or(DEFAULT_SHERPA_VAD_THRESHOLD, |val| {
                    val.clamp(SHERPA_VAD_THRESHOLD_MIN, SHERPA_VAD_THRESHOLD_MAX)
                })
        });

        let row = adw::SpinRow::builder()
            .title("VAD threshold")
            .subtitle("Lower catches quieter audio (NFM); higher is stricter (talk radio)")
            .adjustment(&gtk4::Adjustment::new(
                saved_vad_threshold,
                SHERPA_VAD_THRESHOLD_MIN,
                SHERPA_VAD_THRESHOLD_MAX,
                SHERPA_VAD_THRESHOLD_STEP,
                SHERPA_VAD_THRESHOLD_PAGE,
                0.0,
            ))
            .digits(2)
            .build();
        group.add(&row);

        let config_vad = Arc::clone(config);
        row.connect_value_notify(move |r| {
            let val = r.value();
            config_vad.write(|v| {
                v[KEY_SHERPA_VAD_THRESHOLD] = serde_json::json!(val);
            });
        });

        row
    };

    #[cfg(feature = "sherpa")]
    let auto_break_row = {
        let saved = config.read(|v| {
            v.get(KEY_AUTO_BREAK_ENABLED)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        });

        let row = adw::SwitchRow::builder()
            .title("Auto Break")
            .subtitle(
                "Use the radio's squelch as the transcription boundary instead of VAD. NFM only.",
            )
            .active(saved)
            .build();
        group.add(&row);

        let config_ab = Arc::clone(config);
        row.connect_active_notify(move |r| {
            let active = r.is_active();
            config_ab.write(|v| {
                v[KEY_AUTO_BREAK_ENABLED] = serde_json::json!(active);
            });
        });

        row
    };

    // --- Auto Break timing sliders (Sherpa only) ---
    //
    // Three SpinRows for the tunable hold-off constants. Only visible
    // when Auto Break itself is visible (offline sherpa model + NFM)
    // AND Auto Break is ON — mirroring the mutex with `vad_threshold_row`.
    // Defaults match the PR 8 hardcoded values; user overrides are
    // persisted as u64 milliseconds in config. Construction is
    // delegated to `build_persisted_ms_slider` so the three rows
    // share one load/clamp/build/persist code path.
    #[cfg(feature = "sherpa")]
    let auto_break_min_open_row = build_persisted_ms_slider(
        &group,
        config,
        &MsSliderSpec {
            key: KEY_AUTO_BREAK_MIN_OPEN_MS,
            title: "Auto Break: min open (ms)",
            subtitle: "Transmissions shorter than this are discarded as noise spikes",
            min: AUTO_BREAK_MIN_OPEN_MS_MIN,
            max: AUTO_BREAK_MIN_OPEN_MS_MAX,
            default: AUTO_BREAK_MIN_OPEN_MS_DEFAULT,
        },
    );

    #[cfg(feature = "sherpa")]
    let auto_break_tail_row = build_persisted_ms_slider(
        &group,
        config,
        &MsSliderSpec {
            key: KEY_AUTO_BREAK_TAIL_MS,
            title: "Auto Break: tail (ms)",
            subtitle: "Continue buffering audio this long after squelch closes",
            min: AUTO_BREAK_TAIL_MS_MIN,
            max: AUTO_BREAK_TAIL_MS_MAX,
            default: AUTO_BREAK_TAIL_MS_DEFAULT,
        },
    );

    #[cfg(feature = "sherpa")]
    let auto_break_min_segment_row = build_persisted_ms_slider(
        &group,
        config,
        &MsSliderSpec {
            key: KEY_AUTO_BREAK_MIN_SEGMENT_MS,
            title: "Auto Break: min segment (ms)",
            subtitle: "Segments shorter than this are discarded instead of decoded",
            min: AUTO_BREAK_MIN_SEGMENT_MS_MIN,
            max: AUTO_BREAK_MIN_SEGMENT_MS_MAX,
            default: AUTO_BREAK_MIN_SEGMENT_MS_DEFAULT,
        },
    );

    // --- Display mode selector (Sherpa only) ---
    //
    // Whisper builds never compile this in — Whisper does not emit
    // `TranscriptionEvent::Partial`, so there's nothing to render in a
    // "live line". Sherpa builds default to "Live captions" because
    // streaming is the whole point; users can switch to "Final only"
    // if the in-place updates are visually distracting.
    #[cfg(feature = "sherpa")]
    let display_mode_row = {
        let list = gtk4::StringList::new(DISPLAY_MODE_LABELS);

        let saved_idx = config.read(|v| {
            v.get(KEY_DISPLAY_MODE)
                .and_then(serde_json::Value::as_str)
                .map_or(DISPLAY_MODE_LIVE_IDX, |s| match s {
                    "final" => DISPLAY_MODE_FINAL_IDX,
                    _ => DISPLAY_MODE_LIVE_IDX,
                })
        });

        let row = adw::ComboRow::builder()
            .title("Display mode")
            .subtitle("Live captions update in place; Final only shows committed text")
            .model(&list)
            .selected(saved_idx)
            .build();
        group.add(&row);

        let config_display = Arc::clone(config);
        row.connect_selected_notify(move |r| {
            let value = match r.selected() {
                DISPLAY_MODE_FINAL_IDX => "final",
                _ => "live",
            };
            config_display.write(|v| {
                v[KEY_DISPLAY_MODE] = serde_json::json!(value);
            });
        });

        row
    };

    // Toggle display_mode_row and vad_threshold_row visibility based on
    // whether the selected model emits partial hypotheses:
    //   - display_mode_row: visible for online models (supports_partials)
    //   - vad_threshold_row: visible for offline models (!supports_partials)
    // Models like Moonshine/Parakeet are offline — the Live/Final distinction
    // is meaningless so display_mode_row is hidden; but they DO use Silero
    // VAD so vad_threshold_row is shown. Zipformer is streaming so
    // display_mode_row is shown and vad_threshold_row is hidden.
    // Initial visibility is set here based on the currently-saved model index.
    #[cfg(feature = "sherpa")]
    {
        let initial_supports_partials = sdr_transcription::SherpaModel::ALL
            .get(saved_model_idx as usize)
            .copied()
            .is_some_and(sdr_transcription::SherpaModel::supports_partials);
        let initial_is_offline = !initial_supports_partials;
        let initial_auto_break_active = auto_break_row.is_active();

        display_mode_row.set_visible(initial_supports_partials);
        // VAD slider visible only when offline model AND Auto Break is OFF.
        // When Auto Break is ON, it replaces the VAD slider functionally so
        // showing both would confuse the user about which one is driving
        // segmentation.
        vad_threshold_row.set_visible(initial_is_offline && !initial_auto_break_active);
        // Auto Break toggle visible only when an offline model is selected.
        // The additional NFM demod-mode gate is applied by window.rs's
        // DemodModeChanged handler — at widget-build time we don't yet
        // know the demod mode and assume NFM is the common case.
        auto_break_row.set_visible(initial_is_offline);
        // Auto Break timing sliders are the mirror of the VAD slider:
        // visible when offline model AND Auto Break is ON. Together
        // with `vad_threshold_row` they form a mutex visible triplet
        // — exactly one of (VAD slider) or (Auto Break sliders) is
        // shown at any time for an offline model.
        let ab_sliders_visible = initial_is_offline && initial_auto_break_active;
        auto_break_min_open_row.set_visible(ab_sliders_visible);
        auto_break_tail_row.set_visible(ab_sliders_visible);
        auto_break_min_segment_row.set_visible(ab_sliders_visible);

        let display_mode_row_for_visibility = display_mode_row.clone();
        let vad_threshold_row_for_visibility = vad_threshold_row.clone();
        let auto_break_row_for_model_change = auto_break_row.clone();
        let ab_min_open_for_model_change = auto_break_min_open_row.clone();
        let ab_tail_for_model_change = auto_break_tail_row.clone();
        let ab_min_segment_for_model_change = auto_break_min_segment_row.clone();
        model_row.connect_selected_notify(move |r| {
            let idx = r.selected() as usize;
            let supports_partials = sdr_transcription::SherpaModel::ALL
                .get(idx)
                .copied()
                .is_some_and(sdr_transcription::SherpaModel::supports_partials);
            let is_offline = !supports_partials;
            let ab_active = auto_break_row_for_model_change.is_active();
            let ab_sliders = is_offline && ab_active;

            display_mode_row_for_visibility.set_visible(supports_partials);
            vad_threshold_row_for_visibility.set_visible(is_offline && !ab_active);
            auto_break_row_for_model_change.set_visible(is_offline);
            ab_min_open_for_model_change.set_visible(ab_sliders);
            ab_tail_for_model_change.set_visible(ab_sliders);
            ab_min_segment_for_model_change.set_visible(ab_sliders);
        });

        // Mutex: toggling Auto Break hides/shows the VAD threshold slider
        // AND the Auto Break timing sliders. Only applies when Auto Break
        // itself is currently visible (an offline model is selected). If
        // the row is hidden (streaming Zipformer), the mutex doesn't
        // apply because the VAD slider is already hidden by the
        // offline-model check and the AB sliders follow.
        let vad_threshold_row_for_mutex = vad_threshold_row.clone();
        let auto_break_row_for_mutex = auto_break_row.clone();
        let ab_min_open_for_mutex = auto_break_min_open_row.clone();
        let ab_tail_for_mutex = auto_break_tail_row.clone();
        let ab_min_segment_for_mutex = auto_break_min_segment_row.clone();
        auto_break_row.connect_active_notify(move |r| {
            if auto_break_row_for_mutex.is_visible() {
                let ab_on = r.is_active();
                vad_threshold_row_for_mutex.set_visible(!ab_on);
                ab_min_open_for_mutex.set_visible(ab_on);
                ab_tail_for_mutex.set_visible(ab_on);
                ab_min_segment_for_mutex.set_visible(ab_on);
            }
        });
    }

    // --- Live caption line (Sherpa only) ---
    //
    // Dimmed italic label that renders in-progress Sherpa partials.
    // Initially hidden; becomes visible once a Partial event arrives
    // and the current display mode is "Live captions". When display
    // mode is "Final only" the label stays hidden entirely.
    #[cfg(feature = "sherpa")]
    let live_line_label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .css_classes(["dim-label"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(2)
        .margin_bottom(4)
        .visible(false)
        .build();

    // Italicize via Pango markup attribute list so we don't need a
    // custom CSS rule. The text is set via set_text() later; the
    // attributes persist across text changes.
    #[cfg(feature = "sherpa")]
    {
        let attrs = gtk4::pango::AttrList::new();
        attrs.insert(gtk4::pango::AttrInt::new_style(gtk4::pango::Style::Italic));
        live_line_label.set_attributes(Some(&attrs));
    }

    // Mid-session mode flip: if the user switches to "Final only" while a
    // partial is visible, clear and hide it immediately. Without this, a
    // stale live line would linger until the next commit or Clear press.
    // Persistence is handled by the first connect_selected_notify above;
    // GLib chains handlers, so both fire on every selection change.
    #[cfg(feature = "sherpa")]
    {
        let live_line_for_mode = live_line_label.clone();
        display_mode_row.connect_selected_notify(move |r| {
            if r.selected() == DISPLAY_MODE_FINAL_IDX {
                live_line_for_mode.set_text("");
                live_line_for_mode.set_visible(false);
            }
        });
    }

    // Always clear the live line on model change. The visibility-toggle
    // handler earlier hides display_mode_row when switching to a
    // non-partial-emitting model, but it can't see live_line_label
    // (which is built after that handler runs) so it leaves any stale
    // live-line content visible. Without this third chained handler, a
    // user who ran a Zipformer session and then switched to Moonshine
    // or Parakeet would see leftover italic text dangling under the
    // text view.
    #[cfg(feature = "sherpa")]
    {
        let live_line_for_model_change = live_line_label.clone();
        model_row.connect_selected_notify(move |_| {
            live_line_for_model_change.set_text("");
            live_line_for_model_change.set_visible(false);
        });
    }

    let status_label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .css_classes(["dim-label"])
        .visible(false)
        .margin_start(12)
        .margin_top(4)
        .build();

    let progress_bar = gtk4::ProgressBar::builder()
        .visible(false)
        .margin_start(12)
        .margin_end(12)
        .margin_top(4)
        .build();

    // `WordChar` wraps on word boundaries OR mid-word when a single
    // token is wider than the panel — critical for monospace-rendered
    // transcription output, where a long contiguous token (non-Latin
    // script, URLs, technical jargon) with plain `Word` wrapping
    // grows the `TextView`'s natural width, propagates up through the
    // scrolled window, and fights the sidebar `min-sidebar-width`.
    // That fight reads as layout "bouncing" while captions stream in.
    let text_view = gtk4::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk4::WrapMode::WordChar)
        .monospace(true)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();

    let scroll = gtk4::ScrolledWindow::builder()
        .child(&text_view)
        // `hscrollbar_policy=Never` keeps the horizontal scrollbar
        // from appearing as a secondary symptom of the above —
        // with `WordChar` wrapping it's never needed, and an
        // `Automatic` policy would briefly flash the scrollbar
        // while content was still renegotiating width.
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .min_content_height(150)
        .vexpand(true)
        .css_classes(["card"])
        .margin_top(8)
        .build();

    let clear_button = gtk4::Button::builder()
        .label("Clear")
        .halign(gtk4::Align::Start)
        .margin_top(4)
        .build();

    let text_view_clear = text_view.clone();
    #[cfg(feature = "sherpa")]
    let live_line_for_clear = live_line_label.clone();
    clear_button.connect_clicked(move |_| {
        text_view_clear.buffer().set_text("");
        #[cfg(feature = "sherpa")]
        {
            live_line_for_clear.set_text("");
            live_line_for_clear.set_visible(false);
        }
    });

    let content_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(4)
        .vexpand(true)
        .build();
    content_box.append(&status_label);
    content_box.append(&progress_bar);
    content_box.append(&scroll);
    #[cfg(feature = "sherpa")]
    content_box.append(&live_line_label);
    content_box.append(&clear_button);
    group.add(&content_box);

    TranscriptPanel {
        widget: group,
        enable_row,
        model_row,
        #[cfg(feature = "whisper")]
        silence_row,
        noise_gate_row,
        audio_enhancement_row,
        #[cfg(feature = "sherpa")]
        display_mode_row,
        #[cfg(feature = "sherpa")]
        vad_threshold_row,
        #[cfg(feature = "sherpa")]
        auto_break_row,
        #[cfg(feature = "sherpa")]
        auto_break_min_open_row,
        #[cfg(feature = "sherpa")]
        auto_break_tail_row,
        #[cfg(feature = "sherpa")]
        auto_break_min_segment_row,
        #[cfg(feature = "sherpa")]
        live_line_label,
        status_label,
        progress_bar,
        text_view,
        scroll,
        clear_button,
    }
}
