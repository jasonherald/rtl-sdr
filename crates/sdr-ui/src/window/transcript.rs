//! Transcript panel wiring and transcription-session row state.

#[cfg(feature = "sherpa")]
use crate::sidebar::transcript_panel::DISPLAY_MODE_FINAL_IDX;
#[cfg(feature = "sherpa")]
use crate::viewer::plain_toast;

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::{AppState, Duration, Rc, RefCell, adw, glib, sidebar};

/// Weak handles to every transcription settings row that gets locked
/// during an active session. Single source of truth for the
/// row-unlock side of the four session-end paths in
/// [`connect_transcript_panel`]: the `Error` arm, the unexpected
/// channel disconnect, the start-failure path, and the user stop.
#[allow(
    clippy::struct_field_names,
    reason = "fields deliberately mirror the panel widget names they weakly reference"
)]
pub(super) struct SessionRowWeaks {
    model_row: glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "whisper")]
    silence_row: glib::WeakRef<adw::SpinRow>,
    noise_gate_row: glib::WeakRef<adw::SpinRow>,
    audio_enhancement_row: glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "sherpa")]
    display_mode_row: glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "sherpa")]
    vad_threshold_row: glib::WeakRef<adw::SpinRow>,
    #[cfg(feature = "sherpa")]
    auto_break_row: glib::WeakRef<adw::SwitchRow>,
    #[cfg(feature = "sherpa")]
    auto_break_min_open_row: glib::WeakRef<adw::SpinRow>,
    #[cfg(feature = "sherpa")]
    auto_break_tail_row: glib::WeakRef<adw::SpinRow>,
    #[cfg(feature = "sherpa")]
    auto_break_min_segment_row: glib::WeakRef<adw::SpinRow>,
}

impl SessionRowWeaks {
    pub(super) fn from_panel(t: &sidebar::transcript_panel::TranscriptPanel) -> Self {
        Self {
            model_row: t.model_row.downgrade(),
            #[cfg(feature = "whisper")]
            silence_row: t.silence_row.downgrade(),
            noise_gate_row: t.noise_gate_row.downgrade(),
            audio_enhancement_row: t.audio_enhancement_row.downgrade(),
            #[cfg(feature = "sherpa")]
            display_mode_row: t.display_mode_row.downgrade(),
            #[cfg(feature = "sherpa")]
            vad_threshold_row: t.vad_threshold_row.downgrade(),
            #[cfg(feature = "sherpa")]
            auto_break_row: t.auto_break_row.downgrade(),
            #[cfg(feature = "sherpa")]
            auto_break_min_open_row: t.auto_break_min_open_row.downgrade(),
            #[cfg(feature = "sherpa")]
            auto_break_tail_row: t.auto_break_tail_row.downgrade(),
            #[cfg(feature = "sherpa")]
            auto_break_min_segment_row: t.auto_break_min_segment_row.downgrade(),
        }
    }

    /// Re-enable every locked row. Weak upgrades no-op for widgets
    /// the window teardown already dropped.
    pub(super) fn unlock(&self) {
        for row in self.spin_rows() {
            if let Some(row) = row.upgrade() {
                row.set_sensitive(true);
            }
        }
        for row in self.combo_rows() {
            if let Some(row) = row.upgrade() {
                row.set_sensitive(true);
            }
        }
        #[cfg(feature = "sherpa")]
        if let Some(row) = self.auto_break_row.upgrade() {
            row.set_sensitive(true);
        }
    }

    fn spin_rows(&self) -> Vec<&glib::WeakRef<adw::SpinRow>> {
        vec![
            #[cfg(feature = "whisper")]
            &self.silence_row,
            &self.noise_gate_row,
            #[cfg(feature = "sherpa")]
            &self.vad_threshold_row,
            #[cfg(feature = "sherpa")]
            &self.auto_break_min_open_row,
            #[cfg(feature = "sherpa")]
            &self.auto_break_tail_row,
            #[cfg(feature = "sherpa")]
            &self.auto_break_min_segment_row,
        ]
    }

    fn combo_rows(&self) -> Vec<&glib::WeakRef<adw::ComboRow>> {
        vec![
            &self.model_row,
            &self.audio_enhancement_row,
            #[cfg(feature = "sherpa")]
            &self.display_mode_row,
        ]
    }
}

