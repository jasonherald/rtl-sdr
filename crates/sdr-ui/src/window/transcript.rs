//! Transcript panel wiring and transcription-session row state.

#[cfg(feature = "sherpa")]
use crate::sidebar::transcript_panel::DISPLAY_MODE_FINAL_IDX;
#[cfg(feature = "sherpa")]
use crate::viewer::plain_toast;

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::{AppState, Duration, Rc, RefCell, SidebarPanels, adw, glib, sidebar};

/// Re-enable every transcription settings row that gets locked during
/// an active session.
///
/// Single source of truth for the row-unlock side of the four
/// session-end paths in [`connect_transcript_panel`]:
///
/// 1. `TranscriptionEvent::Error` arm in the timeout closure
/// 2. `TryRecvError::Disconnected` arm in the timeout closure
/// 3. Synchronous `engine.start()` failure in `connect_active_notify`
/// 4. Normal stop (off branch of `connect_active_notify`)
///
/// Takes weak refs so paths 1 and 2 (which hold weak refs to avoid
/// keeping widgets alive past their UI lifetime) can call it directly.
/// Paths 3 and 4 hold strong refs and pass `&strong.downgrade()` —
/// the temporary lives through the function call.
///
/// Tolerant of any individual weak ref failing to upgrade (window close
/// race) — each row is checked independently so a partially-dropped UI
/// still recovers what it can.
#[allow(clippy::too_many_arguments)]
pub(super) fn unlock_transcription_session_rows(
    model_row: &glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "whisper")] silence_row: &glib::WeakRef<adw::SpinRow>,
    noise_gate_row: &glib::WeakRef<adw::SpinRow>,
    audio_enhancement_row: &glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "sherpa")] display_mode_row: &glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "sherpa")] vad_threshold_row: &glib::WeakRef<adw::SpinRow>,
    #[cfg(feature = "sherpa")] auto_break_row: &glib::WeakRef<adw::SwitchRow>,
    #[cfg(feature = "sherpa")] auto_break_min_open_row: &glib::WeakRef<adw::SpinRow>,
    #[cfg(feature = "sherpa")] auto_break_tail_row: &glib::WeakRef<adw::SpinRow>,
    #[cfg(feature = "sherpa")] auto_break_min_segment_row: &glib::WeakRef<adw::SpinRow>,
) {
    if let Some(row) = model_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "whisper")]
    if let Some(row) = silence_row.upgrade() {
        row.set_sensitive(true);
    }
    if let Some(row) = noise_gate_row.upgrade() {
        row.set_sensitive(true);
    }
    if let Some(row) = audio_enhancement_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "sherpa")]
    if let Some(row) = display_mode_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "sherpa")]
    if let Some(row) = vad_threshold_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "sherpa")]
    if let Some(row) = auto_break_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "sherpa")]
    if let Some(row) = auto_break_min_open_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "sherpa")]
    if let Some(row) = auto_break_tail_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "sherpa")]
    if let Some(row) = auto_break_min_segment_row.upgrade() {
        row.set_sensitive(true);
    }
}

/// Load saved transmitter ERP and receiver calibration offset
/// into the Radio panel's FSPL distance estimator rows, and wire
/// value-change handlers to persist any edits back to the config
/// (ticket #164). The distance display refresh wiring lives
/// inside `build_radio_panel` — this function is only about
/// config ↔ row synchronisation.
pub(super) fn connect_distance_estimator_persistence(
    panels: &SidebarPanels,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    use sidebar::radio_panel::{KEY_RADIO_DISTANCE_CALIBRATION_DB, KEY_RADIO_DISTANCE_ERP_WATTS};

    // Seed the rows with the saved values (clamped to the spin
    // rows' own adjustment bounds via their `set_value`). The
    // default spin row values were already applied at
    // `build_radio_panel` time, so `config.read` here only
    // overrides when a saved value exists.
    let saved_erp = config.read(|v| {
        v.get(KEY_RADIO_DISTANCE_ERP_WATTS)
            .and_then(serde_json::Value::as_f64)
    });
    let saved_cal = config.read(|v| {
        v.get(KEY_RADIO_DISTANCE_CALIBRATION_DB)
            .and_then(serde_json::Value::as_f64)
    });
    if let Some(erp) = saved_erp {
        panels.radio.erp_row.set_value(erp);
    }
    if let Some(cal) = saved_cal {
        panels.radio.calibration_row.set_value(cal);
    }

    // Persist-on-change. Uses `value_notify` (not the adjustment's
    // `value_changed`) to match the signal the in-panel distance
    // refresh handler is already listening to — both fire on the
    // same user edit.
    let config_erp = std::sync::Arc::clone(config);
    panels.radio.erp_row.connect_value_notify(move |row| {
        config_erp.write(|v| {
            v[KEY_RADIO_DISTANCE_ERP_WATTS] = serde_json::json!(row.value());
        });
    });
    let config_cal = std::sync::Arc::clone(config);
    panels
        .radio
        .calibration_row
        .connect_value_notify(move |row| {
            config_cal.write(|v| {
                v[KEY_RADIO_DISTANCE_CALIBRATION_DB] = serde_json::json!(row.value());
            });
        });
}

