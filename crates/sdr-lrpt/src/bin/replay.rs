//! `sdr-lrpt-replay` — decode a captured Meteor LRPT IQ file
//! to per-channel PNGs.
//!
//! ```text
//!   sdr-lrpt-replay <input.iq> <output_dir>
//! ```
//!
//! Input format: complex<f32> interleaved (real, imag) at the
//! Meteor LRPT working sample rate
//! ([`sdr_dsp::lrpt::SAMPLE_RATE_HZ`] = 144 ksps). The file is
//! streamed in fixed-size chunks via `BufReader`; each chunk is
//! `bytemuck::cast_slice`d into [`Complex`] pairs in place — no
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

use std::fs::File;
use std::io::{BufReader, Read};
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

/// CLI argv length: `sdr-lrpt-replay <input.iq> <output_dir>`
/// = 3 elements (program name + 2 positional args).
const EXPECTED_ARGC: usize = 3;

/// Exit code for usage errors (missing args). Convention follows
/// BSD `sysexits.h`: 64 = `EX_USAGE`. We use 2 here for parity
/// with `getopts`-style tools (Python argparse, GNU getopt).
const USAGE_EXIT_CODE: u8 = 2;

/// Streaming chunk size for the IQ file reader. 64 KiB = 8192
/// IQ samples per chunk: small enough to keep peak memory flat
/// regardless of capture length (a 2-hour pass is ~8 GiB on
/// disk; whole-file slurp would OOM), large enough to amortize
/// syscall + bytemuck-cast overhead. Multiple of
/// `IQ_SAMPLE_BYTES` so chunks split exactly on sample
/// boundaries — no in-flight partial sample to carry over.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const _: () = assert!(STREAM_CHUNK_BYTES.is_multiple_of(IQ_SAMPLE_BYTES));

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
    if args.len() != EXPECTED_ARGC {
        eprintln!("usage: sdr-lrpt-replay <input.iq> <output_dir>");
        eprintln!();
        eprintln!("input.iq:    interleaved complex<f32> @ {SAMPLE_RATE_HZ} Hz");
        eprintln!("output_dir:  one ch<APID>.png written per detected channel");
        return ExitCode::from(USAGE_EXIT_CODE);
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

    let file = File::open(in_path).map_err(|e| format!("open {in_path}: {e}"))?;
    let mut reader = BufReader::new(file);

    let mut demod = LrptDemod::new().map_err(|e| format!("LrptDemod::new: {e}"))?;
    let mut pipeline = LrptPipeline::new();
    let mut buf = vec![0_u8; STREAM_CHUNK_BYTES];
    let mut total_samples = 0_u64;
    let mut symbol_count = 0_u64;
    loop {
        // Read up to STREAM_CHUNK_BYTES. Chunks at end-of-file
        // may be short; only multiples of IQ_SAMPLE_BYTES are
        // processed and the trailing partial sample (if any)
        // gets reported as an alignment error after the loop.
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("read {in_path}: {e}"))?;
        if n == 0 {
            break;
        }
        let aligned = n - (n % IQ_SAMPLE_BYTES);
        let samples: &[Complex] = bytemuck::cast_slice(&buf[..aligned]);
        for &sample in samples {
            if let Some(soft) = demod.process(sample) {
                pipeline.push_symbol(soft);
                symbol_count += 1;
            }
        }
        total_samples += (aligned / IQ_SAMPLE_BYTES) as u64;
        // If the read was short AND alignment trimmed bytes off,
        // the file's last partial sample is invalid.
        if n != STREAM_CHUNK_BYTES && n != aligned {
            return Err(format!(
                "input {in_path} has {n_partial} trailing bytes that don't form a complete sample (need a multiple of {IQ_SAMPLE_BYTES})",
                n_partial = n - aligned,
            ));
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "total_samples is bounded by file size; even hours-long captures stay below f64's 52-bit mantissa"
    )]
    let duration_s = total_samples as f64 / f64::from(SAMPLE_RATE_HZ);
    eprintln!("input: {total_samples} samples ({duration_s:.1} s @ {SAMPLE_RATE_HZ} Hz)");
    eprintln!("processed: {symbol_count} symbol pairs from {total_samples} IQ samples");

    let assembler = pipeline.assembler();
    let mut saved = 0_usize;
    let mut apids: Vec<u16> = assembler.channels().map(|(&apid, _)| apid).collect();
    apids.sort_unstable();
    for apid in &apids {
        // Defensive lookup: the assembler's channel set could
        // theoretically change between the `apids` enumeration
        // and this read in a future refactor that adds another
        // path mutating it. Today there's only one writer (the
        // single-threaded loop above), but skipping silently is
        // cheap and keeps the binary robust against future
        // changes. Per CR round 2 on PR #540.
        let Some(channel) = assembler.channel(*apid) else {
            tracing::warn!("APID {apid} disappeared during export; skipping");
            continue;
        };
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
