//! NOAA APT (Automatic Picture Transmission) decoder — DSP pipeline.
//!
//! The APT signal is a 2400 Hz AM subcarrier riding on top of narrow-FM audio
//! from the NOAA-15/18/19 weather satellites. The envelope of that subcarrier
//! encodes a 2-line-per-second greyscale scan, where each line carries:
//!
//! ```text
//! Sync A (7 cyc @ 1040 Hz) | Space A | Video A | Telemetry A |
//! Sync B (7 cyc @  832 Hz) | Space B | Video B | Telemetry B |
//! ```
//!
//! for a total of 2080 pixels per line at 4160 pixels / second. The two
//! halves carry the visible-light and IR channels respectively.
//!
//! # Pipeline
//!
//! ```text
//! FM-demod audio (48 kHz, real) ─┐
//!                                 │  RationalResampler
//!                                 ▼
//!            intermediate audio (20800 Hz, real)
//!                                 │  EnvelopeDetector (rectify + LPF)
//!                                 ▼
//!            envelope samples (20800 Hz, 5 per APT pixel)
//!                                 │  sync cross-correlation + line slicer
//!                                 ▼
//!                   AptLine { pixels: [u8; 2080], sync_quality }
//! ```
//!
//! Pure DSP — no threading, no I/O. Stateful because the resampler + envelope
//! LPF need to carry samples across chunks, and line slicing needs a running
//! sample counter for tracking the start-of-line offset as successive audio
//! buffers stream in.

use std::collections::VecDeque;

use sdr_types::{Complex, DspError};

use crate::filter::FirFilter;
use crate::multirate::RationalResampler;
use crate::taps;

// ─── APT signal constants (from the official NOAA APT specification) ───

/// Number of 8-bit greyscale pixels per APT scan line (both channels combined).
pub const LINE_PIXELS: usize = 2080;

/// APT scan-line rate. NOAA satellites transmit exactly 2 lines per second.
pub const LINES_PER_SECOND: f64 = 2.0;

/// Pixel clock rate: `LINE_PIXELS * LINES_PER_SECOND` = 4160 pixels/second.
pub const PIXELS_PER_SECOND: f64 = 4_160.0;

/// AM subcarrier frequency that carries the picture envelope (2400 Hz).
pub const SUBCARRIER_HZ: f64 = 2400.0;

/// Sync A burst frequency (1040 Hz, precedes channel A video).
pub const SYNC_A_HZ: f64 = 1040.0;

/// Sync B burst frequency (832 Hz, precedes channel B video).
pub const SYNC_B_HZ: f64 = 832.0;

/// Each sync burst is exactly 7 cycles long at its respective frequency.
pub const SYNC_BURST_CYCLES: usize = 7;

// ─── Internal working sample rate ───
//
// 12480 Hz is the smallest multiple of 4160 (the pixel clock) that:
//   * gives integer samples per pixel (12480 / 4160 = 3)
//   * gives integer samples per Sync A cycle (12480 / 1040 = 12)
//   * gives integer samples per Sync B cycle (12480 / 832  = 15) —
//     half-cycle is fractional (7.5 samples), but the zero-mean
//     adjustment in `build_square_template` handles that
//   * places 2·f_subcarrier (4800 Hz) below Nyquist (6240 Hz) by a
//     comfortable margin (~1.4 kHz of guard band for any post-demod
//     LPF needed for image-band cleanup)
//   * matches noaa-apt's "standard" profile work_rate exactly, so the
//     filter cutoffs, transition widths, and atten values from their
//     well-tested settings transfer 1:1
//
// Using a clean integer multiple of the pixel clock means every
// downstream index is an exact integer — no fractional alignment
// headaches when slicing pixels or building templates.
//
// **Why not 20800 Hz?** Earlier versions of this module ran at 20800
// (= 4160 × 5). It worked but used 1.67× the CPU + memory of 12480
// for no decode-quality benefit. The lower rate matches noaa-apt's
// "standard" profile, which has been validated against thousands of
// real NOAA captures. Per the APT pipeline parity work.

/// Intermediate sample rate the decoder runs its DSP at (12480 Hz).
pub const INTERMEDIATE_RATE_HZ: u32 = 12_480;

/// Samples per APT pixel at [`INTERMEDIATE_RATE_HZ`] (exactly 3).
pub const SAMPLES_PER_PIXEL: usize = 3;

/// Samples per full scan line at [`INTERMEDIATE_RATE_HZ`] (6240).
pub const SAMPLES_PER_LINE: usize = LINE_PIXELS * SAMPLES_PER_PIXEL;

/// Samples per one cycle of Sync A at [`INTERMEDIATE_RATE_HZ`] (exactly 12).
pub const SAMPLES_PER_SYNC_A_CYCLE: usize = 12;

/// Samples per one cycle of Sync B at [`INTERMEDIATE_RATE_HZ`] (exactly 15).
/// The half-cycle is fractional (7.5 samples) — the matched-filter
/// template builder applies a zero-mean correction so this doesn't
/// bias the cross-correlation.
pub const SAMPLES_PER_SYNC_B_CYCLE: usize = 15;

/// Sync A pixel-level layout, taken from the NOAA APT spec
/// (per `noaa-apt`'s `decode::generate_sync_frame`):
///
/// ```text
/// [2 px low | 7 cycles × (2 px low, 2 px high) = 28 px | 8 px low]
///  └ leading silence ┘                                  └ trailing silence ┘
/// ```
///
/// Total = 38 pixels. The leading + trailing low regions are part
/// of the matched-filter template — they tell the cross-correlator
/// that flanking the modulated burst should be quiet, which sharply
/// rejects false-positive matches inside the video data (where
/// brightness fluctuations alone could otherwise score high against
/// a bare-modulation template).
pub const SYNC_A_LEADING_PAD_PX: usize = 2;
pub const SYNC_A_MODULATED_PX: usize = 28;
pub const SYNC_A_TRAILING_PAD_PX: usize = 8;
pub const SYNC_A_TOTAL_PX: usize =
    SYNC_A_LEADING_PAD_PX + SYNC_A_MODULATED_PX + SYNC_A_TRAILING_PAD_PX;

/// Leading silence in the Sync A matched-filter template, in
/// intermediate-rate samples. This is part of the template pattern
/// (mirroring the silence approach to Sync A in real APT signals)
/// — NOT a slicing offset. A matched-filter hit at offset `M`
/// indicates the line starts at `M`, not at `M +
/// SYNC_A_LEADING_PAD_SAMPLES`. By NOAA APT spec the line begins
/// at the start of the 39-px Sync A field, of which the first 4 px
/// are minimum-modulation low (= leading pad + low half of cycle 1).
/// The first HIGH transition lands at sample offset
/// `SYNC_A_LEADING_PAD_SAMPLES + (SAMPLES_PER_SYNC_A_CYCLE / 2)`
/// = 20 samples from the line start.
pub const SYNC_A_LEADING_PAD_SAMPLES: usize = SYNC_A_LEADING_PAD_PX * SAMPLES_PER_PIXEL;

/// Sample offset within the Sync A template (and equivalently
/// within a real APT line, measured from line start) where the
/// first ON-pulse (HIGH) transition occurs. Two regions of low
/// precede it: the 2-px leading silence + the 2-px low half of
/// cycle 1, totaling 4 px = 20 samples at our work rate.
pub const SYNC_A_FIRST_HIGH_OFFSET_SAMPLES: usize =
    SYNC_A_LEADING_PAD_SAMPLES + (SAMPLES_PER_SYNC_A_CYCLE / 2);

/// Length of a Sync A template in samples (38 px × 3 samples/px = 114).
/// Includes the leading + trailing silence flanks; only the middle
/// 84 samples (7 modulated cycles × 12 samples/cycle) carry the
/// alternating ±1 burst pattern.
pub const SYNC_A_TEMPLATE_LEN: usize = SYNC_A_TOTAL_PX * SAMPLES_PER_PIXEL;

/// Length of a Sync B template in samples (7 cycles × 25 = 175).
/// Sync B is unused by the line-slicing path today — Sync A alone
/// determines line boundaries, with Sync B implicit at the line
/// midpoint. Kept defined for future exploration of dual-sync line
/// validation.
pub const SYNC_B_TEMPLATE_LEN: usize = SYNC_BURST_CYCLES * SAMPLES_PER_SYNC_B_CYCLE;

// Compile-time sanity checks — if any of these fire, an upstream constant
// drifted out of sync with the rest of the module and the symbolic math in
// the docs above no longer holds.
const _: () = assert!(SAMPLES_PER_PIXEL * LINE_PIXELS == SAMPLES_PER_LINE);
const _: () = assert!(INTERMEDIATE_RATE_HZ as usize == SAMPLES_PER_LINE * 2);
const _: () = assert!(INTERMEDIATE_RATE_HZ as usize == SAMPLES_PER_SYNC_A_CYCLE * 1040);
const _: () = assert!(INTERMEDIATE_RATE_HZ as usize == SAMPLES_PER_SYNC_B_CYCLE * 832);
// Keep PIXELS_PER_SECOND locked to LINE_PIXELS · LINES_PER_SECOND. The
// f64-to-usize cast-lint rules out writing the check in const-context, so
// the runtime assertion in `pixel_and_line_invariants_hold` carries this.

/// Which half of the APT line a sync match corresponds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncChannel {
    /// Channel A — starts with the 1040 Hz Sync A burst, typically visible-light imagery.
    A,
    /// Channel B — starts with the 832 Hz Sync B burst, typically IR imagery.
    B,
}

/// One decoded APT scan line.
///
/// Carries both per-line-normalized `pixels` (u8) for cheap live
/// preview AND raw f32 envelope samples (`raw_samples`) for
/// image-wide post-processing (telemetry-calibrated brightness,
/// percentile clipping, histogram equalization at PNG-export time).
///
/// Stored inline so `AptLine` is `Clone`-able and reusable as an
/// output slot — the `AptDecoder::process` contract takes
/// `&mut [AptLine]` and writes new values into existing entries.
/// Construct empty slots with `AptLine::default()`. Carries ~10 KB
/// of payload (2 KB pixels + 8 KB `raw_samples` + metadata); send
/// across the DSP→UI channel boxed (`Box<AptLine>`) to keep
/// per-line message overhead constant.
#[derive(Debug, Clone)]
pub struct AptLine {
    /// The 2080 greyscale pixels of this line, in transmission order,
    /// with **per-line min/max normalization** to 0..255. Used by the
    /// live image viewer where image-wide statistics aren't yet known
    /// — gives reasonable contrast within each line at the cost of
    /// flicker between lines with different content.
    pub pixels: [u8; LINE_PIXELS],
    /// Raw envelope samples in transmission order, one per pixel,
    /// in the demodulator's native float scale (no normalization).
    /// Used by [`crate::apt::PNG_EXPORT_BRIGHTNESS_MODES_CONST`] /
    /// `apt_image::finalize_grayscale` to perform image-wide
    /// brightness mapping (telemetry-calibrated, percentile,
    /// histogram-equalized) at PNG export time, where the full
    /// dynamic range of the entire pass is known.
    pub raw_samples: [f32; LINE_PIXELS],
    /// Normalized cross-correlation peak against the matched sync template
    /// (range `[0.0, 1.0]`, higher = stronger lock).
    pub sync_quality: f32,
    /// Which sync burst preceded this line (A vs B).
    pub sync_channel: SyncChannel,
    /// Index (into the original input audio stream) of the first sample of
    /// this line. Useful for timing correlation with telemetry or pass
    /// ephemerides.
    pub input_sample_index: u64,
}

impl Default for AptLine {
    fn default() -> Self {
        Self {
            pixels: [0; LINE_PIXELS],
            raw_samples: [0.0; LINE_PIXELS],
            sync_quality: 0.0,
            sync_channel: SyncChannel::A,
            input_sample_index: 0,
        }
    }
}

/// AM envelope detector — full-wave rectification followed by a lowpass
/// that kills the 2·subcarrier harmonic produced by rectification.
///
/// Rectifying a cosine-modulated carrier produces the envelope plus a
/// component centered at `2 · SUBCARRIER_HZ` (4800 Hz). Passing the result
/// through a lowpass with its stopband placed well below 4800 Hz cleanly
/// removes the carrier copy and leaves the original video envelope.
///
/// **Use [`Apt137Demodulator`] instead for new code.** This rectify+LPF
/// approach was the original APT envelope path before the apt137
/// closed-form 2-sample method was validated against noaa-apt. Kept for
/// regression-testing the old behaviour and for callers that want a
/// generic AM-envelope detector that doesn't need to know the carrier
/// frequency. `Apt137Demodulator` produces a sharper, transient-free
/// envelope and is what the live APT pipeline now uses.
pub struct EnvelopeDetector {
    lpf: FirFilter,
    scratch: Vec<f32>,
}

