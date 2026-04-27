//! Spectrum display: FFT plot (top) + waterfall spectrogram (bottom).
//!
//! Both are rendered via `DrawingArea` widgets using Cairo. A `GtkPaned`
//! splits them vertically, with the FFT plot on top (~30%) and the
//! waterfall below (~70%).

pub mod colormap;
pub mod fft_plot;
pub mod frequency_axis;
pub mod signal_history;
pub mod vfo_overlay;
pub mod waterfall;

use std::cell::{Cell, RefCell};

use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use fft_plot::{FftPlotRenderer, SCANNER_HIGHLIGHT_COLOR};
use signal_history::SignalHistoryRenderer;
use vfo_overlay::{BwHandle, HitZone, VfoOverlayRenderer, VfoState};
use waterfall::WaterfallRenderer;

use crate::messages::UiToDsp;

/// Shared cursor callback type — invoked with `(frequency_hz, power_db)`.
type CursorCallback = Rc<RefCell<Option<Box<dyn Fn(f64, f32)>>>>;

/// Shared VFO-offset callback type — invoked with `(offset_hz)` when the
/// user click-to-tunes or drags the VFO to a new frequency offset.
type VfoOffsetCallback = Rc<RefCell<Option<Box<dyn Fn(f64)>>>>;

/// Scanner-mode click-to-tune callback — invoked with the
/// **absolute** frequency (in Hz) under the click when the
/// scanner-axis lock is engaged. The wiring layer registers a
/// callback that force-disables the scanner (via
/// `ScannerForceDisable::trigger`, which flips the master
/// switch and tears down the lock) and dispatches a manual
/// tune. Distinct from `VfoOffsetCallback` because the
/// dispatch shape is different — `UiToDsp::Tune(absolute)`,
/// not `UiToDsp::SetVfoOffset(relative)` — and because the
/// scanner-disable side-effect needs widget access that
/// `attach_click_gesture` doesn't have. Per issue #563.
type LockedClickCallback = Rc<RefCell<Option<Box<dyn Fn(f64)>>>>;

/// Number of FFT bins for the display (used for initial buffer sizing).
const FFT_SIZE: usize = 2048;

/// Default FFT plot pane height fraction (30% of total).
const FFT_PANE_FRACTION: f64 = 0.30;

/// Default minimum display level — matches SDR++ default of -70 dB.
/// Hides the ADC noise floor so the waterfall background is black.
const DEFAULT_MIN_DB: f32 = -70.0;
/// Default maximum display level in dB.
const DEFAULT_MAX_DB: f32 = 0.0;

/// Margin (px) between the floating "Reset VFO" overlay button
/// and the top-right edge of the spectrum area. 8 px is a visual
/// match with the GNOME Adwaita toast-overlay button inset.
const VFO_RESET_BUTTON_MARGIN_PX: i32 = 8;

/// Exponential moving average smoothing factor for `RunningAvg` mode.
const AVERAGING_ALPHA: f32 = 0.3;

/// Fraction of `max_span_hz` below which the click-to-tune diagnostic
/// classifies the view as "zoomed in". Set slightly below 1.0 so tiny
/// floating-point drift in the span arithmetic doesn't flip the
/// classification on an unzoomed view. Per `CodeRabbit` round 1 on
/// PR #418 — the threshold was previously a bare `0.99` literal inside
/// the tracing call.
const ZOOMED_IN_SPAN_RATIO_THRESHOLD: f64 = 0.99;

/// Spectrum averaging mode for the FFT display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AveragingMode {
    /// No averaging — display raw FFT data.
    #[default]
    None,
    /// Hold peak values across frames.
    PeakHold,
    /// Exponential moving average (smoothed).
    RunningAvg,
    /// Hold minimum values across frames.
    MinHold,
}

/// Shared state for the FFT plot `DrawingArea`.
struct FftPlotState {
    renderer: FftPlotRenderer,
    vfo_renderer: VfoOverlayRenderer,
    current_data: Vec<f32>,
}

/// Shared state for the waterfall `DrawingArea`.
struct WaterfallState {
    renderer: WaterfallRenderer,
    vfo_renderer: VfoOverlayRenderer,
}

/// Shared state for the signal history `DrawingArea`.
struct SignalHistoryState {
    renderer: SignalHistoryRenderer,
}

