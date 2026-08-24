//! Auto-recorder action layer: [`RecorderDeps`] and the interpreter
//! that maps each pure `AutoRecorder` action onto the live UI / DSP /
//! filesystem — AOS tune + audio-chain force-off, LOS restore, and
//! viewer bring-up. Split out of `window/satellites.rs` per the
//! Codacy 500-NLOC file gate on PR #844.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{AppState, CtcssMode, Rc, SidebarPanels, TuneFn, UiToDsp, adw, glib, sidebar};
use super::{
    compute_apt_rotate_180_for_pass, on_save_apt_png, on_save_lrpt_pass, on_save_sstv_pass,
    post_toast,
};

/// Build the recorder-action interpreter: a parent-window resolver for
/// the auto-open-viewer side effect plus a [`RecorderDeps`] bundle,
/// wrapped in a closure that dispatches each [`RecorderAction`]
/// through `interpret_recorder_action`.
pub(super) fn build_recorder_interpreter(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    cache: Option<&std::sync::Arc<sdr_sat::TleCache>>,
    toast_overlay: &adw::ToastOverlay,
    tune_to_satellite: &Rc<TuneFn>,
    set_playing: &Rc<dyn Fn(bool)>,
) -> Rc<dyn Fn(sidebar::satellites_recorder::Action)> {
    use sidebar::satellites_recorder::Action as RecorderAction;
    // Parent-window resolver for the auto-open-viewer side effect.
    // Walks up the widget tree from the satellites page; falls
    // back to `None` if the widget has been detached, in which
    // case the open is silently skipped. Holds a `WeakRef` so the
    // 1 Hz timer's `panel_weak.upgrade() == None` exit gate can
    // actually fire — a strong clone here would keep the panel
    // widget alive and the timer would never break.
    let parent_provider_for_recorder: Rc<dyn Fn() -> Option<gtk4::Window>> = {
        let widget_weak = panels.satellites.widget.downgrade();
        Rc::new(move || {
            widget_weak
                .upgrade()
                .and_then(|w| w.root())
                .and_then(|r| r.downcast::<gtk4::Window>().ok())
        })
    };

    {
        let deps = Rc::new(RecorderDeps {
            state: Rc::clone(state),
            tune: Rc::clone(tune_to_satellite),
            set_playing: Rc::clone(set_playing),
            // Optional TLE cache — used by the SavePng wiring to
            // compute `is_ascending` for the rotate-180 flag (B2 of
            // the noaa-apt parity work). `None` when the host
            // platform refused us a cache directory; the rotate path
            // falls back to "no rotation" in that case.
            cache: cache.map(std::sync::Arc::clone),
            // Weak refs for the same lifecycle reason as
            // `parent_provider_for_recorder` — strong clones would
            // pin the widgets alive past window close.
            toast_overlay: toast_overlay.downgrade(),
            parent_provider: Rc::clone(&parent_provider_for_recorder),
            // Scanner master switch handle for the LOS-side restore;
            // radio panel rows for the AOS audio-chain force-off +
            // LOS restore (#555/#556). Set-active fires the widgets'
            // own notify handlers — the same path a user flip takes.
            scanner_switch: panels.scanner.master_switch.clone(),
            radio: panels.radio.clone(),
            doppler_switch: panels.satellites.doppler_switch.clone(),
            auto_record_composites_switch: panels.satellites.auto_record_composites_switch.clone(),
        });
        Rc::new(move |action: RecorderAction| interpret_recorder_action(&deps, action))
    }
}

/// Everything the auto-recorder action interpreter needs, captured
/// once. The recorder itself is pure — `tick()` returns actions —
/// and this is the wiring layer that gives each action its widgets.
pub(in crate::window) struct RecorderDeps {
    pub(super) state: Rc<AppState>,
    pub(super) tune: Rc<TuneFn>,
    pub(super) set_playing: Rc<dyn Fn(bool)>,
    pub(super) cache: Option<std::sync::Arc<sdr_sat::TleCache>>,
    pub(super) toast_overlay: glib::WeakRef<adw::ToastOverlay>,
    pub(super) parent_provider: Rc<dyn Fn() -> Option<gtk4::Window>>,
    pub(super) scanner_switch: gtk4::Switch,
    pub(super) radio: sidebar::radio_panel::RadioPanel,
    pub(super) doppler_switch: adw::SwitchRow,
    pub(super) auto_record_composites_switch: adw::SwitchRow,
}