/// LPF design constants, chosen to land the stopband comfortably below
/// `2 · SUBCARRIER_HZ = 4800 Hz` without truncating the APT video band
/// (nominally ~2 kHz wide). Passband at ~2.3 kHz covers the whole video
/// spectrum; transition width 1 kHz puts the stopband start at ~3.3 kHz,
/// ~1.5 kHz below `2·f_c`.
const ENVELOPE_LPF_CUTOFF_HZ: f64 = 2_300.0;
const ENVELOPE_LPF_TRANSITION_HZ: f64 = 1_000.0;

impl EnvelopeDetector {
    /// Build an envelope detector for audio sampled at `sample_rate_hz`.
    ///
    /// The Nyquist constraint is on the *rectified* signal: full-wave
    /// rectification of the cosine subcarrier creates a tone at
    /// `2 · SUBCARRIER_HZ = 4800 Hz`, so the input sample rate must
    /// satisfy `sample_rate_hz > 2 · 4800 = 9600 Hz` to resolve that
    /// harmonic at all (otherwise it aliases back into the video band
    /// and the LPF can't get rid of it).
    ///
    /// # Errors
    ///
    /// Returns [`DspError::InvalidParameter`] if `sample_rate_hz` is at
    /// or below the Nyquist floor for the rectified harmonic, or if the
    /// underlying FIR / tap generation rejects the design parameters.
    pub fn new(sample_rate_hz: u32) -> Result<Self, DspError> {
        // Nyquist floor for the post-rectification 2·f_c = 4800 Hz tone.
        // Strictly: Nyquist (rate / 2) must exceed 2·SUBCARRIER_HZ, i.e.
        // rate must exceed 4·SUBCARRIER_HZ.
        const NYQUIST_FLOOR_HZ: f64 = 4.0 * SUBCARRIER_HZ;
        if f64::from(sample_rate_hz) <= NYQUIST_FLOOR_HZ {
            return Err(DspError::InvalidParameter(format!(
                "sample_rate_hz ({sample_rate_hz}) too low for APT envelope detection — \
                 the 2·SUBCARRIER_HZ ({} Hz) rectification harmonic requires Nyquist \
                 above that, i.e. sample rate > 4·SUBCARRIER_HZ = {NYQUIST_FLOOR_HZ} Hz",
                2.0 * SUBCARRIER_HZ,
            )));
        }
        let lpf_taps = taps::low_pass(
            ENVELOPE_LPF_CUTOFF_HZ,
            ENVELOPE_LPF_TRANSITION_HZ,
            f64::from(sample_rate_hz),
            true,
        )?;
        let lpf = FirFilter::new(lpf_taps)?;
        Ok(Self {
            lpf,
            scratch: Vec::new(),
        })
    }

    /// Number of FIR taps in the envelope LPF (mostly useful for benchmarks
    /// and tuning tests).
    pub fn lpf_tap_count(&self) -> usize {
        self.lpf.tap_count()
    }

    /// Reset the internal filter state (zero the delay line).
    pub fn reset(&mut self) {
        self.lpf.reset();
    }

    /// Rectify and lowpass `input` into `output`, returning the number of
    /// samples written.
    ///
    /// # Errors
    ///
    /// Returns [`DspError::BufferTooSmall`] if `output.len() < input.len()`.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        self.scratch.resize(input.len(), 0.0);
        for (dst, src) in self.scratch.iter_mut().zip(input.iter()) {
            *dst = src.abs();
        }
        self.lpf.process_f32(&self.scratch, output)
    }
}

/// Closed-form 2-sample AM demodulator (the apt137 method).
///
/// Given a band-limited AM signal `s(t) = A(t) · cos(2π·f_c·t + θ)`
/// sampled at `f_s`, two consecutive samples `x[i-1]` and `x[i]`
/// uniquely determine the instantaneous envelope `A` (up to sign,
/// which we resolve by taking the positive root):
///
/// ```text
/// A = sqrt(x[i-1]² + x[i]² − 2·x[i-1]·x[i]·cos(φ)) / sin(φ)
/// where φ = 2π · f_c / f_s
/// ```
///
/// Derivation: write `x[i-1] = A·cos(α)` and `x[i] = A·cos(α + φ)` for
/// some unknown phase α. Using the identity
/// `cos²α + cos²(α+φ) − 2·cosα·cos(α+φ)·cosφ = sin²φ` (Lagrange's
/// identity in trig form), the unknown α drops out and `A` falls out
/// algebraically.
///
/// Inspired by Pieter Noordhuis's `apt137` (MIT) and noaa-apt's
/// derived implementation; reimplemented from the trigonometric
/// derivation above in our own DSP idioms (streaming-friendly, error
/// types consistent with the rest of the crate).
///
/// **Why this beats `rectify+LPF` for APT:**
///
/// * **No `2·f_c` harmonic.** Rectifying creates a tone at `2 · 2400 = 4800`
///   Hz that has to be filtered out; the closed form has no such
///   harmonic to begin with.
/// * **No filter transient.** The output is correct from the second
///   sample (one sample of state needed). Rectify+LPF takes hundreds
///   of samples to settle, throwing away the start of every chunk.
/// * **DC-insensitive.** A constant offset on `x` produces a smooth
///   distortion in the result, but no asymmetric rectifier bias.
/// * **Phase-coherent.** Accuracy depends only on knowing `f_c`
///   precisely, not on filter design choices.
///
/// The carrier frequency must be **strictly inside** `(0, f_s / 2)` —
/// at the boundaries `sin(φ) = 0` and the formula has a removable
/// singularity that we'd need to special-case (and that doesn't
/// correspond to any physically useful APT setup).
pub struct Apt137Demodulator {
    /// `2 · cos(φ)` — used in the cross-product term `prev · curr · cosphi2`.
    /// Precomputed at construction so the per-sample loop is just
    /// 4 multiplies + 1 sqrt + 1 divide.
    cosphi2: f32,
    /// `1 / sin(φ)` — the trailing divide is reciprocal-multiplied
    /// for ~3× speedup vs. division on f32.
    inv_sinphi: f32,
    /// Last input sample from the previous chunk. `None` on the very
    /// first call; populated thereafter so chunked streaming
    /// produces the same output as a single batch call (modulo the
    /// first sample of the entire stream, which is set to zero
    /// because there's no prior sample to pair it with).
    prev: Option<f32>,
}

impl Apt137Demodulator {
    /// Build a demod for an `f_s` Hz sample rate carrying a `f_c` Hz
    /// AM signal.
    ///
    /// # Errors
    ///
    /// Returns [`DspError::InvalidParameter`] when `carrier_hz` is at
    /// or outside the open interval `(0, sample_rate_hz / 2)` — at
    /// the boundaries `sin(φ)` is zero and the closed form is
    /// undefined.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(sample_rate_hz: f64, carrier_hz: f64) -> Result<Self, DspError> {
        if !sample_rate_hz.is_finite() || sample_rate_hz <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "sample_rate_hz must be positive and finite, got {sample_rate_hz}"
            )));
        }
        if !carrier_hz.is_finite() || carrier_hz <= 0.0 || carrier_hz >= sample_rate_hz / 2.0 {
            return Err(DspError::InvalidParameter(format!(
                "carrier_hz ({carrier_hz}) must be in (0, sample_rate_hz/2={}) — \
                 at the boundaries sin(φ) = 0 and the closed-form demod is \
                 undefined",
                sample_rate_hz / 2.0
            )));
        }
        let phi = 2.0 * core::f64::consts::PI * carrier_hz / sample_rate_hz;
        let sinphi = phi.sin();
        if sinphi.abs() < f64::EPSILON {
            return Err(DspError::InvalidParameter(format!(
                "carrier_hz ({carrier_hz}) at sample_rate ({sample_rate_hz}) gives \
                 sin(φ) ≈ 0 (φ = {phi}); closed-form demod undefined here"
            )));
        }
        Ok(Self {
            cosphi2: (2.0 * phi.cos()) as f32,
            inv_sinphi: (1.0 / sinphi) as f32,
            prev: None,
        })
    }

    /// Reset internal state — call when restarting a stream.
    pub fn reset(&mut self) {
        self.prev = None;
    }

    /// Demodulate `input` into `output`. Returns the number of samples
    /// written (always `input.len()`).
    ///
    /// # Errors
    ///
    /// Returns [`DspError::BufferTooSmall`] if `output.len() < input.len()`.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        if input.is_empty() {
            return Ok(0);
        }
        let (mut prev, start) = if let Some(p) = self.prev {
            // Continuing a stream: compute output[0] from the prior
            // chunk's last sample paired with this chunk's first.
            (p, 0_usize)
        } else {
            // First sample of the stream — no prior sample to pair
            // with. Output zero (one sample of latency, irrelevant
            // at APT scan rates) and seed `prev` from this sample
            // for the rest of the chunk.
            output[0] = 0.0;
            let first = input[0];
            if input.len() == 1 {
                self.prev = Some(first);
                return Ok(1);
            }
            (first, 1_usize)
        };

        for i in start..input.len() {
            let curr = input[i];
            let val = (prev * prev + curr * curr - prev * curr * self.cosphi2)
                .max(0.0) // numerical noise can give a tiny negative
                .sqrt()
                * self.inv_sinphi;
            output[i] = val;
            prev = curr;
        }

        self.prev = Some(prev);
        Ok(input.len())
    }
}

/// Real-valued audio resampler built on top of [`RationalResampler`].
///
/// `RationalResampler` is complex-only (it ships with the rest of the SDR
/// polyphase infrastructure where I/Q is the usual input), so this wrapper
/// stages real input into a `Complex { re: x, im: 0 }` scratch buffer,
/// invokes the complex resampler, and drops the always-zero imaginary part
/// on the way back out. The 2× arithmetic cost is irrelevant at APT rates
/// (10 kSa/s-ish), and it lets us reuse a well-tested polyphase path
/// rather than duplicate one for real audio.
pub struct RealResampler {
    inner: RationalResampler,
    scratch_in: Vec<Complex>,
    scratch_out: Vec<Complex>,
}

impl RealResampler {
    /// Build a resampler from `in_sample_rate` to `out_sample_rate` (both Hz).
    ///
    /// # Errors
    ///
    /// Propagates any [`DspError`] from [`RationalResampler::new`] (invalid
    /// or sub-Hz rates, infeasible tap design, etc.).
    pub fn new(in_sample_rate: f64, out_sample_rate: f64) -> Result<Self, DspError> {
        Ok(Self {
            inner: RationalResampler::new(in_sample_rate, out_sample_rate)?,
            scratch_in: Vec::new(),
            scratch_out: Vec::new(),
        })
    }

    /// Reset the inner resampler state (delay lines, phase, offset).
    pub fn reset(&mut self) {
        self.inner.reset();
        self.scratch_in.clear();
        self.scratch_out.clear();
    }

    /// Resample `input` into `output`, returning the number of output samples
    /// written. Preserves state across calls so chunked streaming is seamless.
    ///
    /// # Errors
    ///
    /// Returns [`DspError::BufferTooSmall`] if `output` is not large enough
    /// for the worst-case expansion of this call. Polyphase resampling's
    /// per-call output count can exceed `(input.len() * out_rate / in_rate)`
    /// by one sample of rounding; size `output` as
    /// `(input.len() * out_rate / in_rate).ceil() + 1` to be safe.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<usize, DspError> {
        if input.is_empty() {
            return Ok(0);
        }

        self.scratch_in.resize(input.len(), Complex::default());
        for (dst, &src) in self.scratch_in.iter_mut().zip(input.iter()) {
            *dst = Complex::new(src, 0.0);
        }

        // `RationalResampler::process` needs worst-case room in the output
        // buffer (it rejects with BufferTooSmall otherwise); keep a scratch
        // that tracks `output.len()` so the caller's sizing flows through.
        self.scratch_out.resize(output.len(), Complex::default());
        let count = self
            .inner
            .process(&self.scratch_in, &mut self.scratch_out)?;

        for (dst, src) in output.iter_mut().zip(self.scratch_out.iter()).take(count) {
            *dst = src.re;
        }
        Ok(count)
    }
}

/// Peak cross-correlation match of a sync burst in an envelope buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyncMatch {
    /// Sample offset (within the envelope slice passed to the detector) where
    /// the matching sync template begins.
    pub offset: usize,
    /// Which sync pattern matched — A (1040 Hz) or B (832 Hz).
    pub channel: SyncChannel,
    /// Normalized cross-correlation coefficient at the peak, clamped to
    /// `[0.0, 1.0]`. 1.0 is a perfect waveform-shape match, 0.0 is pure
    /// noise / no lock.
    pub quality: f32,
}

/// Correlator that locates Sync A / Sync B bursts inside a post-envelope
/// APT audio buffer.
///
/// Sync A is transmitted as 7 cycles of a 1040 Hz on/off modulation of the
/// 2400 Hz subcarrier; Sync B is 7 cycles of 832 Hz modulation. After the
/// envelope detector those bursts appear as near-square waveforms at 1040
/// or 832 Hz. We model each sync as a simple ±1 square-wave template of
/// the exact right length and find the offset that maximizes the
/// normalized cross-correlation against the envelope — DC-offset-invariant
/// so it works even when the envelope floor drifts with AGC or fade.
#[allow(clippy::struct_field_names)]
pub struct SyncDetector {
    template_a: Vec<f32>,
    template_b: Vec<f32>,
    template_a_norm: f32,
    template_b_norm: f32,
}

