//! Application state shared across GTK closures.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;

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
            lrpt_viewer: RefCell::new(None),
            lrpt_viewer_window: RefCell::new(None),
            lrpt_image: sdr_radio::lrpt_image::LrptImage::new(),
        })
    }

    /// Send a command to the DSP thread, logging on failure.
    pub fn send_dsp(&self, msg: UiToDsp) {
        if let Err(e) = self.ui_tx.send(msg) {
            tracing::warn!("failed to send DSP command: {e}");
        }
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
