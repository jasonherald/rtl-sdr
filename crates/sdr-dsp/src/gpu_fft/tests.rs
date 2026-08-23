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