/// Interpret one [`RecorderAction`] from the auto-record state
/// machine's tick.
pub(super) fn interpret_recorder_action(
    deps: &RecorderDeps,
    action: sidebar::satellites_recorder::Action,
) {
    use sidebar::satellites_recorder::{Action as RecorderAction, ToastKind};

    match action {
        RecorderAction::StartAutoRecord {
            satellite,
            norad_id,
            freq_hz,
            mode,
            bandwidth_hz,
            protocol,
        } => {
            on_recorder_aos(
                deps,
                &satellite,
                norad_id,
                freq_hz,
                mode,
                bandwidth_hz,
                protocol,
            );
        }
        RecorderAction::StartAutoAudioRecord(path) => {
            tracing::info!("auto-record AOS: opening WAV writer at {path:?}");
            deps.state.send_dsp(UiToDsp::StartAudioRecording(path));
        }
        RecorderAction::StopAutoAudioRecord => {
            tracing::info!("auto-record LOS: closing WAV writer");
            deps.state.send_dsp(UiToDsp::StopAudioRecording);
        }
        RecorderAction::ResetImagingDecoders => {
            // Between-pass decoder flush. The state machine
            // emits this at every `Recording → Finalizing`
            // transition (LOS), AFTER the save action's
            // snapshot read of the shared `LrptImage`. When
            // `was_running == true` pre-AOS this is the only
            // hook between passes — `RestoreTune` keeps the
            // source open across LOS → AOS, so the
            // source-stop reset never fires. When
            // `was_running == false` the subsequent
            // `set_playing(false)` in `RestoreTune` triggers
            // the source-stop path which resets again —
            // idempotent (`reset_imaging_decoders` only
            // touches in-flight buffers), so the
            // double-reset is harmless. Per issue #544.
            tracing::info!("auto-record LOS: resetting imaging decoders");
            deps.state.send_dsp(UiToDsp::ResetImagingDecoders);
        }
        RecorderAction::SavePng(path) => on_save_apt_png(deps, path),
        RecorderAction::SaveLrptPass(dir) => on_save_lrpt_pass(deps, dir),
        RecorderAction::SaveSstvPass(dir) => on_save_sstv_pass(deps, dir),
        RecorderAction::RestoreTune(saved) => on_restore_tune(deps, saved),
        RecorderAction::Toast { message, kind } => {
            if matches!(kind, ToastKind::Warn) {
                // No dedicated warn styling on AdwToast; the
                // message itself carries the severity. Tracing
                // captures it for the log either way.
                tracing::warn!("auto-record: {message}");
            }
            post_toast(&deps.toast_overlay, &message);
        }
    }
}

