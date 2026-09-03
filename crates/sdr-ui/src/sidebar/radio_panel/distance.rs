//! FSPL distance-estimator display logic for the Radio panel —
//! the cached-input refresh methods, the standalone refresher the
//! build path wires to spin-row signals, and the
//! [`DistanceDisplay`] state machine. Split out of
//! `radio_panel.rs` per the file-size pass (issue #819).

use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_dsp::propagation::{fspl_distance_m, watts_to_dbm};

use super::{
    KM_THRESHOLD_M, MAX_MEANINGFUL_DISTANCE_M, MEGAMETRE_ROUND_KM, MEGAMETRE_THRESHOLD_M,
    METRES_PER_KM, NO_ACTIVE_SIGNAL_DBM, RadioPanel,
};

impl RadioPanel {
    pub fn update_distance_from_signal(&self, signal_db: f32, frequency_hz: f64) {
        self.distance_last_signal_db.set(Some(signal_db));
        self.distance_last_frequency_hz.set(Some(frequency_hz));
        if self.widget.is_mapped() {
            self.refresh_distance_display();
        }
    }

    /// Cache a new tuner frequency and recompute the distance
    /// estimate. Called from the tuner-change handler in
    /// `window.rs`. Same visibility-gating as
    /// [`Self::update_distance_from_signal`].
    pub fn update_distance_frequency(&self, frequency_hz: f64) {
        self.distance_last_frequency_hz.set(Some(frequency_hz));
        if self.widget.is_mapped() {
            self.refresh_distance_display();
        }
    }

    /// Recompute and render the distance display from the cached
    /// signal/frequency and the current ERP / calibration row
    /// values. Called by the setters above and by the ERP /
    /// calibration spin-row value-notify handlers.
    pub fn refresh_distance_display(&self) {
        let state = DistanceDisplay::compute(
            self.distance_last_signal_db.get(),
            self.distance_last_frequency_hz.get(),
            self.erp_row.value(),
            self.calibration_row.value(),
        );
        self.distance_row.set_subtitle(&state.format());
    }
}

/// Standalone version of [`RadioPanel::refresh_distance_display`]
/// usable from inside `build_radio_panel` before the `RadioPanel`
/// struct has been materialised — wired to the ERP / calibration
/// value-notify signals so a knob twiddle refreshes the display
/// immediately. Both variants route through [`DistanceDisplay`]
/// so the state machine stays in one place.
pub(super) fn refresh_distance_display_standalone(
    erp_row: &adw::SpinRow,
    calibration_row: &adw::SpinRow,
    distance_row: &adw::ActionRow,
    last_signal_db: Option<f32>,
    last_frequency_hz: Option<f64>,
) {
    let state = DistanceDisplay::compute(
        last_signal_db,
        last_frequency_hz,
        erp_row.value(),
        calibration_row.value(),
    );
    distance_row.set_subtitle(&state.format());
}

/// Distinct visual states the distance display can be in.
/// Split out so the logic is explicit and test-covered rather
/// than buried in a single formatter function that had to
/// overload "—" for several semantically different cases.
#[derive(Debug, PartialEq)]
pub(super) enum DistanceDisplay {
    /// No signal level has ever flowed yet — source not running
    /// or panel freshly constructed.
    NoData,
    /// Calibrated received level is below receiver sensitivity,
    /// so there is nothing real to measure. Typical cause:
    /// squelch gated, source pointed at an empty channel, or
    /// hardware disconnected.
    NoActiveSignal,
    /// Received level ≥ transmitted ERP — physically impossible
    /// under FSPL. The user has a calibration problem (receiver
    /// cal offset too large, or ERP set too low for the actual
    /// transmitter).
    CheckCalibration,
    /// A signal is present above the sensitivity threshold but
    /// path loss implies a distance greater than Earth's great-
    /// circle maximum — the estimator has saturated.
    TooWeak,
    /// Meaningful distance in metres, safe to print as a number.
    Value(f64),
}

impl DistanceDisplay {
    /// Decide the display state from the live inputs. All four
    /// fields-that-matter (last signal, last frequency, ERP,
    /// calibration offset) get threaded through explicitly so
    /// tests can pin every transition without constructing a
    /// full `RadioPanel`.
    pub(super) fn compute(
        signal_db: Option<f32>,
        frequency_hz: Option<f64>,
        erp_watts: f64,
        cal_db: f64,
    ) -> Self {
        let (Some(raw_signal_db), Some(freq)) = (signal_db, frequency_hz) else {
            return Self::NoData;
        };
        let received_dbm = f64::from(raw_signal_db) + cal_db;
        if !received_dbm.is_finite() || received_dbm < NO_ACTIVE_SIGNAL_DBM {
            return Self::NoActiveSignal;
        }
        let erp_dbm = watts_to_dbm(erp_watts);
        let d = fspl_distance_m(erp_dbm, received_dbm, freq);
        if !d.is_finite() || d < f64::EPSILON {
            // `fspl_distance_m` returns 0.0 when received ≥ ERP
            // (i.e., the user is receiving stronger than the
            // transmitter putatively radiates — only reachable
            // by miscalibrated inputs).
            return Self::CheckCalibration;
        }
        if d > MAX_MEANINGFUL_DISTANCE_M {
            return Self::TooWeak;
        }
        Self::Value(d)
    }

    /// Render the state as the subtitle text for the
    /// `distance_row`. Keeps wording in one place so changes
    /// don't drift between the panel helpers and test assertions.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(super) fn format(&self) -> String {
        match *self {
            Self::NoData => "—".to_string(),
            Self::NoActiveSignal => "No active signal".to_string(),
            Self::CheckCalibration => "Check calibration".to_string(),
            Self::TooWeak => "Too weak to measure".to_string(),
            Self::Value(d) => {
                if d < KM_THRESHOLD_M {
                    format!("{} m", d.round() as u64)
                } else if d < MEGAMETRE_THRESHOLD_M {
                    format!("{:.1} km", d / METRES_PER_KM)
                } else {
                    let km_rounded =
                        (d / METRES_PER_KM / MEGAMETRE_ROUND_KM).round() * MEGAMETRE_ROUND_KM;
                    format!("{km_rounded:.0} km")
                }
            }
        }
    }
}
