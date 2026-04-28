//! Standalone APT decoder CLI — decode a mono PCM WAV through our
//! `AptDecoder` and write the result as an 8-bit grayscale PNG.
//!
//! Used for offline parity comparison against `noaa-apt` and for
//! debugging by replaying real captures without the GTK app.
//!
//! Usage:
//!
//! ```text
//! cargo run --release -p sdr-dsp --example apt_decode_wav -- <input.wav> <output.png>
//! ```

// CLI tool — relax the workspace-pedantic casts. Every numeric `as`
// cast in here is over a domain we control (sample counts, pixel
// values, RGB byte components, PNG header fields), so the loud
// pedantic warnings would just litter the file. Library code stays
// strict.
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

use sdr_dsp::apt::{AptDecoder, AptLine, LINE_PIXELS, READY_QUEUE_CAP};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!(
            "usage: {} <input.wav> <output.png>\n\
             input.wav must be PCM 16-bit, mono, sample rate >= 11025 Hz",
            args[0]
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

    // Read all samples, normalize to f32 in [-1, 1], average channels
    // to mono if needed. Signed 16-bit PCM normalizes by 2^15 = 32768
    // (not by `i16::MAX` = 32767) so `i16::MIN` maps to exactly -1.0
    // — keeps amplitude symmetric and matches the integration test
    // path. Per CR round 2 on PR #571.
    const PCM16_SCALE: f32 = 32_768.0;
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| f32::from(s.unwrap()) / PCM16_SCALE)
            .collect(),
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
    };
    let samples: Vec<f32> = if spec.channels == 1 {
        raw
    } else {
        let n = spec.channels as usize;
        raw.chunks_exact(n)
            .map(|frame| frame.iter().sum::<f32>() / n as f32)
            .collect()
    };
    eprintln!(
        "loaded {} samples ({:.1} s)",
        samples.len(),
        samples.len() as f64 / f64::from(spec.sample_rate)
    );

    let mut decoder = AptDecoder::new(spec.sample_rate)
        .unwrap_or_else(|e| panic!("AptDecoder::new({}): {e}", spec.sample_rate));
    let mut buf = vec![AptLine::default(); READY_QUEUE_CAP];
    let mut lines: Vec<AptLine> = Vec::new();

    // Stream in 1 K chunks so the streaming path is exercised.
    for chunk in samples.chunks(1_024) {
        let n = decoder.process(chunk, &mut buf).expect("APT process");
        for slot in buf.iter_mut().take(n) {
            lines.push(std::mem::take(slot));
        }
    }
    // Drain remaining buffered lines. One `process(&[], ...)` call
    // can return up to `buf.len()` lines and the decoder may hold
    // more in its ready queue; loop until empty so we don't silently
    // truncate the line count. Per CR round 2 on PR #571.
    loop {
        let n = decoder.process(&[], &mut buf).expect("APT flush");
        if n == 0 {
            break;
        }
        for slot in buf.iter_mut().take(n) {
            lines.push(std::mem::take(slot));
        }
    }

    let qualities: Vec<f32> = lines.iter().map(|l| l.sync_quality).collect();
    let mut sorted = qualities.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
    let above = qualities.iter().filter(|&&q| q > 0.85).count();
    let frac = above as f64 / qualities.len().max(1) as f64;
    eprintln!(
        "decoded {} lines, median sync_quality={:.3}, {:.1}% at >0.85 lock",
        lines.len(),
        median,
        frac * 100.0
    );

    // Render to a flat row-major u8 grayscale buffer with image-wide
    // percentile normalization (the same finalize_grayscale path the
    // GUI uses for PNG export). We do percentile inline here rather
    // than depending on apt_image (which lives in sdr-radio) so this
    // example stays inside the sdr-dsp crate.
    let height = lines.len();
    // Two-buffer split: `render_samples` carries every line including
    // gap-filled zeros (so the final pixel write has the right shape);
    // `valid_samples` carries only above-threshold lines (so the
    // percentile reference range comes from the actual signal, not
    // the zero-fill). Per CR round 1 on PR #571.
    let mut render_samples: Vec<f32> = Vec::with_capacity(height * LINE_PIXELS);
    let mut valid_samples: Vec<f32> = Vec::new();
    for line in &lines {
        if line.sync_quality < 0.5 {
            render_samples.extend(std::iter::repeat_n(0.0_f32, LINE_PIXELS));
            continue;
        }
        render_samples.extend_from_slice(&line.raw_samples);
        valid_samples.extend_from_slice(&line.raw_samples);
    }
    if valid_samples.is_empty() {
        eprintln!(
            "decoded {} lines but none above the sync-quality threshold — \
             nothing to render",
            lines.len(),
        );
        std::process::exit(1);
    }

    let mut sorted_samples = valid_samples;
    sorted_samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted_samples.len();
    let p = 0.98_f32;
    let tail = ((1.0 - p) / 2.0 * n as f32) as usize;
    let lo = sorted_samples[tail.min(n.saturating_sub(1))];
    let hi = sorted_samples[(n - 1).saturating_sub(tail)];
    let range = (hi - lo).max(1e-9);
    eprintln!("brightness range: lo={lo:.3} hi={hi:.3}");

    let pixels: Vec<u8> = render_samples
        .iter()
        .map(|&v| {
            let norm = ((v - lo) / range).clamp(0.0, 1.0);
            (norm * 255.0).round() as u8
        })
        .collect();

    write_grayscale_png(output, LINE_PIXELS as u32, height as u32, &pixels)
        .unwrap_or_else(|e| panic!("write {}: {e}", output.display()));
    eprintln!(
        "wrote {} ({}×{} grayscale)",
        output.display(),
        LINE_PIXELS,
        height
    );
}

