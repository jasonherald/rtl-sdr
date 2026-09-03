//! Distance Estimator section for the Radio panel build — the
//! rows, group, shared input caches, refresh wiring, and the
//! final page assembly that consumes them. Sub-split out of
//! `build.rs` per the 500-NLOC file gate (#819).

use std::cell::Cell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::prelude::*;

use super::super::distance::refresh_distance_display_standalone;

/// Shared cache of the most recent signal level (dB), threaded
/// from the DSP handler into the display refreshers.
type SignalCache = Rc<Cell<Option<f32>>>;
/// Shared cache of the most recent tuner frequency (Hz).
type FrequencyCache = Rc<Cell<Option<f64>>>;
use super::super::{
    CALIBRATION_PAGE_DB, CALIBRATION_STEP_DB, DEFAULT_CALIBRATION_DB, DEFAULT_ERP_WATTS,
    ERP_PAGE_WATTS, ERP_STEP_WATTS, MAX_CALIBRATION_DB, MAX_ERP_WATTS, MIN_CALIBRATION_DB,
    MIN_ERP_WATTS,
};

/// Everything the Distance Estimator section produces — rows,
/// the packed group, and the two shared input caches. Named per
/// the bundle convention (#819).
pub(super) struct DistanceSection {
    pub(super) erp_row: adw::SpinRow,
    pub(super) calibration_row: adw::SpinRow,
    pub(super) distance_row: adw::ActionRow,
    pub(super) group: adw::PreferencesGroup,
    pub(super) last_signal_db: Rc<Cell<Option<f32>>>,
    pub(super) last_frequency_hz: Rc<Cell<Option<f64>>>,
}

/// Distance Estimator section: rows, group, caches, and the
/// value-notify refresh wiring. Split out per the 50-NLOC gate
/// (#819).
pub(super) fn build_distance_section() -> DistanceSection {
    let (erp_row, calibration_row, distance_row) = build_distance_rows();
    let group = build_distance_group(&erp_row, &calibration_row, &distance_row);
    let (last_signal_db, last_frequency_hz) =
        wire_distance_refresh(&erp_row, &calibration_row, &distance_row);
    DistanceSection {
        erp_row,
        calibration_row,
        distance_row,
        group,
        last_signal_db,
        last_frequency_hz,
    }
}

/// ERP / calibration / distance-display rows for the FSPL
/// estimator. Split out per the 50-NLOC gate (#819).
fn build_distance_rows() -> (adw::SpinRow, adw::SpinRow, adw::ActionRow) {
    // --- Distance Estimator (FSPL, ticket #164) ---
    let erp_adj = gtk4::Adjustment::new(
        DEFAULT_ERP_WATTS,
        MIN_ERP_WATTS,
        MAX_ERP_WATTS,
        ERP_STEP_WATTS,
        ERP_PAGE_WATTS,
        0.0,
    );
    let erp_row = adw::SpinRow::builder()
        .title("Transmitter Power")
        .subtitle("Effective radiated power, in watts (handheld ~5, mobile ~25-50)")
        .adjustment(&erp_adj)
        .digits(3)
        .build();

    let cal_adj = gtk4::Adjustment::new(
        DEFAULT_CALIBRATION_DB,
        MIN_CALIBRATION_DB,
        MAX_CALIBRATION_DB,
        CALIBRATION_STEP_DB,
        CALIBRATION_PAGE_DB,
        0.0,
    );
    let calibration_row = adw::SpinRow::builder()
        .title("Receiver Calibration")
        .subtitle("dB offset applied to raw level before computing path loss")
        .adjustment(&cal_adj)
        .digits(1)
        .build();

    let distance_row = adw::ActionRow::builder()
        .title("Estimated Distance")
        .subtitle("—")
        .selectable(false)
        .activatable(false)
        .build();

    // The retained group is built (and the rows parented) in
    // `build_distance_group` — an earlier carve draft ALSO built a
    // throwaway group here, wastefully parenting the rows into a
    // widget that was immediately dropped (`CodeRabbit` round 1 on
    // PR #887).
    (erp_row, calibration_row, distance_row)
}

