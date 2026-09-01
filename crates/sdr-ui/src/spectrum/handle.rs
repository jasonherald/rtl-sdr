//! The spectrum display's data-push handle (issue #819):
//! [`SpectrumHandle`] is what the wiring layer holds to feed FFT
//! frames / signal levels into the display and to drive dB range,
//! colormap, averaging, VFO reflection, and the scanner X-axis
//! lock ([`ScannerAxisLock`]). Split out of `spectrum/mod.rs` per
//! the file-size pass.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;

use super::vfo_overlay::VfoState;
use super::{
    AveragingMode, CursorCallback, FftPlotState, LockedClickCallback, MIN_DISPLAY_BANDWIDTH_HZ,
    SignalHistoryState, VfoOffsetCallback, WaterfallState, colormap, waterfall,
};
use crate::spectrum::AVERAGING_ALPHA;

/// Handle for pushing FFT data into the spectrum display from outside.
///
/// Obtained from `build_spectrum_view` and used by the `DspToUi::FftData`
/// handler to update both the FFT plot and waterfall with real DSP data.
pub struct SpectrumHandle {
    // Fields are `pub(super)` (not private) because
    // `build::build_spectrum_view` constructs this struct with a
    // literal from its sibling module post-#819-split; nothing
    // outside `crate::spectrum` can see them.
    pub(super) fft_state: Rc<RefCell<Option<FftPlotState>>>,
    pub(super) waterfall_state: Rc<RefCell<Option<WaterfallState>>>,
    pub(super) signal_history_state: Rc<RefCell<Option<SignalHistoryState>>>,
    pub(super) vfo_state: Rc<RefCell<VfoState>>,
    pub(super) fft_area: gtk4::DrawingArea,
    pub(super) waterfall_area: gtk4::DrawingArea,
    pub(super) signal_history_area: gtk4::DrawingArea,
    pub(super) min_db: Rc<Cell<f32>>,
    pub(super) max_db: Rc<Cell<f32>>,
    pub(super) fill_enabled: Rc<Cell<bool>>,
    pub(super) averaging_mode: Rc<Cell<AveragingMode>>,
    pub(super) avg_buffer: Rc<RefCell<Vec<f32>>>,
    pub(super) cursor_callback: CursorCallback,
    /// Callback invoked when the VFO offset changes from user interaction
    /// (click-to-tune or drag). Used by `window.rs` to update the frequency
    /// display and status bar.
    pub(super) vfo_offset_callback: VfoOffsetCallback,
    /// Callback invoked on click in scanner-locked mode, with the
    /// absolute frequency under the click. Lets the wiring layer
    /// force-disable the scanner and tune absolutely without
    /// `attach_click_gesture` needing direct access to
    /// `ScannerForceDisable`. Per issue #563.
    pub(super) locked_click_callback: LockedClickCallback,
    /// Full (unzoomed) FFT bandwidth in Hz, set by `set_display_bandwidth()`.
    /// Used by the FFT plot and waterfall renderers for zoom mapping.
    pub(super) full_bandwidth: Rc<Cell<f64>>,
    /// Tuner center frequency in Hz (for absolute frequency labels).
    pub(super) center_freq: Rc<Cell<f64>>,
    /// Scanner-mode X-axis lock. `Some` while the scanner is
    /// active — pins the spectrum + waterfall to a wide band
    /// covering all scanner channels' downlink frequencies, so
    /// retunes between channels don't recentre the X axis on
    /// every hop. The narrow FFT data the dongle actually
    /// produces gets projected into the active channel's slice
    /// of the wide range; unsampled bands render as dark grey
    /// noise floor instead of being skipped. Active channel is
    /// highlighted with a vertical band so the user can read
    /// "where in the band is the scanner picking things up?"
    /// at a glance. Per issue #516.
    pub(super) scanner_axis_lock: Rc<RefCell<Option<ScannerAxisLock>>>,
    /// Floating "Reset VFO" button overlaid on the top-right of
    /// the spectrum area. Visibility is driven by window.rs:
    /// visible when bandwidth ≠ mode default OR vfo offset ≠ 0.
    /// `window.rs` also wires the click handler (needs access to
    /// `AppState` to compute the mode's default bandwidth).
    /// Per issue #341.
    pub vfo_reset_button: gtk4::Button,
}

