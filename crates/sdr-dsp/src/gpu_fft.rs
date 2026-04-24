//! wgpu-backed FFT engine — tiered 2D Cooley-Tukey decomposition
//! with single-dispatch shared-memory sub-FFTs per tier (epic
//! #452 phase 2b / #179).
//!
//! Lives behind the same [`FftEngine`] trait as [`RustFftEngine`]
//! so callers can hot-swap CPU↔GPU without touching the signal
//! path. The host side owns every wgpu object (device, queue,
//! pipelines, ping-pong buffers, uniform buffer, bind groups,
//! staging) for the engine's lifetime; the hot path (`forward`)
//! only issues **two** compute dispatches plus one readback copy,
//! zero allocations.
//!
//! # Why tiered
//!
//! Phase 2 (see `git log` for the replaced code) issued
//! `log2(N)` sequential dispatches — one per Stockham pass —
//! and paid ~15 µs of driver scheduling overhead on each. At
//! N = 65536 that was ~200 µs of pure dispatch overhead, which
//! exceeded the CPU's entire compute budget (~126 µs) before any
//! GPU work happened.
//!
//! Phase 2b collapses the sequential passes into a single
//! shared-memory sub-FFT per workgroup, then composes larger
//! transforms via a 2D decomposition `N = P·Q`:
//!
//! - Stage 1: P workgroups, each computing a size-Q sub-FFT
//!   over stride-P reads of the input, applying the cross-tier
//!   twiddle `ω_N^(p·k_Q)`, and writing to a column-major
//!   scratch buffer.
//! - Stage 2: Q workgroups, each computing a size-P sub-FFT over
//!   contiguous reads of a scratch column and writing to the
//!   output in natural order.
//!
//! Two dispatches per transform, regardless of N (up to the max
//! supported size of 256·256 = 65536 — a third stage for larger
//! N is a phase-2c extension). Each dispatch pays its own
//! driver-scheduling cost once, not `log2(N)` times.
//!
//! # Decomposition
//!
//! | N     | P   | Q   | stage-1 sub-FFT | stage-2 sub-FFT |
//! |-------|-----|-----|-----------------|-----------------|
//! | 2048  | 32  | 64  | 32              | 64              |
//! | 4096  | 64  | 64  | 64              | 64              |
//! | 8192  | 64  | 128 | 64              | 128             |
//! | 16384 | 128 | 128 | 128             | 128             |
//! | 32768 | 128 | 256 | 128             | 256             |
//! | 65536 | 256 | 256 | 256             | 256             |
//!
//! Sizes below `MAX_SUB_N` (≤ 256) use the degenerate P = 1
//! case — single stage, no cross-tier twiddle.
//!
//! # Shared memory per workgroup
//!
//! Two `var<workgroup>` arrays of `vec2<f32>` each sized `SUB_N`
//! via the shader's override constants. Max usage is
//! 2 · 256 · 8 B = 4 KB — well under the 32 KB limit every GPU
//! we target publishes (NVIDIA discrete, AMD RDNA iGPU), leaving
//! room for 8+ concurrent workgroups per compute unit.
//!
//! # Pre-allocation discipline (unchanged from phase 2)
//!
//! Everything is built once in [`GpuFftEngine::with_options`]:
//! device, queue, both pipelines (one per distinct sub-FFT size),
//! both storage buffers, uniform buffer, both stage bind groups,
//! and the staging download buffer. `forward()` issues only queue
//! operations: `write_buffer` for the input, two compute
//! dispatches, a `copy_buffer_to_buffer` for readback, and a
//! blocking `poll(Wait)`. No heap activity on the hot path.
//!
//! # Sync surface over async wgpu
//!
//! wgpu 29's `request_adapter` / `request_device` / `map_async`
//! are futures. The [`FftEngine`] trait is sync, so we block via
//! [`pollster`] on the construction path and poll the device on
//! the readback path. The forward path does *not* spawn an async
//! runtime — `device.poll(PollType::Wait)` blocks on the calling
//! thread inside wgpu-core's own event loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytemuck::{Pod, Zeroable};
use sdr_types::{Complex, DspError};
use tracing::{debug, info};

use crate::fft::FftEngine;

/// WGSL source for the tiered FFT compute kernel. Specialized at
/// pipeline creation time via override constants.
const SHADER_SRC: &str = include_str!("gpu_fft.wgsl");

