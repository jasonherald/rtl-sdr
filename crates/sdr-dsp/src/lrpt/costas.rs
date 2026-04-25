//! QPSK Costas loop for Meteor-M LRPT carrier recovery.
//!
//! Locks onto the suppressed QPSK carrier by computing phase error
//! from the rotated samples and driving SDR++'s 2nd-order
//! [`PhaseControlLoop`]. Reuses the existing critically-damped loop
//! primitive from [`crate::loops`] rather than re-deriving alpha /
//! beta inline — `PhaseControlLoop::critically_damped(bw)` is the
//! single source of truth for the project's loop math.
//!
//! Reference (read-only):
//! `original/SDRPlusPlus/decoder_modules/meteor_demodulator/src/meteor_costas.h`

use core::f32::consts::PI;

use sdr_types::{Complex, DspError};

use crate::loops::PhaseControlLoop;

/// QPSK Costas loop. Single-instance, single-threaded — caller
/// hands in IQ samples and gets back de-rotated samples.
pub struct Costas {
    pcl: PhaseControlLoop,
}

impl Costas {
    /// Build a QPSK Costas loop with normalized loop bandwidth
    /// `loop_bw` (cycles per sample). Meteor's working value is
    /// `0.005` per SDR++ `meteor_demodulator/src/main.cpp`.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `loop_bw` is not
    /// finite or not positive — `critically_damped` would otherwise
    /// fold NaN into the alpha/beta coefficients and silently
    /// corrupt every later `process` call.
    pub fn new(loop_bw: f32) -> Result<Self, DspError> {
        if !loop_bw.is_finite() || loop_bw <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "loop_bw must be finite and positive, got {loop_bw}"
            )));
        }
        let (alpha, beta) = PhaseControlLoop::critically_damped(loop_bw);
        let pcl = PhaseControlLoop::new(
            alpha, beta, 0.0, // initial phase
            -PI, // min phase — wraps to ±π
            PI,  // max phase
            0.0, // initial freq
            -PI, // min freq (rad/sample)
            PI,  // max freq
        )?;
        Ok(Self { pcl })
    }

    /// De-rotate one IQ sample. Updates the internal phase / freq
    /// estimate, returns the rotated sample.
    pub fn process(&mut self, sample: Complex) -> Complex {
        // NCO: rotate by -phase to derotate the input. Manual
        // construction avoids pulling in a polar constructor we
        // don't need anywhere else.
        let nco = Complex::new(self.pcl.phase.cos(), -self.pcl.phase.sin());
        let out = sample * nco;
        // QPSK phase error: hard-decision quadrant gradient. The
        // sign of each axis identifies the nearest constellation
        // point; the gradient drops to zero at the points
        // themselves and grows as the rotated sample drifts.
        let err = out.re.signum() * out.im - out.im.signum() * out.re;
        self.pcl.advance(err);
        out
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::*;

    /// Normalized Costas BW per SDR++ `meteor_demodulator/src/main.cpp`
    /// (= 0.005 cycles/sample at the standard 144 ksps demod rate,
    /// = 720 Hz of effective loop bandwidth).
    const TEST_LOOP_BW: f32 = 0.005;

    #[test]
    fn locks_onto_clean_qpsk_constellation() {
        // Synthesize a clean QPSK signal at zero frequency offset.
        // After Costas settles, the rotated output magnitude
        // should still be ~1 (de-rotation preserves magnitude).
        let symbols = [
            Complex::new(0.707, 0.707),
            Complex::new(-0.707, 0.707),
            Complex::new(0.707, -0.707),
            Complex::new(-0.707, -0.707),
        ];
        let mut costas = Costas::new(TEST_LOOP_BW).expect("Costas::new");
        let mut last_out = Complex::new(0.0, 0.0);
        // Settling time ~3 / loop_bw_norm ≈ 8640 samples; pump
        // 10k to be safely past lock.
        for i in 0..10_000 {
            last_out = costas.process(symbols[i % 4]);
        }
        let mag = (last_out.re * last_out.re + last_out.im * last_out.im).sqrt();
        assert!(
            (mag - 1.0).abs() < 0.01,
            "post-lock magnitude {mag} should be ~1; Costas isn't preserving sample magnitude",
        );
    }

    #[test]
    fn corrects_small_frequency_offset() {
        // Inject 100 Hz of carrier offset at 144 ksps. The Costas
        // loop should track it; post-lock output phase variance
        // should be small.
        let offset_hz = 100.0_f32;
        let fs = 144_000.0_f32;
        let mut costas = Costas::new(TEST_LOOP_BW).expect("Costas::new");
        let mut output_phases: Vec<f32> = Vec::new();
        let symbol = Complex::new(0.707, 0.707);
        for i in 0..20_000 {
            let phase = 2.0 * PI * offset_hz * (i as f32) / fs;
            let rotator = Complex::new(phase.cos(), phase.sin());
            let s = symbol * rotator;
            let out = costas.process(s);
            if i > 15_000 {
                output_phases.push(out.im.atan2(out.re));
            }
        }
        let mean: f32 = output_phases.iter().sum::<f32>() / output_phases.len() as f32;
        let var: f32 = output_phases
            .iter()
            .map(|p| (p - mean).powi(2))
            .sum::<f32>()
            / output_phases.len() as f32;
        assert!(
            var < 0.01,
            "post-lock phase variance {var} too high; Costas isn't tracking 100 Hz offset",
        );
    }
}
