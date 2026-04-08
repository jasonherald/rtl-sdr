//! VFO overlay for FFT plot and waterfall displays.
//!
//! Draws a semi-transparent passband rectangle, center frequency line, and
//! bandwidth handles on top of both spectrum views. Provides click-to-tune,
//! drag-to-move, bandwidth adjustment, and scroll-to-zoom interaction.

use glow::HasContext;

use super::gl_renderer::{self, GlError, f32_slice_as_bytes};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default VFO passband fill color (semi-transparent blue).
const VFO_COLOR: [f32; 4] = [0.2, 0.6, 1.0, 0.15];

/// VFO center frequency line color (brighter blue).
const VFO_CENTER_COLOR: [f32; 4] = [0.3, 0.7, 1.0, 0.5];

/// VFO bandwidth handle edge color.
const VFO_EDGE_COLOR: [f32; 4] = [0.5, 0.8, 1.0, 0.6];

/// Click zone width in pixels for grabbing a bandwidth handle.
const BW_HANDLE_THRESHOLD_PX: f64 = 8.0;

/// Default VFO bandwidth in Hz (NFM default).
const DEFAULT_BANDWIDTH_HZ: f64 = 12_500.0;

/// Minimum bandwidth the user can set, in Hz.
const MIN_BANDWIDTH_HZ: f64 = 500.0;

/// Maximum bandwidth the user can set, in Hz.
const MAX_BANDWIDTH_HZ: f64 = 250_000.0;

/// Default display span in Hz (1 MHz).
const DEFAULT_DISPLAY_SPAN_HZ: f64 = 1_000_000.0;

/// Zoom factor per scroll notch.
const ZOOM_FACTOR: f64 = 1.2;

/// Minimum display span in Hz to prevent zooming into nothing.
const MIN_DISPLAY_SPAN_HZ: f64 = 1_000.0;

/// Maximum display span in Hz.
const MAX_DISPLAY_SPAN_HZ: f64 = 50_000_000.0;

/// Maximum number of overlay vertices for any single draw call.
/// Each primitive is drawn separately: passband rect (4), center quad (4), edge lines (4).
const MAX_OVERLAY_VERTICES: usize = 4;

/// Maximum vertex buffer size in floats (2 floats per vertex).
const MAX_OVERLAY_FLOATS: usize = MAX_OVERLAY_VERTICES * 2;

// ---------------------------------------------------------------------------
// VFO state
// ---------------------------------------------------------------------------

/// Which bandwidth handle is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BwHandle {
    /// Left (lower frequency) edge.
    Left,
    /// Right (upper frequency) edge.
    Right,
}

/// State of a single VFO channel overlay.
#[derive(Debug, Clone)]
pub struct VfoState {
    /// Center frequency offset from display center, in Hz.
    pub offset_hz: f64,
    /// Passband bandwidth in Hz.
    pub bandwidth_hz: f64,
    /// Display frequency range start (relative to tuner center), in Hz.
    pub display_start_hz: f64,
    /// Display frequency range end (relative to tuner center), in Hz.
    pub display_end_hz: f64,
    /// VFO passband fill color (RGBA).
    pub color: [f32; 4],
    /// Whether the VFO center is currently being dragged.
    pub dragging: bool,
    /// Whether a bandwidth handle is being dragged.
    pub bw_dragging: Option<BwHandle>,
}

impl Default for VfoState {
    fn default() -> Self {
        Self {
            offset_hz: 0.0,
            bandwidth_hz: DEFAULT_BANDWIDTH_HZ,
            display_start_hz: -DEFAULT_DISPLAY_SPAN_HZ / 2.0,
            display_end_hz: DEFAULT_DISPLAY_SPAN_HZ / 2.0,
            color: VFO_COLOR,
            dragging: false,
            bw_dragging: None,
        }
    }
}

impl VfoState {
    /// Convert a frequency (in Hz, relative to display center) to clip-space X.
    ///
    /// Clip space ranges from -1.0 (left) to 1.0 (right).
    fn hz_to_clip_x(&self, hz: f64) -> f64 {
        let span = self.display_end_hz - self.display_start_hz;
        if span <= 0.0 {
            return 0.0;
        }
        2.0 * (hz - self.display_start_hz) / span - 1.0
    }