/// Snapshot of the scanner X-axis lock — what frequency range
/// the spectrum / waterfall is pinned to, and which channel is
/// currently being sampled (if any).
///
/// Lifecycle:
/// 1. `enter_scanner_mode(min, max)` — wiring layer pushes the
///    union of all scanner-channel downlink frequencies. The
///    lock is `Some` from this point until `exit_scanner_mode`,
///    so the X axis stays pinned even while the scanner is
///    between channels.
/// 2. `set_scanner_active_channel(freq, bw)` — wiring layer
///    pushes the current active-channel context on every
///    `ScannerActiveChannelChanged` event. Drives the
///    highlight-band overlay and the FFT data projection.
/// 3. `exit_scanner_mode()` — wiring layer clears the lock when
///    the scanner is disabled / hits empty rotation / a manual
///    tune supersedes it. The X axis reverts to the normal
///    "current channel ± half BW" view.
///
/// Per issue #516.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScannerAxisLock {
    /// Lower bound of the locked X axis, absolute Hz.
    pub min_hz: f64,
    /// Upper bound of the locked X axis, absolute Hz.
    pub max_hz: f64,
    /// Centre frequency of the channel the scanner is currently
    /// sampling, absolute Hz. `None` between
    /// `enter_scanner_mode` and the first
    /// `set_scanner_active_channel` call (e.g. the first retune
    /// hasn't completed yet).
    pub active_channel_hz: Option<f64>,
    /// Channel filter bandwidth of the active channel, in Hz.
    /// Used to size the highlight band and project the narrow
    /// FFT bins into the correct slice of the wide range.
    /// Always paired with `active_channel_hz` — both `Some` or
    /// both `None`.
    pub active_channel_bw_hz: Option<f64>,
}

impl SpectrumHandle {
    /// Push a new FFT frame into both the FFT plot and waterfall display.
    ///
    /// Applies the current averaging mode before storing into the display buffer.
    /// Call this from the GTK main loop when `DspToUi::FftData` arrives.
    pub fn push_fft_data(&self, data: &[f32]) {
        // Apply averaging, then update FFT plot data.
        if let Some(s) = self.fft_state.borrow_mut().as_mut() {
            let mode = self.averaging_mode.get();
            let mut avg = self.avg_buffer.borrow_mut();
            apply_averaging(mode, data, &mut avg, &mut s.current_data);

            // NOTE: no display-side fftshift here. The DSP pipeline
            // (`crates/sdr-pipeline/src/iq_frontend.rs::compute_fft`)
            // now shifts the FFT output before publishing so both
            // GTK and the macOS Metal renderer see the natural
            // ordering [-Nyquist … DC … +Nyquist]. A display-side
            // shift on top of that double-shifts the buffer and
            // splits signals to both edges.
        }
        self.fft_area.queue_draw();

        // Push a new line to the waterfall. Auto-resize the
        // waterfall when the FFT size changes — driven by the
        // first matching-size frame rather than synchronously from
        // the UI, avoiding races with queued old-size frames. No
        // display-side fftshift (see note above on the FFT plot
        // branch).
        if let Some(s) = self.waterfall_state.borrow_mut().as_mut() {
            let target_width = waterfall::supported_texture_width_for(data.len());
            if target_width != s.renderer.texture_width() {
                s.renderer.resize(data.len());
            }
            // Scanner-axis lock takes precedence: project narrow
            // bins into the active channel's pixel slice with
            // dark-grey fill of unsampled regions, so historical
            // rows render as a sparse spatial picture of every
            // channel the scanner has touched. Per issue #516.
            let lock = *self.scanner_axis_lock.borrow();
            if let Some(lock) = lock {
                s.renderer
                    .push_line_locked(data, self.full_bandwidth.get(), &lock);
            } else {
                s.renderer.push_line(data);
            }
        }
        self.waterfall_area.queue_draw();
    }

    /// Change the waterfall colormap.
    pub fn set_colormap(&self, style: colormap::ColormapStyle) {
        if let Some(s) = self.waterfall_state.borrow_mut().as_mut() {
            s.renderer.set_colormap(style);
        }
        self.waterfall_area.queue_draw();
    }

