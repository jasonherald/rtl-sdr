//! Transcript sidebar panel — displays live transcription results.

use std::sync::Arc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;

/// Config key for the persisted Whisper model index.
const KEY_MODEL: &str = "transcription_model";
/// Config key for the silence threshold after spectral denoising.
const KEY_SILENCE_THRESHOLD: &str = "transcription_silence_threshold";
/// Config key for the spectral noise gate ratio.
const KEY_NOISE_GATE: &str = "transcription_noise_gate";
/// Config key for the persisted backend selection ("whisper" or "sherpa").
const KEY_BACKEND: &str = "transcription_backend";
/// Config key for the persisted Sherpa model index.
const KEY_SHERPA_MODEL: &str = "transcription_sherpa_model";

/// Backend index for Whisper in the backend selector `ComboRow`.
const BACKEND_IDX_WHISPER: u32 = 0;
/// Backend index for Sherpa in the backend selector `ComboRow`.
const BACKEND_IDX_SHERPA: u32 = 1;

// Silence threshold slider defaults and range.
const DEFAULT_SILENCE_THRESHOLD: f64 = 0.007;
const SILENCE_THRESHOLD_MIN: f64 = 0.001;
const SILENCE_THRESHOLD_MAX: f64 = 0.100;
const SILENCE_THRESHOLD_STEP: f64 = 0.001;
const SILENCE_THRESHOLD_PAGE: f64 = 0.01;

