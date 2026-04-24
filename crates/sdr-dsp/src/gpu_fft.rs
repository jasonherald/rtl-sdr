//! wgpu-backed FFT engine — Stockham autosort radix-2 forward DFT,
//! one pass per compute dispatch (epic #452 phase 2 / #179).
//!
//! Lives behind the same [`FftEngine`] trait as [`RustFftEngine`]
//! so callers can hot-swap CPU↔GPU without touching the signal
//! path. The host-side scaffolding here owns the device / queue /
//! pipeline / ping-pong buffers for the engine's lifetime; the
//! hot path (`forward`) only issues `log2(N)` compute dispatches
//! plus one readback copy, zero allocations.
//!
//! # Algorithm
//!
//! Stockham out-of-place radix-2 FFT. Pass `s` (0 ≤ s < log2(N))
//! combines two size-`2^s` subFFTs into one size-`2^(s+1)` subFFT,
//! reading from one ping-pong buffer and writing to the other.
//! No bit-reversal permutation pass — Stockham's "contiguous
//! output" layout already produces the final natural order after
//! the last pass. See `gpu_fft.wgsl` for the per-butterfly
//! arithmetic.
//!
//! # Pre-allocation
//!
//! Everything is created once in [`GpuFftEngine::with_options`]:
//!
//! - `wgpu::Instance`, `wgpu::Adapter`, `wgpu::Device`, `wgpu::Queue`
//! - Compute pipeline + bind group layout
//! - Two ping-pong storage buffers (sized `N * 8 bytes`)
//! - One uniform buffer holding all `log2(N)` `Params` entries at
//!   `min_uniform_buffer_offset_alignment` stride; written once,
//!   bound with dynamic offsets per pass
//! - Two bind groups (ping and pong) with the uniform in both
//! - One staging download buffer (`MAP_READ | COPY_DST`)
//!
//! GPU pipeline state caching is paramount — wgpu device+pipeline
//! construction is the expensive part (~ms), dispatches are cheap
//! (~µs). Anything that skips this cache defeats the point of GPU
//! compute for this workload, which is why the trait is
//! `&mut self` and every per-FFT allocation is denied.
//!
//! # Sync surface over async wgpu
//!
//! wgpu 29's `request_adapter` / `request_device` / `map_async`
//! are all futures. The [`FftEngine`] trait is sync, so we block
//! via [`pollster`] on the `new`/`with_options` path and poll the
//! device on the `forward` readback path. The forward path does
//! *not* spawn an async runtime — `device.poll(PollType::Wait)`
//! blocks on the calling thread inside wgpu-core's own event loop.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use sdr_types::{Complex, DspError};
use tracing::{debug, info};

use crate::fft::FftEngine;

/// WGSL source for the Stockham FFT compute kernel. Compiled into
/// a single pipeline reused across all passes.
const SHADER_SRC: &str = include_str!("gpu_fft.wgsl");

/// Workgroup size declared in the shader. Must match
/// `@workgroup_size(N)` in `gpu_fft.wgsl` — the CPU dispatch math
/// divides butterfly count by this.
const SHADER_WORKGROUP_SIZE: u32 = 64;

/// `Params` struct size in WGSL is two `u32`s = 8 bytes, but we
/// stride entries by the device's
/// `min_uniform_buffer_offset_alignment` so each dispatch can use
/// a dynamic offset. 256 covers every GPU we care about (NVIDIA /
/// AMD discrete and integrated); we query the actual limit at
/// construction time rather than hard-coding it.
const PARAMS_SIZE_BYTES: u64 = std::mem::size_of::<ShaderParams>() as u64;

/// Adapter-selection knobs for [`GpuFftEngine::with_options`].
///
/// Default = discrete high-performance adapter via the default
/// backend set (Vulkan on Linux, Metal on macOS, DX12 on Windows).
/// The bench harness in `benches/fft_gpu.rs` overrides these to
/// sweep the 4080 Super / 3090 / AMD iGPU matrix.
#[derive(Debug, Clone)]
pub struct GpuFftOptions {
    /// Power-preference hint. `HighPerformance` picks the discrete
    /// GPU on laptops and multi-GPU desktops; `LowPower` picks
    /// the iGPU.
    pub power_preference: wgpu::PowerPreference,
    /// Which GPU backend(s) the instance is allowed to see. Defaults
    /// to `Backends::all()` so wgpu picks per-platform defaults;
    /// narrow to `VULKAN` / `METAL` / `DX12` to test a specific
    /// driver stack.
    pub backends: wgpu::Backends,
    /// If true, force a software ("CPU fallback") adapter. Useful
    /// as a sanity check — the shader should produce identical
    /// output on any compliant backend.
    pub force_fallback_adapter: bool,
}

