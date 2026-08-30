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
use crate::reassembly::{DEFAULT_MAX_AGE_PACKETS, Reassembler};
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

impl Channel {
    /// Run one source block through this channel's chain.
    #[allow(clippy::cast_possible_truncation)]
    fn process(
        &mut self,
        iq: &[Complex],
        mixed: &mut Vec<Complex>,
        events: &mut Vec<OrbcommEvent>,
    ) {
        let Self {
            freq_hz,
            dsp,
            packets_ok,
            checksum_fail,
            repaired,
        } = self;
        let Some(dsp) = dsp.as_mut() else {
            return;
        };

        // Disjoint field borrows: step 2a hands step 2b a slice borrowed from
        // `predecim`'s own buffer while `resampler` and `decimated` are held
        // mutably, which only type-checks through separate bindings.
        let ChannelDsp {
            nco_phase,
            nco_step,
            predecim,
            out_per_in,
            resampler,
            fll,
            demod,
            deframer,
            reassembler,
            decimated,
            bits,
            frames,
        } = dsp;

        // 1. Mix the channel down to DC, phase-continuously across blocks.
        mixed.clear();
        mixed.reserve(iq.len());
        for &sample in iq {
            let (sin, cos) = nco_phase.sin_cos();
            mixed.push(sample * Complex::new(cos as f32, sin as f32));
            *nco_phase = wrap_phase(*nco_phase + *nco_step);
        }

        // 2a. Optional integer pre-decimation (see `plan_resampling`). Only
        //     engaged for source rates that defeat a direct resampler.
        let stage_input: &[Complex] = if let Some(pre) = predecim.as_mut() {
            let need = resample_capacity_for(iq.len(), pre.out_per_in);
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
                    warn!(channel_hz = *freq_hz, %error, "orbcomm channel pre-decimation failed");
                    return;
                }
            }
        } else {
            mixed
        };

        // 2b. Decimate to the channel rate. This is also the channel filter.
        let need = resample_capacity_for(stage_input.len(), *out_per_in);
        if decimated.len() < need {
            decimated.resize(need, Complex::default());
        }
        let count = match resampler.process(stage_input, decimated) {
            Ok(count) => count,
            Err(error) => {
                // Unreachable given `resample_capacity_for`, but dropping the
                // block is the only safe response: the resampler's delay line
                // has already advanced, so re-running it would duplicate input.
                warn!(channel_hz = *freq_hz, %error, "orbcomm channel resample failed");
                return;
            }
        };

        // 3. Coarse frequency correction, in place.
        fll.process(&mut decimated[..count]);

        // 4. Demodulate to information bits.
        bits.clear();
        demod.process(&decimated[..count], bits);

        // 5. Deframe, parse, reassemble.
        for &bit in bits.iter() {
            frames.clear();
            deframer.push_bit(bit, frames);
            for frame in frames.iter() {
                if let Some(packet) = parse_packet(&frame.bytes) {
                    *packets_ok = packets_ok.saturating_add(1);
                    // Counted inside this arm, not beside it: `repaired` is
                    // documented as a subset of `packets_ok`, and only counting
                    // a repair that actually yielded a packet makes that
                    // structural rather than an invariant borrowed from the
                    // deframer's header/length checks happening to match
                    // `parse_packet`'s.
                    if frame.repaired {
                        *repaired = repaired.saturating_add(1);
                    }
                    events.push(OrbcommEvent {
                        channel_hz: *freq_hz,
                        kind: OrbcommEventKind::Packet {
                            packet,
                            repaired: frame.repaired,
                        },
                    });
                }
                // Only Message packets carry reassembly fragments; the
                // reassembler wants their raw bytes, header and check bytes
                // included.
                if frame
                    .bytes
                    .first()
                    .copied()
                    .and_then(PacketType::from_header)
                    == Some(PacketType::Message)
                    && let Some(message) = reassembler.push(&frame.bytes)
                {
                    events.push(OrbcommEvent {
                        channel_hz: *freq_hz,
                        kind: OrbcommEventKind::MessageComplete {
                            bytes: message.bytes,
                            partial: message.partial,
                        },
                    });
                }
            }
        }
        // The deframer swallows a failed stride rather than emitting it, so
        // its rejection counter is the only view of checksum failures.
        *checksum_fail = deframer.bad_strides();
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
mod tests {
    use super::*;
    use crate::demod::modulate_sdpsk_at_sps;
    use crate::packet::{PacketType, fletcher16_check_bytes};
    use crate::{ORBCOMM_CHANNELS_HZ, SYMBOL_RATE_HZ};