/// Handle for pushing FFT data into the spectrum display from outside.
///
/// Obtained from `build_spectrum_view` and used by the `DspToUi::FftData`
/// handler to update both the FFT plot and waterfall with real DSP data.
pub struct SpectrumHandle {
    fft_state: Rc<RefCell<Option<FftPlotState>>>,
    waterfall_state: Rc<RefCell<Option<WaterfallState>>>,
    signal_history_state: Rc<RefCell<Option<SignalHistoryState>>>,
    vfo_state: Rc<RefCell<VfoState>>,
    fft_area: gtk4::DrawingArea,
    waterfall_area: gtk4::DrawingArea,
    signal_history_area: gtk4::DrawingArea,
    min_db: Rc<Cell<f32>>,
    max_db: Rc<Cell<f32>>,
    fill_enabled: Rc<Cell<bool>>,
    averaging_mode: Rc<Cell<AveragingMode>>,
    avg_buffer: Rc<RefCell<Vec<f32>>>,
    cursor_callback: CursorCallback,
    /// Callback invoked when the VFO offset changes from user interaction
    /// (click-to-tune or drag). Used by `window.rs` to update the frequency
    /// display and status bar.
    vfo_offset_callback: VfoOffsetCallback,
    /// Callback invoked on click in scanner-locked mode, with the
    /// absolute frequency under the click. Lets the wiring layer
    /// force-disable the scanner and tune absolutely without
    /// `attach_click_gesture` needing direct access to
    /// `ScannerForceDisable`. Per issue #563.
    locked_click_callback: LockedClickCallback,
    /// Full (unzoomed) FFT bandwidth in Hz, set by `set_display_bandwidth()`.
    /// Used by the FFT plot and waterfall renderers for zoom mapping.
    full_bandwidth: Rc<Cell<f64>>,
    /// Tuner center frequency in Hz (for absolute frequency labels).
    center_freq: Rc<Cell<f64>>,
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
    scanner_axis_lock: Rc<RefCell<Option<ScannerAxisLock>>>,
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

            // Seed the averaging buffer from the first frame, or re-seed if
            // the FFT size changed. This avoids mode-specific init values
            // (e.g., MinHold needs high init, PeakHold needs low init) and
            // prevents one-frame artifacts after mode switches.
            if avg.len() != data.len() {
                *avg = data.to_vec();
            }

            match mode {
                AveragingMode::None => {
                    s.current_data.resize(data.len(), 0.0);
                    s.current_data.copy_from_slice(data);
                }
                AveragingMode::PeakHold => {
                    for (i, &d) in data.iter().enumerate() {
                        avg[i] = avg[i].max(d);
                    }
                    s.current_data.resize(avg.len(), 0.0);
                    s.current_data.copy_from_slice(&avg);
                }
                AveragingMode::RunningAvg => {
                    for (i, &d) in data.iter().enumerate() {
                        avg[i] = AVERAGING_ALPHA.mul_add(d, (1.0 - AVERAGING_ALPHA) * avg[i]);
                    }
                    s.current_data.resize(avg.len(), 0.0);
                    s.current_data.copy_from_slice(&avg);
                }
                AveragingMode::MinHold => {
                    for (i, &d) in data.iter().enumerate() {
                        avg[i] = avg[i].min(d);
                    }
                    s.current_data.resize(avg.len(), 0.0);
                    s.current_data.copy_from_slice(&avg);
                }
            }

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
        if min_db >= max_db {
            tracing::trace!(min_db, max_db, "set_db_range: ignoring inverted range");
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

/// Height in pixels for the collapsible signal history area.
const SIGNAL_HISTORY_HEIGHT: i32 = 100;

/// Build the spectrum view containing the FFT plot, waterfall display,
/// and a collapsible signal history graph.
///
/// Returns a `(gtk4::Box, SpectrumHandle)` — the box widget for layout,
/// and a handle for pushing real FFT/signal data into the display.
#[allow(clippy::too_many_lines)]
pub fn build_spectrum_view(
    dsp_tx: std::sync::mpsc::Sender<UiToDsp>,
) -> (gtk4::Box, SpectrumHandle) {
    let vfo_state: Rc<RefCell<VfoState>> = Rc::new(RefCell::new(VfoState::default()));
    let fft_state: Rc<RefCell<Option<FftPlotState>>> = Rc::new(RefCell::new(None));
    let waterfall_state: Rc<RefCell<Option<WaterfallState>>> = Rc::new(RefCell::new(None));
    let signal_history_state: Rc<RefCell<Option<SignalHistoryState>>> = Rc::new(RefCell::new(None));

    let min_db: Rc<Cell<f32>> = Rc::new(Cell::new(DEFAULT_MIN_DB));
    let max_db: Rc<Cell<f32>> = Rc::new(Cell::new(DEFAULT_MAX_DB));
    let fill_enabled: Rc<Cell<bool>> = Rc::new(Cell::new(true));
    let cursor_callback: CursorCallback = Rc::new(RefCell::new(None));
    let vfo_offset_callback: VfoOffsetCallback = Rc::new(RefCell::new(None));
    let locked_click_callback: LockedClickCallback = Rc::new(RefCell::new(None));
    let full_bandwidth: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));
    let center_freq: Rc<Cell<f64>> = Rc::new(Cell::new(100_000_000.0)); // default 100 MHz
    let scanner_axis_lock: Rc<RefCell<Option<ScannerAxisLock>>> = Rc::new(RefCell::new(None));

