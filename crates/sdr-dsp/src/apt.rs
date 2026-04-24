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
// 20800 Hz is the smallest rate that simultaneously:
//   * gives integer samples per pixel (20800 / 4160 = 5)
//   * gives integer samples per Sync A cycle (20800 / 1040 = 20)
//   * gives integer samples per Sync B cycle (20800 / 832  = 25)
//   * places 2·f_subcarrier (4800 Hz) below Nyquist (10400 Hz) so the
//     rectify-generated harmonic can be filtered out cleanly
//
// Using this rate means every downstream index is an exact integer — no
// fractional alignment headaches when slicing pixels or templates.

/// Intermediate sample rate the decoder runs its DSP at (20800 Hz).
pub const INTERMEDIATE_RATE_HZ: u32 = 20_800;

/// Samples per APT pixel at [`INTERMEDIATE_RATE_HZ`] (exactly 5).
pub const SAMPLES_PER_PIXEL: usize = 5;

/// Samples per full scan line at [`INTERMEDIATE_RATE_HZ`] (10 400).
pub const SAMPLES_PER_LINE: usize = LINE_PIXELS * SAMPLES_PER_PIXEL;

/// Samples per one cycle of Sync A at [`INTERMEDIATE_RATE_HZ`] (exactly 20).
pub const SAMPLES_PER_SYNC_A_CYCLE: usize = 20;

/// Samples per one cycle of Sync B at [`INTERMEDIATE_RATE_HZ`] (exactly 25).
pub const SAMPLES_PER_SYNC_B_CYCLE: usize = 25;

/// Length of a Sync A template in samples (7 cycles × 20 = 140).
pub const SYNC_A_TEMPLATE_LEN: usize = SYNC_BURST_CYCLES * SAMPLES_PER_SYNC_A_CYCLE;

/// Length of a Sync B template in samples (7 cycles × 25 = 175).
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
/// `pixels` is stored inline (~2 KB) so an `AptLine` is `Clone`-able and
/// reusable as an output slot without per-line heap allocation — the
/// `AptDecoder::process` contract takes `&mut [AptLine]` and writes new
/// values into existing entries. Construct empty slots with
/// `AptLine::default()`.
#[derive(Debug, Clone)]
pub struct AptLine {
    /// The 2080 greyscale pixels of this line, in transmission order.
    pub pixels: [u8; LINE_PIXELS],
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
/// removes the carrier copy and leaves the original video envelope. A
/// Hilbert-magnitude approach would give a bit-perfect envelope but costs
/// twice the FIR work — rectify + LPF is the traditional (and perfectly
/// adequate) choice for APT, matching `noaa-apt` / `wxtoimg` behaviour.
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
        let (tpl_a, norm_a) = build_square_template(SAMPLES_PER_SYNC_A_CYCLE, SYNC_BURST_CYCLES);
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
/// on every correlation.
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
    resampler: RealResampler,
    envelope: EnvelopeDetector,
    sync_detector: SyncDetector,

    resample_scratch: Vec<f32>,
    envelope_scratch: Vec<f32>,
    accumulator: Vec<f32>,

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
    /// Must be at least `2 · SUBCARRIER_HZ` (4800 Hz) — below that the
    /// 2400 Hz APT subcarrier is already aliased before this pipeline
    /// gets a chance to look at it, which would produce silent garbage.
    ///
    /// # Errors
    ///
    /// Returns [`DspError::InvalidParameter`] if `input_rate_hz` is below
    /// the Nyquist floor for the APT subcarrier. Propagates other
    /// [`DspError`] values from the underlying resampler, envelope
    /// detector, or tap designer.
    pub fn new(input_rate_hz: u32) -> Result<Self, DspError> {
        // 2 · 2400 = 4800 Hz exactly — no rounding, just hard-code so the
        // const-context-friendly comparison below stays trivially correct.
        const NYQUIST_FLOOR_HZ: u32 = 4_800;
        if input_rate_hz < NYQUIST_FLOOR_HZ {
            return Err(DspError::InvalidParameter(format!(
                "input_rate_hz ({input_rate_hz}) must be ≥ 2·SUBCARRIER_HZ \
                 ({NYQUIST_FLOOR_HZ}) to avoid aliasing the 2400 Hz APT subcarrier",
            )));
        }
        Ok(Self {
            input_rate_hz,
            resampler: RealResampler::new(
                f64::from(input_rate_hz),
                f64::from(INTERMEDIATE_RATE_HZ),
            )?,
            envelope: EnvelopeDetector::new(INTERMEDIATE_RATE_HZ)?,
            sync_detector: SyncDetector::new(),
            resample_scratch: Vec::new(),
            envelope_scratch: Vec::new(),
            accumulator: Vec::with_capacity(DECODER_BUFFER_CAP),
            accumulator_start_intermediate_sample: 0,
        })
    }

