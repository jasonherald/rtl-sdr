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

/// In-place FFT shift: swap the first and second halves of a buffer
/// so that DC (bin 0) moves to the center position. Used on the display
/// side rather than the DSP pipeline to correctly handle the R820T
/// tuner's hardware spectrum inversion.
fn fftshift_in_place(buf: &mut [f32]) {
    let n = buf.len();
    if n < 2 {
        return;
    }
    let mid = n / 2;
    // Swap first half with second half in-place.
    for i in 0..mid {
        buf.swap(i, i + mid);
    }
}
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use fft_plot::FftPlotRenderer;
use signal_history::SignalHistoryRenderer;
use vfo_overlay::{BwHandle, HitZone, VfoOverlayRenderer, VfoState};
use waterfall::WaterfallRenderer;

use crate::messages::UiToDsp;

/// Shared cursor callback type — invoked with `(frequency_hz, power_db)`.
type CursorCallback = Rc<RefCell<Option<Box<dyn Fn(f64, f32)>>>>;

/// Number of FFT bins for the display (used for initial buffer sizing).
const FFT_SIZE: usize = 2048;

/// Default FFT plot pane height fraction (30% of total).
const FFT_PANE_FRACTION: f64 = 0.30;

/// Default minimum display level — matches SDR++ default of -70 dB.
/// Hides the ADC noise floor so the waterfall background is black.
const DEFAULT_MIN_DB: f32 = -70.0;
/// Default maximum display level in dB.
const DEFAULT_MAX_DB: f32 = 0.0;

/// Exponential moving average smoothing factor for `RunningAvg` mode.
const AVERAGING_ALPHA: f32 = 0.3;

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
    /// Pre-allocated buffer for fftshift of waterfall data (avoids per-frame alloc).
    shift_buffer: Rc<RefCell<Vec<f32>>>,
    cursor_callback: CursorCallback,
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

            // Display-side fftshift: rotate bins so DC (bin 0) moves to center.
            // Done here rather than in the DSP pipeline because the R820T's
            // spectrum inversion makes pipeline-side fftshift show the signal
            // on the wrong side. Rotating the display data is equivalent.
            fftshift_in_place(&mut s.current_data);
        }
        self.fft_area.queue_draw();

        // Push a new line to the waterfall (also needs fftshift).
        // Auto-resize the waterfall when the FFT size changes —
        // driven by the first matching-size frame rather than synchronously
        // from the UI, avoiding races with queued old-size frames.
        if let Some(s) = self.waterfall_state.borrow_mut().as_mut() {
            let target_width = waterfall::supported_texture_width_for(data.len());
            if target_width != s.renderer.texture_width() {
                s.renderer.resize(data.len());
            }
            let mut shifted = self.shift_buffer.borrow_mut();
            shifted.resize(data.len(), 0.0);
            shifted.copy_from_slice(data);
            fftshift_in_place(&mut shifted);
            s.renderer.push_line(&shifted);
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
    /// source switch). Sets the display to show +/-bandwidth/2 centered on DC.
    pub fn set_display_bandwidth(&self, effective_sample_rate: f64) {
        let half = effective_sample_rate / 2.0;
        let mut vfo = self.vfo_state.borrow_mut();
        vfo.display_start_hz = -half;
        vfo.display_end_hz = half;
        self.fft_area.queue_draw();
        self.waterfall_area.queue_draw();
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
}

/// Height in pixels for the collapsible signal history area.
const SIGNAL_HISTORY_HEIGHT: i32 = 100;

/// Build the spectrum view containing the FFT plot, waterfall display,
/// and a collapsible signal history graph.
///
/// Returns a `(gtk4::Box, SpectrumHandle)` — the box widget for layout,
/// and a handle for pushing real FFT/signal data into the display.
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
    );
    let waterfall_area = build_waterfall_area(Rc::clone(&waterfall_state), Rc::clone(&vfo_state));
    let signal_history_area =
        build_signal_history_area(Rc::clone(&signal_history_state), &min_db, &max_db);

    // Attach interaction gestures to both the waterfall and FFT areas.
    attach_click_gesture(&waterfall_area, &vfo_state, dsp_tx.clone());
    attach_drag_gesture(&waterfall_area, &vfo_state, dsp_tx);
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

    // Wrap the signal history DrawingArea in a collapsible expander.
    let expander = gtk4::Expander::builder()
        .label("Signal History")
        .expanded(true)
        .build();
    expander.set_child(Some(&signal_history_area));

    // Combine the FFT+waterfall paned and the signal history expander
    // into a vertical box.
    let outer_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    outer_box.append(&paned);
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
        shift_buffer: Rc::new(RefCell::new(Vec::new())),
        cursor_callback,
    };

    (outer_box, handle)
}