    /// Convert a pixel X coordinate to frequency in Hz.
    ///
    /// `width` is the viewport width in logical (not physical) pixels.
    pub fn pixel_to_hz(&self, pixel_x: f64, width: f64) -> f64 {
        if width <= 0.0 {
            return 0.0;
        }
        let frac = pixel_x / width;
        self.display_start_hz + frac * (self.display_end_hz - self.display_start_hz)
    }

    /// Convert a frequency offset in Hz to a pixel distance.
    pub fn hz_to_pixels(&self, hz: f64, width: f64) -> f64 {
        let span = self.display_end_hz - self.display_start_hz;
        if span <= 0.0 {
            return 0.0;
        }
        hz / span * width
    }

    /// Convert a pixel distance to frequency offset in Hz.
    pub fn pixels_to_hz(&self, pixels: f64, width: f64) -> f64 {
        let span = self.display_end_hz - self.display_start_hz;
        if width <= 0.0 {
            return 0.0;
        }
        pixels / width * span
    }

    /// Clip-space X for the left edge of the passband.
    fn left_clip_x(&self) -> f64 {
        self.hz_to_clip_x(self.offset_hz - self.bandwidth_hz / 2.0)
    }

    /// Clip-space X for the right edge of the passband.
    fn right_clip_x(&self) -> f64 {
        self.hz_to_clip_x(self.offset_hz + self.bandwidth_hz / 2.0)
    }

    /// Clip-space X for the VFO center line.
    fn center_clip_x(&self) -> f64 {
        self.hz_to_clip_x(self.offset_hz)
    }

    /// Pixel X for the left edge of the passband.
    fn left_pixel_x(&self, width: f64) -> f64 {
        clip_x_to_pixel(self.left_clip_x(), width)
    }

    /// Pixel X for the right edge of the passband.
    fn right_pixel_x(&self, width: f64) -> f64 {
        clip_x_to_pixel(self.right_clip_x(), width)
    }

    /// Determine what was clicked: a bandwidth handle, the passband body,
    /// or nothing (outside the VFO rectangle).
    pub fn hit_test(&self, pixel_x: f64, width: f64) -> HitZone {
        let left_px = self.left_pixel_x(width);
        let right_px = self.right_pixel_x(width);

        if (pixel_x - left_px).abs() <= BW_HANDLE_THRESHOLD_PX {
            HitZone::LeftHandle
        } else if (pixel_x - right_px).abs() <= BW_HANDLE_THRESHOLD_PX {
            HitZone::RightHandle
        } else if pixel_x >= left_px && pixel_x <= right_px {
            HitZone::Passband
        } else {
            HitZone::Outside
        }
    }

    /// Apply a scroll zoom centered at the given frequency.
    ///
    /// Positive `delta` zooms in, negative zooms out.
    pub fn zoom(&mut self, center_hz: f64, delta: f64) {
        if delta == 0.0 {
            return;
        }
        let factor = if delta > 0.0 {
            1.0 / ZOOM_FACTOR
        } else {
            ZOOM_FACTOR
        };

        let span = self.display_end_hz - self.display_start_hz;
        let new_span = (span * factor).clamp(MIN_DISPLAY_SPAN_HZ, MAX_DISPLAY_SPAN_HZ);

        // Keep the cursor frequency at the same relative position.
        let frac = if span > 0.0 {
            (center_hz - self.display_start_hz) / span
        } else {
            0.5
        };

        self.display_start_hz = center_hz - frac * new_span;
        self.display_end_hz = center_hz + (1.0 - frac) * new_span;
    }

    /// Clamp bandwidth to allowed range.
    pub fn clamp_bandwidth(&mut self) {
        self.bandwidth_hz = self.bandwidth_hz.clamp(MIN_BANDWIDTH_HZ, MAX_BANDWIDTH_HZ);
    }
}