    /// Update the display dB range for the FFT plot, waterfall, and signal history.
    pub fn set_db_range(&self, min_db: f32, max_db: f32) {
        // Non-finite bounds must be rejected explicitly: `NaN >= x`
        // is false, so a NaN would sail past the inverted-range
        // check and hand the renderers an invalid coordinate range.
        // Per `CodeRabbit` round 1 on PR #883.
        if !min_db.is_finite() || !max_db.is_finite() || min_db >= max_db {
            tracing::trace!(
                min_db,
                max_db,
                "set_db_range: ignoring non-finite or inverted range"
            );
            return;
        }
        self.min_db.set(min_db);
        self.max_db.set(max_db);
        if let Some(s) = self.waterfall_state.borrow_mut().as_mut() {
            s.renderer.set_db_range(min_db, max_db);
        }
        self.fft_area.queue_draw();
        self.waterfall_area.queue_draw();
        self.signal_history_area.queue_draw();
    }

    /// Enable or disable the spectrum fill area under the trace.
    pub fn set_fill_enabled(&self, enabled: bool) {
        self.fill_enabled.set(enabled);
        self.fft_area.queue_draw();
    }

    /// Wipe the waterfall surface, clear the spectrum trace data,
    /// and reset the signal-history ring + averaging buffer. Used
    /// when the waterfall master toggle goes off (#646) so the
    /// user doesn't see a frozen pre-disable snapshot while the
    /// FFT compute is suspended.
    ///
    /// Three states are reset:
    /// - **Waterfall pixel buffer** (`WaterfallRenderer::clear`):
    ///   surface returns to its as-built blank state.
    /// - **FFT trace** (`FftPlotState::current_data` cleared): the
    ///   spectrum line draws empty until new data arrives.
    /// - **Signal history** (`SignalHistoryRenderer::clear`): the
    ///   top signal-strength chart resets to its empty as-built
    ///   state.
    /// - **Averaging buffer**: cleared so re-enable starts a fresh
    ///   running average instead of continuing from stale samples.
    ///
    /// Each `DrawingArea` is queued for a redraw so the user sees
    /// the cleared state immediately, including when the receiver
    /// isn't currently playing (no incoming data to trigger a
    /// natural redraw).
    pub fn clear_displays(&self) {
        if let Some(s) = self.waterfall_state.borrow_mut().as_mut() {
            s.renderer.clear();
        }
        if let Some(s) = self.fft_state.borrow_mut().as_mut() {
            s.current_data.clear();
        }
        if let Some(s) = self.signal_history_state.borrow_mut().as_mut() {
            s.renderer.clear();
        }
        self.avg_buffer.borrow_mut().clear();
        self.fft_area.queue_draw();
        self.waterfall_area.queue_draw();
        self.signal_history_area.queue_draw();
    }

    /// Set the spectrum averaging mode, resetting the averaging buffer.
    pub fn set_averaging_mode(&self, mode: AveragingMode) {
        self.averaging_mode.set(mode);
        // Reset the averaging buffer so stale data doesn't persist.
        self.avg_buffer.borrow_mut().clear();
        tracing::debug!(?mode, "averaging mode changed");
    }

    /// Update the VFO display range to match the effective FFT bandwidth.
    ///
    /// Called when the sample rate changes (mode switch, decimation change,
    /// source switch). Sets the display to show +/-bandwidth/2 centered on DC
    /// and stores the full bandwidth for zoom calculations.
    pub fn set_display_bandwidth(&self, effective_sample_rate: f64) {
        // Same guard `enter_scanner_mode` applies: a file/network source
        // can report 0 / NaN / a sub-kHz rate, and `zoom()` clamps
        // against this value (#768). Keep the previous span instead.
        if !effective_sample_rate.is_finite() || effective_sample_rate < MIN_DISPLAY_BANDWIDTH_HZ {
            tracing::warn!(
                effective_sample_rate,
                "ignoring invalid display bandwidth; keeping the current span"
            );
            return;
        }
        let half = effective_sample_rate / 2.0;
        let mut vfo = self.vfo_state.borrow_mut();
        vfo.display_start_hz = -half;
        vfo.display_end_hz = half;
        vfo.max_span_hz = effective_sample_rate;
        self.full_bandwidth.set(effective_sample_rate);
        self.fft_area.queue_draw();
        self.waterfall_area.queue_draw();
    }

