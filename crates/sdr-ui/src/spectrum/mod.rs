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

mod build;
mod handle;
mod interaction;

pub use build::build_spectrum_view;
pub use handle::{ScannerAxisLock, SpectrumHandle};

use std::cell::RefCell;
use std::rc::Rc;

use fft_plot::FftPlotRenderer;
use signal_history::SignalHistoryRenderer;
use vfo_overlay::VfoOverlayRenderer;
use waterfall::WaterfallRenderer;

/// Smallest display bandwidth the spectrum accepts from the engine; below
/// this the VFO zoom clamp (`MIN_DISPLAY_SPAN_HZ`) would invert (#768).
const MIN_DISPLAY_BANDWIDTH_HZ: f64 = 1_000.0;

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
/// Center frequency seeded into the frequency axis before the
/// first real tune arrives — 100 MHz, matching upstream
/// `rtl_tcp.c`'s `frequency = 100000000` default. Named per the
/// magic-number convention (`CodeRabbit` round 1 on PR #883).
const DEFAULT_CENTER_FREQ_HZ: f64 = 100_000_000.0;

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

/// Height in pixels for the collapsible signal history area.
const SIGNAL_HISTORY_HEIGHT: i32 = 100;
