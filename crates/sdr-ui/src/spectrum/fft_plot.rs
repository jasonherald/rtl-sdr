//! FFT spectrum plot renderer using OpenGL via glow.
//!
//! Draws the power spectrum as a filled area with a line trace on top,
//! plus horizontal dB grid lines and vertical frequency grid lines.

use glow::HasContext;

use super::gl_renderer::{self, GlError, f32_slice_as_bytes};

/// Maximum bins for display rendering (limits vertex count).
/// FFT data wider than this is max-pooled down before drawing.
const MAX_DISPLAY_BINS: usize = 4096;

/// Number of horizontal dB grid lines.
const DB_GRID_LINE_COUNT: usize = 8;

/// Number of vertical frequency grid lines.
const FREQ_GRID_LINE_COUNT: usize = 10;

/// Maximum vertices for the filled spectrum area (2 per bin: top + bottom).
const MAX_FILL_VERTICES: usize = MAX_DISPLAY_BINS * 2;

/// Maximum vertices for grid lines (2 per line, both axes).
const MAX_GRID_VERTICES: usize = (DB_GRID_LINE_COUNT + FREQ_GRID_LINE_COUNT) * 2;

// Colors (RGBA, 0.0..1.0)
/// Spectrum trace line color — accent blue.
const TRACE_COLOR: [f32; 4] = [0.3, 0.7, 1.0, 1.0];
/// Spectrum fill color — semi-transparent blue.
const FILL_COLOR: [f32; 4] = [0.2, 0.4, 0.8, 0.35];
/// Grid line color — dim gray.
const GRID_COLOR: [f32; 4] = [0.4, 0.4, 0.4, 0.5];
/// Background clear color — near-black.
const BACKGROUND_COLOR: [f32; 4] = [0.08, 0.08, 0.10, 1.0];

/// Vertex shader — maps 2D positions directly to clip space.
const VERT_SHADER: &str = r"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_pos;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
";

/// Fragment shader — outputs a uniform color.
const FRAG_SHADER: &str = r"#version 300 es
precision highp float;
uniform vec4 u_color;
out vec4 frag_color;
void main() {
    frag_color = u_color;
}
";

/// Downsample FFT data by max-pooling bins to fit display width.
///
/// When the input has more bins than `MAX_DISPLAY_BINS`, groups of bins
/// are reduced to a single bin by taking the maximum dB value in each group.
/// This preserves signal peaks. Returns a slice of the downsampled buffer,
/// or the original data if no downsampling is needed.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn downsample_fft<'a>(data: &'a [f32], buf: &'a mut Vec<f32>) -> &'a [f32] {
    if data.len() <= MAX_DISPLAY_BINS {
        return data;
    }
    let out_bins = MAX_DISPLAY_BINS;
    buf.resize(out_bins, f32::NEG_INFINITY);
    let ratio = data.len() as f32 / out_bins as f32;
    for (i, out) in buf.iter_mut().enumerate().take(out_bins) {
        let start = (i as f32 * ratio) as usize;
        let end = (((i + 1) as f32) * ratio) as usize;
        let end = end.min(data.len());
        let mut max_val = f32::NEG_INFINITY;
        for &v in &data[start..end] {
            if v > max_val {
                max_val = v;
            }
        }
        *out = max_val;
    }
    buf
}

/// OpenGL renderer for the FFT power spectrum plot.
///
/// Renders a filled area under the spectrum curve, a line trace on top,
/// and grid lines for dB and frequency reference.
pub struct FftPlotRenderer {
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    color_location: glow::UniformLocation,
    /// Pre-allocated buffer for downsampling large FFT data.
    downsample_buf: Vec<f32>,
    // Pre-allocated vertex staging buffers to avoid per-frame allocation.
    grid_vertices: Vec<f32>,
    fill_vertices: Vec<f32>,
    trace_vertices: Vec<f32>,
}

impl FftPlotRenderer {
    /// Create a new FFT plot renderer, compiling shaders and allocating GL buffers.
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

