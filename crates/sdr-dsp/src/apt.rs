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

/// Width of the Sync A *field* in the NOAA APT line format (39 px).
/// Distinct from [`SYNC_A_TOTAL_PX`], which is the matched-filter
/// template width (38 px); image-layout code must use this one — the
/// video band starts at `SYNC_A_FIELD_PX + 47` (#774).
pub const SYNC_A_FIELD_PX: usize = 39;

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

    /// See [`RationalResampler::group_delay_input_samples`].
    #[must_use]
    pub fn group_delay_input_samples(&self) -> usize {
        self.inner.group_delay_input_samples()
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

/// Denominator guard for the normalised cross-correlation: windows
/// whose energy is below this are treated as zero-signal rather than
/// dividing by (nearly) nothing. Shared by `find_best` and
/// `quality_at` so both paths score identically.
const NORMALIZED_CORRELATION_DENOM_GUARD: f32 = 1e-9;

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
        let (template, template_norm) = self.template_for(channel);
        let len = template.len();
        if envelope.len() < len {
            return None;
        }

        let mut best_ncc = f32::NEG_INFINITY;
        let mut best_off = 0_usize;

        // Naive O(N·L) normalized cross-correlation. Good enough at APT
        // sample rates: even a generous 2-line search window is under
        // ~4 M multiplies, negligible at 2 lines/sec.
        for tau in 0..=envelope.len() - len {
            let window = &envelope[tau..tau + len];
            let ncc = normalized_corr(
                window,
                template,
                template_norm,
                NORMALIZED_CORRELATION_DENOM_GUARD,
            );
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

    /// Normalised correlation of the template at one specific offset —
    /// the quality a line would carry if sliced there. `None` when the
    /// template does not fit.
    #[must_use]
    pub fn quality_at(&self, envelope: &[f32], offset: usize, channel: SyncChannel) -> Option<f32> {
        let (template, template_norm) = self.template_for(channel);
        let end = offset.checked_add(template.len())?;
        let window = envelope.get(offset..end)?;
        Some(
            normalized_corr(
                window,
                template,
                template_norm,
                NORMALIZED_CORRELATION_DENOM_GUARD,
            )
            .clamp(0.0, 1.0),
        )
    }

    /// The matched-filter template and its norm for `channel`.
    fn template_for(&self, channel: SyncChannel) -> (&[f32], f32) {
        match channel {
            SyncChannel::A => (self.template_a.as_slice(), self.template_a_norm),
            SyncChannel::B => (self.template_b.as_slice(), self.template_b_norm),
        }
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

/// Sync quality at or above which the decoder considers itself locked
/// onto the line cadence. Real captures sit at 0.7–0.95; pure noise
/// occasionally reaches 0.6 on a single window but cannot sustain it.
const SYNC_LOCK_MIN_QUALITY: f32 = 0.4;

/// While locked, a best match further than this from the expected
/// line start (20 % of a line) is a false peak — video content that
/// happened to correlate, or the *next* line's sync when this one is
/// buried in noise. noaa-apt applies the same idea as a "≥ 0.8 row
/// apart" rule on its sync list; in this streaming decoder the next
/// sync is expected at offset 0, so the rule is a drift bound (#774).
const MAX_SYNC_DRIFT_SAMPLES: usize = SAMPLES_PER_LINE / 5;

/// Consecutive nominal-spacing fallbacks before the lock is dropped and
/// the search becomes unconstrained again (a real gap in reception).
const MAX_NOMINAL_FALLBACKS: u32 = 8;

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
/// Lowest input rate [`AptDecoder::new`] accepts (exclusive). The
/// DC-removing bandpass needs its upper transition edge
/// (`cutoff + transition/2` = 5300 Hz) below Nyquist, which is a
/// stricter floor than the 2·`SUBCARRIER_HZ` sampling requirement.
/// [`AptDecoder::new`] rejects rates at or below this with
/// [`DspError::InvalidParameter`]; before #776 they slipped past the
/// 4800 Hz guard and failed inside the tap designer.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub const MIN_INPUT_RATE_HZ: u32 =
    (2.0 * (DC_BANDPASS_CUTOUT_HZ + DC_BANDPASS_TRANSITION_HZ / 2.0)) as u32;

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
    /// Per-line scratch buffer for the final-resample stage: the
    /// primed slice's output (`LINE_PIXELS` plus the warm-up pixels).
    final_resamp_scratch: Vec<f32>,
    /// `final_resampler` group delay rounded up to a whole pixel, in
    /// intermediate-rate samples. Each line is resampled from
    /// `[start − prime, end + prime)` and the output window is shifted
    /// by `2·prime / SAMPLES_PER_PIXEL`, so the filter is primed with
    /// the true preceding samples and the line lands on its own pixels
    /// instead of `prime/3` px late behind a zero ramp (#774).
    prime: usize,
    /// The last `prime` intermediate-rate samples drained from the
    /// accumulator — the context that precedes `accumulator[0]`.
    history: Vec<f32>,
    /// Scratch for the primed line slice.
    line_ctx: Vec<f32>,
    /// Locked onto the line cadence (see `SYNC_LOCK_MIN_QUALITY`).
    locked: bool,
    /// Consecutive lines sliced at nominal spacing because the best
    /// match drifted too far (see `MAX_SYNC_DRIFT_SAMPLES`).
    nominal_fallbacks: u32,

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
    /// Must be **strictly greater than** [`MIN_INPUT_RATE_HZ`]
    /// (10 600 Hz): the input-rate DC-removing bandpass places its
    /// upper transition edge at 5300 Hz, which has to sit below
    /// Nyquist. That floor already covers the `2 · SUBCARRIER_HZ`
    /// (4800 Hz) sampling requirement for the 2400 Hz subcarrier.
    ///
    /// # Errors
    ///
    /// Returns [`DspError::InvalidParameter`] if `input_rate_hz` is at or
    /// below [`MIN_INPUT_RATE_HZ`]. Propagates other [`DspError`] values
    /// from the underlying resampler, envelope detector, or tap designer.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(input_rate_hz: u32) -> Result<Self, DspError> {
        // The DC-removing bandpass below needs `cutoff + transition/2`
        // (5300 Hz) strictly below Nyquist, which subsumes the
        // 2·SUBCARRIER_HZ = 4800 Hz sampling floor. Rejecting here names
        // the rate; letting the tap designer fail named filter
        // internals and was cached as a session-long error (#776).
        if input_rate_hz <= MIN_INPUT_RATE_HZ {
            return Err(DspError::InvalidParameter(format!(
                "input_rate_hz ({input_rate_hz}) must be > {MIN_INPUT_RATE_HZ} Hz so the \
                 DC-removing bandpass (cutoff {DC_BANDPASS_CUTOUT_HZ} Hz, transition \
                 {DC_BANDPASS_TRANSITION_HZ} Hz) fits below Nyquist",
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

        let dc_bandpass = build_dc_bandpass(input_rate_hz)?;

        let final_resampler =
            RealResampler::new(f64::from(INTERMEDIATE_RATE_HZ), PIXELS_PER_SECOND)?;
        // Round the group delay up to a whole pixel so the output
        // window shift `2·prime / SAMPLES_PER_PIXEL` is exact.
        let prime = final_resampler
            .group_delay_input_samples()
            .div_ceil(SAMPLES_PER_PIXEL)
            * SAMPLES_PER_PIXEL;
        // `drain_accumulator` refills the whole history from the drained
        // line, which needs a line to be at least `prime` long; the
        // resampler's delay is ~150 samples against a 6240-sample line.
        if prime > SAMPLES_PER_LINE {
            return Err(DspError::InvalidParameter(format!(
                "final resampler group delay ({prime}) exceeds a line ({SAMPLES_PER_LINE})"
            )));
        }

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
            final_resampler,
            final_resamp_scratch: Vec::with_capacity(
                LINE_PIXELS + 2 * prime / SAMPLES_PER_PIXEL + 4,
            ),
            prime,
            history: vec![0.0; prime],
            line_ctx: Vec::with_capacity(SAMPLES_PER_LINE + 2 * prime),
            locked: false,
            nominal_fallbacks: 0,
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
        self.history.fill(0.0);
        self.locked = false;
        self.nominal_fallbacks = 0;
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
        while self.accumulator.len() >= MIN_ACCUMULATOR_FOR_DECODE + self.prime {
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
            let m = self.gate_sync_match(m);

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
            // Final-rate resampler error path propagates: per the
            // sdr-dsp pure-DSP rule (no I/O / no side effects /
            // return Result), we don't log + fabricate a black line
            // here. Callers see the failure and can decide whether
            // to log, retry, or surface a UI error. Per CR round 1 on
            // PR #571.
            self.resample_line(line_start, line_end, &mut line)?;
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

            self.drain_accumulator(line_end);
        }
        Ok(produced)
    }

    /// Apply the lock / drift rule to a raw sync match (#774). A
    /// confident match near the expected position (re)locks; a match
    /// that drifted more than `MAX_SYNC_DRIFT_SAMPLES` while locked is
    /// replaced by the nominal line start (offset 0) carrying the
    /// correlation actually measured there, so a line buried in noise
    /// becomes one low-quality row instead of a skipped row that
    /// compresses the image. After `MAX_NOMINAL_FALLBACKS` such rows
    /// the lock is dropped (that line still at nominal spacing) and the
    /// next search is unconstrained again.
    fn gate_sync_match(&mut self, m: SyncMatch) -> SyncMatch {
        if self.locked && m.offset > MAX_SYNC_DRIFT_SAMPLES {
            self.nominal_fallbacks += 1;
            if self.nominal_fallbacks >= MAX_NOMINAL_FALLBACKS {
                // This line still keeps its nominal start; only the
                // *next* search runs unconstrained.
                self.locked = false;
                self.nominal_fallbacks = 0;
            }
            let quality = self
                .sync_detector
                .quality_at(&self.accumulator, 0, SyncChannel::A)
                .unwrap_or(0.0);
            return SyncMatch {
                offset: 0,
                channel: SyncChannel::A,
                quality,
            };
        }
        if m.quality >= SYNC_LOCK_MIN_QUALITY {
            self.locked = true;
            self.nominal_fallbacks = 0;
        }
        m
    }

    /// Resample `accumulator[line_start..line_end]` to `LINE_PIXELS`
    /// pixels with the resampler primed by the true preceding samples
    /// (from `history` when the line starts near the accumulator head)
    /// and the output aligned by the filter's group delay (#774).
    ///
    /// With delay `D = prime` (input samples) and ratio 1/3, output
    /// `y[j]` represents the input at `3j − D` relative to the slice
    /// fed. Feeding `[start − D, end + D)` therefore puts pixel `k` at
    /// `y[k + 2D/3]`, and the first `2D/3` outputs are the filter's
    /// warm-up over the primed context.
    fn resample_line(
        &mut self,
        line_start: usize,
        line_end: usize,
        line: &mut AptLine,
    ) -> Result<(), DspError> {
        let prime = self.prime;
        self.line_ctx.clear();
        let from_history = prime.saturating_sub(line_start);
        if from_history > 0 {
            self.line_ctx
                .extend_from_slice(&self.history[self.history.len() - from_history..]);
        }
        let ctx_start = line_start - (prime - from_history);
        let ctx_end = (line_end + prime).min(self.accumulator.len());
        self.line_ctx
            .extend_from_slice(&self.accumulator[ctx_start..ctx_end]);

        self.final_resampler.reset();
        let max_out = self.line_ctx.len().div_ceil(SAMPLES_PER_PIXEL) + 2;
        self.final_resamp_scratch.resize(max_out, 0.0);
        let n_out = self
            .final_resampler
            .process(&self.line_ctx, &mut self.final_resamp_scratch)?;
        let skip = 2 * prime / SAMPLES_PER_PIXEL;
        let available = n_out.saturating_sub(skip).min(LINE_PIXELS);
        let aligned = &self.final_resamp_scratch[skip..skip + available];
        // Trailing pixels beyond the resampler's output are zero-padded
        // by `AptLine::default()`.
        line.raw_samples[..available].copy_from_slice(aligned);
        decimate_into_pixels(aligned, &mut line.pixels);
        Ok(())
    }

    /// Drop `line_end` samples from the accumulator, keeping the last
    /// `prime` of them as the priming context for the next line.
    /// `line_end >= SAMPLES_PER_LINE >= prime` (checked in `new`), so
    /// the drained span always covers the whole history.
    fn drain_accumulator(&mut self, line_end: usize) {
        let prime = self.prime;
        self.history
            .copy_from_slice(&self.accumulator[line_end - prime..line_end]);
        self.accumulator.drain(..line_end);
        self.accumulator_start_intermediate_sample += line_end as u64;
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

/// Build the input-rate DC-removing bandpass for [`AptDecoder::new`].
fn build_dc_bandpass(input_rate_hz: u32) -> Result<FirFilter, DspError> {
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
    FirFilter::new(dc_bandpass_taps)
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
mod tests;
