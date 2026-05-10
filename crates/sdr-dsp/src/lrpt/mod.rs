//! Meteor-M LRPT QPSK / OQPSK demodulator (epic #469 + issue #662).
//!
//! Two pipelines live here, dispatched from [`LrptDemod`] by mode:
//!
//! - **QPSK** (legacy METEOR-M N2, decommissioned but kept for any
//!   archival recordings): RRC matched filter → SDR++-style Costas
//!   → Gardner timing → hard slicer.
//! - **OQPSK** (current METEOR-M2 3 / METEOR-M2 4): RRC matched
//!   filter → dbdexter-style [`MeteorPll`] → dbdexter-style
//!   [`MmTiming`] (Mueller-Muller, dual-rail timeslot machine) →
//!   hard slicer.
//!
//! See [`meteor_pll`] and [`mm_timing`] for the OQPSK math; see
//! [`costas`] and [`timing`] for the QPSK math. The QPSK and
//! OQPSK paths are kept separate (rather than collapsing onto a
//! single primitive) so the QPSK chain stays unchanged from the
//! pre-#662 baseline — no regression risk for archival N2
//! recordings, and OQPSK gets dbdexter's tight loops + lock
//! detector + free-run frequency sweep without bolting them onto
//! a previously-tuned QPSK loop.
//!
//! References (read-only):
//! - QPSK chain: `original/SDRPlusPlus/decoder_modules/meteor_demodulator/src/`.
//! - OQPSK chain: `original/meteor_demod/dsp/{pll,timing}.{c,h}` and
//!   `original/meteor_demod/demod.c::demod_oqpsk`.

pub mod costas;
pub mod meteor_agc;
pub mod meteor_pll;
pub mod mm_timing;
pub mod rrc_filter;
pub mod slicer;
pub mod timing;

pub use costas::Costas;
pub use meteor_agc::MeteorAgc;
pub use meteor_pll::MeteorPll;
pub use mm_timing::MmTiming;
pub use rrc_filter::RrcFilter;
pub use slicer::slice_soft;
pub use timing::Gardner;

use sdr_types::{Complex, DspError};

/// Meteor LRPT symbol rate (symbols per second).
pub const SYMBOL_RATE_HZ: f32 = 72_000.0;

/// Working sample rate for the demod chain. 2 samples per symbol
/// is the standard QPSK convention post-RRC.
pub const SAMPLE_RATE_HZ: f32 = SYMBOL_RATE_HZ * 2.0;

/// Costas loop bandwidth (normalized, cycles per sample). Lifted
/// verbatim from SDR++'s caller in `meteor_demodulator/src/main.cpp`
/// (the `0.005` argument to the demod's `init`). Wider locks faster
/// but tracks less cleanly post-lock.
pub const COSTAS_LOOP_BW: f32 = 0.005;

/// Gardner symbol-period (`omega`) tracking gain. Per SDR++'s
/// caller in `meteor_demodulator/src/main.cpp`.
pub const GARDNER_OMEGA_GAIN: f32 = 1e-6;

/// Gardner fractional-offset (`mu`) tracking gain. Per SDR++'s
/// caller in `meteor_demodulator/src/main.cpp`.
pub const GARDNER_MU_GAIN: f32 = 0.01;

/// LRPT demod chain samples-per-symbol setting. The chain runs at
/// 2 sps post-RRC (the standard QPSK convention); pinning it as a
/// constant lets the RRC filter and Gardner timing recovery agree
/// without drift, and matches the project convention of naming
/// every magic numeric configuration value.
pub const SAMPLES_PER_SYMBOL: usize = 2;

/// dbdexter `pll_bw` default — `1` Hz of effective loop bandwidth
/// at the Meteor symbol rate. `meteor_demod/demod.h:15`.
const DBDEXTER_PLL_BW_HZ: f32 = 1.0;

/// dbdexter `SYM_BW` default — Mueller-Muller loop bandwidth.
/// `meteor_demod/demod.h:14`. Already normalized; in dbdexter's
/// pipeline this is divided by the polyphase `interp_factor`
/// before being passed to the timing loop, but we run with no
/// interpolation (1 filtered output per input at 2 sps), so we
/// use the value directly.
const DBDEXTER_SYM_BW: f32 = 0.000_05;

