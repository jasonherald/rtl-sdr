//! Per-channel receive chain and the crate's public runtime API.
//!
//! One wideband IQ stream in, decoded packets out, one independent receiver
//! per requested downlink channel. The chain each channel runs is:
//!
//! 1. **NCO mix** — multiply by `exp(−j2π(f_ch − f_center)t)` to bring the
//!    channel to DC. The phase is an `f64` accumulator wrapped modulo 2π and
//!    carried across `process` calls, so block boundaries introduce no phase
//!    discontinuity.
//! 2. **[`RationalResampler`]** down to [`CHANNEL_SAMPLE_RATE_HZ`], optionally
//!    behind an integer pre-decimation stage ([`plan_resampling`], engaged only
//!    for source rates that defeat a direct chain — the Airspy's 2.5 / 5 /
//!    10 Msps among them). The final stage is also the channel filter: its
//!    lowpass is `min(in, out) / 2 = 9.6 kHz`
//!    with a ~960 Hz transition, so a neighbour at the real 20 kHz Orbcomm
//!    channel spacing lands squarely in the Nuttall stopband.
//! 3. **[`Fll`]** — coarse carrier-frequency correction, see its docs. Brings
//!    the ±3.5 kHz of 137 MHz Doppler inside the demodulator's ±800 Hz
//!    residual contract.
//! 4. **[`SdpskDemod`]** → **[`Deframer`]** → [`parse_packet`] →
//!    **[`Reassembler`]** (fed the raw bytes of Message packets only).
//!
//! Channels that do not fit inside the source span are kept as stubs so the
//! stats vector still lines up with the caller's channel list, but they build
//! no DSP state and ignore input entirely.
//!
//! Pure decode: no I/O, no threads. `process` allocates only when a block is
//! larger than any seen before (the scratch buffers grow) or when a packet is
//! actually produced.

use sdr_dsp::multirate::RationalResampler;
use sdr_types::Complex;
use tracing::warn;

use crate::deframe::{DeframedPacket, Deframer};
use crate::demod::SdpskDemod;
use crate::packet::{PacketType, parse_packet};
use crate::reassembly::{CompletedMessage, DEFAULT_MAX_AGE_PACKETS, Reassembler};
use crate::{CHANNEL_SAMPLE_RATE_HZ, OrbcommError, packet::OrbcommPacket};

/// Something a channel produced, tagged with the channel it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct OrbcommEvent {
    /// Downlink channel centre frequency, Hz.
    pub channel_hz: f64,
    /// What happened.
    pub kind: OrbcommEventKind,
}

/// The two things a channel can produce.
#[derive(Clone, Debug, PartialEq)]
pub enum OrbcommEventKind {
    /// A checksum-valid packet was deframed and parsed.
    Packet {
        /// The parsed packet.
        packet: OrbcommPacket,
        /// `true` when it only passed after single-bit repair.
        repaired: bool,
    },
    /// A multi-packet subscriber message finished reassembling.
    MessageComplete {
        /// Concatenated fragment payloads.
        bytes: Vec<u8>,
        /// `true` when the message was flushed early (missing fragments).
        partial: bool,
    },
}

/// Per-channel counters, in the order the channels were requested.
#[derive(Clone, Debug)]
pub struct ChannelStats {
    /// The channel's centre frequency, Hz.
    pub freq_hz: f64,
    /// `false` when the channel falls outside the source's span; such a
    /// channel ignores input and never produces events.
    pub in_span: bool,
    /// Packets deframed and parsed successfully.
    pub packets_ok: u64,
    /// Locked strides rejected by the checksum (and by single-bit repair).
    pub checksum_fail: u64,
    /// Packets that only passed after single-bit repair. A subset of
    /// [`Self::packets_ok`] — a repair that did not yield a parsable packet is
    /// not counted here.
    pub repaired: u64,
}

/// Half the per-channel bandwidth kept by the decimation chain.
const CHANNEL_HALF_BANDWIDTH_HZ: f64 = CHANNEL_SAMPLE_RATE_HZ / 2.0;