/// Everything a transcription session's start/stop paths need,
/// captured once by the enable-row handler. The panel handle carries
/// the status area, transcript log, and every settings row; the
/// session-rows set is the lock/unlock surface.
struct SessionDeps {
    engine: Rc<RefCell<sdr_transcription::TranscriptionEngine>>,
    /// Liveness token of the CURRENT session's 100 ms event tick.
    /// `start` installs a fresh `Cell(true)`; `stop` takes it and
    /// flips it false. A stale tick from a previous session sees
    /// `false` on its next fire and breaks immediately — without
    /// this, a detached worker's late `Disconnected` could read the
    /// NEW session's `enable_row` and tear the new session down as a
    /// spontaneous death. Per CR round 1 on PR #844.
    session_alive: Rc<RefCell<Option<Rc<std::cell::Cell<bool>>>>>,
    state: Rc<AppState>,
    panel: sidebar::transcript_panel::TranscriptPanel,
    session_rows: Rc<SessionRowWeaks>,
    #[cfg(feature = "sherpa")]
    squelch_enabled_row: adw::SwitchRow,
    #[cfg(feature = "sherpa")]
    toast_overlay: glib::WeakRef<adw::ToastOverlay>,
}

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
    let engine = Rc::new(RefCell::new(sdr_transcription::TranscriptionEngine::new()));

    #[cfg(feature = "sherpa")]
    wire_sherpa_model_reload(
        transcript,
        &transcript.status_label,
        &transcript.progress_bar,
        config,
    );

    let deps = SessionDeps {
        engine: Rc::clone(&engine),
        session_alive: Rc::new(RefCell::new(None)),
        state: Rc::clone(state),
        panel: transcript.clone(),
        session_rows: Rc::new(SessionRowWeaks::from_panel(transcript)),
        #[cfg(feature = "sherpa")]
        squelch_enabled_row: squelch_enabled_row.clone(),
        #[cfg(feature = "sherpa")]
        toast_overlay: toast_overlay.downgrade(),
    };
    transcript.enable_row.connect_active_notify(move |row| {
        if row.is_active() {
            start_transcription_session(&deps, row);
        } else {
            stop_transcription_session(&deps);
        }
    });

    engine
}

/// Session-start half of the enable toggle: eligibility guards, row
/// locking, backend config, and the engine start with its event loop.
fn start_transcription_session(deps: &SessionDeps, row: &adw::SwitchRow) {
    // Read the selected model index once at the top of the
    // session-start branch; the Auto Break eligibility check needs
    // it, and the BackendConfig construction reuses it.
    let model_idx = deps.panel.model_row.selected() as usize;

    #[cfg(feature = "sherpa")]
    let auto_break_enabled = auto_break_eligible(deps, model_idx);
    #[cfg(feature = "whisper")]
    let auto_break_enabled = false;

    // Auto Break precondition: squelch must be enabled so the
    // radio produces the open/close transitions the state
    // machine needs for segmentation. Without squelch enabled,
    // the session would sit in Idle indefinitely producing
    // zero transcripts — silent failure mode. Block session
    // start with an actionable toast.
    #[cfg(feature = "sherpa")]
    if auto_break_enabled && !deps.squelch_enabled_row.is_active() {
        let toast = plain_toast(
            "Auto Break needs squelch enabled to detect transmission boundaries. \
             Enable squelch in the radio panel, or turn off Auto Break to use VAD.",
        );
        if let Some(overlay) = deps.toast_overlay.upgrade() {
            overlay.add_toast(toast);
        }
        // Revert the enable toggle so the user can take action first.
        // The OFF branch of the handler is a safe no-op on an
        // inactive session (it just drops any backend channels).
        row.set_active(false);
        return;
    }

    // Lock model and tuning controls while transcription is active.
    // All settings lock during a session for mid-session fault
    // tolerance — walks back PR 4's earlier display_mode_row
    // exception. User stops, changes, starts.
    lock_session_rows(&deps.panel);

    let config = build_backend_config(&deps.panel, model_idx, auto_break_enabled);

    // Scope the borrow so it's dropped before any potential re-entry
    // from row.set_active(false) on error.
    let start_result = deps.engine.borrow_mut().start(config);
    match start_result {
        Ok(event_rx) => begin_session_event_loop(deps, event_rx),
        Err(e) => {
            tracing::warn!("failed to start transcription: {e}");
            deps.session_rows.unlock();
            // Reset the toggle FIRST (the off branch clears
            // status_label as part of its normal teardown), then
            // set the error text so the user actually sees it.
            // Otherwise the failure is silent — only in stderr.
            row.set_active(false);
            deps.panel.status_label.set_text(&e.to_string());
            deps.panel.status_label.set_css_classes(&["error"]);
            deps.panel.status_label.set_visible(true);
            deps.panel.progress_bar.set_visible(false);
        }
    }
}