/// AOS side of the auto-recorder: tune, open the protocol viewer,
/// force the audio chain off, and stop the scanner.
/// Split out of [`interpret_recorder_action`] per the 50-NLOC gate
/// (#817).
#[allow(clippy::too_many_arguments)]
fn on_recorder_aos(
    deps: &RecorderDeps,
    satellite: &str,
    norad_id: u32,
    freq_hz: u64,
    mode: sdr_types::DemodMode,
    bandwidth_hz: u32,
    protocol: sdr_sat::ImagingProtocol,
) {
    tracing::info!(
        "auto-record AOS: tuning to {satellite} @ {freq_hz} Hz, BW {bandwidth_hz} Hz, protocol {protocol:?}",
    );
    // ACARS-engaged gating happens at the recorder
    // tick site (the for-loop calling
    // `interpret_action`), not here — that's the
    // only level that has visibility into the
    // entire `Vec<RecorderAction>` batch and can
    // defer it as a unit. CR round 1 on PR #591
    // flagged the gap: stashing only this single
    // `StartAutoRecord` while iterating the rest
    // of the batch left `StartAutoAudioRecord` etc.
    // running while ACARS was still engaged.
    // Per-protocol viewer dispatch. Adding a new
    // protocol means adding a match arm here +
    // flipping `imaging_protocol` on the catalog
    // entry — no recorder change needed. Per #514.
    //
    // **Fail closed on unsupported protocols.** All
    // AOS side effects (set_playing, tune, zero VFO,
    // open viewer) live INSIDE each arm rather than
    // unconditionally before the match. Per CR
    // round 1 on PR #541: if a catalog entry is
    // flipped to `Some(Lrpt)` ahead of Task 7 wiring
    // the LRPT viewer, the user's tune state must
    // NOT be hijacked just to land in a no-op
    // branch.
    // Audio-chain force-off helper. Flips the three
    // user-visible widgets to disabled; each one's
    // `connect_*_notify` handler then dispatches the
    // corresponding `UiToDsp::Set*` message — same
    // path a manual flip takes. Per #555 (squelch +
    // CTCSS) / #556 (FM IF NR). Idempotent: a
    // `set_active(false)` on an already-false switch
    // is a no-op (no notify fired, no redundant
    // dispatch).
    //
    // Called inside each protocol arm rather than
    // before the match so an unsupported protocol
    // doesn't hijack the user's audio chain (mirrors
    // the AOS side-effect-isolation rule from CR
    // round 1 on PR #541).

    match protocol {
        sdr_sat::ImagingProtocol::Apt => {
            aos_apt(deps, norad_id, freq_hz, mode, bandwidth_hz);
        }
        sdr_sat::ImagingProtocol::Lrpt => {
            aos_lrpt(deps, norad_id, freq_hz, mode, bandwidth_hz);
        }
        sdr_sat::ImagingProtocol::Sstv => {
            aos_sstv(deps, norad_id, freq_hz, mode, bandwidth_hz);
        }
    }
}

/// APT AOS wiring. Split out of [`on_recorder_aos`] per the 50-NLOC
/// gate (#817).
fn aos_apt(
    deps: &RecorderDeps,
    norad_id: u32,
    freq_hz: u64,
    mode: sdr_types::DemodMode,
    bandwidth_hz: u32,
) {
    // **Order is load-bearing.** Force the
    // audio chain off BEFORE start/tune so
    // the very first samples through the
    // freshly-tuned demod aren't gated by a
    // stale squelch / CTCSS / FM-IF-NR
    // setting. Otherwise low-SNR AOS rows
    // race against the change-notify chain
    // dispatching the SetX messages and the
    // first few scan lines could be silenced
    // before the demod even sees them. Per
    // CR round 1 on PR #557.
    force_audio_chain_off(deps);
    // Drive Start through the header play
    // button — its `connect_toggled` handler
    // is the single place that updates
    // `state.is_running`, dispatches
    // `UiToDsp::Start`, and swaps the
    // play/stop icon. `set_active` is a
    // no-op when the radio is already
    // running, so this is safe to call
    // unconditionally without a duplicate
    // Start. The pre-AOS `was_running` flag
    // (captured in `SavedTune` by the 1 Hz
    // tick before this action fires) drives
    // the corresponding LOS-side stop.
    (deps.set_playing)(true);
    (deps.tune)(freq_hz, mode, bandwidth_hz);
    // Zero the live VFO offset for the
    // auto-record pass. The user's pre-AOS
    // offset (a manual VFO drag away from
    // centre) is preserved in `SavedTune`
    // for the LOS restore, but during the
    // pass the demod must align *exactly*
    // with the satellite's downlink —
    // otherwise we'd demod at `freq_hz +
    // saved_offset` and the APT subcarrier
    // would land outside the channel
    // filter. The DSP's
    // `DspToUi::VfoOffsetChanged` echo
    // updates the spectrum widget, freq
    // selector, and status bar; no manual
    // mirror needed.
    deps.state.dispatch_vfo_offset(0.0);
    crate::apt_viewer::open_apt_viewer_if_needed(&deps.parent_provider, &deps.state);
    // Clear the canvas at AOS so a back-to-back
    // pass (e.g. NOAA 18 → NOAA 19 with
    // overlapping viewer sessions) starts on a
    // clean image. The viewer was either just
    // opened above (already empty) or carried
    // over from a previous pass — either way,
    // an explicit clear keeps the image we're
    // about to save scoped to *this* pass.
    if let Some(view) = deps.state.apt_viewer.borrow().as_ref() {
        view.clear();
    }
    // Stash recording-pass info for the LOS-side
    // SavePng wiring to compute the auto-rotation
    // flag (B2 of the noaa-apt parity work). The
    // exact AOS time matters less than "around
    // when" — `is_ascending` checks the lat
    // derivative over a 30 s window, which is
    // valid anywhere mid-pass. NORAD id arrives
    // pre-resolved on the recorder action — no
    // name → catalog lookup at this layer means
    // no silent rotation breakage if the catalog
    // ever picks up alias drift. Per CR round 3
    // on PR #571.
    let aos = chrono::Utc::now();
    *deps.state.apt_recording_pass.borrow_mut() = Some((norad_id, aos));
    // Push the rotate-180 flag down to the
    // renderer so the toolbar's manual `Export
    // PNG` button matches the auto-record
    // orientation. Without this, manual exports
    // of ascending passes come out upside-down
    // even though the auto-record save rotates
    // them. Per CR round 1 on PR #571.
    let rotate_180 = compute_apt_rotate_180_for_pass(deps.cache.as_ref(), norad_id, aos);
    if let Some(view) = deps.state.apt_viewer.borrow().as_ref() {
        view.set_rotate_180(rotate_180);
    }
}