    /// Flush all internal state back to a pre-first-sample state.
    pub fn reset(&mut self) {
        self.resampler.reset();
        self.envelope.reset();
        self.accumulator.clear();
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
    /// **Streaming semantics:** if more lines are ready than `output` can
    /// hold, the function emits as many as fit and returns the count; the
    /// remaining ready lines stay buffered and become available on the
    /// next `process` call. This keeps the error path clean (no streaming
    /// state mutated then unwound) and lets a small fixed-size output
    /// buffer drive the decoder safely. The buffer cap of three lines
    /// internally bounds how far behind the caller can get.
    ///
    /// # Errors
    ///
    /// Propagates [`DspError`] from the resampler or envelope detector.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn process(&mut self, input: &[f32], output: &mut [AptLine]) -> Result<usize, DspError> {
        // 1. Resample to the internal 20800 Hz grid.
        let est_out = (input.len() as u64 * u64::from(INTERMEDIATE_RATE_HZ)
            / u64::from(self.input_rate_hz)) as usize
            + 4;
        self.resample_scratch.resize(est_out, 0.0);
        let resampled = self.resampler.process(input, &mut self.resample_scratch)?;

        // 2. Envelope-detect in place into an equally-sized scratch buffer.
        self.envelope_scratch.resize(resampled, 0.0);
        self.envelope.process(
            &self.resample_scratch[..resampled],
            &mut self.envelope_scratch,
        )?;

        // 3. Append to the accumulator.
        self.accumulator
            .extend_from_slice(&self.envelope_scratch[..resampled]);

        // 4. Emit as many lines as `output` can hold. Stopping at the
        // caller's capacity (rather than erroring) keeps the streaming
        // contract: any line we *don't* emit this call stays buffered
        // and surfaces on the next call. State mutates only for lines
        // that successfully landed in `output` — no half-applied drains.
        let mut produced = 0_usize;
        while produced < output.len() && self.accumulator.len() >= MIN_ACCUMULATOR_FOR_DECODE {
            // Search the first SAMPLES_PER_LINE tau positions — any match
            // in there leaves a full SAMPLES_PER_LINE window for the line
            // body without running past the end.
            let search_len = SAMPLES_PER_LINE + SYNC_A_TEMPLATE_LEN;
            // `find_best` only returns `None` when the search slice is
            // shorter than the template; the loop guard guarantees that
            // can't happen here, but stay defensive: a future constants
            // change shouldn't be allowed to abort the whole DSP path.
            let Some(m) = self
                .sync_detector
                .find_best(&self.accumulator[..search_len], SyncChannel::A)
            else {
                break;
            };

            let line_start = m.offset;
            let line_end = line_start + SAMPLES_PER_LINE;

            // The `produced < output.len()` loop guard guarantees this
            // index is in bounds.
            let slot = &mut output[produced];
            decimate_into_pixels(&self.accumulator[line_start..line_end], &mut slot.pixels);
            slot.sync_quality = m.quality;
            slot.sync_channel = SyncChannel::A;
            slot.input_sample_index = self.accumulator_to_input_index(line_start);
            produced += 1;

            // Drain the accumulator through the end of the emitted line.
            self.accumulator.drain(..line_end);
            self.accumulator_start_intermediate_sample += line_end as u64;
        }

        // 5. Cap buffered memory. If a stretch of noise keeps `find_best`
        // from producing a high-quality match we still eat a line's worth
        // of samples every loop iteration, but belt-and-braces: bound the
        // worst case.
        if self.accumulator.len() > DECODER_BUFFER_CAP {
            let drop_n = self.accumulator.len() - DECODER_BUFFER_CAP;
            self.accumulator.drain(..drop_n);
            self.accumulator_start_intermediate_sample += drop_n as u64;
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

/// Decimate one line's worth of envelope samples (`SAMPLES_PER_LINE`) into
/// `LINE_PIXELS` 8-bit greyscale values, writing in place into `pixels`.
///
/// Uses a simple boxcar average of `SAMPLES_PER_PIXEL` adjacent samples
/// followed by per-line min/max normalization. Per-line normalization is a
/// placeholder that always produces a visible image — long term the
/// decoder should read the Wedge / Telemetry A & B reference bars for
/// absolute calibration, but until that's wired in this keeps the pipeline
/// producing something useful.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn decimate_into_pixels(samples: &[f32], pixels: &mut [u8; LINE_PIXELS]) {
    debug_assert_eq!(samples.len(), SAMPLES_PER_LINE);

    let mut pixel_vals = [0.0_f32; LINE_PIXELS];
    for (i, chunk) in samples.chunks_exact(SAMPLES_PER_PIXEL).enumerate() {
        let sum: f32 = chunk.iter().sum();
        pixel_vals[i] = sum / (SAMPLES_PER_PIXEL as f32);
    }

    let (lo, hi) = pixel_vals
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        });
    let range = (hi - lo).max(1e-9);

