//! Spectrum display: FFT plot (top) + waterfall spectrogram (bottom).
//!
//! Both are rendered via `GtkGLArea` widgets using OpenGL through `glow`.
//! A `GtkPaned` splits them vertically, with the FFT plot on top (~30%)
//! and the waterfall below (~70%).

pub mod colormap;
pub mod fft_plot;
pub mod frequency_axis;
pub mod gl_renderer;
pub mod vfo_overlay;
pub mod waterfall;

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use fft_plot::FftPlotRenderer;
use vfo_overlay::{BwHandle, HitZone, VfoOverlayRenderer, VfoState};
use waterfall::WaterfallRenderer;

/// Number of FFT bins for the display.
const FFT_SIZE: usize = 1024;

/// Default FFT plot pane height fraction (30% of total).
const FFT_PANE_FRACTION: f64 = 0.30;

/// Minimum display level in dB.
const MIN_DB: f32 = -120.0;
/// Maximum display level in dB.
const MAX_DB: f32 = 0.0;

/// Interval between synthetic test frames, in milliseconds.
const TEST_FRAME_INTERVAL_MS: u64 = 16;

/// Width of synthetic signal peaks in bins.
const TEST_PEAK_WIDTH: usize = 20;
/// Center offset within the peak width.
const TEST_PEAK_CENTER: i32 = 10;
/// Noise floor level in dB for test data.
const TEST_NOISE_FLOOR_DB: f32 = -90.0;
/// Peak 1 signal level in dB.
const TEST_PEAK1_DB: f32 = -30.0;
/// Peak 2 signal level in dB.
const TEST_PEAK2_DB: f32 = -40.0;
/// dB falloff per bin distance from peak center.
const TEST_PEAK_FALLOFF_DB: f32 = -3.0;
/// Peak 1 drift rate in bins per frame.
const TEST_PEAK1_DRIFT: usize = 100;
/// Peak 2 drift rate in bins per frame.
const TEST_PEAK2_DRIFT: usize = 80;
/// Noise variation amplitude in dB.
const TEST_NOISE_VARIATION_DB: f32 = 3.0;
/// Noise variation frequency scaling factor.
const TEST_NOISE_FREQ_SCALE: f32 = 0.1;

/// Shared state for the FFT plot `GtkGLArea`.
struct FftPlotState {
    gl: glow::Context,
    renderer: FftPlotRenderer,
    vfo_renderer: VfoOverlayRenderer,
    current_data: Vec<f32>,
}

/// Shared state for the waterfall `GtkGLArea`.
struct WaterfallState {
    gl: glow::Context,
    renderer: WaterfallRenderer,
    vfo_renderer: VfoOverlayRenderer,
}

/// Build the spectrum view containing the FFT plot and waterfall display.
///
/// Returns a `GtkPaned` with vertical orientation: FFT plot on top,
/// waterfall on bottom. Both are `GtkGLArea` widgets rendered via OpenGL.
///
/// Currently drives both displays with synthetic test data for visual validation.
pub fn build_spectrum_view() -> gtk4::Paned {
    let vfo_state: Rc<RefCell<VfoState>> = Rc::new(RefCell::new(VfoState::default()));
    let fft_state: Rc<RefCell<Option<FftPlotState>>> = Rc::new(RefCell::new(None));
    let waterfall_state: Rc<RefCell<Option<WaterfallState>>> = Rc::new(RefCell::new(None));

    let fft_area = build_fft_area(Rc::clone(&fft_state), Rc::clone(&vfo_state));
    let waterfall_area = build_waterfall_area(Rc::clone(&waterfall_state), Rc::clone(&vfo_state));

    // Attach interaction gestures to both the waterfall and FFT areas.
    attach_click_gesture(&waterfall_area, &vfo_state);
    attach_drag_gesture(&waterfall_area, &vfo_state);
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

    // Start synthetic test data generation.
    start_test_data_timer(
        Rc::clone(&fft_state),
        Rc::clone(&waterfall_state),
        &fft_area,
        &waterfall_area,
    );

    paned
}

