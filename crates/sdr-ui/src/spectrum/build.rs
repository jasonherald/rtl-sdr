//! Widget construction for the spectrum display (issue #819):
//! [`build_spectrum_view`] assembles the FFT plot + waterfall
//! `GtkPaned`, the collapsible signal-history graph, the floating
//! "Reset VFO" overlay button, and each pane's Cairo draw
//! function. Split out of `spectrum/mod.rs` per the file-size
//! pass; the former nine-parameter pane builders now share one
//! [`SpectrumShared`] state bundle (the deps-struct idiom from
//! PR #880) per the 8-parameter / 50-NLOC gates.

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

/// The spectrum display's shared mutable state, created once by
/// [`build_spectrum_view`] and threaded (as one `Rc`) through the
/// pane builders, the draw functions, and the interaction
/// gestures. Replaces the former practice of passing eight or
/// nine individual `Rc` handles per builder — same sharing
/// semantics (each field is still its own `Rc`/`Cell`/`RefCell`),
/// one name to capture per closure. Holds NO widget references,
/// so closures on the drawing areas that capture it can't form a
/// widget refcount cycle. Per the 8-parameter gate (#819,
/// PR #880 `ClientSetupDeps` precedent).
pub(super) struct SpectrumShared {
    pub(super) vfo_state: Rc<RefCell<VfoState>>,
    pub(super) fft_state: Rc<RefCell<Option<FftPlotState>>>,
    pub(super) waterfall_state: Rc<RefCell<Option<WaterfallState>>>,
    pub(super) signal_history_state: Rc<RefCell<Option<SignalHistoryState>>>,
    pub(super) min_db: Rc<Cell<f32>>,
    pub(super) max_db: Rc<Cell<f32>>,
    pub(super) fill_enabled: Rc<Cell<bool>>,
    pub(super) cursor_callback: CursorCallback,
    pub(super) vfo_offset_callback: VfoOffsetCallback,
    pub(super) locked_click_callback: LockedClickCallback,
    pub(super) full_bandwidth: Rc<Cell<f64>>,
    pub(super) center_freq: Rc<Cell<f64>>,
    pub(super) scanner_axis_lock: Rc<RefCell<Option<ScannerAxisLock>>>,
}

/// Fresh [`SpectrumShared`] with every field at its as-built
/// default. Split out of [`build_spectrum_view`] per the 50-NLOC
/// gate (#819).
fn new_spectrum_shared() -> Rc<SpectrumShared> {
    Rc::new(SpectrumShared {
        vfo_state: Rc::new(RefCell::new(VfoState::default())),
        fft_state: Rc::new(RefCell::new(None)),
        waterfall_state: Rc::new(RefCell::new(None)),
        signal_history_state: Rc::new(RefCell::new(None)),
        min_db: Rc::new(Cell::new(DEFAULT_MIN_DB)),
        max_db: Rc::new(Cell::new(DEFAULT_MAX_DB)),
        fill_enabled: Rc::new(Cell::new(true)),
        cursor_callback: Rc::new(RefCell::new(None)),
        vfo_offset_callback: Rc::new(RefCell::new(None)),
        locked_click_callback: Rc::new(RefCell::new(None)),
        full_bandwidth: Rc::new(Cell::new(0.0)),
        center_freq: Rc::new(Cell::new(100_000_000.0)), // default 100 MHz
        scanner_axis_lock: Rc::new(RefCell::new(None)),
    })
}

/// Initialize renderer state eagerly (no GL context needed).
/// Split out of [`build_spectrum_view`] per the 50-NLOC gate
/// (#819).
fn init_pane_states(shared: &SpectrumShared) {
    *shared.fft_state.borrow_mut() = Some(FftPlotState {
        renderer: FftPlotRenderer::new(),
        vfo_renderer: VfoOverlayRenderer::new(),
        current_data: vec![DEFAULT_MIN_DB; FFT_SIZE],
    });
    *shared.waterfall_state.borrow_mut() = Some(WaterfallState {
        renderer: {
            let mut r = WaterfallRenderer::new(FFT_SIZE);
            r.set_db_range(DEFAULT_MIN_DB, DEFAULT_MAX_DB);
            r
        },
        vfo_renderer: VfoOverlayRenderer::new(),
    });
    *shared.signal_history_state.borrow_mut() = Some(SignalHistoryState {
        renderer: SignalHistoryRenderer::new(),
    });
    tracing::info!("spectrum renderers initialized (Cairo)");
}