/// Channel samples averaged into one frequency-error estimate. 256 samples is
/// 64 symbols — long enough for the ±90° modulation to average out, short
/// enough for a 75 Hz update rate.
const FLL_BLOCK_SAMPLES: usize = 256;
/// Fraction of each block's measured error folded into the NCO frequency.
/// With [`FLL_BLOCK_SAMPLES`] this puts the loop bandwidth at
/// `FLL_GAIN · 19200 / 256 / 2π ≈ 6 Hz`.
const FLL_GAIN: f64 = 0.5;
/// Clamp on the integrated correction, set just inside the loop's practical
/// capture range (see [`Fll`]). Past ±6.2 kHz the offset has pushed the
/// signal's upper sideband into the channel filter's stopband *before* the
/// discriminator sees it, so the loop cannot usefully occupy that state — and
/// no real Doppler at 137 MHz asks it to.
const FLL_MAX_OFFSET_HZ: f64 = 6000.0;
/// Slack added to a computed resampler output length. See
/// [`resample_capacity_for`] for why the computed part suffices.
const RESAMPLER_OUTPUT_MARGIN: usize = 16;

/// Lowest rate [`plan_resampling`] will pre-decimate to, as a multiple of
/// [`CHANNEL_SAMPLE_RATE_HZ`]. Pre-decimation happens *after* the NCO has
/// centred the channel at DC, so the only thing the intermediate Nyquist has
/// to clear is the ±9.6 kHz the channel itself occupies; 5× puts it at
/// ±48 kHz, five times the width that matters.
const MIN_INTERMEDIATE_RATE_MULTIPLE: f64 = 5.0;
/// Lowest intermediate rate [`plan_resampling`] will pre-decimate to, Hz.
const MIN_INTERMEDIATE_RATE_HZ: f64 = MIN_INTERMEDIATE_RATE_MULTIPLE * CHANNEL_SAMPLE_RATE_HZ;
/// Largest pre-decimation factor [`plan_resampling`] will consider. Bounds
/// the search on an absurd source rate; 1024 already covers a 98 Msps front
/// end at [`MIN_INTERMEDIATE_RATE_HZ`].
const MAX_PREDECIMATION: u32 = 1024;
/// Tolerance for "this rate is a whole number of hertz". Sample rates are
/// integers in every source this crate is fed from, and
/// [`RationalResampler`] rounds its rates to integers before reducing them,
/// so a candidate intermediate rate that is not whole would silently convert
/// to a slightly different rate than the next stage assumes.
const WHOLE_HERTZ_TOLERANCE: f64 = 1e-6;

/// `true` when `rate_hz` is a whole number of hertz.
fn is_whole_hertz(rate_hz: f64) -> bool {
    rate_hz.is_finite() && (rate_hz - rate_hz.round()).abs() < WHOLE_HERTZ_TOLERANCE
}

/// Output slots needed to resample `input_len` samples at `out_per_in`.
///
/// [`RationalResampler`] refuses a short output buffer *after* its
/// pre-decimation stage has already consumed the input, so a retry would
/// double-process the stream — the buffer has to be right the first time.
/// Its worst case is `ceil(input_len / r) · interp / decim + 2` slots,
/// where `r` is the power-of-two pre-decimation and `interp / decim` the
/// polyphase ratio at the pre-decimated rate. Since `r ≤ in/out`,
/// `r · out / in ≤ 1`, that is at most `input_len · out/in + 3`;
/// [`RESAMPLER_OUTPUT_MARGIN`] covers the rounding and then some.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn resample_capacity_for(input_len: usize, out_per_in: f64) -> usize {
    (input_len as f64 * out_per_in).ceil() as usize + RESAMPLER_OUTPUT_MARGIN
}

/// One channel's integer pre-decimation stage.
struct PreDecimation {
    stage: RationalResampler,
    /// Output samples produced per input sample: `intermediate / source`.
    out_per_in: f64,
    /// Stage output, reused across calls.
    buf: Vec<Complex>,
}

/// How one channel gets from the source rate down to
/// [`CHANNEL_SAMPLE_RATE_HZ`].
struct ResamplePlan {
    /// Integer pre-decimation ahead of [`Self::resampler`]. `None` in the
    /// common case, where the source rate reaches 19.2 kHz in one stage.
    predecim: Option<PreDecimation>,
    /// The stage that lands on [`CHANNEL_SAMPLE_RATE_HZ`].
    resampler: RationalResampler,
    /// The rate feeding [`Self::resampler`] — the source rate when there is
    /// no pre-decimation, the intermediate rate when there is.
    resampler_in_rate_hz: f64,
}

