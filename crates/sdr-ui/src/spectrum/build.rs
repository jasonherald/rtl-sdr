//! Widget construction for the spectrum display (issue #819):
//! [`build_spectrum_view`] assembles the FFT plot + waterfall
//! `GtkPaned`, the collapsible signal-history graph, the floating
//! "Reset VFO" overlay button, and each pane's Cairo draw
//! function. Split out of `spectrum/mod.rs` per the file-size
//! pass.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;

use super::fft_plot::{FftPlotRenderer, SCANNER_HIGHLIGHT_COLOR};
use super::handle::{ScannerAxisLock, SpectrumHandle};
use super::interaction::{attach_click_gesture, attach_drag_gesture, attach_scroll_gesture};
use super::signal_history::SignalHistoryRenderer;
use super::vfo_overlay::{VfoOverlayRenderer, VfoState};
use super::waterfall::WaterfallRenderer;
use super::{
    AveragingMode, CursorCallback, DEFAULT_MAX_DB, DEFAULT_MIN_DB, FFT_PANE_FRACTION, FFT_SIZE,
    FftPlotState, LockedClickCallback, SIGNAL_HISTORY_HEIGHT, SignalHistoryState,
    VFO_RESET_BUTTON_MARGIN_PX, VfoOffsetCallback, WaterfallState,
};
use crate::messages::UiToDsp;

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
    // Scroll-to-zoom on both spectrum panes. They share `vfo_state`, so each
    // zoom redraws both panes to keep them in sync. Per #657.
    attach_scroll_gesture(&fft_area, &waterfall_area, &vfo_state);

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