/// Build the `GtkGLArea` for the FFT power spectrum plot.
fn build_fft_area(
    state: Rc<RefCell<Option<FftPlotState>>>,
    vfo_state: Rc<RefCell<VfoState>>,
) -> gtk4::GLArea {
    let area = gtk4::GLArea::builder()
        .hexpand(true)
        .vexpand(true)
        .auto_render(false)
        .build();
    area.set_required_version(3, 0);

    // On realize: create GL context and renderer.
    let state_realize = Rc::clone(&state);
    area.connect_realize(move |area| {
        area.make_current();
        if area.error().is_some() {
            tracing::warn!("FFT GLArea has error after make_current");
            return;
        }

        match create_gl_context_and_fft_renderer() {
            Ok((gl, renderer, vfo_renderer)) => {
                *state_realize.borrow_mut() = Some(FftPlotState {
                    gl,
                    renderer,
                    vfo_renderer,
                    current_data: vec![MIN_DB; FFT_SIZE],
                });
                tracing::info!("FFT plot GL renderer initialized");
            }
            Err(e) => {
                tracing::warn!("failed to initialize FFT plot renderer: {e}");
            }
        }
    });

    // On unrealize: clean up GL resources.
    let state_unrealize = Rc::clone(&state);
    area.connect_unrealize(move |area| {
        area.make_current();
        if area.error().is_some() {
            tracing::warn!("FFT GLArea error on unrealize — skipping GL cleanup");
        } else if let Some(s) = state_unrealize.borrow().as_ref() {
            s.vfo_renderer.destroy(&s.gl);
            s.renderer.destroy(&s.gl);
            tracing::info!("FFT plot GL renderer destroyed");
        }
        *state_unrealize.borrow_mut() = None;
    });

    // On render: draw the FFT plot, then the VFO overlay on top.
    area.connect_render(move |area, _ctx| {
        if let Some(s) = state.borrow().as_ref() {
            let width = area.width();
            let height = area.height();
            let scale = area.scale_factor();
            let phys_w = width * scale;
            let phys_h = height * scale;

            s.renderer
                .render(&s.gl, &s.current_data, phys_w, phys_h, MIN_DB, MAX_DB);

            let vfo = vfo_state.borrow();
            s.vfo_renderer.render(&s.gl, &vfo, phys_w, phys_h);
        }
        glib::Propagation::Stop
    });

    area
}

/// Build the `GtkGLArea` for the waterfall spectrogram.
fn build_waterfall_area(
    state: Rc<RefCell<Option<WaterfallState>>>,
    vfo_state: Rc<RefCell<VfoState>>,
) -> gtk4::GLArea {
    let area = gtk4::GLArea::builder()
        .hexpand(true)
        .vexpand(true)
        .auto_render(false)
        .build();
    area.set_required_version(3, 0);

    // On realize: create GL context and renderer.
    let state_realize = Rc::clone(&state);
    area.connect_realize(move |area| {
        area.make_current();
        if area.error().is_some() {
            tracing::warn!("waterfall GLArea has error after make_current");
            return;
        }

        match create_gl_context_and_waterfall_renderer() {
            Ok((gl, renderer, vfo_renderer)) => {
                *state_realize.borrow_mut() = Some(WaterfallState {
                    gl,
                    renderer,
                    vfo_renderer,
                });
                tracing::info!("waterfall GL renderer initialized");
            }
            Err(e) => {
                tracing::warn!("failed to initialize waterfall renderer: {e}");
            }
        }
    });

    // On unrealize: clean up GL resources.
    let state_unrealize = Rc::clone(&state);
    area.connect_unrealize(move |area| {
        area.make_current();
        if area.error().is_some() {
            tracing::warn!("waterfall GLArea error on unrealize — skipping GL cleanup");
        } else if let Some(s) = state_unrealize.borrow().as_ref() {
            s.vfo_renderer.destroy(&s.gl);
            s.renderer.destroy(&s.gl);
            tracing::info!("waterfall GL renderer destroyed");
        }
        *state_unrealize.borrow_mut() = None;
    });

    // On render: draw the waterfall, then the VFO overlay on top.
    area.connect_render(move |area, _ctx| {
        if let Some(s) = state.borrow().as_ref() {
            let width = area.width();
            let height = area.height();
            let scale = area.scale_factor();
            let phys_w = width * scale;
            let phys_h = height * scale;

            s.renderer.render(&s.gl, phys_w, phys_h);

            let vfo = vfo_state.borrow();
            s.vfo_renderer.render(&s.gl, &vfo, phys_w, phys_h);
        }
        glib::Propagation::Stop
    });

    area
}

