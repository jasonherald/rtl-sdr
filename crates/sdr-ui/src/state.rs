//! Application state shared across GTK closures.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::mpsc;

use sdr_acars::{AcarsMessage, ChannelStats};
use sdr_core::acars_airband_lock::{PreLockSnapshot, US_SIX_CHANNEL_COUNT};
use sdr_types::DemodMode;

use crate::messages::UiToDsp;

/// Default center frequency in Hz (100 MHz — FM broadcast band).
const DEFAULT_CENTER_FREQUENCY_HZ: f64 = 100_000_000.0;

// Discriminant constants for `last_rtl_tcp_state_disc`. Matches
// the variant ordering of `sdr_types::RtlTcpConnectionState` —
// not a wire contract (we never serialize these), just a stable
// `u8` representation so `Cell::set` works across the enum's
// non-Copy variants. Per #396.
pub const RTL_TCP_STATE_DISC_DISCONNECTED: u8 = 0;
pub const RTL_TCP_STATE_DISC_CONNECTING: u8 = 1;
pub const RTL_TCP_STATE_DISC_CONNECTED: u8 = 2;
pub const RTL_TCP_STATE_DISC_RETRYING: u8 = 3;
pub const RTL_TCP_STATE_DISC_FAILED: u8 = 4;
pub const RTL_TCP_STATE_DISC_CONTROLLER_BUSY: u8 = 5;
pub const RTL_TCP_STATE_DISC_AUTH_REQUIRED: u8 = 6;
pub const RTL_TCP_STATE_DISC_AUTH_FAILED: u8 = 7;

/// Project an `RtlTcpConnectionState` to its `u8` discriminant
/// for use in the edge-detection path. Kept as a free function
/// so callers don't have to reach into the enum's internal
/// representation. Per #396.
pub fn rtl_tcp_state_discriminant(state: &sdr_types::RtlTcpConnectionState) -> u8 {
    match state {
        sdr_types::RtlTcpConnectionState::Disconnected => RTL_TCP_STATE_DISC_DISCONNECTED,
        sdr_types::RtlTcpConnectionState::Connecting => RTL_TCP_STATE_DISC_CONNECTING,
        sdr_types::RtlTcpConnectionState::Connected { .. } => RTL_TCP_STATE_DISC_CONNECTED,
        sdr_types::RtlTcpConnectionState::Retrying { .. } => RTL_TCP_STATE_DISC_RETRYING,
        sdr_types::RtlTcpConnectionState::Failed { .. } => RTL_TCP_STATE_DISC_FAILED,
        sdr_types::RtlTcpConnectionState::ControllerBusy => RTL_TCP_STATE_DISC_CONTROLLER_BUSY,
        sdr_types::RtlTcpConnectionState::AuthRequired => RTL_TCP_STATE_DISC_AUTH_REQUIRED,
        sdr_types::RtlTcpConnectionState::AuthFailed => RTL_TCP_STATE_DISC_AUTH_FAILED,
    }
}

