//! Waterfall display renderer using OpenGL via glow.
//!
//! Renders a scrolling spectrogram: each FFT frame becomes one horizontal line
//! in a ring-buffer texture, mapped through a colormap for visualization.

use glow::HasContext;

use super::colormap;
use super::gl_renderer::{self, GlError, f32_slice_as_bytes};

/// Number of history lines stored in the ring-buffer texture.
const HISTORY_LINES: usize = 1024;

/// Maximum waterfall texture width. FFT data wider than this is downsampled.
pub const MAX_TEXTURE_WIDTH: usize = 4096;

/// Default minimum display level in dB.
const DEFAULT_MIN_DB: f32 = -70.0;
/// Default maximum display level in dB.
const DEFAULT_MAX_DB: f32 = 0.0;

/// Background clear color — near-black, matching FFT plot.
const BACKGROUND_COLOR: [f32; 4] = [0.08, 0.08, 0.10, 1.0];

/// Vertex shader for the waterfall full-screen quad.
/// Passes texture coordinates to the fragment shader.
const VERT_SHADER: &str = r"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
out vec2 v_uv;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
    v_uv = a_uv;
}
";

/// Fragment shader for the waterfall display.
/// Samples the data texture (R channel = normalized dB),
/// applies the scroll offset, and maps through the colormap.
const FRAG_SHADER: &str = r"#version 300 es
precision highp float;
in vec2 v_uv;
uniform sampler2D u_data_tex;
uniform sampler2D u_colormap_tex;
uniform float u_scroll_offset;
uniform float u_history_lines;
out vec4 frag_color;
void main() {
    // Apply ring-buffer scroll: shift V coordinate by the write position.
    float v = v_uv.y + u_scroll_offset / u_history_lines;
    v = fract(v); // wrap around
    vec2 uv = vec2(v_uv.x, v);

    // Sample data texture: R channel holds normalized power (0..1).
    float power = texture(u_data_tex, uv).r;

    // Map through colormap (1D lookup along U axis).
    vec4 color = texture(u_colormap_tex, vec2(power, 0.5));
    frag_color = color;
}
";

/// Full-screen quad vertices: position (x, y) + texcoord (u, v).
/// Two triangles covering the entire NDC [-1, 1] range.
#[allow(clippy::excessive_precision)]
const QUAD_VERTICES: [f32; 24] = [
    // Triangle 1
    -1.0, -1.0, 0.0, 1.0, // bottom-left  (v=1 = oldest)
    1.0, -1.0, 1.0, 1.0, // bottom-right
    -1.0, 1.0, 0.0, 0.0, // top-left     (v=0 = newest)
    // Triangle 2
    1.0, -1.0, 1.0, 1.0, // bottom-right
    1.0, 1.0, 1.0, 0.0, // top-right
    -1.0, 1.0, 0.0, 0.0, // top-left
];

/// Downsample FFT data by max-pooling bins to a target width.
///
/// Groups of input bins are reduced to one output bin by taking the maximum
/// dB value in each group, preserving signal peaks for display.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn downsample_to(data: &[f32], buf: &mut Vec<f32>, target_width: usize) {
    buf.resize(target_width, f32::NEG_INFINITY);
    let ratio = data.len() as f32 / target_width as f32;
    for (i, out) in buf.iter_mut().enumerate().take(target_width) {
        let start = (i as f32 * ratio) as usize;
        let end = (((i + 1) as f32) * ratio).ceil() as usize;
        let end = end.min(data.len());
        let mut max_val = f32::NEG_INFINITY;
        for &v in &data[start..end] {
            if v > max_val {
                max_val = v;
            }
        }
        *out = max_val;
    }
}

/// OpenGL renderer for the scrolling waterfall spectrogram.
pub struct WaterfallRenderer {
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    data_texture: glow::Texture,
    colormap_texture: glow::Texture,
    /// Pre-allocated buffer for uploading one row of normalized pixel data.
    row_buffer: Vec<u8>,
    /// Current write position in the ring buffer (`0..HISTORY_LINES`).
    write_row: usize,
    /// Width of the data texture in texels (= number of FFT bins).
    texture_width: usize,
    /// Uniform locations.
    scroll_offset_loc: glow::UniformLocation,
    history_lines_loc: glow::UniformLocation,
    data_tex_loc: glow::UniformLocation,
    colormap_tex_loc: glow::UniformLocation,
    /// Pre-allocated buffer for downsampling large FFT data.
    downsample_buf: Vec<f32>,
    /// Display range in dB.
    min_db: f32,
    max_db: f32,
}