/// Create a glow GL context from the current GDK GL context.
///
/// Must be called after `GtkGLArea::make_current()`.
///
/// Uses `dlsym(RTLD_DEFAULT, ...)` to resolve OpenGL function pointers from
/// the GL library already loaded by GTK4's display backend. At runtime, GTK4
/// has already loaded libEGL + libGLESv2 (Wayland) or libGL (X11), so all
/// GL symbols are available via the dynamic linker's global scope.
#[allow(unsafe_code)]
fn create_glow_context() -> glow::Context {
    unsafe {
        glow::Context::from_loader_function_cstr(|name| {
            // dlsym with RTLD_DEFAULT searches all loaded shared objects.
            // GTK4 has already loaded the platform GL library (libGL, libGLESv2,
            // or libEGL) so GL entry points are resolvable.
            dlsym(RTLD_DEFAULT, name.as_ptr())
        })
    }
}

/// Handle for `dlsym(RTLD_DEFAULT, ...)` — search all loaded shared objects.
#[allow(unsafe_code)]
const RTLD_DEFAULT: *mut std::os::raw::c_void = std::ptr::null_mut();

#[allow(unsafe_code)]
unsafe extern "C" {
    /// POSIX `dlsym` — resolve a symbol from a shared object handle.
    fn dlsym(
        handle: *mut std::os::raw::c_void,
        symbol: *const std::os::raw::c_char,
    ) -> *const std::os::raw::c_void;
}

/// Create a glow context, FFT plot renderer, and VFO overlay renderer.
fn create_gl_context_and_fft_renderer()
-> Result<(glow::Context, FftPlotRenderer, VfoOverlayRenderer), gl_renderer::GlError> {
    let gl = create_glow_context();
    let renderer = FftPlotRenderer::new(&gl)?;
    let vfo_renderer = VfoOverlayRenderer::new(&gl)?;
    Ok((gl, renderer, vfo_renderer))
}

/// Create a glow context, waterfall renderer, and VFO overlay renderer.
fn create_gl_context_and_waterfall_renderer()
-> Result<(glow::Context, WaterfallRenderer, VfoOverlayRenderer), gl_renderer::GlError> {
    let gl = create_glow_context();
    let renderer = WaterfallRenderer::new(&gl, FFT_SIZE)?;
    let vfo_renderer = VfoOverlayRenderer::new(&gl)?;
    Ok((gl, renderer, vfo_renderer))
}

/// Attach a click-to-tune gesture to a `GtkGLArea`.
///
/// Single-clicking sets the VFO center to the clicked frequency.
fn attach_click_gesture(area: &gtk4::GLArea, vfo_state: &Rc<RefCell<VfoState>>) {
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
        tracing::debug!(offset_hz = vfo.offset_hz, "click-to-tune");
        drop(vfo);

        // Request re-render. The periodic test-data timer also triggers
        // re-renders of both areas, so the FFT plot overlay updates too.
        area.queue_render();
    });

    area.add_controller(click);
}

