//! Demodulation mode selector dropdown for the header bar.

use std::cell::Cell;
use std::rc::Rc;

use sdr_types::DemodMode;

/// Display labels for each demodulation mode, in dropdown order.
const DEMOD_LABELS: &[&str] = &["WFM", "NFM", "AM", "USB", "LSB", "DSB", "CW", "RAW"];

/// All demod modes in the same order as `DEMOD_LABELS`.
const DEMOD_MODES: &[DemodMode] = &[
    DemodMode::Wfm,
    DemodMode::Nfm,
    DemodMode::Am,
    DemodMode::Usb,
    DemodMode::Lsb,
    DemodMode::Dsb,
    DemodMode::Cw,
    DemodMode::Raw,
];

/// Default demod mode index (WFM = 0).
const DEFAULT_MODE_INDEX: u32 = 0;

/// Build a demodulation mode dropdown and its shared state cell.
///
/// Returns `(dropdown_widget, shared_demod_mode)`. The `Cell` is updated
/// whenever the user selects a different mode.
pub fn build_demod_selector() -> (gtk4::DropDown, Rc<Cell<DemodMode>>) {
    let model = gtk4::StringList::new(DEMOD_LABELS);
    let dropdown = gtk4::DropDown::builder()
        .model(&model)
        .selected(DEFAULT_MODE_INDEX)
        .tooltip_text("Demodulation mode")
        .build();

    let mode = Rc::new(Cell::new(DEMOD_MODES[DEFAULT_MODE_INDEX as usize]));

    let mode_clone = Rc::clone(&mode);
    dropdown.connect_selected_notify(move |dd| {
        let idx = dd.selected() as usize;
        if let Some(&m) = DEMOD_MODES.get(idx) {
            mode_clone.set(m);
            tracing::debug!(?m, "demod mode selected");
        }
    });

    (dropdown, mode)
}

/// Convert a `DemodMode` enum to its dropdown index.
///
/// Returns `None` if the mode is not in the list (should not happen).
#[allow(clippy::cast_possible_truncation)]
pub fn demod_mode_to_index(mode: DemodMode) -> Option<u32> {
    // DEMOD_MODES has 8 entries — well within u32 range.
    DEMOD_MODES
        .iter()
        .position(|&m| m == mode)
        .map(|i| i as u32)
}

/// Convert a dropdown index to a `DemodMode`.
///
/// Returns `None` if the index is out of range.
pub fn index_to_demod_mode(index: u32) -> Option<DemodMode> {
    DEMOD_MODES.get(index as usize).copied()
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    #[test]
    fn demod_labels_and_modes_same_length() {
        assert_eq!(DEMOD_LABELS.len(), DEMOD_MODES.len());
    }

    #[test]
    fn default_mode_is_wfm() {
        assert_eq!(DEMOD_MODES[DEFAULT_MODE_INDEX as usize], DemodMode::Wfm);
    }

    #[test]
    fn roundtrip_mode_index() {
        for (i, &mode) in DEMOD_MODES.iter().enumerate() {
            let idx = demod_mode_to_index(mode);
            assert_eq!(idx, Some(i as u32), "mode {mode:?} should map to index {i}");
            let back = index_to_demod_mode(i as u32);
            assert_eq!(back, Some(mode), "index {i} should map back to {mode:?}");
        }
    }

    #[test]
    fn out_of_range_index_returns_none() {
        assert!(index_to_demod_mode(99).is_none());
    }
}