    /// Wideband source rate used by the synthesis tests.
    const SOURCE_RATE_HZ: f64 = 2_400_000.0;
    /// Tune centre: midway between the two channels under test.
    const CENTER_HZ: f64 = 137_512_500.0;
    /// Test channel at `CENTER_HZ + 100 kHz`.
    const CHANNEL_A_HZ: f64 = 137_612_500.0;
    /// Test channel at `CENTER_HZ − 150 kHz`.
    const CHANNEL_B_HZ: f64 = 137_362_500.0;
    /// Samples per symbol at the source rate: 2 400 000 / 4800.
    const TX_SPS: usize = 500;
    /// Amplitude scaling for the unit-energy RRC taps at [`TX_SPS`], so the
    /// synthesised waveform peaks near 1.0 regardless of the oversampling.
    const TX_GAIN: f32 = 22.360_68; // sqrt(500)
    /// Sync-packet repeats in a synthesised burst. 25 × 96 bits ≈ 0.5 s of
    /// air time: enough for the resampler, the FLL and the timing loop to
    /// settle and still leave a dozen packets for the deframer.
    const PACKET_REPEATS: usize = 25;
    /// Block size the bank is fed with in the synthesis tests.
    const FEED_BLOCK: usize = 65_536;

    /// A checksum-valid 12-byte Sync packet carrying `sat_id`.
    fn sync_packet(sat_id: u8) -> Vec<u8> {
        let mut p = vec![
            PacketType::Sync.header_byte(),
            0xAA,
            0xBB,
            sat_id,
            0x01,
            0x02,
            0x03,
            0x04,
            0x05,
            0x06,
        ];
        let (c0, c1) = fletcher16_check_bytes(&p);
        p.push(c0);
        p.push(c1);
        p
    }

    /// A checksum-valid 12-byte Message fragment. `seq` is zero-based, and
    /// byte 1 is `total` in the high nibble — see
    /// [`crate::reassembly::msg_total_len`].
    fn message_fragment(seq: u8, total: u8, fill: u8) -> Vec<u8> {
        let mut p = vec![
            PacketType::Message.header_byte(),
            (total << 4) | (seq & 0x0F),
        ];
        p.extend_from_slice(&[fill; 8]);
        let (c0, c1) = fletcher16_check_bytes(&p);
        p.push(c0);
        p.push(c1);
        p
    }

    /// `repeats` back-to-back copies of `bytes` as wire-order bits (LSB first
    /// within each byte — the convention the deframer assembles bytes with).
    fn repeated_bits(bytes: &[u8], repeats: usize) -> Vec<bool> {
        let mut bits = Vec::with_capacity(repeats * bytes.len() * 8);
        for _ in 0..repeats {
            for b in bytes {
                for k in 0..8 {
                    bits.push((b >> k) & 1 == 1);
                }
            }
        }
        bits
    }

    /// Modulate `bits` at the source rate, shift to `offset_hz` and sum into
    /// `dst` — the exact inverse of the channelizer's own mix-and-decimate.
    fn transmit_into(dst: &mut Vec<Complex>, bits: &[bool], offset_hz: f64) {
        let base = modulate_sdpsk_at_sps(bits, TX_SPS);
        if dst.len() < base.len() {
            dst.resize(base.len(), Complex::default());
        }
        let step = std::f64::consts::TAU * offset_hz / SOURCE_RATE_HZ;
        let mut phase = 0.0_f64;
        for (slot, &s) in dst.iter_mut().zip(base.iter()) {
            let (sin, cos) = phase.sin_cos();
            *slot += s * Complex::new(cos as f32, sin as f32) * TX_GAIN;
            phase = wrap_phase(phase + step);
        }
    }

    /// Feed `iq` to `bank` in [`FEED_BLOCK`]-sample blocks.
    fn run(bank: &mut ChannelBank, iq: &[Complex]) -> Vec<OrbcommEvent> {
        let mut events = Vec::new();
        for block in iq.chunks(FEED_BLOCK) {
            bank.process(block, &mut events);
        }
        events
    }