/// LRPT AOS wiring. Split out of [`on_recorder_aos`] per the 50-NLOC
/// gate (#817).
fn aos_lrpt(
    deps: &RecorderDeps,
    norad_id: u32,
    freq_hz: u64,
    mode: sdr_types::DemodMode,
    bandwidth_hz: u32,
) {
    // **Order is load-bearing.** Reset the
    // shared image AND the viewer canvas
    // BEFORE starting playback / retuning
    // so the freshly-tuned LRPT decoder
    // can't push pass-1 leftover rows (or
    // race the clear and erase the first
    // few rows of the new pass). Per
    // CodeRabbit round 8 on PR #543.
    //
    // The viewer is opened first so the
    // `view.clear()` call below can target
    // it — the open path also sends
    // `UiToDsp::SetLrptImage(handle)` to
    // the DSP thread, which lazy-inits the
    // decoder against the (now-cleared)
    // shared image. Catalog's
    // `demod_mode: Lrpt` lines up the
    // controller's IF rate (144 ksps) with
    // the QPSK demod's expected sample rate
    // — without that, `radio_input` would
    // be at the wrong rate and the demod's
    // resampler would sit at the wrong
    // setpoint. Per epic #469 task 7.

    // Tell the DSP thread which Meteor
    // downlink profile to use for this pass
    // — METEOR-M N2 was QPSK with
    // differential precoding; the active
    // METEOR-M2 3 / M2-4 are plain OQPSK
    // (#730). Sent
    // BEFORE `open_lrpt_viewer_if_needed`
    // (which triggers lazy decoder init)
    // so the freshly-built decoder uses
    // the right inner chain. Per #662.
    //
    // Always send — even when the catalog
    // lookup misses or returns
    // `lrpt_modulation = None`. Otherwise
    // the previous pass's modulation leaks
    // into the next decoder init (the
    // controller stash defaults to OQPSK at
    // startup but mutates per
    // `SetLrptDownlink`, so a Qpsk-then-
    // None sequence would silently keep the
    // Qpsk chain). Fallback is `Qpsk`
    // because that's the standards-default
    // LRPT modulation; a future LRPT
    // satellite that turns up uncatalogued
    // is more likely to be standard-spec
    // than Meteor-style OQPSK. Per CR
    // round 1 on PR #663.
    // Profile first, then the canvas wipe on the
    // DSP thread — the order matters (see
    // `lrpt_pass_start_commands`).
    for command in crate::lrpt_viewer::lrpt_pass_start_commands(norad_id, &deps.state.lrpt_image) {
        deps.state.send_dsp(command);
    }

    crate::lrpt_viewer::open_lrpt_viewer_if_needed(&deps.parent_provider, &deps.state);
    if let Some(view) = deps.state.lrpt_viewer.borrow().as_ref() {
        view.clear();
    }
    // Force audio chain off BEFORE start/tune
    // so the freshly-tuned demod isn't gated
    // by a stale squelch / CTCSS / FM-IF-NR
    // setting. Per CR round 1 on PR #557 —
    // see APT arm above for the full
    // rationale.
    force_audio_chain_off(deps);
    // Now safe to start playback + retune;
    // any decoded rows from this point
    // forward land in the cleared image.
    (deps.set_playing)(true);
    (deps.tune)(freq_hz, mode, bandwidth_hz);
    deps.state.dispatch_vfo_offset(0.0);
    // Mirror into AppState so is_recording() (used by
    // the close-to-tray Quit confirmation modal)
    // reflects an in-progress LRPT pass. Per #512.
    // The `(norad_id, aos)` tuple lets the LOS
    // completion path snapshot-and-compare so an
    // overlapping pass-N+1 AOS that starts during
    // pass-N's encode doesn't have its `is_recording`
    // flag clobbered when pass-N's completion
    // fires. Mirrors the APT
    // `apt_recording_pass` pattern from PR #571
    // round 4. Per CR round 2 on PR #575.
    let aos = chrono::Utc::now();
    *deps.state.lrpt_recording_pass.borrow_mut() = Some((norad_id, aos));
}

