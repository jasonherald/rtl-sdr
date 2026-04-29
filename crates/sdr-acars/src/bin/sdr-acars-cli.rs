//! `sdr-acars-cli` — read a WAV or IQ file, decode ACARS messages,
//! print in the same text format as `acarsdec -o 1`. Used as the
//! validation harness for the Rust port: diffing this binary's
//! output against `acarsdec`'s on shared input (with volatile
//! fields stripped) is the acceptance test for the DSP / parser
//! correctness — see `tests/e2e_acarsdec_compat.rs`.
//!
//! Two input modes:
//!
//! 1. **WAV** (positional): N-channel WAV at `IF_RATE_HZ` Hz. Each
//!    WAV channel is one ACARS frequency, **already decimated** to
//!    the IF rate. Bypasses [`ChannelBank`]'s decimator stage and
//!    drives [`MskDemod`] + [`FrameParser`] directly per channel,
//!    matching `acarsdec`'s `soundfile.c` path.
//! 2. **IQ** (`--iq <PATH> --rate <Hz> --center <Hz> --channels`):
//!    raw interleaved-`i16` complex samples (the `cs16` convention
//!    used by `rtl_sdr` recordings). Drives through
//!    [`ChannelBank::new`] + [`ChannelBank::process`] end-to-end.
//!
//! Output format mirrors `original/acarsdec/output.c::printmsg`
//! for `inmode == 2` (file-input mode): the date is suppressed,
//! the per-channel `F:` line is omitted (it only appears for
//! live-RTL builds), the channel index is the only header
//! identifier, and the body lines are emitted in the same field
//! order with the same trailing spaces and conditional newlines.
//! Volatile fields (channel-index, level, error count, optional
//! timestamp) are stripped before the e2e diff.