/// Distance Estimator group with its three rows. Split out of
/// [`build_radio_panel`] per the 50-NLOC gate (#819).
fn build_distance_group(
    erp_row: &adw::SpinRow,
    calibration_row: &adw::SpinRow,
    distance_row: &adw::ActionRow,
) -> adw::PreferencesGroup {
    let distance_group = adw::PreferencesGroup::builder()
        .title("Distance Estimator")
        .description(
            "Rough line-of-sight (FSPL) estimate — read as an upper bound, not precision ranging",
        )
        .build();
    distance_group.add(erp_row);
    distance_group.add(calibration_row);
    distance_group.add(distance_row);

    distance_group
}

/// Shared distance-input caches + the build-time value-notify
/// wiring that refreshes the display on ERP / calibration edits.
/// Split out per the 50-NLOC gate (#819).
fn wire_distance_refresh(
    erp_row: &adw::SpinRow,
    calibration_row: &adw::SpinRow,
    distance_row: &adw::ActionRow,
) -> (SignalCache, FrequencyCache) {
    // Internal state shared across the panel clone surface — see
    // the field docs on `RadioPanel` for why this is `Rc<Cell>`
    // rather than plain `Cell`.
    let distance_last_signal_db: Rc<Cell<Option<f32>>> = Rc::new(Cell::new(None));
    let distance_last_frequency_hz: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));

    // Wire ERP and calibration spin-row changes to trigger a
    // distance refresh using the cached signal/frequency. Config
    // persistence and any DSP plumbing that cares about these
    // values is wired separately in `window.rs` on the same
    // signal — both handlers run on value change.
    {
        let last_signal = Rc::clone(&distance_last_signal_db);
        let last_freq = Rc::clone(&distance_last_frequency_hz);
        let erp_row_for_signal = erp_row.clone();
        let cal_row_for_signal = calibration_row.clone();
        let distance_row_for_signal = distance_row.clone();
        let refresh = move || {
            refresh_distance_display_standalone(
                &erp_row_for_signal,
                &cal_row_for_signal,
                &distance_row_for_signal,
                last_signal.get(),
                last_freq.get(),
            );
        };
        let refresh_for_erp = refresh.clone();
        erp_row.connect_value_notify(move |_| refresh_for_erp());
        calibration_row.connect_value_notify(move |_| refresh());
    }

    (distance_last_signal_db, distance_last_frequency_hz)
}

/// Pack the six groups onto the page in the pre-split order.
/// Split out per the 50-NLOC gate (#819).
pub(super) fn assemble_page(
    bandwidth_group: &adw::PreferencesGroup,
    squelch_group: &adw::PreferencesGroup,
    filters_group: &adw::PreferencesGroup,
    deemphasis_group: &adw::PreferencesGroup,
    ctcss_group: &adw::PreferencesGroup,
    distance: &DistanceSection,
) -> adw::PreferencesPage {
    let distance_group = &distance.group;
    let page = adw::PreferencesPage::new();
    page.add(bandwidth_group);
    page.add(squelch_group);
    page.add(filters_group);
    page.add(deemphasis_group);
    page.add(ctcss_group);
    page.add(distance_group);

    wire_map_refresh(&page, distance);
    page
}

/// Map-signal refresh so switching to the Radio tab renders the
/// distance estimate from the cached inputs. Split out per the
/// 50-NLOC gate (#819).
fn wire_map_refresh(page: &adw::PreferencesPage, distance: &DistanceSection) {
    let erp_row = &distance.erp_row;
    let calibration_row = &distance.calibration_row;
    let distance_row = &distance.distance_row;
    let distance_last_signal_db = &distance.last_signal_db;
    let distance_last_frequency_hz = &distance.last_frequency_hz;
    // When the Radio tab becomes visible (user switches to it
    // via the activity bar), render the distance estimate with
    // whatever cached inputs are current. The DSP-driven
    // `update_distance_*` methods skip the render step while the
    // panel is unmapped — without this handler the user would see
    // a stale subtitle until the next SignalLevel message arrived.
    {
        let erp_for_map = erp_row.clone();
        let cal_for_map = calibration_row.clone();
        let dist_for_map = distance_row.clone();
        let last_signal_for_map = Rc::clone(distance_last_signal_db);
        let last_freq_for_map = Rc::clone(distance_last_frequency_hz);
        page.connect_map(move |_| {
            refresh_distance_display_standalone(
                &erp_for_map,
                &cal_for_map,
                &dist_for_map,
                last_signal_for_map.get(),
                last_freq_for_map.get(),
            );
        });
    }
}