/// Auto Break is eligible ONLY when all three conditions hold:
/// (1) the toggle itself is on, (2) the current demod mode is NFM,
/// and (3) the selected sherpa model is offline (Moonshine,
/// Parakeet). The toggle is persisted, so without this computed gate
/// it would still report "on" after a restart into WFM, or after the
/// user switched to streaming Zipformer and the row went invisible —
/// either of which would produce an unsupported session (streaming
/// Zipformer rejects AutoBreak at session start; non-NFM modes never
/// emit squelch edges so the state machine sits in Idle forever).
#[cfg(feature = "sherpa")]
fn auto_break_eligible(deps: &SessionDeps, model_idx: usize) -> bool {
    let selected_is_offline = sdr_transcription::SherpaModel::ALL
        .get(model_idx)
        .copied()
        .is_some_and(|m| !m.supports_partials());
    deps.panel.auto_break_row.is_active()
        && deps.state.demod_mode.get() == sdr_types::DemodMode::Nfm
        && selected_is_offline
}

/// Hook the freshly-started backend into the DSP audio tap and arm
/// the 100 ms event-drain tick.
fn begin_session_event_loop(
    deps: &SessionDeps,
    event_rx: std::sync::mpsc::Receiver<sdr_transcription::TranscriptionEvent>,
) {
    attach_session_audio_tap(deps);

    // Weak refs for the entire timeout source — don't keep widgets
    // alive past their UI lifetime through the glib timeout source.
    let status_weak = deps.panel.status_label.downgrade();
    let progress_weak = deps.panel.progress_bar.downgrade();
    let tv_weak = deps.panel.text_view.downgrade();
    let enable_row_weak = deps.panel.enable_row.downgrade();
    #[cfg(feature = "sherpa")]
    let model_row_weak = deps.panel.model_row.downgrade();
    #[cfg(feature = "sherpa")]
    let display_mode_row_weak = deps.panel.display_mode_row.downgrade();
    #[cfg(feature = "sherpa")]
    let live_line_weak = deps.panel.live_line_label.downgrade();
    let state_for_marker = Rc::clone(&deps.state);
    let session_rows = Rc::clone(&deps.session_rows);
    let alive = Rc::new(std::cell::Cell::new(true));
    *deps.session_alive.borrow_mut() = Some(Rc::clone(&alive));

    glib::timeout_add_local(Duration::from_millis(100), move || {
        // Session-identity guard: if `stop` already invalidated this
        // session, break without touching any widget — the enable
        // row and locked rows now belong to a newer session (or to
        // no session). Per CR round 1 on PR #844.
        if !alive.get() {
            return glib::ControlFlow::Break;
        }
        // Upgrade once per tick. If any widget has been dropped
        // (e.g. window closed), stop the timeout immediately so we
        // don't resurrect dead UI.
        let (Some(status), Some(progress), Some(tv)) = (
            status_weak.upgrade(),
            progress_weak.upgrade(),
            tv_weak.upgrade(),
        ) else {
            return glib::ControlFlow::Break;
        };
        let ui = SessionTickUi {
            status,
            progress,
            tv,
            enable_row: enable_row_weak.clone(),
            session_rows: Rc::clone(&session_rows),
            state: Rc::clone(&state_for_marker),
            #[cfg(feature = "sherpa")]
            model_row: model_row_weak.clone(),
            #[cfg(feature = "sherpa")]
            display_mode_row: display_mode_row_weak.clone(),
            #[cfg(feature = "sherpa")]
            live_line: live_line_weak.clone(),
        };
        if let Some(flow) = drain_transcription_events(&event_rx, &ui) {
            return flow;
        }
        glib::ControlFlow::Continue
    });
}

/// Hand the backend's audio sender to the DSP tap and reset the
/// per-session UI state. Drops any channel-marker buffered while
/// transcription was off — the first text event after re-enable
/// should attribute to the *next* hop, not whichever channel the
/// scanner happened to land on during the off period (CR round 1 on
/// PR #558).
fn attach_session_audio_tap(deps: &SessionDeps) {
    if let Some(audio_tx) = deps.engine.borrow().audio_sender() {
        deps.state
            .send_dsp(crate::messages::UiToDsp::EnableTranscription(audio_tx));
    }
    *deps.state.pending_channel_marker.borrow_mut() = None;
    deps.panel.status_label.set_text("Starting...");
    deps.panel.status_label.set_visible(true);
}

