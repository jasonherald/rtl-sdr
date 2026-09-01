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
use ms_slider::{MsSliderSpec, build_persisted_ms_slider};

use super::{
    AUDIO_ENHANCEMENT_BROADBAND_IDX, AUDIO_ENHANCEMENT_LABELS, AUDIO_ENHANCEMENT_OFF_IDX,
    AUDIO_ENHANCEMENT_VOICE_BAND_IDX, DEFAULT_NOISE_GATE, KEY_AUDIO_ENHANCEMENT, KEY_NOISE_GATE,
    NOISE_GATE_MAX, NOISE_GATE_MIN, NOISE_GATE_PAGE, NOISE_GATE_STEP, STATUS_AREA_MARGIN_H,
    STATUS_AREA_MARGIN_TOP, STATUS_LABEL_MAX_WIDTH_CHARS, TranscriptPanel,
};
#[cfg(feature = "sherpa")]
use super::{
    AUTO_BREAK_MIN_OPEN_MS_DEFAULT, AUTO_BREAK_MIN_OPEN_MS_MAX, AUTO_BREAK_MIN_OPEN_MS_MIN,
    AUTO_BREAK_MIN_SEGMENT_MS_DEFAULT, AUTO_BREAK_MIN_SEGMENT_MS_MAX,
    AUTO_BREAK_MIN_SEGMENT_MS_MIN, AUTO_BREAK_TAIL_MS_DEFAULT, AUTO_BREAK_TAIL_MS_MAX,
    AUTO_BREAK_TAIL_MS_MIN, DEFAULT_SHERPA_VAD_THRESHOLD, DISPLAY_MODE_FINAL_IDX,
    DISPLAY_MODE_LABELS, DISPLAY_MODE_LIVE_IDX, KEY_AUTO_BREAK_ENABLED, KEY_AUTO_BREAK_MIN_OPEN_MS,
    KEY_AUTO_BREAK_MIN_SEGMENT_MS, KEY_AUTO_BREAK_TAIL_MS, KEY_DISPLAY_MODE, KEY_SHERPA_MODEL,
    KEY_SHERPA_VAD_THRESHOLD, SHERPA_VAD_THRESHOLD_MAX, SHERPA_VAD_THRESHOLD_MIN,
    SHERPA_VAD_THRESHOLD_PAGE, SHERPA_VAD_THRESHOLD_STEP,
};
#[cfg(feature = "whisper")]
use super::{
    DEFAULT_SILENCE_THRESHOLD, KEY_MODEL, KEY_SILENCE_THRESHOLD, SILENCE_THRESHOLD_MAX,
    SILENCE_THRESHOLD_MIN, SILENCE_THRESHOLD_PAGE, SILENCE_THRESHOLD_STEP,
};

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