    // Initialize renderer state eagerly (no GL context needed).
    *fft_state.borrow_mut() = Some(FftPlotState {
        renderer: FftPlotRenderer::new(),
        vfo_renderer: VfoOverlayRenderer::new(),
        current_data: vec![DEFAULT_MIN_DB; FFT_SIZE],
    });
    *waterfall_state.borrow_mut() = Some(WaterfallState {
        renderer: {
            let mut r = WaterfallRenderer::new(FFT_SIZE);
            r.set_db_range(DEFAULT_MIN_DB, DEFAULT_MAX_DB);
            r
        },
        vfo_renderer: VfoOverlayRenderer::new(),
    });
    *signal_history_state.borrow_mut() = Some(SignalHistoryState {
        renderer: SignalHistoryRenderer::new(),
    });
    tracing::info!("spectrum renderers initialized (Cairo)");

    let fft_area = build_fft_area(
        Rc::clone(&fft_state),
        &vfo_state,
        &min_db,
        &max_db,
        &fill_enabled,
        &cursor_callback,
        &full_bandwidth,
        &center_freq,
        &scanner_axis_lock,
    );
    let waterfall_area = build_waterfall_area(
        Rc::clone(&waterfall_state),
        Rc::clone(&vfo_state),
        Rc::clone(&full_bandwidth),
        Rc::clone(&scanner_axis_lock),
    );
    let signal_history_area =
        build_signal_history_area(Rc::clone(&signal_history_state), &min_db, &max_db);

    // Attach interaction gestures to both the waterfall and FFT areas.
    attach_click_gesture(
        &waterfall_area,
        &vfo_state,
        dsp_tx.clone(),
        &vfo_offset_callback,
        &scanner_axis_lock,
        &locked_click_callback,
    );
    attach_drag_gesture(
        &waterfall_area,
        &vfo_state,
        dsp_tx,
        &vfo_offset_callback,
        &scanner_axis_lock,
    );
    attach_scroll_gesture(&waterfall_area, &vfo_state);

    // Also attach scroll-to-zoom on the FFT area for convenience.
    attach_scroll_gesture(&fft_area, &vfo_state);

    let paned = gtk4::Paned::builder()
        .orientation(gtk4::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();

    paned.set_start_child(Some(&fft_area));
    paned.set_end_child(Some(&waterfall_area));

    // Set the initial split position once the widget has a size.
    paned.connect_realize(|paned| {
        let height = paned.height();
        if height > 0 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pos = (f64::from(height) * FFT_PANE_FRACTION) as i32;
            paned.set_position(pos);
        }
    });

    // Floating "Reset VFO" button in the top-right of the
    // spectrum area. Hidden by default; `window.rs` toggles
    // visibility whenever the VFO enters or leaves a non-default
    // state. Uses the `osd` CSS class for a translucent overlay
    // feel that doesn't steal too much visual weight from the
    // spectrum underneath.
    let vfo_reset_button = gtk4::Button::builder()
        .icon_name("edit-undo-symbolic")
        .tooltip_text("Reset VFO to defaults")
        .css_classes(["osd", "circular"])
        .halign(gtk4::Align::End)
        .valign(gtk4::Align::Start)
        .margin_top(VFO_RESET_BUTTON_MARGIN_PX)
        .margin_end(VFO_RESET_BUTTON_MARGIN_PX)
        .visible(false)
        .build();
    vfo_reset_button.update_property(&[gtk4::accessible::Property::Label(
        "Reset VFO bandwidth and offset to defaults",
    )]);

