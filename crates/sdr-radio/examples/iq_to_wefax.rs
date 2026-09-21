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
use std::path::{Path, PathBuf};

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
/// Ceiling on the resampler output buffer. The buffer grows as
/// `WEFAX_IF_RATE / source_rate` for below-24 kHz inputs, so an absurd
/// header rate (a 1 Hz WAV → ~6.3 billion samples ≈ 47 GiB) is rejected up
/// front rather than OOM'ing on allocation. 64 chunks (~16.8M samples,
/// ~128 MB) is far above any real SDR IQ rate's need.
const MAX_IF_BUF_SAMPLES: usize = 64 * CHUNK_FRAMES;
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

/// Resolve `path` to a form comparable for collision detection: the real
/// canonical path if it already exists, or (for a not-yet-created output)
/// the canonicalized parent directory joined with the file name. This lets
/// an output path that doesn't exist yet still compare equal to another
/// path that resolves to the same effective location (e.g. via `.`/`..` or
/// a symlinked parent directory).
fn canonical_or_parent(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Ok(existing) = std::fs::canonicalize(path) {
        return Ok(existing);
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("path has no file name: {}", path.display()))?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent_canon = match parent {
        Some(p) => std::fs::canonicalize(p)?,
        None => std::env::current_dir()?,
    };
    Ok(parent_canon.join(file_name))
}

/// Reject a run whose input/output paths collide: `File::create` truncates
/// its target, so an output aliasing the input IQ capture (or the two
/// outputs aliasing each other) would silently clobber data. Canonicalizing
/// and comparing for exact equality is enough here — this need not chase
/// every exotic hard-link case.
fn validate_distinct_paths(
    input: &Path,
    free_png: &Path,
    sync_png: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let input_r = canonical_or_parent(input)?;
    let free_r = canonical_or_parent(free_png)?;
    let sync_r = canonical_or_parent(sync_png)?;

    let pairs = [
        ("input", &input_r, "free_png", &free_r),
        ("input", &input_r, "sync_png", &sync_r),
        ("free_png", &free_r, "sync_png", &sync_r),
    ];
    for (name_a, path_a, name_b, path_b) in pairs {
        if path_a == path_b {
            return Err(format!(
                "refusing to run: {name_a} and {name_b} resolve to the same path ({}) \
                 — decoding would clobber it",
                path_a.display(),
            )
            .into());
        }
    }
    Ok(())
}

/// Read the WAV header and validate it's 2-channel 32-bit float (interleaved
/// I,Q). Returns the source sample rate.
fn read_and_validate_header(
    reader: &hound::WavReader<std::io::BufReader<File>>,
) -> Result<u32, Box<dyn std::error::Error>> {
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
    Ok(spec.sample_rate)
}

/// End-of-run accumulator folded by [`Pipeline::process_chunk`] over every
/// chunk in the stream.
#[derive(Default)]
struct DecodeAccum {
    free_rows: Vec<[u8; PIXELS_PER_LINE]>,
    sync_rows: Vec<[u8; PIXELS_PER_LINE]>,
    sync_reached_imaging: bool,
    mono_samples: u64,
    sum_abs: f64,
    peak_abs: f32,
}

/// The DSP chain (resampler + demod + both decoders) plus its reusable
/// scratch buffers. One instance is built per run and driven chunk by chunk.
struct Pipeline {
    resampler: RationalResampler,
    demod: WefaxDemodulator,
    free_dec: WefaxDecoder,
    sync_dec: WefaxDecoder,
    source_rate: u32,
    iq_chunk: Vec<Complex>,
    if_buf: Vec<Complex>,
    stereo_buf: Vec<Stereo>,
    mono: Vec<f32>,
    line_buf: Vec<WefaxLine>,
}

/// Output capacity the resampler can produce from `input_len` complex input
/// samples at `source_rate`, targeting [`WEFAX_IF_RATE`]. Ceil division plus
/// one: when `source_rate < WEFAX_IF_RATE` the resampler up-samples (output
/// larger than input), so sizing `if_buf` to the input length would overrun
/// with `DspError::BufferTooSmall`. For the intended 2.5/10 MHz captures this
/// down-samples and stays well under `CHUNK_FRAMES`, but the harness accepts
/// any rate.
fn if_capacity(input_len: usize, source_rate: u32) -> usize {
    let num = input_len as u64 * u64::from(WEFAX_IF_RATE);
    num.div_ceil(u64::from(source_rate)) as usize + 1
}

