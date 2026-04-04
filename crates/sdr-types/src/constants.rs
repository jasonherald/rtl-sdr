/// Stream buffer size in samples — matches SDR++ `STREAM_BUFFER_SIZE`.
pub const STREAM_BUFFER_SIZE: usize = 1_000_000;

/// Default audio sample rate in Hz.
pub const DEFAULT_AUDIO_SAMPLE_RATE: f64 = 48_000.0;

/// Alignment in bytes for sample buffer allocation.
///
/// 32 bytes covers AVX (256-bit) alignment. AVX-512 would need 64.
pub const BUFFER_ALIGNMENT: usize = 32;
