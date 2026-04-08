//! Signal strength history graph — a time-series line plot showing signal
//! level (dB) over the last ~60 seconds, rendered via OpenGL.

use glow::HasContext;

use super::gl_renderer::{self, GlError, f32_slice_as_bytes};

/// Number of samples in the history buffer (~60 seconds at ~10 updates/sec).
const HISTORY_SIZE: usize = 600;

/// Number of horizontal dB grid lines in the history plot.
const DB_GRID_LINE_COUNT: usize = 4;

// Colors (RGBA, 0.0..1.0)
/// Trace line color — green.
const TRACE_COLOR: [f32; 4] = [0.3, 0.85, 0.4, 1.0];
/// Grid line color — dim gray.
const GRID_COLOR: [f32; 4] = [0.4, 0.4, 0.4, 0.3];
/// Background clear color — near-black.
const BACKGROUND_COLOR: [f32; 4] = [0.08, 0.08, 0.10, 1.0];

/// Maximum vertices needed: history samples for the trace, plus grid lines.
const MAX_VERTICES: usize = HISTORY_SIZE + (DB_GRID_LINE_COUNT + 1) * 2;

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

/// OpenGL renderer for the signal strength history plot.
///
/// Maintains a circular buffer of dB samples and draws them as a line strip
/// with horizontal grid lines for dB reference.
pub struct SignalHistoryRenderer {
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    color_location: glow::UniformLocation,
    samples: Vec<f32>,
    write_pos: usize,
    count: usize,
    // Reusable staging buffers to avoid per-frame allocations.
    grid_vertices: Vec<f32>,
    trace_vertices: Vec<f32>,
}

impl SignalHistoryRenderer {
    /// Create a new signal history renderer, compiling shaders and allocating GL buffers.
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

            // Pre-allocate buffer for the largest usage (2 floats per vertex).
            let max_bytes = MAX_VERTICES * 2 * std::mem::size_of::<f32>();
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
            samples: vec![f32::NEG_INFINITY; HISTORY_SIZE],
            write_pos: 0,
            count: 0,
            grid_vertices: Vec::with_capacity((DB_GRID_LINE_COUNT + 1) * 4),
            trace_vertices: Vec::with_capacity(HISTORY_SIZE * 2),
        })
    }

    /// Add a signal level sample (in dB) to the circular buffer.
    pub fn push(&mut self, db: f32) {
        self.samples[self.write_pos] = db;
        self.write_pos = (self.write_pos + 1) % HISTORY_SIZE;
        if self.count < HISTORY_SIZE {
            self.count += 1;
        }
    }

    /// Render the signal history line plot.
    ///
    /// # Arguments
    ///
    /// * `gl` — The glow GL context.
    /// * `width` — Viewport width in pixels.
    /// * `height` — Viewport height in pixels.
    /// * `min_db` — Bottom of the display range in dB.
    /// * `max_db` — Top of the display range in dB.
    #[allow(
        unsafe_code,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    pub fn render(
        &mut self,
        gl: &glow::Context,
        width: i32,
        height: i32,
        min_db: f32,
        max_db: f32,
    ) {
        if width <= 0 || height <= 0 {
            return;
        }

        let db_range = max_db - min_db;
        if db_range <= 0.0 {
            return;
        }

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

        // Draw horizontal dB grid lines.
        self.draw_grid(gl, db_range, min_db);

        // Draw the signal trace as a line strip.
        self.draw_trace(gl, db_range, min_db);

        unsafe {
            gl.disable(glow::BLEND);
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.use_program(None);
        }
    }

    /// Draw horizontal dB grid lines.
    #[allow(
        unsafe_code,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    fn draw_grid(&mut self, gl: &glow::Context, db_range: f32, min_db: f32) {
        // Draw grid lines at round 20 dB intervals aligned to actual dB values.
        const DB_STEP: f32 = 20.0;
        self.grid_vertices.clear();

        if db_range > 0.0 {
            let first = (min_db / DB_STEP).ceil() * DB_STEP;
            let mut db = first;
            while db < min_db + db_range {
                let y = -1.0 + 2.0 * ((db - min_db) / db_range);
                self.grid_vertices.extend_from_slice(&[-1.0, y, 1.0, y]);
                db += DB_STEP;
            }
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

    /// Draw the signal level trace as a line strip.
    ///
    /// Reads from the circular buffer starting at `write_pos` (oldest sample)
    /// and wrapping around to `write_pos - 1` (newest sample).
    #[allow(
        unsafe_code,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    fn draw_trace(&mut self, gl: &glow::Context, db_range: f32, min_db: f32) {
        if self.count == 0 {
            return;
        }

        let n = self.count;
        self.trace_vertices.clear();

        // Start from the oldest sample in the circular buffer.
        let start = if self.count < HISTORY_SIZE {
            0
        } else {
            self.write_pos
        };

        for i in 0..n {
            let idx = (start + i) % HISTORY_SIZE;
            let db = self.samples[idx];

            // X axis: time, oldest on left, newest on right.
            let x = -1.0 + 2.0 * (i as f32 / (n - 1).max(1) as f32);
            // Y axis: dB mapped to [-1, 1] clip space.
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

#[cfg(test)]
mod tests {
    /// Compile-time validation that signal history constants are consistent.
    const _: () = {
        assert!(super::HISTORY_SIZE > 0);
        assert!(super::DB_GRID_LINE_COUNT > 0);
        assert!(super::MAX_VERTICES >= super::HISTORY_SIZE + (super::DB_GRID_LINE_COUNT + 1) * 2);
    };
}