    /// Export the current waterfall display as a PNG file.
    pub fn export_waterfall_png(&self, path: &std::path::Path) -> Result<(), String> {
        if let Some(s) = self.waterfall_state.borrow().as_ref() {
            s.renderer.export_png(path)
        } else {
            Err("waterfall not initialized".to_string())
        }
    }

    /// Update the tuner center frequency for frequency axis labels.
    ///
    /// Also resets the VFO overlay offset to 0 so the passband rectangle
    /// tracks the new center. Every caller of this method is placing the
    /// tuner ON a specific channel — manual header tune, bookmark recall,
    /// preset selection, scanner retune — so the VFO's
    /// offset-from-center should be zero afterwards. Without this reset
    /// the rectangle would stick at the previous center-relative offset,
    /// visually drifting across the waterfall as center moves, even
    /// though the tuner is centered on the new frequency. Per
    /// issue #376.
    ///
    /// Note: click-to-tune does NOT go through this path. It dispatches
    /// `UiToDsp::SetVfoOffset(offset)` with the clicked offset,
    /// deliberately keeping the tuner center fixed and sliding the VFO
    /// passband to the click position instead.
    pub fn set_center_frequency(&self, freq_hz: f64) {
        self.center_freq.set(freq_hz);
        self.vfo_state.borrow_mut().offset_hz = 0.0;
        self.fft_area.queue_draw();
        self.waterfall_area.queue_draw();
    }

    /// Engage the scanner X-axis lock. The wiring layer calls
    /// this on `UiToDsp::SetScannerEnabled(true)` with the
    /// (min, max) envelope of all scanner-channel downlink
    /// frequencies. From this point until `exit_scanner_mode`,
    /// the spectrum + waterfall stay pinned to that range —
    /// retunes between channels no longer recentre the X axis.
    ///
    /// Initial state: `active_channel_*` is `None` until the
    /// first `set_scanner_active_channel` call. The renderer
    /// should treat that gap as "scanner mode is on but no
    /// channel is currently being sampled" — wide axis with
    /// no highlight band yet. Per issue #516.
    pub fn enter_scanner_mode(&self, min_hz: f64, max_hz: f64) {
        debug_assert!(
            min_hz < max_hz,
            "enter_scanner_mode: min ({min_hz}) must be < max ({max_hz})",
        );
        // Release-build guard: invalid bounds (non-finite or
        // inverted) would silently produce broken projections
        // (division by zero / negative span / NaN-poisoned
        // pixels) where `debug_assert!` is compiled out. Log
        // and bail instead of engaging the lock with garbage.
        // Per `CodeRabbit` round 1 on PR #562.
        if !min_hz.is_finite() || !max_hz.is_finite() || min_hz >= max_hz {
            tracing::warn!(
                min_hz,
                max_hz,
                "enter_scanner_mode: ignoring invalid lock bounds",
            );
            return;
        }
        *self.scanner_axis_lock.borrow_mut() = Some(ScannerAxisLock {
            min_hz,
            max_hz,
            active_channel_hz: None,
            active_channel_bw_hz: None,
        });
        self.fft_area.queue_draw();
        self.waterfall_area.queue_draw();
    }

    /// Update the active-channel context within an engaged
    /// scanner-axis lock. The wiring layer calls this on every
    /// `DspToUi::ScannerActiveChannelChanged` event with the
    /// channel's centre frequency and bandwidth. Drives the
    /// highlight-band overlay and the FFT/waterfall projection
    /// of narrow data into the wide range.
    ///
    /// No-op when the lock is not engaged — the wiring layer
    /// guards against race ordering by always calling
    /// `enter_scanner_mode` first, but a stray
    /// `ScannerActiveChannelChanged` arriving after
    /// `exit_scanner_mode` (e.g. mid-shutdown) shouldn't cause
    /// the lock to mysteriously re-engage. Also rejects
    /// non-finite frequency / bandwidth and non-positive
    /// bandwidth — invalid values would silently produce
    /// broken projections in release builds where
    /// `debug_assert!` is compiled out. Per issue #516 +
    /// `CodeRabbit` round 1 on PR #562.
    pub fn set_scanner_active_channel(&self, freq_hz: f64, bw_hz: f64) {
        if !freq_hz.is_finite() || !bw_hz.is_finite() || bw_hz <= 0.0 {
            tracing::trace!(
                freq_hz,
                bw_hz,
                "set_scanner_active_channel: ignoring invalid channel context",
            );
            return;
        }
        if let Some(lock) = self.scanner_axis_lock.borrow_mut().as_mut() {
            lock.active_channel_hz = Some(freq_hz);
            lock.active_channel_bw_hz = Some(bw_hz);
            self.fft_area.queue_draw();
            self.waterfall_area.queue_draw();
        }
    }

