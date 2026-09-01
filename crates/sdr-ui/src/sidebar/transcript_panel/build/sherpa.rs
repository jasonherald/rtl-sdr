//! Sherpa-only rows and wiring for the Transcript panel
//! (issue #819): the VAD-threshold slider, the Auto Break toggle +
//! timing sliders, the display-mode selector, the live caption
//! line, and the model/offline visibility policy that ties them
//! together. Split out of `build.rs` per the 500-NLOC file gate;
//! the whole module is `#[cfg(feature = "sherpa")]` (gated at the
//! `mod` declaration), mirroring the whisper/sherpa mutex.

use std::sync::Arc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;

use super::super::{
    AUTO_BREAK_MIN_OPEN_MS_DEFAULT, AUTO_BREAK_MIN_OPEN_MS_MAX, AUTO_BREAK_MIN_OPEN_MS_MIN,
    AUTO_BREAK_MIN_SEGMENT_MS_DEFAULT, AUTO_BREAK_MIN_SEGMENT_MS_MAX,
    AUTO_BREAK_MIN_SEGMENT_MS_MIN, AUTO_BREAK_TAIL_MS_DEFAULT, AUTO_BREAK_TAIL_MS_MAX,
    AUTO_BREAK_TAIL_MS_MIN, DEFAULT_SHERPA_VAD_THRESHOLD, DISPLAY_MODE_FINAL_IDX,
    DISPLAY_MODE_LABELS, DISPLAY_MODE_LIVE_IDX, KEY_AUTO_BREAK_ENABLED, KEY_AUTO_BREAK_MIN_OPEN_MS,
    KEY_AUTO_BREAK_MIN_SEGMENT_MS, KEY_AUTO_BREAK_TAIL_MS, KEY_DISPLAY_MODE,
    KEY_SHERPA_VAD_THRESHOLD, SHERPA_VAD_THRESHOLD_MAX, SHERPA_VAD_THRESHOLD_MIN,
    SHERPA_VAD_THRESHOLD_PAGE, SHERPA_VAD_THRESHOLD_STEP,
};
use super::ms_slider::{MsSliderSpec, build_persisted_ms_slider};

/// The sherpa-only widget bundle [`super::build_transcript_panel`]
/// threads into the panel literal. Grouped per the `ShareConfigRows`
/// precedent (#819).
pub(super) struct SherpaRows {
    pub(super) display_mode_row: adw::ComboRow,
    pub(super) vad_threshold_row: adw::SpinRow,
    pub(super) auto_break_row: adw::SwitchRow,
    pub(super) auto_break_min_open_row: adw::SpinRow,
    pub(super) auto_break_tail_row: adw::SpinRow,
    pub(super) auto_break_min_segment_row: adw::SpinRow,
    pub(super) live_line_label: gtk4::Label,
}

/// Build every sherpa-only row in the original display order (VAD →
/// Auto Break toggle → timing sliders → display mode), seed + wire
/// the offline/streaming visibility policy, and build + wire the
/// live caption line. Orchestrator over one helper per stage per
/// the 50-NLOC gate (#819, PR #880 Codacy precedent).
pub(super) fn build_sherpa_rows(
    group: &adw::PreferencesGroup,
    config: &Arc<ConfigManager>,
    model_row: &adw::ComboRow,
    saved_model_idx: u32,
) -> SherpaRows {
    // Attachment order == call order (each helper `group.add`s its
    // own rows), so the helpers run in the original display order:
    // VAD → Auto Break toggle → timing sliders → display mode. The
    // first carve draft called `build_auto_break_sliders` ahead of
    // the struct literal, attaching the sliders ABOVE the toggle
    // that controls them (`CodeRabbit` round 1 on PR #886).
    let vad_threshold_row = build_vad_threshold_row(group, config);
    let auto_break_row = build_auto_break_toggle(group, config);
    let (auto_break_min_open_row, auto_break_tail_row, auto_break_min_segment_row) =
        build_auto_break_sliders(group, config);
    let rows = SherpaRows {
        vad_threshold_row,
        auto_break_row,
        auto_break_min_open_row,
        auto_break_tail_row,
        auto_break_min_segment_row,
        display_mode_row: build_display_mode_row(group, config),
        live_line_label: build_live_line_label(),
    };
    seed_and_wire_visibility(model_row, &rows, saved_model_idx);
    wire_live_line(model_row, &rows);
    rows
}