    // Wrap the paned in an Overlay so the floating button can
    // sit on top of both the FFT plot and the waterfall without
    // shifting their layout.
    let spectrum_overlay = gtk4::Overlay::builder().hexpand(true).vexpand(true).build();
    spectrum_overlay.set_child(Some(&paned));
    spectrum_overlay.add_overlay(&vfo_reset_button);

    // Wrap the signal history DrawingArea in a collapsible expander.
    let expander = gtk4::Expander::builder()
        .label("Signal History")
        .expanded(true)
        .build();
    expander.set_child(Some(&signal_history_area));

    // Combine the FFT+waterfall overlay and the signal history
    // expander into a vertical box.
    let outer_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    outer_box.append(&spectrum_overlay);
    outer_box.append(&expander);

    let handle = SpectrumHandle {
        fft_state,
        waterfall_state,
        signal_history_state,
        vfo_state,
        fft_area: fft_area.clone(),
        waterfall_area: waterfall_area.clone(),
        signal_history_area: signal_history_area.clone(),
        min_db,
        max_db,
        fill_enabled,
        averaging_mode: Rc::new(Cell::new(AveragingMode::default())),
        avg_buffer: Rc::new(RefCell::new(Vec::new())),
        cursor_callback,
        vfo_offset_callback,
        locked_click_callback,
        full_bandwidth,
        center_freq,
        scanner_axis_lock,
        vfo_reset_button,
    };

    (outer_box, handle)
}

/// Build the `DrawingArea` for the FFT power spectrum plot.
#[allow(clippy::too_many_arguments)]
fn build_fft_area(
    state: Rc<RefCell<Option<FftPlotState>>>,
    vfo_state: &Rc<RefCell<VfoState>>,
    min_db: &Rc<Cell<f32>>,
    max_db: &Rc<Cell<f32>>,
    fill_enabled: &Rc<Cell<bool>>,
    cursor_callback: &CursorCallback,
    full_bandwidth: &Rc<Cell<f64>>,
    center_freq: &Rc<Cell<f64>>,
    scanner_axis_lock: &Rc<RefCell<Option<ScannerAxisLock>>>,
) -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .build();

    // Set the draw function — called on every queue_draw().
    let min_db_render = Rc::clone(min_db);
    let max_db_render = Rc::clone(max_db);
    let fill_render = Rc::clone(fill_enabled);
    let vfo_render = Rc::clone(vfo_state);
    let full_bw_render = Rc::clone(full_bandwidth);
    let center_freq_render = Rc::clone(center_freq);
    let scanner_lock_render = Rc::clone(scanner_axis_lock);
    area.set_draw_func(move |_area, cr, width, height| {
        if let Some(s) = state.borrow_mut().as_mut() {
            // Scanner-axis lock takes precedence: when the
            // scanner is engaged, X axis is pinned to the
            // channel envelope and the narrow FFT data lands
            // in the active channel's slice. The VFO overlay
            // is suppressed in scanner mode — its meaning
            // (drag-to-tune within the current channel)
            // doesn't apply when the X axis represents a wide
            // multi-channel range. Per issue #516.
            let lock = *scanner_lock_render.borrow();
            if let Some(lock) = lock {
                s.renderer.render_locked(
                    cr,
                    &s.current_data,
                    width,
                    height,
                    min_db_render.get(),
                    max_db_render.get(),
                    fill_render.get(),
                    full_bw_render.get(),
                    &lock,
                );
                return;
            }

            let vfo = vfo_render.borrow();
            s.renderer.render(
                cr,
                &s.current_data,
                width,
                height,
                min_db_render.get(),
                max_db_render.get(),
                fill_render.get(),
                vfo.display_start_hz,
                vfo.display_end_hz,
                full_bw_render.get(),
                center_freq_render.get(),
            );

            s.vfo_renderer.render(cr, &vfo, width, height);
        }
    });

    // Cursor readout: track mouse motion to compute frequency and power.
    let motion = gtk4::EventControllerMotion::new();
    let cursor_vfo = Rc::clone(vfo_state);
    let cursor_min = Rc::clone(min_db);
    let cursor_max = Rc::clone(max_db);
    let cursor_cb = Rc::clone(cursor_callback);
    let cursor_lock = Rc::clone(scanner_axis_lock);
    let area_weak_motion = area.downgrade();
    motion.connect_motion(move |_ctrl, x, y| {
        let Some(area) = area_weak_motion.upgrade() else {
            return;
        };
        let width = f64::from(area.width());
        let height = f64::from(area.height());
        if width <= 0.0 || height <= 0.0 {
            return;
        }

        // Pixel → frequency in lock-aware fashion. In scanner
        // mode the X axis spans `[lock.min_hz, lock.max_hz]`
        // absolutely; out of mode it's center-relative via the
        // VFO's `pixel_to_hz`. Without this branch the cursor
        // readout shows wildly wrong frequencies in scanner
        // mode (it'd report values relative to the wandering
        // active-channel centre instead of the wide locked
        // range). Per `CodeRabbit` round 1 on PR #562.
        let lock = *cursor_lock.borrow();
        let freq_hz = if let Some(lock) = lock {
            let frac = (x / width).clamp(0.0, 1.0);
            lock.min_hz + frac * (lock.max_hz - lock.min_hz)
        } else {
            let vfo = cursor_vfo.borrow();
            vfo.pixel_to_hz(x, width)
        };

        let lo = cursor_min.get();
        let hi = cursor_max.get();
        let db_range = hi - lo;
        // y=0 is top (max_db), y=height is bottom (min_db).
        #[allow(clippy::cast_possible_truncation)]
        let power_db = hi - (y as f32 / height as f32) * db_range;

        if let Some(cb) = cursor_cb.borrow().as_ref() {
            cb(freq_hz, power_db);
        }
    });

    let cursor_cb_leave = Rc::clone(cursor_callback);
    motion.connect_leave(move |_ctrl| {
        if let Some(cb) = cursor_cb_leave.borrow().as_ref() {
            cb(0.0, f32::NEG_INFINITY);
        }
    });

    area.add_controller(motion);

    area
}

