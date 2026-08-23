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
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sdr_dsp::lrpt::{LrptDemod, LrptMode, SAMPLE_RATE_HZ, SYMBOL_RATE_HZ};
use sdr_lrpt::{
    LrptPipeline,
    image::{save_channel, save_composite},
};
use sdr_types::Complex;

/// Bytes per IQ sample on disk: two f32s (real + imag).
const IQ_SAMPLE_BYTES: usize = 8;

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
    // Status lines go through `tracing` at `info` (stderr, no
    // timestamps); RUST_LOG still overrides the filter to surface the
    // chain's `debug!` / `trace!` lines (#733).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .with_target(false)
        .try_init()
        .ok();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!(
            "usage: sdr-lrpt-replay <input.iq> <output_dir> \
             [qpsk|qpsk-diff|oqpsk|oqpsk-diff|soft|soft-diff]"
        );
        eprintln!();
        eprintln!("input.iq:    interleaved complex<f32> @ {SAMPLE_RATE_HZ} Hz");
        eprintln!("             (or interleaved i8 soft pairs for soft / soft-diff)");
        eprintln!("output_dir:  one ch<APID>.png written per detected channel");
        eprintln!(
            "mode:        oqpsk (current M2-3/M2-4, default) | qpsk (legacy M N2)  — demod IQ"
        );
        eprintln!("             qpsk-diff | oqpsk-diff  — demod IQ + differential decode");
        eprintln!("             soft  — feed a meteor_demod .s file straight to the FEC chain");
        eprintln!("             soft-diff  — soft input + differential decode (legacy M2)");
        return ExitCode::from(USAGE_EXIT_CODE);
    }
    let mode_arg = args.get(3).map(String::as_str);
    let Some((mode, soft, differential)) = parse_mode(mode_arg) else {
        eprintln!(
            "error: unknown mode '{}' (expected qpsk, qpsk-diff, oqpsk, oqpsk-diff, soft, or soft-diff)",
            mode_arg.unwrap_or_default()
        );
        return ExitCode::from(USAGE_EXIT_CODE);
    };
    tracing::info!("mode: {mode:?} soft_input={soft} differential={differential}");
    match run(&args[1], &PathBuf::from(&args[2]), mode, soft, differential) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Map the optional mode argument to (demod mode, bypass-demod soft
/// input?, differential precoding?). A bare invocation replays the
/// current METEOR-M2 3 / M2-4 downlink: OQPSK, no precoding (#730).
fn parse_mode(arg: Option<&str>) -> Option<(LrptMode, bool, bool)> {
    match arg {
        None | Some("oqpsk") => Some((LrptMode::Oqpsk, false, false)),
        Some("oqpsk-diff") => Some((LrptMode::Oqpsk, false, true)),
        Some("qpsk") => Some((LrptMode::Qpsk, false, false)),
        Some("qpsk-diff") => Some((LrptMode::Qpsk, false, true)),
        Some("soft") => Some((LrptMode::Qpsk, true, false)),
        Some("soft-diff") => Some((LrptMode::Qpsk, true, true)),
        Some(_) => None,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "single linear CLI driver: stream IQ/soft input → demod → FEC → PNGs, plus optional diagnostics"
)]
fn run(
    in_path: &str,
    out_dir: &Path,
    mode: LrptMode,
    soft: bool,
    differential: bool,
) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;

    let file = File::open(in_path).map_err(|e| format!("open {in_path}: {e}"))?;
    let mut reader = BufReader::new(file);

    let mut pipeline = LrptPipeline::new_with_differential(differential);
    // Optional debug: dump the demod's soft symbols (IQ path only) to
    // the file named by SDR_LRPT_DUMP_SYMS, for comparison against a
    // reference .s stream.
    let mut sym_dump = match std::env::var_os("SDR_LRPT_DUMP_SYMS") {
        Some(p) => Some(std::io::BufWriter::new(File::create(&p).map_err(|e| {
            format!(
                "create SDR_LRPT_DUMP_SYMS file {}: {e}",
                Path::new(&p).display()
            )
        })?)),
        None => None,
    };
    let mut buf = vec![0_u8; STREAM_CHUNK_BYTES];
    let mut total_samples = 0_u64;
    let mut symbol_count = 0_u64;
    if soft {
        // Input is interleaved 8-bit signed soft QPSK symbols (I, Q) —
        // e.g. a meteor_demod `.s` file. Bypass the demod and push the
        // pairs straight into the FEC chain. A single pending I byte is
        // carried across read-chunk boundaries so a pair split at the
        // buffer edge isn't dropped (which would desync every later
        // pair); an odd total byte count is rejected at EOF.
        let mut pending_i: Option<i8> = None;
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| format!("read {in_path}: {e}"))?;
            if n == 0 {
                break;
            }
            for &b in &buf[..n] {
                match pending_i.take() {
                    None => pending_i = Some(b.cast_signed()),
                    Some(i) => {
                        // (i, q) = a complete soft I/Q symbol pair.
                        pipeline.push_symbol([i, b.cast_signed()]);
                        symbol_count += 1;
                    }
                }
            }
        }
        if pending_i.is_some() {
            return Err(format!(
                "input {in_path} has an odd number of soft bytes — not whole i8 I/Q pairs"
            ));
        }
    } else {
        let mut demod =
            LrptDemod::new_with_mode(mode).map_err(|e| format!("LrptDemod::new_with_mode: {e}"))?;
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
            // `try_cast_slice` (not `cast_slice`) so a misaligned/odd
            // buffer surfaces a clean error instead of panicking.
            let samples: &[Complex] = bytemuck::try_cast_slice(&buf[..aligned])
                .map_err(|e| format!("IQ sample cast failed ({e:?})"))?;
            for &sample in samples {
                if let Some(soft) = demod.process(sample) {
                    if let Some(w) = sym_dump.as_mut() {
                        w.write_all(&[soft[0].cast_unsigned(), soft[1].cast_unsigned()])
                            .map_err(|e| format!("symbol-dump write: {e}"))?;
                    }
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
    }

    // Report in the units that match the input: soft mode reads
    // symbol pairs at the symbol rate, IQ mode reads complex samples
    // at the IF sample rate.
    #[allow(
        clippy::cast_precision_loss,
        reason = "counts are bounded by file size; even hours-long captures stay below f64's 52-bit mantissa"
    )]
    if soft {
        let duration_s = symbol_count as f64 / f64::from(SYMBOL_RATE_HZ);
        tracing::info!(
            "input: {symbol_count} soft symbol pairs ({duration_s:.1} s @ {SYMBOL_RATE_HZ} sym/s)"
        );
    } else {
        let duration_s = total_samples as f64 / f64::from(SAMPLE_RATE_HZ);
        tracing::info!(
            "input: {total_samples} IQ samples ({duration_s:.1} s @ {SAMPLE_RATE_HZ} Hz)"
        );
        tracing::info!("processed: {symbol_count} symbol pairs from {total_samples} IQ samples");
    }
    let st = pipeline.fec_stats();
    tracing::info!(
        "fec: rotation_locks={} rotation_rehunts={} sync_timeouts={} cadus_decoded={} cadus_failed={}",
        st.rotation_locks,
        st.rotation_rehunts,
        st.sync_timeouts,
        st.cadus_decoded,
        st.cadus_failed,
    );

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
                tracing::info!("saved {} ({}× lines)", path.display(), channel.lines);
                saved += 1;
            }
            Err(e) => tracing::warn!("ch{apid} not saved ({e})"),
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
            tracing::info!("saved {}", composite_path.display());
            saved += 1;
        }
        Err(e) => tracing::warn!(
            "composite-rgb (APIDs {COMPOSITE_R_APID}/{COMPOSITE_G_APID}/{COMPOSITE_B_APID}) not saved ({e})"
        ),
    }
    tracing::info!("total: {saved} PNGs in {}", out_dir.display());
    if saved == 0 {
        let input_kind = if soft {
            "soft-symbol input"
        } else {
            "input IQ"
        };
        return Err(format!(
            "no PNGs written — likely no usable signal in the {input_kind}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The current Meteor-M2 birds are OQPSK without differential
    /// precoding, so that is what a bare invocation replays (#730);
    /// the legacy soft-file path keeps its explicit `-diff` form.
    #[test]
    fn default_mode_is_oqpsk_without_precoding() {
        assert_eq!(parse_mode(None), Some((LrptMode::Oqpsk, false, false)));
        assert_eq!(
            parse_mode(Some("oqpsk")),
            Some((LrptMode::Oqpsk, false, false))
        );
        assert_eq!(
            parse_mode(Some("oqpsk-diff")),
            Some((LrptMode::Oqpsk, false, true))
        );
        assert_eq!(
            parse_mode(Some("qpsk")),
            Some((LrptMode::Qpsk, false, false))
        );
        assert_eq!(
            parse_mode(Some("qpsk-diff")),
            Some((LrptMode::Qpsk, false, true))
        );
        assert_eq!(
            parse_mode(Some("soft")),
            Some((LrptMode::Qpsk, true, false))
        );
        assert_eq!(
            parse_mode(Some("soft-diff")),
            Some((LrptMode::Qpsk, true, true))
        );
        assert_eq!(parse_mode(Some("bogus")), None);
    }
}