/// Connect transcript panel controls to DSP commands.
///
/// Returns the engine handle so it can be stopped on window close.
#[allow(clippy::too_many_lines)]
pub(super) fn connect_transcript_panel(
    transcript: &sidebar::transcript_panel::TranscriptPanel,
    state: &Rc<AppState>,
    #[cfg_attr(not(feature = "sherpa"), allow(unused_variables))] config: &std::sync::Arc<
        sdr_config::ConfigManager,
    >,
    #[cfg_attr(not(feature = "sherpa"), allow(unused_variables))]
    squelch_enabled_row: &adw::SwitchRow,
    #[cfg_attr(not(feature = "sherpa"), allow(unused_variables))] toast_overlay: &adw::ToastOverlay,
) -> Rc<RefCell<sdr_transcription::TranscriptionEngine>> {
    use sdr_transcription::{TranscriptionEngine, TranscriptionEvent};

    let engine: Rc<RefCell<TranscriptionEngine>> =
        Rc::new(RefCell::new(TranscriptionEngine::new()));

    let state_clone = Rc::clone(state);
    let engine_clone = Rc::clone(&engine);
    let status_label = transcript.status_label.clone();
    let progress_bar = transcript.progress_bar.clone();
    let text_view = transcript.text_view.clone();
    let model_row = transcript.model_row.clone();
    #[cfg(feature = "whisper")]
    let silence_row = transcript.silence_row.clone();
    let noise_gate_row = transcript.noise_gate_row.clone();
    let audio_enhancement_row = transcript.audio_enhancement_row.clone();
    // Weak refs used by the async event-loop closure to drive the same
    // teardown the synchronous error path does (see below) when the
    // backend fires TranscriptionEvent::Error mid-session. Weak so the
    // timeout closure doesn't keep widgets alive past their UI lifetime.
    let enable_row_weak = transcript.enable_row.downgrade();
    let model_row_weak = model_row.downgrade();
    #[cfg(feature = "whisper")]
    let silence_row_weak = silence_row.downgrade();
    let noise_gate_row_weak = noise_gate_row.downgrade();
    let audio_enhancement_row_weak = audio_enhancement_row.downgrade();
    #[cfg(feature = "sherpa")]
    let display_mode_row = transcript.display_mode_row.clone();
    #[cfg(feature = "sherpa")]
    let vad_threshold_row = transcript.vad_threshold_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_row = transcript.auto_break_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_min_open_row = transcript.auto_break_min_open_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_tail_row = transcript.auto_break_tail_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_min_segment_row = transcript.auto_break_min_segment_row.clone();
    #[cfg(feature = "sherpa")]
    let squelch_enabled_row_for_session = squelch_enabled_row.clone();
    #[cfg(feature = "sherpa")]
    let toast_overlay_for_session = toast_overlay.downgrade();
    #[cfg(feature = "sherpa")]
    let live_line_label = transcript.live_line_label.clone();
    #[cfg(feature = "sherpa")]
    let display_mode_row_weak = display_mode_row.downgrade();
    #[cfg(feature = "sherpa")]
    let vad_threshold_row_weak = vad_threshold_row.downgrade();
    #[cfg(feature = "sherpa")]
    let auto_break_row_weak = auto_break_row.downgrade();
    #[cfg(feature = "sherpa")]
    let auto_break_min_open_row_weak = auto_break_min_open_row.downgrade();
    #[cfg(feature = "sherpa")]
    let auto_break_tail_row_weak = auto_break_tail_row.downgrade();
    #[cfg(feature = "sherpa")]
    let auto_break_min_segment_row_weak = auto_break_min_segment_row.downgrade();
    #[cfg(feature = "sherpa")]
    let live_line_weak = live_line_label.downgrade();

    #[cfg(feature = "sherpa")]
    wire_sherpa_model_reload(transcript, &status_label, &progress_bar, config);

    transcript.enable_row.connect_active_notify(move |row| {
        if row.is_active() {
            // Read the selected model index once at the top of the
            // session-start branch; the Auto Break eligibility check
            // below needs it, and the BackendConfig construction
            // below reuses it.
            let model_idx = model_row.selected() as usize;

            // Auto Break is eligible ONLY when all three conditions
            // hold: (1) the toggle itself is on, (2) the current demod
            // mode is NFM, and (3) the selected sherpa model is offline
            // (Moonshine, Parakeet). The toggle is persisted, so
            // without this computed gate it would still report "on"
            // after a restart into WFM, or after the user switched to
            // streaming Zipformer and the row went invisible — either
            // of which would produce an unsupported session
            // (streaming Zipformer rejects AutoBreak at session start;
            // non-NFM modes never emit squelch edges so the state
            // machine sits in Idle forever). Compute the effective
            // value once here and use it for both the precondition
            // check and the BackendConfig assignment.
            #[cfg(feature = "sherpa")]
            let auto_break_enabled = {
                let selected_is_offline = sdr_transcription::SherpaModel::ALL
                    .get(model_idx)
                    .copied()
                    .is_some_and(|m| !m.supports_partials());
                auto_break_row.is_active()
                    && state_clone.demod_mode.get() == sdr_types::DemodMode::Nfm
                    && selected_is_offline
            };

            // Auto Break precondition: squelch must be enabled so the
            // radio produces the open/close transitions the state
            // machine needs for segmentation. Without squelch enabled,
            // the session would sit in Idle indefinitely producing
            // zero transcripts — silent failure mode. Block session
            // start with an actionable toast.
            #[cfg(feature = "sherpa")]
            if auto_break_enabled && !squelch_enabled_row_for_session.is_active() {
                let toast = plain_toast(
                    "Auto Break needs squelch enabled to detect transmission boundaries. \
                     Enable squelch in the radio panel, or turn off Auto Break to use VAD.",
                );
                if let Some(overlay) = toast_overlay_for_session.upgrade() {
                    overlay.add_toast(toast);
                }
                // Revert the enable toggle so the user can take action first.
                // The OFF branch of the handler is a safe no-op on an
                // inactive session (it just drops any backend channels).
                row.set_active(false);
                return;
            }

            // Lock model and tuning controls while transcription is active.
            model_row.set_sensitive(false);
            #[cfg(feature = "whisper")]
            silence_row.set_sensitive(false);
            noise_gate_row.set_sensitive(false);
            audio_enhancement_row.set_sensitive(false);
            // All settings lock during a session for mid-session fault
            // tolerance — walks back PR 4's earlier display_mode_row
            // exception. User stops, changes, starts.
            #[cfg(feature = "sherpa")]
            display_mode_row.set_sensitive(false);
            #[cfg(feature = "sherpa")]
            vad_threshold_row.set_sensitive(false);
            #[cfg(feature = "sherpa")]
            auto_break_row.set_sensitive(false);
            #[cfg(feature = "sherpa")]
            auto_break_min_open_row.set_sensitive(false);
            #[cfg(feature = "sherpa")]
            auto_break_tail_row.set_sensitive(false);
            #[cfg(feature = "sherpa")]
            auto_break_min_segment_row.set_sensitive(false);

            // Read tuning slider values.
            #[cfg(feature = "whisper")]
            #[allow(clippy::cast_possible_truncation)]
            let silence_threshold = silence_row.value() as f32;
            // Sherpa builds: silence_threshold is unused by SherpaBackend
            // (see build_recognizer_config doc comment). Pass a sentinel.
            #[cfg(feature = "sherpa")]
            let silence_threshold: f32 = 0.0;
            #[allow(clippy::cast_possible_truncation)]
            let noise_gate_ratio = noise_gate_row.value() as f32;

            // Build BackendConfig — Whisper and Sherpa are mutually exclusive
            // cargo features, so exactly one variant is compiled in.
            #[cfg(feature = "whisper")]
            let model = {
                let whisper_model = sdr_transcription::WhisperModel::ALL
                    .get(model_idx)
                    .copied()
                    .unwrap_or(sdr_transcription::WhisperModel::TinyEn);
                sdr_transcription::ModelChoice::Whisper(whisper_model)
            };
            #[cfg(feature = "sherpa")]
            let model = {
                let sherpa_model = sdr_transcription::SherpaModel::ALL
                    .get(model_idx)
                    .copied()
                    .unwrap_or(sdr_transcription::SherpaModel::StreamingZipformerEn);
                sdr_transcription::ModelChoice::Sherpa(sherpa_model)
            };

            #[cfg(feature = "sherpa")]
            #[allow(clippy::cast_possible_truncation)]
            let vad_threshold = vad_threshold_row.value() as f32;
            // Whisper builds compile the field but ignore it (no Silero VAD).
            #[cfg(feature = "whisper")]
            let vad_threshold: f32 = sdr_transcription::VAD_THRESHOLD_DEFAULT;

            #[cfg(feature = "sherpa")]
            let segmentation_mode = if auto_break_enabled {
                sdr_transcription::SegmentationMode::AutoBreak
            } else {
                sdr_transcription::SegmentationMode::Vad
            };
            #[cfg(feature = "whisper")]
            let segmentation_mode = sdr_transcription::SegmentationMode::Vad;

            // Auto Break timing parameters read from the session sliders.
            // Whisper builds hardcode the defaults (these fields are
            // never consumed because Whisper uses a different backend).
            #[cfg(feature = "sherpa")]
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let auto_break_min_open_ms = auto_break_min_open_row.value() as u32;
            #[cfg(feature = "sherpa")]
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let auto_break_tail_ms = auto_break_tail_row.value() as u32;
            #[cfg(feature = "sherpa")]
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let auto_break_min_segment_ms = auto_break_min_segment_row.value() as u32;
            #[cfg(feature = "whisper")]
            let auto_break_min_open_ms = sdr_transcription::AUTO_BREAK_MIN_OPEN_MS_DEFAULT;
            #[cfg(feature = "whisper")]
            let auto_break_tail_ms = sdr_transcription::AUTO_BREAK_TAIL_MS_DEFAULT;
            #[cfg(feature = "whisper")]
            let auto_break_min_segment_ms =
                sdr_transcription::AUTO_BREAK_MIN_SEGMENT_MS_DEFAULT;

            // Audio enhancement mode from the transcript panel
            // combo row. The row's persisted index is captured at
            // session start (not subscribed to — matches the
            // existing "lock during session" behavior for all
            // transcription settings).
            let audio_enhancement = match audio_enhancement_row.selected() {
                sidebar::transcript_panel::AUDIO_ENHANCEMENT_BROADBAND_IDX => {
                    sdr_transcription::denoise::AudioEnhancement::Broadband
                }
                sidebar::transcript_panel::AUDIO_ENHANCEMENT_OFF_IDX => {
                    sdr_transcription::denoise::AudioEnhancement::Off
                }
                _ => sdr_transcription::denoise::AudioEnhancement::VoiceBand,
            };

            let config = sdr_transcription::BackendConfig {
                model,
                silence_threshold,
                noise_gate_ratio,
                vad_threshold,
                segmentation_mode,
                auto_break_min_open_ms,
                auto_break_tail_ms,
                auto_break_min_segment_ms,
                audio_enhancement,
            };

            // Scope the borrow so it's dropped before any potential re-entry
            // from row.set_active(false) on error.
            let start_result = engine_clone.borrow_mut().start(config);
            match start_result {
                Ok(event_rx) => {
                    if let Some(audio_tx) = engine_clone.borrow().audio_sender() {
                        state_clone
                            .send_dsp(crate::messages::UiToDsp::EnableTranscription(audio_tx));
                    }
                    // Drop any channel-marker buffered while
                    // transcription was off — the first text
                    // event after re-enable should attribute to
                    // the *next* hop, not whichever channel the
                    // scanner happened to land on during the
                    // off period. Per CodeRabbit round 1 on PR
                    // #558.
                    *state_clone.pending_channel_marker.borrow_mut() = None;

                    status_label.set_text("Starting...");
                    status_label.set_visible(true);

                    // Weak refs for the entire timeout source — see the
                    // weak-ref decl block at the top of connect_transcript_panel
                    // for the rationale (don't keep widgets alive past their
                    // UI lifetime through the glib timeout source).
                    let status_weak = status_label.downgrade();
                    let progress_weak = progress_bar.downgrade();
                    let tv_weak = text_view.downgrade();
                    let enable_row_weak = enable_row_weak.clone();
                    let model_row_weak = model_row_weak.clone();
                    #[cfg(feature = "whisper")]
                    let silence_row_weak = silence_row_weak.clone();
                    let noise_gate_row_weak = noise_gate_row_weak.clone();
                    let audio_enhancement_row_weak = audio_enhancement_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let display_mode_row_weak = display_mode_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let vad_threshold_row_weak = vad_threshold_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let auto_break_row_weak = auto_break_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let auto_break_min_open_row_weak = auto_break_min_open_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let auto_break_tail_row_weak = auto_break_tail_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let auto_break_min_segment_row_weak =
                        auto_break_min_segment_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let live_line_weak = live_line_weak.clone();
                    // State handle for the lazy channel-marker
                    // emission (#517) — the closure consumes
                    // `state_clone.pending_channel_marker` from
                    // the `TranscriptionEvent::Text` arm below.
                    let state_for_marker = Rc::clone(&state_clone);

                    glib::timeout_add_local(Duration::from_millis(100), move || {
                        // Upgrade once per tick. If any widget has been
                        // dropped (e.g. window closed), stop the timeout
                        // immediately so we don't resurrect dead UI.
                        let Some(status) = status_weak.upgrade() else {
                            return glib::ControlFlow::Break;
                        };
                        let Some(progress) = progress_weak.upgrade() else {
                            return glib::ControlFlow::Break;
                        };
                        let Some(tv) = tv_weak.upgrade() else {
                            return glib::ControlFlow::Break;
                        };

                        loop {
                            match event_rx.try_recv() {
                                Ok(event) => match event {
                                    TranscriptionEvent::Downloading { progress_pct } => {
                                        status.set_text(&format!(
                                            "Downloading model ({progress_pct}%)..."
                                        ));
                                        status.set_visible(true);
                                        progress.set_fraction(f64::from(progress_pct) / 100.0);
                                        progress.set_visible(true);
                                    }
                                    TranscriptionEvent::Ready => {
                                        status.set_text("Listening...");
                                        status.set_css_classes(&["success"]);
                                        progress.set_visible(false);
                                    }
                                    TranscriptionEvent::Partial { text } => {
                                        #[cfg(feature = "sherpa")]
                                        {
                                            // Belt-and-suspenders: only paint
                                            // the live line if (a) the current
                                            // model actually supports partials
                                            // and (b) display mode is Live.
                                            //
                                            // (a) defends against a future bug
                                            // where an offline model accidentally
                                            // emits a Partial event — today the
                                            // offline session loop never does,
                                            // but the UI shouldn't trust that.
                                            // Without this check, italics would
                                            // appear on Moonshine/Parakeet on
                                            // any spurious Partial.
                                            //
                                            // (b) honors the user's display-mode
                                            // preference for partial-emitting
                                            // models. Re-read on every event so
                                            // mid-session toggle takes effect.
                                            let model_supports_partials = model_row_weak
                                                .upgrade()
                                                .is_some_and(|row| {
                                                    let idx = row.selected() as usize;
                                                    sdr_transcription::SherpaModel::ALL
                                                        .get(idx)
                                                        .copied()
                                                        .is_some_and(
                                                            sdr_transcription::SherpaModel::supports_partials,
                                                        )
                                                });
                                            let show_live = model_supports_partials
                                                && display_mode_row_weak.upgrade().is_some_and(
                                                    |row| row.selected() != DISPLAY_MODE_FINAL_IDX,
                                                );
                                            if show_live
                                                && let Some(label) = live_line_weak.upgrade()
                                            {
                                                label.set_text(&text);
                                                label.set_visible(true);
                                            }
                                            // Privacy: never log the raw text.
                                            tracing::debug!(
                                                target: "transcription",
                                                partial_chars = text.chars().count(),
                                                "sherpa partial received"
                                            );
                                        }
                                        #[cfg(not(feature = "sherpa"))]
                                        {
                                            // Whisper never emits Partial, but
                                            // the enum variant is compiled in.
                                            // Defensive no-op.
                                            let _ = text;
                                        }
                                    }
                                    TranscriptionEvent::Text { timestamp, text } => {
                                        // Drain the pending channel-marker
                                        // (#517) BEFORE inserting the
                                        // transcribed text — the marker
                                        // belongs ABOVE the text it
                                        // precedes. Lazy emission means
                                        // markers only land when there's
                                        // actual audio to attribute, so
                                        // a quiet channel never produces
                                        // a divider.
                                        if let Some((switched_at, channel_name)) =
                                            state_for_marker
                                                .pending_channel_marker
                                                .borrow_mut()
                                                .take()
                                        {
                                            sidebar::transcript_panel::push_channel_marker(
                                                &tv,
                                                switched_at,
                                                &channel_name,
                                            );
                                        }
                                        let buf = tv.buffer();
                                        let mut end = buf.end_iter();
                                        buf.insert(&mut end, &format!("[{timestamp}] {text}\n"));
                                        let mark = buf.create_mark(None, &buf.end_iter(), false);
                                        tv.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
                                        buf.delete_mark(&mark);

                                        // An utterance committed — the live
                                        // line is now stale. Clear and hide
                                        // it so the next Partial starts fresh.
                                        #[cfg(feature = "sherpa")]
                                        if let Some(label) = live_line_weak.upgrade() {
                                            label.set_text("");
                                            label.set_visible(false);
                                        }
                                    }
                                    TranscriptionEvent::Error(msg) => {
                                        // Fatal — backend has exited.
                                        // Mirror the synchronous start()
                                        // failure teardown so the UI
                                        // isn't left locked.
                                        unlock_transcription_session_rows(
                                            &model_row_weak,
                                            #[cfg(feature = "whisper")]
                                            &silence_row_weak,
                                            &noise_gate_row_weak,
                                            &audio_enhancement_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &display_mode_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &vad_threshold_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &auto_break_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &auto_break_min_open_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &auto_break_tail_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &auto_break_min_segment_row_weak,
                                        );
                                        if let Some(enable) = enable_row_weak.upgrade() {
                                            enable.set_active(false);
                                        }
                                        status.set_text(&msg);
                                        status.set_css_classes(&["error"]);
                                        status.set_visible(true);
                                        progress.set_visible(false);
                                        // Clear any stale partial so it
                                        // doesn't linger into the next session.
                                        #[cfg(feature = "sherpa")]
                                        if let Some(label) = live_line_weak.upgrade() {
                                            label.set_text("");
                                            label.set_visible(false);
                                        }
                                        return glib::ControlFlow::Break;
                                    }
                                },
                                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    // Distinguish a normal user-initiated stop
                                    // from a spontaneous backend death:
                                    //
                                    // - User stop: the off branch of
                                    //   enable_row.connect_active_notify already
                                    //   ran (it dropped audio_tx, which is what
                                    //   caused the worker to exit and drop
                                    //   event_tx, which we're now seeing as
                                    //   Disconnected). The toggle is already
                                    //   inactive and all the rows have been
                                    //   re-enabled. Nothing to do here — the
                                    //   off branch did the cleanup. Without
                                    //   this check the disconnect arm overwrote
                                    //   the off branch's clean state with a
                                    //   spurious "Transcription stopped
                                    //   unexpectedly" error message on every
                                    //   normal stop.
                                    //
                                    // - Spontaneous death: the worker dropped
                                    //   event_tx without the user clicking
                                    //   anything. The toggle is still active.
                                    //   Mirror the Error arm's teardown so the
                                    //   UI doesn't strand the user with locked
                                    //   controls and a stale "Listening..."
                                    //   status.
                                    let was_user_stop =
                                        enable_row_weak.upgrade().is_none_or(|e| !e.is_active());

                                    if was_user_stop {
                                        tracing::debug!(
                                            "transcription event channel closed (user stop)"
                                        );
                                        return glib::ControlFlow::Break;
                                    }

                                    tracing::warn!(
                                        "transcription event channel disconnected unexpectedly"
                                    );
                                    unlock_transcription_session_rows(
                                        &model_row_weak,
                                        #[cfg(feature = "whisper")]
                                        &silence_row_weak,
                                        &noise_gate_row_weak,
                                        &audio_enhancement_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &display_mode_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &vad_threshold_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &auto_break_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &auto_break_min_open_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &auto_break_tail_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &auto_break_min_segment_row_weak,
                                    );
                                    if let Some(enable) = enable_row_weak.upgrade() {
                                        enable.set_active(false);
                                    }
                                    status.set_text("Transcription stopped unexpectedly");
                                    status.set_css_classes(&["error"]);
                                    status.set_visible(true);
                                    progress.set_visible(false);
                                    #[cfg(feature = "sherpa")]
                                    if let Some(label) = live_line_weak.upgrade() {
                                        label.set_text("");
                                        label.set_visible(false);
                                    }
                                    return glib::ControlFlow::Break;
                                }
                            }
                        }
                        glib::ControlFlow::Continue
                    });
                }
                Err(e) => {
                    tracing::warn!("failed to start transcription: {e}");
                    unlock_transcription_session_rows(
                        &model_row.downgrade(),
                        #[cfg(feature = "whisper")]
                        &silence_row.downgrade(),
                        &noise_gate_row.downgrade(),
                        &audio_enhancement_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &display_mode_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &vad_threshold_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &auto_break_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &auto_break_min_open_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &auto_break_tail_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &auto_break_min_segment_row.downgrade(),
                    );
                    // Reset the toggle FIRST (the else branch clears
                    // status_label as part of its normal teardown), then
                    // set the error text so the user actually sees it.
                    // Otherwise the failure is silent — only in stderr.
                    row.set_active(false);
                    status_label.set_text(&e.to_string());
                    status_label.set_css_classes(&["error"]);
                    status_label.set_visible(true);
                    progress_bar.set_visible(false);
                }
            }
        } else {
            unlock_transcription_session_rows(
                &model_row.downgrade(),
                #[cfg(feature = "whisper")]
                &silence_row.downgrade(),
                &noise_gate_row.downgrade(),
                &audio_enhancement_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &display_mode_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &vad_threshold_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &auto_break_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &auto_break_min_open_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &auto_break_tail_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &auto_break_min_segment_row.downgrade(),
            );
            state_clone.send_dsp(crate::messages::UiToDsp::DisableTranscription);
            // Drop any pending channel-marker so a stray scanner
            // hop that landed during the live session doesn't
            // poison the next enable's first text event. Per
            // CodeRabbit round 1 on PR #558.
            *state_clone.pending_channel_marker.borrow_mut() = None;
            engine_clone.borrow_mut().shutdown_nonblocking();
            status_label.set_text("");
            status_label.set_visible(false);
            progress_bar.set_visible(false);
            // Clear any stale partial on stop so the previous session's
            // last in-progress text doesn't linger on screen.
            #[cfg(feature = "sherpa")]
            {
                live_line_label.set_text("");
                live_line_label.set_visible(false);
            }
        }
    });

    engine
}