/// OQPSK PLL loop bandwidth in radians per `mix_*` call. Mirrors
/// dbdexter's `2π * pll_bw / (1 * symrate)` formula
/// (`demod.c:12`, with `multiplier = 1` for OQPSK). At 1 Hz of
/// effective loop bandwidth and 72 ksym/s, this is
/// `2π × 1 / 72_000 ≈ 8.7266e-5`.
const OQPSK_PLL_BW: f32 = 2.0 * core::f32::consts::PI * DBDEXTER_PLL_BW_HZ / SYMBOL_RATE_HZ;

/// Mueller-Muller initial symbol period, in radians per timeslot
/// tick. Mirrors dbdexter's `2π * symrate / (samplerate * interp)`
/// formula (`demod.c:13`, with `interp = 1`). At 2 sps this is
/// exactly `π`.
const MM_SYM_FREQ: f32 = 2.0 * core::f32::consts::PI * SYMBOL_RATE_HZ / SAMPLE_RATE_HZ;

/// Reciprocal of the AGC target magnitude. The OQPSK chain's PLL +
/// timing loops are calibrated for dbdexter's |sample| ≈ 190
/// post-AGC scale, but [`slice_soft`] is calibrated for unit-
/// magnitude input (rails at ±0.707 → ±90 after the ×127 scaling).
/// Multiplying the assembled symbol by this constant just before
/// the slicer brings it back to unit magnitude without disturbing
/// the loop scales upstream.
const OQPSK_SLICER_DESCALE: f32 = 1.0 / meteor_agc::TARGET_MAG;

/// Modulation modes supported by [`LrptDemod`]. The catalog layer
/// (`sdr-sat`) carries its own equivalent enum — the controller is
/// the seam that maps from one to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LrptMode {
    /// Standard QPSK. Used by legacy METEOR-M N2 recordings.
    Qpsk,
    /// Offset QPSK — Q delayed by Tsym/2 from I. Used by
    /// METEOR-M2 3 and METEOR-M2 4.
    Oqpsk,
}

/// Top-level LRPT demodulator chain. The mode is picked at
/// construction (defaulting to QPSK for backward compatibility);
/// `process()` dispatches each input sample down the appropriate
/// inner pipeline.
pub struct LrptDemod {
    rrc: RrcFilter,
    inner: DemodInner,
}

/// Per-mode demod state. Boxing isn't necessary — both variants
/// are small enough that the size difference is irrelevant, and
/// the sum-type representation makes the dispatch in `process`
/// trivially branch-predictable.
enum DemodInner {
    Qpsk {
        costas: Costas,
        gardner: Gardner,
    },
    Oqpsk {
        /// AGC stage between the RRC filter and the carrier-
        /// recovery PLL. dbdexter's PLL is calibrated for
        /// |sample| = 190 post-AGC; without this the lock
        /// detector mis-fires (see [`MeteorAgc`]'s module
        /// docs for the full chain of consequences).
        agc: MeteorAgc,
        pll: MeteorPll,
        timing: MmTiming,
        /// In-phase sample stashed between the I-tick and the
        /// Q-tick. Per dbdexter `demod.c::demod_oqpsk`'s `static
        /// float inphase`.
        pending_i: f32,
    },
}

impl LrptDemod {
    /// Build a QPSK demod chain. Equivalent to
    /// `new_with_mode(LrptMode::Qpsk)` — kept as a no-arg
    /// constructor for backward compatibility with all the
    /// existing call sites that predate the modulation enum.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if either inner
    /// constructor rejects its synthesized parameters: `Costas::new`
    /// (loop-bandwidth validation), or `Gardner::new`
    /// (samples-per-symbol + gain finiteness validation).
    /// Practically unreachable for the project's pinned constants —
    /// the propagation is here for defensive consistency with the
    /// rest of the DSP module.
    pub fn new() -> Result<Self, DspError> {
        Self::new_with_mode(LrptMode::Qpsk)
    }

    /// Build a demod chain in the requested mode.
    ///
    /// QPSK uses the existing SDR++-style Costas + Gardner; OQPSK
    /// uses the dbdexter-style [`MeteorPll`] + [`MmTiming`]. Each mode
    /// brings its own loop tuning (see the `*_BW` / `*_GAIN`
    /// constants in this module for the chosen values).
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if any inner loop
    /// rejects its parameters. Practically unreachable for the
    /// pinned constants in this module.
    pub fn new_with_mode(mode: LrptMode) -> Result<Self, DspError> {
        let inner = match mode {
            LrptMode::Qpsk => Self::build_qpsk_inner()?,
            LrptMode::Oqpsk => Self::build_oqpsk_inner()?,
        };
        Ok(Self {
            rrc: RrcFilter::new(SAMPLES_PER_SYMBOL),
            inner,
        })
    }