/// Build the `DrawingArea` for the waterfall spectrogram.
fn build_waterfall_area(
    state: Rc<RefCell<Option<WaterfallState>>>,
    vfo_state: Rc<RefCell<VfoState>>,
    full_bandwidth: Rc<Cell<f64>>,
    scanner_axis_lock: Rc<RefCell<Option<ScannerAxisLock>>>,
) -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .build();

    area.set_draw_func(move |_area, cr, width, height| {
        if let Some(s) = state.borrow().as_ref() {
            // Scanner-axis lock: pixels are pre-projected to
            // the wide locked range at push time, so the
            // renderer just needs a no-zoom call (display
            // matches full surface 1:1). VFO overlay is
            // suppressed because its drag-to-tune semantics
            // don't apply to a multi-channel range. Per issue
            // #516.
            let lock = *scanner_axis_lock.borrow();
            if let Some(lock) = lock {
                let bw = full_bandwidth.get();
                let half = bw / 2.0;
                s.renderer.render(cr, width, height, -half, half, bw);
                // Scanner-axis active-channel highlight band —
                // mirrors the FFT plot's `render_locked` band
                // so the user has a continuous visual anchor
                // for "where is the scanner sampling right
                // now?" across both panels. Drawn AFTER the
                // texture blit so it overlays the data; spans
                // the full height for visibility while the
                // historical rows scroll past underneath. Per
                // `CodeRabbit` round 3 on PR #562.
                if let (Some(active_hz), Some(active_bw)) =
                    (lock.active_channel_hz, lock.active_channel_bw_hz)
                {
                    let span = lock.max_hz - lock.min_hz;
                    if span > 0.0 {
                        let w = f64::from(width);
                        let h = f64::from(height);
                        let band_min_x = w * (active_hz - active_bw / 2.0 - lock.min_hz) / span;
                        let band_max_x = w * (active_hz + active_bw / 2.0 - lock.min_hz) / span;
                        let band_w = (band_max_x - band_min_x).max(1.0);
                        cr.set_source_rgba(
                            SCANNER_HIGHLIGHT_COLOR[0],
                            SCANNER_HIGHLIGHT_COLOR[1],
                            SCANNER_HIGHLIGHT_COLOR[2],
                            SCANNER_HIGHLIGHT_COLOR[3],
                        );
                        cr.rectangle(band_min_x, 0.0, band_w, h);
                        let _ = cr.fill();
                    }
                }
                return;
            }

            let vfo = vfo_state.borrow();
            s.renderer.render(
                cr,
                width,
                height,
                vfo.display_start_hz,
                vfo.display_end_hz,
                full_bandwidth.get(),
            );

            s.vfo_renderer.render(cr, &vfo, width, height);
        }
    });

    area
}