/// Widget handles for an in-flight sherpa model reload: the status
/// area plus weak refs to the two rows that get locked while the
/// swap runs. Weak upgrades no-op when the window is closing.
#[cfg(feature = "sherpa")]
struct ReloadUi {
    status: gtk4::Label,
    progress: gtk4::ProgressBar,
    model_row: glib::WeakRef<adw::ComboRow>,
    enable_row: glib::WeakRef<adw::SwitchRow>,
}

/// Sherpa model-selector reload wiring: on selection change, disable
/// the model + enable rows, kick `reload_sherpa_host`, and drain its
/// `InitEvent`s on a 100 ms tick. `KEY_SHERPA_MODEL` persists only
/// after `Ready` so a failed swap can't wedge the next startup.
/// Split out per the 50-NLOC gate (#817).
#[cfg(feature = "sherpa")]
fn wire_sherpa_model_reload(
    transcript: &sidebar::transcript_panel::TranscriptPanel,
    status_label: &gtk4::Label,
    progress_bar: &gtk4::ProgressBar,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    let status_label_reload = status_label.clone();
    let progress_bar_reload = progress_bar.clone();
    let enable_row_reload = transcript.enable_row.clone();
    // Config handle for the deferred-persistence path. We write
    // KEY_SHERPA_MODEL only after InitEvent::Ready fires so a
    // failed recognizer swap can't leave a broken model idx in
    // config that would wedge next startup's init_sherpa_host.
    let config_for_reload_persist = std::sync::Arc::clone(config);
    transcript.model_row.connect_selected_notify(move |row| {
        let idx = row.selected() as usize;
        let Some(new_model) = sdr_transcription::SherpaModel::ALL.get(idx).copied() else {
            return;
        };

        tracing::info!(?new_model, "user changed model — triggering runtime reload");

        // Disable BOTH rows while the reload is in flight:
        // - model_row so the user can't queue up multiple reloads
        //   via rapid switching
        // - enable_row so the user can't start/stop transcription
        //   on top of an in-flight recognizer swap. Without this,
        //   the stop-path teardown would re-enable model_row before
        //   the reload finishes, reopening the queued-reload window
        //   this block is closing.
        // Both are re-enabled from the timeout closure on Ready /
        // Failed / channel disconnect.
        row.set_sensitive(false);
        enable_row_reload.set_sensitive(false);
        let model_row_reload_weak = row.downgrade();
        let enable_row_reload_weak = enable_row_reload.downgrade();

        begin_model_reload_ui(
            &status_label_reload,
            &progress_bar_reload,
            new_model.label(),
        );

        let event_rx = sdr_transcription::reload_sherpa_host(new_model);
        arm_reload_poll_tick(
            &status_label_reload,
            &progress_bar_reload,
            model_row_reload_weak,
            enable_row_reload_weak,
            event_rx,
            new_model.label().to_owned(),
            std::sync::Arc::clone(&config_for_reload_persist),
            idx,
        );
    });
}

