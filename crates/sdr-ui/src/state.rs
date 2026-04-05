//! Application state shared across GTK closures.

use std::cell::Cell;
use std::rc::Rc;

use sdr_types::DemodMode;

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
}

impl AppState {
    /// Create a new `AppState` wrapped in `Rc` for GTK closure sharing.
    pub fn new_shared() -> Rc<Self> {
        Rc::new(Self {
            is_running: Cell::new(false),
            center_frequency: Cell::new(DEFAULT_CENTER_FREQUENCY_HZ),
            demod_mode: Cell::new(DemodMode::Wfm),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_state() {
        let state = AppState::new_shared();
        assert!(!state.is_running.get());
        assert!((state.center_frequency.get() - DEFAULT_CENTER_FREQUENCY_HZ).abs() < f64::EPSILON);
        assert_eq!(state.demod_mode.get(), DemodMode::Wfm);
    }

    #[test]
    fn test_state_mutation() {
        let state = AppState::new_shared();
        state.is_running.set(true);
        state.center_frequency.set(144_000_000.0);
        state.demod_mode.set(DemodMode::Nfm);

        assert!(state.is_running.get());
        assert!((state.center_frequency.get() - 144_000_000.0).abs() < f64::EPSILON);
        assert_eq!(state.demod_mode.get(), DemodMode::Nfm);
    }
}