use std::{
    fs::File,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use num_complex::Complex32;
use sdr_acars::{AcarsError, AcarsMessage, ChannelBank, FrameParser, IF_RATE_HZ, MskDemod};

/// Per-channel WAV-input chunk size, matching acarsdec's
/// `MAXNBFRAMES = 4096` (soundfile.c). Keeping this aligned
/// makes the chunked dispatch order byte-equal to acarsdec's,
/// which is what lets the e2e diff strip only volatile fields.
const WAV_CHUNK_FRAMES: usize = 4096;

/// US-6 default channel set (matches the spec). Primary-first
/// order — the same ordering the workspace docs use.
const US_ACARS_CHANNELS: &[f64] = &[
    131_550_000.0,
    131_525_000.0,
    130_025_000.0,
    130_425_000.0,
    130_450_000.0,
    129_125_000.0,
];

#[derive(Parser, Debug)]
#[command(version, about = "ACARS decoder (Rust port of acarsdec)")]
struct Cli {
    /// WAV file (multi-channel @ `IF_RATE_HZ`). Positional.
    /// Mutually exclusive with `--iq`.
    #[arg(value_name = "WAV", conflicts_with = "iq")]
    wav: Option<PathBuf>,

    /// Raw cs16 IQ file (interleaved i16 I/Q at `--rate`).
    #[arg(long, value_name = "PATH", conflicts_with = "wav")]
    iq: Option<PathBuf>,

    /// Source sample rate in Hz (IQ mode only). Default 2.5 `MSps`
    /// matches the airband-mode rate from the spec — fits the
    /// full US-6 channel cluster (span 2.425 MHz) inside Nyquist.
    #[arg(long, default_value_t = 2_500_000)]
    rate: u32,

    /// Source center frequency in Hz (IQ mode only). Default
    /// 130.3375 MHz is the midpoint of the US-6 channel extremes.
    #[arg(long, default_value_t = 130_337_500)]
    center: u32,

    /// Channel list as comma-separated MHz (e.g.
    /// `"131.550,131.525"`). For WAV mode, indexes WAV channels
    /// in order; defaults to the US-6 set.
    #[arg(long, value_delimiter = ',', value_parser = parse_mhz)]
    channels: Option<Vec<f64>>,
}

fn parse_mhz(s: &str) -> Result<f64, String> {
    s.parse::<f64>()
        .map(|mhz| mhz * 1_000_000.0)
        .map_err(|e| format!("invalid frequency '{s}': {e}"))
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sdr-acars-cli: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), AcarsError> {
    let mut stdout = std::io::stdout().lock();

    if let Some(wav_path) = &cli.wav {
        decode_wav(wav_path, cli.channels.as_deref(), &mut stdout)
    } else if let Some(iq_path) = &cli.iq {
        decode_iq(
            iq_path,
            f64::from(cli.rate),
            f64::from(cli.center),
            cli.channels.as_deref().unwrap_or(US_ACARS_CHANNELS),
            &mut stdout,
        )
    } else {
        Err(AcarsError::InvalidInput(
            "no input file: pass a WAV path or --iq <PATH>".into(),
        ))
    }
}

/// Read an N-channel WAV at `IF_RATE_HZ`. Each channel is one
/// ACARS frequency pre-decimated to the IF rate; drive
/// [`MskDemod`] + [`FrameParser`] directly per channel, matching
/// `acarsdec`'s `soundfile.c` flow.
fn decode_wav(
    path: &Path,
    user_channels: Option<&[f64]>,
    out: &mut impl Write,
) -> Result<(), AcarsError> {
    let mut reader = hound::WavReader::open(path).map_err(|e| AcarsError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(e),
    })?;
    let spec = reader.spec();
    if spec.sample_rate != IF_RATE_HZ {
        return Err(AcarsError::InvalidInput(format!(
            "WAV sample rate {} Hz != expected IF rate {IF_RATE_HZ} Hz",
            spec.sample_rate
        )));
    }
    let n_channels = spec.channels as usize;
    let channels: Vec<f64> = match user_channels {
        Some(cs) if cs.len() == n_channels => cs.to_vec(),
        Some(cs) => {
            return Err(AcarsError::InvalidInput(format!(
                "WAV has {n_channels} channels but --channels provided {}",
                cs.len()
            )));
        }
        None => {
            if n_channels > US_ACARS_CHANNELS.len() {
                return Err(AcarsError::InvalidInput(format!(
                    "WAV has {n_channels} channels but US-6 default only \
                     covers {} — pass --channels explicitly",
                    US_ACARS_CHANNELS.len()
                )));
            }
            US_ACARS_CHANNELS.iter().copied().take(n_channels).collect()
        }
    };

    // One demod + parser per channel.
    let mut demods: Vec<MskDemod> = (0..n_channels).map(|_| MskDemod::new()).collect();
    let mut parsers: Vec<FrameParser> = channels
        .iter()
        .enumerate()
        .map(|(i, &f)| {
            // n_channels is bounded by the WAV header (u16) and
            // the US-6 default cap, so the cast is safe.
            #[allow(clippy::cast_possible_truncation)]
            FrameParser::new(i as u8, f)
        })
        .collect();

    // Stream the WAV reader and accumulate one chunk per
    // channel at a time, matching acarsdec's
    // `runSoundfileSample` (soundfile.c:60-78). Demuxing on the
    // fly via `i % n_channels` keeps peak memory bounded to
    // `WAV_CHUNK_FRAMES * n_channels * f32` (~16 KB for the
    // default 4096 frames × 1 channel) regardless of recording
    // length. Per CR round 1 on PR #583 — was previously
    // O(file) RAM because we materialized every sample first.
    let mut per_channel: Vec<Vec<f32>> = (0..n_channels)
        .map(|_| Vec::with_capacity(WAV_CHUNK_FRAMES))
        .collect();
    let mut emit_buf: Vec<AcarsMessage> = Vec::new();
    for (i, sample_result) in reader.samples::<i16>().enumerate() {
        let sample = sample_result.map_err(|e| AcarsError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(e),
        })?;
        let ch_idx = i % n_channels;
        per_channel[ch_idx].push(f32::from(sample) / f32::from(i16::MAX));
        // After every n_channels samples we've completed one
        // frame across all channels; check whether the chunk
        // is full. The first channel reaches WAV_CHUNK_FRAMES
        // exactly when every other channel does too (assuming
        // the WAV file has a complete number of frames, which
        // hound guarantees via header validation).
        if ch_idx == n_channels - 1 && per_channel[0].len() == WAV_CHUNK_FRAMES {
            for (idx, samples) in per_channel.iter_mut().enumerate() {
                demods[idx].process(samples, &mut parsers[idx]);
                parsers[idx].drain(|msg| emit_buf.push(msg));
                samples.clear();
            }
            for msg in emit_buf.drain(..) {
                print_message(&msg, out)?;
            }
        }
    }
    // Flush any tail samples (last partial chunk).
    if !per_channel[0].is_empty() {
        for (idx, samples) in per_channel.iter_mut().enumerate() {
            demods[idx].process(samples, &mut parsers[idx]);
            parsers[idx].drain(|msg| emit_buf.push(msg));
        }
        for msg in emit_buf.drain(..) {
            print_message(&msg, out)?;
        }
    }
    Ok(())
}