/// Arm the 100 ms poll tick that drains a reload's `InitEvent`s.
/// Self-cancels via `Break` when the status widgets are gone (window
/// closing) or on any terminal event.
#[cfg(feature = "sherpa")]
#[allow(clippy::too_many_arguments)]
fn arm_reload_poll_tick(
    status_label: &gtk4::Label,
    progress_bar: &gtk4::ProgressBar,
    model_row_reload_weak: glib::WeakRef<adw::ComboRow>,
    enable_row_reload_weak: glib::WeakRef<adw::SwitchRow>,
    event_rx: std::sync::mpsc::Receiver<sdr_transcription::InitEvent>,
    initial_component: String,
    config_for_this_reload: std::sync::Arc<sdr_config::ConfigManager>,
    persist_idx: usize,
) {
    let status_weak = status_label.downgrade();
    let progress_weak = progress_bar.downgrade();
    let mut current_component = initial_component;

    // Drain progress events on the main thread via a periodic
    // timeout. The Arc + idx captures are the deferred-persistence
    // path — written to config on Ready, dropped silently on
    // Failed/Disconnected.
    glib::timeout_add_local(Duration::from_millis(100), move || {
        // Widgets gone (window closing) → the model row is gone
        // too, so no need to re-enable it.
        let (Some(status), Some(progress)) = (status_weak.upgrade(), progress_weak.upgrade())
        else {
            return glib::ControlFlow::Break;
        };
        let ui = ReloadUi {
            status,
            progress,
            model_row: model_row_reload_weak.clone(),
            enable_row: enable_row_reload_weak.clone(),
        };
        if let Some(flow) = drain_sherpa_reload_events(
            &event_rx,
            &ui,
            &mut current_component,
            &config_for_this_reload,
            persist_idx,
        ) {
            return flow;
        }
        glib::ControlFlow::Continue
    });
}

