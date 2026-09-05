//! Offline WEFAX render harness (real-data gate): decode a WAV of USB fax
//! audio into a greyscale PNG. Stereo is downmixed to mono.
//!
//! The decoder is free-running (no start-tone / phasing sync yet — that
//! lands in a later task), so the rendered chart may be sheared or
//! horizontally offset. This harness exists to prove the discriminator +
//! line assembly produce recognizable chart structure from real audio.
//!
//! Usage:
//!
//! ```text
//! cargo run -p sdr-dsp --example wefax_decode_wav -- <input.wav> <output.png>
//! ```

// CLI tool — relax the workspace-pedantic casts, matching
// `apt_decode_wav.rs`. Library code (`sdr_dsp::wefax`) stays strict.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::trivially_copy_pass_by_ref,
    clippy::items_after_statements,
    clippy::bool_to_int_with_if,
    clippy::too_many_lines
)]

use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use sdr_dsp::wefax::{PIXELS_PER_LINE, WefaxDecoder, WefaxLine};

/// Lines are drained from `process` in batches this large.
const READY_QUEUE_CAP: usize = 64;
/// Input is streamed through the decoder in chunks this large so the
/// streaming path (not a single giant call) is exercised.
const CHUNK_SAMPLES: usize = 8_192;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!(
            "usage: {} <input.wav> <output.png>\n\
             input.wav: PCM 16-bit OR float WAV; multi-channel input is \
             downmixed to mono",
            args[0],
        );
        std::process::exit(2);
    }
    let input = Path::new(&args[1]);
    let output = Path::new(&args[2]);

    let mut reader =
        hound::WavReader::open(input).unwrap_or_else(|e| panic!("open {}: {e}", input.display()));
    let spec = reader.spec();
    eprintln!(
        "input: {} ch, {} Hz, {} bits/sample, {:?}",
        spec.channels, spec.sample_rate, spec.bits_per_sample, spec.sample_format,
    );
    if spec.channels != 1 {
        eprintln!(
            "note: input is {} channel; averaging to mono",
            spec.channels
        );
    }

    // Read all samples, normalize to f32 in [-1, 1]. Signed 16-bit PCM
    // normalizes by 2^15 (not `i16::MAX`) so `i16::MIN` maps to exactly
    // -1.0, matching the convention used throughout the sdr-dsp examples.
    const PCM16_SCALE: f32 = 32_768.0;
    // Some real-world captures declare more data in the RIFF header than
    // is actually present (a truncated download, in the fixture this
    // harness is exercised against). Rather than aborting the whole
    // render over a short tail, warn and decode whatever samples were
    // actually readable.
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let mut out = Vec::with_capacity(reader.duration() as usize);
            for sample in reader.samples::<i16>() {
                match sample {
                    Ok(s) => out.push(f32::from(s) / PCM16_SCALE),
                    Err(e) => {
                        eprintln!(
                            "warning: WAV data chunk truncated after sample {}: {e} — \
                             decoding what was read",
                            out.len()
                        );
                        break;
                    }
                }
            }
            out
        }
        hound::SampleFormat::Float => {
            let mut out = Vec::with_capacity(reader.duration() as usize);
            for sample in reader.samples::<f32>() {
                match sample {
                    Ok(s) => out.push(if s.is_finite() { s } else { 0.0 }),
                    Err(e) => {
                        eprintln!(
                            "warning: WAV data chunk truncated after sample {}: {e} — \
                             decoding what was read",
                            out.len()
                        );
                        break;
                    }
                }
            }
            out
        }
    };
    let mono: Vec<f32> = if spec.channels == 1 {
        raw
    } else {
        let n = spec.channels as usize;
        raw.chunks_exact(n)
            .map(|frame| frame.iter().sum::<f32>() / n as f32)
            .collect()
    };
    eprintln!(
        "loaded {} samples ({:.1} s)",
        mono.len(),
        mono.len() as f64 / f64::from(spec.sample_rate)
    );

    let mut decoder = WefaxDecoder::new(spec.sample_rate)
        .unwrap_or_else(|e| panic!("WefaxDecoder::new({}): {e}", spec.sample_rate));
    let mut buf = vec![WefaxLine::default(); READY_QUEUE_CAP];
    let mut rows: Vec<[u8; PIXELS_PER_LINE]> = Vec::new();

    for chunk in mono.chunks(CHUNK_SAMPLES) {
        let n = decoder.process(chunk, &mut buf).expect("WEFAX process");
        for line in buf.iter().take(n) {
            rows.push(line.pixels);
        }
    }

    if rows.is_empty() {
        eprintln!("decoded 0 lines — input too short for even one scanline");
        std::process::exit(1);
    }

    let height = rows.len();
    let mut pixels = vec![0u8; height * PIXELS_PER_LINE];
    for (r, row) in rows.iter().enumerate() {
        pixels[r * PIXELS_PER_LINE..(r + 1) * PIXELS_PER_LINE].copy_from_slice(row);
    }

    write_grayscale_png(output, PIXELS_PER_LINE as u32, height as u32, &pixels)
        .unwrap_or_else(|e| panic!("write {}: {e}", output.display()));
    eprintln!(
        "wrote {} ({}×{} grayscale)",
        output.display(),
        PIXELS_PER_LINE,
        height
    );
}