/// SSTV AOS wiring. Split out of [`on_recorder_aos`] per the 50-NLOC
/// gate (#817).
fn aos_sstv(
    deps: &RecorderDeps,
    norad_id: u32,
    freq_hz: u64,
    mode: sdr_types::DemodMode,
    bandwidth_hz: u32,
) {
    // **Order is load-bearing.** Open the viewer
    // (which sends `UiToDsp::SetSstvImage`) and
    // clear both the shared handle and the canvas
    // BEFORE starting playback / retuning so no
    // leftover rows from a previous pass land in
    // the fresh image buffer.  Mirrors the LRPT
    // arm's clear-before-start discipline from
    // CR round 8 on PR #543.
    crate::sstv_viewer::open_sstv_viewer_if_needed(&deps.parent_provider, &deps.state);
    deps.state.sstv_image.clear();
    // Failed-pass images now live in
    // `sstv_pending_export` keyed by their
    // original pass directory (moved there at
    // `SaveSstvPass` time, not here at AOS).
    // The current-pass buffer is the round-4
    // simple clear. Per CR round 6 #21 on
    // PR #599 (refines round 5 #20).
    deps.state.sstv_completed_images.borrow_mut().clear();
    if let Some(view) = deps.state.sstv_viewer.borrow().as_ref() {
        view.clear();
    }
    // ISS SSTV is audible NFM — do NOT force the
    // audio chain off.  The user's squelch /
    // CTCSS / FM-IF-NR settings are correct for
    // the mode; we only suppress them for
    // silent-passthrough decoders (LRPT) and the
    // APT path (subcarrier noise). Per CLAUDE.md
    // and step 8 of epic #472 task spec.
    (deps.set_playing)(true);
    (deps.tune)(freq_hz, mode, bandwidth_hz);
    deps.state.dispatch_vfo_offset(0.0);
    let aos = chrono::Utc::now();
    *deps.state.sstv_recording_pass.borrow_mut() = Some((norad_id, aos));
}

