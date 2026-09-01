//! Pointer interaction for the spectrum display (issue #819):
//! click-to-tune (VFO and scanner-locked absolute variants),
//! VFO passband / bandwidth-handle drag, and scroll-to-zoom.
//! Split out of `spectrum/mod.rs` per the file-size pass; the
//! gesture installers take the [`SpectrumShared`] bundle (per the
//! 8-parameter gate, PR #880 deps-struct precedent) and each
//! signal's body lives in its own handler function per the
//! 50-NLOC gate.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use super::ZOOMED_IN_SPAN_RATIO_THRESHOLD;
use super::build::SpectrumShared;
use super::vfo_overlay::{BwHandle, HitZone, VfoState};
use crate::messages::UiToDsp;

/// Attach a click-to-tune gesture to a `DrawingArea`.
///
/// Single-clicking sets the VFO center to the clicked frequency.
pub(super) fn attach_click_gesture(
    area: &gtk4::DrawingArea,
    shared: &Rc<SpectrumShared>,
    dsp_tx: std::sync::mpsc::Sender<UiToDsp>,
) {
    let click = gtk4::GestureClick::new();
    let shared = Rc::clone(shared);
    let area_weak = area.downgrade();
    click.connect_pressed(move |_gesture, _n_press, x, _y| {
        let Some(area) = area_weak.upgrade() else {
            return;
        };
        on_spectrum_click(&area, &shared, &dsp_tx, x);
    });
    area.add_controller(click);
}

/// Body of the click-to-tune press handler. Split out of
/// [`attach_click_gesture`] per the 50-NLOC gate (#819).
fn on_spectrum_click(
    area: &gtk4::DrawingArea,
    shared: &SpectrumShared,
    dsp_tx: &std::sync::mpsc::Sender<UiToDsp>,
    x: f64,
) {
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
    let lock_snapshot = *shared.scanner_axis_lock.borrow();
    if let Some(lock) = lock_snapshot {
        locked_click_to_tune(area, shared, &lock, x, width);
        return;
    }

    let mut vfo = shared.vfo_state.borrow_mut();
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
    if let Some(cb) = shared.vfo_offset_callback.borrow().as_ref() {
        cb(offset);
    }

    area.queue_draw();
}

/// Scanner-locked click: map the click's X fraction onto the
/// locked absolute range and hand it to the wiring layer's
/// locked-click callback (which force-disables the scanner and
/// dispatches the absolute tune — see [`on_spectrum_click`]'s
/// rationale). Split out per the 50-NLOC gate (#819).
fn locked_click_to_tune(
    area: &gtk4::DrawingArea,
    shared: &SpectrumShared,
    lock: &super::handle::ScannerAxisLock,
    x: f64,
    width: f64,
) {
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
    if let Some(cb) = shared.locked_click_callback.borrow().as_ref() {
        cb(abs_freq_hz);
    }
    area.queue_draw();
}

/// Attach a drag gesture for VFO center movement and bandwidth handle adjustment.
pub(super) fn attach_drag_gesture(
    area: &gtk4::DrawingArea,
    shared: &Rc<SpectrumShared>,
    dsp_tx: std::sync::mpsc::Sender<UiToDsp>,
) {
    let drag = gtk4::GestureDrag::new();

    // Snapshot of VFO state at drag start, for computing deltas.
    let drag_start_offset_hz: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));
    let drag_start_bw_hz: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));

    // On drag begin: determine if we're dragging a handle or the passband.
    let shared_begin = Rc::clone(shared);
    let start_offset = Rc::clone(&drag_start_offset_hz);
    let start_bw = Rc::clone(&drag_start_bw_hz);
    let area_weak_begin = area.downgrade();
    drag.connect_drag_begin(move |_gesture, x, _y| {
        let Some(area) = area_weak_begin.upgrade() else {
            return;
        };
        on_drag_begin(&area, &shared_begin, &start_offset, &start_bw, x);
    });

    // On drag update: move VFO or adjust bandwidth.
    let shared_update = Rc::clone(shared);
    let start_offset_update = Rc::clone(&drag_start_offset_hz);
    let start_bw_update = Rc::clone(&drag_start_bw_hz);
    let area_weak_update = area.downgrade();
    drag.connect_drag_update(move |_gesture, offset_x, _offset_y| {
        let Some(area) = area_weak_update.upgrade() else {
            return;
        };
        on_drag_update(
            &area,
            &shared_update,
            &dsp_tx,
            &start_offset_update,
            &start_bw_update,
            offset_x,
        );
    });

    // On drag end: clear drag state.
    let shared_end = Rc::clone(shared);
    drag.connect_drag_end(move |_gesture, _offset_x, _offset_y| {
        let mut vfo = shared_end.vfo_state.borrow_mut();
        vfo.dragging = false;
        vfo.bw_dragging = None;
    });

    area.add_controller(drag);
}

/// Drag-begin half of the VFO drag gesture: hit-test the press
/// position against the passband / bandwidth handles and record
/// the starting offset + bandwidth for delta math. Split out of
/// [`attach_drag_gesture`] per the 50-NLOC gate (#819).
fn on_drag_begin(
    area: &gtk4::DrawingArea,
    shared: &SpectrumShared,
    start_offset: &Cell<f64>,
    start_bw: &Cell<f64>,
    x: f64,
) {
    // Suppress drag entirely while the scanner-axis lock
    // is engaged. Drag-VFO and drag-bandwidth are inherently
    // single-channel operations; the wide multi-channel
    // axis has no compatible meaning. The user can still
    // click-to-tune (see `attach_click_gesture`'s lock-aware
    // path), which force-disables the scanner before
    // tuning. Per issue #563.
    if shared.scanner_axis_lock.borrow().is_some() {
        let mut vfo = shared.vfo_state.borrow_mut();
        vfo.dragging = false;
        vfo.bw_dragging = None;
        return;
    }
    let width = f64::from(area.width());
    let mut vfo = shared.vfo_state.borrow_mut();
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
}

