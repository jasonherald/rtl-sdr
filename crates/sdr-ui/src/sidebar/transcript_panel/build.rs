//! Widget construction for the Transcript panel (issue #819):
//! [`build_transcript_panel`] plus the [`MsSliderSpec`]-driven
//! Auto Break slider builder. Split out of `transcript_panel.rs`
//! per the file-size pass. The heavy `#[cfg(feature = …)]` gating
//! mirrors the whisper/sherpa mutex documented in
//! `sdr-transcription`'s lib.rs.

use std::sync::Arc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;

mod ms_slider;
#[cfg(feature = "sherpa")]
mod sherpa;
#[cfg(feature = "sherpa")]
use sherpa::{SherpaRows, build_sherpa_rows};

#[cfg(feature = "sherpa")]
use super::KEY_SHERPA_MODEL;
use super::{
    AUDIO_ENHANCEMENT_BROADBAND_IDX, AUDIO_ENHANCEMENT_LABELS, AUDIO_ENHANCEMENT_OFF_IDX,
    AUDIO_ENHANCEMENT_VOICE_BAND_IDX, DEFAULT_NOISE_GATE, KEY_AUDIO_ENHANCEMENT, KEY_NOISE_GATE,
    NOISE_GATE_MAX, NOISE_GATE_MIN, NOISE_GATE_PAGE, NOISE_GATE_STEP, STATUS_AREA_MARGIN_H,
    STATUS_AREA_MARGIN_TOP, STATUS_LABEL_MAX_WIDTH_CHARS, TranscriptPanel,
};
#[cfg(feature = "whisper")]
use super::{
    DEFAULT_SILENCE_THRESHOLD, KEY_MODEL, KEY_SILENCE_THRESHOLD, SILENCE_THRESHOLD_MAX,
    SILENCE_THRESHOLD_MIN, SILENCE_THRESHOLD_PAGE, SILENCE_THRESHOLD_STEP,
};

/// Model-selector combo (Whisper or Sherpa catalog, per the
/// compiled backend) plus its persistence wiring. Returns the row
/// and the restored model index (the sherpa visibility seeding
/// needs it). Split out of [`build_transcript_panel`] per the
/// 50-NLOC gate (#819, PR #880 Codacy precedent).
fn build_model_selector(
    group: &adw::PreferencesGroup,
    config: &Arc<ConfigManager>,
) -> (adw::ComboRow, u32) {
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

    wire_model_persistence(&model_row, config, max_model_idx, key_for_persistence);

    (model_row, saved_model_idx)
}

/// Persist the model selection on change — whisper immediately,
/// sherpa deferred to the reload handler (see the comment inside).
/// Split out of [`build_model_selector`] per the 50-NLOC gate (#819).
#[allow(
    unused_variables,
    reason = "sherpa builds intentionally consume the args without wiring — the reload handler in window.rs owns KEY_SHERPA_MODEL persistence"
)]
fn wire_model_persistence(
    model_row: &adw::ComboRow,
    config: &Arc<ConfigManager>,
    max_model_idx: u32,
    key_for_persistence: &'static str,
) {
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
}

/// Whisper-only silence-threshold slider. Split out per the 50-NLOC
/// gate (#819).
#[cfg(feature = "whisper")]
fn build_silence_row(group: &adw::PreferencesGroup, config: &Arc<ConfigManager>) -> adw::SpinRow {
    // Silence threshold slider — Whisper-only because Sherpa has
    // native endpoint detection (see SherpaBackend::build_recognizer_config).
    {
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
    }
}

/// Noise-gate slider. Split out per the 50-NLOC gate (#819).
fn build_noise_gate_row(
    group: &adw::PreferencesGroup,
    config: &Arc<ConfigManager>,
) -> adw::SpinRow {
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

    noise_gate_row
}