/// Build the `DrawingArea` for the signal strength history graph.
fn build_signal_history_area(
    state: Rc<RefCell<Option<SignalHistoryState>>>,
    min_db: &Rc<Cell<f32>>,
    max_db: &Rc<Cell<f32>>,
) -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::builder()
        .hexpand(true)
        .vexpand(false)
        .height_request(SIGNAL_HISTORY_HEIGHT)
        .build();

    let min_db_render = Rc::clone(min_db);
    let max_db_render = Rc::clone(max_db);
    area.set_draw_func(move |_area, cr, width, height| {
        if let Some(s) = state.borrow().as_ref() {
            s.renderer
                .render(cr, width, height, min_db_render.get(), max_db_render.get());
        }
    });

    area
}

/// Attach a click-to-tune gesture to a `DrawingArea`.
///
/// Single-clicking sets the VFO center to the clicked frequency.
#[allow(clippy::too_many_arguments)]
fn attach_click_gesture(
    area: &gtk4::DrawingArea,
    vfo_state: &Rc<RefCell<VfoState>>,
    dsp_tx: std::sync::mpsc::Sender<UiToDsp>,
    vfo_offset_callback: &VfoOffsetCallback,
    scanner_axis_lock: &Rc<RefCell<Option<ScannerAxisLock>>>,
    locked_click_callback: &LockedClickCallback,
) {
    let click = gtk4::GestureClick::new();

    let vfo_state = Rc::clone(vfo_state);
    let area_weak = area.downgrade();
    let offset_cb = Rc::clone(vfo_offset_callback);
    let click_lock = Rc::clone(scanner_axis_lock);
    let locked_click_cb = Rc::clone(locked_click_callback);
    click.connect_pressed(move |_gesture, _n_press, x, _y| {
        let Some(area) = area_weak.upgrade() else {
            return;
        };
        let width = f64::from(area.width());

        // Scanner-locked path: the X axis represents an
        // absolute multi-channel range, not a centre-relative
        // VFO. A click here means "I see something interesting,
        // jump to it" — which only makes sense after force-
        // disabling the scanner so the radio actually parks on
        // the chosen frequency. The wiring layer's callback
        // handles both: flips the master switch off (which
        // tears down the lock via `connect_active_notify`)
        // and dispatches `UiToDsp::Tune(absolute_freq)`. Skip
        // the regular SetVfoOffset path entirely — it'd dispatch
        // a centre-relative offset against a wandering active-
        // channel centre, which is meaningless. Per issue #563.
        let lock_snapshot = *click_lock.borrow();
        if let Some(lock) = lock_snapshot {
            if width <= 0.0 {
                return;
            }
            let frac = (x / width).clamp(0.0, 1.0);
            let abs_freq_hz = lock.min_hz + frac * (lock.max_hz - lock.min_hz);
            tracing::debug!(
                click_x = x,
                width,
                abs_freq_hz,
                "scanner-locked click-to-tune: dispatching absolute tune"
            );
            if let Some(cb) = locked_click_cb.borrow().as_ref() {
                cb(abs_freq_hz);
            }
            area.queue_draw();
            return;
        }

        let mut vfo = vfo_state.borrow_mut();
        let hz = vfo.pixel_to_hz(x, width);
        // Snapshot display span + max span BEFORE mutating offset so
        // a post-investigation diff of the trace can tell (a) whether
        // the click landed inside the AA-filter-safe subset of the
        // display, and (b) whether the user was zoomed in — zoom
        // modifies `display_start_hz` / `display_end_hz` at runtime
        // so a fixed ±bandwidth/2 assumption doesn't hold. Per #337
        // investigation in PR batch with #407 / #157 / #400.
        let display_start_hz = vfo.display_start_hz;
        let display_end_hz = vfo.display_end_hz;
        let max_span_hz = vfo.max_span_hz;
        vfo.offset_hz = hz;
        let offset = vfo.offset_hz;
        tracing::debug!(
            click_x = x,
            width,
            display_start_hz,
            display_end_hz,
            max_span_hz,
            zoomed_in =
                (display_end_hz - display_start_hz) < max_span_hz * ZOOMED_IN_SPAN_RATIO_THRESHOLD,
            offset_hz = offset,
            "click-to-tune: computed offset from pixel"
        );
        drop(vfo);

        // Send VFO offset to DSP thread for actual tuning
        if let Err(e) = dsp_tx.send(UiToDsp::SetVfoOffset(offset)) {
            tracing::warn!("click-to-tune DSP send failed: {e}");
        }

        // Notify the UI so the frequency display and status bar update.
        if let Some(cb) = offset_cb.borrow().as_ref() {
            cb(offset);
        }

        area.queue_draw();
    });

    area.add_controller(click);
}