            // Pre-allocate buffer for the largest usage (fill vertices: 2 floats each).
            let max_bytes = MAX_FILL_VERTICES * 2 * std::mem::size_of::<f32>();
            gl.buffer_data_size(glow::ARRAY_BUFFER, max_bytes as i32, glow::DYNAMIC_DRAW);

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
            downsample_buf: Vec::with_capacity(MAX_DISPLAY_BINS),
            grid_vertices: Vec::with_capacity(MAX_GRID_VERTICES * 2),
            fill_vertices: Vec::with_capacity(MAX_DISPLAY_BINS * 4),
            trace_vertices: Vec::with_capacity(MAX_DISPLAY_BINS * 2),
        })
    }

    /// Render the FFT spectrum plot.
    ///
    /// # Arguments
    ///
    /// * `gl` — The glow GL context.
    /// * `fft_data` — Power spectrum values in dB (one per frequency bin).
    /// * `width` — Viewport width in pixels.
    /// * `height` — Viewport height in pixels.
    /// * `min_db` — Bottom of the display range in dB.
    /// * `max_db` — Top of the display range in dB.
    #[allow(unsafe_code, clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        gl: &glow::Context,
        fft_data: &[f32],
        width: i32,
        height: i32,
        min_db: f32,
        max_db: f32,
        fill_enabled: bool,
    ) {
        if fft_data.is_empty() || width <= 0 || height <= 0 {
            return;
        }

        let db_range = max_db - min_db;
        if db_range <= 0.0 {
            return;
        }

        // Downsample large FFTs to limit vertex count.
        // Take the buffer out of self to avoid holding a mutable borrow
        // across the draw calls that also borrow self.
        let mut ds_buf = std::mem::take(&mut self.downsample_buf);
        let display_data = downsample_fft(fft_data, &mut ds_buf);

        unsafe {
            gl.viewport(0, 0, width, height);
            gl.clear_color(
                BACKGROUND_COLOR[0],
                BACKGROUND_COLOR[1],
                BACKGROUND_COLOR[2],
                BACKGROUND_COLOR[3],
            );
            gl.clear(glow::COLOR_BUFFER_BIT);

            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));

            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
        }

        // Draw grid lines.
        self.draw_grid(gl);

        // Draw filled area under the spectrum curve (when enabled).
        if fill_enabled {
            self.draw_fill(gl, display_data, db_range, min_db);
        }

        // Draw the spectrum line trace on top.
        self.draw_trace(gl, display_data, db_range, min_db);

        // Return the downsample buffer to self for reuse next frame.
        let _ = display_data;
        self.downsample_buf = ds_buf;

        unsafe {
            gl.disable(glow::BLEND);
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.use_program(None);
        }
    }

    /// Draw horizontal dB grid lines and vertical frequency grid lines.
    #[allow(
        unsafe_code,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    fn draw_grid(&mut self, gl: &glow::Context) {
        self.grid_vertices.clear();

        for i in 0..=DB_GRID_LINE_COUNT {
            let frac = i as f32 / DB_GRID_LINE_COUNT as f32;
            let y = -1.0 + 2.0 * frac;
            self.grid_vertices.extend_from_slice(&[-1.0, y, 1.0, y]);
        }

        for i in 0..=FREQ_GRID_LINE_COUNT {
            let frac = i as f32 / FREQ_GRID_LINE_COUNT as f32;
            let x = -1.0 + 2.0 * frac;
            self.grid_vertices.extend_from_slice(&[x, -1.0, x, 1.0]);
        }

        let bytes = f32_slice_as_bytes(&self.grid_vertices);
        let vertex_count = self.grid_vertices.len() / 2;

        unsafe {
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytes);

            gl.uniform_4_f32(
                Some(&self.color_location),
                GRID_COLOR[0],
                GRID_COLOR[1],
                GRID_COLOR[2],
                GRID_COLOR[3],
            );

            gl.draw_arrays(glow::LINES, 0, vertex_count as i32);
        }
    }

    /// Draw the filled area under the spectrum curve as a triangle strip.
    #[allow(
        unsafe_code,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    fn draw_fill(&mut self, gl: &glow::Context, fft_data: &[f32], db_range: f32, min_db: f32) {
        let bin_count = fft_data.len();
        self.fill_vertices.clear();

        for (i, &db) in fft_data.iter().take(bin_count).enumerate() {
            let x = -1.0 + 2.0 * (i as f32 / (bin_count - 1).max(1) as f32);
            let y = -1.0 + 2.0 * ((db - min_db) / db_range).clamp(0.0, 1.0);
            self.fill_vertices.extend_from_slice(&[x, -1.0, x, y]);
        }

        let bytes = f32_slice_as_bytes(&self.fill_vertices);
        let vertex_count = self.fill_vertices.len() / 2;

        unsafe {
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytes);

            gl.uniform_4_f32(
                Some(&self.color_location),
                FILL_COLOR[0],
                FILL_COLOR[1],
                FILL_COLOR[2],
                FILL_COLOR[3],
            );

            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, vertex_count as i32);
        }
    }

    /// Draw the spectrum line trace.
    #[allow(
        unsafe_code,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    fn draw_trace(&mut self, gl: &glow::Context, fft_data: &[f32], db_range: f32, min_db: f32) {
        let bin_count = fft_data.len();
        self.trace_vertices.clear();

        for (i, &db) in fft_data.iter().take(bin_count).enumerate() {
            let x = -1.0 + 2.0 * (i as f32 / (bin_count - 1).max(1) as f32);
            let y = -1.0 + 2.0 * ((db - min_db) / db_range).clamp(0.0, 1.0);
            self.trace_vertices.extend_from_slice(&[x, y]);
        }

        let bytes = f32_slice_as_bytes(&self.trace_vertices);
        let vertex_count = self.trace_vertices.len() / 2;

        unsafe {
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytes);

            gl.uniform_4_f32(
                Some(&self.color_location),
                TRACE_COLOR[0],
                TRACE_COLOR[1],
                TRACE_COLOR[2],
                TRACE_COLOR[3],
            );

            gl.draw_arrays(glow::LINE_STRIP, 0, vertex_count as i32);
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