    /// Sync `sat_id`s reported on `channel_hz`.
    fn sat_ids_on(events: &[OrbcommEvent], channel_hz: f64) -> Vec<u8> {
        events
            .iter()
            .filter(|e| e.channel_hz.to_bits() == channel_hz.to_bits())
            .filter_map(|e| match &e.kind {
                OrbcommEventKind::Packet {
                    packet: OrbcommPacket::Sync { sat_id, .. },
                    ..
                } => Some(*sat_id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn two_channels_decode_independently() {
        let bits_a = repeated_bits(&sync_packet(0x2C), PACKET_REPEATS);
        let bits_b = repeated_bits(&sync_packet(0x51), PACKET_REPEATS);
        let mut iq = Vec::new();
        transmit_into(&mut iq, &bits_a, CHANNEL_A_HZ - CENTER_HZ);
        transmit_into(&mut iq, &bits_b, CHANNEL_B_HZ - CENTER_HZ);

        let mut bank = ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ, CHANNEL_B_HZ])
            .expect("both channels are inside a 2.4 Msps span");
        let events = run(&mut bank, &iq);

        let a = sat_ids_on(&events, CHANNEL_A_HZ);
        let b = sat_ids_on(&events, CHANNEL_B_HZ);
        assert!(a.len() >= 4, "channel A produced {} sync packets", a.len());
        assert!(b.len() >= 4, "channel B produced {} sync packets", b.len());
        assert!(a.iter().all(|&id| id == 0x2C), "channel A leaked: {a:?}");
        assert!(b.iter().all(|&id| id == 0x51), "channel B leaked: {b:?}");

        let stats = bank.stats();
        assert_eq!(stats.len(), 2);
        assert!(stats.iter().all(|s| s.in_span));
        assert!(stats[0].packets_ok >= 4 && stats[1].packets_ok >= 4);
        // On a clean link nothing is rejected and nothing needs repairing —
        // both stats are wired to the deframer, not to a spurious source.
        assert!(
            stats
                .iter()
                .all(|s| s.checksum_fail == 0 && s.repaired == 0)
        );
    }

    #[test]
    fn multi_packet_message_reassembles_end_to_end() {
        // Two-fragment sequences over the air: only checksum-valid Message
        // packets reach the reassembler, and its completions surface as their
        // own event kind alongside the packet events that produced them.
        let mut wire = message_fragment(0, 2, 0xA1);
        wire.extend(message_fragment(1, 2, 0xB2));
        let bits = repeated_bits(&wire, 13);
        let mut iq = Vec::new();
        transmit_into(&mut iq, &bits, CHANNEL_A_HZ - CENTER_HZ);

        let mut bank =
            ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ]).expect("channel in span");
        let events = run(&mut bank, &iq);

        let mut expected = vec![0xA1_u8; 8];
        expected.extend_from_slice(&[0xB2; 8]);
        let completed: Vec<&OrbcommEvent> = events
            .iter()
            .filter(|e| matches!(e.kind, OrbcommEventKind::MessageComplete { .. }))
            .collect();
        assert!(completed.len() >= 3, "got {} messages", completed.len());
        for event in completed {
            assert_eq!(event.channel_hz.to_bits(), CHANNEL_A_HZ.to_bits());
            let OrbcommEventKind::MessageComplete { bytes, partial } = &event.kind else {
                unreachable!("filtered above")
            };
            assert!(!partial, "fragment lost on a clean link");
            assert_eq!(bytes, &expected);
        }
    }