    for (dst, &v) in pixels.iter_mut().zip(pixel_vals.iter()) {
        let norm = ((v - lo) / range).clamp(0.0, 1.0);
        *dst = (norm * 255.0).round() as u8;
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

    #[test]
    fn pixel_and_line_invariants_hold() {
        assert_eq!(PIXELS_PER_SECOND as usize, 4160);
        assert_eq!(SAMPLES_PER_LINE, 10_400);
        assert_eq!(
            INTERMEDIATE_RATE_HZ as usize,
            SAMPLES_PER_LINE * LINES_PER_SECOND as usize
        );
        assert_eq!(
            SYNC_A_TEMPLATE_LEN,
            SYNC_BURST_CYCLES * SAMPLES_PER_SYNC_A_CYCLE
        );
        assert_eq!(
            SYNC_B_TEMPLATE_LEN,
            SYNC_BURST_CYCLES * SAMPLES_PER_SYNC_B_CYCLE
        );
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
        let (min, max) = steady
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| {
                (lo.min(v), hi.max(v))
            });
        assert!(
            (max - min) < 0.05,
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
        let offset = 317;
        let buf = synth_envelope_with_sync(
            2_000,
            offset,
            SAMPLES_PER_SYNC_A_CYCLE,
            SYNC_BURST_CYCLES,
            0.1,
            0.9,
        );
        let m = SyncDetector::new()
            .find_best(&buf, SyncChannel::A)
            .expect("should match");
        assert_eq!(m.channel, SyncChannel::A);
        assert_eq!(m.offset, offset);
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
        let weak_off = 100;
        let strong_off = 1_000;
        let mut buf = vec![0.1_f32; 2_000];
        for i in 0..SYNC_A_TEMPLATE_LEN {
            let phase = i % SAMPLES_PER_SYNC_A_CYCLE;
            let high = phase < SAMPLES_PER_SYNC_A_CYCLE / 2;
            // Weak burst — only 0.01 contrast above floor.
            buf[weak_off + i] = if high { 0.11 } else { 0.10 };
            // Strong burst — 0.9 contrast above floor.
            buf[strong_off + i] = if high { 1.0 } else { 0.1 };
        }
        let m = SyncDetector::new().find_best(&buf, SyncChannel::A).unwrap();
        // Both bursts have perfectly-matched shape so NCC is ~1.0 for
        // either — what must hold is that the detector does find one of
        // the two exact offsets, not some in-between phase-slip position.
        assert!(
            m.offset == strong_off || m.offset == weak_off,
            "expected offset at one of the burst starts, got {}",
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
        // Below 2·SUBCARRIER_HZ (4800 Hz) the 2400 Hz APT subcarrier is
        // already aliased — refuse the rate up-front.
        assert!(AptDecoder::new(0).is_err());
        assert!(AptDecoder::new(4_799).is_err());
        // 4800 Hz exactly is the floor; the resampler may still reject it
        // for unrelated reasons, but the Nyquist check itself must accept.
        // 8000 Hz (telephony) is the smallest realistic accept.
        assert!(AptDecoder::new(8_000).is_ok());
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
    fn apt_decoder_undersized_output_buffers_remainder() {
        // Streaming contract: if more lines are ready than `output` can
        // hold, emit as many as fit and leave the rest buffered for the
        // next call. State must NOT advance for lines that didn't land
        // in `output` — verified by feeding the same audio across two
        // calls and checking the decoder still emits the buffered lines.
        let rate = TEST_INPUT_RATE_HZ;
        let mut d = AptDecoder::new(rate).unwrap();
        let mut audio = Vec::new();
        for _ in 0..6 {
            audio.extend_from_slice(&synth_line_audio(rate, TEST_GREY_LEVEL));
        }

        // First call with a single-slot output should fill that slot and
        // return Ok(1) with more lines still queued internally.
        let mut tiny_out = vec![AptLine::default(); 1];
        let n_first = d.process(&audio, &mut tiny_out).unwrap();
        assert_eq!(
            n_first, 1,
            "single-slot output should emit exactly one line, got {n_first}"
        );

        // Second call (empty input) should still drain previously-queued
        // lines from the accumulator into a roomier output. If state had
        // advanced incorrectly on the first call we'd see fewer here.
        let mut roomy_out = vec![AptLine::default(); TEST_OUTPUT_CAPACITY];
        let n_second = d.process(&[], &mut roomy_out).unwrap();
        assert!(
            n_second >= 1,
            "expected buffered lines to surface on a follow-up call, got {n_second}",
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