impl Default for GpuFftOptions {
    fn default() -> Self {
        Self {
            power_preference: wgpu::PowerPreference::HighPerformance,
            backends: wgpu::Backends::all(),
            force_fallback_adapter: false,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable)]
struct ShaderParams {
    pass_s: u32,
    size: u32,
}

/// Adapter / backend summary for a constructed GPU engine, exposed
/// so the bench harness can print which GPU a given run measured
/// (the dev machine in epic #452 has 3 GPUs and the "default
/// high-performance" pick can depend on display vs. render
/// attachments and driver priority).
#[derive(Debug, Clone)]
pub struct AdapterSummary {
    pub name: String,
    pub backend: wgpu::Backend,
    pub device_type: wgpu::DeviceType,
    pub driver: String,
}

/// wgpu-backed FFT engine. Implements [`FftEngine`] with identical
/// semantics to [`RustFftEngine`]: in-place forward complex DFT.
///
/// Not `Clone` — each instance owns its own wgpu device and a
/// sized set of buffers. Sharing across threads is possible via
/// `Arc<Mutex<GpuFftEngine>>` but each instance is single-threaded
/// internally.
#[derive(Debug)]
pub struct GpuFftEngine {
    adapter_summary: AdapterSummary,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    pipeline: wgpu::ComputePipeline,

    // Ping-pong storage buffers. Both sized `size * 8 bytes` (size
    // complex × 2 × f32). `buf_a` also carries `COPY_DST` so we
    // can upload the input via `queue.write_buffer`.
    buf_a: wgpu::Buffer,
    buf_b: wgpu::Buffer,

    // Uniform buffer holding `log2(size)` `ShaderParams` entries at
    // `params_stride` bytes each. Pre-filled once in `new()` and
    // addressed via dynamic offsets at dispatch time — so the host
    // never writes to it on the forward path.
    params_bg_a: wgpu::BindGroup,
    params_bg_b: wgpu::BindGroup,
    params_stride: u32,

    // MAP_READ | COPY_DST staging buffer for device→host readback.
    staging: wgpu::Buffer,

    size: usize,
    log2_size: u32,
    workgroup_count: u32,

    // After `log2_size` ping-pong passes, the final result lives in
    // `buf_a` iff `log2_size` is even (last pass reads from the
    // buffer flipped-to-pong-side, writes back to A).
    output_in_buf_a: bool,
}

impl GpuFftEngine {
    /// Create a new GPU FFT engine for the given size with default
    /// adapter options (discrete high-performance adapter).
    ///
    /// # Errors
    ///
    /// - [`DspError::InvalidParameter`] if `size` is not a power
    ///   of two ≥ 2.
    /// - [`DspError::GpuUnavailable`] if no compatible adapter /
    ///   device can be acquired. Callers that want the CPU
    ///   engine as a fallback should catch this and construct
    ///   [`RustFftEngine`] instead.
    pub fn new(size: usize) -> Result<Self, DspError> {
        Self::with_options(size, &GpuFftOptions::default())
    }

    /// Create a new GPU FFT engine with explicit adapter options.
    /// Used by the bench harness to pin a specific adapter when
    /// sweeping a multi-GPU matrix.
    ///
    /// # Errors
    ///
    /// Same as [`GpuFftEngine::new`].
    pub fn with_options(size: usize, opts: &GpuFftOptions) -> Result<Self, DspError> {
        if size < 2 || !size.is_power_of_two() {
            return Err(DspError::InvalidParameter(format!(
                "GPU FFT size must be a power of two ≥ 2, got {size}"
            )));
        }
        pollster::block_on(Self::build_async(size, opts))
    }

    /// Adapter / backend / driver string for the underlying wgpu
    /// adapter. Captured once at engine construction so downstream
    /// (benches, telemetry) never has to go re-query wgpu itself.
    #[must_use]
    pub fn adapter_summary(&self) -> &AdapterSummary {
        &self.adapter_summary
    }