    /// Clear the active-channel context within an engaged
    /// scanner-axis lock — keeps the wide X axis pinned but
    /// drops the highlight band and the narrow-data
    /// projection. The wiring layer calls this on
    /// `ScannerActiveChannelChanged { key: None }` (scanner
    /// went idle without disengaging the lock — e.g. between
    /// rotations, or after the rotation drained but before the
    /// engine flips back to Idle). Without this, the previous
    /// channel's highlight + projection stays rendered until
    /// the next hop or scanner exit. Per `CodeRabbit` round 1
    /// on PR #562.
    pub fn clear_scanner_active_channel(&self) {
        if let Some(lock) = self.scanner_axis_lock.borrow_mut().as_mut() {
            lock.active_channel_hz = None;
            lock.active_channel_bw_hz = None;
            self.fft_area.queue_draw();
            self.waterfall_area.queue_draw();
        }
    }

    /// Disengage the scanner X-axis lock. Wiring layer calls
    /// this on `UiToDsp::SetScannerEnabled(false)`, scanner
    /// empty rotation, or any user manual tune that supersedes
    /// the scanner. The X axis reverts to the normal "current
    /// channel ± half BW" view on the next render. Per issue
    /// #516.
    pub fn exit_scanner_mode(&self) {
        *self.scanner_axis_lock.borrow_mut() = None;
        self.fft_area.queue_draw();
        self.waterfall_area.queue_draw();
    }

    /// Read-only accessor for the current lock state. Used by
    /// the Display panel's status row to render the locked
    /// range, and by tests to assert state-machine transitions.
    /// Per issue #516.
    #[must_use]
    pub fn scanner_axis_lock(&self) -> Option<ScannerAxisLock> {
        *self.scanner_axis_lock.borrow()
    }

    /// Programmatically update the VFO overlay's offset-from-center.
    ///
    /// Called from the `DspToUi::VfoOffsetChanged` handler so DSP-
    /// originated offset changes (e.g. a "reset VFO" button that
    /// dispatches `SetVfoOffset(0)`, or a future scripting hook)
    /// reflect on the overlay immediately. Click-to-tune and
    /// drag paths update `vfo_state.offset_hz` directly inline
    /// with the gesture, so they don't need to go through this
    /// method. Per issue #341.
    pub fn set_vfo_offset(&self, offset_hz: f64) {
        self.vfo_state.borrow_mut().offset_hz = offset_hz;
        self.fft_area.queue_draw();
        self.waterfall_area.queue_draw();
    }

    /// Programmatically set the VFO's visible channel-filter
    /// width. Called from `DspToUi::BandwidthChanged` so a
    /// bandwidth change originating outside the spectrum (Radio
    /// panel `AdwSpinRow`, reset button, mode switch, scanner
    /// retune) updates the visible VFO rectangle on the
    /// waterfall, not just the panel's numeric readout.
    ///
    /// VFO drag handles update `vfo_state.bandwidth_hz` inline
    /// during the gesture, so this method isn't on their hot
    /// path — it exists for the panel-side and DSP-echo
    /// reflection paths. Per issue #504.
    ///
    /// **No clamping here.** The drag path uses
    /// [`vfo_overlay::VfoState::clamp_bandwidth`] which enforces
    /// a global `[500 Hz, 250 kHz]` envelope — appropriate as a
    /// safety net for unbounded user drag input, but wrong for
    /// values arriving via DSP echo. Those values have already
    /// been clamped to the active demod's actual `[min, max]`
    /// (CW: `[50, 500]`, NFM: `[1k, 50k]`, etc.) by the demod's
    /// own `set_bandwidth`. Re-clamping to the global envelope
    /// here would push CW's 50 Hz bandwidth back up to 500 Hz,
    /// desyncing the visible width from the actual filter. Per
    /// `CodeRabbit` round 1 on PR #548.
    pub fn set_vfo_bandwidth(&self, bandwidth_hz: f64) {
        self.vfo_state.borrow_mut().bandwidth_hz = bandwidth_hz;
        self.fft_area.queue_draw();
        self.waterfall_area.queue_draw();
    }