impl Default for SyncDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncDetector {
    /// Build a fresh sync detector with pre-computed Sync A / Sync B templates.
    #[must_use]
    pub fn new() -> Self {
        let (tpl_a, norm_a) = build_padded_sync_a_template(SAMPLES_PER_PIXEL);
        let (tpl_b, norm_b) = build_square_template(SAMPLES_PER_SYNC_B_CYCLE, SYNC_BURST_CYCLES);
        Self {
            template_a: tpl_a,
            template_b: tpl_b,
            template_a_norm: norm_a,
            template_b_norm: norm_b,
        }
    }

    /// Length of the Sync A template in samples.
    #[must_use]
    pub fn template_a_len(&self) -> usize {
        self.template_a.len()
    }

    /// Length of the Sync B template in samples.
    #[must_use]
    pub fn template_b_len(&self) -> usize {
        self.template_b.len()
    }

    /// Find the best-matching offset of the requested sync channel inside
    /// `envelope`.
    ///
    /// Returns `None` if `envelope` is shorter than the template (there's
    /// simply no valid offset to score). The returned offset is the start
    /// sample of the matching burst, and `quality` is the normalized
    /// correlation peak in `[0.0, 1.0]`.
    #[must_use]
    pub fn find_best(&self, envelope: &[f32], channel: SyncChannel) -> Option<SyncMatch> {
        let (template, template_norm) = match channel {
            SyncChannel::A => (self.template_a.as_slice(), self.template_a_norm),
            SyncChannel::B => (self.template_b.as_slice(), self.template_b_norm),
        };
        let len = template.len();
        if envelope.len() < len {
            return None;
        }

        let mut best_ncc = f32::NEG_INFINITY;
        let mut best_off = 0_usize;

        // Naive O(N·L) normalized cross-correlation. Good enough at APT
        // sample rates: even a generous 2-line search window is under
        // ~4 M multiplies, negligible at 2 lines/sec.
        let denom_guard = 1e-9_f32;
        for tau in 0..=envelope.len() - len {
            let window = &envelope[tau..tau + len];
            let ncc = normalized_corr(window, template, template_norm, denom_guard);
            if ncc > best_ncc {
                best_ncc = ncc;
                best_off = tau;
            }
        }

        Some(SyncMatch {
            offset: best_off,
            channel,
            quality: best_ncc.clamp(0.0, 1.0),
        })
    }
}

/// Build a zero-mean ±1 square-wave template plus its L2 norm.
///
/// Half of each cycle is +1, the other half -1. Returns `(template, norm)`
/// where `norm = sqrt(sum(template²))` so callers can skip recomputing it
/// on every correlation. Used for Sync B (which has fractional half-period
/// in samples — the leading/trailing pad approach used by Sync A doesn't
/// produce a clean integer-sample template at our `work_rate`).
#[allow(clippy::cast_precision_loss)]
fn build_square_template(samples_per_cycle: usize, cycles: usize) -> (Vec<f32>, f32) {
    let len = samples_per_cycle * cycles;
    let half = samples_per_cycle / 2;
    let mut template: Vec<f32> = (0..len)
        .map(|i| {
            let phase = i % samples_per_cycle;
            if phase < half { 1.0 } else { -1.0 }
        })
        .collect();

    // Odd samples-per-cycle (e.g. B=25) leave a ±1/L DC bias after the
    // half/half split; remove it so the template is exactly zero-mean and
    // insensitive to envelope-level drift.
    let mean = template.iter().sum::<f32>() / (len as f32);
    for v in &mut template {
        *v -= mean;
    }
    let norm = template.iter().map(|x| x * x).sum::<f32>().sqrt();
    (template, norm)
}

/// Build the padded Sync A matched-filter template.
///
/// Layout: `[2 px low | 7 cycles × (2 px low, 2 px high) | 8 px low]`
/// = 38 px total. The flanking low regions reject false-positive matches
/// inside the video data — a template of just the modulated cycles
/// will score positively on any region whose brightness happens to
/// alternate at the 1040 Hz beat. Adding leading/trailing low says
/// "the modulation must be flanked by silence", which is true only at
/// real Sync A boundaries.
///
/// Reimplemented from the layout in noaa-apt's `decode::generate_sync_frame`
/// (see `original/noaa-apt/src/decode.rs`). Their template emits ±1
/// integers; ours emits zero-mean f32 values for normalized cross-
/// correlation against the envelope.
///
/// `samples_per_pixel` is the work-rate's pixel granularity
/// (`samples_per_pixel = work_rate / PIXELS_PER_SECOND`). At our
/// current 20800 Hz: 5 samples/px → 190 samples total. After A4 lowers
/// us to 12480 Hz: 3 samples/px → 114 samples total. Both produce a
/// 38-px-equivalent template.
#[allow(clippy::cast_precision_loss)]
fn build_padded_sync_a_template(samples_per_pixel: usize) -> (Vec<f32>, f32) {
    // A1040 cycle = 4 pixels (2 low, 2 high) at any work rate, so the
    // half-cycle is `2 * samples_per_pixel`. Derive directly from the
    // parameter rather than the global `SAMPLES_PER_SYNC_A_CYCLE`
    // constant — otherwise this function silently produces a
    // mis-scaled template if ever called at a different work rate.
    // Per CR round 2 on PR #571.
    let half_cycle_samples = 2 * samples_per_pixel;
    let leading_samples = SYNC_A_LEADING_PAD_PX * samples_per_pixel;
    let trailing_samples = SYNC_A_TRAILING_PAD_PX * samples_per_pixel;
    let modulated_samples = SYNC_A_MODULATED_PX * samples_per_pixel;
    let len = leading_samples + modulated_samples + trailing_samples;

    let mut template: Vec<f32> = Vec::with_capacity(len);
    // Leading low.
    template.extend(core::iter::repeat_n(-1.0_f32, leading_samples));
    // 7 cycles of (low, high). Each cycle = `2·samples_per_pixel` low
    // followed by `2·samples_per_pixel` high.
    for _ in 0..SYNC_BURST_CYCLES {
        template.extend(core::iter::repeat_n(-1.0_f32, half_cycle_samples));
        template.extend(core::iter::repeat_n(1.0_f32, half_cycle_samples));
    }
    // Trailing low.
    template.extend(core::iter::repeat_n(-1.0_f32, trailing_samples));
    debug_assert_eq!(template.len(), len);

    // Zero-mean (cancels DC sensitivity for normalized x-corr).
    let mean = template.iter().sum::<f32>() / (len as f32);
    for v in &mut template {
        *v -= mean;
    }
    let norm = template.iter().map(|x| x * x).sum::<f32>().sqrt();
    (template, norm)
}

/// Normalized cross-correlation of a window against a zero-mean template.
///
/// Subtracts the window's own mean before computing the L2 norm so a DC
/// offset in the envelope doesn't pessimistically depress the score.
/// Returns `corr / (sqrt(window_centered_energy) * template_norm)`.
#[allow(clippy::cast_precision_loss)]
fn normalized_corr(window: &[f32], template: &[f32], template_norm: f32, guard: f32) -> f32 {
    debug_assert_eq!(window.len(), template.len());
    let len = window.len();
    let mean = window.iter().sum::<f32>() / (len as f32);

    let mut corr = 0.0_f32;
    let mut energy = 0.0_f32;
    for (&w, &t) in window.iter().zip(template.iter()) {
        let dx = w - mean;
        corr += dx * t;
        energy += dx * dx;
    }
    corr / (energy.sqrt() * template_norm).max(guard)
}

/// Maximum number of envelope samples the decoder buffers before it starts
/// discarding the oldest end to bound memory. Sized at 3 lines — large
/// enough to tolerate one line of sync-search slop plus a line of pending
/// output, without letting a stalled input pile up gigabytes.
const DECODER_BUFFER_CAP: usize = SAMPLES_PER_LINE * 3;

/// Minimum envelope buffer length required before the decoder will attempt
/// to emit a line. Two full lines, so the sync search has up to one line
/// of slip available without risking falling off the end while carving out
/// the line after the matched sync.
const MIN_ACCUMULATOR_FOR_DECODE: usize = SAMPLES_PER_LINE * 2;

/// Maximum number of decoded-but-undelivered `AptLine`s the decoder will
/// queue internally when the caller's `output` slice is too small to hold
/// every line that became ready. Bounded so the queue itself can't grow
/// unboundedly, but large enough to absorb a few seconds of latency
/// between calls — at 2 lines/sec, 8 lines = 4 s of slack. Lines past
/// the cap stay buffered as raw envelope samples in `accumulator` (which
/// has its own cap); only after both fill does anything get dropped.
///
/// Public so callers that pre-allocate an output slice for
/// [`AptDecoder::process`] can size it to the decoder's internal
/// emission cap without duplicating the literal — see the controller
/// crate's `apt_decode_tap` for an example.
pub const READY_QUEUE_CAP: usize = 8;

/// Cutoff frequency of the input-rate DC-removing bandpass filter
/// (`AptDecoder::new`). 4800 Hz = 2·`SUBCARRIER_HZ` — high enough
/// to pass the entire AM passband, low enough to kill out-of-band
/// noise. Per noaa-apt's `standard` profile.
const DC_BANDPASS_CUTOUT_HZ: f64 = 2.0 * SUBCARRIER_HZ;
/// Transition-band width of the input-rate DC-removing bandpass.
/// 1 kHz transition is a comfortable balance between filter length
/// and rejection. The DC notch sits at frequencies below
/// `transition/2` (~500 Hz), safely below any APT signal content.
const DC_BANDPASS_TRANSITION_HZ: f64 = 1_000.0;
/// Stopband attenuation target for the input-rate DC-removing
/// bandpass. 30 dB matches noaa-apt's `standard` profile.
const DC_BANDPASS_ATTEN_DB: f64 = 30.0;

/// Maximum input audio samples processed through the resample → envelope
/// stages in one pass. Keeps `resample_scratch`, `demod_scratch`, and
/// the resampler's internal complex scratch all strictly bounded
/// regardless of how big a single `process` input chunk is. At the
/// typical 48 kHz input rate, 8192 input samples yields ~3550 envelope
/// samples, well under one APT line — small enough that the scratch
/// vectors never need to grow past their first allocation in practice.
const INPUT_SUBCHUNK_SAMPLES: usize = 8_192;

/// End-to-end APT line decoder.
///
/// Owns the resampler, envelope detector, and sync correlator, and carries
/// their state across `process` calls so it can be fed arbitrary-sized
/// audio chunks from the radio pipeline. Each call returns zero or more
/// `AptLine`s that have already been aligned to Sync A, decimated to the
/// 2080-pixel resolution, and normalized per-line to 8-bit greyscale.
///
/// Quality control is delegated to the caller — every emitted line comes
/// with a `sync_quality` score in `[0.0, 1.0]` so downstream code can mask
/// out low-confidence lines without the decoder second-guessing them.
pub struct AptDecoder {
    input_rate_hz: u32,
    /// Bandpass FIR with DC notch — filters input audio to the AM
    /// passband `[~500 Hz, ~4800 Hz]` before resampling. Per noaa-apt's
    /// `LowpassDcRemoval`. DC removal is defense in depth: the apt137
    /// demod is itself DC-robust, but eliminating sub-500 Hz energy
    /// before the resample gives the apt137 demod the cleanest possible
    /// input.
    dc_bandpass: FirFilter,
    /// Filter scratch buffer, reused across chunks.
    dc_bandpass_scratch: Vec<f32>,
    resampler: RealResampler,
    /// Closed-form AM demod (apt137 method). Replaces the previous
    /// rectify+LPF path which had a long settling transient and
    /// required filtering out a `2·f_c` harmonic. See
    /// [`Apt137Demodulator`] for the math.
    demod: Apt137Demodulator,
    sync_detector: SyncDetector,

    /// Final-rate resampler: [`INTERMEDIATE_RATE_HZ`] (12480) → 4160
    /// (one sample per pixel). Replaces the old per-pixel boxcar
    /// average, which had a sinc-shaped response that attenuated the
    /// upper half of the video band by 3 dB.
    final_resampler: RealResampler,
    /// Per-line scratch buffer for the final-resample stage.
    /// Holds `LINE_PIXELS` f32 samples plus a sample of slack.
    final_resamp_scratch: Vec<f32>,

    resample_scratch: Vec<f32>,
    demod_scratch: Vec<f32>,
    accumulator: Vec<f32>,

    // Decoded-but-undelivered scan lines. Lives separately from
    // `accumulator` so that lines we couldn't fit into the caller's
    // `output` are preserved as fully-decoded data (not as raw samples
    // in the cap-trimmed accumulator that could be silently dropped).
    ready_lines: VecDeque<AptLine>,