/// Show the reload status area in its initial "Reloading…" state.
#[cfg(feature = "sherpa")]
fn begin_model_reload_ui(status: &gtk4::Label, progress: &gtk4::ProgressBar, model_label: &str) {
    status.set_text(&format!("Reloading {model_label}..."));
    status.set_css_classes(&["dim-label"]);
    status.set_visible(true);
    progress.set_fraction(0.0);
    progress.set_visible(true);
}

/// Drain pending `InitEvent`s for an in-flight model reload. Returns
/// `Some(Break)` on a terminal event (Ready / Failed / worker
/// disconnect) and `None` when the queue is drained and polling
/// should continue. Split out per the 50-NLOC gate (#817).
#[cfg(feature = "sherpa")]
#[allow(clippy::too_many_arguments)]
fn drain_sherpa_reload_events(
    event_rx: &std::sync::mpsc::Receiver<sdr_transcription::InitEvent>,
    ui: &ReloadUi,
    current_component: &mut String,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    persist_idx: usize,
) -> Option<glib::ControlFlow> {
    loop {
        match event_rx.try_recv() {
            Ok(sdr_transcription::InitEvent::DownloadStart { component }) => {
                component.clone_into(current_component);
                ui.status.set_text(&format!("Downloading {component}..."));
                ui.progress.set_fraction(0.0);
            }
            Ok(sdr_transcription::InitEvent::DownloadProgress { pct }) => {
                ui.status
                    .set_text(&format!("Downloading {current_component}... {pct}%"));
                ui.progress.set_fraction(f64::from(pct) / 100.0);
            }
            Ok(sdr_transcription::InitEvent::Extracting { component }) => {
                component.clone_into(current_component);
                ui.status.set_text(&format!("Extracting {component}..."));
            }
            Ok(sdr_transcription::InitEvent::CreatingRecognizer) => {
                ui.status.set_text("Creating recognizer...");
                ui.progress.set_visible(false);
            }
            Ok(sdr_transcription::InitEvent::Ready) => {
                finish_reload_success(ui, config, persist_idx);
                return Some(glib::ControlFlow::Break);
            }
            Ok(sdr_transcription::InitEvent::Failed { message }) => {
                tracing::warn!(%message, "sherpa host reload failed");
                show_reload_failure(ui, &format!("Reload failed: {message}"));
                return Some(glib::ControlFlow::Break);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Worker dropped its sender without sending Ready
                // or Failed — unusual but don't strand the UI in
                // a "Reloading..." state. Surface the disconnect
                // as an error and re-enable the controls so the
                // user can try a different model.
                tracing::warn!("sherpa host reload event channel disconnected unexpectedly");
                show_reload_failure(ui, "Reload failed: recognizer worker disconnected");
                return Some(glib::ControlFlow::Break);
            }
        }
    }
    None
}