/// Build the spectrum view containing the FFT plot, waterfall display,
/// and a collapsible signal history graph.
///
/// Returns a `(gtk4::Box, SpectrumHandle)` — the box widget for layout,
/// and a handle for pushing real FFT/signal data into the display.
pub fn build_spectrum_view(
    dsp_tx: std::sync::mpsc::Sender<UiToDsp>,
) -> (gtk4::Box, SpectrumHandle) {
    let shared = new_spectrum_shared();
    init_pane_states(&shared);

    let fft_area = build_fft_area(&shared);
    let waterfall_area = build_waterfall_area(&shared);
    let signal_history_area = build_signal_history_area(&shared);

    // Attach interaction gestures to both the waterfall and FFT areas.
    attach_click_gesture(&waterfall_area, &shared, dsp_tx.clone());
    attach_drag_gesture(&waterfall_area, &shared, dsp_tx);
    // Scroll-to-zoom on both spectrum panes. They share `vfo_state`, so each
    // zoom redraws both panes to keep them in sync. Per #657.
    attach_scroll_gesture(&fft_area, &waterfall_area, &shared);

    let vfo_reset_button = build_vfo_reset_button();
    let outer_box = assemble_spectrum_layout(
        &fft_area,
        &waterfall_area,
        &signal_history_area,
        &vfo_reset_button,
    );

    let handle = spectrum_handle_from(
        &shared,
        fft_area,
        waterfall_area,
        signal_history_area,
        vfo_reset_button,
    );
    (outer_box, handle)
}

/// Assemble the [`SpectrumHandle`] from the shared state bundle
/// plus the built widgets. Each state field is an `Rc` clone of
/// the same cell the draw functions and gestures captured, so the
/// handle's mutations reflect immediately. Split out of
/// [`build_spectrum_view`] per the 50-NLOC gate (#819).
fn spectrum_handle_from(
    shared: &SpectrumShared,
    fft_area: gtk4::DrawingArea,
    waterfall_area: gtk4::DrawingArea,
    signal_history_area: gtk4::DrawingArea,
    vfo_reset_button: gtk4::Button,
) -> SpectrumHandle {
    SpectrumHandle {
        fft_state: Rc::clone(&shared.fft_state),
        waterfall_state: Rc::clone(&shared.waterfall_state),
        signal_history_state: Rc::clone(&shared.signal_history_state),
        vfo_state: Rc::clone(&shared.vfo_state),
        fft_area,
        waterfall_area,
        signal_history_area,
        min_db: Rc::clone(&shared.min_db),
        max_db: Rc::clone(&shared.max_db),
        fill_enabled: Rc::clone(&shared.fill_enabled),
        averaging_mode: Rc::new(Cell::new(AveragingMode::default())),
        avg_buffer: Rc::new(RefCell::new(Vec::new())),
        cursor_callback: Rc::clone(&shared.cursor_callback),
        vfo_offset_callback: Rc::clone(&shared.vfo_offset_callback),
        locked_click_callback: Rc::clone(&shared.locked_click_callback),
        full_bandwidth: Rc::clone(&shared.full_bandwidth),
        center_freq: Rc::clone(&shared.center_freq),
        scanner_axis_lock: Rc::clone(&shared.scanner_axis_lock),
        vfo_reset_button,
    }
}

/// Floating "Reset VFO" button in the top-right of the
/// spectrum area. Hidden by default; `window.rs` toggles
/// visibility whenever the VFO enters or leaves a non-default
/// state. Uses the `osd` CSS class for a translucent overlay
/// feel that doesn't steal too much visual weight from the
/// spectrum underneath. Split out of [`build_spectrum_view`] per
/// the 50-NLOC gate (#819).
fn build_vfo_reset_button() -> gtk4::Button {
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
    vfo_reset_button
}