/// Attach a drag gesture for VFO center movement and bandwidth handle adjustment.
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
fn attach_drag_gesture(
    area: &gtk4::DrawingArea,
    vfo_state: &Rc<RefCell<VfoState>>,
    dsp_tx: std::sync::mpsc::Sender<UiToDsp>,
    vfo_offset_callback: &VfoOffsetCallback,
    scanner_axis_lock: &Rc<RefCell<Option<ScannerAxisLock>>>,
) {
    let drag = gtk4::GestureDrag::new();

    // Snapshot of VFO state at drag start, for computing deltas.
    let drag_start_offset_hz: Rc<std::cell::Cell<f64>> = Rc::new(std::cell::Cell::new(0.0));
    let drag_start_bw_hz: Rc<std::cell::Cell<f64>> = Rc::new(std::cell::Cell::new(0.0));

    // On drag begin: determine if we're dragging a handle or the passband.
    let vfo_begin = Rc::clone(vfo_state);
    let start_offset = Rc::clone(&drag_start_offset_hz);
    let start_bw = Rc::clone(&drag_start_bw_hz);
    let area_weak_begin = area.downgrade();
    let drag_lock = Rc::clone(scanner_axis_lock);
    drag.connect_drag_begin(move |_gesture, x, _y| {
        let Some(area) = area_weak_begin.upgrade() else {
            return;
        };
        // Suppress drag entirely while the scanner-axis lock
        // is engaged. Drag-VFO and drag-bandwidth are inherently
        // single-channel operations; the wide multi-channel
        // axis has no compatible meaning. The user can still
        // click-to-tune (see `attach_click_gesture`'s lock-aware
        // path), which force-disables the scanner before
        // tuning. Per issue #563.
        if drag_lock.borrow().is_some() {
            let mut vfo = vfo_begin.borrow_mut();
            vfo.dragging = false;
            vfo.bw_dragging = None;
            return;
        }
        let width = f64::from(area.width());
        let mut vfo = vfo_begin.borrow_mut();
        let hit = vfo.hit_test(x, width);

        start_offset.set(vfo.offset_hz);
        start_bw.set(vfo.bandwidth_hz);

        match hit {
            HitZone::LeftHandle => {
                vfo.bw_dragging = Some(BwHandle::Left);
                vfo.dragging = false;
            }
            HitZone::RightHandle => {
                vfo.bw_dragging = Some(BwHandle::Right);
                vfo.dragging = false;
            }
            HitZone::Passband => {
                vfo.dragging = true;
                vfo.bw_dragging = None;
            }
            HitZone::Outside => {
                // Click-to-tune is handled by the click gesture; drag from
                // outside does nothing.
                vfo.dragging = false;
                vfo.bw_dragging = None;
            }
        }
    });

    // On drag update: move VFO or adjust bandwidth.
    let vfo_update = Rc::clone(vfo_state);
    let start_offset_update = Rc::clone(&drag_start_offset_hz);
    let start_bw_update = Rc::clone(&drag_start_bw_hz);
    let area_weak_update = area.downgrade();
    let dsp_tx_update = dsp_tx.clone();
    let offset_cb = Rc::clone(vfo_offset_callback);
    let drag_lock_update = Rc::clone(scanner_axis_lock);
    drag.connect_drag_update(move |_gesture, offset_x, _offset_y| {
        let Some(area) = area_weak_update.upgrade() else {
            return;
        };
        // Re-check the lock at every update tick. If the
        // scanner engaged mid-gesture (e.g. a Doppler retune
        // hit at the same moment the user started dragging),
        // bail and clear the local drag flags so subsequent
        // updates also short-circuit. Without this, a drag
        // begun before the lock engaged would keep emitting
        // `SetVfoOffset` / `SetBandwidth` against a wide
        // multi-channel axis where the math is meaningless.
        // Per `CodeRabbit` round 1 on PR #565.
        if drag_lock_update.borrow().is_some() {
            let mut vfo = vfo_update.borrow_mut();
            vfo.dragging = false;
            vfo.bw_dragging = None;
            return;
        }
        let width = f64::from(area.width());
        let mut vfo = vfo_update.borrow_mut();

        if vfo.dragging {
            let delta_hz = vfo.pixels_to_hz(offset_x, width);
            vfo.offset_hz = start_offset_update.get() + delta_hz;
            let offset = vfo.offset_hz;
            let _ = dsp_tx_update.send(UiToDsp::SetVfoOffset(offset));
            drop(vfo);
            if let Some(cb) = offset_cb.borrow().as_ref() {
                cb(offset);
            }
            area.queue_draw();
        } else if let Some(handle) = vfo.bw_dragging {
            let delta_hz = vfo.pixels_to_hz(offset_x, width);
            let original_bw = start_bw_update.get();
            let original_offset = start_offset_update.get();

            match handle {
                BwHandle::Left => {
                    // Moving the left edge: the left edge moves by delta,
                    // but the right edge stays fixed.
                    // right_edge = original_offset + original_bw/2 (fixed)
                    // left_edge  = original_offset - original_bw/2 + delta
                    // new_bw = right_edge - left_edge = original_bw - delta
                    // new_center = (left_edge + right_edge) / 2
                    let new_bw = original_bw - delta_hz;
                    if new_bw > 0.0 {
                        let right_edge = original_offset + original_bw / 2.0;
                        vfo.bandwidth_hz = new_bw;
                        vfo.clamp_bandwidth();
                        vfo.offset_hz = right_edge - vfo.bandwidth_hz / 2.0;
                    }
                }
                BwHandle::Right => {
                    // Moving the right edge: the left edge stays fixed.
                    let new_bw = original_bw + delta_hz;
                    if new_bw > 0.0 {
                        let left_edge = original_offset - original_bw / 2.0;
                        vfo.bandwidth_hz = new_bw;
                        vfo.clamp_bandwidth();
                        vfo.offset_hz = left_edge + vfo.bandwidth_hz / 2.0;
                    }
                }
            }
            let offset = vfo.offset_hz;
            let _ = dsp_tx_update.send(UiToDsp::SetVfoOffset(offset));
            let _ = dsp_tx_update.send(UiToDsp::SetBandwidth(vfo.bandwidth_hz));
            drop(vfo);
            if let Some(cb) = offset_cb.borrow().as_ref() {
                cb(offset);
            }
            area.queue_draw();
        }
    });

    // On drag end: clear drag state.
    let vfo_end = Rc::clone(vfo_state);
    drag.connect_drag_end(move |_gesture, _offset_x, _offset_y| {
        let mut vfo = vfo_end.borrow_mut();
        vfo.dragging = false;
        vfo.bw_dragging = None;
    });

    area.add_controller(drag);
}