/// The three Auto Break timing sliders, in display order. Split out
/// of [`build_sherpa_rows`] per the 50-NLOC gate (#819).
fn build_auto_break_sliders(
    group: &adw::PreferencesGroup,
    config: &Arc<ConfigManager>,
) -> (adw::SpinRow, adw::SpinRow, adw::SpinRow) {
    let auto_break_min_open_row = build_persisted_ms_slider(
        group,
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
    let auto_break_tail_row = build_persisted_ms_slider(
        group,
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
    let auto_break_min_segment_row = build_persisted_ms_slider(
        group,
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
    (
        auto_break_min_open_row,
        auto_break_tail_row,
        auto_break_min_segment_row,
    )
}

// --- VAD threshold slider (Sherpa + offline models only) ---
//
// Only visible when an offline model (Moonshine, Parakeet) is selected.
// The Silero VAD's default 0.5 threshold is too strict for noisy NFM
// scanner audio — this slider lets the user tune it per source.
// Visibility is toggled in the sherpa block below alongside display_mode_row.

/// Split out per the 50-NLOC gate (#819).
fn build_vad_threshold_row(
    group: &adw::PreferencesGroup,
    config: &Arc<ConfigManager>,
) -> adw::SpinRow {
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
}

/// Auto Break master toggle (squelch-edge segmentation). Split
/// out per the 50-NLOC gate (#819).
fn build_auto_break_toggle(
    group: &adw::PreferencesGroup,
    config: &Arc<ConfigManager>,
) -> adw::SwitchRow {
    let saved = config.read(|v| {
        v.get(KEY_AUTO_BREAK_ENABLED)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    });

    let row = adw::SwitchRow::builder()
        .title("Auto Break")
        .subtitle("Use the radio's squelch as the transcription boundary instead of VAD. NFM only.")
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
}

// --- Display mode selector (Sherpa only) ---
//
// Whisper builds never compile this in — Whisper does not emit
// `TranscriptionEvent::Partial`, so there's nothing to render in a
// "live line". Sherpa builds default to "Live captions" because
// streaming is the whole point; users can switch to "Final only"
// if the in-place updates are visually distracting.

/// Split out per the 50-NLOC gate (#819).
fn build_display_mode_row(
    group: &adw::PreferencesGroup,
    config: &Arc<ConfigManager>,
) -> adw::ComboRow {
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
}

// Toggle display_mode_row and vad_threshold_row visibility based on
// whether the selected model emits partial hypotheses:
//   - display_mode_row: visible for online models (supports_partials)
//   - vad_threshold_row: visible for offline models (!supports_partials)
// Models like Moonshine/Parakeet are offline — the Live/Final distinction
// is meaningless so display_mode_row is hidden; but they DO use Silero
// VAD so vad_threshold_row is shown. Zipformer is streaming so
// display_mode_row is shown and vad_threshold_row is hidden.
// Initial visibility is set here based on the currently-saved model index.
/// Split out per the 50-NLOC gate (#819).
fn seed_and_wire_visibility(model_row: &adw::ComboRow, rows: &SherpaRows, saved_model_idx: u32) {
    let initial_supports_partials = sdr_transcription::SherpaModel::ALL
        .get(saved_model_idx as usize)
        .copied()
        .is_some_and(sdr_transcription::SherpaModel::supports_partials);
    let initial_is_offline = !initial_supports_partials;
    let initial_auto_break_active = rows.auto_break_row.is_active();

    rows.display_mode_row.set_visible(initial_supports_partials);
    // VAD slider visible only when offline model AND Auto Break is OFF.
    // When Auto Break is ON, it replaces the VAD slider functionally so
    // showing both would confuse the user about which one is driving
    // segmentation.
    rows.vad_threshold_row
        .set_visible(initial_is_offline && !initial_auto_break_active);
    // Auto Break toggle visible only when an offline model is selected.
    // The additional NFM demod-mode gate is applied by window.rs's
    // DemodModeChanged handler — at widget-build time we don't yet
    // know the demod mode and assume NFM is the common case.
    rows.auto_break_row.set_visible(initial_is_offline);
    // Auto Break timing sliders are the mirror of the VAD slider:
    // visible when offline model AND Auto Break is ON. Together
    // with `vad_threshold_row` they form a mutex visible triplet
    // — exactly one of (VAD slider) or (Auto Break sliders) is
    // shown at any time for an offline model.
    let ab_sliders_visible = initial_is_offline && initial_auto_break_active;
    rows.auto_break_min_open_row.set_visible(ab_sliders_visible);
    rows.auto_break_tail_row.set_visible(ab_sliders_visible);
    rows.auto_break_min_segment_row
        .set_visible(ab_sliders_visible);

    wire_model_visibility(model_row, rows);
    wire_auto_break_mutex(rows);
}

/// Model-change half of the visibility policy. Split out per the
/// 50-NLOC gate (#819).
fn wire_model_visibility(model_row: &adw::ComboRow, rows: &SherpaRows) {
    let display_mode_row_for_visibility = rows.display_mode_row.clone();
    let vad_threshold_row_for_visibility = rows.vad_threshold_row.clone();
    let auto_break_row_for_model_change = rows.auto_break_row.clone();
    let ab_min_open_for_model_change = rows.auto_break_min_open_row.clone();
    let ab_tail_for_model_change = rows.auto_break_tail_row.clone();
    let ab_min_segment_for_model_change = rows.auto_break_min_segment_row.clone();
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
}

fn wire_auto_break_mutex(rows: &SherpaRows) {
    // Mutex: toggling Auto Break hides/shows the VAD threshold slider
    // AND the Auto Break timing sliders. Only applies when Auto Break
    // itself is currently visible (an offline model is selected). If
    // the row is hidden (streaming Zipformer), the mutex doesn't
    // apply because the VAD slider is already hidden by the
    // offline-model check and the AB sliders follow.
    let vad_threshold_row_for_mutex = rows.vad_threshold_row.clone();
    let auto_break_row_for_mutex = rows.auto_break_row.clone();
    let ab_min_open_for_mutex = rows.auto_break_min_open_row.clone();
    let ab_tail_for_mutex = rows.auto_break_tail_row.clone();
    let ab_min_segment_for_mutex = rows.auto_break_min_segment_row.clone();
    rows.auto_break_row.connect_active_notify(move |r| {
        if auto_break_row_for_mutex.is_visible() {
            let ab_on = r.is_active();
            vad_threshold_row_for_mutex.set_visible(!ab_on);
            ab_min_open_for_mutex.set_visible(ab_on);
            ab_tail_for_mutex.set_visible(ab_on);
            ab_min_segment_for_mutex.set_visible(ab_on);
        }
    });
}

/// Dimmed italic live caption line (initially hidden). Split out
/// per the 50-NLOC gate (#819).
fn build_live_line_label() -> gtk4::Label {
    // --- Live caption line (Sherpa only) ---
    //
    // Dimmed italic label that renders in-progress Sherpa partials.
    // Initially hidden; becomes visible once a Partial event arrives
    // and the current display mode is "Live captions". When display
    // mode is "Final only" the label stays hidden entirely.
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
    let attrs = gtk4::pango::AttrList::new();
    attrs.insert(gtk4::pango::AttrInt::new_style(gtk4::pango::Style::Italic));
    live_line_label.set_attributes(Some(&attrs));
    live_line_label
}

/// Live-line staleness wiring: clear + hide on a switch to
/// "Final only" and on every model change. Split out per the
/// 50-NLOC gate (#819).
fn wire_live_line(model_row: &adw::ComboRow, rows: &SherpaRows) {
    // Mid-session mode flip: if the user switches to "Final only" while a
    // partial is visible, clear and hide it immediately. Without this, a
    // stale live line would linger until the next commit or Clear press.
    // Persistence is handled by the first connect_selected_notify above;
    // GLib chains handlers, so both fire on every selection change.
    let live_line_for_mode = rows.live_line_label.clone();
    rows.display_mode_row.connect_selected_notify(move |r| {
        if r.selected() == DISPLAY_MODE_FINAL_IDX {
            live_line_for_mode.set_text("");
            live_line_for_mode.set_visible(false);
        }
    });
    // Always clear the live line on model change. The visibility-toggle
    // handler earlier hides display_mode_row when switching to a
    // non-partial-emitting model, but it can't see live_line_label
    // (which is built after that handler runs) so it leaves any stale
    // live-line content visible. Without this third chained handler, a
    // user who ran a Zipformer session and then switched to Moonshine
    // or Parakeet would see leftover italic text dangling under the
    // text view.
    let live_line_for_model_change = rows.live_line_label.clone();
    model_row.connect_selected_notify(move |_| {
        live_line_for_model_change.set_text("");
        live_line_for_model_change.set_visible(false);
    });
}