/// Choose the decimation chain for `source_rate_hz` → 19.2 kHz.
///
/// # Why a pre-decimation stage exists at all
///
/// [`RationalResampler`] first decimates by the largest power of two below
/// `in / out`, then reduces the *remaining* ratio by GCD. For 2.5 Msps that
/// pre-decimation is 128, leaving an intermediate rate of 19 531.25 Hz — not
/// a whole number, so the GCD reduction sees `19531 / 19200`, which is
/// coprime. The polyphase prototype is then designed at
/// `19531.25 × 19200 = 375 MHz` with a 960 Hz transition, i.e. 1 484 375 taps,
/// and construction fails outright. The same collapse hits 5 and 10 Msps
/// (identical intermediate rate, larger power-of-two pre-decimation) — every
/// native Airspy R2 rate.
///
/// # The plan
///
/// The direct single-stage chain is tried first, so every rate that already
/// worked keeps the exact chain it had — notably the RTL-SDR rates and the
/// 1.2288 Msps reference captures, where `1228800 / 64 = 19200` reduces to a
/// pure power-of-two decimation with no polyphase stage at all.
///
/// Only when that fails does the search run: the largest integer `D` with
/// `source / D ≥ MIN_INTERMEDIATE_RATE_HZ` and `source / D` a whole number of
/// hertz, for which *both* `source → source/D` and `source/D → 19.2 kHz`
/// construct. Largest-first picks the cheapest chain, since the polyphase
/// cost of both stages scales with their output rates. At 2.5 / 5 / 10 Msps
/// it settles on `D = 25 / 50 / 100` — a 100 kHz intermediate in all three
/// cases, whose Nyquist (±50 kHz) clears the ±9.6 kHz the channel occupies
/// by 5×.
///
/// # Anti-aliasing
///
/// The pre-decimation stage is itself a [`RationalResampler`], so it carries
/// the same windowed-sinc lowpass every other stage does — at
/// `min(in, out) / 2`, i.e. 50 kHz for the 100 kHz intermediate, with a 5 kHz
/// transition. The NCO has already centred the wanted channel at DC, so the
/// only content that can fold into the ±9.6 kHz that survives stage 2 sits at
/// least 40 kHz into that filter's stopband — eight transition widths past
/// cutoff, where the Nuttall window is far below its −93 dB peak sidelobe.
/// Adjacent-channel rejection is unchanged either way: it is set by stage 2's
/// 9.6 kHz lowpass, which every plan ends with.
fn plan_resampling(source_rate_hz: f64) -> Result<ResamplePlan, OrbcommError> {
    let direct_err = match RationalResampler::new(source_rate_hz, CHANNEL_SAMPLE_RATE_HZ) {
        Ok(resampler) => {
            return Ok(ResamplePlan {
                predecim: None,
                resampler,
                resampler_in_rate_hz: source_rate_hz,
            });
        }
        Err(error) => error,
    };

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let max_d = if source_rate_hz.is_finite() && source_rate_hz > 0.0 {
        ((source_rate_hz / MIN_INTERMEDIATE_RATE_HZ).floor() as u32).min(MAX_PREDECIMATION)
    } else {
        0
    };
    for d in (2..=max_d).rev() {
        let intermediate_hz = source_rate_hz / f64::from(d);
        if !is_whole_hertz(intermediate_hz) {
            continue;
        }
        let Ok(stage) = RationalResampler::new(source_rate_hz, intermediate_hz) else {
            continue;
        };
        let Ok(resampler) = RationalResampler::new(intermediate_hz, CHANNEL_SAMPLE_RATE_HZ) else {
            continue;
        };
        return Ok(ResamplePlan {
            predecim: Some(PreDecimation {
                stage,
                out_per_in: intermediate_hz / source_rate_hz,
                buf: Vec::new(),
            }),
            resampler,
            resampler_in_rate_hz: intermediate_hz,
        });
    }
    Err(direct_err.into())
}

