//! Turns `WefaxCatcher` [`Action`]s into `UiToDsp` messages, viewer
//! calls, and toasts (epic #913, Task 6). Mirrors
//! `window::satellites::recorder`'s `RecorderDeps` +
//! `interpret_recorder_action` split.

use std::rc::Rc;

use gtk4::glib;
use libadwaita as adw;
use sdr_types::DemodMode;

use crate::messages::UiToDsp;
use crate::sidebar::wefax_catcher::{Action, SavedTune};
use crate::state::AppState;

/// Everything [`interpret_wefax_action`] needs, captured once per
/// enable. The catcher itself is pure — `tick()` returns actions —
/// this is the wiring layer that gives each action its side effects.
pub(super) struct WefaxDeps {
    pub(super) state: Rc<AppState>,
    /// Resolves the current main window for
    /// [`crate::wefax_viewer::open_wefax_viewer_if_needed`]. `WeakRef`
    /// would be wrong here (this is a plain closure, not a widget
    /// clone captured by a long-lived signal handler on a *different*
    /// widget); it already walks up from a `WeakRef` internally — see
    /// `super::build_parent_provider`.
    pub(super) parent_provider: Rc<dyn Fn() -> Option<gtk4::Window>>,
    pub(super) toast_overlay: glib::WeakRef<adw::ToastOverlay>,
}

/// Interpret one [`Action`] from the WEFAX auto-catch state machine's
/// tick.
#[allow(
    clippy::cast_precision_loss,
    reason = "freq_hz is a WEFAX HF channel (<=30 MHz), well below f64's 2^53 mantissa ceiling"
)]
pub(super) fn interpret_wefax_action(deps: &WefaxDeps, action: Action) {
    match action {
        Action::Tune(freq_hz) => deps.state.send_dsp(UiToDsp::Tune(freq_hz as f64)),
        Action::SetDemodMode => deps.state.send_dsp(UiToDsp::SetDemodMode(DemodMode::Wefax)),
        Action::ResetDecoder => deps.state.send_dsp(UiToDsp::ResetImagingDecoders),
        Action::OpenViewer => {
            crate::wefax_viewer::open_wefax_viewer_if_needed(&deps.parent_provider, &deps.state);
        }
        // PNG auto-save already happens unconditionally on every
        // `DspToUi::WefaxImageComplete`
        // (`window/dsp_events.rs::on_wefax_image_complete`) — WEFAX has
        // no pass/AOS-LOS concept to batch a save against, so every
        // completed chart is saved as soon as it arrives regardless of
        // whether auto-catch is what tuned to it. Documented no-op
        // per Task 6 ruling: driving a second save here would double
        // -write the same chart.
        Action::SavePng(_) => {}
        Action::RestoreTune(saved) => restore_tune(deps, &saved),
        Action::Toast(msg) => post_toast(deps, &msg),
    }
}

/// `Action::RestoreTune` — put the receiver back on the user's pre
/// -auto-catch frequency and demod mode. Mirrors the DSP-facing half
/// of `window::tune_to_target` (no widget mirroring: the header
/// frequency display / demod dropdown intentionally don't track the
/// auto-catch scan, so there is nothing to restore there either).
fn restore_tune(deps: &WefaxDeps, saved: &SavedTune) {
    deps.state.send_dsp(UiToDsp::Tune(saved.center_hz));
    deps.state.send_dsp(UiToDsp::SetDemodMode(saved.demod_mode));
    // Bandwidth MUST land after SetDemodMode: the DSP's SetDemodMode
    // handler resets the channel bandwidth to the mode's default, which
    // would otherwise clobber the user's restored width. Mirrors the
    // load-bearing order in `window::tune_to_target`.
    deps.state
        .send_dsp(UiToDsp::SetBandwidth(saved.bandwidth_hz));
    deps.state.center_frequency.set(saved.center_hz);
    deps.state.demod_mode.set(saved.demod_mode);
}

fn post_toast(deps: &WefaxDeps, msg: &str) {
    if let Some(overlay) = deps.toast_overlay.upgrade() {
        overlay.add_toast(crate::viewer::plain_toast(msg));
    }
}