impl WaterfallRenderer {
    /// Create a new waterfall renderer.
    ///
    /// # Arguments
    ///
    /// * `gl` — The glow GL context.
    /// * `width` — Number of FFT bins (data texture width).
    ///
    /// # Errors
    ///
    /// Returns `GlError` if shader compilation, linking, or resource creation fails.
    #[allow(
        unsafe_code,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    pub fn new(gl: &glow::Context, requested_width: usize) -> Result<Self, GlError> {
        let width = requested_width.min(MAX_TEXTURE_WIDTH);
        let vert = gl_renderer::compile_shader(gl, glow::VERTEX_SHADER, VERT_SHADER)?;
        let frag = gl_renderer::compile_shader(gl, glow::FRAGMENT_SHADER, FRAG_SHADER)?;
        let program = gl_renderer::link_program(gl, vert, frag)?;

        // Look up uniforms.
        let scroll_offset_loc = unsafe {
            gl.get_uniform_location(program, "u_scroll_offset")
                .ok_or_else(|| {
                    GlError::ResourceCreation("u_scroll_offset uniform not found".into())
                })?
        };
        let history_lines_loc = unsafe {
            gl.get_uniform_location(program, "u_history_lines")
                .ok_or_else(|| {
                    GlError::ResourceCreation("u_history_lines uniform not found".into())
                })?
        };
        let data_tex_loc = unsafe {
            gl.get_uniform_location(program, "u_data_tex")
                .ok_or_else(|| GlError::ResourceCreation("u_data_tex uniform not found".into()))?
        };
        let colormap_tex_loc = unsafe {
            gl.get_uniform_location(program, "u_colormap_tex")
                .ok_or_else(|| {
                    GlError::ResourceCreation("u_colormap_tex uniform not found".into())
                })?
        };

        // Create and upload quad VBO/VAO.
        let (vao, vbo) = unsafe {
            let vao = gl
                .create_vertex_array()
                .map_err(|e| GlError::ResourceCreation(e.clone()))?;
            let vbo = gl
                .create_buffer()
                .map_err(|e| GlError::ResourceCreation(e.clone()))?;

            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));