/// Session-stop half of the enable toggle: unlock the rows, detach
/// the DSP tap, shut the backend down, and clear the status area.
fn stop_transcription_session(deps: &SessionDeps) {
    if let Some(alive) = deps.session_alive.borrow_mut().take() {
        alive.set(false);
    }
    deps.session_rows.unlock();
    deps.state
        .send_dsp(crate::messages::UiToDsp::DisableTranscription);
    // Drop any pending channel-marker so a stray scanner hop that
    // landed during the live session doesn't poison the next
    // enable's first text event. Per CodeRabbit round 1 on PR #558.
    *deps.state.pending_channel_marker.borrow_mut() = None;
    deps.engine.borrow_mut().shutdown_nonblocking();
    deps.panel.status_label.set_text("");
    deps.panel.status_label.set_visible(false);
    deps.panel.progress_bar.set_visible(false);
    // Clear any stale partial on stop so the previous session's last
    // in-progress text doesn't linger on screen.
    #[cfg(feature = "sherpa")]
    {
        deps.panel.live_line_label.set_text("");
        deps.panel.live_line_label.set_visible(false);
    }
}

/// Drain pending backend events for one poll tick of an active
/// transcription session. Returns `Some(Break)` on a terminal event
/// (fatal error, channel disconnect) and `None` when the queue is
/// drained and polling should continue. Split out per the 50-NLOC
/// gate (#817).
#[allow(clippy::too_many_arguments)]
fn drain_transcription_events(
    event_rx: &std::sync::mpsc::Receiver<sdr_transcription::TranscriptionEvent>,
    ui: &SessionTickUi,
) -> Option<glib::ControlFlow> {
    use sdr_transcription::TranscriptionEvent;

    loop {
        match event_rx.try_recv() {
            Ok(event) => match event {
                TranscriptionEvent::Downloading { progress_pct } => {
                    ui.status
                        .set_text(&format!("Downloading model ({progress_pct}%)..."));
                    ui.status.set_visible(true);
                    ui.progress.set_fraction(f64::from(progress_pct) / 100.0);
                    ui.progress.set_visible(true);
                }
                TranscriptionEvent::Ready => {
                    ui.status.set_text("Listening...");
                    ui.status.set_css_classes(&["success"]);
                    ui.progress.set_visible(false);
                }
                TranscriptionEvent::Partial { text } => {
                    #[cfg(feature = "sherpa")]
                    show_live_partial(&text, &ui.model_row, &ui.display_mode_row, &ui.live_line);
                    #[cfg(not(feature = "sherpa"))]
                    {
                        // Whisper never emits Partial, but
                        // the enum variant is compiled in.
                        // Defensive no-op.
                        let _ = text;
                    }
                }
                TranscriptionEvent::Text { timestamp, text } => {
                    append_transcribed_line(&ui.tv, &ui.state, &timestamp, &text);
                    // An utterance committed — the live
                    // line is now stale. Clear and hide
                    // it so the next Partial starts fresh.
                    #[cfg(feature = "sherpa")]
                    clear_live_line(&ui.live_line);
                }
                TranscriptionEvent::Error(msg) => {
                    // Fatal — backend has exited.
                    // Mirror the synchronous start()
                    // failure teardown so the UI
                    // isn't left locked.
                    teardown_failed_session(ui, &msg);
                    return Some(glib::ControlFlow::Break);
                }
            },
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                on_transcription_channel_closed(ui);
                return Some(glib::ControlFlow::Break);
            }
        }
    }
    None
}

/// Channel-disconnect handling: distinguish a normal user stop from a
/// spontaneous backend death.
///
/// - User stop: the off branch of `enable_row.connect_active_notify`
///   already ran (it dropped `audio_tx`, which made the worker exit
///   and drop `event_tx` — the disconnect we're seeing). The toggle
///   is inactive and the rows are re-enabled; doing anything more
///   would overwrite that clean state with a spurious error message
///   on every normal stop.
/// - Spontaneous death: the worker dropped `event_tx` without the
///   user clicking anything (toggle still active). Mirror the Error
///   arm's teardown so the UI doesn't strand the user with locked
///   controls and a stale "Listening..." status.
fn on_transcription_channel_closed(ui: &SessionTickUi) {
    let was_user_stop = ui.enable_row.upgrade().is_none_or(|e| !e.is_active());
    if was_user_stop {
        tracing::debug!("transcription event channel closed (user stop)");
        return;
    }
    tracing::warn!("transcription event channel disconnected unexpectedly");
    teardown_failed_session(ui, "Transcription stopped unexpectedly");
}