/// Wrap a phase accumulator into `[0, 2π)`.
///
/// Both callers step by strictly less than 2π per sample, so a single
/// conditional fold is exact — and, unlike `rem_euclid`, it keeps the
/// accumulator's low bits identical no matter how the stream is blocked.
fn wrap_phase(phase: f64) -> f64 {
    if phase >= std::f64::consts::TAU {
        phase - std::f64::consts::TAU
    } else if phase < 0.0 {
        phase + std::f64::consts::TAU
    } else {
        phase
    }
}

/// Coarse carrier-frequency correction for one channel.
///
/// # Why a frequency-locked loop, and why this discriminator
///
/// Doppler at 137 MHz reaches ±3.5 kHz, and the demodulator's delay-conjugate
/// detector turns a residual offset into a fixed per-symbol phase bias that
/// destroys detection past ±1200 Hz (its contract is ±800 Hz). Something has
/// to close that gap without an FFT.
///
/// The discriminator is the argument of the **mean** delay-conjugate product,
///
/// ```text
/// err = arg( Σ s[n] · conj(s[n−1]) ) · fs / 2π
/// ```
///
/// taken over [`FLL_BLOCK_SAMPLES`] of the *already corrected* stream. For
/// balanced SDPSK the per-sample phase increment contributed by the modulation
/// is symmetric about zero, so its expectation is real and positive and
/// contributes no argument; what is left is the residual carrier rotation.
/// Summing the complex products before taking the argument (rather than
/// averaging per-sample arguments) is what makes this work in practice:
/// amplitude nulls in the RRC-shaped waveform, where the instantaneous phase
/// slews fastest and wraps, are weighted by their own near-zero amplitude
/// instead of dominating the mean.
///
/// # Capture range
///
/// The discriminator itself is unambiguous for `|Δf| < fs/2 = 9.6 kHz`, but
/// that is **not** the binding limit: the FLL sits *downstream* of the
/// resampler, whose lowpass cuts off at `min(in, out) / 2 = 9.6 kHz` with a
/// ~960 Hz Nuttall transition. The RRC-shaped signal is
/// `±(SYMBOL_RATE/2)(1 + α) = ±3.36 kHz` wide, so an offset only stays fully
/// inside the passband while
///
/// ```text
/// |Δf| + 3360 ≤ 9600   ⇒   |Δf| ≲ 6.2 kHz
/// ```
///
/// Past that the upper sideband is attenuated before the discriminator ever
/// sees it. **Practical capture is therefore ≈ ±6.2 kHz — about 1.8× the
/// ±3.5 kHz worst-case Doppler at 137 MHz**, not the 2.7× the raw ambiguity
/// limit would suggest. [`FLL_MAX_OFFSET_HZ`] clamps the integrator just
/// inside it. That is still enough margin to need no acquisition sweep and no
/// squaring or 4th-power nonlinearity; if a future front end pushes the
/// initial offset past ~6 kHz (a large uncorrected LO error stacked on
/// Doppler), the fix is a wider intermediate rate, not a bigger clamp.
///
/// # Loop
///
/// `freq_hz` is a pure integrator: `freq += FLL_GAIN · err` once per block.
/// A first-order loop is enough because Doppler *rate* on a LEO pass is a few
/// tens of Hz/s — three orders of magnitude below the ~6 Hz loop bandwidth —
/// so the velocity lag is negligible. Convergence from a cold start is
/// geometric with ratio `1 − FLL_GAIN`; measured on a 4096-symbol random
/// stream at a 3.5 kHz offset, the residual runs
/// `3500 → 1867 → 878 → 465 → 269 → 197 …` Hz per block, i.e. inside the
/// demodulator's ±800 Hz contract after ~1024 channel samples (53 ms, 256
/// symbols) and settling into a ±300 Hz jitter band thereafter. That band is
/// data-dependent: the ±90° increments only average to zero over *balanced*
/// data, and a 64-symbol window is short enough to see imbalance. It is well
/// inside the contract, and widening the window to shrink it would slow
/// acquisition, which costs more bits than the jitter does.
///
/// The accumulator and block counter are members, not locals, so the loop
/// behaves identically no matter how the sample stream is chopped into
/// `process` calls.
struct Fll {
    /// Integrated frequency estimate, Hz.
    freq_hz: f64,
    /// NCO phase, radians in `[0, 2π)`.
    phase: f64,
    /// Previous *corrected* sample, the delay-conjugate reference.
    prev: Option<Complex>,
    /// Running Σ `s[n]·conj(s[n−1])` for the block in progress.
    acc_re: f64,
    /// Imaginary half of [`Self::acc_re`].
    acc_im: f64,
    /// Products accumulated into the current block.
    count: usize,
}

