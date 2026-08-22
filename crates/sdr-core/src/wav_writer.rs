//! WAV file writer for recording audio and IQ data.
//!
//! Writes IEEE float 32-bit WAV files. The header is written on creation
//! with placeholder sizes, then patched on [`Drop`] to finalize the file.

use std::fs::File;
use std::io::{BufWriter, ErrorKind, Seek, SeekFrom, Write};
use std::path::Path;

use sdr_types::{Complex, Stereo};

/// WAV format tag for IEEE 32-bit floating-point samples.
const WAV_FORMAT_IEEE_FLOAT: u16 = 3;

/// WAV header size in bytes (RIFF + fmt + data chunk headers).
const WAV_HEADER_SIZE: u32 = 44;

/// Bits per sample for f32 audio.
const BITS_PER_SAMPLE: u16 = 32;

/// Bytes per f32 sample.
const BYTES_PER_SAMPLE: u32 = 4;

/// Bytes per interleaved two-channel f32 frame — one (L, R) stereo pair
/// or one (I, Q) pair; both recording kinds share this layout.
const FRAME_BYTES: u32 = 2 * BYTES_PER_SAMPLE;

/// Offset of the RIFF file-size field (byte 4).
const RIFF_SIZE_OFFSET: u64 = 4;

/// Offset of the data chunk size field (byte 40).
const DATA_SIZE_OFFSET: u64 = 40;

/// Size of the RIFF header prefix that is excluded from the file-size field.
const RIFF_HEADER_PREFIX: u32 = 8;

/// Largest data chunk a classic RIFF/WAV file can describe: both the
/// `data` size and the RIFF size (`data + 36`) are `u32` fields, so the
/// data chunk is capped just under 4 GiB. IQ at 2.4 Msps reaches this in
/// ~224 s; the writer refuses writes past it instead of wrapping the
/// header (#694). Rounded down to a whole number of frames so the cap
/// is reached exactly rather than with an unusable tail.
pub const MAX_WAV_DATA_BYTES: u64 =
    (u32::MAX as u64 - (WAV_HEADER_SIZE - RIFF_HEADER_PREFIX) as u64) / FRAME_BYTES as u64
        * FRAME_BYTES as u64;

/// Writes demodulated stereo audio or raw IQ samples to a WAV file.
///
/// - Audio recording: 2-channel (L, R) at 48 kHz.
/// - IQ recording: 2-channel (I, Q) at the source sample rate.
///
/// The file is finalized automatically when dropped.
///
/// Generic over the sink so tests can inject write failures; production
/// code only ever uses the default `File` sink via [`WavWriter::new`].
pub struct WavWriter<W: Write + Seek = File> {
    writer: BufWriter<W>,
    /// Bytes of sample data the sink has *accepted* so far (`u64` so the
    /// accounting can never wrap; the WAV cap is enforced by
    /// [`Self::check_capacity`]). Advanced per accepted chunk, so a write
    /// that fails part-way still leaves the header describing exactly the
    /// payload on disk (#694).
    bytes_written: u64,
    /// Data-chunk byte cap — [`MAX_WAV_DATA_BYTES`] outside tests.
    max_data_bytes: u64,
    finalized: bool,
}

impl WavWriter<File> {
    /// Create a new WAV writer at `path`.
    ///
    /// Writes the 44-byte WAV header with placeholder sizes. The sizes are
    /// patched in [`Drop`] once all samples have been written.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the file cannot be created or the header write fails.
    pub fn new(path: &Path, sample_rate: u32, channels: u16) -> std::io::Result<Self> {
        Self::with_sink(BufWriter::new(File::create(path)?), sample_rate, channels)
    }
}