/// Convert a clip-space X coordinate (-1..1) to pixel X coordinate (0..width).
fn clip_x_to_pixel(clip_x: f64, width: f64) -> f64 {
    // Equivalent to (clip_x + 1.0) / 2.0 * width, rewritten to avoid
    // triggering clippy::manual_midpoint.
    clip_x.mul_add(0.5, 0.5) * width
}

/// Result of a hit-test on the VFO overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitZone {
    /// Click landed on the left bandwidth handle.
    LeftHandle,
    /// Click landed on the right bandwidth handle.
    RightHandle,
    /// Click landed inside the passband (but not on a handle).
    Passband,
    /// Click landed outside the VFO overlay.
    Outside,
}

// ---------------------------------------------------------------------------
// VFO overlay renderer
// ---------------------------------------------------------------------------

/// OpenGL renderer for the VFO overlay.
///
/// Draws a semi-transparent passband rectangle, center line, and bandwidth
/// handle edges on top of the FFT plot or waterfall. Uses the same simple
/// position + uniform color shader as the FFT plot.
pub struct VfoOverlayRenderer {
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    color_location: glow::UniformLocation,
}

impl VfoOverlayRenderer {
    /// Create a new VFO overlay renderer.
    ///
    /// # Errors
    ///
    /// Returns `GlError` if shader compilation, linking, or buffer creation fails.
    #[allow(
        unsafe_code,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    pub fn new(gl: &glow::Context) -> Result<Self, GlError> {
        let vert = gl_renderer::compile_shader(gl, glow::VERTEX_SHADER, VERT_SHADER)?;
        let frag = gl_renderer::compile_shader(gl, glow::FRAGMENT_SHADER, FRAG_SHADER)?;
        let program = gl_renderer::link_program(gl, vert, frag)?;

        let color_location = unsafe {
            gl.get_uniform_location(program, "u_color")
                .ok_or_else(|| GlError::ResourceCreation("u_color uniform not found".into()))?
        };

        let (vao, vbo) = unsafe {
            let vao = gl
                .create_vertex_array()
                .map_err(|e| GlError::ResourceCreation(e.clone()))?;
            let vbo = gl
                .create_buffer()
                .map_err(|e| GlError::ResourceCreation(e.clone()))?;

            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));

            // Pre-allocate for the overlay geometry.
            let max_bytes = MAX_OVERLAY_FLOATS * std::mem::size_of::<f32>();
            gl.buffer_data_size(glow::ARRAY_BUFFER, max_bytes as i32, glow::STREAM_DRAW);