    /// Create the wgpu instance, pick an adapter per `opts`, and
    /// request a device + queue. Split out of `build_async` to
    /// keep that function under the `too_many_lines` clippy bar.
    async fn acquire_device(
        opts: &GpuFftOptions,
    ) -> Result<(Arc<wgpu::Device>, Arc<wgpu::Queue>, AdapterSummary), DspError> {
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = opts.backends;
        let instance = wgpu::Instance::new(instance_desc);

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: opts.power_preference,
                force_fallback_adapter: opts.force_fallback_adapter,
                compatible_surface: None,
            })
            .await
            .map_err(|e| DspError::GpuUnavailable(format!("request_adapter failed: {e}")))?;

        let info = adapter.get_info();
        let summary = AdapterSummary {
            name: info.name.clone(),
            backend: info.backend,
            device_type: info.device_type,
            driver: info.driver.clone(),
        };
        info!(
            target: "sdr_dsp::gpu_fft",
            backend = ?summary.backend,
            device = %summary.name,
            device_type = ?summary.device_type,
            driver = %summary.driver,
            "selected GPU FFT adapter"
        );

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("sdr-dsp GPU FFT device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::Performance,
                ..Default::default()
            })
            .await
            .map_err(|e| DspError::GpuUnavailable(format!("request_device failed: {e}")))?;
        Ok((Arc::new(device), Arc::new(queue), summary))
    }

    /// Pre-fill the per-pass uniforms. Stride is the device's
    /// reported `min_uniform_buffer_offset_alignment`, which is
    /// 256 on almost every GPU. Each entry is only 8 bytes so we
    /// zero-pad the remainder of each stride.
    ///
    /// The `try_from` cascades can't actually fail for any shape
    /// the caller could construct — FFT sizes are validated at
    /// power-of-two above and `min_uniform_buffer_offset_alignment`
    /// is always a small u32 — but going through fallible casts
    /// keeps clippy's pedantic cast lint happy and documents the
    /// narrowing explicitly.
    fn build_params_buffer(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        size: usize,
        log2_size: u32,
    ) -> Result<(wgpu::Buffer, u32), DspError> {
        let params_size_u32 = u32::try_from(PARAMS_SIZE_BYTES)
            .map_err(|_| DspError::InvalidParameter("ShaderParams size exceeds u32".into()))?;
        let params_stride = device
            .limits()
            .min_uniform_buffer_offset_alignment
            .max(params_size_u32);
        let size_u32 = u32::try_from(size)
            .map_err(|_| DspError::InvalidParameter(format!("FFT size {size} exceeds u32")))?;
        let params_buf_bytes = u64::from(params_stride) * u64::from(log2_size);
        let params_buf_bytes_usize = usize::try_from(params_buf_bytes).map_err(|_| {
            DspError::InvalidParameter(format!(
                "uniform buffer size {params_buf_bytes} exceeds usize"
            ))
        })?;

        let mut params_bytes = vec![0_u8; params_buf_bytes_usize];
        for s in 0..log2_size {
            let params = ShaderParams {
                pass_s: s,
                size: size_u32,
            };
            let start = (s * params_stride) as usize;
            let end = start + std::mem::size_of::<ShaderParams>();
            params_bytes[start..end].copy_from_slice(bytemuck::bytes_of(&params));
        }
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sdr-dsp GPU FFT params"),
            size: params_buf_bytes,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&params_buf, 0, &params_bytes);

        Ok((params_buf, params_stride))
    }

    /// Build the single compute pipeline + its bind group layout.
    /// The pipeline is reused across every pass.
    fn build_pipeline(device: &wgpu::Device) -> (wgpu::ComputePipeline, wgpu::BindGroupLayout) {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sdr-dsp GPU FFT bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(PARAMS_SIZE_BYTES),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sdr-dsp GPU FFT shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sdr-dsp GPU FFT pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("sdr-dsp GPU FFT pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

        (pipeline, bind_group_layout)
    }

    async fn build_async(size: usize, opts: &GpuFftOptions) -> Result<Self, DspError> {
        let (device, queue, adapter_summary) = Self::acquire_device(opts).await?;

        // Buffer sizing. Each complex point is two f32s = 8 bytes.
        let buffer_bytes = (size as u64)
            .checked_mul(std::mem::size_of::<Complex>() as u64)
            .ok_or_else(|| {
                DspError::InvalidParameter(format!("FFT size {size} overflows buffer bytes"))
            })?;

        let buf_a = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sdr-dsp GPU FFT buf_a"),
            size: buffer_bytes,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let buf_b = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sdr-dsp GPU FFT buf_b"),
            size: buffer_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sdr-dsp GPU FFT staging"),
            size: buffer_bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let log2_size = size.trailing_zeros();
        let (params_buf, params_stride) =
            Self::build_params_buffer(&device, &queue, size, log2_size)?;
        let (pipeline, bind_group_layout) = Self::build_pipeline(&device);

        // Two bind groups — alternating which ping-pong buffer is
        // read-only (src) vs read-write (dst). Both share the same
        // dynamic-offset uniform binding.
        let make_bg = |label: &str, src: &wgpu::Buffer, dst: &wgpu::Buffer| -> wgpu::BindGroup {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &params_buf,
                            offset: 0,
                            size: wgpu::BufferSize::new(PARAMS_SIZE_BYTES),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: src.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: dst.as_entire_binding(),
                    },
                ],
            })
        };
        let params_bg_a = make_bg("sdr-dsp GPU FFT bg (read A, write B)", &buf_a, &buf_b);
        let params_bg_b = make_bg("sdr-dsp GPU FFT bg (read B, write A)", &buf_b, &buf_a);

        let butterflies_per_pass = size / 2;
        let workgroup_count = u32::try_from(
            butterflies_per_pass.div_ceil(SHADER_WORKGROUP_SIZE as usize),
        )
        .map_err(|_| {
            DspError::InvalidParameter(format!(
                "FFT size {size} produces too many workgroups for u32 dispatch"
            ))
        })?;

        // Last pass is `log2_size - 1` (0-indexed). After an even
        // number of total passes, result is back in `buf_a`
        // (pass 0 A→B, pass 1 B→A, pass 2 A→B, ...).
        let output_in_buf_a = log2_size.is_multiple_of(2);

        debug!(
            target: "sdr_dsp::gpu_fft",
            size = size,
            log2_size = log2_size,
            buffer_bytes = buffer_bytes,
            params_stride = params_stride,
            workgroup_count = workgroup_count,
            output_in_buf_a = output_in_buf_a,
            "GPU FFT engine ready"
        );

        Ok(Self {
            adapter_summary,
            device,
            queue,
            pipeline,
            buf_a,
            buf_b,
            params_bg_a,
            params_bg_b,
            params_stride,
            staging,
            size,
            log2_size,
            workgroup_count,
            output_in_buf_a,
        })
    }
}