/// `Ready` arm of a model reload: clear the status area, re-enable
/// the rows, and persist the new selection. Persistence is deferred
/// to here so a failed swap can't leave a broken model idx in config
/// that would wedge the next startup's `init_sherpa_host`.
#[cfg(feature = "sherpa")]
fn finish_reload_success(
    ui: &ReloadUi,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    persist_idx: usize,
) {
    tracing::info!("sherpa host reload complete");
    ui.status.set_text("");
    ui.status.set_visible(false);
    ui.progress.set_visible(false);
    reenable_reload_rows(ui);
    config.write(|v| {
        v[crate::sidebar::transcript_panel::KEY_SHERPA_MODEL] = serde_json::json!(persist_idx);
    });
}

/// Terminal-failure UI for a model reload: error text on the status
/// label, progress hidden, and the model/enable rows re-enabled so
/// the user can try a different model.
#[cfg(feature = "sherpa")]
fn show_reload_failure(ui: &ReloadUi, msg: &str) {
    ui.status.set_text(msg);
    ui.status.set_css_classes(&["error"]);
    ui.status.set_visible(true);
    ui.progress.set_visible(false);
    reenable_reload_rows(ui);
}

/// Re-enable the model + enable rows after a reload finishes (Ready,
/// Failed, or worker disconnect). Weak upgrades no-op when the window
/// is closing.
#[cfg(feature = "sherpa")]
fn reenable_reload_rows(ui: &ReloadUi) {
    if let Some(model_row) = ui.model_row.upgrade() {
        model_row.set_sensitive(true);
    }
    if let Some(enable_row) = ui.enable_row.upgrade() {
        enable_row.set_sensitive(true);
    }
}