            // Vertex attribute: vec2 at location 0.
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(
                0,
                2,
                glow::FLOAT,
                false,
                (2 * std::mem::size_of::<f32>()) as i32,
                0,
            );

            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);

            (vao, vbo)
        };

        Ok(Self {
            program,
            vao,
            vbo,
            color_location,
        })
    }

    /// Render the VFO overlay on top of the current GL framebuffer.
    ///
    /// Must be called after the main FFT plot or waterfall has been rendered,
    /// while the GL context is still current.
    #[allow(
        unsafe_code,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    pub fn render(&self, gl: &glow::Context, vfo: &VfoState, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }

        let span = vfo.display_end_hz - vfo.display_start_hz;
        if span <= 0.0 {
            return;
        }

        unsafe {
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));

            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
        }

        // Draw passband fill rectangle.
        self.draw_passband_fill(gl, vfo);

        // Draw center frequency line.
        self.draw_center_line(gl, vfo, width);

        // Draw left and right bandwidth handle edges.
        self.draw_edge_lines(gl, vfo);

        unsafe {
            gl.disable(glow::BLEND);
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.use_program(None);
        }
    }

    /// Draw the semi-transparent passband fill as a triangle strip.
    #[allow(
        unsafe_code,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    fn draw_passband_fill(&self, gl: &glow::Context, vfo: &VfoState) {
        #[allow(clippy::cast_precision_loss)]
        let left_x = vfo.left_clip_x() as f32;
        #[allow(clippy::cast_precision_loss)]
        let right_x = vfo.right_clip_x() as f32;

        // Triangle strip: bottom-left, top-left, bottom-right, top-right.
        let vertices: [f32; 8] = [
            left_x, -1.0, // bottom-left
            left_x, 1.0, // top-left
            right_x, -1.0, // bottom-right
            right_x, 1.0, // top-right
        ];

        let bytes = f32_slice_as_bytes(&vertices);

        unsafe {
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytes);

            gl.uniform_4_f32(
                Some(&self.color_location),
                vfo.color[0],
                vfo.color[1],
                vfo.color[2],
                vfo.color[3],
            );

            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Draw the center frequency line as a thin quad for consistent width.
    ///
    /// Uses a triangle strip instead of `GL_LINES` with `line_width(2.0)`
    /// because OpenGL core profile only guarantees line width 1.0.
    #[allow(
        unsafe_code,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    fn draw_center_line(&self, gl: &glow::Context, vfo: &VfoState, width: i32) {
        #[allow(clippy::cast_precision_loss)]
        let cx = vfo.center_clip_x() as f32;

        // 1px wide quad in clip space — thin enough to see the signal behind it.
        #[allow(clippy::cast_precision_loss)]
        let half_w = if width > 0 { 1.0 / width as f32 } else { 0.001 };
        let vertices: [f32; 8] = [
            cx - half_w,
            -1.0,
            cx + half_w,
            -1.0,
            cx - half_w,
            1.0,
            cx + half_w,
            1.0,
        ];
        let bytes = f32_slice_as_bytes(&vertices);

        unsafe {
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytes);

            gl.uniform_4_f32(
                Some(&self.color_location),
                VFO_CENTER_COLOR[0],
                VFO_CENTER_COLOR[1],
                VFO_CENTER_COLOR[2],
                VFO_CENTER_COLOR[3],
            );

            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Draw left and right bandwidth handle edge lines.
    #[allow(
        unsafe_code,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    fn draw_edge_lines(&self, gl: &glow::Context, vfo: &VfoState) {
        #[allow(clippy::cast_precision_loss)]
        let left_x = vfo.left_clip_x() as f32;
        #[allow(clippy::cast_precision_loss)]
        let right_x = vfo.right_clip_x() as f32;

        let vertices: [f32; 8] = [
            left_x, -1.0, left_x, 1.0, // left edge
            right_x, -1.0, right_x, 1.0, // right edge
        ];
        let bytes = f32_slice_as_bytes(&vertices);

        unsafe {
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytes);

            gl.uniform_4_f32(
                Some(&self.color_location),
                VFO_EDGE_COLOR[0],
                VFO_EDGE_COLOR[1],
                VFO_EDGE_COLOR[2],
                VFO_EDGE_COLOR[3],
            );

            gl.draw_arrays(glow::LINES, 0, 4);
        }
    }

    /// Release GL resources.
    #[allow(unsafe_code)]
    pub fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_buffer(self.vbo);
            gl.delete_vertex_array(self.vao);
            gl.delete_program(self.program);
        }
    }
}

/// Vertex shader — identical to the FFT plot shader.
const VERT_SHADER: &str = r"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_pos;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
";

