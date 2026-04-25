//! Meteor-M LRPT QPSK demodulator (epic #469, stage 1).
//!
//! Pipeline: RRC matched filter → Costas loop → Gardner symbol-
//! timing recovery → hard slicer → soft symbols (i8 ±127). No AGC
//! in v1 — RRC normalization handles unity gain.
//!
//! Each module is small, pure, and unit-testable in isolation. The
//! `LrptDemod` chain wires them together; callers push complex
//! baseband samples and pull soft symbol pairs out.
//!
//! Reference (read-only):
//! `original/SDRPlusPlus/decoder_modules/meteor_demodulator/src/`
//! and `original/meteor_demod/dsp/`.

pub mod costas;
pub mod rrc_filter;
pub mod slicer;
pub mod timing;

pub use costas::Costas;
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

/// Top-level LRPT demodulator chain.
pub struct LrptDemod {
    rrc: RrcFilter,
    costas: Costas,
    gardner: Gardner,
}

impl LrptDemod {
    /// Build a demod chain at the standard Meteor parameters
    /// (2 sps, [`COSTAS_LOOP_BW`], [`GARDNER_OMEGA_GAIN`],
    /// [`GARDNER_MU_GAIN`]).
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` only if the Costas
    /// loop's internal `PhaseControlLoop` rejects the synthesized
    /// alpha/beta — practically unreachable for valid constants.
    pub fn new() -> Result<Self, DspError> {
        Ok(Self {
            rrc: RrcFilter::new(2),
            costas: Costas::new(COSTAS_LOOP_BW)?,
            gardner: Gardner::new(2.0, GARDNER_OMEGA_GAIN, GARDNER_MU_GAIN)?,
        })
    }

    /// Push one complex baseband sample. Returns up to one soft-
    /// symbol pair `[i, q]` when the timing recovery fires a tick.
    pub fn process(&mut self, x: Complex) -> Option<[i8; 2]> {
        let filtered = self.rrc.process(x);
        let derotated = self.costas.process(filtered);
        self.gardner.process(derotated).map(slice_soft)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_produces_soft_symbols_from_synthetic_qpsk() {
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
            "pipeline should emit ~2000 soft symbols, got {emitted}",
        );
    }
}