/// Audio-enhancement mode selector (#281). Split out per the
/// 50-NLOC gate (#819).
fn build_audio_enhancement_row(
    group: &adw::PreferencesGroup,
    config: &Arc<ConfigManager>,
) -> adw::ComboRow {
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
        let saved_idx = saved_audio_enhancement_idx(config);

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

    wire_audio_enhancement_persistence(&audio_enhancement_row, config);
    audio_enhancement_row
}

/// Restored combo index for the audio-enhancement selector,
/// mapping the persisted stable string id back through
/// `AudioEnhancement::from_config_str`. Split out of
/// [`build_audio_enhancement_row`] per the 50-NLOC gate (#819).
fn saved_audio_enhancement_idx(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
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
    })
}

/// Strict-gate persistence for the audio-enhancement combo. Split
/// out of [`build_audio_enhancement_row`] per the 50-NLOC gate
/// (#819); the transient-index rationale in the comment below is
/// why this handler can't share the dispatch path's lenient parse.
fn wire_audio_enhancement_persistence(row: &adw::ComboRow, config: &Arc<ConfigManager>) {
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
            AUDIO_ENHANCEMENT_OFF_IDX => Some(sdr_transcription::denoise::AudioEnhancement::Off),
            _ => None,
        }) else {
            return;
        };
        config_enhancement.write(|v| {
            v[KEY_AUDIO_ENHANCEMENT] = serde_json::json!(value.as_config_str());
        });
    });
}

/// Status label + model-download progress bar. Split out per the
/// 50-NLOC gate (#819).
fn build_status_area() -> (gtk4::Label, gtk4::ProgressBar) {
    let status_label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .css_classes(["dim-label"])
        .visible(false)
        .margin_start(STATUS_AREA_MARGIN_H)
        .margin_end(STATUS_AREA_MARGIN_H)
        .margin_top(STATUS_AREA_MARGIN_TOP)
        // Long status/error text (e.g. the per-backend model-support
        // message, #858) must wrap instead of forcing the panel — and
        // with it the whole window — wider. xalign keeps wrapped
        // lines left-aligned.
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .max_width_chars(STATUS_LABEL_MAX_WIDTH_CHARS)
        .xalign(0.0)
        .build();

    let progress_bar = gtk4::ProgressBar::builder()
        .visible(false)
        .margin_start(STATUS_AREA_MARGIN_H)
        .margin_end(STATUS_AREA_MARGIN_H)
        .margin_top(STATUS_AREA_MARGIN_TOP)
        .build();

    // `WordChar` wraps on word boundaries OR mid-word when a single
    // token is wider than the panel — critical for monospace-rendered
    // transcription output, where a long contiguous token (non-Latin
    // script, URLs, technical jargon) with plain `Word` wrapping
    // grows the `TextView`'s natural width, propagates up through the
    // scrolled window, and fights the sidebar `min-sidebar-width`.
    // That fight reads as layout "bouncing" while captions stream in.
    (status_label, progress_bar)
}

/// Transcript text view, its scroll container, and the Clear
/// button (unwired). Split out per the 50-NLOC gate (#819).
fn build_transcript_log() -> (gtk4::TextView, gtk4::ScrolledWindow, gtk4::Button) {
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

    (text_view, scroll, clear_button)
}

/// Shared (whisper + sherpa) widget bundle handed to
/// [`assemble_transcript_panel`]. Grouped per the `ShareConfigRows`
/// precedent so the assembler stays under the parameter gate (#819).
struct SharedRows {
    enable_row: adw::SwitchRow,
    model_row: adw::ComboRow,
    noise_gate_row: adw::SpinRow,
    audio_enhancement_row: adw::ComboRow,
    status_label: gtk4::Label,
    progress_bar: gtk4::ProgressBar,
    text_view: gtk4::TextView,
    scroll: gtk4::ScrolledWindow,
    clear_button: gtk4::Button,
}