/// Fragment shader — identical to the FFT plot shader.
const FRAG_SHADER: &str = r"#version 300 es
precision highp float;
uniform vec4 u_color;
out vec4 frag_color;
void main() {
    frag_color = u_color;
}
";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a VFO with a known display range for testing.
    fn test_vfo() -> VfoState {
        VfoState {
            offset_hz: 0.0,
            bandwidth_hz: 10_000.0,
            display_start_hz: -500_000.0,
            display_end_hz: 500_000.0,
            color: VFO_COLOR,
            dragging: false,
            bw_dragging: None,
        }
    }

    #[test]
    fn hz_to_clip_x_center() {
        let vfo = test_vfo();
        let x = vfo.hz_to_clip_x(0.0);
        assert!((x - 0.0).abs() < 1e-10, "center should map to x=0, got {x}");
    }

    #[test]
    fn hz_to_clip_x_left_edge() {
        let vfo = test_vfo();
        let x = vfo.hz_to_clip_x(-500_000.0);
        assert!(
            (x - (-1.0)).abs() < 1e-10,
            "left edge should map to x=-1, got {x}"
        );
    }

    #[test]
    fn hz_to_clip_x_right_edge() {
        let vfo = test_vfo();
        let x = vfo.hz_to_clip_x(500_000.0);
        assert!(
            (x - 1.0).abs() < 1e-10,
            "right edge should map to x=1, got {x}"
        );
    }

    #[test]
    fn pixel_to_hz_center() {
        let vfo = test_vfo();
        let hz = vfo.pixel_to_hz(500.0, 1000.0);
        assert!(
            (hz - 0.0).abs() < 1e-6,
            "center pixel should map to 0 Hz, got {hz}"
        );
    }

    #[test]
    fn pixel_to_hz_left() {
        let vfo = test_vfo();
        let hz = vfo.pixel_to_hz(0.0, 1000.0);
        assert!(
            (hz - (-500_000.0)).abs() < 1e-6,
            "pixel 0 should map to -500kHz, got {hz}"
        );
    }

    #[test]
    fn pixel_to_hz_right() {
        let vfo = test_vfo();
        let hz = vfo.pixel_to_hz(1000.0, 1000.0);
        assert!(
            (hz - 500_000.0).abs() < 1e-6,
            "pixel 1000 should map to +500kHz, got {hz}"
        );
    }

    #[test]
    fn pixels_to_hz_round_trip() {
        let vfo = test_vfo();
        let hz = 50_000.0;
        let pixels = vfo.hz_to_pixels(hz, 1000.0);
        let back = vfo.pixels_to_hz(pixels, 1000.0);
        assert!(
            (back - hz).abs() < 1e-6,
            "round-trip failed: {hz} -> {pixels} px -> {back}"
        );
    }

    #[test]
    fn hit_test_outside() {
        let vfo = test_vfo();
        // VFO at center, 10kHz BW => left edge at pixel ~490, right at ~510 (1000px width).
        assert_eq!(vfo.hit_test(100.0, 1000.0), HitZone::Outside);
        assert_eq!(vfo.hit_test(900.0, 1000.0), HitZone::Outside);
    }

    #[test]
    fn hit_test_passband() {
        let mut vfo = test_vfo();
        // Use a wider bandwidth so the passband center is far from the edges.
        // 100kHz BW on 1MHz span = 100px on a 1000px display.
        // Left edge at 450, right edge at 550 — center at 500 is 50px from edges.
        vfo.bandwidth_hz = 100_000.0;
        assert_eq!(vfo.hit_test(500.0, 1000.0), HitZone::Passband);
    }

    #[test]
    fn hit_test_left_handle() {
        let vfo = test_vfo();
        // Left edge at 500 - (10000/1000000 * 1000 / 2) = 500 - 5 = 495.
        let left_px = vfo.left_pixel_x(1000.0);
        assert_eq!(vfo.hit_test(left_px, 1000.0), HitZone::LeftHandle);
    }

    #[test]
    fn hit_test_right_handle() {
        let vfo = test_vfo();
        let right_px = vfo.right_pixel_x(1000.0);
        assert_eq!(vfo.hit_test(right_px, 1000.0), HitZone::RightHandle);
    }

    #[test]
    fn zoom_in_narrows_span() {
        let mut vfo = test_vfo();
        let span_before = vfo.display_end_hz - vfo.display_start_hz;
        vfo.zoom(0.0, 1.0); // positive = zoom in
        let span_after = vfo.display_end_hz - vfo.display_start_hz;
        assert!(
            span_after < span_before,
            "zoom in should narrow span: {span_before} -> {span_after}"
        );
    }

    #[test]
    fn zoom_out_widens_span() {
        let mut vfo = test_vfo();
        let span_before = vfo.display_end_hz - vfo.display_start_hz;
        vfo.zoom(0.0, -1.0); // negative = zoom out
        let span_after = vfo.display_end_hz - vfo.display_start_hz;
        assert!(
            span_after > span_before,
            "zoom out should widen span: {span_before} -> {span_after}"
        );
    }

    #[test]
    fn zoom_clamps_to_min_span() {
        let mut vfo = test_vfo();
        // Zoom in many times.
        for _ in 0..200 {
            vfo.zoom(0.0, 1.0);
        }
        let span = vfo.display_end_hz - vfo.display_start_hz;
        assert!(
            span >= MIN_DISPLAY_SPAN_HZ,
            "span should not go below minimum: {span}"
        );
    }

    #[test]
    fn zoom_clamps_to_max_span() {
        let mut vfo = test_vfo();
        // Zoom out many times.
        for _ in 0..200 {
            vfo.zoom(0.0, -1.0);
        }
        let span = vfo.display_end_hz - vfo.display_start_hz;
        assert!(
            span <= MAX_DISPLAY_SPAN_HZ,
            "span should not exceed maximum: {span}"
        );
    }

    #[test]
    fn clamp_bandwidth_enforces_limits() {
        let mut vfo = test_vfo();

        vfo.bandwidth_hz = 100.0; // below minimum
        vfo.clamp_bandwidth();
        assert!(
            (vfo.bandwidth_hz - MIN_BANDWIDTH_HZ).abs() < 1e-10,
            "should clamp to min: {}",
            vfo.bandwidth_hz
        );

        vfo.bandwidth_hz = 1_000_000.0; // above maximum
        vfo.clamp_bandwidth();
        assert!(
            (vfo.bandwidth_hz - MAX_BANDWIDTH_HZ).abs() < 1e-10,
            "should clamp to max: {}",
            vfo.bandwidth_hz
        );
    }

    #[test]
    fn default_state_is_centered() {
        let vfo = VfoState::default();
        assert!(
            (vfo.offset_hz - 0.0).abs() < 1e-10,
            "default offset should be 0"
        );
        assert!(
            (vfo.bandwidth_hz - DEFAULT_BANDWIDTH_HZ).abs() < 1e-10,
            "default bandwidth should be {DEFAULT_BANDWIDTH_HZ}"
        );
        assert!(
            (vfo.display_end_hz - vfo.display_start_hz - DEFAULT_DISPLAY_SPAN_HZ).abs() < 1e-10,
            "default span should be {DEFAULT_DISPLAY_SPAN_HZ}"
        );
    }

    #[test]
    fn pixel_to_hz_zero_width_returns_zero() {
        let vfo = test_vfo();
        assert!((vfo.pixel_to_hz(100.0, 0.0)).abs() < 1e-10);
    }

    #[test]
    fn hz_to_pixels_zero_span_returns_zero() {
        let mut vfo = test_vfo();
        vfo.display_start_hz = 0.0;
        vfo.display_end_hz = 0.0;
        assert!((vfo.hz_to_pixels(100.0, 1000.0)).abs() < 1e-10);
    }

    #[test]
    fn zoom_preserves_cursor_position() {
        let mut vfo = test_vfo();
        // Zoom centered at 100 kHz offset.
        let cursor_hz = 100_000.0;
        let frac_before =
            (cursor_hz - vfo.display_start_hz) / (vfo.display_end_hz - vfo.display_start_hz);

        vfo.zoom(cursor_hz, 1.0);

        let frac_after =
            (cursor_hz - vfo.display_start_hz) / (vfo.display_end_hz - vfo.display_start_hz);

        assert!(
            (frac_before - frac_after).abs() < 1e-10,
            "cursor relative position should be preserved: {frac_before} vs {frac_after}"
        );
    }

    #[test]
    fn vfo_with_offset_renders_correctly() {
        let mut vfo = test_vfo();
        vfo.offset_hz = 100_000.0;
        // Center should be at clip x = 2 * (100000 - (-500000)) / 1000000 - 1 = 0.2
        let cx = vfo.center_clip_x();
        assert!(
            (cx - 0.2).abs() < 1e-10,
            "offset VFO center clip x should be 0.2, got {cx}"
        );
    }
}