/// Largest sub-FFT size (per stage) this phase supports. Each
/// workgroup allocates `2 · SUB_N · 8 B` of shared memory; keeping
/// `MAX_SUB_N = 256` holds the footprint at 4 KB, which every
/// GPU in our matrix runs comfortably with multiple concurrent
/// workgroups per compute unit.
const MAX_SUB_N: usize = 256;

/// Fixed number of complex points each shader thread owns across
/// one sub-FFT. With `POINTS_PER_THREAD = 2` and
/// `WORKGROUP_SIZE = SUB_N / 2`, each thread does exactly one
/// butterfly per pass — no bounds-checking, no wasted lanes.
const POINTS_PER_THREAD: u32 = 2;

/// Size of [`ShaderParams`] in bytes — small enough that we just
/// stride by `min_uniform_buffer_offset_alignment` (256 on every
/// real GPU) between stage 1 and stage 2 entries in the uniform
/// buffer.
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

/// Layout must match `struct Params` in `gpu_fft.wgsl`. All
/// fields `u32` so the struct has scalar alignment 4 and total
/// size 36 bytes. We stride by
/// `min_uniform_buffer_offset_alignment` (≥256 on all supported
/// GPUs) between entries, so the 36-byte natural size just fits
/// within the first entry and the rest of the stride is padding.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable)]
struct ShaderParams {
    total_n: u32,
    input_sub_offset_base: u32,
    input_sub_offset_mult: u32,
    input_stride: u32,
    output_sub_offset_base: u32,
    output_sub_offset_mult: u32,
    output_stride: u32,
    apply_twiddle: u32,
    twiddle_p_mult: u32,
}

/// Shared completion slot for the async readback callback.
///
/// `forward()` clones this `Arc` into a `map_async` closure and
/// then blocks on `device.poll(Wait)`. The callback fires inside
/// `poll` on the calling thread, so the `Mutex` is never
/// contended — we use it only to satisfy `map_async`'s
/// `WasmNotSend + 'static` closure bound (the slot has to be
/// `Send`) and to cleanly handle the "callback didn't fire"
/// error case. Allocated once in `new()`; reused by every
/// `forward()` call.
type MapComplete = Arc<Mutex<Option<Result<(), wgpu::BufferAsyncError>>>>;

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

/// 2D decomposition `N = P · Q` used internally by
/// [`GpuFftEngine`]. `P == 1` encodes the degenerate "single
/// stage" case (for `N ≤ MAX_SUB_N`).
#[derive(Debug, Clone, Copy)]
struct Decomposition {
    p: u32,
    q: u32,
}

impl Decomposition {
    fn for_size(n: usize) -> Result<Self, DspError> {
        if n < 2 || !n.is_power_of_two() {
            return Err(DspError::InvalidParameter(format!(
                "GPU FFT size must be a power of two ≥ 2, got {n}"
            )));
        }
        if n <= MAX_SUB_N {
            // Single-stage path: P = 1, Q = N. Stage 1 dispatches
            // one workgroup, no cross-tier twiddle, natural-order
            // output.
            let q = u32::try_from(n)
                .map_err(|_| DspError::InvalidParameter(format!("FFT size {n} exceeds u32")))?;
            return Ok(Self { p: 1, q });
        }

        // Two-stage path: pick P as the largest power of two
        // ≤ √N that still keeps Q ≤ MAX_SUB_N. `trailing_zeros`
        // on a power of two gives log2 directly; dividing log2(N)
        // by 2 (rounding down) gives log2(floor(√N)).
        let log2_n = n.trailing_zeros();
        let max_log2 = MAX_SUB_N.trailing_zeros();
        let p_log2 = (log2_n / 2).clamp(1, max_log2);
        let mut p = 1_u32 << p_log2;
        #[allow(clippy::cast_possible_truncation)]
        let mut q = (n / (p as usize)) as u32;

        // If `Q` exceeds MAX_SUB_N the decomposition is
        // unbalanced — this happens for sizes like N > 65536
        // where we'd need three tiers.
        if q as usize > MAX_SUB_N {
            return Err(DspError::InvalidParameter(format!(
                "GPU FFT size {n} requires a 3-stage decomposition (Q = {q} > MAX_SUB_N = {MAX_SUB_N}); not implemented in phase 2b"
            )));
        }

        // Canonicalize so the smaller factor is P — stage 1 sub-FFTs
        // are often smaller than stage 2, and having the smaller
        // one run first keeps per-workgroup pressure lower on the
        // first wave of workgroups.
        if p > q {
            std::mem::swap(&mut p, &mut q);
        }

        Ok(Self { p, q })
    }