/// Drag-update half of the VFO drag gesture: move the passband or
/// resize via a bandwidth handle, dispatching the offset /
/// bandwidth to the DSP thread and the UI callback. Split out of
/// [`attach_drag_gesture`] per the 50-NLOC gate (#819).
fn on_drag_update(
    area: &gtk4::DrawingArea,
    shared: &SpectrumShared,
    dsp_tx: &std::sync::mpsc::Sender<UiToDsp>,
    start_offset: &Cell<f64>,
    start_bw: &Cell<f64>,
    offset_x: f64,
) {
    // Re-check the lock at every update tick. If the
    // scanner engaged mid-gesture (e.g. a Doppler retune
    // hit at the same moment the user started dragging),
    // bail and clear the local drag flags so subsequent
    // updates also short-circuit. Without this, a drag
    // begun before the lock engaged would keep emitting
    // `SetVfoOffset` / `SetBandwidth` against a wide
    // multi-channel axis where the math is meaningless.
    // Per `CodeRabbit` round 1 on PR #565.
    if shared.scanner_axis_lock.borrow().is_some() {
        let mut vfo = shared.vfo_state.borrow_mut();
        vfo.dragging = false;
        vfo.bw_dragging = None;
        return;
    }
    let width = f64::from(area.width());
    let mut vfo = shared.vfo_state.borrow_mut();

    if vfo.dragging {
        let delta_hz = vfo.pixels_to_hz(offset_x, width);
        vfo.offset_hz = start_offset.get() + delta_hz;
        let offset = vfo.offset_hz;
        let _ = dsp_tx.send(UiToDsp::SetVfoOffset(offset));
        drop(vfo);
        if let Some(cb) = shared.vfo_offset_callback.borrow().as_ref() {
            cb(offset);
        }
        area.queue_draw();
    } else if let Some(handle) = vfo.bw_dragging {
        let delta_hz = vfo.pixels_to_hz(offset_x, width);
        apply_bw_handle_drag(
            &mut vfo,
            handle,
            delta_hz,
            start_bw.get(),
            start_offset.get(),
        );
        let offset = vfo.offset_hz;
        let _ = dsp_tx.send(UiToDsp::SetVfoOffset(offset));
        let _ = dsp_tx.send(UiToDsp::SetBandwidth(vfo.bandwidth_hz));
        drop(vfo);
        if let Some(cb) = shared.vfo_offset_callback.borrow().as_ref() {
            cb(offset);
        }
        area.queue_draw();
    }
}

/// Apply a bandwidth-handle drag to the VFO: the dragged edge
/// moves by `delta_hz` while the opposite edge stays fixed, so
/// both the bandwidth and the centre offset update together
/// (post-clamp, the fixed edge is re-derived from the clamped
/// width). Split out of [`attach_drag_gesture`] per the 50-NLOC
/// gate (#819).
fn apply_bw_handle_drag(
    vfo: &mut VfoState,
    handle: BwHandle,
    delta_hz: f64,
    original_bw: f64,
    original_offset: f64,
) {
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
}

/// Attach a scroll-to-zoom gesture to a `DrawingArea`.
///
/// Scrolling zooms the frequency display range centered on the cursor position.
///
/// Attaches to BOTH spectrum panes (FFT plot + waterfall). They share one
/// `VfoState`, so a zoom on either pane must redraw both — otherwise the pane
/// that didn't receive the scroll keeps rendering the previous zoom range and
/// its VFO overlay / frequency axis drift out of alignment with the zoomed
/// pane. Most visible while paused, when no fresh FFT frames force a redraw.
/// Per #657.
pub(super) fn attach_scroll_gesture(
    fft_area: &gtk4::DrawingArea,
    waterfall_area: &gtk4::DrawingArea,
    shared: &Rc<SpectrumShared>,
) {
    for target in [fft_area, waterfall_area] {
        let scroll = gtk4::EventControllerScroll::new(
            gtk4::EventControllerScrollFlags::VERTICAL | gtk4::EventControllerScrollFlags::DISCRETE,
        );

        let shared = Rc::clone(shared);
        let target_weak = target.downgrade();
        let fft_weak = fft_area.downgrade();
        let waterfall_weak = waterfall_area.downgrade();
        scroll.connect_scroll(move |_controller, _dx, dy| {
            let Some(target) = target_weak.upgrade() else {
                return glib::Propagation::Stop;
            };
            let width = f64::from(target.width());

            // TODO: Anchor zoom on cursor position instead of display center.
            // GTK4 EventControllerScroll doesn't provide position in the scroll
            // signal. Add an EventControllerMotion to track the pointer and use
            // its last-known X coordinate here for cursor-centered zoom.
            let cursor_x = width / 2.0;

            let mut vfo = shared.vfo_state.borrow_mut();
            let cursor_hz = vfo.pixel_to_hz(cursor_x, width);

            // dy > 0 = scroll down = zoom out; dy < 0 = scroll up = zoom in.
            vfo.zoom(cursor_hz, -dy);
            drop(vfo);

            // Redraw BOTH panes — they share `vfo_state`, so a one-sided
            // queue_draw would strand the other pane on the old zoom. Per #657.
            if let Some(a) = fft_weak.upgrade() {
                a.queue_draw();
            }
            if let Some(a) = waterfall_weak.upgrade() {
                a.queue_draw();
            }

            glib::Propagation::Stop
        });

        target.add_controller(scroll);
    }
}