/// Minimal hand-rolled PNG writer — header + IHDR + IDAT + IEND. Mirrors
/// `apt_decode_wav.rs`'s writer so this example doesn't need to pull in the
/// `image` crate as a new dev-dependency just to emit a grayscale PNG.
fn write_grayscale_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> std::io::Result<()> {
    assert_eq!(pixels.len(), (width * height) as usize);
    let mut f = BufWriter::new(File::create(path)?);

    // PNG signature
    f.write_all(b"\x89PNG\r\n\x1a\n")?;

    // IHDR chunk
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(0); // color type: grayscale
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    write_chunk(&mut f, b"IHDR", &ihdr)?;

    // IDAT — prepend filter byte 0 to each row, then zlib-compress.
    let mut raw = Vec::with_capacity((width * height + height) as usize);
    for row in 0..height {
        raw.push(0); // filter type 0 (none)
        let start = (row * width) as usize;
        raw.extend_from_slice(&pixels[start..start + width as usize]);
    }
    let compressed = zlib_encode(&raw);
    write_chunk(&mut f, b"IDAT", &compressed)?;

    // IEND
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

/// Minimal zlib wrapper around DEFLATE-stored blocks. Stored blocks are
/// uncompressed but zlib-framed — gives us a valid IDAT without pulling in
/// a real DEFLATE compressor. The result file is larger than a real zlib
/// stream, but it parses cleanly and that's what matters for offline
/// comparison.
fn zlib_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // zlib header: deflate, 32K window, no dict, fastest level. FCHECK
    // adjustment: (CMF*256 + FLG) must be divisible by 31 — 0x7800 + 0x01 =
    // 30721 = 31 * 991.
    out.push(0x78); // CMF: deflate, 32K window
    out.push(0x01); // FLG: fastest, no dict, FCHECK adjusted

    // Write data as DEFLATE stored blocks. Each block: BFINAL bit + BTYPE
    // bits (00 = stored), then byte-aligned, then LEN (le16), ~LEN (le16),
    // then LEN bytes of data. Max LEN per block = 65535.
    const MAX_BLOCK: usize = 65_535;
    let mut offset = 0;
    while offset < data.len() {
        let remaining = data.len() - offset;
        let take = remaining.min(MAX_BLOCK);
        let is_final = offset + take == data.len();
        // BFINAL (1 bit) + BTYPE 00 (2 bits) = 3 bits, packed in low byte
        let header_byte: u8 = u8::from(is_final);
        out.push(header_byte);
        let len_u16 = take as u16;
        out.extend_from_slice(&len_u16.to_le_bytes());
        out.extend_from_slice(&(!len_u16).to_le_bytes());
        out.extend_from_slice(&data[offset..offset + take]);
        offset += take;
    }

    // Adler-32 checksum of the uncompressed data.
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