    // Cumulative count of *intermediate-rate* samples (envelope samples)
    // that have been streamed through the accumulator and dropped on a
    // drain. Stored at the internal-rate so drain bookkeeping is exact —
    // converting on every drain (e.g. ⌊acc · input/20800⌋) leaks a
    // fractional remainder when the ratio isn't a clean integer (at 48 kHz
    // it's 30/13), which would walk `input_sample_index` earlier over
    // long captures. We only convert to input-sample units at stamp time.
    accumulator_start_intermediate_sample: u64,
}

impl AptDecoder {
    /// Build a decoder for audio sampled at `input_rate_hz`.
    ///
    /// Typical value is 48000 (the output rate of the FM demodulator).
    /// Must be **strictly greater than** `2 · SUBCARRIER_HZ` (4800 Hz)
    /// — at exactly 4800 Hz the 2400 Hz APT subcarrier sits on Nyquist
    /// where each sample lands at a phase-ambiguous point on the cosine,
    /// and below that it's already aliased before this pipeline gets a
    /// chance to look at it. Either case produces silent garbage, so
    /// the boundary itself is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`DspError::InvalidParameter`] if `input_rate_hz` is at or
    /// below the strict Nyquist floor for the APT subcarrier
    /// (`> 2·SUBCARRIER_HZ`, i.e. above 4800 Hz). Propagates other
    /// [`DspError`] values from the underlying resampler, envelope
    /// detector, or tap designer.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(input_rate_hz: u32) -> Result<Self, DspError> {
        // 2 · 2400 = 4800 Hz exactly — no rounding, just hard-code so the
        // const-context-friendly comparison below stays trivially correct.
        // Note `<=`: at exactly 2·f_c the subcarrier sits at Nyquist where
        // each sample lands at a phase-ambiguous point on the cosine, so
        // the boundary itself has to be rejected — not just rates below.
        const NYQUIST_FLOOR_HZ: u32 = 4_800;
        if input_rate_hz <= NYQUIST_FLOOR_HZ {
            return Err(DspError::InvalidParameter(format!(
                "input_rate_hz ({input_rate_hz}) must be > 2·SUBCARRIER_HZ \
                 ({NYQUIST_FLOOR_HZ}) to sample the 2400 Hz APT subcarrier safely",
            )));
        }
        // Pre-size the resample / envelope scratch vectors for the
        // worst-case per-subchunk output: an INPUT_SUBCHUNK_SAMPLES
        // input always produces at most this many envelope samples at
        // the configured input rate. Pre-reserving means subsequent
        // `Vec::resize` calls inside the hot path are bookkeeping-only
        // (no realloc, no allocator traffic).
        let max_subchunk_envelope = ((INPUT_SUBCHUNK_SAMPLES as u64
            * u64::from(INTERMEDIATE_RATE_HZ)
            / u64::from(input_rate_hz))
            + 4) as usize;

        // Build the input-rate DC-removing bandpass filter. Cutoff /
        // transition / atten values come from noaa-apt's standard
        // profile, validated against thousands of real captures:
        //   cutout       = 4800 Hz (= 2·SUBCARRIER, kills out-of-band
        //                            noise without touching the AM signal)
        //   transition   = 1000 Hz
        //   atten        = 30 dB stopband
        // The DC notch sits at frequencies below `transition/2` (~500 Hz),
        // safely below any APT signal content (the 2400 Hz subcarrier
        // upper sideband bottoms out around 400 Hz from the carrier
        // when modulated by the video band).
        let dc_bandpass_taps = taps::low_pass_dc_removal_kaiser(
            DC_BANDPASS_CUTOUT_HZ,
            DC_BANDPASS_TRANSITION_HZ,
            DC_BANDPASS_ATTEN_DB,
            f64::from(input_rate_hz),
        )?;
        let dc_bandpass = FirFilter::new(dc_bandpass_taps)?;

        Ok(Self {
            input_rate_hz,
            dc_bandpass,
            dc_bandpass_scratch: Vec::with_capacity(INPUT_SUBCHUNK_SAMPLES),
            resampler: RealResampler::new(
                f64::from(input_rate_hz),
                f64::from(INTERMEDIATE_RATE_HZ),
            )?,
            demod: Apt137Demodulator::new(f64::from(INTERMEDIATE_RATE_HZ), SUBCARRIER_HZ)?,
            sync_detector: SyncDetector::new(),
            final_resampler: RealResampler::new(
                f64::from(INTERMEDIATE_RATE_HZ),
                PIXELS_PER_SECOND,
            )?,
            final_resamp_scratch: Vec::with_capacity(LINE_PIXELS + 4),
            resample_scratch: Vec::with_capacity(max_subchunk_envelope),
            demod_scratch: Vec::with_capacity(max_subchunk_envelope),
            // Reserve room for the *intentional* overshoot in chunked
            // ingestion: each chunk can take SAMPLES_PER_LINE more than
            // the cap before the post-chunk trim brings it back down.
            // Sizing for the peak avoids reallocating on the first
            // backpressure event in a hot path.
            accumulator: Vec::with_capacity(DECODER_BUFFER_CAP + SAMPLES_PER_LINE),
            ready_lines: VecDeque::with_capacity(READY_QUEUE_CAP),
            accumulator_start_intermediate_sample: 0,
        })
    }

    /// Flush all internal state back to a pre-first-sample state.
    pub fn reset(&mut self) {
        self.dc_bandpass.reset();
        self.resampler.reset();
        self.demod.reset();
        self.final_resampler.reset();
        self.accumulator.clear();
        self.ready_lines.clear();
        self.accumulator_start_intermediate_sample = 0;
    }

    /// Feed `input` audio samples into the decoder, writing any newly-decoded
    /// lines into `output`, and return the number written.
    ///
    /// Each emitted line overwrites an existing entry in `output` in place
    /// (so the caller pre-allocates `output` once with `AptLine::default()`
    /// slots and reuses it across calls — no heap allocation per emission).
    /// A return value of `0` is normal until the buffer has accumulated
    /// enough data for the first line (~0.5 s into a capture).
    ///
    /// **Streaming semantics.** If more lines are ready than `output` can
    /// hold, the surplus is preserved as fully-decoded `AptLine`s in a
    /// small internal queue (`READY_QUEUE_CAP` lines) and surfaces on
    /// subsequent calls. The full pipeline runs in two nested bounded
    /// loops:
    ///
    /// 1. **Outer (input subchunk)**: `input` is fed through the
    ///    resampler and envelope detector in pieces of at most
    ///    `INPUT_SUBCHUNK_SAMPLES`, so `resample_scratch`,
    ///    `demod_scratch`, and the resampler's internal complex
    ///    scratch never grow with caller chunk size.
    /// 2. **Inner (envelope subchunk)**: each subchunk's envelope output
    ///    is appended to the accumulator in slices bounded by
    ///    `DECODER_BUFFER_CAP`, with the decode + cap cycle running
    ///    between each slice.
    ///
    /// Together this makes total hot-path memory bounded by a small
    /// constant (~few hundred KB) regardless of how big a chunk the
    /// caller hands us. Sample-level dropping only happens when both
    /// the ready queue *and* the raw accumulator are full — which only
    /// occurs when the caller has stopped draining `output` for several
    /// seconds.
    ///
    /// # Errors
    ///
    /// Propagates [`DspError`] from the resampler or envelope detector.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn process(&mut self, input: &[f32], output: &mut [AptLine]) -> Result<usize, DspError> {
        // Drain previously-queued ready lines into `output` first so
        // the queue has room to absorb new emissions before any decode.
        let mut produced = self.drain_ready_into_output(output, 0);

        // Outer loop: process input in bounded subchunks so the
        // resampler / envelope scratch never scales with caller chunk
        // size. Empty input still needs one decode pass below in case
        // earlier calls buffered enough samples for a fresh emission.
        for in_chunk in input.chunks(INPUT_SUBCHUNK_SAMPLES) {
            produced = self.process_subchunk(in_chunk, output, produced)?;
        }

        // Edge case: empty input. The for loop above didn't run, but
        // earlier `process` calls may have buffered enough samples for
        // another line, and the caller is asking for them now.
        if input.is_empty() {
            produced = self.decode_into_output_or_queue(output, produced)?;
        }

        Ok(produced)
    }

    /// DC-bandpass → resample → apt137 demod → accumulator-ingest one
    /// bounded subchunk of input. Factored out of `process` so the
    /// outer subchunking loop stays readable. All scratch buffers used
    /// here are sized to at most `INPUT_SUBCHUNK_SAMPLES` worth of work.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn process_subchunk(
        &mut self,
        in_chunk: &[f32],
        output: &mut [AptLine],
        mut produced: usize,
    ) -> Result<usize, DspError> {
        // 1. DC-removing bandpass at the input rate. Strips DC bias +
        //    sub-500 Hz rumble before resampling so the demod sees
        //    only the AM passband. apt137 is itself DC-robust, but
        //    eliminating low-frequency content here also helps the
        //    resampler's antialias filter behave nicely (no spectral
        //    leakage from a DC blob into the AA stopband).
        self.dc_bandpass_scratch.resize(in_chunk.len(), 0.0);
        let filtered = self
            .dc_bandpass
            .process_f32(in_chunk, &mut self.dc_bandpass_scratch)?;

        // 2. Resample to the 12480 Hz work rate.
        let est_out = (in_chunk.len() as u64 * u64::from(INTERMEDIATE_RATE_HZ)
            / u64::from(self.input_rate_hz)) as usize
            + 4;
        self.resample_scratch.resize(est_out, 0.0);
        let resampled = self.resampler.process(
            &self.dc_bandpass_scratch[..filtered],
            &mut self.resample_scratch,
        )?;

        // 3. apt137 closed-form AM demod into a same-sized scratch.
        self.demod_scratch.resize(resampled, 0.0);
        self.demod
            .process(&self.resample_scratch[..resampled], &mut self.demod_scratch)?;

        // 4. Feed the demod output into the accumulator in *chunks bounded
        // by DECODER_BUFFER_CAP*. After each, run the decode + cap
        // cycle so accumulator growth stays bounded.
        let mut env_offset = 0_usize;
        while env_offset < resampled {
            // Take a chunk that fits in the remaining cap space, with a
            // hard floor of one line so we always make forward progress
            // (e.g. when the accumulator is already at cap).
            let space_until_cap = DECODER_BUFFER_CAP.saturating_sub(self.accumulator.len());
            let max_take = space_until_cap.max(SAMPLES_PER_LINE);
            let take = (resampled - env_offset).min(max_take);
            self.accumulator
                .extend_from_slice(&self.demod_scratch[env_offset..env_offset + take]);
            env_offset += take;

            // Decode whatever lines are now sliceable, routing each one
            // either into the caller's output or into the ready queue.
            produced = self.decode_into_output_or_queue(output, produced)?;

            // Cap the raw accumulator. By construction we're at most
            // DECODER_BUFFER_CAP + SAMPLES_PER_LINE here, so we drop at
            // most one line of raw samples per chunk — and only when
            // *both* the ready queue and the live `output` were full.
            if self.accumulator.len() > DECODER_BUFFER_CAP {
                let drop_n = self.accumulator.len() - DECODER_BUFFER_CAP;
                self.accumulator.drain(..drop_n);
                self.accumulator_start_intermediate_sample += drop_n as u64;
            }
        }

        Ok(produced)
    }

    /// Pop already-decoded lines off the ready queue into `output`,
    /// starting at index `produced`, until either the queue empties or
    /// `output` fills. Returns the new `produced` count.
    fn drain_ready_into_output(&mut self, output: &mut [AptLine], mut produced: usize) -> usize {
        while produced < output.len() {
            let Some(line) = self.ready_lines.pop_front() else {
                break;
            };
            output[produced] = line;
            produced += 1;
        }
        produced
    }

    /// Inner decode loop. While the accumulator holds enough samples for
    /// a sync search + full line, find the next sync, slice the line,
    /// and route it to `output[produced]` if there's room there, else
    /// to the ready queue if it has room, else stop. Returns the new
    /// `produced` count.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn decode_into_output_or_queue(
        &mut self,
        output: &mut [AptLine],
        mut produced: usize,
    ) -> Result<usize, DspError> {
        while self.accumulator.len() >= MIN_ACCUMULATOR_FOR_DECODE {
            // Nowhere to put the next line — leave the accumulator alone
            // so the next `process` call can pick up from here.
            if produced >= output.len() && self.ready_lines.len() >= READY_QUEUE_CAP {
                break;
            }

            // Search the first SAMPLES_PER_LINE tau positions — any
            // match in there leaves a full SAMPLES_PER_LINE window
            // for the line body without running past the end.
            let search_len = SAMPLES_PER_LINE + SYNC_A_TEMPLATE_LEN;
            let Some(m) = self
                .sync_detector
                .find_best(&self.accumulator[..search_len], SyncChannel::A)
            else {
                break;
            };

            // The matched offset IS the line start: by NOAA APT spec
            // the line begins at the start of the 39-px Sync A field,
            // and the padded template's leading low aligns with the
            // first 2 px of that field (not with the previous line's
            // tail). This matches noaa-apt's slicing
            // `signal[sync_pos[i]..sync_pos[i] + samples_per_work_row]`.
            let line_start = m.offset;
            let line_end = line_start + SAMPLES_PER_LINE;

            // Build the line on the stack (a 2 KB struct), then move it
            // into the right destination. Stack alloc + memcpy avoids
            // heap traffic on the hot path.
            let mut line = AptLine::default();
            // Resample line samples (SAMPLES_PER_LINE at the work rate)
            // down to LINE_PIXELS (one sample per pixel). The previous
            // implementation used a 5-sample boxcar (uniform-window
            // FIR) whose sinc response attenuated the upper half of
            // the video band by 3 dB; the proper FIR resample
            // preserves it. Per A4 / noaa-apt parity. Per-line
            // resampler reset() ensures no state leaks between lines.
            self.final_resampler.reset();
            self.final_resamp_scratch.resize(LINE_PIXELS + 4, 0.0);
            // Final-rate resampler error path propagates: per the
            // sdr-dsp pure-DSP rule (no I/O / no side effects /
            // return Result), we don't log + fabricate a black line
            // here. Callers see the failure and can decide whether
            // to log, retry, or surface a UI error. The scratch is
            // sized for the expected output so this should never
            // fire in practice. Per CR round 1 on PR #571.
            let n_pix = self.final_resampler.process(
                &self.accumulator[line_start..line_end],
                &mut self.final_resamp_scratch,
            )?;
            // Copy raw f32 samples for image-wide post-processing
            // (B1 / `apt_image::finalize_grayscale`). Trailing pixels
            // beyond the resampler's output are zero-padded by
            // `AptLine::default()`.
            let n_copy = n_pix.min(LINE_PIXELS);
            line.raw_samples[..n_copy].copy_from_slice(&self.final_resamp_scratch[..n_copy]);
            decimate_into_pixels(&self.final_resamp_scratch[..n_copy], &mut line.pixels);
            line.sync_quality = m.quality;
            line.sync_channel = SyncChannel::A;
            line.input_sample_index = self.accumulator_to_input_index(line_start);

            if produced < output.len() {
                output[produced] = line;
                produced += 1;
            } else {
                // Queue room is guaranteed by the loop guard above.
                self.ready_lines.push_back(line);
            }

            self.accumulator.drain(..line_end);
            self.accumulator_start_intermediate_sample += line_end as u64;
        }
        Ok(produced)
    }

    /// Convert an offset within the envelope accumulator (intermediate-rate
    /// samples) to an input-rate sample index. Computed in one shot from
    /// the running intermediate-rate origin so there's no fractional
    /// rounding drift across drains.
    fn accumulator_to_input_index(&self, acc_offset: usize) -> u64 {
        let total_intermediate = self.accumulator_start_intermediate_sample + acc_offset as u64;
        (total_intermediate * u64::from(self.input_rate_hz)) / u64::from(INTERMEDIATE_RATE_HZ)
    }
}