impl Fll {
    fn new() -> Self {
        Self {
            freq_hz: 0.0,
            phase: 0.0,
            prev: None,
            acc_re: 0.0,
            acc_im: 0.0,
            count: 0,
        }
    }

    /// Derotate `samples` in place by the current estimate, measuring the
    /// residual as it goes.
    #[allow(clippy::cast_possible_truncation)]
    fn process(&mut self, samples: &mut [Complex]) {
        for sample in samples.iter_mut() {
            let step = -std::f64::consts::TAU * self.freq_hz / CHANNEL_SAMPLE_RATE_HZ;
            let (sin, cos) = self.phase.sin_cos();
            let corrected = *sample * Complex::new(cos as f32, sin as f32);
            *sample = corrected;
            self.phase = wrap_phase(self.phase + step);

            if let Some(prev) = self.prev {
                let d = corrected * prev.conj();
                self.acc_re += f64::from(d.re);
                self.acc_im += f64::from(d.im);
                self.count += 1;
            }
            self.prev = Some(corrected);

            if self.count >= FLL_BLOCK_SAMPLES {
                self.update();
            }
        }
    }

    /// Fold the finished block's error into the frequency estimate.
    fn update(&mut self) {
        let err_hz =
            self.acc_im.atan2(self.acc_re) * CHANNEL_SAMPLE_RATE_HZ / std::f64::consts::TAU;
        // A non-finite sample poisons the accumulator; `atan2` then yields NaN,
        // which would freeze the loop (`clamp` on a NaN keeps the NaN) and put
        // a NaN phase on every later output sample. Drop such a block instead —
        // including its reference sample, so a poisoned value cannot leak into
        // the next block's first product.
        //
        // Note this is all-or-nothing: *one* bad sample discards the whole
        // block of 256 products, costing ~13 ms of tracking. That is deliberate
        // — the frequency estimate is only meaningful over a full averaging
        // window, and salvaging a partial block would silently shorten it (and
        // with it the modulation averaging the discriminator depends on) at
        // exactly the moment the data is least trustworthy. The loop simply
        // coasts at its current estimate and resumes on the next clean block,
        // which `fll_survives_non_finite_samples` pins.
        if err_hz.is_finite() {
            self.freq_hz =
                (self.freq_hz + FLL_GAIN * err_hz).clamp(-FLL_MAX_OFFSET_HZ, FLL_MAX_OFFSET_HZ);
        } else {
            self.prev = None;
        }
        self.acc_re = 0.0;
        self.acc_im = 0.0;
        self.count = 0;
    }
}

/// The DSP state one in-span channel owns.
struct ChannelDsp {
    /// Source-rate NCO phase, radians in `[0, 2π)`.
    nco_phase: f64,
    /// Source-rate NCO phase increment per sample, radians.
    nco_step: f64,
    /// Integer pre-decimation ahead of [`Self::resampler`], see
    /// [`plan_resampling`]. `None` for every rate that reaches 19.2 kHz in
    /// one stage.
    predecim: Option<PreDecimation>,
    /// Output samples produced per input sample by [`Self::resampler`], at
    /// *its* input rate (the intermediate rate when pre-decimation is on).
    out_per_in: f64,
    resampler: RationalResampler,
    fll: Fll,
    demod: SdpskDemod,
    deframer: Deframer,
    reassembler: Reassembler,
    /// Resampler output, reused across calls.
    decimated: Vec<Complex>,
    /// Demodulated bits for the current block, reused across calls.
    bits: Vec<bool>,
    /// Packets the deframer emitted for the current bit, reused across
    /// calls. A single bit can yield two: the one that confirms an
    /// acquisition also releases the candidate it confirmed.
    frames: Vec<DeframedPacket>,
    /// Messages the reassembler emitted for the current Message packet,
    /// reused across calls — the [`Reassembler::push`] sink-style
    /// counterpart to `frames` above. A single push can yield two: a flush
    /// of a superseded/stale sequence and the immediate completion of the
    /// fragment that triggered it. Draining both from `out` in the same
    /// call (rather than a one-`Option`-per-call return the caller might
    /// only re-poll on the next packet) is what keeps a completed
    /// message from ever waiting behind a later, unrelated packet before
    /// it's surfaced as an event.
    messages: Vec<CompletedMessage>,
}