/// Shared application state, designed for single-threaded GTK main loop access.
///
/// Wrap in `Rc<AppState>` and clone into GTK closures.
pub struct AppState {
    /// Whether the DSP pipeline is currently running.
    pub is_running: Cell<bool>,
    /// Current center frequency in Hz.
    pub center_frequency: Cell<f64>,
    /// Latest VFO offset (Hz) the DSP is known to hold. Updated
    /// from the [`spectrum::SpectrumHandle::connect_vfo_offset_changed`]
    /// callback (which fires on both DSP echo via
    /// `DspToUi::VfoOffsetChanged` AND direct user-drag dispatches),
    /// so it stays in sync regardless of which path produced the
    /// change.
    ///
    /// Read by [`crate::doppler_tracker`]'s wiring in `window.rs`
    /// to gate its rate-limited `SetVfoOffset` dispatches —
    /// without this echo-driven baseline, an external write
    /// (spectrum drag, auto-record AOS reset) would leave the
    /// tracker's local "last dispatched" value stale and the
    /// next Doppler dispatch could be falsely suppressed by
    /// `DOPPLER_DISPATCH_THRESHOLD_HZ`. Per CR round 7 on PR
    /// #554.
    pub last_dispatched_vfo_offset_hz: Cell<f64>,
    /// Current demodulation mode.
    pub demod_mode: Cell<DemodMode>,
    /// Sender for dispatching commands to the DSP thread.
    pub ui_tx: mpsc::Sender<UiToDsp>,
    /// Re-entrancy guard for programmatic `bandwidth_row.set_value`
    /// calls from the `DspToUi::BandwidthChanged` handler. Toggled
    /// true before the `set_value`, cleared after, and checked in
    /// the spin row's `connect_value_notify` handler — prevents a DSP-
    /// originated bandwidth update (e.g. from a VFO drag) from
    /// bouncing back as a redundant `UiToDsp::SetBandwidth` dispatch
    /// which would in turn re-emit `BandwidthChanged` and waste a
    /// round trip per UI reflection.
    pub suppress_bandwidth_notify: Cell<bool>,
    /// Mirror of `suppress_bandwidth_notify` for the demod dropdown.
    /// Set true when we're programmatically changing the selected
    /// demod mode (e.g. the scanner `ScannerActiveChannelChanged`
    /// fan-out) so the dropdown's `connect_selected_notify` doesn't
    /// bounce a `UiToDsp::SetDemodMode` command back to DSP and
    /// accidentally tear down the scanner-driven retune the UI is
    /// only trying to reflect.
    pub suppress_demod_notify: Cell<bool>,
    /// Scanner's currently-active channel key (or `None` when
    /// scanner is Idle / Retuning). Written by the
    /// `ScannerActiveChannelChanged` fan-out in `handle_dsp_message`
    /// and read by the lockout button's click handler so a lockout
    /// click targets whichever channel the scanner latched onto
    /// most recently. `RefCell` rather than `Cell` because
    /// `ChannelKey` owns a `String` — `Cell::set` would require
    /// moving the stored value out, which interferes with the
    /// borrow-and-clone pattern the button handler uses.
    pub scanner_active_key: RefCell<Option<sdr_scanner::ChannelKey>>,
    /// Channel hop buffered for lazy emission of a transcript
    /// channel-marker (#517). Written by the
    /// `DspToUi::ScannerActiveChannelChanged` handler when the
    /// scanner switches to a non-idle channel; consumed by the
    /// `TranscriptionEvent::Text` handler when the next
    /// transcribed text arrives. The lazy approach skips marker
    /// emission entirely when (a) transcription is OFF (no
    /// `TranscriptionEvent::Text` ever fires, so the buffered
    /// hop stays unconsumed), and (b) the scanner hops past a
    /// channel without producing any audio (the next channel
    /// overwrites the buffered hop before it's consumed).
    /// Squashes runs of empty-channel hops to a single marker
    /// at the next channel that actually produces text.
    ///
    /// Stored as `(switched_at, channel_name)` so the marker
    /// renders the actual hop time rather than render time —
    /// otherwise a busy transcription backend with seconds of
    /// buffered audio would stamp markers with a clock that
    /// drifts past the real channel switch. Per `CodeRabbit`
    /// round 1 on PR #558.
    pub pending_channel_marker: RefCell<Option<(chrono::DateTime<chrono::Local>, String)>>,
    /// Previous `rtl_tcp` connection state discriminant, used to
    /// detect edge transitions into terminal role-denial states
    /// (`ControllerBusy`, `AuthRequired`, `AuthFailed`). The toast
    /// UX only wants to fire ONCE per entry into each state, not
    /// on every poll tick that re-publishes the same state. `u8`
    /// discriminant instead of the full enum so `Cell::set` works
    /// (the full enum carries `String` fields that defeat Copy).
    /// Per issue #396.
    pub last_rtl_tcp_state_disc: Cell<u8>,
    /// Host:port string for the currently-selected `rtl_tcp`
    /// server, kept in lockstep with the UI's `hostname_row` +
    /// `port_row`. Captured on every successful `AuthRequired` /
    /// `AuthFailed` transition so a subsequent successful
    /// `Connected` can save the user-entered key to the right
    /// per-server keyring entry. Empty string when no server is
    /// selected. Per issue #396.
    pub rtl_tcp_active_server: RefCell<String>,
    /// `true` while `apply_rtl_tcp_connect` (or the matching
    /// startup hydration in `connect_rtl_tcp_discovery`) is
    /// programmatically rewriting the shared
    /// `hostname_row` / `port_row` / `protocol_row` triple to
    /// point at an RTL-TCP server. The change-notify handlers
    /// for those rows always dispatch `SetNetworkConfig` (so the
    /// running session re-points), but they MUST NOT persist the
    /// values to `KEY_SOURCE_NETWORK_*` while this flag is set —
    /// the user's independent raw-network selection lives in
    /// those keys and would otherwise be silently overwritten by
    /// every RTL-TCP hydration. Per `CodeRabbit` round 1 on PR
    /// #558.
    pub rtl_tcp_hydration_in_progress: std::cell::Cell<bool>,
    /// Currently-open NOAA APT viewer window, or `None` when no
    /// viewer is open. Set by the viewer's open path (the
    /// activity-bar / shortcut handler) and cleared by its
    /// `close-request` signal. The `DspToUi::AptLine` handler in
    /// `handle_dsp_message` routes incoming lines here — when
    /// `None`, the lines are dropped (the decoder runs anyway, but
    /// nothing is displayed). Per epic #468 / ticket #482.
    ///
    /// `RefCell<Option<AptImageView>>` rather than
    /// `RefCell<Option<glib::WeakRef<…>>>` because `AptImageView`
    /// is internally `Rc`-shared already, and we want the line
    /// router to fail closed (drop the line) when the viewer's
    /// `close-request` fires, not when the `GObject`'s last strong
    /// ref happens to drop.
    pub apt_viewer: RefCell<Option<crate::apt_viewer::AptImageView>>,
    /// Weak handle to the open APT viewer window. Populated by
    /// [`crate::apt_viewer::open_apt_viewer_if_needed`]; cleared
    /// by the window's `close-request` handler alongside
    /// `apt_viewer`. Auto-record's LOS save path uses this to
    /// `.close()` the viewer after the PNG export finishes so
    /// the next pass starts with a fresh viewer instead of
    /// stale lines from the prior pass. Mirrors the
    /// `lrpt_viewer_window` weak-ref pattern. Per a user
    /// request during PR #554 live testing.
    pub apt_viewer_window: RefCell<Option<gtk4::glib::WeakRef<libadwaita::Window>>>,
    /// `(satellite_norad_id, aos_time)` for the currently-recording
    /// APT pass, or `None` when no auto-record is in flight. Set by
    /// the `RecorderAction::StartAutoRecord` wiring at AOS, cleared
    /// after `RecorderAction::SavePng` consumes it. Used to compute
    /// the rotate-180 flag for image export — the wiring layer needs
    /// the satellite + AOS time to evaluate `sdr_sat::is_ascending`,
    /// and the recorder's `Action::SavePng(PathBuf)` doesn't carry
    /// that. Stored as the stable NORAD id (not the display name)
    /// so a catalog rename doesn't silently break the rotation
    /// lookup. Per B2 of the noaa-apt parity work + CR round 2 on
    /// PR #571.
    pub apt_recording_pass: RefCell<Option<(u32, chrono::DateTime<chrono::Utc>)>>,
    /// `true` when the user closes the window the app should hide
    /// instead of exiting. Default `true` — set by `build_window`
    /// from the persisted config (key `close_to_tray`). Per #512.
    pub close_to_tray: Cell<bool>,
    /// `true` once the user has hidden the window at least once with
    /// the close button — used to fire the "App still running in
    /// tray …" toast exactly once per fresh config. Per #512.
    pub tray_first_close_seen: Cell<bool>,
    /// `true` while the tray service is alive and registered with
    /// the session bus. Defaults `true` (optimistic) and is flipped
    /// to `false` if `sdr_tray::spawn` returns Err. The close-request
    /// handler short-circuits to `Propagation::Proceed` when this is
    /// false. Per #512.
    pub tray_available: Cell<bool>,
    /// `true` while a `StartAudioRecording` is in flight. Used by
    /// `AppState::is_recording` to gate the tray-Quit confirmation.
    /// Per #512.
    pub audio_recording_active: Cell<bool>,
    /// `true` while a `StartIqRecording` is in flight. Per #512.
    pub iq_recording_active: Cell<bool>,
    /// `(satellite_norad_id, aos_time)` for the currently-recording
    /// LRPT pass, or `None` between passes. Set by the
    /// `RecorderAction::StartAutoRecord` LRPT arm at AOS, cleared
    /// after `RecorderAction::SaveLrptPass` completes its async
    /// composite + per-APID PNG export — but only if the slot
    /// still holds the same pass we entered the LOS export with.
    /// Mirrors the [`Self::apt_recording_pass`] compare-and-clear
    /// pattern from PR #571 round 4: with composite work added to
    /// LOS, the export window grew long enough that pass N+1 can
    /// AOS while pass N is still encoding. Without the snapshot +
    /// compare guard, pass N's completion would clobber the new
    /// pass's slot to `None` and `is_recording()` would lie about
    /// the in-flight pass. Per CR round 2 on PR #575.
    pub lrpt_recording_pass: RefCell<Option<(u32, chrono::DateTime<chrono::Utc>)>>,
    /// Owned handle to the tray service. Held in `AppState` so the
    /// `tray-quit` action can `shutdown()` to join the worker thread
    /// before `app.release()`. Per #512.
    pub tray_handle: RefCell<Option<sdr_tray::TrayHandle>>,
    /// RAII guard for `app.hold()`. The gio binding turns hold/release
    /// into a guard whose `Drop` calls `release()`. Stash it here so
    /// the application keeps running across last-window-close — the
    /// `tray-quit` action (added in CT-11) takes + drops it to
    /// trigger the natural shutdown. Per #512.
    pub app_hold_guard: RefCell<Option<gtk4::gio::ApplicationHoldGuard>>,
    /// Currently-open Meteor-M LRPT viewer window, or `None`
    /// when no viewer is open. Same lifecycle pattern as
    /// `apt_viewer` above. Per epic #469 task 7.
    pub lrpt_viewer: RefCell<Option<crate::lrpt_viewer::LrptImageView>>,
    /// Weak handle to the open LRPT viewer window so the
    /// `app.lrpt-open` action can `present()` (raise) an
    /// already-open-but-buried viewer instead of being a
    /// silent no-op. Cleared by the viewer's `close-request`
    /// alongside `lrpt_viewer`. Weak rather than strong so the
    /// `AppState` slot doesn't keep the window alive past its
    /// natural lifetime (the GTK toplevel registry owns the
    /// strong ref). Per `CodeRabbit` round 13 on PR #543.
    pub lrpt_viewer_window: RefCell<Option<gtk4::glib::WeakRef<libadwaita::Window>>>,
    /// Long-lived shared image handle for the LRPT decoder /
    /// viewer. Allocated once per process — every pass reuses
    /// the same handle so the open viewer's poll tick keeps
    /// reading from the same `Arc<Mutex<…>>` even as the DSP
    /// thread tears down + re-inits the per-pass decoder.
    /// Cleared between passes via `LrptImage::clear` rather
    /// than reconstructed.
    pub lrpt_image: sdr_radio::lrpt_image::LrptImage,
    /// ACARS toggle (mirrors persisted `acars_enabled`).
    pub acars_enabled: Cell<bool>,
    /// Bounded ring of recent decoded messages. Cap is set
    /// from `acars_recent_keep_count` config (default 500).
    pub acars_recent: RefCell<VecDeque<AcarsMessage>>,
    /// Cumulative decoded-message count since toggle-on.
    /// Reset by `SetAcarsEnabled(true)` — gives the UI a
    /// running counter without scanning the bounded ring.
    pub acars_total_count: Cell<u64>,
    /// Latest per-channel stats, populated by the
    /// `DspToUi::AcarsChannelStats` arm. Defaulted on init.
    pub acars_channel_stats: RefCell<[ChannelStats; US_SIX_CHANNEL_COUNT]>,
    /// Mirror of the DSP-side snapshot, populated when the
    /// engage ack arrives. Lets the UI display "restoring
    /// to `{prior_freq}`" hints on disengage.
    pub acars_pre_lock_state: RefCell<Option<PreLockSnapshot>>,
    /// Currently-open ACARS viewer window, or `None` when no
    /// viewer is open. `glib::WeakRef` so the `AppState` slot
    /// doesn't keep the window alive past its natural
    /// lifetime. Set by [`crate::acars_viewer::open_acars_viewer_if_needed`];
    /// cleared by the window's `close-request` handler.
    pub acars_viewer_window: RefCell<Option<gtk4::glib::WeakRef<libadwaita::Window>>>,
    /// Per-viewer mutable handles (column-view store, filter,
    /// status label, etc). `Some` only while a viewer window
    /// is open. Set by `acars_viewer::build_acars_viewer_window`;
    /// cleared by the window's close-request handler alongside
    /// `acars_viewer_window`. Held in `Rc` so the close-request
    /// closure and the message-append site in `window.rs` can
    /// both reach it without lifetime juggling.
    pub acars_viewer_handles: RefCell<Option<Rc<crate::acars_viewer::ViewerHandles>>>,
    /// Pre-engage center frequency (Hz), captured by the
    /// `AcarsEnabledChanged(Ok(true))` arm so the disengage path
    /// can restore the header frequency selector display. The
    /// DSP retunes silently on engage/disengage (no `Tune` ack),
    /// so the UI has to remember the snapshot itself. `None`
    /// when ACARS is disengaged.
    pub acars_saved_freq_hz: Cell<Option<u64>>,
}

