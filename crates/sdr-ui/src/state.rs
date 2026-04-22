//! Application state shared across GTK closures.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;

use sdr_types::DemodMode;

use crate::messages::UiToDsp;

/// Default center frequency in Hz (100 MHz — FM broadcast band).
const DEFAULT_CENTER_FREQUENCY_HZ: f64 = 100_000_000.0;

/// Shared application state, designed for single-threaded GTK main loop access.
///
/// Wrap in `Rc<AppState>` and clone into GTK closures.
pub struct AppState {
    /// Whether the DSP pipeline is currently running.
    pub is_running: Cell<bool>,
    /// Current center frequency in Hz.
    pub center_frequency: Cell<f64>,
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
}

impl AppState {
    /// Create a new `AppState` wrapped in `Rc` for GTK closure sharing.
    ///
    /// The `ui_tx` sender is used to dispatch commands to the DSP thread.
    pub fn new_shared(ui_tx: mpsc::Sender<UiToDsp>) -> Rc<Self> {
        Rc::new(Self {
            is_running: Cell::new(false),
            center_frequency: Cell::new(DEFAULT_CENTER_FREQUENCY_HZ),
            demod_mode: Cell::new(DemodMode::Wfm),
            ui_tx,
            suppress_bandwidth_notify: Cell::new(false),
            suppress_demod_notify: Cell::new(false),
            scanner_active_key: RefCell::new(None),
        })
    }

    /// Send a command to the DSP thread, logging on failure.
    pub fn send_dsp(&self, msg: UiToDsp) {
        if let Err(e) = self.ui_tx.send(msg) {
            tracing::warn!("failed to send DSP command: {e}");
        }
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
}