/// Attach a drag gesture for VFO center movement and bandwidth handle adjustment.
fn attach_drag_gesture(area: &gtk4::GLArea, vfo_state: &Rc<RefCell<VfoState>>) {
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
    drag.connect_drag_update(move |_gesture, offset_x, _offset_y| {
        let Some(area) = area_weak_update.upgrade() else {
            return;
        };
        let width = f64::from(area.width());
        let mut vfo = vfo_update.borrow_mut();

        if vfo.dragging {
            let delta_hz = vfo.pixels_to_hz(offset_x, width);
            vfo.offset_hz = start_offset_update.get() + delta_hz;
            area.queue_render();
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
            area.queue_render();
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

/// Attach a scroll-to-zoom gesture to a `GtkGLArea`.
///
/// Scrolling zooms the frequency display range centered on the cursor position.
fn attach_scroll_gesture(area: &gtk4::GLArea, vfo_state: &Rc<RefCell<VfoState>>) {
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
        area.queue_render();

        glib::Propagation::Stop
    });

    area.add_controller(scroll);
}

/// Start a periodic timer that generates synthetic FFT test data and pushes
/// it to both renderers. This provides visual validation until real DSP data
/// is connected.
fn start_test_data_timer(
    fft_state: Rc<RefCell<Option<FftPlotState>>>,
    waterfall_state: Rc<RefCell<Option<WaterfallState>>>,
    fft_area: &gtk4::GLArea,
    waterfall_area: &gtk4::GLArea,
) {
    let frame_counter = Rc::new(std::cell::Cell::new(0_usize));

    // Use weak references so the timer stops when the GLAreas are dropped.
    let fft_area_weak = fft_area.downgrade();
    let waterfall_area_weak = waterfall_area.downgrade();

    glib::timeout_add_local(
        std::time::Duration::from_millis(TEST_FRAME_INTERVAL_MS),
        move || {
            // Stop the timer if either GLArea has been destroyed.
            let (Some(fft_area), Some(waterfall_area)) =
                (fft_area_weak.upgrade(), waterfall_area_weak.upgrade())
            else {
                return glib::ControlFlow::Break;
            };

            let frame = frame_counter.get();
            frame_counter.set(frame.wrapping_add(1));

            let data = generate_test_fft(FFT_SIZE, frame);

            // Update FFT plot data.
            if let Some(s) = fft_state.borrow_mut().as_mut() {
                s.current_data.clear();
                s.current_data.extend_from_slice(&data);
            }
            fft_area.queue_render();

            // Push a new line to the waterfall.
            if let Some(s) = waterfall_state.borrow_mut().as_mut() {
                waterfall_area.make_current();
                s.renderer.push_line(&s.gl, &data);
            }
            waterfall_area.queue_render();

            glib::ControlFlow::Continue
        },
    );
}

/// Generate synthetic FFT test data for visual validation.
///
/// Produces a noise floor with two moving peaks, suitable for verifying
/// that the spectrum display and waterfall are rendering correctly.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
fn generate_test_fft(size: usize, frame: usize) -> Vec<f32> {
    let mut data = vec![TEST_NOISE_FLOOR_DB; size];

    // Two peaks that slowly drift across the spectrum.
    let peak1_center = (size / 4 + frame % TEST_PEAK1_DRIFT) % size;
    let peak2_center = (3 * size / 4 + size - frame % TEST_PEAK2_DRIFT) % size;

    for i in 0..TEST_PEAK_WIDTH {
        let dist = (TEST_PEAK_CENTER - i as i32).abs() as f32;

        let idx1 = (peak1_center + i) % size;
        data[idx1] = TEST_PEAK1_DB + dist * TEST_PEAK_FALLOFF_DB;

        let idx2 = (peak2_center + i) % size;
        data[idx2] = TEST_PEAK2_DB + dist * TEST_PEAK_FALLOFF_DB;
    }

    // Add subtle noise variation.
    let variation = (frame as f32 * TEST_NOISE_FREQ_SCALE).sin() * TEST_NOISE_VARIATION_DB;
    for d in &mut data {
        *d += variation;
    }

    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_test_fft_size() {
        let data = generate_test_fft(FFT_SIZE, 0);
        assert_eq!(data.len(), FFT_SIZE);
    }

    #[test]
    fn test_generate_test_fft_has_peaks() {
        let data = generate_test_fft(FFT_SIZE, 0);
        let max_val = data.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let min_val = data.iter().copied().fold(f32::INFINITY, f32::min);
        // Peaks should be well above the noise floor.
        assert!(
            max_val > TEST_NOISE_FLOOR_DB + 20.0,
            "max value {max_val} should be well above noise floor"
        );
        // There should be dynamic range.
        assert!(
            max_val - min_val > 30.0,
            "dynamic range {} dB is too small",
            max_val - min_val
        );
    }

    #[test]
    fn test_generate_test_fft_peaks_move() {
        let data_frame_start = generate_test_fft(FFT_SIZE, 0);
        let data_frame_later = generate_test_fft(FFT_SIZE, 50);

        // Find peak positions.
        let peak_at_start = data_frame_start
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i);
        let peak_at_later = data_frame_later
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i);

        // Peaks should have moved between frames.
        assert_ne!(
            peak_at_start, peak_at_later,
            "peaks should drift between frames"
        );
    }
}