/// Build the `DrawingArea` for the FFT power spectrum plot.
fn build_fft_area(
    state: Rc<RefCell<Option<FftPlotState>>>,
    vfo_state: &Rc<RefCell<VfoState>>,
    min_db: &Rc<Cell<f32>>,
    max_db: &Rc<Cell<f32>>,
    fill_enabled: &Rc<Cell<bool>>,
    cursor_callback: &CursorCallback,
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
    area.set_draw_func(move |_area, cr, width, height| {
        if let Some(s) = state.borrow_mut().as_mut() {
            s.renderer.render(
                cr,
                &s.current_data,
                width,
                height,
                min_db_render.get(),
                max_db_render.get(),
                fill_render.get(),
            );

            let vfo = vfo_render.borrow();
            s.vfo_renderer.render(cr, &vfo, width, height);
        }
    });

    // Cursor readout: track mouse motion to compute frequency and power.
    let motion = gtk4::EventControllerMotion::new();
    let cursor_vfo = Rc::clone(vfo_state);
    let cursor_min = Rc::clone(min_db);
    let cursor_max = Rc::clone(max_db);
    let cursor_cb = Rc::clone(cursor_callback);
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

        let vfo = cursor_vfo.borrow();
        let freq_hz = vfo.pixel_to_hz(x, width);
        drop(vfo);

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
) -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .build();

    area.set_draw_func(move |_area, cr, width, height| {
        if let Some(s) = state.borrow().as_ref() {
            s.renderer.render(cr, width, height);

            let vfo = vfo_state.borrow();
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
fn attach_click_gesture(
    area: &gtk4::DrawingArea,
    vfo_state: &Rc<RefCell<VfoState>>,
    dsp_tx: std::sync::mpsc::Sender<UiToDsp>,
) {
    let click = gtk4::GestureClick::new();

    let vfo_state = Rc::clone(vfo_state);
    let area_weak = area.downgrade();
    click.connect_pressed(move |_gesture, _n_press, x, _y| {
        let Some(area) = area_weak.upgrade() else {
            return;
        };
        let width = f64::from(area.width());
        let mut vfo = vfo_state.borrow_mut();
        let hz = vfo.pixel_to_hz(x, width);
        vfo.offset_hz = hz;
        let offset = vfo.offset_hz;
        tracing::debug!(offset_hz = offset, "click-to-tune");
        drop(vfo);

        // Send VFO offset to DSP thread for actual tuning
        if let Err(e) = dsp_tx.send(UiToDsp::SetVfoOffset(offset)) {
            tracing::warn!("click-to-tune DSP send failed: {e}");
        }

        area.queue_draw();
    });

    area.add_controller(click);
}

/// Attach a drag gesture for VFO center movement and bandwidth handle adjustment.
#[allow(clippy::needless_pass_by_value)]
fn attach_drag_gesture(
    area: &gtk4::DrawingArea,
    vfo_state: &Rc<RefCell<VfoState>>,
    dsp_tx: std::sync::mpsc::Sender<UiToDsp>,
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
    drag.connect_drag_begin(move |_gesture, x, _y| {
        let Some(area) = area_weak_begin.upgrade() else {
            return;
        };
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
    drag.connect_drag_update(move |_gesture, offset_x, _offset_y| {
        let Some(area) = area_weak_update.upgrade() else {
            return;
        };
        let width = f64::from(area.width());
        let mut vfo = vfo_update.borrow_mut();

        if vfo.dragging {
            let delta_hz = vfo.pixels_to_hz(offset_x, width);
            vfo.offset_hz = start_offset_update.get() + delta_hz;
            let _ = dsp_tx_update.send(UiToDsp::SetVfoOffset(vfo.offset_hz));
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
            let _ = dsp_tx_update.send(UiToDsp::SetVfoOffset(vfo.offset_hz));
            let _ = dsp_tx_update.send(UiToDsp::SetBandwidth(vfo.bandwidth_hz));
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