impl<W: Write + Seek> WavWriter<W> {
    /// Create a writer over an arbitrary buffered sink (tests inject a
    /// failing sink here; production goes through [`WavWriter::new`]).
    fn with_sink(
        mut writer: BufWriter<W>,
        sample_rate: u32,
        channels: u16,
    ) -> std::io::Result<Self> {
        // RIFF header
        writer.write_all(b"RIFF")?;
        writer.write_all(&0u32.to_le_bytes())?; // placeholder: file size - 8
        writer.write_all(b"WAVE")?;

        // fmt sub-chunk (16 bytes for PCM/float)
        writer.write_all(b"fmt ")?;
        writer.write_all(&16u32.to_le_bytes())?; // sub-chunk size
        writer.write_all(&WAV_FORMAT_IEEE_FLOAT.to_le_bytes())?;
        writer.write_all(&channels.to_le_bytes())?;
        writer.write_all(&sample_rate.to_le_bytes())?;
        let byte_rate = sample_rate * u32::from(channels) * BYTES_PER_SAMPLE;
        writer.write_all(&byte_rate.to_le_bytes())?;
        let block_align = channels * (BITS_PER_SAMPLE / 8);
        writer.write_all(&block_align.to_le_bytes())?;
        writer.write_all(&BITS_PER_SAMPLE.to_le_bytes())?;

        // data sub-chunk header
        writer.write_all(b"data")?;
        writer.write_all(&0u32.to_le_bytes())?; // placeholder: data size

        Ok(Self {
            writer,
            bytes_written: 0,
            max_data_bytes: MAX_WAV_DATA_BYTES,
            finalized: false,
        })
    }

    /// Bytes of sample data written so far.
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// `true` once fewer than one complete frame fits under the WAV size
    /// cap; every further non-empty write returns
    /// [`std::io::ErrorKind::StorageFull`].
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.max_data_bytes.saturating_sub(self.bytes_written) < u64::from(FRAME_BYTES)
    }

    /// Check that `bytes` more of sample data fit under the cap without
    /// touching the accounting; the whole write is refused if not, so the
    /// file never holds a partial frame. `bytes_written` is advanced only
    /// after the writer has accepted the complete buffer, so the header
    /// written by `finalize` never describes bytes that are not on disk.
    fn check_capacity(&self, bytes: u64) -> std::io::Result<()> {
        if self.bytes_written.saturating_add(bytes) > self.max_data_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "WAV data chunk limit reached (4 GiB); start a new recording",
            ));
        }
        Ok(())
    }

    /// Write `bytes` to the sink, advancing `bytes_written` by every chunk
    /// the sink accepts. Unlike `write_all`, a failure part-way leaves the
    /// accounting equal to what was actually accepted, so `finalize` never
    /// advertises less data than the file holds (#694).
    fn write_accounted(&mut self, mut bytes: &[u8]) -> std::io::Result<()> {
        while !bytes.is_empty() {
            match self.writer.write(bytes) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        ErrorKind::WriteZero,
                        "WAV sink accepted no bytes",
                    ));
                }
                Ok(n) => {
                    self.bytes_written += n as u64;
                    bytes = &bytes[n..];
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Write a slice of stereo audio samples (L, R interleaved as f32 pairs).
    ///
    /// On little-endian targets, uses `bytemuck::cast_slice` for a single bulk
    /// write. On big-endian targets, falls back to per-sample `to_le_bytes()`
    /// serialization to satisfy WAV's little-endian format requirement.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the write fails (the accounting then covers
    /// exactly the bytes the sink accepted), or
    /// [`std::io::ErrorKind::StorageFull`] (with nothing written) if the
    /// slice would push the data chunk past [`MAX_WAV_DATA_BYTES`].
    pub fn write_stereo(&mut self, samples: &[Stereo]) -> std::io::Result<()> {
        self.check_capacity(samples.len() as u64 * u64::from(FRAME_BYTES))?;
        #[cfg(target_endian = "little")]
        {
            self.write_accounted(bytemuck::cast_slice(samples))
        }
        #[cfg(not(target_endian = "little"))]
        {
            for s in samples {
                self.write_accounted(&s.l.to_le_bytes())?;
                self.write_accounted(&s.r.to_le_bytes())?;
            }
            Ok(())
        }
    }

    /// Write a slice of IQ samples (I, Q interleaved as f32 pairs).
    ///
    /// On little-endian targets, uses `bytemuck::cast_slice` for a single bulk
    /// write. On big-endian targets, falls back to per-sample `to_le_bytes()`
    /// serialization to satisfy WAV's little-endian format requirement.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the write fails (the accounting then covers
    /// exactly the bytes the sink accepted), or
    /// [`std::io::ErrorKind::StorageFull`] (with nothing written) if the
    /// slice would push the data chunk past [`MAX_WAV_DATA_BYTES`].
    pub fn write_iq(&mut self, samples: &[Complex]) -> std::io::Result<()> {
        self.check_capacity(samples.len() as u64 * u64::from(FRAME_BYTES))?;
        #[cfg(target_endian = "little")]
        {
            self.write_accounted(bytemuck::cast_slice(samples))
        }
        #[cfg(not(target_endian = "little"))]
        {
            for s in samples {
                self.write_accounted(&s.re.to_le_bytes())?;
                self.write_accounted(&s.im.to_le_bytes())?;
            }
            Ok(())
        }
    }

    /// Finalize the WAV file by patching the RIFF and data chunk sizes.
    ///
    /// Called automatically on [`Drop`], but can be called explicitly for
    /// error handling. Subsequent calls are no-ops.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the seek or write fails.
    pub fn finalize(&mut self) -> std::io::Result<()> {
        if self.finalized {
            return Ok(());
        }
        self.finalized = true;

        self.writer.flush()?;

        // `check_capacity` guarantees `bytes_written <= MAX_WAV_DATA_BYTES`, so
        // both header fields fit; the conversion is checked anyway rather
        // than trusted (#694). A write that failed mid-frame may have left a
        // partial frame on disk — the header only advertises whole frames.
        let whole_frames = self.bytes_written / u64::from(FRAME_BYTES) * u64::from(FRAME_BYTES);
        let data_size = u32::try_from(whole_frames).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "WAV data chunk exceeds the u32 header field",
            )
        })?;
        let file_size = data_size + WAV_HEADER_SIZE - RIFF_HEADER_PREFIX;

        // Patch RIFF file size (offset 4)
        self.writer.seek(SeekFrom::Start(RIFF_SIZE_OFFSET))?;
        self.writer.write_all(&file_size.to_le_bytes())?;

        // Patch data chunk size (offset 40)
        self.writer.seek(SeekFrom::Start(DATA_SIZE_OFFSET))?;
        self.writer.write_all(&data_size.to_le_bytes())?;

        self.writer.flush()?;
        Ok(())
    }
}