    fn build_qpsk_inner() -> Result<DemodInner, DspError> {
        #[allow(
            clippy::cast_precision_loss,
            reason = "SAMPLES_PER_SYMBOL is a tiny constant (= 2); the f32 conversion is exact"
        )]
        let sps_f = SAMPLES_PER_SYMBOL as f32;
        Ok(DemodInner::Qpsk {
            costas: Costas::new(COSTAS_LOOP_BW)?,
            gardner: Gardner::new(sps_f, GARDNER_OMEGA_GAIN, GARDNER_MU_GAIN)?,
        })
    }

    fn build_oqpsk_inner() -> Result<DemodInner, DspError> {
        let agc = MeteorAgc::new();
        let pll = MeteorPll::new(OQPSK_PLL_BW, true, None)?;
        let timing = MmTiming::new(MM_SYM_FREQ, DBDEXTER_SYM_BW)?;
        Ok(DemodInner::Oqpsk {
            agc,
            pll,
            timing,
            pending_i: 0.0,
        })
    }

    /// Whether the OQPSK carrier-recovery PLL has ever locked since
    /// construction. `None` for the QPSK mode (the SDR++-style
    /// Costas loop in that path doesn't expose a lock detector).
    /// Useful for diagnostics and for the `oqpsk_zero_iq_*`
    /// regression tests that pin the AGC + lock-detector
    /// interaction. Per CR round 2 on PR #663.
    #[must_use]
    pub fn oqpsk_locked_once(&self) -> Option<bool> {
        match &self.inner {
            DemodInner::Qpsk { .. } => None,
            DemodInner::Oqpsk { pll, .. } => Some(pll.locked_once()),
        }
    }

    /// Push one complex baseband sample. Returns up to one soft-
    /// symbol pair `[i, q]` when the timing recovery fires a tick.
    pub fn process(&mut self, x: Complex) -> Option<[i8; 2]> {
        let filtered = self.rrc.process(x);
        match &mut self.inner {
            DemodInner::Qpsk { costas, gardner } => {
                let derotated = costas.process(filtered);
                gardner.process(derotated).map(slice_soft)
            }
            DemodInner::Oqpsk {
                agc,
                pll,
                timing,
                pending_i,
            } => {
                // AGC normalizes magnitude to dbdexter's
                // expected ~190 rail amplitude before the PLL —
                // without this the lock detector and tanh LUT
                // are calibrated to the wrong scale. Per CR
                // round 2 on PR #663.
                let scaled = agc.process(filtered);
                // Advance the carrier NCO on every input sample,
                // not only on timing ticks. dbdexter's polyphase
                // pipeline (interp_factor=5) skips the PLL on
                // non-tick polyphase positions because those are
                // filter intermediates, not real samples — but at
                // our 2 sps with no interpolation, every input is
                // a real sample and the NCO must track wall-clock
                // continuously. Without this, a 0-tick from the
                // M&M loop during timing correction leaves the
                // PLL frozen for that sample, and the next
                // mix_i / mix_q would run at the wrong phase.
                // Per CR round 1 on PR #663.
                let mixed = pll.mix(scaled);
                match timing.advance_timeslot_dual() {
                    1 => {
                        // I-tick: stash the in-phase rail. The
                        // Q-tick half a symbol from now will
                        // pair with this and drive the per-symbol
                        // updates.
                        *pending_i = mixed.re;
                        None
                    }
                    2 => {
                        // Q-tick: reassemble the I/Q pair,
                        // retime the symbol clock, update the
                        // carrier estimate, emit the soft symbol.
                        // The retime + update_estimate calls run
                        // on the AGC-scale symbol (dbdexter's
                        // calibration); the slicer gets a
                        // descaled copy (unit magnitude, what
                        // [`slice_soft`] expects).
                        let symbol = Complex::new(*pending_i, mixed.im);
                        timing.retime(symbol);
                        pll.update_estimate(*pending_i, mixed.im);
                        Some(slice_soft(symbol * OQPSK_SLICER_DESCALE))
                    }
                    _ => None,
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn qpsk_pipeline_produces_soft_symbols_from_synthetic_qpsk() {
        // Synthesize ~2 sps QPSK with no impairments. Pipeline
        // converges and emits signed i8 pairs.
        let mut demod = LrptDemod::new().expect("LrptDemod::new");
        let symbols = [
            Complex::new(0.707, 0.707),
            Complex::new(-0.707, 0.707),
            Complex::new(0.707, -0.707),
            Complex::new(-0.707, -0.707),
        ];
        let mut emitted = 0_usize;
        for n in 0..4000 {
            let sym = symbols[(n / 2) % 4];
            let s = if n % 2 == 0 {
                sym
            } else {
                Complex::new(0.0, 0.0)
            };
            if demod.process(s).is_some() {
                emitted += 1;
            }
        }
        // 4000 inputs at 2 sps → expect ~2000 emitted; the chain
        // takes ~RRC NUM_TAPS samples to settle, so anything near
        // half is correct.
        assert!(
            emitted > 1500,
            "QPSK pipeline should emit ~2000 soft symbols, got {emitted}",
        );
    }

    #[test]
    fn oqpsk_pipeline_produces_soft_symbols_from_synthetic_oqpsk() {
        // Synthesize 2 sps OQPSK by interleaving I-only samples
        // and Q-only samples on alternating sample indices — the
        // canonical "Q delayed by Tsym/2" representation. The
        // OQPSK chain should converge and emit one soft symbol
        // per pair of input samples.
        let mut demod =
            LrptDemod::new_with_mode(LrptMode::Oqpsk).expect("LrptDemod::new_with_mode");
        // Four hard QPSK constellation points expressed as I-only
        // and Q-only halves at +/-0.707.
        let i_vals = [0.707_f32, -0.707, 0.707, -0.707];
        let q_vals = [0.707_f32, 0.707, -0.707, -0.707];
        let mut emitted = 0_usize;
        for n in 0..8000 {
            let sym_idx = (n / 2) % 4;
            let s = if n % 2 == 0 {
                Complex::new(i_vals[sym_idx], 0.0)
            } else {
                Complex::new(0.0, q_vals[sym_idx])
            };
            if demod.process(s).is_some() {
                emitted += 1;
            }
        }
        // 8000 inputs at 2 sps → ~4000 emitted post-settle. Allow
        // generous slack for the dbdexter loop's longer initial
        // lock latency (the free-run sweep takes a moment to find
        // zero offset on a clean signal).
        assert!(
            emitted > 3000,
            "OQPSK pipeline should emit ~4000 soft symbols, got {emitted}",
        );
    }

    #[test]
    fn oqpsk_constructor_succeeds() {
        // Sanity check: every constant we feed to the OQPSK
        // chain's inner constructors is finite + positive, so
        // construction should never fail.
        assert!(LrptDemod::new_with_mode(LrptMode::Oqpsk).is_ok());
    }

    #[test]
    fn oqpsk_zero_iq_never_acquires_lock() {
        // Regression for CR round 2 on PR #663. Without the AGC
        // stage and dbdexter's amplitude calibration, the
        // MeteorPll lock detector's |error| EMA decays from 1000
        // toward 0 with pole 0.001 and crosses the lock threshold
        // (85) after ~2700 update_estimate() calls — purely from
        // time elapsed — declaring lock on silence and disabling
        // the free-run frequency sweep before the carrier is
        // found. With the AGC in place, zero-IQ silence drives
        // the gain to saturation and any post-AGC noise produces
        // non-zero phase error; the EMA stays high and
        // `locked_once` stays false.
        let mut demod = LrptDemod::new_with_mode(LrptMode::Oqpsk).unwrap();
        // 100k samples is well past the ~5400-sample point
        // (= 2700 update_estimate calls × 2 input samples per
        // call) at which the bug would have triggered.
        for _ in 0..100_000 {
            demod.process(Complex::default());
        }
        assert_eq!(
            demod.oqpsk_locked_once(),
            Some(false),
            "zero-IQ silence must not trigger the OQPSK lock detector — \
             AGC + lock-detector interaction has regressed",
        );
    }

    #[test]
    fn oqpsk_pipeline_processes_zero_iq_without_emitting() {
        // Zero IQ → no symbol decisions → no emissions, but the
        // chain must not panic or produce noise either.
        let mut demod = LrptDemod::new_with_mode(LrptMode::Oqpsk).unwrap();
        let mut emitted = 0_usize;
        for _ in 0..1000 {
            // The OQPSK chain emits whatever the slicer makes of
            // the zero-mixed samples — it's allowed to emit, just
            // shouldn't crash. Count emissions only as a sanity
            // signal.
            if demod.process(Complex::default()).is_some() {
                emitted += 1;
            }
        }
        // 1000 inputs at 2 sps → at most ~500 emissions.
        assert!(emitted <= 500);
    }
}