    fn is_single_stage(self) -> bool {
        self.p == 1
    }
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

    // Pipelines keyed by sub-FFT size. Either one (single-stage,
    // or P == Q) or two entries.
    pipelines: HashMap<u32, wgpu::ComputePipeline>,

    // Ping-pong storage buffers. Both sized `size · 8 bytes`.
    // `buf_a` carries `COPY_DST` so `queue.write_buffer` uploads
    // the input here.
    buf_a: wgpu::Buffer,
    buf_b: wgpu::Buffer,

    // Uniform buffer holding at most two `ShaderParams` entries at
    // `params_stride` bytes each. Pre-filled once in `new()` and
    // addressed via dynamic offsets at dispatch time — so the host
    // never writes to it on the forward path.
    bind_group_stage1: wgpu::BindGroup,
    bind_group_stage2: Option<wgpu::BindGroup>,
    params_stride: u32,

    // MAP_READ | COPY_DST staging buffer for device→host readback.
    staging: wgpu::Buffer,
    // Reusable completion slot for the `map_async` readback
    // callback, written while `device.poll(Wait)` is executing
    // and drained by `forward()` afterwards. Held as an `Arc<…>`
    // so the closure can capture a clone without allocating a
    // fresh channel on every transform.
    map_complete: MapComplete,

    size: usize,

    // Sub-FFT size of each stage. Used to dispatch the right
    // pipeline and compute the workgroup count.
    stage1_sub_n: u32,
    stage2_sub_n: u32,
    stage1_workgroups: u32,
    stage2_workgroups: u32,

    // Which physical buffer holds the final result after stage 2
    // (or stage 1 if single-stage). Determines which buffer we
    // copy to staging.
    output_in_buf_a: bool,
}

impl GpuFftEngine {
    /// Create a new GPU FFT engine for the given size with default
    /// adapter options (discrete high-performance adapter).
    ///
    /// # Errors
    ///
    /// - [`DspError::InvalidParameter`] if `size` is not a power
    ///   of two ≥ 2, or if `size > MAX_SUB_N² = 65536` (which
    ///   would require a 3-stage decomposition).
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
        let decomp = Decomposition::for_size(size)?;
        pollster::block_on(Self::build_async(size, decomp, opts))
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