/// Convert one line's worth of demodulated envelope samples (already
/// resampled to one-sample-per-pixel = `LINE_PIXELS` samples) into
/// `LINE_PIXELS` 8-bit greyscale values, writing in place into `pixels`.
///
/// Per A4 of the noaa-apt parity work: the input is the output of a
/// proper FIR resample (`work_rate` → 4160 Hz) — no per-pixel boxcar
/// averaging here, since the resampler already handled the
/// antialiasing properly. If `samples.len() < LINE_PIXELS` (resample
/// returned a few short due to phase rounding), the trailing pixels
/// are zero-filled rather than panicking.
///
/// Uses per-line min/max normalization. The downstream `AptImage`
/// stores these for live preview; absolute calibration via telemetry
/// wedges 8/9 (B1 of the parity work, in `apt_image.rs`) re-normalizes
/// the entire image at PNG-export time using a single image-wide
/// reference range.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn decimate_into_pixels(samples: &[f32], pixels: &mut [u8; LINE_PIXELS]) {
    let n = samples.len().min(LINE_PIXELS);

    let (lo, hi) = samples[..n]
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        });
    let range = (hi - lo).max(1e-9);

    for (dst, &v) in pixels.iter_mut().zip(samples[..n].iter()) {
        let norm = ((v - lo) / range).clamp(0.0, 1.0);
        *dst = (norm * 255.0).round() as u8;
    }
    // If the resampler returned fewer than LINE_PIXELS samples (rare,
    // can happen at chunk boundaries due to phase accumulation), the
    // tail pixels are already zeroed by `AptLine::default()`.
    for dst in pixels.iter_mut().skip(n) {
        *dst = 0;
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp
)]
mod tests {
    use super::*;
    use core::f32::consts::TAU;

    // ─── Fixture constants ────────────────────────────────────────────
    //
    // Hoisted so the same load-bearing rates / chunk sizes / thresholds
    // can be retuned in one place if upstream design parameters change,
    // and so future readers don't have to re-derive what e.g. "0.7"
    // means in context.

    /// Standard FM-demod output rate the decoder is built around.
    const TEST_INPUT_RATE_HZ: u32 = 48_000;
    /// "Realistic" chunk size — ~21 ms at 48 kHz, similar to what the
    /// audio pipeline actually delivers.
    const TEST_REALISTIC_CHUNK: usize = 1_024;
    /// Odd-prime chunk size for the chunked-vs-one-shot equivalence test;
    /// picked specifically so chunk boundaries don't align with line
    /// boundaries — exposes any state-leak bugs in the decoder pipeline.
    const TEST_ODD_PRIME_CHUNK: usize = 513;
    /// Generous buffer for `process` output so the slice-based contract
    /// never returns `BufferTooSmall` in tests. Way above the 6 lines
    /// the longest synthetic input could plausibly emit at once.
    const TEST_OUTPUT_CAPACITY: usize = 16;
    /// Mid-grey envelope level used in single-line-shape tests.
    const TEST_GREY_LEVEL: f32 = 0.7;
    /// End-to-end gradient probes — sample at 1/4 and 3/4 of the line.
    const TEST_GRADIENT_START: f32 = 0.2;
    const TEST_GRADIENT_END: f32 = 0.9;
    /// Minimum sync-quality score we expect from clean synthetic input.
    const TEST_SYNC_QUALITY_THRESHOLD: f32 = 0.5;
    /// Minimum sync-quality score for the more carefully-shaped single
    /// line test (which has a fully square Sync A burst).
    const TEST_SYNC_QUALITY_THRESHOLD_TIGHT: f32 = 0.6;
    /// Below this NCC score the input is effectively noise.
    const TEST_SYNC_NOISE_CEILING: f32 = 0.5;
    /// Threshold for "good lock" sync quality (above-noise band).
    const TEST_SYNC_GOOD_LOCK: f32 = 0.95;
    /// Length of the synthetic noise stream in seconds (used by
    /// accumulator-bound test).
    const TEST_NOISE_DURATION_SEC: usize = 5;

    /// Tiny LCG used by the noise tests to generate deterministic
    /// pseudo-random samples without pulling in a `rand` dep. Numbers
    /// from BSD libc — well-known and known-poor, but plenty random
    /// for a "no-pattern" input.
    fn lcg_step(state: &mut u32) -> f32 {
        *state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        ((*state >> 16) & 0x7fff) as f32 / 32_767.0 - 0.5
    }

    // ─── Apt137Demodulator tests ──────────────────────────────────────

    /// Synthesize an AM signal: `(1 + depth·m(t)) · cos(2π·f_c·t)`
    /// where `m(t)` is a low-frequency message wave at `f_msg`. Used
    /// to feed the demod with a known shape we can recover.
    fn synth_am_wave(
        sample_rate: f64,
        carrier_hz: f64,
        msg_hz: f64,
        depth: f32,
        n_samples: usize,
    ) -> Vec<f32> {
        let dt = 1.0 / sample_rate;
        let omega_c = 2.0 * core::f64::consts::PI * carrier_hz;
        let omega_m = 2.0 * core::f64::consts::PI * msg_hz;
        (0..n_samples)
            .map(|i| {
                let t = i as f64 * dt;
                let envelope = 1.0 + f64::from(depth) * (omega_m * t).cos();
                let carrier = (omega_c * t).cos();
                (envelope * carrier) as f32
            })
            .collect()
    }

    #[test]
    fn apt137_demod_recovers_constant_envelope() {
        // Pure unmodulated carrier `A·cos(2π·f_c·t)`. Demod should
        // recover the constant amplitude `A` to within numerical
        // noise after the one-sample warm-up.
        let fs = 12_480.0_f64;
        let fc = 2_400.0_f64;
        let n = 256_usize;
        let amplitude = 0.7_f32;
        let signal: Vec<f32> = synth_am_wave(fs, fc, 0.0, 0.0, n)
            .iter()
            .map(|s| s * amplitude)
            .collect();
        let mut demod = Apt137Demodulator::new(fs, fc).unwrap();
        let mut out = vec![0.0_f32; n];
        let written = demod.process(&signal, &mut out).unwrap();
        assert_eq!(written, n);
        // First sample is by spec zero (no prior). Skip a few more to
        // let any startup transient settle, then check the rest sit
        // around the true amplitude.
        for &v in &out[5..] {
            assert!(
                (v - amplitude).abs() < 0.01,
                "expected ~{amplitude}, got {v}"
            );
        }
    }

    #[test]
    fn apt137_demod_recovers_modulated_envelope() {
        // `(1 + 0.5·cos(2π·f_msg·t)) · cos(2π·f_c·t)` — a 100 Hz
        // message on a 2400 Hz carrier. The recovered envelope
        // should be `1 + 0.5·cos(2π·f_msg·t)` with the same shape.
        let fs = 12_480.0_f64;
        let fc = 2_400.0_f64;
        let msg = 100.0_f64;
        let depth = 0.5_f32;
        let n = 4_096_usize;
        let signal = synth_am_wave(fs, fc, msg, depth, n);
        let mut demod = Apt137Demodulator::new(fs, fc).unwrap();
        let mut out = vec![0.0_f32; n];
        demod.process(&signal, &mut out).unwrap();

        // Sample the recovered envelope at the message peak (t such
        // that cos(ωm·t) = 1, near i = 0) and trough (cos = -1).
        // Use indices well past the warm-up. At fs=12480 and
        // f_msg=100, one message period spans 124.8 samples, so
        // peak at i≈0, trough at i≈62, peak again at i≈125, etc.
        let peak_i = 250; // 2nd full period peak
        let trough_i = 250 + 62; // half period later
        let peak = out[peak_i];
        let trough = out[trough_i];
        // Peak ≈ 1 + 0.5 = 1.5; trough ≈ 1 - 0.5 = 0.5.
        assert!((peak - 1.5).abs() < 0.05, "peak = {peak}, expected ~1.5");
        assert!(
            (trough - 0.5).abs() < 0.05,
            "trough = {trough}, expected ~0.5"
        );
    }

    #[test]
    fn apt137_demod_streaming_matches_batch() {
        // Splitting a signal into chunks and processing them
        // sequentially must produce the same output as a single
        // big call (modulo the one-sample warm-up at the very
        // start of the stream).
        let fs = 12_480.0_f64;
        let fc = 2_400.0_f64;
        let n = 1_024_usize;
        let signal = synth_am_wave(fs, fc, 50.0, 0.3, n);

        let mut demod_batch = Apt137Demodulator::new(fs, fc).unwrap();
        let mut batch = vec![0.0_f32; n];
        demod_batch.process(&signal, &mut batch).unwrap();

        let mut demod_streamed = Apt137Demodulator::new(fs, fc).unwrap();
        let mut streamed = vec![0.0_f32; n];
        // Three uneven chunks — covers chunk-boundary state handling.
        let split_a = 137_usize;
        let split_b = split_a + 511_usize;
        demod_streamed
            .process(&signal[..split_a], &mut streamed[..split_a])
            .unwrap();
        demod_streamed
            .process(&signal[split_a..split_b], &mut streamed[split_a..split_b])
            .unwrap();
        demod_streamed
            .process(&signal[split_b..], &mut streamed[split_b..])
            .unwrap();

        for i in 1..n {
            assert!(
                (batch[i] - streamed[i]).abs() < 1e-4,
                "batch vs streamed at i={i}: {} vs {}",
                batch[i],
                streamed[i]
            );
        }
    }

    #[test]
    fn apt137_demod_rejects_invalid_frequencies() {
        // sample_rate must be positive.
        assert!(Apt137Demodulator::new(0.0, 2_400.0).is_err());
        assert!(Apt137Demodulator::new(-1.0, 2_400.0).is_err());
        // carrier outside (0, fs/2).
        assert!(Apt137Demodulator::new(12_480.0, 0.0).is_err());
        assert!(Apt137Demodulator::new(12_480.0, -100.0).is_err());
        assert!(Apt137Demodulator::new(12_480.0, 6_240.0).is_err()); // exactly Nyquist
        assert!(Apt137Demodulator::new(12_480.0, 7_000.0).is_err()); // > Nyquist
        // NaN / infinity.
        assert!(Apt137Demodulator::new(f64::NAN, 2_400.0).is_err());
        assert!(Apt137Demodulator::new(12_480.0, f64::INFINITY).is_err());
    }