    #[test]
    fn doppler_shifted_channel_still_decodes() {
        // +3 kHz on top of the channel offset — worst-case 137 MHz Doppler,
        // nearly 4× the demodulator's ±800 Hz residual contract. Only the FLL
        // can bring this inside it.
        const DOPPLER_HZ: f64 = 3000.0;
        let bits = repeated_bits(&sync_packet(0x7A), PACKET_REPEATS);
        let mut iq = Vec::new();
        transmit_into(&mut iq, &bits, CHANNEL_A_HZ - CENTER_HZ + DOPPLER_HZ);

        let mut bank =
            ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ]).expect("channel in span");
        let events = run(&mut bank, &iq);

        let ids = sat_ids_on(&events, CHANNEL_A_HZ);
        assert!(ids.len() >= 4, "only {} packets under Doppler", ids.len());
        assert!(ids.iter().all(|&id| id == 0x7A), "got {ids:?}");
    }

    #[test]
    fn adjacent_channels_at_real_spacing_do_not_leak() {
        // The real Orbcomm grid puts 137.440 and 137.460 MHz just 20 kHz apart,
        // barely wider than the 19.2 kHz each channel's decimation keeps. This
        // is the module doc's "a neighbour at the real spacing lands in the
        // Nuttall stopband" claim, and it is what a real capture will stress.
        const LOW_HZ: f64 = 137_440_000.0;
        const HIGH_HZ: f64 = 137_460_000.0;
        const PAIR_CENTER_HZ: f64 = 137_450_000.0;

        let bits_low = repeated_bits(&sync_packet(0x11), PACKET_REPEATS);
        let bits_high = repeated_bits(&sync_packet(0x22), PACKET_REPEATS);
        let mut iq = Vec::new();
        transmit_into(&mut iq, &bits_low, LOW_HZ - PAIR_CENTER_HZ);
        transmit_into(&mut iq, &bits_high, HIGH_HZ - PAIR_CENTER_HZ);

        let mut bank = ChannelBank::new(SOURCE_RATE_HZ, PAIR_CENTER_HZ, &[LOW_HZ, HIGH_HZ])
            .expect("both channels in span");
        let events = run(&mut bank, &iq);

        let low = sat_ids_on(&events, LOW_HZ);
        let high = sat_ids_on(&events, HIGH_HZ);
        assert!(low.len() >= 4, "137.440 produced {} packets", low.len());
        assert!(high.len() >= 4, "137.460 produced {} packets", high.len());
        assert!(
            low.iter().all(|&id| id == 0x11),
            "137.460 leaked into 137.440: {low:?}"
        );
        assert!(
            high.iter().all(|&id| id == 0x22),
            "137.440 leaked into 137.460: {high:?}"
        );
    }

    #[test]
    fn repaired_packets_are_a_subset_of_parsed_packets() {
        // One flipped bit inside an otherwise clean burst: the deframer is
        // already locked when that stride arrives, repairs it, and emits the
        // original packet with `repaired: true`. Both counters must move
        // together — `repaired` is documented as a subset of `packets_ok`.
        const CORRUPTED_PACKET: usize = 10;
        let packet = sync_packet(0x2C);
        let mut bits = repeated_bits(&packet, PACKET_REPEATS);
        let flip = CORRUPTED_PACKET * packet.len() * 8 + 40;
        bits[flip] = !bits[flip];

        let mut iq = Vec::new();
        transmit_into(&mut iq, &bits, CHANNEL_A_HZ - CENTER_HZ);
        let mut bank =
            ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ]).expect("channel in span");
        let events = run(&mut bank, &iq);

        let stats = bank.stats();
        assert_eq!(stats.len(), 1);
        assert!(stats[0].repaired >= 1, "the flipped bit was never repaired");
        assert!(
            stats[0].repaired <= stats[0].packets_ok,
            "repaired {} exceeds packets_ok {}",
            stats[0].repaired,
            stats[0].packets_ok
        );

        // Every repaired event still carries the *original* packet.
        let repaired: Vec<&OrbcommPacket> = events
            .iter()
            .filter_map(|e| match &e.kind {
                OrbcommEventKind::Packet {
                    packet,
                    repaired: true,
                } => Some(packet),
                _ => None,
            })
            .collect();
        assert_eq!(repaired.len() as u64, stats[0].repaired);
        for packet in repaired {
            assert!(
                matches!(packet, OrbcommPacket::Sync { sat_id: 0x2C, .. }),
                "repair produced {packet:?}"
            );
        }
    }

    // --- Source-rate coverage (final review, C1) ---------------------------

    /// Airspy R2's low native rate. `2_500_000 / 4800 = 520.83` samples per
    /// symbol is not an integer, so [`modulate_sdpsk_at_sps`] cannot synthesise
    /// at it directly — see [`transmit_at_airspy_rate`].
    const AIRSPY_RATE_HZ: f64 = 2_500_000.0;

    #[test]
    fn bank_constructs_at_airspy_rates() {
        // 2.5 and 10 Msps are the Airspy R2's native rates
        // (`sdr-source-airspy::DEFAULT_SAMPLE_RATES`); 5 Msps is the Mini's
        // middle step. All three used to blow up inside `RationalResampler`
        // with a 1 484 375-tap prototype, because its power-of-two
        // pre-decimation leaves a fractional 19 531.25 Hz intermediate rate
        // that shares no factor with 19 200. The three RTL-SDR rates below
        // them must stay green — they take the unchanged direct chain.
        for rate_hz in [
            2_500_000.0_f64,
            5_000_000.0,
            10_000_000.0,
            2_400_000.0,
            3_200_000.0,
            250_000.0,
        ] {
            let bank = ChannelBank::new(rate_hz, CENTER_HZ, &ORBCOMM_CHANNELS_HZ);
            assert!(
                bank.is_ok(),
                "ChannelBank::new failed at {rate_hz} Hz: {:?}",
                bank.err()
            );
        }
    }

    #[test]
    fn predecimation_engages_only_where_the_direct_chain_fails() {
        // Rates that already worked must keep the exact chain they had —
        // notably 1.2288 Msps, the reference captures' rate, where the direct
        // plan reduces to a pure power-of-two decimation with no polyphase
        // stage at all.
        for rate_hz in [
            250_000.0_f64,
            1_228_800.0,
            2_400_000.0,
            3_200_000.0,
            SOURCE_RATE_HZ,
        ] {
            let plan = plan_resampling(rate_hz).expect("direct chain constructs");
            assert!(
                plan.predecim.is_none(),
                "{rate_hz} Hz gained a pre-decimation stage it did not need"
            );
            assert!((plan.resampler_in_rate_hz - rate_hz).abs() < f64::EPSILON);
        }

        // The three broken rates all settle on the same 100 kHz intermediate:
        // the largest D with `rate / D >= MIN_INTERMEDIATE_RATE_HZ` for which
        // both stages construct.
        for rate_hz in [AIRSPY_RATE_HZ, 5_000_000.0, 10_000_000.0] {
            let plan = plan_resampling(rate_hz).expect("pre-decimated chain constructs");
            let Some(pre) = plan.predecim.as_ref() else {
                unreachable!("{rate_hz} Hz must gain a pre-decimation stage")
            };
            assert!(
                (plan.resampler_in_rate_hz - 100_000.0).abs() < f64::EPSILON,
                "{rate_hz} Hz picked a {} Hz intermediate",
                plan.resampler_in_rate_hz
            );
            assert!(plan.resampler_in_rate_hz >= MIN_INTERMEDIATE_RATE_HZ);
            // `out_per_in` is the stage's own ratio, and the two stages'
            // ratios must multiply back to the end-to-end 19.2 kHz / source.
            let end_to_end = pre.out_per_in * (CHANNEL_SAMPLE_RATE_HZ / plan.resampler_in_rate_hz);
            assert!((end_to_end - CHANNEL_SAMPLE_RATE_HZ / rate_hz).abs() < 1e-12);
        }
    }

    /// Synthesise `bits` at [`AIRSPY_RATE_HZ`] and shift to `offset_hz`.
    ///
    /// The modulator only takes an integer samples-per-symbol, and 2.5 Msps
    /// is 520.83 samples per 4800 baud symbol. So the waveform is built at
    /// [`SOURCE_RATE_HZ`] (an exact 500 sps) and resampled 24:25 — an exact
    /// integer ratio, so no timing error is introduced — before the channel
    /// offset is applied at the destination rate.
    fn transmit_at_airspy_rate(bits: &[bool], offset_hz: f64) -> Vec<Complex> {
        let base = modulate_sdpsk_at_sps(bits, TX_SPS);
        let mut up = RationalResampler::new(SOURCE_RATE_HZ, AIRSPY_RATE_HZ)
            .expect("2.4 -> 2.5 Msps is a 24:25 upsample");
        let mut out = vec![Complex::default(); base.len() * 2 + RESAMPLER_OUTPUT_MARGIN];
        let count = up.process(&base, &mut out).expect("output buffer is 2x");
        out.truncate(count);

        let step = std::f64::consts::TAU * offset_hz / AIRSPY_RATE_HZ;
        let mut phase = 0.0_f64;
        for slot in &mut out {
            let (sin, cos) = phase.sin_cos();
            *slot = *slot * Complex::new(cos as f32, sin as f32) * TX_GAIN;
            phase = wrap_phase(phase + step);
        }
        out
    }

    #[test]
    fn decodes_at_airspy_rate_through_predecimation() {
        // End-to-end proof that the pre-decimated chain is not merely
        // constructible: a real burst at the Airspy's 2.5 Msps has to come
        // out the far side as checksum-valid Sync packets carrying the right
        // satellite id.
        let bits = repeated_bits(&sync_packet(0x2C), PACKET_REPEATS);
        let iq = transmit_at_airspy_rate(&bits, CHANNEL_A_HZ - CENTER_HZ);

        let mut bank = ChannelBank::new(AIRSPY_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ])
            .expect("channel in span at 2.5 Msps");
        let events = run(&mut bank, &iq);

        let ids = sat_ids_on(&events, CHANNEL_A_HZ);
        assert!(
            ids.len() >= 4,
            "only {} packets decoded through the pre-decimation path at {AIRSPY_RATE_HZ} Hz",
            ids.len()
        );
        assert!(ids.iter().all(|&id| id == 0x2C), "got {ids:?}");
        let stats = bank.stats();
        assert_eq!(stats[0].packets_ok as usize, ids.len());
    }

    #[test]
    fn out_of_span_channel_flagged() {
        // 240 kHz of span around 137.5 MHz reaches ±120 kHz, so only the
        // 137.44 / 137.46 MHz pair fits (with their ±9.6 kHz of bandwidth).
        let bank = ChannelBank::new(240_000.0, 137_500_000.0, &ORBCOMM_CHANNELS_HZ)
            .expect("two channels are in span");
        let stats = bank.stats();
        assert_eq!(stats.len(), ORBCOMM_CHANNELS_HZ.len());
        for (s, &f) in stats.iter().zip(ORBCOMM_CHANNELS_HZ.iter()) {
            assert_eq!(s.freq_hz.to_bits(), f.to_bits());
            let expect = (f - 137_500_000.0).abs() + CHANNEL_HALF_BANDWIDTH_HZ <= 120_000.0;
            assert_eq!(s.in_span, expect, "channel {f} in_span");
        }
        assert_eq!(stats.iter().filter(|s| s.in_span).count(), 2);
    }

    #[test]
    fn out_of_span_channels_ignore_input() {
        let mut bank = ChannelBank::new(240_000.0, 137_500_000.0, &ORBCOMM_CHANNELS_HZ)
            .expect("two channels are in span");
        let iq = vec![Complex::new(0.5, -0.25); 4096];
        let mut events = Vec::new();
        bank.process(&iq, &mut events);
        for (s, &f) in bank.stats().iter().zip(ORBCOMM_CHANNELS_HZ.iter()) {
            if !s.in_span {
                assert_eq!(s.packets_ok, 0, "channel {f} decoded while out of span");
                assert_eq!(s.checksum_fail, 0);
                assert_eq!(s.repaired, 0);
            }
        }
        // Anything emitted must carry an *in-span* channel's frequency.
        // (Asserting membership of ORBCOMM_CHANNELS_HZ would be vacuous —
        // every event's `channel_hz` is copied from the requested list.)
        let in_span: Vec<f64> = bank
            .stats()
            .iter()
            .filter(|s| s.in_span)
            .map(|s| s.freq_hz)
            .collect();
        assert_eq!(in_span.len(), 2);
        for event in &events {
            assert!(
                in_span
                    .iter()
                    .any(|f| f.to_bits() == event.channel_hz.to_bits()),
                "event from out-of-span channel {}",
                event.channel_hz
            );
        }
    }

    #[test]
    fn no_channels_in_span_errors() {
        // Tuned 37 MHz away: nothing fits.
        let err = ChannelBank::new(240_000.0, 100_000_000.0, &ORBCOMM_CHANNELS_HZ);
        assert!(matches!(err, Err(OrbcommError::NoChannelsInSpan { .. })));
        // An empty request has nothing in span either.
        assert!(matches!(
            ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[]),
            Err(OrbcommError::NoChannelsInSpan { .. })
        ));
        // A degenerate source rate must not be treated as infinite span.
        assert!(matches!(
            ChannelBank::new(f64::NAN, CENTER_HZ, &ORBCOMM_CHANNELS_HZ),
            Err(OrbcommError::NoChannelsInSpan { .. })
        ));
    }

    #[test]
    fn block_fragmentation_does_not_change_the_output() {
        // Every stage carries state across calls — NCO phase, resampler delay
        // lines, the FLL's accumulator and block counter, the demodulator and
        // the deframer — so ragged blocks must be bit-for-bit invisible.
        let bits = repeated_bits(&sync_packet(0x2C), 12);
        let mut iq = Vec::new();
        transmit_into(&mut iq, &bits, CHANNEL_A_HZ - CENTER_HZ);

        let mut whole =
            ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ]).expect("in span");
        let mut whole_events = Vec::new();
        whole.process(&iq, &mut whole_events);

        let mut ragged =
            ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ]).expect("in span");
        let mut ragged_events = Vec::new();
        let mut start = 0;
        let mut size = 1;
        while start < iq.len() {
            let end = (start + size).min(iq.len());
            ragged.process(&iq[start..end], &mut ragged_events);
            start = end;
            size = size % 4099 + 1;
        }

        assert_eq!(whole_events, ragged_events);
        assert!(!whole_events.is_empty(), "the harness decoded nothing");
    }

    #[test]
    fn sample_rate_ratio_matches_the_test_modulator() {
        assert!((SOURCE_RATE_HZ / SYMBOL_RATE_HZ - TX_SPS as f64).abs() < f64::EPSILON);
        assert!((TX_GAIN - (TX_SPS as f32).sqrt()).abs() < 1e-4);
    }

    // --- FLL ---------------------------------------------------------------

    /// Deterministic xorshift64* PRNG — tests must never be flaky.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        /// Uniform in `(0, 1)`.
        fn next_f64(&mut self) -> f64 {
            ((self.next_u64() >> 11) as f64 + 0.5) / 9_007_199_254_740_992.0
        }
        /// Standard normal via Box–Muller.
        fn next_normal(&mut self) -> f64 {
            let u1 = self.next_f64();
            let u2 = self.next_f64();
            (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        }
    }

    /// Add complex AWGN at the requested per-sample SNR, the same convention
    /// `demod.rs`'s loopback tests use (in-band, at the channel rate — so
    /// 10 dB here reads as roughly 16 dB of Es/N0 at 4 samples/symbol).
    fn add_awgn(samples: &mut [Complex], snr_db: f64, seed: u64) {
        if samples.is_empty() {
            return;
        }
        let signal_power = samples
            .iter()
            .map(|s| f64::from(s.re) * f64::from(s.re) + f64::from(s.im) * f64::from(s.im))
            .sum::<f64>()
            / samples.len() as f64;
        let sigma = (signal_power / 10.0_f64.powf(snr_db / 10.0) / 2.0).sqrt();
        let mut rng = Rng::new(seed);
        for s in samples.iter_mut() {
            *s = Complex::new(
                s.re + (sigma * rng.next_normal()) as f32,
                s.im + (sigma * rng.next_normal()) as f32,
            );
        }
    }

    fn apply_cfo(samples: &mut [Complex], cfo_hz: f64) {
        let step = std::f64::consts::TAU * cfo_hz / CHANNEL_SAMPLE_RATE_HZ;
        let mut phase = 0.0_f64;
        for s in samples.iter_mut() {
            let (sin, cos) = phase.sin_cos();
            *s = *s * Complex::new(cos as f32, sin as f32);
            phase = wrap_phase(phase + step);
        }
    }

    #[test]
    fn fll_pulls_in_worst_case_doppler() {
        // The demodulator's contract is ±800 Hz; Doppler at 137 MHz reaches
        // ±3.5 kHz. Both signs, and the residual is measured on the corrected
        // stream, not inferred from the loop state.
        for cfo_hz in [-3500.0_f64, -3000.0, 3000.0, 3500.0] {
            let mut rng = Rng::new(0x5EED_0100);
            let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
            let mut iq = modulate_sdpsk_at_sps(&bits, 4);
            apply_cfo(&mut iq, cfo_hz);

            let mut fll = Fll::new();
            fll.process(&mut iq);
            let residual = cfo_hz - fll.freq_hz;
            assert!(
                residual.abs() < 800.0,
                "cfo {cfo_hz}: residual {residual} Hz breaks the demod contract"
            );
        }
    }

    #[test]
    fn fll_pull_in_time_is_bounded() {
        // The pull-in transient costs bits, so bound it: from a 3.5 kHz cold
        // start the loop must be inside the ±800 Hz contract within 2048
        // channel samples (512 symbols, ~107 ms).
        const BUDGET: usize = 2048;
        let mut rng = Rng::new(0x5EED_0101);
        let bits: Vec<bool> = (0..1024).map(|_| rng.next_u64() & 1 == 1).collect();
        let mut iq = modulate_sdpsk_at_sps(&bits, 4);
        apply_cfo(&mut iq, 3500.0);
        assert!(iq.len() > BUDGET);

        let mut fll = Fll::new();
        fll.process(&mut iq[..BUDGET]);
        assert!(
            (3500.0 - fll.freq_hz).abs() < 800.0,
            "after {BUDGET} samples the estimate is still {} Hz",
            fll.freq_hz
        );
    }

    #[test]
    fn fll_stays_put_with_no_offset() {
        // Data-dependent jitter (±300 Hz block to block) must not accumulate
        // into a walk-off on a signal that has no offset to correct.
        let mut rng = Rng::new(0x5EED_0102);
        let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
        let mut iq = modulate_sdpsk_at_sps(&bits, 4);
        let mut fll = Fll::new();
        fll.process(&mut iq);
        assert!(
            fll.freq_hz.abs() < 400.0,
            "loop drifted to {} Hz on a clean signal",
            fll.freq_hz
        );
    }

    #[test]
    fn fll_pulls_in_at_10_db_snr() {
        // The discriminator runs on the decimated stream *before* the
        // demodulator's matched filter, so it sees in-band noise out to
        // ±9.6 kHz rather than the RRC's ±3.36 kHz. Bound that cost at the
        // same 10 dB per-sample SNR `demod.rs` uses for its noise-margin test:
        // pull-in from ±3 kHz must still land inside the ±800 Hz contract.
        //
        // This is a regression guard, not the cliff. Sweeping the SNR down,
        // the loop still pulls in at −10 dB and only breaks near −14 dB — the
        // 256-sample coherent average buys ~24 dB, so the demodulator (already
        // at 5 % BER by 10 dB) fails long before the FLL does. The wider
        // measurement bandwidth costs far less than it looked like it might.
        for (seed, cfo_hz) in [(0x5EED_0110_u64, -3000.0_f64), (0x5EED_0111, 3000.0)] {
            let mut rng = Rng::new(seed);
            let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
            let mut iq = modulate_sdpsk_at_sps(&bits, 4);
            apply_cfo(&mut iq, cfo_hz);
            add_awgn(&mut iq, 10.0, seed);

            let mut fll = Fll::new();
            fll.process(&mut iq);
            let residual = cfo_hz - fll.freq_hz;
            assert!(
                residual.abs() < 800.0,
                "cfo {cfo_hz} at 10 dB SNR: residual {residual} Hz"
            );
        }
    }

    #[test]
    fn fll_survives_non_finite_samples() {
        // A poisoned prefix must neither park the loop on a NaN nor stop it
        // tracking: the poisoned blocks are discarded whole, then the clean
        // tail behind them has to pull in normally.
        const POISON: usize = 1024;
        let mut poisoned = vec![Complex::new(f32::NAN, f32::NAN); POISON];
        poisoned[500] = Complex::new(f32::INFINITY, 1.0);
        poisoned[700] = Complex::new(1.0, f32::NEG_INFINITY);

        let mut fll = Fll::new();
        fll.process(&mut poisoned);
        assert!(fll.freq_hz.is_finite(), "freq went to {}", fll.freq_hz);
        assert!(fll.phase.is_finite());

        let mut rng = Rng::new(0x5EED_0104);
        let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
        let mut clean = modulate_sdpsk_at_sps(&bits, 4);
        apply_cfo(&mut clean, 3000.0);
        fll.process(&mut clean);
        let residual = 3000.0 - fll.freq_hz;
        assert!(
            residual.abs() < 800.0,
            "loop did not resume tracking after the poison: residual {residual} Hz"
        );
        assert!(clean.iter().all(|s| s.re.is_finite() && s.im.is_finite()));
    }

    #[test]
    fn fll_block_boundaries_are_invisible() {
        let mut rng = Rng::new(0x5EED_0103);
        let bits: Vec<bool> = (0..2048).map(|_| rng.next_u64() & 1 == 1).collect();
        let mut iq = modulate_sdpsk_at_sps(&bits, 4);
        apply_cfo(&mut iq, 2000.0);

        let mut whole = iq.clone();
        let mut a = Fll::new();
        a.process(&mut whole);

        let mut ragged = iq;
        let mut b = Fll::new();
        let mut start = 0;
        let mut size = 1;
        while start < ragged.len() {
            let end = (start + size).min(ragged.len());
            b.process(&mut ragged[start..end]);
            start = end;
            size = size % 97 + 1;
        }
        assert_eq!(whole, ragged);
        assert!((a.freq_hz - b.freq_hz).abs() < f64::EPSILON);
    }
}