impl<W: Write + Seek> Drop for WavWriter<W> {
    fn drop(&mut self) {
        if let Err(e) = self.finalize() {
            tracing::warn!("WAV finalize failed: {e}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Compile-time validation of WAV constants.
    const _: () = {
        assert!(WAV_HEADER_SIZE == 44);
        assert!(BITS_PER_SAMPLE == 32);
        assert!(BYTES_PER_SAMPLE == 4);
        assert!(RIFF_HEADER_PREFIX == 8);
    };

    #[test]
    fn write_and_finalize_stereo_wav() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_stereo.wav");

        let sample_rate = 48_000;
        let channels = 2;
        let samples = vec![
            Stereo { l: 0.5, r: -0.5 },
            Stereo { l: 0.25, r: -0.25 },
            Stereo { l: 0.0, r: 0.0 },
        ];

        {
            let mut writer = WavWriter::new(&path, sample_rate, channels).unwrap();
            writer.write_stereo(&samples).unwrap();
            // Drop finalizes
        }

        // Read back and verify header
        let data = std::fs::read(&path).unwrap();
        assert!(data.len() >= 44, "WAV file too short");

        // RIFF header
        assert_eq!(&data[0..4], b"RIFF");
        assert_eq!(&data[8..12], b"WAVE");

        // fmt chunk
        assert_eq!(&data[12..16], b"fmt ");
        let format = u16::from_le_bytes([data[20], data[21]]);
        assert_eq!(format, WAV_FORMAT_IEEE_FLOAT);
        let ch = u16::from_le_bytes([data[22], data[23]]);
        assert_eq!(ch, channels);
        let sr = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
        assert_eq!(sr, sample_rate);

        // data chunk
        assert_eq!(&data[36..40], b"data");
        let data_size = u32::from_le_bytes([data[40], data[41], data[42], data[43]]);
        // 3 samples * 2 channels * 4 bytes = 24
        assert_eq!(data_size, 24);

        // RIFF size = data_size + 36
        let riff_size = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        assert_eq!(riff_size, data_size + 36);

        // Verify sample data
        let first_l = f32::from_le_bytes([data[44], data[45], data[46], data[47]]);
        assert!((first_l - 0.5).abs() < f32::EPSILON);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn write_and_finalize_iq_wav() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_iq.wav");

        let sample_rate = 2_400_000;
        let channels = 2;
        let samples = vec![Complex::new(1.0, 0.0), Complex::new(0.0, 1.0)];

        {
            let mut writer = WavWriter::new(&path, sample_rate, channels).unwrap();
            writer.write_iq(&samples).unwrap();
        }

        let data = std::fs::read(&path).unwrap();
        let data_size = u32::from_le_bytes([data[40], data[41], data[42], data[43]]);
        // 2 samples * 2 channels * 4 bytes = 16
        assert_eq!(data_size, 16);

        std::fs::remove_file(&path).unwrap();
    }

    /// #694 — a WAV data chunk is a `u32` byte count. Past the cap the
    /// header would wrap and a multi-GB file would open as a short clip
    /// (or panic in debug inside `Drop`), so the writer must refuse the
    /// write that would cross it and keep the file consistent.
    #[test]
    fn write_refuses_past_data_cap() {
        // Two stereo frames (16 bytes) plus 4 bytes that cannot hold a
        // complete frame — `is_full` must report full at that point.
        const TEST_CAP_BYTES: u64 = 20;
        const EXPECTED_DATA_BYTES: u64 = 16;
        let dir = std::env::temp_dir();
        let path = dir.join("test_data_cap.wav");

        let two = vec![Stereo { l: 0.5, r: -0.5 }; 2];
        let one = vec![Stereo { l: 0.1, r: 0.1 }];
        {
            let mut writer = WavWriter::new(&path, 48_000, 2).unwrap();
            writer.max_data_bytes = TEST_CAP_BYTES;
            writer.write_stereo(&two).unwrap();
            assert!(writer.is_full(), "less than one frame left counts as full");
            let err = writer.write_stereo(&one).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);
            assert_eq!(writer.bytes_written(), EXPECTED_DATA_BYTES);
        }

        let data = std::fs::read(&path).unwrap();
        let data_size = u32::from_le_bytes([data[40], data[41], data[42], data[43]]);
        assert_eq!(u64::from(data_size), EXPECTED_DATA_BYTES);
        assert_eq!(
            data.len() as u64,
            u64::from(WAV_HEADER_SIZE) + EXPECTED_DATA_BYTES
        );

        std::fs::remove_file(&path).unwrap();
    }

    /// Sink that accepts up to `accept_limit` appended bytes in total,
    /// then fails every further append — models a disk that fills
    /// mid-buffer. Overwrites inside the existing content (the header
    /// patches `finalize` performs) need no space and always succeed.
    struct FailingSink {
        buf: std::io::Cursor<Vec<u8>>,
        accept_limit: usize,
        accepted: usize,
    }

    impl FailingSink {
        fn new(accept_limit: usize) -> Self {
            Self {
                buf: std::io::Cursor::new(Vec::new()),
                accept_limit,
                accepted: 0,
            }
        }
    }

    impl Write for FailingSink {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let pos = usize::try_from(self.buf.position()).unwrap();
            let len = self.buf.get_ref().len();
            if pos < len {
                // Overwrite of existing bytes (header patch): no budget.
                let n = data.len().min(len - pos);
                self.buf.write_all(&data[..n])?;
                return Ok(n);
            }
            let room = self.accept_limit.saturating_sub(self.accepted);
            if room == 0 {
                return Err(std::io::Error::other("injected disk failure"));
            }
            let n = data.len().min(room);
            self.buf.write_all(&data[..n])?;
            self.accepted += n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.buf.flush()
        }
    }