/// Assemble the widget tree: FFT plot over waterfall in a
/// vertical `GtkPaned` (~30 % / ~70 % initial split), wrapped in
/// an `Overlay` so the floating reset button can sit on top of
/// both panes without shifting their layout, then stacked over
/// the collapsible signal-history expander in a vertical box.
/// Split out of [`build_spectrum_view`] per the 50-NLOC gate
/// (#819).
fn assemble_spectrum_layout(
    fft_area: &gtk4::DrawingArea,
    waterfall_area: &gtk4::DrawingArea,
    signal_history_area: &gtk4::DrawingArea,
    vfo_reset_button: &gtk4::Button,
) -> gtk4::Box {
    let paned = gtk4::Paned::builder()
        .orientation(gtk4::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();

    paned.set_start_child(Some(fft_area));
    paned.set_end_child(Some(waterfall_area));

    // Set the initial split position once the widget has a size.
    paned.connect_realize(|paned| {
        let height = paned.height();
        if height > 0 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pos = (f64::from(height) * FFT_PANE_FRACTION) as i32;
            paned.set_position(pos);
        }
    });

    // Wrap the paned in an Overlay so the floating button can
    // sit on top of both the FFT plot and the waterfall without
    // shifting their layout.
    let spectrum_overlay = gtk4::Overlay::builder().hexpand(true).vexpand(true).build();
    spectrum_overlay.set_child(Some(&paned));
    spectrum_overlay.add_overlay(vfo_reset_button);

    // Wrap the signal history DrawingArea in a collapsible expander.
    let expander = gtk4::Expander::builder()
        .label("Signal History")
        .expanded(true)
        .build();
    expander.set_child(Some(signal_history_area));

    // Combine the FFT+waterfall overlay and the signal history
    // expander into a vertical box.
    let outer_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    outer_box.append(&spectrum_overlay);
    outer_box.append(&expander);
    outer_box
}

/// Build the `DrawingArea` for the FFT power spectrum plot.
fn build_fft_area(shared: &Rc<SpectrumShared>) -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .build();
    install_fft_draw_func(&area, shared);
    install_cursor_readout(&area, shared);
    area
}

/// Set the FFT pane's draw function — called on every
/// `queue_draw()`. Split out of [`build_fft_area`] per the
/// 50-NLOC gate (#819).
fn install_fft_draw_func(area: &gtk4::DrawingArea, shared: &Rc<SpectrumShared>) {
    let shared = Rc::clone(shared);
    area.set_draw_func(move |_area, cr, width, height| {
        if let Some(s) = shared.fft_state.borrow_mut().as_mut() {
            // Scanner-axis lock takes precedence: when the
            // scanner is engaged, X axis is pinned to the
            // channel envelope and the narrow FFT data lands
            // in the active channel's slice. The VFO overlay
            // is suppressed in scanner mode — its meaning
            // (drag-to-tune within the current channel)
            // doesn't apply when the X axis represents a wide
            // multi-channel range. Per issue #516.
            let lock = *shared.scanner_axis_lock.borrow();
            if let Some(lock) = lock {
                s.renderer.render_locked(
                    cr,
                    &s.current_data,
                    width,
                    height,
                    shared.min_db.get(),
                    shared.max_db.get(),
                    shared.fill_enabled.get(),
                    shared.full_bandwidth.get(),
                    &lock,
                );
                return;
            }

            let vfo = shared.vfo_state.borrow();
            s.renderer.render(
                cr,
                &s.current_data,
                width,
                height,
                shared.min_db.get(),
                shared.max_db.get(),
                shared.fill_enabled.get(),
                vfo.display_start_hz,
                vfo.display_end_hz,
                shared.full_bandwidth.get(),
                shared.center_freq.get(),
            );

            s.vfo_renderer.render(cr, &vfo, width, height);
        }
    });
}

