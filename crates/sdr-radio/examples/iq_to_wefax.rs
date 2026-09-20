//! Offline IQ → WEFAX render harness (live-capture diagnosis): downconvert a
//! raw complex-IQ WAV to the WEFAX 24 kHz IF rate, run it through the real
//! `WefaxDemodulator` → `WefaxDecoder` chain, and emit greyscale PNGs so we
//! can tell whether a strong near-DC signal is an actual fax chart or just a
//! steady carrier.
//!
//! Unlike `sdr-dsp`'s `wefax_decode_wav` (which takes already-demodulated USB
//! fax *audio*), this harness starts from the RF-level IQ capture: it VFO-tunes
//! at offset 0 (the fax subcarrier is expected at/near baseband DC), rationally
//! resamples the source rate (2.5 MHz / 10 MHz) down to the WEFAX IF rate
//! (24 kHz) with the `sdr-dsp` `RationalResampler` (which anti-alias filters
//! internally), then feeds the complex IF into `WefaxDemodulator` (which applies
//! the #911 +700 Hz subcarrier translation + SSB-USB) and the mono `.l` audio
//! into `WefaxDecoder`.
//!
//! The 10 MHz capture is multi-GB, so the file is streamed in chunks — never
//! loaded whole. Both the free-running (`new_free_running`, guaranteed image)
//! and sync-gated (`new`, may emit 0 lines without a clean phasing preamble)
//! decoders are run in a single pass over the same demodulated audio.
//!
//! Usage:
//!
//! ```text
//! cargo run -p sdr-radio --example iq_to_wefax -- <iq.wav> <free.png> <sync.png>
//! ```

// CLI tool — relax the workspace-pedantic casts, matching
// `wefax_decode_wav.rs`. Library code stays strict.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::print_stdout,
    clippy::trivially_copy_pass_by_ref,
    clippy::items_after_statements,
    clippy::bool_to_int_with_if,
    clippy::too_many_lines
)]

use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use sdr_dsp::multirate::RationalResampler;
use sdr_dsp::wefax::{PIXELS_PER_LINE, WefaxDecoder, WefaxLine, WefaxState};
use sdr_radio::demod::{Demodulator, WefaxDemodulator};
use sdr_types::{Complex, Stereo};

/// WEFAX IF / AF rate the decoder chain expects (Hz).
const WEFAX_IF_RATE: u32 = 24_000;
/// Complex input frames pulled from the WAV per resampler call. Keeps the
/// streaming path exercised and per-chunk allocations bounded (~2 MB of
/// complex data per chunk).
const CHUNK_FRAMES: usize = 1 << 18;
/// Completed-line drain buffer capacity per `process` call.
const READY_QUEUE_CAP: usize = 256;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        eprintln!(
            "usage: {} <iq.wav> <free.png> <sync.png>\n\
             iq.wav: WAVE_FORMAT float, 2 channels = interleaved I,Q f32",
            args[0],
        );
        std::process::exit(2);
    }
    let input = Path::new(&args[1]);
    let free_png = Path::new(&args[2]);
    let sync_png = Path::new(&args[3]);

    let stats = decode(input, free_png, sync_png)
        .unwrap_or_else(|e| panic!("decode {}: {e}", input.display()));

    // Machine-parseable summary line plus a human block.
    println!("--- {} ---", input.display());
    println!("source_rate_hz          {}", stats.source_rate);
    println!(
        "mono_audio_seconds      {:.2}  ({} samples @ {} Hz)",
        stats.mono_samples as f64 / f64::from(WEFAX_IF_RATE),
        stats.mono_samples,
        WEFAX_IF_RATE,
    );
    println!("free_running_lines      {}", stats.free_lines);
    println!("sync_gated_lines        {}", stats.sync_lines);
    println!("sync_reached_imaging    {}", stats.sync_reached_imaging);
    println!("audio_mean_abs          {:.5}", stats.mean_abs);
    println!("audio_peak_abs          {:.5}", stats.peak_abs);
    println!("free_png                {}", free_png.display());
    println!("sync_png                {}", sync_png.display());
}

/// End-of-run statistics reported for the operator.
struct Stats {
    source_rate: u32,
    mono_samples: u64,
    free_lines: usize,
    sync_lines: usize,
    sync_reached_imaging: bool,
    mean_abs: f64,
    peak_abs: f32,
}