/// Final assembly: pack the status/progress/log/clear column into
/// the group and build the flat [`TranscriptPanel`] handle from the
/// grouped bundles. Split out per the 50-NLOC gate (#819).
fn assemble_transcript_panel(
    widget: adw::PreferencesGroup,
    shared: SharedRows,
    #[cfg(feature = "whisper")] silence_row: adw::SpinRow,
    #[cfg(feature = "sherpa")] sherpa: SherpaRows,
) -> TranscriptPanel {
    let content_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(4)
        .vexpand(true)
        .build();
    content_box.append(&shared.status_label);
    content_box.append(&shared.progress_bar);
    content_box.append(&shared.scroll);
    #[cfg(feature = "sherpa")]
    content_box.append(&sherpa.live_line_label);
    content_box.append(&shared.clear_button);
    widget.add(&content_box);

    TranscriptPanel {
        widget,
        enable_row: shared.enable_row,
        model_row: shared.model_row,
        #[cfg(feature = "whisper")]
        silence_row,
        noise_gate_row: shared.noise_gate_row,
        audio_enhancement_row: shared.audio_enhancement_row,
        #[cfg(feature = "sherpa")]
        display_mode_row: sherpa.display_mode_row,
        #[cfg(feature = "sherpa")]
        vad_threshold_row: sherpa.vad_threshold_row,
        #[cfg(feature = "sherpa")]
        auto_break_row: sherpa.auto_break_row,
        #[cfg(feature = "sherpa")]
        auto_break_min_open_row: sherpa.auto_break_min_open_row,
        #[cfg(feature = "sherpa")]
        auto_break_tail_row: sherpa.auto_break_tail_row,
        #[cfg(feature = "sherpa")]
        auto_break_min_segment_row: sherpa.auto_break_min_segment_row,
        #[cfg(feature = "sherpa")]
        live_line_label: sherpa.live_line_label,
        status_label: shared.status_label,
        progress_bar: shared.progress_bar,
        text_view: shared.text_view,
        scroll: shared.scroll,
        clear_button: shared.clear_button,
    }
}

/// Wire the Clear button: empties the transcript buffer and, on
/// sherpa builds, clears + hides the live caption line. Split out
/// per the 50-NLOC gate (#819).
fn wire_clear_button(
    clear_button: &gtk4::Button,
    text_view: &gtk4::TextView,
    #[cfg(feature = "sherpa")] live_line_label: &gtk4::Label,
) {
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
}

/// Build the transcript sidebar panel.
pub fn build_transcript_panel(config: &Arc<ConfigManager>) -> TranscriptPanel {
    let group = adw::PreferencesGroup::builder()
        .title("Transcript")
        .description("Live speech-to-text")
        .build();

    let enable_row = adw::SwitchRow::builder()
        .title("Enable Transcription")
        .build();
    group.add(&enable_row);

    let (model_row, saved_model_idx) = build_model_selector(&group, config);
    #[cfg(not(feature = "sherpa"))]
    let _ = saved_model_idx;
    #[cfg(feature = "whisper")]
    let silence_row = build_silence_row(&group, config);
    let noise_gate_row = build_noise_gate_row(&group, config);
    let audio_enhancement_row = build_audio_enhancement_row(&group, config);
    #[cfg(feature = "sherpa")]
    let sherpa = build_sherpa_rows(&group, config, &model_row, saved_model_idx);

    let (status_label, progress_bar) = build_status_area();
    let (text_view, scroll, clear_button) = build_transcript_log();
    wire_clear_button(
        &clear_button,
        &text_view,
        #[cfg(feature = "sherpa")]
        &sherpa.live_line_label,
    );

    assemble_transcript_panel(
        group,
        SharedRows {
            enable_row,
            model_row,
            noise_gate_row,
            audio_enhancement_row,
            status_label,
            progress_bar,
            text_view,
            scroll,
            clear_button,
        },
        #[cfg(feature = "whisper")]
        silence_row,
        #[cfg(feature = "sherpa")]
        sherpa,
    )
}
