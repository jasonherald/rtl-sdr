//! Transcript sidebar panel — displays live transcription results.

use gtk4::prelude::*;
use libadwaita as adw;

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

/// Horizontal inset for the status label / progress bar under the
/// model dropdown, matching the `AdwPreferencesGroup` gutter.
const STATUS_AREA_MARGIN_H: i32 = 12;
/// Vertical gap between the dropdown row and the status area.
const STATUS_AREA_MARGIN_TOP: i32 = 4;
/// Wrap width for status/error text — long per-backend messages
/// (#858) must wrap here instead of forcing the panel wider.
const STATUS_LABEL_MAX_WIDTH_CHARS: i32 = 48;

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
///
/// `Clone` is a cheap per-widget `GObject` refcount bump — same idiom as
/// `RadioPanel` — so session closures can capture one panel handle
/// instead of a dozen row clones.
#[derive(Clone)]
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
///
/// `switched_at` is the wall-clock instant the scanner emitted
/// `ScannerActiveChannelChanged` — captured at hop time and
/// passed through here. Render time can lag by seconds when the
/// transcription backend is busy, so using `chrono::Local::now()`
/// inside this helper would stamp markers with the wrong time on
/// busy passes. Per `CodeRabbit` round 1 on PR #558.
pub fn push_channel_marker(
    text_view: &gtk4::TextView,
    switched_at: chrono::DateTime<chrono::Local>,
    channel_name: &str,
) {
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

    let timestamp = switched_at.format("%H:%M:%S");
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

mod build;

pub use build::build_transcript_panel;