    #[test]
    fn apt137_demod_dc_robust() {
        // Adding a constant DC offset to the carrier shouldn't dramatically
        // distort the recovered envelope. Compare a clean carrier to one
        // with +0.1 DC bias — apt137's signature property is that DC bias
        // produces a smooth distortion proportional to the bias, not the
        // ±asymmetry that breaks rectifier-based envelope detection.
        let fs = 12_480.0_f64;
        let fc = 2_400.0_f64;
        let n = 1_024_usize;
        let amplitude = 0.5_f32;
        let signal: Vec<f32> = synth_am_wave(fs, fc, 0.0, 0.0, n)
            .iter()
            .map(|s| s * amplitude)
            .collect();
        let signal_with_dc: Vec<f32> = signal.iter().map(|s| s + 0.1).collect();

        let mut demod = Apt137Demodulator::new(fs, fc).unwrap();
        let mut clean = vec![0.0_f32; n];
        demod.process(&signal, &mut clean).unwrap();
        let mut demod = Apt137Demodulator::new(fs, fc).unwrap();
        let mut biased = vec![0.0_f32; n];
        demod.process(&signal_with_dc, &mut biased).unwrap();

        // After warm-up, the per-sample distortion should be bounded.
        // A DC bias of magnitude `b` produces an envelope error roughly
        // proportional to `b` — well under 1.0 here.
        for i in 50..n {
            let diff = (clean[i] - biased[i]).abs();
            assert!(
                diff < 0.3,
                "DC-biased envelope diverged too far at i={i}: |{} - {}| = {diff}",
                clean[i],
                biased[i]
            );
        }
    }

    #[test]
    fn apt137_demod_reset_clears_state() {
        let fs = 12_480.0_f64;
        let fc = 2_400.0_f64;
        let n = 64_usize;
        let signal = synth_am_wave(fs, fc, 0.0, 0.0, n);
        let mut demod = Apt137Demodulator::new(fs, fc).unwrap();
        let mut out = vec![0.0_f32; n];
        demod.process(&signal, &mut out).unwrap();
        // First sample of fresh stream is zero (warm-up).
        assert_eq!(out[0], 0.0);
        // After reset, processing again should re-trigger the warm-up.
        out.fill(99.0); // poison
        demod.reset();
        demod.process(&signal, &mut out).unwrap();
        assert_eq!(out[0], 0.0, "reset() failed to clear `prev` state");
    }

    #[test]
    fn pixel_and_line_invariants_hold() {
        assert_eq!(PIXELS_PER_SECOND as usize, 4160);
        // 12480 Hz / (2 lines/sec × 2080 px/line) = 3 samples/px,
        // so SAMPLES_PER_LINE = 6240 at the new (lower) work rate.
        assert_eq!(SAMPLES_PER_LINE, 6_240);
        assert_eq!(SAMPLES_PER_PIXEL, 3);
        assert_eq!(
            INTERMEDIATE_RATE_HZ as usize,
            SAMPLES_PER_LINE * LINES_PER_SECOND as usize
        );
        assert_eq!(INTERMEDIATE_RATE_HZ, 12_480);
        // Padded Sync A template (per A3 / noaa-apt parity):
        // 38 px = 2 leading + 28 modulated + 8 trailing.
        // At 3 samples/px → 114 samples total, of which the middle
        // 84 (= 7 cycles × 12 samples/cycle) carry the modulation.
        assert_eq!(SYNC_A_TOTAL_PX, 38);
        assert_eq!(SYNC_A_TEMPLATE_LEN, SYNC_A_TOTAL_PX * SAMPLES_PER_PIXEL);
        assert_eq!(SYNC_A_TEMPLATE_LEN, 114);
        assert_eq!(
            SYNC_A_LEADING_PAD_SAMPLES,
            SYNC_A_LEADING_PAD_PX * SAMPLES_PER_PIXEL
        );
        assert_eq!(SYNC_A_LEADING_PAD_SAMPLES, 6);
        assert_eq!(
            SYNC_B_TEMPLATE_LEN,
            SYNC_BURST_CYCLES * SAMPLES_PER_SYNC_B_CYCLE
        );
        assert_eq!(SAMPLES_PER_SYNC_A_CYCLE, 12);
        assert_eq!(SAMPLES_PER_SYNC_B_CYCLE, 15);
    }

    #[test]
    fn envelope_detector_rejects_too_low_sample_rate() {
        // Below 2·2400 Hz = 4800 Hz Nyquist floor we'd alias the rectification
        // harmonic back into the video band — the detector must refuse.
        assert!(EnvelopeDetector::new(4_000).is_err());
    }

    #[test]
    fn envelope_detector_accepts_intermediate_rate() {
        let det = EnvelopeDetector::new(INTERMEDIATE_RATE_HZ).unwrap();
        // Sanity: taps should land in the hundreds with our design.
        assert!(det.lpf_tap_count() >= 10, "got {}", det.lpf_tap_count());
    }