    /// Current VFO offset (Hz from tuner center). Used by the
    /// reset-affordance visibility logic so the floating button
    /// can decide whether the VFO is in a non-default state
    /// without replicating the state cache. Per issue #341.
    #[must_use]
    pub fn vfo_offset_hz(&self) -> f64 {
        self.vfo_state.borrow().offset_hz
    }

    /// Push a signal level sample (in dB) into the history graph.
    ///
    /// Call this from the GTK main loop when `DspToUi::SignalLevel` arrives.
    pub fn push_signal_level(&self, db: f32) {
        if let Some(s) = self.signal_history_state.borrow_mut().as_mut() {
            s.renderer.push(db);
        }
        self.signal_history_area.queue_draw();
    }

    /// Register a callback invoked when the cursor moves over the FFT area.
    ///
    /// The callback receives `(frequency_hz, power_db)`. When the cursor
    /// leaves the area, `power_db` is `f32::NEG_INFINITY`.
    pub fn connect_cursor_moved<F: Fn(f64, f32) + 'static>(&self, f: F) {
        *self.cursor_callback.borrow_mut() = Some(Box::new(f));
    }

    /// Register a callback invoked when the VFO offset changes from user
    /// interaction (click-to-tune or drag).
    ///
    /// The callback receives `offset_hz` — the new VFO offset from center.
    /// Use this to update the frequency display and status bar.
    pub fn connect_vfo_offset_changed<F: Fn(f64) + 'static>(&self, f: F) {
        *self.vfo_offset_callback.borrow_mut() = Some(Box::new(f));
    }

    /// Register a callback invoked on click in scanner-locked
    /// mode, with the **absolute** frequency (Hz) under the
    /// click. The wiring layer registers a callback that
    /// force-disables the scanner (which tears down the lock
    /// via the master switch's `connect_active_notify`) and
    /// dispatches `UiToDsp::Tune(absolute)` plus the usual
    /// frequency-display + spectrum-centre + status-bar
    /// updates. Without this, click in scanner mode would
    /// dispatch a centre-relative VFO offset against a
    /// wandering active-channel centre — semantically broken.
    /// Per issue #563.
    pub fn connect_locked_click_to_tune<F: Fn(f64) + 'static>(&self, f: F) {
        *self.locked_click_callback.borrow_mut() = Some(Box::new(f));
    }
}

/// Apply the selected [`AveragingMode`] to an incoming FFT frame,
/// updating the running `avg` buffer and writing the display data
/// into `current_data`. The averaging buffer is seeded from the
/// first frame (or re-seeded when the FFT size changes) — this
/// avoids mode-specific init values (e.g., `MinHold` needs high
/// init, `PeakHold` needs low init) and prevents one-frame
/// artifacts after mode switches. Split out of
/// [`SpectrumHandle::push_fft_data`] per the 50-NLOC gate (#819,
/// PR #880 Codacy precedent).
fn apply_averaging(
    mode: AveragingMode,
    data: &[f32],
    avg: &mut Vec<f32>,
    current_data: &mut Vec<f32>,
) {
    if avg.len() != data.len() {
        *avg = data.to_vec();
    }
    match mode {
        AveragingMode::None => {
            current_data.resize(data.len(), 0.0);
            current_data.copy_from_slice(data);
        }
        AveragingMode::PeakHold => {
            for (i, &d) in data.iter().enumerate() {
                avg[i] = avg[i].max(d);
            }
            current_data.resize(avg.len(), 0.0);
            current_data.copy_from_slice(avg);
        }
        AveragingMode::RunningAvg => {
            for (i, &d) in data.iter().enumerate() {
                avg[i] = AVERAGING_ALPHA.mul_add(d, (1.0 - AVERAGING_ALPHA) * avg[i]);
            }
            current_data.resize(avg.len(), 0.0);
            current_data.copy_from_slice(avg);
        }
        AveragingMode::MinHold => {
            for (i, &d) in data.iter().enumerate() {
                avg[i] = avg[i].min(d);
            }
            current_data.resize(avg.len(), 0.0);
            current_data.copy_from_slice(avg);
        }
    }
}
