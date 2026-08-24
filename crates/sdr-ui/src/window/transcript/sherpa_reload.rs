//! Sherpa in-place model reload (PR 5 architecture): drop the old
//! recognizer, build the new one on a worker thread, poll for the
//! outcome, and keep the reload-sensitive rows locked meanwhile.
//! Split out of `window/transcript.rs` per the Codacy 500-NLOC file
//! gate on PR #844. The whole module is `sherpa`-only — whisper
//! builds never compile it.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{Duration, adw, glib, sidebar};

/// Widget handles for an in-flight sherpa model reload: the status
/// area plus weak refs to the two rows that get locked while the
/// swap runs. Weak upgrades no-op when the window is closing.
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
pub(super) fn wire_sherpa_model_reload(
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
fn reenable_reload_rows(ui: &ReloadUi) {
    if let Some(model_row) = ui.model_row.upgrade() {
        model_row.set_sensitive(true);
    }
    if let Some(enable_row) = ui.enable_row.upgrade() {
        enable_row.set_sensitive(true);
    }
}
