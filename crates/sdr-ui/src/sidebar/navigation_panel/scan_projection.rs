//! Bookmark-list → scanner-channel projection for the Navigation
//! panel — filtering to scan-enabled bookmarks, bandwidth clamping,
//! the axis-lock frequency envelope, and the project-and-dispatch
//! convenience. Split out of `navigation_panel.rs` per the
//! file-size pass (issue #819).

use crate::messages::UiToDsp;

use super::bookmarks::{Bookmark, parse_demod_mode};

// ---------------------------------------------------------------------------
// Scanner projection — bookmark list → `Vec<ScannerChannel>`
// ---------------------------------------------------------------------------

/// Project the in-memory bookmark list into the
/// [`sdr_scanner::ScannerChannel`] form the scanner state machine
/// expects. Filters to `scan_enabled == true`, parses the bookmark's
/// string-form demod mode, and folds per-channel dwell/hang overrides
/// on top of the UI-provided defaults (the scanner itself has no
/// notion of defaults — resolution happens at projection time).
///
/// Pure function: no I/O, no threading, no global state. The channel
/// list order mirrors the bookmark list order, which in turn mirrors
/// save order — keeping the scanner rotation predictable and under
/// user control.
#[must_use]
pub fn project_scanner_channels(
    bookmarks: &[Bookmark],
    default_dwell_ms: u32,
    default_hang_ms: u32,
) -> Vec<sdr_scanner::ScannerChannel> {
    bookmarks
        .iter()
        .filter(|b| b.scan_enabled)
        .map(|b| {
            let demod_mode = parse_demod_mode(&b.demod_mode);
            sdr_scanner::ScannerChannel {
                key: sdr_scanner::ChannelKey {
                    name: b.name.clone(),
                    frequency_hz: b.frequency,
                },
                demod_mode,
                bandwidth: clamp_bandwidth_for_mode(b.bandwidth, demod_mode, &b.name),
                ctcss: b.ctcss_mode,
                voice_squelch: b.voice_squelch_mode,
                priority: b.priority,
                dwell_ms: b.dwell_ms_override.unwrap_or(default_dwell_ms),
                hang_ms: b.hang_ms_override.unwrap_or(default_hang_ms),
            }
        })
        .collect()
}

/// Clamp a bookmark's bandwidth into the demod mode's legal range
/// before it reaches the scanner. The Radio panel clamps interactive
/// edits via the `SpinRow` range, but a bookmark saved under a
/// different mode (or edited on disk) can carry an out-of-range
/// value; the VFO would accept it while NFM ignores it, letting the
/// DSP state diverge from the UI. The normalized value flows to both
/// the `UpdateScannerChannels` dispatch and the axis-lock UI, which
/// consume this projection. Per CR round 2 on PR #844.
fn clamp_bandwidth_for_mode(bandwidth: f64, mode: sdr_types::DemodMode, name: &str) -> f64 {
    let (Ok(min_bw), Ok(max_bw)) = (
        sdr_radio::demod::min_bandwidth_for_mode(mode),
        sdr_radio::demod::max_bandwidth_for_mode(mode),
    ) else {
        // No defined range for this mode — pass through unchanged.
        return bandwidth;
    };
    let clamped = bandwidth.clamp(min_bw, max_bw);
    if (clamped - bandwidth).abs() > f64::EPSILON {
        tracing::warn!(
            name,
            ?mode,
            bandwidth,
            clamped,
            "scanner channel bandwidth out of range for mode; clamped"
        );
    }
    clamped
}

/// Compute the (min, max) frequency envelope of a scanner channel
/// list — the lower edge of the lowest-frequency channel's
/// passband to the upper edge of the highest-frequency channel's
/// passband, both in absolute Hz. Used by the scanner-axis-lock
/// (#516) to pin the spectrum/waterfall X axis to the full range
/// the scanner sweeps across.
///
/// Returns `None` if the channel list is empty (scanner has no
/// channels selected, so there's no meaningful range to lock).
/// Per issue #516.
#[must_use]
pub fn scanner_channel_envelope(channels: &[sdr_scanner::ScannerChannel]) -> Option<(f64, f64)> {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for ch in channels {
        // Skip malformed channels (NaN / Inf / non-positive
        // bandwidth) so a single bad bookmark can't poison the
        // envelope math and silently drop the lock.
        // `f64::min` / `f64::max` propagate NaN per IEEE 754,
        // so even one NaN bandwidth would taint `min`/`max`
        // and make the final `max > min` check fail (returning
        // `None` for an otherwise-valid channel set). Per
        // `CodeRabbit` round 1 on PR #562.
        if !ch.bandwidth.is_finite() || ch.bandwidth <= 0.0 {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let center = ch.key.frequency_hz as f64;
        if !center.is_finite() {
            continue;
        }
        let half_bw = ch.bandwidth / 2.0;
        min = min.min(center - half_bw);
        max = max.max(center + half_bw);
    }
    if max > min { Some((min, max)) } else { None }
}

/// Convenience: read the persisted default dwell/hang from config,
/// project the bookmark list into scanner channels, and dispatch
/// `UiToDsp::UpdateScannerChannels` so the running scanner picks up
/// the change on its next tick. Call sites are every bookmark-list
/// mutation (Add, Delete, RR import, scan-toggle, priority-toggle)
/// plus both default-slider notify handlers.
pub fn project_and_push_scanner_channels(
    bookmarks: &[Bookmark],
    state: &std::rc::Rc<crate::state::AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    let default_dwell_ms = crate::sidebar::scanner_panel::load_default_dwell_ms(config);
    let default_hang_ms = crate::sidebar::scanner_panel::load_default_hang_ms(config);
    let channels = project_scanner_channels(bookmarks, default_dwell_ms, default_hang_ms);
    state.send_dsp(UiToDsp::UpdateScannerChannels(channels));
}