/// Force every audio-chain gate off for an imaging pass: squelch,
/// auto-squelch, CTCSS, FM IF NR, de-emphasis, notch, and the Doppler
/// tracker — each through the widget's own notify chain so LOS
/// restore replays the persisted values the same way a user flip
/// would. De-emphasis (US 75 µs cutoff ~2122 Hz / EU 50 µs
/// ~3183 Hz) and an audio-band notch would both attenuate the
/// 2400 Hz APT subcarrier; the Doppler tracker's 4 Hz `SetVfoOffset`
/// ticks can disrupt QPSK Costas lock and the APT line-rate clock
/// (per the NOAA 15 silent-fail investigation).
fn force_audio_chain_off(deps: &RecorderDeps) {
    if deps.radio.squelch_enabled_row.is_active() {
        deps.radio.squelch_enabled_row.set_active(false);
    }
    if deps.radio.auto_squelch_row.is_active() {
        deps.radio.auto_squelch_row.set_active(false);
    }
    let off_idx = sidebar::radio_panel::RadioPanel::ctcss_index_from_mode(CtcssMode::Off);
    if deps.radio.ctcss_row.selected() != off_idx {
        deps.radio.ctcss_row.set_selected(off_idx);
    }
    if deps.radio.fm_if_nr_row.is_active() {
        deps.radio.fm_if_nr_row.set_active(false);
    }
    // De-emphasis ComboRow index 0 = "None". Both
    // US 75 µs (cutoff ~2122 Hz) and EU 50 µs
    // (cutoff ~3183 Hz) attenuate the 2400 Hz APT
    // subcarrier that lives in the demodulated
    // audio. Force to "None" so the AF chain
    // passes the subcarrier through flat. Per
    // silent-fail investigation following the
    // NOAA 15 pass.
    let deemp_off_idx: u32 = 0;
    if deps.radio.deemphasis_row.selected() != deemp_off_idx {
        deps.radio.deemphasis_row.set_selected(deemp_off_idx);
    }
    // Notch: a user-set notch frequency anywhere in
    // the 1.5-3 kHz audio band (the typical "remove
    // a hum" use case) would null the APT subcarrier.
    // Force off; LOS restore reapplies the persisted
    // value via the same notify chain.
    if deps.radio.notch_enabled_row.is_active() {
        deps.radio.notch_enabled_row.set_active(false);
    }
    // Doppler tracker: 4 Hz `SetVfoOffset` ticks can
    // disrupt QPSK Costas lock and the APT line-rate
    // clock. The ±3.5 kHz worst-case shift fits
    // inside every imaging-protocol channel filter,
    // so disabling for the pass loses no functional
    // value and removes a known disruption source.
    // Per silent-fail investigation following the
    // NOAA 15 pass.
    if deps.doppler_switch.is_active() {
        deps.doppler_switch.set_active(false);
    }
}

/// LOS restore: put the radio + audio chain + scanner back the way
/// the user had them before AOS.
/// Split out of [`interpret_recorder_action`] per the 50-NLOC gate
/// (#817).
#[allow(clippy::too_many_arguments)]
fn on_restore_tune(deps: &RecorderDeps, saved: sidebar::satellites_recorder::SavedTune) {
    tracing::info!(
        "auto-record LOS: restoring tune to {} Hz (offset {} Hz), BW {} Hz",
        saved.freq_hz,
        saved.vfo_offset_hz,
        saved.bandwidth_hz,
    );
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "saved freq came from the same widget we're feeding back; \
              non-negative and well within u64"
    )]
    let freq_hz = saved.freq_hz as u64;
    (deps.tune)(freq_hz, saved.mode, saved.bandwidth_hz);
    // Replay the user's pre-AOS VFO offset so a
    // dragged-from-centre carrier comes back. The
    // existing `DspToUi::VfoOffsetChanged` handler
    // updates the spectrum + freq selector + status
    // bar when the DSP echoes the change, so we
    // don't have to mirror those widgets manually.
    deps.state.dispatch_vfo_offset(saved.vfo_offset_hz);
    // Re-engage ACARS if it was on before the pass.
    // Goes after the tune so the airband lock retunes
    // from the user's just-restored freq rather than
    // racing against it. Symmetric with the AOS arm.
    if deps.state.acars_was_engaged_pre_pass.replace(false) {
        tracing::info!("auto-record LOS: re-engaging ACARS (was on pre-pass)");
        deps.state.send_dsp(UiToDsp::SetAcarsEnabled(true));
    }
    // If the user had playback off pre-AOS, we
    // started the radio at AOS to make audio flow —
    // honour that round trip and stop it now. A user
    // who explicitly turned playback off during the
    // pass (rare) loses that intent here, but the
    // expected case (set-and-forget overnight) gets
    // the right behaviour. Routed through
    // `set_playing` (header play button) so the icon,
    // `state.is_running`, and DSP all move together.
    if !saved.was_running {
        tracing::info!("auto-record: stopping source (was stopped pre-AOS)");
        (deps.set_playing)(false);
    }
    // **Order is load-bearing.** Restore the audio-
    // chain settings BEFORE re-arming the scanner.
    // A scanner that wakes back up while the audio
    // chain is still in its forced-off pass state
    // would briefly run with wrong squelch / CTCSS /
    // FM-IF-NR — wasting the first dwell or two on
    // un-gated noise. Per CR round 1 on PR #557.
    //
    // Same `set_*`-fires-notify pattern the rest of
    // this handler uses (and that the AOS-side
    // `force_audio_chain_off` mirrors). Squelch
    // level is restored unconditionally (cheap,
    // idempotent on equal value); the toggles are
    // guarded against no-op writes for cleaner
    // tracing logs.
    restore_audio_chain(deps, &saved);
    // Re-arm the scanner if it was running pre-AOS.
    // The AOS-side `deps.tune` call goes through
    // `tune_to_satellite`, which fires
    // `ScannerForceDisable::trigger("satellite tune")`
    // as a manual-tune side effect — without this
    // restore, an active pre-AOS scan would be left
    // permanently off after the pass. Same idiom as
    // `was_running`: snapshot the user's pre-AOS
    // state, return them to it. `set_active(true)`
    // fires the switch's notify handler, which
    // dispatches `SetScannerEnabled(true)` to the
    // engine — same path a manual flip takes.
    if saved.scanner_running && !deps.scanner_switch.is_active() {
        tracing::info!("auto-record: re-arming scanner (was running pre-AOS)");
        deps.scanner_switch.set_active(true);
    }
}