impl AppState {
    /// Create a new `AppState` wrapped in `Rc` for GTK closure sharing.
    ///
    /// The `ui_tx` sender is used to dispatch commands to the DSP thread.
    pub fn new_shared(ui_tx: mpsc::Sender<UiToDsp>) -> Rc<Self> {
        Rc::new(Self {
            is_running: Cell::new(false),
            center_frequency: Cell::new(DEFAULT_CENTER_FREQUENCY_HZ),
            last_dispatched_vfo_offset_hz: Cell::new(0.0),
            demod_mode: Cell::new(DemodMode::Wfm),
            ui_tx,
            suppress_bandwidth_notify: Cell::new(false),
            suppress_demod_notify: Cell::new(false),
            scanner_active_key: RefCell::new(None),
            pending_channel_marker: RefCell::new(None),
            // Initialize to `RTL_TCP_STATE_DISC_DISCONNECTED` — same
            // as the connection manager's initial state so the first
            // real transition into ControllerBusy / AuthRequired /
            // AuthFailed is correctly detected as an edge.
            last_rtl_tcp_state_disc: Cell::new(RTL_TCP_STATE_DISC_DISCONNECTED),
            rtl_tcp_active_server: RefCell::new(String::new()),
            rtl_tcp_hydration_in_progress: std::cell::Cell::new(false),
            apt_viewer: RefCell::new(None),
            apt_viewer_window: RefCell::new(None),
            apt_recording_pass: RefCell::new(None),
            close_to_tray: Cell::new(true),
            tray_first_close_seen: Cell::new(false),
            tray_available: Cell::new(true),
            audio_recording_active: Cell::new(false),
            iq_recording_active: Cell::new(false),
            lrpt_recording_pass: RefCell::new(None),
            tray_handle: RefCell::new(None),
            app_hold_guard: RefCell::new(None),
            lrpt_viewer: RefCell::new(None),
            lrpt_viewer_window: RefCell::new(None),
            lrpt_image: sdr_radio::lrpt_image::LrptImage::new(),
            acars_enabled: Cell::new(false),
            acars_recent: RefCell::new(VecDeque::with_capacity(
                crate::acars_config::default_recent_keep() as usize,
            )),
            acars_total_count: Cell::new(0),
            acars_channel_stats: RefCell::new([ChannelStats::default(); US_SIX_CHANNEL_COUNT]),
            acars_pre_lock_state: RefCell::new(None),
            acars_viewer_window: RefCell::new(None),
            acars_viewer_handles: RefCell::new(None),
            acars_saved_freq_hz: Cell::new(None),
        })
    }