impl ChannelDsp {
    fn new(freq_hz: f64, center_hz: f64, source_rate_hz: f64) -> Result<Self, OrbcommError> {
        let plan = plan_resampling(source_rate_hz)?;
        Ok(Self {
            nco_phase: 0.0,
            nco_step: wrap_phase(-std::f64::consts::TAU * (freq_hz - center_hz) / source_rate_hz),
            predecim: plan.predecim,
            out_per_in: CHANNEL_SAMPLE_RATE_HZ / plan.resampler_in_rate_hz,
            resampler: plan.resampler,
            fll: Fll::new(),
            demod: SdpskDemod::new(),
            deframer: Deframer::new(),
            reassembler: Reassembler::new(DEFAULT_MAX_AGE_PACKETS),
            decimated: Vec::new(),
            bits: Vec::new(),
            frames: Vec::new(),
            messages: Vec::new(),
        })
    }
}

/// One requested channel: its identity, its counters, and — when it is inside
/// the source span — its receive chain.
struct Channel {
    freq_hz: f64,
    dsp: Option<ChannelDsp>,
    packets_ok: u64,
    checksum_fail: u64,
    repaired: u64,
}

/// `true` when the channel and its ±[`CHANNEL_HALF_BANDWIDTH_HZ`] fit inside
/// the source's Nyquist span. Non-finite or non-positive geometry is never in
/// span — a NaN comparison would otherwise read as "fits".
fn channel_in_span(freq_hz: f64, center_hz: f64, source_rate_hz: f64) -> bool {
    if !freq_hz.is_finite() || !center_hz.is_finite() || !source_rate_hz.is_finite() {
        return false;
    }
    if source_rate_hz <= 0.0 {
        return false;
    }
    (freq_hz - center_hz).abs() + CHANNEL_HALF_BANDWIDTH_HZ <= source_rate_hz / 2.0
}

/// A bank of per-channel Orbcomm receivers over one wideband IQ stream.
///
/// See the module docs for the chain each channel runs.
pub struct ChannelBank {
    channels: Vec<Channel>,
    /// Mixed source-rate scratch, shared by every channel in turn.
    mixed: Vec<Complex>,
}

impl ChannelBank {
    /// Build a bank for `channels` (Hz) out of a stream of `source_rate_hz`
    /// complex samples centred on `center_hz`.
    ///
    /// Channels outside the source span are kept as inert stubs — they appear
    /// in [`Self::stats`] with `in_span: false` and never produce events — so
    /// the caller can pass the whole [`crate::ORBCOMM_CHANNELS_HZ`] list at any
    /// tuning and see which ones the current span actually covers.
    ///
    /// # Errors
    ///
    /// [`OrbcommError::NoChannelsInSpan`] when no requested channel fits, and
    /// [`OrbcommError::Dsp`] if no decimation chain can be built for the rate
    /// pair — neither the direct one nor any pre-decimated one, see
    /// [`plan_resampling`]; the error carried is the direct chain's.
    pub fn new(
        source_rate_hz: f64,
        center_hz: f64,
        channels: &[f64],
    ) -> Result<Self, OrbcommError> {
        let mut built = Vec::with_capacity(channels.len());
        let mut any_in_span = false;
        for &freq_hz in channels {
            let dsp = if channel_in_span(freq_hz, center_hz, source_rate_hz) {
                any_in_span = true;
                Some(ChannelDsp::new(freq_hz, center_hz, source_rate_hz)?)
            } else {
                None
            };
            built.push(Channel {
                freq_hz,
                dsp,
                packets_ok: 0,
                checksum_fail: 0,
                repaired: 0,
            });
        }
        if !any_in_span {
            return Err(OrbcommError::NoChannelsInSpan {
                center_hz,
                source_rate_hz,
            });
        }
        Ok(Self {
            channels: built,
            mixed: Vec::new(),
        })
    }