// Noise gate slider defaults and range.
const DEFAULT_NOISE_GATE: f64 = 3.0;
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
    /// Backend selector (Whisper / Sherpa).
    pub backend_row: adw::ComboRow,
    /// Model size selector — contents change based on backend selection.
    pub model_row: adw::ComboRow,
    /// Silence threshold spin row.
    pub silence_row: adw::SpinRow,
    /// Noise gate spin row.
    pub noise_gate_row: adw::SpinRow,
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

    // --- Backend selector ---
    let backend_labels = ["Whisper", "Sherpa (streaming)"];
    let backend_list = gtk4::StringList::new(&backend_labels);

    let saved_backend_idx = config.read(|v| {
        v.get(KEY_BACKEND)
            .and_then(serde_json::Value::as_str)
            .map_or(BACKEND_IDX_WHISPER, |s| match s {
                "sherpa" => BACKEND_IDX_SHERPA,
                _ => BACKEND_IDX_WHISPER,
            })
    });

    let backend_row = adw::ComboRow::builder()
        .title("Backend")
        .model(&backend_list)
        .selected(saved_backend_idx)
        .build();
    group.add(&backend_row);

    // --- Model selector ---
    //
    // Contents are populated based on the active backend. We rebuild the
    // string list each time the backend changes; the selected index
    // resets to whichever value was last persisted for that backend.
    let whisper_model_labels: Vec<&'static str> = sdr_transcription::WhisperModel::ALL
        .iter()
        .map(|m| m.label())
        .collect();
    let sherpa_model_labels: Vec<&'static str> = sdr_transcription::SherpaModel::ALL
        .iter()
        .map(|m| m.label())
        .collect();

    #[allow(clippy::cast_possible_truncation)]
    let max_whisper_idx = sdr_transcription::WhisperModel::ALL.len() as u32;
    #[allow(clippy::cast_possible_truncation)]
    let max_sherpa_idx = sdr_transcription::SherpaModel::ALL.len() as u32;

    let saved_whisper_model_idx = config.read(|v| {
        v.get(KEY_MODEL)
            .and_then(serde_json::Value::as_u64)
            .and_then(|idx| u32::try_from(idx).ok())
            .filter(|&idx| idx < max_whisper_idx)
            .unwrap_or(0)
    });
    let saved_sherpa_model_idx = config.read(|v| {
        v.get(KEY_SHERPA_MODEL)
            .and_then(serde_json::Value::as_u64)
            .and_then(|idx| u32::try_from(idx).ok())
            .filter(|&idx| idx < max_sherpa_idx)
            .unwrap_or(0)
    });

    let initial_model_list = if saved_backend_idx == BACKEND_IDX_SHERPA {
        gtk4::StringList::new(&sherpa_model_labels)
    } else {
        gtk4::StringList::new(&whisper_model_labels)
    };
    let initial_model_idx = if saved_backend_idx == BACKEND_IDX_SHERPA {
        saved_sherpa_model_idx
    } else {
        saved_whisper_model_idx
    };

    let model_row = adw::ComboRow::builder()
        .title("Model")
        .model(&initial_model_list)
        .selected(initial_model_idx)
        .build();
    group.add(&model_row);

    // --- Backend change handler ---
    //
    // Rebuilds the model picker contents and persists the new backend.
    let config_backend = Arc::clone(config);
    let model_row_for_backend_change = model_row.clone();
    let whisper_labels_for_backend = whisper_model_labels.clone();
    let sherpa_labels_for_backend = sherpa_model_labels.clone();
    backend_row.connect_selected_notify(move |row| {
        let idx = row.selected();
        let (backend_str, new_list, new_idx) = if idx == BACKEND_IDX_SHERPA {
            (
                "sherpa",
                gtk4::StringList::new(&sherpa_labels_for_backend),
                config_backend.read(|v| {
                    v.get(KEY_SHERPA_MODEL)
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|i| u32::try_from(i).ok())
                        .filter(|&i| i < max_sherpa_idx)
                        .unwrap_or(0)
                }),
            )
        } else {
            (
                "whisper",
                gtk4::StringList::new(&whisper_labels_for_backend),
                config_backend.read(|v| {
                    v.get(KEY_MODEL)
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|i| u32::try_from(i).ok())
                        .filter(|&i| i < max_whisper_idx)
                        .unwrap_or(0)
                }),
            )
        };

        model_row_for_backend_change.set_model(Some(&new_list));
        model_row_for_backend_change.set_selected(new_idx);

        config_backend.write(|v| {
            v[KEY_BACKEND] = serde_json::json!(backend_str);
        });
    });

    // --- Model change handler ---
    //
    // Persists to KEY_MODEL or KEY_SHERPA_MODEL depending on which
    // backend is currently selected.
    let config_model = Arc::clone(config);
    let backend_row_for_model_change = backend_row.clone();
    model_row.connect_selected_notify(move |row| {
        let idx = row.selected();
        let backend_idx = backend_row_for_model_change.selected();
        let key = if backend_idx == BACKEND_IDX_SHERPA {
            KEY_SHERPA_MODEL
        } else {
            KEY_MODEL
        };
        config_model.write(|v| {
            v[key] = serde_json::json!(idx);
        });
    });

    // --- Tuning sliders ---

    let saved_silence = config.read(|v| {
        v.get(KEY_SILENCE_THRESHOLD)
            .and_then(serde_json::Value::as_f64)
            .map_or(DEFAULT_SILENCE_THRESHOLD, |val| {
                val.clamp(SILENCE_THRESHOLD_MIN, SILENCE_THRESHOLD_MAX)
            })
    });

    let silence_row = adw::SpinRow::builder()
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
    group.add(&silence_row);

    let config_silence = Arc::clone(config);
    silence_row.connect_value_notify(move |row| {
        let val = row.value();
        config_silence.write(|v| {
            v[KEY_SILENCE_THRESHOLD] = serde_json::json!(val);
        });
    });

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

    let text_view = gtk4::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk4::WrapMode::Word)
        .monospace(true)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();

    let scroll = gtk4::ScrolledWindow::builder()
        .child(&text_view)
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
    clear_button.connect_clicked(move |_| {
        text_view_clear.buffer().set_text("");
    });

    let content_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(4)
        .vexpand(true)
        .build();
    content_box.append(&status_label);
    content_box.append(&progress_bar);
    content_box.append(&scroll);
    content_box.append(&clear_button);
    group.add(&content_box);

    TranscriptPanel {
        widget: group,
        enable_row,
        backend_row,
        model_row,
        silence_row,
        noise_gate_row,
        status_label,
        progress_bar,
        text_view,
        scroll,
        clear_button,
    }
}