    impl Seek for FailingSink {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.buf.seek(pos)
        }
    }

    /// Header-only sink budget: the 44-byte header must always land.
    const HEADER_ONLY: usize = WAV_HEADER_SIZE as usize;
    /// Tiny `BufWriter` so sample writes reach the sink immediately.
    const UNBUFFERED: usize = 1;

    fn read_data_size(writer: &WavWriter<FailingSink>) -> u32 {
        let bytes = writer.writer.get_ref().buf.get_ref();
        u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]])
    }

    /// #694 (CR round 3) — a sink that fails after accepting a prefix of
    /// the buffer must leave the accounting (and so the finalized header)
    /// equal to what it accepted, rounded down to whole frames.
    #[test]
    fn partial_stereo_write_is_accounted_before_finalize() {
        const ACCEPT_FRAMES: u64 = 2;
        let limit =
            HEADER_ONLY + usize::try_from(ACCEPT_FRAMES * u64::from(FRAME_BYTES)).unwrap() + 3;
        let mut writer = WavWriter::with_sink(
            BufWriter::with_capacity(UNBUFFERED, FailingSink::new(limit)),
            48_000,
            2,
        )
        .unwrap();
        let five = vec![Stereo { l: 0.5, r: -0.5 }; 5];
        assert!(writer.write_stereo(&five).is_err());
        assert_eq!(
            writer.bytes_written(),
            ACCEPT_FRAMES * u64::from(FRAME_BYTES) + 3,
            "accounting must cover exactly the accepted bytes"
        );
        writer.finalize().unwrap();
        assert_eq!(
            u64::from(read_data_size(&writer)),
            ACCEPT_FRAMES * u64::from(FRAME_BYTES),
            "header advertises the whole frames actually on disk"
        );
    }

    /// #694 (CR round 3) — same contract for the IQ path.
    #[test]
    fn partial_iq_write_is_accounted_before_finalize() {
        const ACCEPT_FRAMES: u64 = 3;
        let limit = HEADER_ONLY + usize::try_from(ACCEPT_FRAMES * u64::from(FRAME_BYTES)).unwrap();
        let mut writer = WavWriter::with_sink(
            BufWriter::with_capacity(UNBUFFERED, FailingSink::new(limit)),
            2_400_000,
            2,
        )
        .unwrap();
        let eight = vec![Complex::new(1.0, 0.0); 8];
        assert!(writer.write_iq(&eight).is_err());
        assert_eq!(
            writer.bytes_written(),
            ACCEPT_FRAMES * u64::from(FRAME_BYTES)
        );
        writer.finalize().unwrap();
        assert_eq!(
            u64::from(read_data_size(&writer)),
            ACCEPT_FRAMES * u64::from(FRAME_BYTES)
        );
    }

    /// #694 — the default cap must keep both `u32` header fields in range.
    #[test]
    fn default_cap_fits_the_riff_size_field() {
        assert!(
            u32::try_from(MAX_WAV_DATA_BYTES + u64::from(WAV_HEADER_SIZE - RIFF_HEADER_PREFIX))
                .is_ok()
        );
    }

    #[test]
    fn explicit_finalize_is_idempotent() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_idempotent.wav");

        let mut writer = WavWriter::new(&path, 48_000, 2).unwrap();
        writer.finalize().unwrap();
        writer.finalize().unwrap(); // second call is a no-op

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn empty_wav_is_valid() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_empty.wav");

        {
            let _writer = WavWriter::new(&path, 48_000, 2).unwrap();
        }

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 44); // header only
        let data_size = u32::from_le_bytes([data[40], data[41], data[42], data[43]]);
        assert_eq!(data_size, 0);
        let riff_size = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        assert_eq!(riff_size, 36);

        std::fs::remove_file(&path).unwrap();
    }
}