    /// Push a block of source samples, appending whatever the channels
    /// produce. Events are appended, never cleared — the caller owns `events`.
    ///
    /// All state carries across calls, so the caller may use any block size.
    pub fn process(&mut self, iq: &[Complex], events: &mut Vec<OrbcommEvent>) {
        if iq.is_empty() {
            return;
        }
        // Disjoint field borrows: every channel reuses the one source-rate
        // scratch buffer rather than owning a copy of it.
        let Self { channels, mixed } = self;
        for channel in channels.iter_mut() {
            channel.process(iq, mixed, events);
        }
    }

    /// Per-channel counters, in the order the channels were requested.
    pub fn stats(&self) -> Vec<ChannelStats> {
        self.channels
            .iter()
            .map(|c| ChannelStats {
                freq_hz: c.freq_hz,
                in_span: c.dsp.is_some(),
                packets_ok: c.packets_ok,
                checksum_fail: c.checksum_fail,
                repaired: c.repaired,
            })
            .collect()
    }
}

impl ChannelDsp {
    /// Step 1: mix the channel down to DC, phase-continuously across
    /// blocks. Writes the source-rate result into the bank-shared
    /// `mixed` scratch (cleared and refilled here, never grown per
    /// call beyond the block's own length).
    #[allow(clippy::cast_possible_truncation)]
    fn mix_to_baseband(&mut self, iq: &[Complex], mixed: &mut Vec<Complex>) {
        mixed.clear();
        mixed.reserve(iq.len());
        for &sample in iq {
            let (sin, cos) = self.nco_phase.sin_cos();
            mixed.push(sample * Complex::new(cos as f32, sin as f32));
            self.nco_phase = wrap_phase(self.nco_phase + self.nco_step);
        }
    }

    /// Step 2: optional integer pre-decimation (see [`plan_resampling`],
    /// only engaged for source rates that defeat a direct resampler),
    /// then decimation to the channel rate — which is also the channel
    /// filter. Returns the number of valid samples now at the head of
    /// `self.decimated`, or `None` when the block had to be dropped.
    fn resample_to_channel_rate(&mut self, mixed: &[Complex], freq_hz: f64) -> Option<usize> {
        // Disjoint field borrows: step 2a hands step 2b a slice borrowed from
        // `predecim`'s own buffer while `resampler` and `decimated` are held
        // mutably, which only type-checks through separate bindings.
        let Self {
            predecim,
            out_per_in,
            resampler,
            decimated,
            ..
        } = self;

        // 2a.
        let stage_input: &[Complex] = if let Some(pre) = predecim.as_mut() {
            let need = resample_capacity_for(mixed.len(), pre.out_per_in);
            if pre.buf.len() < need {
                pre.buf.resize(need, Complex::default());
            }
            match pre.stage.process(mixed, &mut pre.buf) {
                Ok(count) => &pre.buf[..count],
                Err(error) => {
                    // Unreachable given `resample_capacity_for`, but dropping
                    // the block is the only safe response: the stage's delay
                    // line has already advanced, so re-running it would
                    // duplicate input.
                    warn!(channel_hz = freq_hz, %error, "orbcomm channel pre-decimation failed");
                    return None;
                }
            }
        } else {
            mixed
        };

        // 2b.
        let need = resample_capacity_for(stage_input.len(), *out_per_in);
        if decimated.len() < need {
            decimated.resize(need, Complex::default());
        }
        match resampler.process(stage_input, decimated) {
            Ok(count) => Some(count),
            Err(error) => {
                // Unreachable given `resample_capacity_for`, but dropping the
                // block is the only safe response: the resampler's delay line
                // has already advanced, so re-running it would duplicate input.
                warn!(channel_hz = freq_hz, %error, "orbcomm channel resample failed");
                None
            }
        }
    }

    /// Steps 3 and 4: coarse frequency correction in place over the
    /// first `count` channel-rate samples, then demodulation to
    /// information bits in `self.bits`.
    fn demodulate(&mut self, count: usize) {
        let Self {
            fll,
            demod,
            decimated,
            bits,
            ..
        } = self;
        fll.process(&mut decimated[..count]);
        bits.clear();
        demod.process(&decimated[..count], bits);
    }

