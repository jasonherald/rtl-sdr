//! Transcript sidebar panel — displays live transcription results.

use std::sync::Arc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;

#[cfg(feature = "whisper")]
/// Config key for the persisted Whisper model index.
const KEY_MODEL: &str = "transcription_model";
/// Config key for the silence threshold after spectral denoising.
const KEY_SILENCE_THRESHOLD: &str = "transcription_silence_threshold";
/// Config key for the spectral noise gate ratio.
const KEY_NOISE_GATE: &str = "transcription_noise_gate";
#[cfg(feature = "sherpa")]
/// Config key for the persisted Sherpa model index.
const KEY_SHERPA_MODEL: &str = "transcription_sherpa_model";

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
    /// Model size selector — shows Whisper or Sherpa models based on
    /// which cargo feature was compiled in.
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
    let config_model = Arc::clone(config);
    model_row.connect_selected_notify(move |row| {
        let idx = row.selected();
        if idx < max_model_idx {
            config_model.write(|v| {
                v[key_for_persistence] = serde_json::json!(idx);
            });
        }
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