/// Minimal hand-rolled PNG writer — header + IHDR + IDAT + IEND.
/// Avoids pulling in the `image` crate as a dev-dep just for this
/// example. PNG is straightforward when you already have raw u8
/// pixels; the only non-trivial bit is the per-row filter byte
/// (we use filter type 0 = none) and the deflate-wrapped IDAT.
fn write_grayscale_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
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

/// Minimal zlib wrapper around DEFLATE-stored blocks. Stored blocks
/// are uncompressed but zlib-framed — gives us a valid IDAT without
/// pulling in a real DEFLATE compressor. The result file is larger
/// than a real zlib stream, but it parses cleanly and that's what
/// matters for offline comparison.
fn zlib_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // zlib header: deflate, 32K window, no dict, fastest level (these
    // bits aren't actually used by parsers, just need a valid value)
    out.push(0x78); // CMF: deflate, 32K window
    out.push(0x01); // FLG: fastest, no dict, FCHECK adjusted
    // FCHECK adjustment: (CMF*256 + FLG) must be divisible by 31.
    // 0x78 << 8 = 0x7800 = 30720. 30720 % 31 = 30720 - 31*991 = 30720 - 30721 = -1, so 30720 mod 31 = 30. So we need FLG = 31 - 30 = 1 to make total ≡ 0 mod 31. 0x01 was already written; verify: (0x7800 + 0x01) = 30721 = 31 * 991 ✓.

    // Write data as DEFLATE stored blocks. Each block: BFINAL bit +
    // BTYPE bits (00 = stored), then byte-aligned, then LEN (le16),
    // ~LEN (le16), then LEN bytes of data. Max LEN per block = 65535.
    const MAX_BLOCK: usize = 65_535;
    let mut offset = 0;
    while offset < data.len() {
        let remaining = data.len() - offset;
        let take = remaining.min(MAX_BLOCK);
        let is_final = offset + take == data.len();
        // BFINAL (1 bit) + BTYPE 00 (2 bits) = 3 bits, packed in low byte
        let header_byte: u8 = if is_final { 0x01 } else { 0x00 };
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