    /// Step 5: deframe, parse and reassemble this block's bits, pushing
    /// one [`OrbcommEvent`] per decoded packet and per completed (or
    /// flushed) message. Returns `(packets_ok, repaired)` deltas for the
    /// channel's counters — `repaired` is a subset of `packets_ok`.
    fn deframe_bits(&mut self, freq_hz: f64, events: &mut Vec<OrbcommEvent>) -> (u64, u64) {
        // Disjoint field borrows again: `bits` is read while the deframer,
        // reassembler and their two scratch buffers are held mutably.
        let Self {
            deframer,
            reassembler,
            bits,
            frames,
            messages,
            ..
        } = self;
        let mut packets_ok = 0u64;
        let mut repaired = 0u64;

        for &bit in bits.iter() {
            frames.clear();
            deframer.push_bit(bit, frames);
            for frame in frames.iter() {
                if let Some(packet) = parse_packet(&frame.bytes) {
                    packets_ok += 1;
                    // Counted inside this arm, not beside it: `repaired` is
                    // documented as a subset of `packets_ok`, and only counting
                    // a repair that actually yielded a packet makes that
                    // structural rather than an invariant borrowed from the
                    // deframer's header/length checks happening to match
                    // `parse_packet`'s.
                    if frame.repaired {
                        repaired += 1;
                    }
                    events.push(OrbcommEvent {
                        channel_hz: freq_hz,
                        kind: OrbcommEventKind::Packet {
                            packet,
                            repaired: frame.repaired,
                        },
                    });
                }
                reassemble_if_message(reassembler, messages, &frame.bytes, freq_hz, events);
            }
        }
        (packets_ok, repaired)
    }
}

/// Feed one deframed packet to `reassembler` when — and only when — it is
/// a Message packet, emitting a `MessageComplete` event per message the
/// push completes or flushes.
///
/// Only Message packets carry reassembly fragments; the reassembler wants
/// their raw bytes, header and check bytes included. `push` is sink-style
/// (like `Deframer::push_bit`): it appends every message it completes or
/// flushes from THIS push to `messages`, which can hold two — see the
/// `messages` field doc — so both land as their own event from this one
/// bit rather than one of them waiting behind a later, unrelated packet.
fn reassemble_if_message(
    reassembler: &mut Reassembler,
    messages: &mut Vec<CompletedMessage>,
    bytes: &[u8],
    freq_hz: f64,
    events: &mut Vec<OrbcommEvent>,
) {
    if bytes.first().copied().and_then(PacketType::from_header) != Some(PacketType::Message) {
        return;
    }
    messages.clear();
    reassembler.push(bytes, messages);
    for message in messages.drain(..) {
        events.push(OrbcommEvent {
            channel_hz: freq_hz,
            kind: OrbcommEventKind::MessageComplete {
                bytes: message.bytes,
                partial: message.partial,
            },
        });
    }
}

impl Channel {
    /// Run one source block through this channel's chain: mix →
    /// decimate → FLL + demod → deframe/reassemble, one
    /// [`ChannelDsp`] step per call. Out-of-span channels are inert.
    fn process(
        &mut self,
        iq: &[Complex],
        mixed: &mut Vec<Complex>,
        events: &mut Vec<OrbcommEvent>,
    ) {
        // Copied out before the `dsp` borrow so the stage calls below can
        // take it by value without re-borrowing `self`.
        let freq_hz = self.freq_hz;
        let Some(dsp) = self.dsp.as_mut() else {
            return;
        };

        dsp.mix_to_baseband(iq, mixed);
        let Some(count) = dsp.resample_to_channel_rate(mixed, freq_hz) else {
            return;
        };
        dsp.demodulate(count);
        let (packets_ok, repaired) = dsp.deframe_bits(freq_hz, events);
        // The deframer swallows a failed stride rather than emitting it, so
        // its rejection counter is the only view of checksum failures.
        let bad_strides = dsp.deframer.bad_strides();

        self.packets_ok = self.packets_ok.saturating_add(packets_ok);
        self.repaired = self.repaired.saturating_add(repaired);
        self.checksum_fail = bad_strides;
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
mod tests;