fn decode(
    input: &Path,
    free_png: &Path,
    sync_png: &Path,
) -> Result<Stats, Box<dyn std::error::Error>> {
    let mut reader = hound::WavReader::open(input)?;
    let spec = reader.spec();
    eprintln!(
        "input: {} ch, {} Hz, {} bits/sample, {:?}",
        spec.channels, spec.sample_rate, spec.bits_per_sample, spec.sample_format,
    );
    if spec.channels != 2 {
        return Err(format!("expected 2 (I/Q) channels, got {}", spec.channels).into());
    }
    if spec.sample_format != hound::SampleFormat::Float || spec.bits_per_sample != 32 {
        return Err(format!(
            "expected 32-bit float IQ, got {} bits {:?}",
            spec.bits_per_sample, spec.sample_format
        )
        .into());
    }
    let source_rate = spec.sample_rate;

    // VFO offset 0: the fax subcarrier is already near baseband DC. The
    // RationalResampler anti-alias filters internally (lowpass at the smaller
    // of in/out Nyquist = 12 kHz) before decimating, so no separate LPF is
    // needed to avoid aliasing into the 24 kHz IF.
    let mut resampler = RationalResampler::new(f64::from(source_rate), f64::from(WEFAX_IF_RATE))?;
    let mut demod = WefaxDemodulator::new()?;
    let mut free_dec = WefaxDecoder::new_free_running(WEFAX_IF_RATE);
    let mut sync_dec = WefaxDecoder::new(WEFAX_IF_RATE)?;

    // Reusable scratch buffers.
    let mut iq_chunk: Vec<Complex> = Vec::with_capacity(CHUNK_FRAMES);
    let mut if_buf = vec![Complex::default(); CHUNK_FRAMES];
    let mut stereo_buf = vec![Stereo::default(); CHUNK_FRAMES];
    let mut line_buf = vec![WefaxLine::default(); READY_QUEUE_CAP];

    let mut free_rows: Vec<[u8; PIXELS_PER_LINE]> = Vec::new();
    let mut sync_rows: Vec<[u8; PIXELS_PER_LINE]> = Vec::new();
    let mut sync_reached_imaging = false;

    let mut mono_samples: u64 = 0;
    let mut sum_abs: f64 = 0.0;
    let mut peak_abs: f32 = 0.0;

    // Stream interleaved I,Q floats, pairing consecutive samples into
    // Complex. A truncated data chunk (short download) is tolerated: warn and
    // decode what was read.
    let mut samples = reader.samples::<f32>();
    let mut pending_i: Option<f32> = None;
    let mut done = false;
    while !done {
        iq_chunk.clear();
        while iq_chunk.len() < CHUNK_FRAMES {
            let Some(next) = samples.next() else {
                done = true;
                break;
            };
            let v = match next {
                Ok(s) => {
                    if s.is_finite() {
                        s
                    } else {
                        0.0
                    }
                }
                Err(e) => {
                    eprintln!("warning: WAV truncated: {e} — decoding what was read");
                    done = true;
                    break;
                }
            };
            match pending_i.take() {
                None => pending_i = Some(v),
                Some(i) => iq_chunk.push(Complex::new(i, v)),
            }
        }
        if iq_chunk.is_empty() {
            break;
        }

        // Downconvert to the 24 kHz IF.
        if if_buf.len() < iq_chunk.len() {
            if_buf.resize(iq_chunk.len(), Complex::default());
        }
        let if_n = resampler.process(&iq_chunk, &mut if_buf)?;
        if if_n == 0 {
            continue;
        }

        // Complex IF → mono fax audio via the real WEFAX demod.
        if stereo_buf.len() < if_n {
            stereo_buf.resize(if_n, Stereo::default());
        }
        let af_n = demod.process(&if_buf[..if_n], &mut stereo_buf[..if_n])?;

        // Collect mono `.l`, update level stats, and drive both decoders.
        let mono: Vec<f32> = stereo_buf[..af_n].iter().map(|s| s.l).collect();
        for &m in &mono {
            let a = m.abs();
            sum_abs += f64::from(a);
            if a > peak_abs {
                peak_abs = a;
            }
        }
        mono_samples += mono.len() as u64;

        let nf = free_dec.process(&mono, &mut line_buf)?;
        for line in line_buf.iter().take(nf) {
            free_rows.push(line.pixels);
        }
        let ns = sync_dec.process(&mono, &mut line_buf)?;
        for line in line_buf.iter().take(ns) {
            sync_rows.push(line.pixels);
        }
        if sync_dec.state() == WefaxState::Imaging {
            sync_reached_imaging = true;
        }
    }

    write_rows_png(free_png, &free_rows)?;
    write_rows_png(sync_png, &sync_rows)?;

    let mean_abs = if mono_samples == 0 {
        0.0
    } else {
        sum_abs / mono_samples as f64
    };

    Ok(Stats {
        source_rate,
        mono_samples,
        free_lines: free_rows.len(),
        sync_lines: sync_rows.len(),
        sync_reached_imaging,
        mean_abs,
        peak_abs,
    })
}