    /// Send a command to the DSP thread, logging on failure.
    pub fn send_dsp(&self, msg: UiToDsp) {
        if let Err(e) = self.ui_tx.send(msg) {
            tracing::warn!("failed to send DSP command: {e}");
        }
    }

    /// `true` if the app is actively writing pass artifacts to disk —
    /// any APT pass, LRPT pass, audio recording, or IQ recording.
    /// Used to gate the tray-Quit confirmation modal.
    ///
    /// Maintenance contract: every new "we're writing pass artifacts"
    /// state added to `AppState` MUST be OR-ed in here, and the
    /// table-driven test in `is_recording_table` must be extended.
    /// Otherwise a future recording type can be silently dropped on Quit.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.apt_recording_pass.borrow().is_some()
            || self.lrpt_recording_pass.borrow().is_some()
            || self.audio_recording_active.get()
            || self.iq_recording_active.get()
    }

    /// Dispatch `UiToDsp::SetVfoOffset(hz)` AND synchronously
    /// update [`Self::last_dispatched_vfo_offset_hz`] in the
    /// same call. Use this for every programmatic VFO-offset
    /// dispatch (auto-record AOS reset, LOS `RestoreTune`, mode-
    /// change reset, Doppler tracker sends, etc.) so the
    /// Doppler dispatch-baseline cell stays in sync without
    /// waiting for the `DspToUi::VfoOffsetChanged` echo to
    /// round-trip through the controller.
    ///
    /// The `connect_vfo_offset_changed` callback in `window.rs`
    /// also writes the cell on echo (and on direct
    /// spectrum-widget drag dispatches that update the spectrum
    /// locally), so this helper's optimistic write is reconciled
    /// with the actual applied value when the echo lands —
    /// matching values overwrite harmlessly; clamped or
    /// rejected values overwrite to truth. Per CR round 10 on
    /// PR #554.
    pub fn dispatch_vfo_offset(&self, hz: f64) {
        self.last_dispatched_vfo_offset_hz.set(hz);
        self.send_dsp(UiToDsp::SetVfoOffset(hz));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_state() -> Rc<AppState> {
        let (tx, _rx) = mpsc::channel();
        AppState::new_shared(tx)
    }

    #[test]
    fn test_default_state() {
        let state = make_test_state();
        assert!(!state.is_running.get());
        assert!((state.center_frequency.get() - DEFAULT_CENTER_FREQUENCY_HZ).abs() < f64::EPSILON);
        assert_eq!(state.demod_mode.get(), DemodMode::Wfm);
    }

    #[test]
    fn acars_defaults_pin_initializer_contract() {
        // Pin the ACARS field defaults so a future regression
        // (e.g. changing the keep-count config helper, swapping
        // the ChannelStats default, or accidentally pre-loading
        // a snapshot) fails this test instead of silently
        // shipping a UI that mis-states ACARS state. Per
        // CodeRabbit round 3 on PR #584.
        let state = make_test_state();
        assert!(!state.acars_enabled.get(), "ACARS toggle defaults off");
        assert_eq!(state.acars_total_count.get(), 0, "no decoded messages yet");
        let recent = state.acars_recent.borrow();
        assert!(recent.is_empty(), "ring is empty on init");
        // `VecDeque::with_capacity(n)` guarantees AT LEAST n —
        // the allocator may round up. Pin the lower bound rather
        // than exact equality so allocator-growth differences
        // across toolchains don't false-fail this test. Per CR
        // round 4 on PR #584.
        assert!(
            recent.capacity() >= crate::acars_config::default_recent_keep() as usize,
            "ring capacity sourced from acars_config::default_recent_keep (>= {}, got {})",
            crate::acars_config::default_recent_keep(),
            recent.capacity(),
        );
        drop(recent);
        assert_eq!(
            state.acars_channel_stats.borrow().len(),
            sdr_core::acars_airband_lock::US_SIX_CHANNEL_COUNT,
            "stats array width sourced from US_SIX_CHANNEL_COUNT"
        );
        assert!(
            state.acars_pre_lock_state.borrow().is_none(),
            "no snapshot until first engage"
        );
        assert!(
            state.acars_viewer_window.borrow().is_none(),
            "no viewer window until first open"
        );
        assert!(
            state.acars_saved_freq_hz.get().is_none(),
            "no saved pre-engage freq until first engage"
        );
        assert!(
            state.acars_viewer_handles.borrow().is_none(),
            "no viewer handles until first open"
        );
    }

    #[test]
    fn last_dispatched_vfo_offset_hz_defaults_to_zero() {
        // Pin the Doppler dispatch baseline default. Per CR
        // round 8 on PR #554 — without this regression test, a
        // future change to the seeded value would silently break
        // the rate-limit gate's "compare against actual current
        // DSP state" invariant. The first 4 Hz Doppler tick
        // computes `live - baseline`; if `baseline` starts at
        // anything but 0, that comparison is wrong until the
        // first echo from a real `SetVfoOffset` lands.
        let state = make_test_state();
        assert!(
            (state.last_dispatched_vfo_offset_hz.get() - 0.0).abs() < f64::EPSILON,
            "got {}",
            state.last_dispatched_vfo_offset_hz.get()
        );
    }

    #[test]
    fn test_state_mutation() {
        let state = make_test_state();
        state.is_running.set(true);
        state.center_frequency.set(144_000_000.0);
        state.demod_mode.set(DemodMode::Nfm);

        assert!(state.is_running.get());
        assert!((state.center_frequency.get() - 144_000_000.0).abs() < f64::EPSILON);
        assert_eq!(state.demod_mode.get(), DemodMode::Nfm);
    }

    #[test]
    fn test_send_dsp_with_dropped_receiver() {
        let (tx, rx) = mpsc::channel();
        let state = AppState::new_shared(tx);
        drop(rx);
        // Should not panic — just logs a warning.
        state.send_dsp(UiToDsp::Stop);
    }

    #[test]
    fn defaults_are_safe_for_close_to_tray() {
        let s = make_test_state();
        assert!(s.close_to_tray.get(), "default close_to_tray must be true");
        assert!(!s.tray_first_close_seen.get());
        assert!(s.tray_available.get());
        assert!(!s.audio_recording_active.get());
        assert!(!s.iq_recording_active.get());
        assert!(s.lrpt_recording_pass.borrow().is_none());
    }

    #[test]
    fn is_recording_is_false_when_idle() {
        let s = make_test_state();
        assert!(!s.is_recording());
    }

    #[test]
    fn is_recording_table() {
        // Each row: (apt, lrpt, audio, iq, expected)
        let cases = [
            (false, false, false, false, false),
            (true, false, false, false, true),
            (false, true, false, false, true),
            (false, false, true, false, true),
            (false, false, false, true, true),
            (true, true, true, true, true),
            (true, false, false, true, true),
            (false, true, true, false, true),
        ];
        for (apt, lrpt, audio, iq, expected) in cases {
            let s = make_test_state();
            if apt {
                *s.apt_recording_pass.borrow_mut() = Some((33_591, chrono::Utc::now()));
            }
            if lrpt {
                // NORAD 33_592 = NOAA 19 placeholder; matches the
                // shape `apt_recording_pass` uses above. Per CR
                // round 2 on PR #575.
                *s.lrpt_recording_pass.borrow_mut() = Some((33_592, chrono::Utc::now()));
            }
            s.audio_recording_active.set(audio);
            s.iq_recording_active.set(iq);
            assert_eq!(
                s.is_recording(),
                expected,
                "row apt={apt} lrpt={lrpt} audio={audio} iq={iq}",
            );
        }
    }

    #[test]
    fn rtl_tcp_state_discriminant_covers_all_variants() {
        // Lock-in test so a future `RtlTcpConnectionState`
        // variant reorder doesn't silently desync the
        // `RTL_TCP_STATE_DISC_*` u8 constants used by the
        // toast edge-detection path. The constants are
        // `Cell<u8>`-friendly projections of the enum's
        // variant ordering and must match 1:1. Per
        // CodeRabbit round 1 on PR #408.
        use std::time::Duration;
        assert_eq!(
            rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::Disconnected),
            RTL_TCP_STATE_DISC_DISCONNECTED
        );
        assert_eq!(
            rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::Connecting),
            RTL_TCP_STATE_DISC_CONNECTING
        );
        assert_eq!(
            rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::Connected {
                tuner_name: "R820T".into(),
                gain_count: 29,
                codec: "None".into(),
                granted_role: Some(true),
            }),
            RTL_TCP_STATE_DISC_CONNECTED
        );
        assert_eq!(
            rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::Retrying {
                attempt: 1,
                retry_in: Duration::from_secs(1),
            }),
            RTL_TCP_STATE_DISC_RETRYING
        );
        assert_eq!(
            rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::Failed {
                reason: "x".into(),
            }),
            RTL_TCP_STATE_DISC_FAILED
        );
        assert_eq!(
            rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::ControllerBusy),
            RTL_TCP_STATE_DISC_CONTROLLER_BUSY
        );
        assert_eq!(
            rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::AuthRequired),
            RTL_TCP_STATE_DISC_AUTH_REQUIRED
        );
        assert_eq!(
            rtl_tcp_state_discriminant(&sdr_types::RtlTcpConnectionState::AuthFailed),
            RTL_TCP_STATE_DISC_AUTH_FAILED
        );
    }
}
