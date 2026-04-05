//! Shared OpenGL utility functions for shader compilation and program linking.
//!
//! Used by both the FFT plot renderer and the waterfall renderer.

use glow::HasContext;

/// Error type for OpenGL operations in the spectrum display.
#[derive(Debug, thiserror::Error)]
pub enum GlError {
    /// Shader compilation failed.
    #[error("shader compilation failed: {0}")]
    ShaderCompile(String),
    /// Program linking failed.
    #[error("program link failed: {0}")]
    ProgramLink(String),
    /// A GL resource could not be created.
    #[error("GL resource creation failed: {0}")]
    ResourceCreation(String),
}

/// Compile a single shader of the given type from GLSL source.
///
/// # Errors
///
/// Returns `GlError::ShaderCompile` if compilation fails, or
/// `GlError::ResourceCreation` if the shader object cannot be allocated.
#[allow(unsafe_code)]
pub fn compile_shader(
    gl: &glow::Context,
    shader_type: u32,
    source: &str,
) -> Result<glow::Shader, GlError> {
    unsafe {
        let shader = gl
            .create_shader(shader_type)
            .map_err(|e| GlError::ResourceCreation(e.clone()))?;

        gl.shader_source(shader, source);
        gl.compile_shader(shader);

        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            return Err(GlError::ShaderCompile(log));
        }

        Ok(shader)
    }
}

/// Link a vertex and fragment shader into a program.
///
/// Both shaders are detached and deleted after successful linking.
///
/// # Errors
///
/// Returns `GlError::ProgramLink` if linking fails, or
/// `GlError::ResourceCreation` if the program object cannot be allocated.
#[allow(unsafe_code)]
pub fn link_program(
    gl: &glow::Context,
    vert: glow::Shader,
    frag: glow::Shader,
) -> Result<glow::Program, GlError> {
    unsafe {
        let program = gl
            .create_program()
            .map_err(|e| GlError::ResourceCreation(e.clone()))?;

        gl.attach_shader(program, vert);
        gl.attach_shader(program, frag);
        gl.link_program(program);

        // Shaders can be detached + deleted after linking regardless of outcome.
        gl.detach_shader(program, vert);
        gl.detach_shader(program, frag);
        gl.delete_shader(vert);
        gl.delete_shader(frag);

        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(GlError::ProgramLink(log));
        }

        Ok(program)
    }
}

/// Log any pending OpenGL errors via `tracing::warn!`.
///
/// Call after GL operations during debugging to catch driver issues.
#[allow(unsafe_code)]
pub fn check_gl_errors(gl: &glow::Context, label: &str) {
    unsafe {
        let err = gl.get_error();
        if err != glow::NO_ERROR {
            tracing::warn!("GL error at {label}: 0x{err:04X}");
        }
    }
}