impl Pipeline {
    /// VFO offset 0: the fax subcarrier is already near baseband DC. The
    /// `RationalResampler` anti-alias filters internally (lowpass at the
    /// smaller of in/out Nyquist = 12 kHz) before decimating, so no separate
    /// LPF is needed to avoid aliasing into the 24 kHz IF.
    fn new(source_rate: u32) -> Result<Self, Box<dyn std::error::Error>> {
        let if_cap = if_capacity(CHUNK_FRAMES, source_rate);
        if if_cap > MAX_IF_BUF_SAMPLES {
            return Err(format!(
                "source rate {source_rate} Hz is too low for this harness: it would need a \
                 {if_cap}-sample resampler output buffer (cap {MAX_IF_BUF_SAMPLES})"
            )
            .into());
        }
        Ok(Self {
            resampler: RationalResampler::new(f64::from(source_rate), f64::from(WEFAX_IF_RATE))?,
            demod: WefaxDemodulator::new()?,
            free_dec: WefaxDecoder::new_free_running(WEFAX_IF_RATE),
            sync_dec: WefaxDecoder::new(WEFAX_IF_RATE)?,
            source_rate,
            iq_chunk: Vec::with_capacity(CHUNK_FRAMES),
            if_buf: vec![Complex::default(); if_cap],
            stereo_buf: vec![Stereo::default(); CHUNK_FRAMES],
            mono: Vec::with_capacity(CHUNK_FRAMES),
            line_buf: vec![WefaxLine::default(); READY_QUEUE_CAP],
        })
    }

    /// Pull up to `CHUNK_FRAMES` interleaved I,Q floats from `samples` into
    /// `self.iq_chunk`, pairing consecutive values into `Complex`. Returns
    /// `true` once the source is exhausted (clean EOF or a truncated tail),
    /// in which case the caller should stop after processing this chunk. A
    /// truncated data chunk (short download) is tolerated: warn and decode
    /// what was read.
    fn fill_iq_chunk(
        &mut self,
        samples: &mut hound::WavSamples<'_, std::io::BufReader<File>, f32>,
        pending_i: &mut Option<f32>,
    ) -> bool {
        self.iq_chunk.clear();
        while self.iq_chunk.len() < CHUNK_FRAMES {
            let Some(next) = samples.next() else {
                return true;
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
                    return true;
                }
            };
            match pending_i.take() {
                None => *pending_i = Some(v),
                Some(i) => self.iq_chunk.push(Complex::new(i, v)),
            }
        }
        false
    }

    /// Downconvert `self.iq_chunk` to WEFAX audio, drive both decoders, and
    /// fold the results into `accum`. A chunk that resamples to 0 output
    /// frames (resampler internal buffering) is a no-op, not an error.
    fn process_chunk(&mut self, accum: &mut DecodeAccum) -> Result<(), Box<dyn std::error::Error>> {
        let iq_len = self.iq_chunk.len();
        let need = if_capacity(iq_len, self.source_rate);
        if self.if_buf.len() < need {
            self.if_buf.resize(need, Complex::default());
        }
        let if_n = self.resampler.process(&self.iq_chunk, &mut self.if_buf)?;
        if if_n == 0 {
            return Ok(());
        }

        if self.stereo_buf.len() < if_n {
            self.stereo_buf.resize(if_n, Stereo::default());
        }
        let af_n = self
            .demod
            .process(&self.if_buf[..if_n], &mut self.stereo_buf[..if_n])?;

        // Reuse the mono scratch buffer instead of allocating one per chunk.
        self.mono.clear();
        self.mono
            .extend(self.stereo_buf[..af_n].iter().map(|s| s.l));
        for &m in &self.mono {
            let a = m.abs();
            accum.sum_abs += f64::from(a);
            if a > accum.peak_abs {
                accum.peak_abs = a;
            }
        }
        accum.mono_samples += self.mono.len() as u64;

        let nf = self.free_dec.process(&self.mono, &mut self.line_buf)?;
        for line in self.line_buf.iter().take(nf) {
            accum.free_rows.push(line.pixels);
        }
        let ns = self.sync_dec.process(&self.mono, &mut self.line_buf)?;
        for line in self.line_buf.iter().take(ns) {
            accum.sync_rows.push(line.pixels);
        }
        if self.sync_dec.state() == WefaxState::Imaging {
            accum.sync_reached_imaging = true;
        }
        Ok(())
    }
}

/// Write both PNGs and assemble the final [`Stats`].
fn finalize(
    source_rate: u32,
    accum: &DecodeAccum,
    free_png: &Path,
    sync_png: &Path,
) -> Result<Stats, Box<dyn std::error::Error>> {
    write_rows_png(free_png, &accum.free_rows)?;
    write_rows_png(sync_png, &accum.sync_rows)?;

    let mean_abs = if accum.mono_samples == 0 {
        0.0
    } else {
        accum.sum_abs / accum.mono_samples as f64
    };

    Ok(Stats {
        source_rate,
        mono_samples: accum.mono_samples,
        free_lines: accum.free_rows.len(),
        sync_lines: accum.sync_rows.len(),
        sync_reached_imaging: accum.sync_reached_imaging,
        mean_abs,
        peak_abs: accum.peak_abs,
    })
}

