//! `sdr-lrpt-replay` — decode a captured Meteor LRPT IQ file
//! to per-channel PNGs.
//!
//! ```text
//!   sdr-lrpt-replay <input.iq> <output_dir>
//! ```
//!
//! Input format: complex<f32> interleaved (real, imag) at the
//! Meteor LRPT working sample rate
//! ([`sdr_dsp::lrpt::SAMPLE_RATE_HZ`] = 144 ksps). Bytes are
//! `bytemuck::cast_slice`d straight into [`Complex`] pairs — no
//! per-sample copy. Files captured by `sdr-cli record` at
//! 144 ksps land in this format already.
//!
//! Output: one grayscale PNG per APID present in the recording
//! (`<output_dir>/ch<apid>.png`) plus a default RGB composite
//! (`<output_dir>/composite-rgb.png`) using APIDs 64/65/66 if
//! all three are present.
//!
//! End-to-end smoke test for the full LRPT chain: IQ → QPSK
//! demod ([`LrptDemod`]) → FEC chain ([`FecChain`] inside
//! [`LrptPipeline::push_symbol`]) → CCSDS demux → JPEG decode
//! → image assembly → PNG. A single binary that exercises
//! every stage of epic #469.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sdr_dsp::lrpt::{LrptDemod, SAMPLE_RATE_HZ};
use sdr_lrpt::{
    LrptPipeline,
    image::{save_channel, save_composite},
};
use sdr_types::Complex;

/// Bytes per IQ sample on disk: two f32s (real + imag).
const IQ_SAMPLE_BYTES: usize = 8;

/// Default RGB composite channel triple. Per the Meteor APID
/// convention: 64 = blue (visible), 65 = green (visible-IR),
/// 66 = red (near-IR). Composite written only when all three
/// channels populated.
const COMPOSITE_R_APID: u16 = 66;
const COMPOSITE_G_APID: u16 = 65;
const COMPOSITE_B_APID: u16 = 64;

fn main() -> ExitCode {
    // Initialise tracing so the chain's `tracing::trace!` /
    // `tracing::warn!` lines surface during a replay run when
    // RUST_LOG is set.
    tracing_subscriber::fmt::try_init().ok();

    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: sdr-lrpt-replay <input.iq> <output_dir>");
        eprintln!();
        eprintln!("input.iq:    interleaved complex<f32> @ 144 ksps");
        eprintln!("output_dir:  one ch<APID>.png written per detected channel");
        return ExitCode::from(2);
    }
    match run(&args[1], &PathBuf::from(&args[2])) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(in_path: &str, out_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;

    let iq_bytes = std::fs::read(in_path).map_err(|e| format!("read {in_path}: {e}"))?;
    if iq_bytes.len() % IQ_SAMPLE_BYTES != 0 {
        return Err(format!(
            "input size {} is not a multiple of {IQ_SAMPLE_BYTES} (one complex<f32> = {IQ_SAMPLE_BYTES} bytes)",
            iq_bytes.len(),
        ));
    }
    let n_samples = iq_bytes.len() / IQ_SAMPLE_BYTES;
    let samples: &[Complex] = bytemuck::cast_slice(&iq_bytes);
    #[allow(
        clippy::cast_precision_loss,
        reason = "n_samples is bounded by file size; even hours-long captures stay below f64's 52-bit mantissa"
    )]
    let duration_s = n_samples as f64 / f64::from(SAMPLE_RATE_HZ);
    eprintln!("input: {n_samples} samples ({duration_s:.1} s @ {SAMPLE_RATE_HZ} Hz)");

    let mut demod = LrptDemod::new().map_err(|e| format!("LrptDemod::new: {e}"))?;
    let mut pipeline = LrptPipeline::new();
    let mut symbol_count = 0_u64;
    for &sample in samples {
        if let Some(soft) = demod.process(sample) {
            pipeline.push_symbol(soft);
            symbol_count += 1;
        }
    }
    eprintln!("processed: {symbol_count} symbol pairs from {n_samples} IQ samples");

    let assembler = pipeline.assembler();
    let mut saved = 0_usize;
    let mut apids: Vec<u16> = assembler.channels().map(|(&apid, _)| apid).collect();
    apids.sort_unstable();
    for apid in &apids {
        let channel = assembler.channel(*apid).expect("listed apid must exist");
        let path = out_dir.join(format!("ch{apid}.png"));
        match save_channel(&path, channel) {
            Ok(()) => {
                eprintln!("saved {} ({}× lines)", path.display(), channel.lines);
                saved += 1;
            }
            Err(e) => eprintln!("note: ch{apid} not saved ({e})"),
        }
    }
    let composite_path = out_dir.join("composite-rgb.png");
    match save_composite(
        &composite_path,
        assembler,
        COMPOSITE_R_APID,
        COMPOSITE_G_APID,
        COMPOSITE_B_APID,
    ) {
        Ok(()) => {
            eprintln!("saved {}", composite_path.display());
            saved += 1;
        }
        Err(e) => eprintln!(
            "note: composite-rgb (APIDs {COMPOSITE_R_APID}/{COMPOSITE_G_APID}/{COMPOSITE_B_APID}) not saved ({e})"
        ),
    }
    eprintln!("total: {saved} PNGs in {}", out_dir.display());
    if saved == 0 {
        return Err("no PNGs written — likely no usable signal in the input IQ".into());
    }
    Ok(())
}