            let bytes = f32_slice_as_bytes(&QUAD_VERTICES);
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);

            let stride = (4 * std::mem::size_of::<f32>()) as i32;

            // Attribute 0: vec2 position.
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);

            // Attribute 1: vec2 texcoord.
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(
                1,
                2,
                glow::FLOAT,
                false,
                stride,
                (2 * std::mem::size_of::<f32>()) as i32,
            );

            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);

            (vao, vbo)
        };

        // Create the data texture (R8, width x HISTORY_LINES).
        let data_texture = create_data_texture(gl, width)?;

        // Create the colormap texture (RGBA, 256 x 1).
        let colormap_texture = create_colormap_texture(gl)?;

        Ok(Self {
            program,
            vao,
            vbo,
            data_texture,
            colormap_texture,
            row_buffer: vec![0u8; width],
            write_row: 0,
            texture_width: width,
            scroll_offset_loc,
            history_lines_loc,
            data_tex_loc,
            colormap_tex_loc,
            downsample_buf: Vec::with_capacity(MAX_TEXTURE_WIDTH),
            min_db: DEFAULT_MIN_DB,
            max_db: DEFAULT_MAX_DB,
        })
    }

    /// Push one FFT frame as a new row in the ring-buffer texture.
    ///
    /// The dB values are normalized to 0..255 using the current display range
    /// and written to the current row via `texSubImage2D`.
    #[allow(
        unsafe_code,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    pub fn push_line(&mut self, gl: &glow::Context, fft_data: &[f32]) {
        let db_range = self.max_db - self.min_db;
        if !db_range.is_finite() || db_range <= 0.0 {
            return;
        }

        // Downsample if FFT bins exceed texture width.
        let display_data = if fft_data.len() > self.texture_width {
            downsample_to(fft_data, &mut self.downsample_buf, self.texture_width);
            &self.downsample_buf
        } else {
            fft_data
        };

        let bin_count = display_data.len().min(self.texture_width);

        // Normalize dB values to 0..255 using pre-allocated row buffer.
        self.row_buffer.fill(0);
        for (i, &db) in display_data.iter().take(bin_count).enumerate() {
            let normalized = ((db - self.min_db) / db_range).clamp(0.0, 1.0);
            self.row_buffer[i] = (normalized * 255.0).round() as u8;
        }

        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.data_texture));
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                self.write_row as i32,
                self.texture_width as i32,
                1,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&self.row_buffer)),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
        }

        self.write_row = (self.write_row + 1) % HISTORY_LINES;
    }

    /// Render the full waterfall display.
    #[allow(unsafe_code, clippy::cast_precision_loss)]
    pub fn render(&self, gl: &glow::Context, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
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

            // Bind data texture to unit 0.
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.data_texture));
            gl.uniform_1_i32(Some(&self.data_tex_loc), 0);

            // Bind colormap texture to unit 1.
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.colormap_texture));
            gl.uniform_1_i32(Some(&self.colormap_tex_loc), 1);

            // Set scroll offset to the newest written row (not the next free slot).
            let newest_row = (self.write_row + HISTORY_LINES - 1) % HISTORY_LINES;
            gl.uniform_1_f32(Some(&self.scroll_offset_loc), newest_row as f32);
            gl.uniform_1_f32(Some(&self.history_lines_loc), HISTORY_LINES as f32);

            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 6);

            gl.bind_vertex_array(None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
        }
    }

    /// Update the colormap texture with a new style.
    #[allow(
        unsafe_code,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    pub fn set_colormap(&self, gl: &glow::Context, style: colormap::ColormapStyle) {
        let map = colormap::generate_colormap(style);
        let flat: Vec<u8> = map.iter().flat_map(|c| c.iter().copied()).collect();
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.colormap_texture));
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                colormap::COLORMAP_SIZE as i32,
                1,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&flat)),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    pub fn set_db_range(&mut self, min_db: f32, max_db: f32) {
        if min_db.is_finite() && max_db.is_finite() && max_db > min_db {
            self.min_db = min_db;
            self.max_db = max_db;
        }
    }

    /// Resize the waterfall texture for a new FFT size.
    ///
    /// Deletes the old data texture and creates a new one at the given width
    /// (capped at `MAX_TEXTURE_WIDTH`). Resets the ring buffer write position.
    #[allow(
        unsafe_code,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    pub fn resize(&mut self, gl: &glow::Context, new_width: usize) {
        let capped_width = new_width.min(MAX_TEXTURE_WIDTH);
        // Always reset history (even at same width) to clear mixed-resolution data.
        // Create replacement texture BEFORE deleting the old one so a failed
        // allocation doesn't leave a dangling handle.
        match create_data_texture(gl, capped_width) {
            Ok(tex) => {
                let old_tex = std::mem::replace(&mut self.data_texture, tex);
                unsafe {
                    gl.delete_texture(old_tex);
                }
                if capped_width == self.texture_width {
                    self.row_buffer.fill(0);
                } else {
                    self.texture_width = capped_width;
                    self.row_buffer = vec![0u8; capped_width];
                }
                self.write_row = 0;
                tracing::debug!(width = capped_width, "waterfall texture reset");
            }
            Err(e) => {
                tracing::warn!("failed to resize waterfall texture: {e}");
            }
        }
    }

    /// Release GL resources.
    #[allow(unsafe_code)]
    pub fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_texture(self.data_texture);
            gl.delete_texture(self.colormap_texture);
            gl.delete_buffer(self.vbo);
            gl.delete_vertex_array(self.vao);
            gl.delete_program(self.program);
        }
    }
}

/// Create the ring-buffer data texture (R8, width x `HISTORY_LINES`).
#[allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
fn create_data_texture(gl: &glow::Context, width: usize) -> Result<glow::Texture, GlError> {
    unsafe {
        let texture = gl
            .create_texture()
            .map_err(|e| GlError::ResourceCreation(e.clone()))?;

        gl.bind_texture(glow::TEXTURE_2D, Some(texture));

        // R8 is 1 byte/pixel — set alignment to 1 so non-power-of-4 widths
        // don't cause row misalignment from the default GL_UNPACK_ALIGNMENT of 4.
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);

        // Initialize with zeros.
        let data = vec![0u8; width * HISTORY_LINES];
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::R8 as i32,
            width as i32,
            HISTORY_LINES as i32,
            0,
            glow::RED,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&data)),
        );

        // Nearest filtering — we want crisp bin boundaries.
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::REPEAT as i32);

        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(texture)
    }
}

/// Create the colormap lookup texture (RGBA, 256 x 1).
#[allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
fn create_colormap_texture(gl: &glow::Context) -> Result<glow::Texture, GlError> {
    unsafe {
        let texture = gl
            .create_texture()
            .map_err(|e| GlError::ResourceCreation(e.clone()))?;

        gl.bind_texture(glow::TEXTURE_2D, Some(texture));

        let colormap = colormap::generate_colormap(colormap::ColormapStyle::Turbo);
        let flat: Vec<u8> = colormap.iter().flat_map(|c| c.iter().copied()).collect();

        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            colormap::COLORMAP_SIZE as i32,
            1,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&flat)),
        );

        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );

        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(texture)
    }
}