impl FftEngine for GpuFftEngine {
    fn forward(&mut self, buf: &mut [Complex]) -> Result<(), DspError> {
        if buf.len() != self.size {
            return Err(DspError::BufferTooSmall {
                need: self.size,
                got: buf.len(),
            });
        }

        // 1. Upload input into buf_a.
        self.queue
            .write_buffer(&self.buf_a, 0, bytemuck::cast_slice(buf));

        // 2. Encode log2(N) compute dispatches, alternating bind
        //    groups so the ping-pong reads/writes flow correctly.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sdr-dsp GPU FFT encoder"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("sdr-dsp GPU FFT compute pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            for s in 0..self.log2_size {
                let bg = if s % 2 == 0 {
                    &self.params_bg_a
                } else {
                    &self.params_bg_b
                };
                let dynamic_offset = s * self.params_stride;
                pass.set_bind_group(0, bg, &[dynamic_offset]);
                pass.dispatch_workgroups(self.workgroup_count, 1, 1);
            }
        }

        // 3. Copy final buffer to staging for readback.
        let final_buf = if self.output_in_buf_a {
            &self.buf_a
        } else {
            &self.buf_b
        };
        let buffer_bytes = (self.size * std::mem::size_of::<Complex>()) as u64;
        encoder.copy_buffer_to_buffer(final_buf, 0, &self.staging, 0, buffer_bytes);

        self.queue.submit(std::iter::once(encoder.finish()));

        // 4. Map staging synchronously (blocks the calling thread
        //    via device.poll(Wait) inside wgpu-core).
        let slice = self.staging.slice(..);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |res| {
            // Ignore send errors — the receiver side always drains
            // exactly once on the same thread, so the channel
            // can't be closed before we post.
            let _ = tx.send(res);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| DspError::GpuUnavailable(format!("device poll failed: {e}")))?;
        rx.recv()
            .map_err(|e| DspError::GpuUnavailable(format!("staging map channel: {e}")))?
            .map_err(|e| DspError::GpuUnavailable(format!("staging map failed: {e}")))?;

        // 5. Copy mapped bytes into the caller's buffer.
        let data = slice.get_mapped_range();
        let as_complex: &[Complex] = bytemuck::cast_slice(&data);
        buf.copy_from_slice(as_complex);
        drop(data);
        self.staging.unmap();

        Ok(())
    }

    fn size(&self) -> usize {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::RustFftEngine;

    /// Max allowed normalized L2 error between GPU and CPU spectra:
    ///
    ///     sum((gpu - cpu)^2) / sum(cpu^2)  <  PARITY_REL_L2_TOL
    ///
    /// A single-scalar comparison averages out the per-bin f32
    /// rounding noise that diverges between rustfft (scalar SIMD)
    /// and the GPU (vendor-specific cos/sin polynomial + possibly
    /// contracted FMAs). Per-bin absolute tolerance is a trap at
    /// high FFT sizes — bin magnitudes scale with √N so any fixed
    /// threshold either accepts nonsense at 65536 or rejects real
    /// parity at 2048. The relative L2 form is scale-invariant.
    ///
    /// 1e-4 is tight enough to catch an algorithmic bug (off-by-one
    /// in pass indexing, wrong twiddle sign, bit-reversal mistake)
    /// and loose enough to tolerate FMA / polynomial-sin/cos
    /// divergence across backends.
    const PARITY_REL_L2_TOL: f32 = 1e-4;

    fn rand_like(n: usize, seed: f32) -> Vec<Complex> {
        (0..n)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f32;
                // A mix of two sinusoids keeps every bin non-trivial
                // — energy scattered across the spectrum instead of
                // a single bright peak.
                Complex {
                    re: (t * 0.011 + seed).sin() + 0.5 * (t * 0.073).cos(),
                    im: (t * 0.019 + seed).cos() - 0.3 * (t * 0.041).sin(),
                }
            })
            .collect()
    }

    fn try_gpu_engine(size: usize) -> Option<GpuFftEngine> {
        match GpuFftEngine::new(size) {
            Ok(engine) => Some(engine),
            Err(DspError::GpuUnavailable(msg)) => {
                eprintln!("skipping GPU parity test at size={size}: {msg}");
                None
            }
            // Any non-"GPU missing" construction failure is a real
            // bug. `unreachable!` keeps clippy's `panic` lint
            // quiet and also documents that the only non-
            // `GpuUnavailable` errors that can get here would
            // indicate a regression in parameter validation.
            Err(e) => unreachable!("unexpected GPU FFT construction error at size={size}: {e}"),
        }
    }

    fn assert_parity(size: usize) {
        let Some(mut gpu) = try_gpu_engine(size) else {
            return;
        };
        let mut cpu = RustFftEngine::new(size).expect("CPU FFT");

        let input = rand_like(size, 1.5);
        let mut gpu_buf = input.clone();
        let mut cpu_buf = input;

        gpu.forward(&mut gpu_buf).expect("GPU forward");
        cpu.forward(&mut cpu_buf).expect("CPU forward");

        // Normalized L2 error — see PARITY_REL_L2_TOL rationale.
        let mut err_sq = 0.0_f64;
        let mut ref_sq = 0.0_f64;
        for (g, c) in gpu_buf.iter().zip(cpu_buf.iter()) {
            let dr = f64::from(g.re - c.re);
            let di = f64::from(g.im - c.im);
            err_sq += dr * dr + di * di;
            let cr = f64::from(c.re);
            let ci = f64::from(c.im);
            ref_sq += cr * cr + ci * ci;
        }
        #[allow(clippy::cast_possible_truncation)]
        let rel = (err_sq / ref_sq.max(f64::MIN_POSITIVE)) as f32;
        assert!(
            rel < PARITY_REL_L2_TOL,
            "size={size} relative L2 error {rel:.3e} exceeds tolerance {PARITY_REL_L2_TOL:.0e}"
        );
    }

    #[test]
    fn parity_2048() {
        assert_parity(2048);
    }

    #[test]
    fn parity_8192() {
        assert_parity(8192);
    }

    #[test]
    fn parity_65536() {
        assert_parity(65_536);
    }

    #[test]
    fn rejects_non_power_of_two() {
        // 1000 isn't a power of two, should fail param validation
        // before even touching the GPU — so this test is safe on
        // CI runners without a GPU.
        let err = GpuFftEngine::new(1000).expect_err("must reject");
        assert!(
            matches!(err, DspError::InvalidParameter(_)),
            "expected InvalidParameter, got {err:?}"
        );
    }

    #[test]
    fn rejects_size_one() {
        let err = GpuFftEngine::new(1).expect_err("must reject");
        assert!(
            matches!(err, DspError::InvalidParameter(_)),
            "expected InvalidParameter, got {err:?}"
        );
    }
}