    /// Build the bind group layout used by every pipeline this
    /// engine creates. The three bindings are (0) a uniform buffer
    /// with dynamic offset, (1) a read-only storage buffer, and
    /// (2) a read-write storage buffer.
    fn build_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
        })
    }

    /// Build one specialized compute pipeline for the given
    /// `sub_n`. The shader's override constants are bound to
    /// match the sub-FFT size — `WORKGROUP_SIZE = sub_n / 2`,
    /// `LOG2_SUB_N`, and the hardcoded `POINTS_PER_THREAD`
    /// contract — so each pipeline runs exactly one butterfly per
    /// thread per pass, no wasted lanes.
    fn build_pipeline(
        device: &wgpu::Device,
        shader: &wgpu::ShaderModule,
        layout: &wgpu::PipelineLayout,
        sub_n: u32,
    ) -> wgpu::ComputePipeline {
        let workgroup_size = sub_n / POINTS_PER_THREAD;
        let log2_sub_n = sub_n.trailing_zeros();

        // wgpu 29 takes override constants as `&[(&str, f64)]`.
        let constants: [(&str, f64); 4] = [
            ("WORKGROUP_SIZE", f64::from(workgroup_size)),
            ("SUB_N", f64::from(sub_n)),
            ("LOG2_SUB_N", f64::from(log2_sub_n)),
            ("POINTS_PER_THREAD", f64::from(POINTS_PER_THREAD)),
        ];

        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("sdr-dsp GPU FFT pipeline"),
            layout: Some(layout),
            module: shader,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions {
                constants: &constants,
                zero_initialize_workgroup_memory: false,
            },
            cache: None,
        })
    }

    /// Fill the uniform buffer with at most two `ShaderParams`
    /// entries (stage 1 + stage 2), each at an aligned offset.
    /// Returns the buffer plus the per-entry stride.
    fn build_params_buffer(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        stage1: &ShaderParams,
        stage2: Option<&ShaderParams>,
    ) -> Result<(wgpu::Buffer, u32), DspError> {
        let params_size_u32 = u32::try_from(PARAMS_SIZE_BYTES)
            .map_err(|_| DspError::InvalidParameter("ShaderParams size exceeds u32".into()))?;
        let params_stride = device
            .limits()
            .min_uniform_buffer_offset_alignment
            .max(params_size_u32);

        let entry_count: u64 = if stage2.is_some() { 2 } else { 1 };
        let params_buf_bytes = u64::from(params_stride) * entry_count;
        let params_buf_bytes_usize = usize::try_from(params_buf_bytes).map_err(|_| {
            DspError::InvalidParameter(format!(
                "uniform buffer size {params_buf_bytes} exceeds usize"
            ))
        })?;

        let mut params_bytes = vec![0_u8; params_buf_bytes_usize];
        let end1 = std::mem::size_of::<ShaderParams>();
        params_bytes[..end1].copy_from_slice(bytemuck::bytes_of(stage1));
        if let Some(s2) = stage2 {
            let start2 = params_stride as usize;
            let end2 = start2 + std::mem::size_of::<ShaderParams>();
            params_bytes[start2..end2].copy_from_slice(bytemuck::bytes_of(s2));
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

    /// Create the three device-side buffers the engine uses for
    /// the lifetime of the FFT: two storage buffers (ping-pong)
    /// and a `MAP_READ`-capable staging buffer for readback.
    fn build_storage_buffers(
        device: &wgpu::Device,
        size: usize,
    ) -> Result<(wgpu::Buffer, wgpu::Buffer, wgpu::Buffer), DspError> {
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
        Ok((buf_a, buf_b, staging))
    }

    /// Compile the shader, build all required pipelines (one per
    /// distinct sub-FFT size), and materialize stage-1 / stage-2
    /// bind groups that swap ping-pong buffer roles. Returns
    /// `None` for stage 2 when `decomp` is single-stage.
    fn build_pipelines_and_bgs(
        device: &wgpu::Device,
        bind_group_layout: &wgpu::BindGroupLayout,
        params_buf: &wgpu::Buffer,
        buf_a: &wgpu::Buffer,
        buf_b: &wgpu::Buffer,
        decomp: Decomposition,
    ) -> (
        HashMap<u32, wgpu::ComputePipeline>,
        wgpu::BindGroup,
        Option<wgpu::BindGroup>,
    ) {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sdr-dsp GPU FFT shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sdr-dsp GPU FFT pipeline layout"),
            bind_group_layouts: &[Some(bind_group_layout)],
            immediate_size: 0,
        });

        // stage 1 runs size-Q sub-FFTs, stage 2 runs size-P.
        let stage1_sub_n = decomp.q;
        let stage2_sub_n = decomp.p;
        let mut pipelines: HashMap<u32, wgpu::ComputePipeline> = HashMap::new();
        pipelines.insert(
            stage1_sub_n,
            Self::build_pipeline(device, &shader, &pipeline_layout, stage1_sub_n),
        );
        if !decomp.is_single_stage() && stage2_sub_n != stage1_sub_n {
            pipelines.insert(
                stage2_sub_n,
                Self::build_pipeline(device, &shader, &pipeline_layout, stage2_sub_n),
            );
        }

        let uniform_binding = wgpu::BindingResource::Buffer(wgpu::BufferBinding {
            buffer: params_buf,
            offset: 0,
            size: wgpu::BufferSize::new(PARAMS_SIZE_BYTES),
        });

        // Stage 1: A→B. Uniform at dynamic offset 0.
        let bind_group_stage1 = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sdr-dsp GPU FFT bg stage1 (A→B)"),
            layout: bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_binding.clone(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: buf_a.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: buf_b.as_entire_binding(),
                },
            ],
        });

        // Stage 2: B→A. Storage buffer roles swap; ping-pong
        // preserved. Uniform at dynamic offset = `params_stride`
        // (supplied at dispatch time in `forward`).
        let bind_group_stage2 = if decomp.is_single_stage() {
            None
        } else {
            Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("sdr-dsp GPU FFT bg stage2 (B→A)"),
                layout: bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniform_binding,
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: buf_b.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: buf_a.as_entire_binding(),
                    },
                ],
            }))
        };

        (pipelines, bind_group_stage1, bind_group_stage2)
    }

    async fn build_async(
        size: usize,
        decomp: Decomposition,
        opts: &GpuFftOptions,
    ) -> Result<Self, DspError> {
        let (device, queue, adapter_summary) = Self::acquire_device(opts).await?;

        let (buf_a, buf_b, staging) = Self::build_storage_buffers(&device, size)?;

        let total_n = u32::try_from(size)
            .map_err(|_| DspError::InvalidParameter(format!("FFT size {size} exceeds u32")))?;
        let (stage1_params, stage2_params_opt, output_in_buf_a) =
            Self::build_stage_params(total_n, decomp);

        let bind_group_layout = Self::build_bind_group_layout(&device);
        let (params_buf, params_stride) =
            Self::build_params_buffer(&device, &queue, &stage1_params, stage2_params_opt.as_ref())?;

        let (pipelines, bind_group_stage1, bind_group_stage2) = Self::build_pipelines_and_bgs(
            &device,
            &bind_group_layout,
            &params_buf,
            &buf_a,
            &buf_b,
            decomp,
        );

        let stage1_sub_n = decomp.q;
        let stage2_sub_n = decomp.p;
        let stage1_workgroups = decomp.p;
        let stage2_workgroups = decomp.q;

        debug!(
            target: "sdr_dsp::gpu_fft",
            size = size,
            p = decomp.p,
            q = decomp.q,
            stage1_sub_n = stage1_sub_n,
            stage2_sub_n = stage2_sub_n,
            stage1_workgroups = stage1_workgroups,
            stage2_workgroups = stage2_workgroups,
            params_stride = params_stride,
            pipeline_count = pipelines.len(),
            "GPU FFT engine ready (tiered)"
        );

        Ok(Self {
            adapter_summary,
            device,
            queue,
            pipelines,
            buf_a,
            buf_b,
            bind_group_stage1,
            bind_group_stage2,
            params_stride,
            staging,
            map_complete: Arc::new(Mutex::new(None)),
            size,
            stage1_sub_n,
            stage2_sub_n,
            stage1_workgroups,
            stage2_workgroups,
            output_in_buf_a,
        })
    }

    /// Compute the per-stage `ShaderParams` given the full
    /// decomposition. The returned `output_in_buf_a` flag names
    /// which physical buffer holds the final result after the
    /// last dispatch.
    fn build_stage_params(
        total_n: u32,
        decomp: Decomposition,
    ) -> (ShaderParams, Option<ShaderParams>, bool) {
        if decomp.is_single_stage() {
            // Single stage: one workgroup doing a size-N FFT over
            // contiguous reads of the input. Output is natural-
            // order in buf_b (stage 1 writes A→B).
            let params = ShaderParams {
                total_n,
                input_sub_offset_base: 0,
                input_sub_offset_mult: 0,
                input_stride: 1,
                output_sub_offset_base: 0,
                output_sub_offset_mult: 0,
                output_stride: 1,
                apply_twiddle: 0,
                twiddle_p_mult: 0,
            };
            return (params, None, /* output_in_buf_a */ false);
        }

        let p = decomp.p;

        // Stage 1: P workgroups. Workgroup `p ∈ [0, P)` reads
        // x[p + k·P] for k ∈ [0, Q), applies the cross-tier
        // twiddle ω_N^(p·k_Q), writes Z[p, k_Q] at column-major
        // address `k_Q · P + p`.
        let stage1 = ShaderParams {
            total_n,
            input_sub_offset_base: 0,
            input_sub_offset_mult: 1, // wg_id * 1 = p
            input_stride: p,          // stride-P reads
            output_sub_offset_base: 0,
            output_sub_offset_mult: 1, // wg_id * 1 = p (column-major base)
            output_stride: p,          // stride-P writes (k_Q * P + p)
            apply_twiddle: 1,
            twiddle_p_mult: 1, // p = wg_id * 1
        };

        // Stage 2: Q workgroups. Workgroup `k_Q ∈ [0, Q)` reads
        // Z[p, k_Q] at addresses `k_Q · P + p` for p ∈ [0, P) —
        // contiguous reads since Z is column-major. Writes to
        // y[k_P · Q + k_Q] — stride-Q writes.
        let q = decomp.q;
        let stage2 = ShaderParams {
            total_n,
            input_sub_offset_base: 0,
            input_sub_offset_mult: p, // wg_id * P = k_Q * P (column base)
            input_stride: 1,          // contiguous reads within column
            output_sub_offset_base: 0,
            output_sub_offset_mult: 1, // wg_id * 1 = k_Q
            output_stride: q,          // stride-Q writes (k_P * Q + k_Q)
            apply_twiddle: 0,
            twiddle_p_mult: 0,
        };

        // Stage 2 writes A→B→A, so the result is back in buf_a.
        (stage1, Some(stage2), /* output_in_buf_a */ true)
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

        // 2. Encode one or two compute dispatches.
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

            // Stage 1 dispatch. Uniform at offset 0. Missing
            // pipeline here means `build_async` returned Ok with a
            // pipelines map that doesn't cover the stage we need —
            // an internal regression, but surface it as a typed
            // `GpuUnavailable` rather than panicking from library
            // code.
            let stage1_pipeline = self.pipelines.get(&self.stage1_sub_n).ok_or_else(|| {
                DspError::GpuUnavailable(format!(
                    "missing stage-1 GPU FFT pipeline for sub_n={}",
                    self.stage1_sub_n,
                ))
            })?;
            pass.set_pipeline(stage1_pipeline);
            pass.set_bind_group(0, &self.bind_group_stage1, &[0]);
            pass.dispatch_workgroups(self.stage1_workgroups, 1, 1);

            // Stage 2 dispatch (only for tiered). Uniform at
            // offset `params_stride`.
            if let Some(bg2) = &self.bind_group_stage2 {
                let stage2_pipeline = self.pipelines.get(&self.stage2_sub_n).ok_or_else(|| {
                    DspError::GpuUnavailable(format!(
                        "missing stage-2 GPU FFT pipeline for sub_n={}",
                        self.stage2_sub_n,
                    ))
                })?;
                pass.set_pipeline(stage2_pipeline);
                pass.set_bind_group(0, bg2, &[self.params_stride]);
                pass.dispatch_workgroups(self.stage2_workgroups, 1, 1);
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
        //    via device.poll(Wait) inside wgpu-core). We reuse the
        //    pre-allocated `map_complete` slot across calls — no
        //    channel/vector allocation on the hot path. The
        //    `map_async` callback fires *during* `device.poll`
        //    on our thread, so ordering is:
        //      - clear slot
        //      - schedule callback (captures an Arc clone)
        //      - poll(Wait) runs the callback synchronously
        //      - poll returns; slot is populated
        //    The `Mutex` is only here to satisfy `map_async`'s
        //    `Send + 'static` closure bound; it's never contended.
        let slice = self.staging.slice(..);
        {
            let mut slot = self
                .map_complete
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = None;
        }
        let completion = Arc::clone(&self.map_complete);
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let mut slot = completion
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some(res);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| DspError::GpuUnavailable(format!("device poll failed: {e}")))?;
        let map_result = self
            .map_complete
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| {
                DspError::GpuUnavailable("map_async callback did not fire during poll".into())
            })?;
        map_result.map_err(|e| DspError::GpuUnavailable(format!("staging map failed: {e}")))?;

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

    /// Extra coverage for a size that hits the single-stage
    /// (P = 1) path — important because the tiered code is the
    /// exciting part, but the degenerate path shares all the
    /// uniform-buffer / bind-group plumbing and deserves its
    /// own correctness check.
    #[test]
    fn parity_single_stage_256() {
        assert_parity(256);
    }

    /// N = 512 decomposes to P = 16, Q = 32. Stage 2's size-16
    /// sub-FFT exercises the smallest pipeline specialisation the
    /// engine can generate (`WORKGROUP_SIZE` = 8, below a single
    /// NVIDIA warp) — a useful correctness check that the
    /// `POINTS_PER_THREAD = 2` invariant holds even when the
    /// workgroup is sub-warp sized.
    #[test]
    fn parity_512() {
        assert_parity(512);
    }

    /// N = 1024 decomposes to P = Q = 32 — the equal-factor path
    /// where `build_pipelines_and_bgs` reuses the same pipeline
    /// for both stages rather than building a second one. This
    /// test confirms the stage-2 bind group and the reused
    /// pipeline cooperate correctly.
    #[test]
    fn parity_1024() {
        assert_parity(1024);
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

    /// Sizes above `MAX_SUB_N² = 65536` need a 3-stage
    /// decomposition that isn't in phase 2b. The error path
    /// should be an `InvalidParameter` with a clear message —
    /// not a `GpuUnavailable` (which would mislead a caller that
    /// has a working GPU but asked for an unsupported size).
    #[test]
    fn rejects_too_large() {
        let err = GpuFftEngine::new(131_072).expect_err("must reject");
        // `unreachable!` rather than `panic!` to satisfy clippy's
        // production-code panic lint — the guard value can only
        // be reached on a real regression in `Decomposition::for_size`.
        let DspError::InvalidParameter(msg) = err else {
            unreachable!("expected InvalidParameter");
        };
        assert!(
            msg.contains("3-stage") || msg.contains("131072"),
            "expected informative error, got: {msg}"
        );
    }
}