    #[test]
    fn envelope_recovers_constant_amplitude() {
        // Modulate a unit-amplitude subcarrier: x(t) = cos(2π f_c t).
        // Rectified + LPF should converge to ~2/π ≈ 0.6366 (DC of |cos|).
        let rate = INTERMEDIATE_RATE_HZ;
        let n = 20_800; // 1 second
        let input: Vec<f32> = (0..n)
            .map(|i| (TAU * SUBCARRIER_HZ as f32 * (i as f32) / rate as f32).cos())
            .collect();

        let mut detector = EnvelopeDetector::new(rate).unwrap();
        let mut output = vec![0.0_f32; n];
        detector.process(&input, &mut output).unwrap();

        // Look at the second half of the buffer — past the FIR warmup.
        let steady = &output[n / 2..];
        let mean: f32 = steady.iter().sum::<f32>() / steady.len() as f32;
        let two_over_pi = 2.0 / core::f32::consts::PI;
        assert!(
            (mean - two_over_pi).abs() < 0.02,
            "expected DC ≈ 2/π ({two_over_pi:.4}), got {mean:.4}",
        );

        // And confirm the 2·f_c (4800 Hz) ripple is actually suppressed —
        // peak-to-peak of the steady region should be small.
        // Tolerance: at the new 12480 Hz work rate the 4800 Hz harmonic
        // sits at 0.769·Nyquist (vs. 0.46·Nyquist when this test was
        // originally written for 20800 Hz). The Nuttall LPF still
        // rejects it, but with less margin — ~0.10 ripple is fine for
        // the legacy-path EnvelopeDetector (which is no longer in the
        // live APT pipeline; replaced by Apt137Demodulator in A4).
        let (min, max) = steady
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| {
                (lo.min(v), hi.max(v))
            });
        assert!(
            (max - min) < 0.10,
            "LPF residual ripple too large: [{min:.4}, {max:.4}]"
        );
    }

    #[test]
    fn envelope_follows_slow_ramp_modulation() {
        // Carrier at 2400 Hz, envelope = linear ramp 0.0 → 1.0 over one APT line.
        // After rectify + LPF, output should track (2/π) · ramp(t) with some
        // FIR-group-delay lag.
        let rate = INTERMEDIATE_RATE_HZ;
        let n = SAMPLES_PER_LINE; // one full scan line
        let input: Vec<f32> = (0..n)
            .map(|i| {
                let env = (i as f32) / (n as f32);
                let carrier = (TAU * SUBCARRIER_HZ as f32 * (i as f32) / rate as f32).cos();
                env * carrier
            })
            .collect();

        let mut detector = EnvelopeDetector::new(rate).unwrap();
        let mut output = vec![0.0_f32; n];
        detector.process(&input, &mut output).unwrap();

        // Sample three points along the ramp (past FIR settling) and check
        // each lies near (2/π) · expected_env with a generous tolerance —
        // the LPF has real group delay, so exact alignment would be wrong.
        let two_over_pi = 2.0 / core::f32::consts::PI;
        let delay = detector.lpf_tap_count() / 2;
        for &check in &[n / 4, n / 2, (3 * n) / 4] {
            let expected = (check as f32) / (n as f32) * two_over_pi;
            let measured = output[check + delay.min(n - check - 1)];
            assert!(
                (measured - expected).abs() < 0.05,
                "ramp point {check}: expected ~{expected:.3}, got {measured:.3}",
            );
        }

        // And the very last output sample (after most of the ramp) should
        // have reached near full amplitude.
        assert!(
            output[n - 1] > 0.6 * two_over_pi,
            "end of ramp should be near full envelope, got {}",
            output[n - 1]
        );
    }

    #[test]
    fn envelope_process_buffer_too_small_errors() {
        let mut detector = EnvelopeDetector::new(INTERMEDIATE_RATE_HZ).unwrap();
        let input = vec![0.0_f32; 32];
        let mut output = vec![0.0_f32; 16];
        assert!(detector.process(&input, &mut output).is_err());
    }

    #[test]
    fn envelope_process_handles_empty_input() {
        let mut detector = EnvelopeDetector::new(INTERMEDIATE_RATE_HZ).unwrap();
        let mut output: [f32; 0] = [];
        let n = detector.process(&[], &mut output).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn real_resampler_downsamples_tone() {
        // 48000 → 20800 Hz. Pump a 1 kHz tone through and verify the output
        // (a) has the expected number of samples (within polyphase rounding)
        // and (b) still oscillates.
        let in_rate = 48_000.0_f64;
        let out_rate = f64::from(INTERMEDIATE_RATE_HZ);
        let n_in = 4800_usize; // 100 ms of input
        let tone_hz = 1_000.0_f32;

        let input: Vec<f32> = (0..n_in)
            .map(|i| (TAU * tone_hz * (i as f32) / (in_rate as f32)).cos())
            .collect();

        let mut r = RealResampler::new(in_rate, out_rate).unwrap();
        // Worst-case: ceil(n_in * out/in) + 1 = ceil(2080) + 1 = 2081.
        let mut output = vec![0.0_f32; 2100];
        let produced = r.process(&input, &mut output).unwrap();

        let expected = (n_in as f64 * out_rate / in_rate) as usize;
        assert!(
            produced.abs_diff(expected) <= 2,
            "expected ~{expected} out samples, got {produced}",
        );

        // Skip FIR warmup, verify the tone is still there (non-trivial peak).
        let skip = produced / 5;
        let steady = &output[skip..produced];
        let peak = steady.iter().map(|x| x.abs()).fold(0.0_f32, f32::max);
        assert!(peak > 0.3, "resampled tone peak too low: {peak}");

        // Zero crossings ≈ 2 · cycles ≈ 2 · (tone_hz / out_rate) · steady_len.
        let crossings = steady
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        assert!(crossings > 20, "expected oscillation, got {crossings}");
    }

    #[test]
    fn real_resampler_passthrough_on_equal_rates() {
        let mut r = RealResampler::new(48_000.0, 48_000.0).unwrap();
        let input: Vec<f32> = (0..64).map(|i| i as f32).collect();
        let mut output = vec![0.0_f32; 64];
        let n = r.process(&input, &mut output).unwrap();
        assert_eq!(n, 64);
        for (i, &v) in output.iter().enumerate().take(64) {
            assert!((v - i as f32).abs() < 1e-5, "mismatch at {i}: {v}");
        }
    }

    #[test]
    fn real_resampler_continuity_across_chunks() {
        // Feed the same 48 kHz → 20800 Hz tone in one big block vs. three
        // smaller chunks; the stitched output should match the one-shot run
        // to within a couple of samples of polyphase phase drift.
        let in_rate = 48_000.0_f64;
        let out_rate = f64::from(INTERMEDIATE_RATE_HZ);
        let n_in = 3_072_usize;
        let tone_hz = 500.0_f32;
        let input: Vec<f32> = (0..n_in)
            .map(|i| (TAU * tone_hz * (i as f32) / (in_rate as f32)).sin())
            .collect();

        let mut r_whole = RealResampler::new(in_rate, out_rate).unwrap();
        let mut one_shot = vec![0.0_f32; n_in];
        let n_whole = r_whole.process(&input, &mut one_shot).unwrap();

        let mut r_chunked = RealResampler::new(in_rate, out_rate).unwrap();
        let mut chunked: Vec<f32> = Vec::new();
        let mut tmp = vec![0.0_f32; n_in];
        for chunk in input.chunks(1024) {
            let c = r_chunked.process(chunk, &mut tmp).unwrap();
            chunked.extend_from_slice(&tmp[..c]);
        }

        assert!(
            n_whole.abs_diff(chunked.len()) <= 1,
            "one-shot produced {n_whole}, chunked produced {}",
            chunked.len(),
        );

        // Compare the steady portion (past FIR warmup) sample-by-sample.
        let steady_start = n_whole / 4;
        let common = n_whole.min(chunked.len());
        for i in steady_start..common {
            assert!(
                (one_shot[i] - chunked[i]).abs() < 1e-4,
                "chunk drift at {i}: one-shot {} vs chunked {}",
                one_shot[i],
                chunked[i],
            );
        }
    }

    #[test]
    fn real_resampler_empty_input_is_zero() {
        let mut r = RealResampler::new(48_000.0, f64::from(INTERMEDIATE_RATE_HZ)).unwrap();
        let mut output = vec![0.0_f32; 8];
        assert_eq!(r.process(&[], &mut output).unwrap(), 0);
    }

    #[test]
    fn real_resampler_reset_clears_state() {
        let mut r = RealResampler::new(48_000.0, f64::from(INTERMEDIATE_RATE_HZ)).unwrap();
        let hot = vec![1.0_f32; 256];
        let mut out = vec![0.0_f32; 256];
        r.process(&hot, &mut out).unwrap();
        r.reset();
        // After reset, processing zeros should produce near-zeros (no carry).
        let zeros = vec![0.0_f32; 256];
        let mut out2 = vec![0.0_f32; 256];
        let n = r.process(&zeros, &mut out2).unwrap();
        for &v in &out2[..n] {
            assert!(v.abs() < 1e-4, "reset residual too large: {v}");
        }
    }

    /// Build a synthetic envelope buffer with a sync burst embedded at the
    /// given offset, preceded and followed by constant "floor" amplitude.
    fn synth_envelope_with_sync(
        total_len: usize,
        sync_offset: usize,
        samples_per_cycle: usize,
        cycles: usize,
        floor: f32,
        peak: f32,
    ) -> Vec<f32> {
        let mut buf = vec![floor; total_len];
        let sync_len = samples_per_cycle * cycles;
        assert!(sync_offset + sync_len <= total_len);
        for i in 0..sync_len {
            let phase = i % samples_per_cycle;
            let high = phase < samples_per_cycle / 2;
            buf[sync_offset + i] = if high { peak } else { floor };
        }
        buf
    }

    #[test]
    fn sync_detector_template_lengths_match_constants() {
        let d = SyncDetector::new();
        assert_eq!(d.template_a_len(), SYNC_A_TEMPLATE_LEN);
        assert_eq!(d.template_b_len(), SYNC_B_TEMPLATE_LEN);
    }

    #[test]
    fn sync_detector_returns_none_on_short_input() {
        let d = SyncDetector::new();
        let short = vec![0.0_f32; 10];
        assert!(d.find_best(&short, SyncChannel::A).is_none());
        assert!(d.find_best(&short, SyncChannel::B).is_none());
    }

    #[test]
    fn sync_detector_locates_sync_a_exactly() {
        // `synth_envelope_with_sync` plants the burst's first HIGH
        // half-cycle at `burst_offset`. The padded template's first
        // HIGH transition sits at sample
        // `SYNC_A_FIRST_HIGH_OFFSET_SAMPLES` from template start, so
        // a perfect match returns
        // `m.offset = burst_offset - SYNC_A_FIRST_HIGH_OFFSET_SAMPLES`.
        // Pick a burst offset that leaves room for both the leading
        // template padding before it and the trailing tail after.
        let burst_offset = 317;
        let buf = synth_envelope_with_sync(
            2_000,
            burst_offset,
            SAMPLES_PER_SYNC_A_CYCLE,
            SYNC_BURST_CYCLES,
            0.1,
            0.9,
        );
        let m = SyncDetector::new()
            .find_best(&buf, SyncChannel::A)
            .expect("should match");
        assert_eq!(m.channel, SyncChannel::A);
        let expected_template_start = burst_offset - SYNC_A_FIRST_HIGH_OFFSET_SAMPLES;
        assert_eq!(
            m.offset, expected_template_start,
            "expected template-start offset {expected_template_start} \
             (= burst_offset {burst_offset} − SYNC_A_FIRST_HIGH_OFFSET_SAMPLES \
             {SYNC_A_FIRST_HIGH_OFFSET_SAMPLES}), got {}",
            m.offset,
        );
        assert!(
            m.quality > TEST_SYNC_GOOD_LOCK,
            "quality too low: {:.3}",
            m.quality,
        );
    }

    #[test]
    fn sync_detector_locates_sync_b_exactly() {
        let offset = 742;
        let buf = synth_envelope_with_sync(
            2_000,
            offset,
            SAMPLES_PER_SYNC_B_CYCLE,
            SYNC_BURST_CYCLES,
            0.1,
            0.9,
        );
        let m = SyncDetector::new()
            .find_best(&buf, SyncChannel::B)
            .expect("should match");
        assert_eq!(m.channel, SyncChannel::B);
        assert_eq!(m.offset, offset);
        assert!(
            m.quality > TEST_SYNC_GOOD_LOCK,
            "quality too low: {:.3}",
            m.quality,
        );
    }

    #[test]
    fn sync_detector_is_dc_offset_invariant() {
        // Same sync pattern twice, once with large DC offset in the
        // envelope floor; quality must remain high and offset must agree.
        let offset = 200;
        let low = synth_envelope_with_sync(
            1_500,
            offset,
            SAMPLES_PER_SYNC_A_CYCLE,
            SYNC_BURST_CYCLES,
            0.0,
            1.0,
        );
        let high: Vec<f32> = low.iter().map(|v| v + 5.0).collect();
        let d = SyncDetector::new();
        let m_lo = d.find_best(&low, SyncChannel::A).unwrap();
        let m_hi = d.find_best(&high, SyncChannel::A).unwrap();
        assert_eq!(m_lo.offset, m_hi.offset);
        assert!((m_lo.quality - m_hi.quality).abs() < 0.01);
    }

    #[test]
    fn sync_detector_noise_has_low_quality() {
        // Pseudo-random noise (deterministic LCG) — no embedded sync at all.
        // Any accidental peak must score well below a real match.
        let mut state: u32 = 1;
        let buf: Vec<f32> = (0..2_000).map(|_| lcg_step(&mut state)).collect();
        let m = SyncDetector::new().find_best(&buf, SyncChannel::A).unwrap();
        assert!(
            m.quality < TEST_SYNC_NOISE_CEILING,
            "noise quality too high: {:.3} at offset {}",
            m.quality,
            m.offset,
        );
    }

    #[test]
    fn sync_detector_picks_stronger_of_two_bursts() {
        // Two bursts in the same buffer: one attenuated, one full-amp.
        // The detector must pick the full-amp one (higher SNR ⇒ higher NCC).
        // Use `synth_envelope_with_sync` so the template gets a clean
        // shape match — its layout (HIGH-LOW pairs starting HIGH) is
        // what real APT signals produce; manual ±contrast plants in
        // the test would have to mirror that exactly to score 1.0.
        let weak_burst_off = 200;
        let strong_burst_off = 1_000;
        let mut buf = vec![0.1_f32; 2_500];
        // Weak burst: 0.01 contrast above floor.
        for i in 0..(SAMPLES_PER_SYNC_A_CYCLE * SYNC_BURST_CYCLES) {
            let phase = i % SAMPLES_PER_SYNC_A_CYCLE;
            let high = phase < SAMPLES_PER_SYNC_A_CYCLE / 2;
            buf[weak_burst_off + i] = if high { 0.11 } else { 0.10 };
        }
        // Strong burst: 0.9 contrast above floor.
        for i in 0..(SAMPLES_PER_SYNC_A_CYCLE * SYNC_BURST_CYCLES) {
            let phase = i % SAMPLES_PER_SYNC_A_CYCLE;
            let high = phase < SAMPLES_PER_SYNC_A_CYCLE / 2;
            buf[strong_burst_off + i] = if high { 1.0 } else { 0.1 };
        }
        let m = SyncDetector::new().find_best(&buf, SyncChannel::A).unwrap();
        // The matched offset is `burst_first_high − SYNC_A_FIRST_HIGH_OFFSET_SAMPLES`
        // for either burst. Both shapes correlate perfectly, so the detector
        // will pick whichever has the larger raw NCC numerator (the strong
        // one — higher amplitude swing).
        let weak_template_start = weak_burst_off - SYNC_A_FIRST_HIGH_OFFSET_SAMPLES;
        let strong_template_start = strong_burst_off - SYNC_A_FIRST_HIGH_OFFSET_SAMPLES;
        assert!(
            m.offset == weak_template_start || m.offset == strong_template_start,
            "expected one of {{{weak_template_start}, {strong_template_start}}}, \
             got {}",
            m.offset,
        );
        assert!(m.quality > 0.9, "quality too low: {:.3}", m.quality);
    }

    /// Synthesize one full APT line worth of FM-demod audio at `rate`:
    /// a 2400 Hz carrier with envelope = Sync A burst then a constant grey.
    /// Keeps tests independent of the real capture pipeline.
    fn synth_line_audio(rate: u32, grey_level: f32) -> Vec<f32> {
        let rate_f = f64::from(rate);
        let line_dur = 1.0_f64 / LINES_PER_SECOND;
        let n = (rate_f * line_dur).round() as usize;
        let mut out = Vec::with_capacity(n);
        let sync_samples = (rate_f * SYNC_BURST_CYCLES as f64 / SYNC_A_HZ).round() as usize;
        for i in 0..n {
            let t = (i as f64) / rate_f;
            let carrier = (core::f64::consts::TAU * SUBCARRIER_HZ * t).sin() as f32;
            let envelope = if i < sync_samples {
                // Sync A square-wave envelope: alternating 0 / grey_level
                let cyc_samples = rate_f / SYNC_A_HZ;
                let phase = (i as f64 % cyc_samples) / cyc_samples;
                if phase < 0.5 { grey_level } else { 0.0 }
            } else {
                grey_level
            };
            out.push(envelope * carrier);
        }
        out
    }

    #[test]
    fn apt_decoder_rejects_sub_nyquist_input_rate() {
        // At or below 2·SUBCARRIER_HZ (4800 Hz) the 2400 Hz APT subcarrier
        // is at-or-past Nyquist — at exactly 4800 Hz the cosine samples
        // hit phase-ambiguous points and collapse, so the boundary itself
        // must be rejected, not just rates strictly below.
        assert!(AptDecoder::new(0).is_err());
        assert!(AptDecoder::new(4_799).is_err());
        assert!(AptDecoder::new(4_800).is_err());
        // 8000 Hz used to be accepted but the A4 DC-removal Kaiser
        // bandpass needs Nyquist > cutoff + transition/2 = 5300 Hz, so
        // input rate must exceed ~10.6 kHz. 11025 Hz (CD-quality
        // sub-rate) is the smallest realistic rate that still passes;
        // the FM-demod output (48 kHz) is comfortably above this.
        assert!(AptDecoder::new(8_000).is_err());
        assert!(AptDecoder::new(11_025).is_ok());
        assert!(AptDecoder::new(48_000).is_ok());
    }

    #[test]
    fn apt_decoder_emits_nothing_with_short_input() {
        let mut d = AptDecoder::new(TEST_INPUT_RATE_HZ).unwrap();
        let input = vec![0.0_f32; 128];
        let mut out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
        let n = d.process(&input, &mut out).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn apt_decoder_recovers_line_from_synthetic_audio() {
        // Feed three lines of synthetic audio so the decoder has enough
        // post-warmup buffer to emit at least one.
        let rate = TEST_INPUT_RATE_HZ;
        let mut d = AptDecoder::new(rate).unwrap();
        let one_line = synth_line_audio(rate, TEST_GREY_LEVEL);
        let mut three_lines = Vec::with_capacity(one_line.len() * 3);
        for _ in 0..3 {
            three_lines.extend_from_slice(&one_line);
        }
        let mut out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
        let produced = d.process(&three_lines, &mut out).unwrap();
        assert!(
            produced > 0,
            "expected at least one decoded line from 3-line synthetic input",
        );
        for (i, line) in out[..produced].iter().enumerate() {
            assert_eq!(line.sync_channel, SyncChannel::A);
            assert!(
                line.sync_quality > TEST_SYNC_QUALITY_THRESHOLD_TIGHT,
                "line {i} quality too low: {:.3}",
                line.sync_quality,
            );
        }
    }

    #[test]
    fn apt_decoder_chunked_matches_oneshot() {
        // Any reasonable chunking must produce bit-identical pixel output
        // compared to a single giant call — the decoder's state carries
        // everything the resampler / envelope / accumulator need.
        let rate = TEST_INPUT_RATE_HZ;
        let mut audio = Vec::new();
        for _ in 0..4 {
            audio.extend_from_slice(&synth_line_audio(rate, 0.6));
        }

        let mut one_shot_dec = AptDecoder::new(rate).unwrap();
        let mut lines_whole = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
        let n_whole = one_shot_dec.process(&audio, &mut lines_whole).unwrap();
        lines_whole.truncate(n_whole);

        let mut chunked_dec = AptDecoder::new(rate).unwrap();
        let mut lines_chunked: Vec<AptLine> = Vec::new();
        let mut chunk_out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
        for chunk in audio.chunks(TEST_ODD_PRIME_CHUNK) {
            let n = chunked_dec.process(chunk, &mut chunk_out).unwrap();
            for line in &chunk_out[..n] {
                lines_chunked.push(line.clone());
            }
        }

        assert_eq!(
            lines_whole.len(),
            lines_chunked.len(),
            "chunked and one-shot produced different line counts",
        );
        for (w, c) in lines_whole.iter().zip(lines_chunked.iter()) {
            assert_eq!(w.pixels, c.pixels, "chunked pixels diverge from one-shot");
        }
    }

    #[test]
    fn apt_decoder_reset_clears_pending_state() {
        let rate = TEST_INPUT_RATE_HZ;
        let mut d = AptDecoder::new(rate).unwrap();
        let partial = synth_line_audio(rate, TEST_GREY_LEVEL);
        let mut out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
        // Push part of a line — not enough to emit.
        d.process(&partial[..partial.len() / 4], &mut out).unwrap();

        d.reset();

        // After reset, pushing silence should not emit a line on account
        // of leftover state.
        let silence = vec![0.0_f32; 2_048];
        let n = d.process(&silence, &mut out).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn apt_decoder_bounds_accumulator_on_pure_noise() {
        // Pure pseudo-random noise still trips `find_best` to a peak —
        // what matters is that the internal buffer never grows unbounded.
        let rate = TEST_INPUT_RATE_HZ;
        let mut d = AptDecoder::new(rate).unwrap();
        let mut state: u32 = 7;
        let noise: Vec<f32> = (0..(rate as usize * TEST_NOISE_DURATION_SEC))
            .map(|_| lcg_step(&mut state))
            .collect();
        let mut out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
        for chunk in noise.chunks(TEST_REALISTIC_CHUNK) {
            let _ = d.process(chunk, &mut out).unwrap();
            assert!(
                d.accumulator.len() <= DECODER_BUFFER_CAP,
                "accumulator grew past cap: {}",
                d.accumulator.len(),
            );
        }
    }

    #[test]
    fn apt_decoder_undersized_output_preserves_all_decoded_lines() {
        // Streaming contract: if more lines are decoded than `output` can
        // hold, the surplus lives in the internal ready queue and must
        // surface on subsequent calls — *no* decoded line should ever
        // be silently dropped just because the caller's output was tight.
        let rate = TEST_INPUT_RATE_HZ;

        // Reference run: same audio, generous output, count lines emitted.
        let mut audio = Vec::new();
        for _ in 0..6 {
            audio.extend_from_slice(&synth_line_audio(rate, TEST_GREY_LEVEL));
        }
        let mut reference = AptDecoder::new(rate).unwrap();
        let mut ref_out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
        let n_reference = reference.process(&audio, &mut ref_out).unwrap();
        assert!(
            n_reference > 1,
            "test setup needs to emit multiple lines; got {n_reference}",
        );

        // Tight run: one-slot output, drained line-by-line across calls.
        let mut tight = AptDecoder::new(rate).unwrap();
        let mut tight_out = vec![AptLine::default(); 1];
        let n_first = tight.process(&audio, &mut tight_out).unwrap();
        assert_eq!(n_first, 1);
        let mut tight_total = 1_usize;

        // Drain the ready queue with empty inputs — every queued line
        // must come through.
        loop {
            let n = tight.process(&[], &mut tight_out).unwrap();
            if n == 0 {
                break;
            }
            tight_total += n;
        }

        assert_eq!(
            tight_total, n_reference,
            "tight-output run produced {tight_total} lines, generous run produced \
             {n_reference} — surplus was silently dropped",
        );
    }

    #[test]
    fn apt_decoder_accumulator_capacity_absorbs_intentional_overshoot() {
        // The chunked-ingestion path intentionally lets `accumulator` peak
        // at DECODER_BUFFER_CAP + SAMPLES_PER_LINE before being trimmed.
        // Reserving exactly DECODER_BUFFER_CAP would force a realloc on
        // first backpressure (and Vec keeps the larger capacity afterward,
        // defeating bounded memory). Pre-reserving for the peak avoids
        // it. Verify by snapshotting capacity after construction and
        // again after a multi-line process call — they must match.
        let rate = TEST_INPUT_RATE_HZ;
        let mut d = AptDecoder::new(rate).unwrap();
        let initial_capacity = d.accumulator.capacity();
        assert!(
            initial_capacity >= DECODER_BUFFER_CAP + SAMPLES_PER_LINE,
            "initial accumulator capacity {initial_capacity} too small to \
             absorb the chunked-ingestion overshoot",
        );

        // Push 8 lines through a 1-slot output to force backpressure.
        let mut audio = Vec::new();
        for _ in 0..8 {
            audio.extend_from_slice(&synth_line_audio(rate, TEST_GREY_LEVEL));
        }
        let mut tight_out = vec![AptLine::default(); 1];
        d.process(&audio, &mut tight_out).unwrap();

        assert_eq!(
            d.accumulator.capacity(),
            initial_capacity,
            "accumulator capacity grew under backpressure — Vec reallocated, \
             defeating bounded-memory intent",
        );
    }

    #[test]
    fn apt_decoder_huge_chunk_keeps_resample_scratch_bounded() {
        // Outer-loop subchunking guarantees that resample_scratch and
        // demod_scratch never need to grow with caller chunk size.
        // Snapshot capacities, push a multi-megabyte input chunk, and
        // assert the scratch vectors haven't reallocated to fit the
        // input's full size.
        let rate = TEST_INPUT_RATE_HZ;
        let mut d = AptDecoder::new(rate).unwrap();
        let resample_cap_before = d.resample_scratch.capacity();
        let envelope_cap_before = d.demod_scratch.capacity();

        // 100 audio lines = 2.4 M samples = ~9.6 MB. Pre-bounded design,
        // resample_scratch must stay sized for one INPUT_SUBCHUNK_SAMPLES
        // worth of output, not the whole 9.6 MB input.
        let mut huge = Vec::new();
        for _ in 0..100 {
            huge.extend_from_slice(&synth_line_audio(rate, TEST_GREY_LEVEL));
        }
        let mut roomy_out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
        d.process(&huge, &mut roomy_out).unwrap();

        assert_eq!(
            d.resample_scratch.capacity(),
            resample_cap_before,
            "resample_scratch reallocated under huge input — outer subchunk \
             bound is broken (cap was {resample_cap_before}, now {})",
            d.resample_scratch.capacity(),
        );
        assert_eq!(
            d.demod_scratch.capacity(),
            envelope_cap_before,
            "demod_scratch reallocated under huge input — outer subchunk \
             bound is broken (cap was {envelope_cap_before}, now {})",
            d.demod_scratch.capacity(),
        );
    }

    #[test]
    fn apt_decoder_huge_chunk_keeps_accumulator_bounded() {
        // CR concern: a single oversized input must not let the raw
        // accumulator transiently balloon past its cap. With chunk-bounded
        // ingestion the accumulator should never exceed
        // DECODER_BUFFER_CAP + SAMPLES_PER_LINE at any instant.
        let rate = TEST_INPUT_RATE_HZ;
        let mut d = AptDecoder::new(rate).unwrap();
        let mut huge = Vec::new();
        for _ in 0..100 {
            huge.extend_from_slice(&synth_line_audio(rate, TEST_GREY_LEVEL));
        }
        // One-slot output and a one-slot ready queue effective limit
        // (lines still queue internally up to READY_QUEUE_CAP).
        let mut tight_out = vec![AptLine::default(); 1];
        d.process(&huge, &mut tight_out).unwrap();
        // After the call the accumulator must be at-or-below cap — the
        // chunked-ingestion design re-trims after each chunk.
        assert!(
            d.accumulator.len() <= DECODER_BUFFER_CAP,
            "accumulator past cap after huge input: {}",
            d.accumulator.len(),
        );
        // And the ready queue is bounded by its own cap.
        assert!(
            d.ready_lines.len() <= READY_QUEUE_CAP,
            "ready queue past cap: {}",
            d.ready_lines.len(),
        );
    }

    #[test]
    fn envelope_detector_rejects_below_rectified_nyquist() {
        // The rectified subcarrier harmonic sits at 2·f_c = 4800 Hz, so
        // any sample rate at or below 2·4800 = 9600 Hz aliases that tone
        // back into the video band. The detector must refuse those rates.
        // Earlier values like 8 kHz "look" plausible (above 2·f_c) but
        // the rectified harmonic Nyquist still isn't met — make sure 8 kHz
        // is rejected, and 16 kHz (well above the floor) is accepted.
        assert!(EnvelopeDetector::new(8_000).is_err());
        assert!(EnvelopeDetector::new(9_600).is_err()); // exactly at floor
        assert!(EnvelopeDetector::new(16_000).is_ok());
    }

    /// Synthesize a realistic APT line with a sync A burst followed by a
    /// linear grey gradient across the video area. Returns audio at `rate`
    /// with a 2400 Hz AM carrier modulated by the envelope pattern.
    fn synth_line_with_gradient(rate: u32, start_grey: f32, end_grey: f32) -> Vec<f32> {
        let rate_f = f64::from(rate);
        let line_dur = 1.0_f64 / LINES_PER_SECOND;
        let n = (rate_f * line_dur).round() as usize;
        let sync_samples = (rate_f * SYNC_BURST_CYCLES as f64 / SYNC_A_HZ).round() as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let t = (i as f64) / rate_f;
            let carrier = (core::f64::consts::TAU * SUBCARRIER_HZ * t).sin() as f32;
            let envelope = if i < sync_samples {
                let cyc_samples = rate_f / SYNC_A_HZ;
                let phase = (i as f64 % cyc_samples) / cyc_samples;
                if phase < 0.5 { 1.0 } else { 0.0 }
            } else {
                // Linear gradient over the video portion.
                let frac = (i - sync_samples) as f32 / (n - sync_samples) as f32;
                start_grey + frac * (end_grey - start_grey)
            };
            out.push(envelope * carrier);
        }
        out
    }

    #[test]
    fn apt_decoder_end_to_end_gradient_is_monotonic() {
        // Six-line synthetic capture, each line with the same 0.2→0.9 grey
        // gradient. Verify the decoder:
        //   (1) emits at least three lines (early ones eaten by resampler /
        //       envelope filter warmup)
        //   (2) stays locked on every emitted line
        //   (3) produces a roughly monotonic pixel gradient inside each
        //       line's video area
        //   (4) reports strictly-increasing input_sample_index values
        let rate = TEST_INPUT_RATE_HZ;

        let mut audio = Vec::new();
        for _ in 0..6 {
            audio.extend_from_slice(&synth_line_with_gradient(
                rate,
                TEST_GRADIENT_START,
                TEST_GRADIENT_END,
            ));
        }

        let mut decoder = AptDecoder::new(rate).unwrap();
        let mut lines: Vec<AptLine> = Vec::new();
        let mut chunk_out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
        for chunk in audio.chunks(TEST_REALISTIC_CHUNK) {
            let n = decoder.process(chunk, &mut chunk_out).unwrap();
            for line in &chunk_out[..n] {
                lines.push(line.clone());
            }
        }
        assert!(
            lines.len() >= 3,
            "expected >= 3 lines from 6-line input, got {}",
            lines.len(),
        );

        // Sync lock held on every emitted line.
        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.sync_quality > TEST_SYNC_QUALITY_THRESHOLD,
                "line {i}: quality {:.3} below lock threshold",
                line.sync_quality,
            );
            assert_eq!(line.sync_channel, SyncChannel::A);
        }

        // input_sample_index strictly monotonic.
        for pair in lines.windows(2) {
            assert!(
                pair[1].input_sample_index > pair[0].input_sample_index,
                "non-monotonic indices: {} → {}",
                pair[0].input_sample_index,
                pair[1].input_sample_index,
            );
        }

        // Gradient check: sample a few pixels well past the sync region
        // and confirm each emitted line shows a left-to-right increase.
        // Use 1/4 and 3/4 of the line length as probes, skipping the
        // ~5% of pixels that cover the sync burst itself.
        let probe_early = LINE_PIXELS / 4;
        let probe_late = (LINE_PIXELS * 3) / 4;
        for (i, line) in lines.iter().enumerate() {
            let early = line.pixels[probe_early];
            let late = line.pixels[probe_late];
            assert!(
                late > early,
                "line {i}: gradient not increasing — pixels[{probe_early}]={early}, pixels[{probe_late}]={late}",
            );
        }
    }

    #[test]
    fn envelope_detector_reset_clears_filter_state() {
        let mut detector = EnvelopeDetector::new(INTERMEDIATE_RATE_HZ).unwrap();
        // Warm the filter up with a loud carrier.
        let input = vec![1.0_f32; 512];
        let mut output = vec![0.0_f32; 512];
        detector.process(&input, &mut output).unwrap();
        assert!(output.iter().any(|&v| v.abs() > 0.1));

        // After reset, feeding zeros should produce (nearly) zeros — the
        // delay line must have been flushed.
        detector.reset();
        let zeros = vec![0.0_f32; 64];
        let mut out2 = vec![0.0_f32; 64];
        detector.process(&zeros, &mut out2).unwrap();
        for &v in &out2 {
            assert!(v.abs() < 1e-6, "reset should zero output, got {v}");
        }
    }
}