/// Restore the audio-chain widgets the AOS path forced off — squelch
/// gates, CTCSS, FM IF NR, de-emphasis, notch, and the Doppler
/// master switch — from the pre-AOS snapshot, through each widget's
/// own notify chain (the same path a user flip takes).
fn restore_audio_chain(deps: &RecorderDeps, saved: &sidebar::satellites_recorder::SavedTune) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "saved.squelch_db came from the same SpinRow we're \
              feeding back into; round-trip preserves precision \
              well within the row's range"
    )]
    let squelch_db_f64 = f64::from(saved.squelch_db);
    deps.radio.squelch_level_row.set_value(squelch_db_f64);
    if saved.squelch_enabled != deps.radio.squelch_enabled_row.is_active() {
        deps.radio
            .squelch_enabled_row
            .set_active(saved.squelch_enabled);
    }
    if saved.auto_squelch_enabled != deps.radio.auto_squelch_row.is_active() {
        deps.radio
            .auto_squelch_row
            .set_active(saved.auto_squelch_enabled);
    }
    let saved_ctcss_idx = sidebar::radio_panel::RadioPanel::ctcss_index_from_mode(saved.ctcss_mode);
    if deps.radio.ctcss_row.selected() != saved_ctcss_idx {
        deps.radio.ctcss_row.set_selected(saved_ctcss_idx);
    }
    if saved.fm_if_nr_enabled != deps.radio.fm_if_nr_row.is_active() {
        deps.radio.fm_if_nr_row.set_active(saved.fm_if_nr_enabled);
    }
    // Restore the AF-chain widgets the AOS path
    // forced off — deemphasis (US 75 µs / EU 50 µs
    // attenuates the 2400 Hz APT subcarrier) and
    // notch (a user-set notch in the 1.5-3 kHz band
    // nulls the subcarrier). Same notify-fires-set
    // pattern as the gate restores above. Per
    // silent-fail investigation following the NOAA
    // 15 pass.
    if saved.deemphasis_idx != deps.radio.deemphasis_row.selected() {
        deps.radio.deemphasis_row.set_selected(saved.deemphasis_idx);
    }
    if saved.notch_enabled != deps.radio.notch_enabled_row.is_active() {
        deps.radio.notch_enabled_row.set_active(saved.notch_enabled);
    }
    // Doppler tracker — restore the user's pre-AOS
    // master-switch preference. The AOS-side
    // `force_audio_chain_off` flipped it off for the
    // pass; this brings it back so the next pass (if
    // they have Doppler on as their default) gets
    // automatic correction again.
    if saved.doppler_enabled != deps.doppler_switch.is_active() {
        deps.doppler_switch.set_active(saved.doppler_enabled);
    }
}