/// Write a greyscale PNG from assembled scanlines. A run that produced 0 lines
/// (legitimate for the sync decoder without a phasing lock) emits a 1×1 black
/// placeholder so downstream tooling always finds a file.
fn write_rows_png(
    path: &Path,
    rows: &[[u8; PIXELS_PER_LINE]],
) -> Result<(), Box<dyn std::error::Error>> {
    if rows.is_empty() {
        eprintln!(
            "note: 0 lines for {} — writing 1x1 placeholder",
            path.display()
        );
        write_grayscale_png(path, 1, 1, &[0u8])?;
        return Ok(());
    }
    let height = rows.len();
    let mut pixels = vec![0u8; height * PIXELS_PER_LINE];
    for (r, row) in rows.iter().enumerate() {
        pixels[r * PIXELS_PER_LINE..(r + 1) * PIXELS_PER_LINE].copy_from_slice(row);
    }
    write_grayscale_png(path, PIXELS_PER_LINE as u32, height as u32, &pixels)?;
    eprintln!(
        "wrote {} ({}x{} grayscale)",
        path.display(),
        PIXELS_PER_LINE,
        height
    );
    Ok(())
}

// --- Minimal grayscale PNG writer (copied from wefax_decode_wav.rs) ---

fn write_grayscale_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> std::io::Result<()> {
    assert_eq!(pixels.len(), (width * height) as usize);
    let mut f = BufWriter::new(File::create(path)?);

    f.write_all(b"\x89PNG\r\n\x1a\n")?;

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(0); // color type: grayscale
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    write_chunk(&mut f, b"IHDR", &ihdr)?;

    let mut raw = Vec::with_capacity((width * height + height) as usize);
    for row in 0..height {
        raw.push(0); // filter type 0 (none)
        let start = (row * width) as usize;
        raw.extend_from_slice(&pixels[start..start + width as usize]);
    }
    let compressed = zlib_encode(&raw);
    write_chunk(&mut f, b"IDAT", &compressed)?;

    write_chunk(&mut f, b"IEND", &[])?;
    f.flush()?;
    Ok(())
}

fn write_chunk<W: Write>(w: &mut W, kind: &[u8; 4], data: &[u8]) -> std::io::Result<()> {
    let len = data.len() as u32;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(kind)?;
    w.write_all(data)?;
    let mut crc = crc32_init();
    crc = crc32_update(crc, kind);
    crc = crc32_update(crc, data);
    let crc = crc32_finalize(crc);
    w.write_all(&crc.to_be_bytes())?;
    Ok(())
}

fn zlib_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x78); // CMF: deflate, 32K window
    out.push(0x01); // FLG: fastest, no dict, FCHECK adjusted

    const MAX_BLOCK: usize = 65_535;
    let mut offset = 0;
    while offset < data.len() {
        let remaining = data.len() - offset;
        let take = remaining.min(MAX_BLOCK);
        let is_final = offset + take == data.len();
        let header_byte: u8 = u8::from(is_final);
        out.push(header_byte);
        let len_u16 = take as u16;
        out.extend_from_slice(&len_u16.to_le_bytes());
        out.extend_from_slice(&(!len_u16).to_le_bytes());
        out.extend_from_slice(&data[offset..offset + take]);
        offset += take;
    }

    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let mut a = 1_u32;
    let mut b = 0_u32;
    for &byte in data {
        a = (a + u32::from(byte)) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0_u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

fn crc32_init() -> u32 {
    0xFFFF_FFFF
}

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

fn crc32_finalize(crc: u32) -> u32 {
    crc ^ 0xFFFF_FFFF
}