fn decode(
    input: &Path,
    free_png: &Path,
    sync_png: &Path,
) -> Result<Stats, Box<dyn std::error::Error>> {
    validate_distinct_paths(input, free_png, sync_png)?;

    let mut reader = hound::WavReader::open(input)?;
    let source_rate = read_and_validate_header(&reader)?;

    let mut pipeline = Pipeline::new(source_rate)?;
    let mut accum = DecodeAccum::default();

    // Stream interleaved I,Q floats, pairing consecutive samples into
    // Complex, one chunk at a time.
    let mut samples = reader.samples::<f32>();
    let mut pending_i: Option<f32> = None;
    loop {
        let done = pipeline.fill_iq_chunk(&mut samples, &mut pending_i);
        if pipeline.iq_chunk.is_empty() {
            break;
        }
        pipeline.process_chunk(&mut accum)?;
        if done {
            break;
        }
    }

    finalize(source_rate, &accum, free_png, sync_png)
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

/// PNG IHDR bit depth: 8 bits per grayscale sample.
const PNG_BIT_DEPTH_8: u8 = 8;
/// PNG IHDR color type: grayscale (no palette, no alpha channel).
const PNG_COLOR_TYPE_GRAYSCALE: u8 = 0;
/// PNG IHDR compression method: 0 (deflate) is the only value the spec defines.
const PNG_COMPRESSION_METHOD_DEFLATE: u8 = 0;
/// PNG IHDR filter method: 0 is the only value the spec defines.
const PNG_FILTER_METHOD_ADAPTIVE: u8 = 0;
/// PNG IHDR interlace method: 0 = no interlacing (Adam7 off).
const PNG_INTERLACE_METHOD_NONE: u8 = 0;
/// Per-scanline filter-type byte prefixed to each row: 0 = "None" predictor.
const PNG_ROW_FILTER_NONE: u8 = 0;

/// zlib (RFC 1950) CMF byte: compression method 8 (deflate), compression
/// info 7 (32K window).
const ZLIB_CMF_DEFLATE_32K_WINDOW: u8 = 0x78;
/// zlib (RFC 1950) FLG byte: fastest compression level, no preset
/// dictionary, FCHECK adjusted so `(CMF * 256 + FLG) % 31 == 0`.
const ZLIB_FLG_FASTEST_NO_DICT: u8 = 0x01;

/// Reversed CRC-32 polynomial (0xEDB88320) used by PNG chunk CRCs (ISO 3309 /
/// ITU-T V.42 / "PKZIP" polynomial, bit-reversed).
const CRC32_POLYNOMIAL: u32 = 0xEDB8_8320;
/// CRC-32 register value used both to seed the running CRC and to XOR the
/// final result (the standard init/finalize convention for this algorithm).
const CRC32_INIT_XOROUT: u32 = 0xFFFF_FFFF;
/// Mask to extract the low byte of the CRC register for the table lookup.
const CRC32_TABLE_INDEX_MASK: u32 = 0xFF;

fn write_grayscale_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> std::io::Result<()> {
    assert_eq!(pixels.len(), (width * height) as usize);
    let mut f = BufWriter::new(File::create(path)?);

    f.write_all(b"\x89PNG\r\n\x1a\n")?;

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(PNG_BIT_DEPTH_8);
    ihdr.push(PNG_COLOR_TYPE_GRAYSCALE);
    ihdr.push(PNG_COMPRESSION_METHOD_DEFLATE);
    ihdr.push(PNG_FILTER_METHOD_ADAPTIVE);
    ihdr.push(PNG_INTERLACE_METHOD_NONE);
    write_chunk(&mut f, b"IHDR", &ihdr)?;

    let mut raw = Vec::with_capacity((width * height + height) as usize);
    for row in 0..height {
        raw.push(PNG_ROW_FILTER_NONE);
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
    out.push(ZLIB_CMF_DEFLATE_32K_WINDOW);
    out.push(ZLIB_FLG_FASTEST_NO_DICT);

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
                CRC32_POLYNOMIAL ^ (c >> 1)
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
    CRC32_INIT_XOROUT
}

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC_TABLE[((crc ^ u32::from(b)) & CRC32_TABLE_INDEX_MASK) as usize] ^ (crc >> 8);
    }
    crc
}

fn crc32_finalize(crc: u32) -> u32 {
    crc ^ CRC32_INIT_XOROUT
}