/// Disable every transcription settings row for the duration of a
/// session. Counterpart of [`SessionRowWeaks::unlock`].
fn lock_session_rows(t: &sidebar::transcript_panel::TranscriptPanel) {
    t.model_row.set_sensitive(false);
    #[cfg(feature = "whisper")]
    t.silence_row.set_sensitive(false);
    t.noise_gate_row.set_sensitive(false);
    t.audio_enhancement_row.set_sensitive(false);
    #[cfg(feature = "sherpa")]
    {
        t.display_mode_row.set_sensitive(false);
        t.vad_threshold_row.set_sensitive(false);
        t.auto_break_row.set_sensitive(false);
        t.auto_break_min_open_row.set_sensitive(false);
        t.auto_break_tail_row.set_sensitive(false);
        t.auto_break_min_segment_row.set_sensitive(false);
    }
}

/// Per-tick view of an active session's widgets + shared state, built
/// once per poll from the weak captures. Collapses the event-drain
/// and teardown signatures.
struct SessionTickUi {
    status: gtk4::Label,
    progress: gtk4::ProgressBar,
    tv: gtk4::TextView,
    enable_row: glib::WeakRef<adw::SwitchRow>,
    session_rows: Rc<SessionRowWeaks>,
    state: Rc<AppState>,
    #[cfg(feature = "sherpa")]
    model_row: glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "sherpa")]
    display_mode_row: glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "sherpa")]
    live_line: glib::WeakRef<gtk4::Label>,
}

/// Mid-session failure teardown, shared by the `Error` event arm and
/// the spontaneous channel-disconnect arm: unlock the settings rows,
/// snap the toggle off, surface the error on the status label, and
/// clear any stale live partial.
fn teardown_failed_session(ui: &SessionTickUi, msg: &str) {
    ui.session_rows.unlock();
    if let Some(enable) = ui.enable_row.upgrade() {
        enable.set_active(false);
    }
    ui.status.set_text(msg);
    ui.status.set_css_classes(&["error"]);
    ui.status.set_visible(true);
    ui.progress.set_visible(false);
    #[cfg(feature = "sherpa")]
    clear_live_line(&ui.live_line);
}

/// Append a finalized utterance to the transcript log. Drains the
/// pending channel-marker (#517) BEFORE inserting — the marker
/// belongs ABOVE the text it precedes, and lazy emission means
/// markers only land when there's actual audio to attribute (a
/// quiet channel never produces a divider). Auto-scrolls to the end.
fn append_transcribed_line(tv: &gtk4::TextView, state: &Rc<AppState>, timestamp: &str, text: &str) {
    if let Some((switched_at, channel_name)) = state.pending_channel_marker.borrow_mut().take() {
        sidebar::transcript_panel::push_channel_marker(tv, switched_at, &channel_name);
    }
    let buf = tv.buffer();
    let mut end = buf.end_iter();
    buf.insert(&mut end, &format!("[{timestamp}] {text}\n"));
    let mark = buf.create_mark(None, &buf.end_iter(), false);
    tv.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
    buf.delete_mark(&mark);
}

/// Clear + hide the live partial line (stale once an utterance
/// commits or the session ends).
#[cfg(feature = "sherpa")]
fn clear_live_line(live_line: &glib::WeakRef<gtk4::Label>) {
    if let Some(label) = live_line.upgrade() {
        label.set_text("");
        label.set_visible(false);
    }
}

/// Paint a sherpa partial into the live line — but only when (a) the
/// current model actually supports partials ((a) defends against a
/// future bug where an offline model emits a spurious `Partial`; the
/// UI shouldn't trust that it can't) and (b) the display mode is
/// Live. Both are re-read per event so a mid-session toggle takes
/// effect. Privacy: the raw text is never logged.
#[cfg(feature = "sherpa")]
fn show_live_partial(
    text: &str,
    model_row: &glib::WeakRef<adw::ComboRow>,
    display_mode_row: &glib::WeakRef<adw::ComboRow>,
    live_line: &glib::WeakRef<gtk4::Label>,
) {
    let model_supports_partials = model_row.upgrade().is_some_and(|row| {
        let idx = row.selected() as usize;
        sdr_transcription::SherpaModel::ALL
            .get(idx)
            .copied()
            .is_some_and(sdr_transcription::SherpaModel::supports_partials)
    });
    let show_live = model_supports_partials
        && display_mode_row
            .upgrade()
            .is_some_and(|row| row.selected() != DISPLAY_MODE_FINAL_IDX);
    if show_live && let Some(label) = live_line.upgrade() {
        label.set_text(text);
        label.set_visible(true);
    }
    tracing::debug!(
        target: "transcription",
        partial_chars = text.chars().count(),
        "sherpa partial received"
    );
}