/// Attach a scroll-to-zoom gesture to a `DrawingArea`.
///
/// Scrolling zooms the frequency display range centered on the cursor position.
fn attach_scroll_gesture(area: &gtk4::DrawingArea, vfo_state: &Rc<RefCell<VfoState>>) {
    let scroll = gtk4::EventControllerScroll::new(
        gtk4::EventControllerScrollFlags::VERTICAL | gtk4::EventControllerScrollFlags::DISCRETE,
    );

    let vfo_state = Rc::clone(vfo_state);
    let area_weak = area.downgrade();
    scroll.connect_scroll(move |_controller, _dx, dy| {
        let Some(area) = area_weak.upgrade() else {
            return glib::Propagation::Stop;
        };
        let width = f64::from(area.width());

        // TODO: Anchor zoom on cursor position instead of display center.
        // GTK4 EventControllerScroll doesn't provide position in the scroll
        // signal. Add an EventControllerMotion to track the pointer and use
        // its last-known X coordinate here for cursor-centered zoom.
        let cursor_x = width / 2.0;

        let mut vfo = vfo_state.borrow_mut();
        let cursor_hz = vfo.pixel_to_hz(cursor_x, width);

        // dy > 0 = scroll down = zoom out; dy < 0 = scroll up = zoom in.
        vfo.zoom(cursor_hz, -dy);

        drop(vfo);
        area.queue_draw();

        glib::Propagation::Stop
    });

    area.add_controller(scroll);
}