/// Cursor readout: track mouse motion over the FFT pane to
/// compute frequency and power for the registered cursor
/// callback. Split out of [`build_fft_area`] per the 50-NLOC
/// gate (#819).
fn install_cursor_readout(area: &gtk4::DrawingArea, shared: &Rc<SpectrumShared>) {
    let motion = gtk4::EventControllerMotion::new();
    let shared_motion = Rc::clone(shared);
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
        let lock = *shared_motion.scanner_axis_lock.borrow();
        let freq_hz = if let Some(lock) = lock {
            let frac = (x / width).clamp(0.0, 1.0);
            lock.min_hz + frac * (lock.max_hz - lock.min_hz)
        } else {
            let vfo = shared_motion.vfo_state.borrow();
            vfo.pixel_to_hz(x, width)
        };

        let lo = shared_motion.min_db.get();
        let hi = shared_motion.max_db.get();
        let db_range = hi - lo;
        // y=0 is top (max_db), y=height is bottom (min_db).
        #[allow(clippy::cast_possible_truncation)]
        let power_db = hi - (y as f32 / height as f32) * db_range;

        if let Some(cb) = shared_motion.cursor_callback.borrow().as_ref() {
            cb(freq_hz, power_db);
        }
    });

    let shared_leave = Rc::clone(shared);
    motion.connect_leave(move |_ctrl| {
        if let Some(cb) = shared_leave.cursor_callback.borrow().as_ref() {
            cb(0.0, f32::NEG_INFINITY);
        }
    });

    area.add_controller(motion);
}

/// Build the `DrawingArea` for the waterfall spectrogram.
fn build_waterfall_area(shared: &Rc<SpectrumShared>) -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .build();

    let shared = Rc::clone(shared);
    area.set_draw_func(move |_area, cr, width, height| {
        if let Some(s) = shared.waterfall_state.borrow().as_ref() {
            // Scanner-axis lock: pixels are pre-projected to
            // the wide locked range at push time, so the
            // renderer just needs a no-zoom call (display
            // matches full surface 1:1). VFO overlay is
            // suppressed because its drag-to-tune semantics
            // don't apply to a multi-channel range. Per issue
            // #516.
            let lock = *shared.scanner_axis_lock.borrow();
            if let Some(lock) = lock {
                let bw = shared.full_bandwidth.get();
                let half = bw / 2.0;
                s.renderer.render(cr, width, height, -half, half, bw);
                paint_scanner_highlight(cr, &lock, width, height);
                return;
            }

            let vfo = shared.vfo_state.borrow();
            s.renderer.render(
                cr,
                width,
                height,
                vfo.display_start_hz,
                vfo.display_end_hz,
                shared.full_bandwidth.get(),
            );

            s.vfo_renderer.render(cr, &vfo, width, height);
        }
    });

    area
}

/// Scanner-axis active-channel highlight band on the waterfall —
/// mirrors the FFT plot's `render_locked` band so the user has a
/// continuous visual anchor for "where is the scanner sampling
/// right now?" across both panels. Drawn AFTER the texture blit
/// so it overlays the data; spans the full height for visibility
/// while the historical rows scroll past underneath. Per
/// `CodeRabbit` round 3 on PR #562. Split out of
/// [`build_waterfall_area`] per the 50-NLOC gate (#819).
fn paint_scanner_highlight(
    cr: &gtk4::cairo::Context,
    lock: &ScannerAxisLock,
    width: i32,
    height: i32,
) {
    if let (Some(active_hz), Some(active_bw)) = (lock.active_channel_hz, lock.active_channel_bw_hz)
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
}

/// Build the `DrawingArea` for the signal strength history graph.
fn build_signal_history_area(shared: &Rc<SpectrumShared>) -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::builder()
        .hexpand(true)
        .vexpand(false)
        .height_request(SIGNAL_HISTORY_HEIGHT)
        .build();

    let shared = Rc::clone(shared);
    area.set_draw_func(move |_area, cr, width, height| {
        if let Some(s) = shared.signal_history_state.borrow().as_ref() {
            s.renderer
                .render(cr, width, height, shared.min_db.get(), shared.max_db.get());
        }
    });

    area
}