/// Assemble the Whisper `BackendConfig` from the panel controls at
/// session start. Whisper has no Silero VAD or Auto Break sliders —
/// those fields carry compile-time defaults the backend never reads.
#[cfg(feature = "whisper")]
fn build_backend_config(
    transcript: &sidebar::transcript_panel::TranscriptPanel,
    model_idx: usize,
    _auto_break_enabled: bool,
) -> sdr_transcription::BackendConfig {
    let whisper_model = sdr_transcription::WhisperModel::ALL
        .get(model_idx)
        .copied()
        .unwrap_or(sdr_transcription::WhisperModel::TinyEn);
    #[allow(clippy::cast_possible_truncation)]
    sdr_transcription::BackendConfig {
        model: sdr_transcription::ModelChoice::Whisper(whisper_model),
        silence_threshold: transcript.silence_row.value() as f32,
        noise_gate_ratio: transcript.noise_gate_row.value() as f32,
        vad_threshold: sdr_transcription::VAD_THRESHOLD_DEFAULT,
        segmentation_mode: sdr_transcription::SegmentationMode::Vad,
        auto_break_min_open_ms: sdr_transcription::AUTO_BREAK_MIN_OPEN_MS_DEFAULT,
        auto_break_tail_ms: sdr_transcription::AUTO_BREAK_TAIL_MS_DEFAULT,
        auto_break_min_segment_ms: sdr_transcription::AUTO_BREAK_MIN_SEGMENT_MS_DEFAULT,
        audio_enhancement: read_audio_enhancement(transcript),
    }
}

/// Assemble the Sherpa `BackendConfig` from the panel controls at
/// session start. `silence_threshold` is a sentinel — `SherpaBackend`
/// never reads it (see `build_recognizer_config`).
#[cfg(feature = "sherpa")]
fn build_backend_config(
    transcript: &sidebar::transcript_panel::TranscriptPanel,
    model_idx: usize,
    auto_break_enabled: bool,
) -> sdr_transcription::BackendConfig {
    let sherpa_model = sdr_transcription::SherpaModel::ALL
        .get(model_idx)
        .copied()
        .unwrap_or(sdr_transcription::SherpaModel::StreamingZipformerEn);
    let segmentation_mode = if auto_break_enabled {
        sdr_transcription::SegmentationMode::AutoBreak
    } else {
        sdr_transcription::SegmentationMode::Vad
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    sdr_transcription::BackendConfig {
        model: sdr_transcription::ModelChoice::Sherpa(sherpa_model),
        silence_threshold: 0.0,
        noise_gate_ratio: transcript.noise_gate_row.value() as f32,
        vad_threshold: transcript.vad_threshold_row.value() as f32,
        segmentation_mode,
        auto_break_min_open_ms: transcript.auto_break_min_open_row.value() as u32,
        auto_break_tail_ms: transcript.auto_break_tail_row.value() as u32,
        auto_break_min_segment_ms: transcript.auto_break_min_segment_row.value() as u32,
        audio_enhancement: read_audio_enhancement(transcript),
    }
}

/// Audio-enhancement mode from the panel combo row, captured at
/// session start (not subscribed to — matches the "lock during
/// session" behavior of every transcription setting).
fn read_audio_enhancement(
    transcript: &sidebar::transcript_panel::TranscriptPanel,
) -> sdr_transcription::denoise::AudioEnhancement {
    match transcript.audio_enhancement_row.selected() {
        sidebar::transcript_panel::AUDIO_ENHANCEMENT_BROADBAND_IDX => {
            sdr_transcription::denoise::AudioEnhancement::Broadband
        }
        sidebar::transcript_panel::AUDIO_ENHANCEMENT_OFF_IDX => {
            sdr_transcription::denoise::AudioEnhancement::Off
        }
        _ => sdr_transcription::denoise::AudioEnhancement::VoiceBand,
    }
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