/// Read raw cs16 (interleaved i16 I/Q at `rate`) and drive
/// through [`ChannelBank`].
fn decode_iq(
    path: &Path,
    rate: f64,
    center: f64,
    channels: &[f64],
    out: &mut impl Write,
) -> Result<(), AcarsError> {
    let mut bank = ChannelBank::new(rate, center, channels)?;
    let file = File::open(path).map_err(|e| AcarsError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut reader = BufReader::new(file);
    // 4096 IQ samples per block = 16 KiB on the wire.
    let mut buf = vec![0_u8; 4096 * 4];
    let mut block: Vec<Complex32> = Vec::with_capacity(4096);
    let mut emit_buf: Vec<AcarsMessage> = Vec::new();
    // Carry holds a partial sample (1-3 bytes) when a `read`
    // boundary lands mid-sample. `std::io::Read::read()` is
    // allowed to return short — even on regular files near EOF
    // — so we can't assume each `read` lands on a 4-byte
    // boundary. Per CR round 1 on PR #583 — previously rejected
    // valid IQ files whose underlying read happened to return
    // a non-multiple-of-4. Carry up to 3 bytes (max possible
    // partial sample); EOF with non-empty carry is a real
    // truncation error.
    let mut carry: Vec<u8> = Vec::with_capacity(4);

    loop {
        let n = reader.read(&mut buf).map_err(|e| AcarsError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            if !carry.is_empty() {
                return Err(AcarsError::InvalidInput(format!(
                    "IQ file ended with {} byte(s) of partial sample (expected multiples of 4)",
                    carry.len()
                )));
            }
            break;
        }
        // Combine carry + new bytes into a single contiguous
        // view, parse complete 4-byte samples, stash the
        // remainder back into carry. Keeps the alignment math
        // in one place and avoids off-by-one bugs from
        // splitting parsing into "parse-from-carry then
        // parse-from-buf" branches.
        let mut combined: Vec<u8> = Vec::with_capacity(carry.len() + n);
        combined.extend_from_slice(&carry);
        combined.extend_from_slice(&buf[..n]);
        carry.clear();
        let usable = combined.len() - (combined.len() % 4);
        block.clear();
        for chunk in combined[..usable].chunks_exact(4) {
            let i = i16::from_le_bytes([chunk[0], chunk[1]]);
            let q = i16::from_le_bytes([chunk[2], chunk[3]]);
            block.push(Complex32::new(
                f32::from(i) / f32::from(i16::MAX),
                f32::from(q) / f32::from(i16::MAX),
            ));
        }
        carry.extend_from_slice(&combined[usable..]);
        if !block.is_empty() {
            bank.process(&block, |msg| emit_buf.push(msg));
            for msg in emit_buf.drain(..) {
                print_message(&msg, out)?;
            }
        }
    }
    Ok(())
}

/// Format an [`AcarsMessage`] as one acarsdec-text record.
/// Mirrors `original/acarsdec/output.c::printmsg` for
/// `inmode == 2` (file-input mode): no date, no per-channel
/// `F:` line, channel index 1-based in the header. Volatile
/// fields (channel index, level, error count, timestamp) are
/// stripped from the e2e diff by the regex in
/// `tests/e2e_acarsdec_compat.rs::strip_volatile`.
fn print_message(msg: &AcarsMessage, out: &mut impl Write) -> Result<(), AcarsError> {
    // C: chn + 1 — 1-indexed channel number in the header.
    let chn_one_based = u32::from(msg.channel_idx) + 1;
    let stamp = format_timestamp(msg.timestamp);
    // Header. C emits a leading newline, then the bracket, then
    // the volatile fields, then ` --------------------------------\n`.
    // For inmode==2 acarsdec's `printdate` is a no-op, so the
    // strip regex's trailing `[0-9./: ]+` would have nothing to
    // match — we always emit `<unix>.<millis>` so the same regex
    // works regardless of inmode.
    writeln!(
        out,
        "\n[#{chn_one_based} (L:{:+5.1} E:{}) {stamp} --------------------------------",
        msg.level_db, msg.error_count,
    )
    .map_err(io_err)?;

    // Mode + Label. Both lines are emitted without a trailing
    // newline — the C terminates them in the unconditional `\n`
    // after the `bid` block (or after Mode/Label if no bid).
    write!(out, "Mode : {} ", msg.mode as char).map_err(io_err)?;
    write!(
        out,
        "Label : {} ",
        std::str::from_utf8(&msg.label).unwrap_or("??")
    )
    .map_err(io_err)?;

    if msg.block_id != 0 {
        write!(out, "Id : {} ", msg.block_id as char).map_err(io_err)?;
        if msg.ack == b'!' {
            writeln!(out, "Nak").map_err(io_err)?;
        } else {
            writeln!(out, "Ack : {}", msg.ack as char).map_err(io_err)?;
        }
        // C `output.c:503-508` builds `addr` by skipping every '.'
        // in the 7-byte wire field. Our `AcarsMessage.aircraft`
        // keeps the leading dot the wire carries, so we strip it
        // here to match acarsdec's text output byte-for-byte.
        let aircraft_clean: String = msg.aircraft.chars().filter(|&c| c != '.').collect();
        write!(out, "Aircraft reg: {aircraft_clean} ").map_err(io_err)?;
        if is_downlink_blk(msg.block_id) {
            let flight = msg.flight_id.as_deref().unwrap_or("");
            writeln!(out, "Flight id: {flight}").map_err(io_err)?;
            let msgno = msg.message_no.as_deref().unwrap_or("");
            // C: `fprintf(fdout, "No: %4s", msg->no);` — width 4
            // formatter, no trailing newline. The `%4s` right-pads
            // (actually left-pads with spaces) to width 4; for the
            // typical 4-char message numbers it's a no-op.
            write!(out, "No: {msgno:>4}").map_err(io_err)?;
        }
    }

    // Unconditional newline that closes whatever line was last
    // written (Mode/Label, Aircraft-reg, or the No: line).
    writeln!(out).map_err(io_err)?;

    if !msg.text.is_empty() {
        writeln!(out, "{}", msg.text).map_err(io_err)?;
    }
    if !msg.end_of_message {
        writeln!(out, "ETB").map_err(io_err)?;
    }

    out.flush().map_err(io_err)?;
    Ok(())
}

/// `IS_DOWNLINK_BLK` from `output.c:31` — block IDs `0..=9` are
/// downlink (aircraft-to-ground), and only those carry flight ID
/// and message number.
fn is_downlink_blk(bid: u8) -> bool {
    bid.is_ascii_digit()
}

fn format_timestamp(ts: SystemTime) -> String {
    match ts.duration_since(UNIX_EPOCH) {
        Ok(d) => format!("{}.{:03}", d.as_secs(), d.subsec_millis()),
        Err(_) => "0.000".to_string(),
    }
}

fn io_err(e: std::io::Error) -> AcarsError {
    AcarsError::Io {
        path: PathBuf::from("<stdout>"),
        source: e,
    }
}
