//! ACARS viewer row helpers: duplicate-message collapse against the
//! recent window (#586), relative-age formatting, and the per-channel
//! status row renderer. Split out of `window/aviation.rs` per the
//! Codacy 500-NLOC file gate on PR #844.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use gtk4::subclass::prelude::ObjectSubclassIsExt;

use super::super::adw;

/// Walk the most recent rows of the viewer store backwards from
/// the end and check for a `(aircraft, mode, label, text)` key
/// match within `ACARS_COLLAPSE_WINDOW`. Returns the matched
/// row's index after bumping its count + `last_seen` in place,
/// or `None` if no in-window match — in which case the caller
/// appends a fresh row. Stops walking as soon as it sees a row
/// older than the recency window (rows are insertion-ordered
/// in the underlying store, oldest at index 0). Issue #586.
pub(in crate::window) fn try_collapse_into_existing(
    store: &gtk4::gio::ListStore,
    msg: &sdr_acars::AcarsMessage,
) -> Option<u32> {
    use gtk4::prelude::ListModelExt;
    let n = store.n_items();
    if n == 0 {
        return None;
    }
    let cutoff = msg
        .timestamp
        .checked_sub(crate::acars_viewer::ACARS_COLLAPSE_WINDOW)?;
    let mut idx = n;
    while idx > 0 {
        idx -= 1;
        let Some(item) = store.item(idx) else {
            continue;
        };
        let Some(obj) = item.downcast_ref::<crate::acars_viewer::AcarsMessageObject>() else {
            continue;
        };
        // Skip rows older than the recency window. We can't
        // early-exit here even though insertion order would
        // suggest later rows have even older `last_seen`:
        // `record_duplicate` updates an existing row's
        // `last_seen` IN PLACE (no store reorder), so the
        // "monotonic by index" invariant doesn't hold once any
        // collapse has fired. CR round 1 on PR #591.
        if obj.last_seen() < cutoff {
            continue;
        }
        let inner = obj.imp().inner.borrow();
        let Some(existing) = inner.as_ref() else {
            continue;
        };
        if existing.aircraft == msg.aircraft
            && existing.mode == msg.mode
            && existing.label == msg.label
            && existing.text == msg.text
        {
            // Drop the borrow before mutating via the public
            // API (which doesn't actually need the borrow held,
            // but keeping the scope tight is cleaner).
            drop(inner);
            obj.record_duplicate(msg.timestamp);
            return Some(idx);
        }
    }
    None
}

/// Format a `SystemTime` as a relative age string ("5s ago",
/// "2m ago", "1h ago"). Returns "—" if the timestamp is in the
/// future or unrepresentable.
pub(in crate::window) fn format_relative_age(ts: std::time::SystemTime) -> String {
    let Ok(elapsed) = ts.elapsed() else {
        return "—".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Paint one per-channel row: lock-state glyph + frequency title and
/// the msgs / level / age subtitle.
pub(super) fn render_acars_channel_row(row: &adw::ActionRow, ch: &sdr_acars::ChannelStats) {
    use crate::sidebar::aviation_panel::{GLYPH_IDLE, GLYPH_LOCKED, GLYPH_SIGNAL};
    use sdr_acars::ChannelLockState;

    let glyph = match ch.lock_state {
        ChannelLockState::Locked => GLYPH_LOCKED,
        ChannelLockState::Idle => GLYPH_IDLE,
        ChannelLockState::Signal => GLYPH_SIGNAL,
    };
    row.set_title(&format!("{glyph}  {:.3} MHz", ch.freq_hz / 1_000_000.0));
    row.set_subtitle(&format!(
        "{} msgs · {:.1} dB · {}",
        ch.msg_count,
        ch.level_db,
        ch.last_msg_at
            .map_or_else(|| "—".to_string(), format_relative_age)
    ));
}
