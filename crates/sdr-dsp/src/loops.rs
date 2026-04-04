//! Control loop processors: AGC, PLL, phase control loop.
//!
//! Ports SDR++ `dsp::loop` namespace (named `loops` to avoid Rust keyword conflict).

use sdr_types::{Complex, DspError};

use crate::math;

/// Phase control loop — second-order feedback loop for phase/frequency tracking.
///
/// Ports SDR++ `dsp::loop::PhaseControlLoop`. Used internally by the PLL.
/// Implements a critically-damped second-order loop with configurable alpha/beta.
pub struct PhaseControlLoop {
    alpha: f32,
    beta: f32,
    /// Current phase estimate.
    pub phase: f32,
    /// Current frequency estimate.
    pub freq: f32,
    min_phase: f32,
    max_phase: f32,
    min_freq: f32,
    max_freq: f32,
    phase_delta: f32,
}

impl PhaseControlLoop {
    /// Create a new phase control loop.
    #[allow(clippy::too_many_arguments)]
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `max_phase <= min_phase` or `max_freq <= min_freq`.
    pub fn new(
        alpha: f32,
        beta: f32,
        phase: f32,
        min_phase: f32,
        max_phase: f32,
        freq: f32,
        min_freq: f32,
        max_freq: f32,
    ) -> Result<Self, DspError> {
        if max_phase <= min_phase {
            return Err(DspError::InvalidParameter(format!(
                "max_phase ({max_phase}) must be > min_phase ({min_phase})"
            )));
        }
        if max_freq <= min_freq {
            return Err(DspError::InvalidParameter(format!(
                "max_freq ({max_freq}) must be > min_freq ({min_freq})"
            )));
        }
        Ok(Self {
            alpha,
            beta,
            phase,
            min_phase,
            max_phase,
            min_freq,
            max_freq,
            freq,
            phase_delta: max_phase - min_phase,
        })
    }

    /// Compute critically-damped loop coefficients from bandwidth.
    ///
    /// Ports SDR++ `PhaseControlLoop::criticallyDamped`.
    pub fn critically_damped(bandwidth: f32) -> (f32, f32) {
        let damping = core::f32::consts::FRAC_1_SQRT_2;
        let denom = 1.0 + 2.0 * damping * bandwidth + bandwidth * bandwidth;
        let alpha = (4.0 * damping * bandwidth) / denom;
        let beta = (4.0 * bandwidth * bandwidth) / denom;
        (alpha, beta)
    }

    /// Advance the loop by one step given a phase error.
    #[inline]
    pub fn advance(&mut self, error: f32) {
        self.freq += self.beta * error;
        self.clamp_freq();
        self.phase += self.freq + self.alpha * error;
        self.clamp_phase();
    }

    fn clamp_freq(&mut self) {
        self.freq = self.freq.clamp(self.min_freq, self.max_freq);
    }

    fn clamp_phase(&mut self) {
        while self.phase > self.max_phase {
            self.phase -= self.phase_delta;
        }
        while self.phase < self.min_phase {
            self.phase += self.phase_delta;
        }
    }
}

/// Automatic Gain Control — normalizes signal amplitude.
///
/// Ports SDR++ `dsp::loop::AGC`. Uses separate attack/decay time constants
/// for fast response to level increases and slow release on decreases.
/// Includes look-ahead clipping prevention.
pub struct Agc {
    set_point: f32,
    attack: f32,
    inv_attack: f32,
    decay: f32,
    inv_decay: f32,
    max_gain: f32,
    max_output_amp: f32,
    init_gain: f32,
    amp: f32,
}

impl Agc {
    /// Create a new AGC.
    ///
    /// - `set_point`: target output amplitude
    /// - `attack`: attack coefficient (0 to 1, higher = faster response to increases)
    /// - `decay`: decay coefficient (0 to 1, higher = faster response to decreases)
    /// - `max_gain`: maximum allowed gain
    /// - `max_output_amp`: clipping threshold
    /// - `init_gain`: initial gain value
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if parameters are invalid.
    pub fn new(
        set_point: f32,
        attack: f32,
        decay: f32,
        max_gain: f32,
        max_output_amp: f32,
        init_gain: f32,
    ) -> Result<Self, DspError> {
        if set_point <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "set_point must be positive, got {set_point}"
            )));
        }
        if init_gain <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "init_gain must be positive, got {init_gain}"
            )));
        }
        if !(0.0..=1.0).contains(&attack) {
            return Err(DspError::InvalidParameter(format!(
                "attack must be in [0, 1], got {attack}"
            )));
        }
        if !(0.0..=1.0).contains(&decay) {
            return Err(DspError::InvalidParameter(format!(
                "decay must be in [0, 1], got {decay}"
            )));
        }
        Ok(Self {
            set_point,
            attack,
            inv_attack: 1.0 - attack,
            decay,
            inv_decay: 1.0 - decay,
            max_gain,
            max_output_amp,
            init_gain,
            amp: set_point / init_gain,
        })
    }

    /// Reset the AGC state to initial gain.
    pub fn reset(&mut self) {
        self.amp = self.set_point / self.init_gain;
    }

    /// Process complex samples through the AGC.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process_complex(
        &mut self,
        input: &[Complex],
        output: &mut [Complex],
    ) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        for (i, &s) in input.iter().enumerate() {
            let in_amp = s.amplitude();
            let gain = self.update_gain(in_amp, i, input.len(), |j| input[j].amplitude());
            output[i] = s * gain;
        }
        Ok(input.len())
    }

    /// Process f32 samples through the AGC.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process_f32(&mut self, input: &[f32], output: &mut [f32]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        for (i, &s) in input.iter().enumerate() {
            let in_amp = s.abs();
            let gain = self.update_gain(in_amp, i, input.len(), |j| input[j].abs());
            output[i] = s * gain;
        }
        Ok(input.len())
    }

    /// Core gain computation with look-ahead clipping prevention.
    fn update_gain<F>(&mut self, in_amp: f32, idx: usize, count: usize, amp_fn: F) -> f32
    where
        F: Fn(usize) -> f32,
    {
        let gain = if in_amp == 0.0 {
            1.0
        } else {
            self.amp = if in_amp > self.amp {
                self.amp * self.inv_attack + in_amp * self.attack
            } else {
                self.amp * self.inv_decay + in_amp * self.decay
            };
            (self.set_point / self.amp).min(self.max_gain)
        };

        // Look-ahead clipping prevention
        if in_amp * gain > self.max_output_amp {
            let mut max_amp = 0.0_f32;
            for j in idx..count {
                let a = amp_fn(j);
                if a > max_amp {
                    max_amp = a;
                }
            }
            self.amp = max_amp;
            return (self.set_point / self.amp).min(self.max_gain);
        }

        gain
    }
}

/// Phase-Locked Loop — tracks phase and frequency of an input signal.
///
/// Ports SDR++ `dsp::loop::PLL`. Outputs a complex phasor locked to the
/// input signal's phase using a second-order phase control loop.
pub struct Pll {
    pcl: PhaseControlLoop,
    init_phase: f32,
    init_freq: f32,
}

impl Pll {
    /// Create a new PLL.
    ///
    /// - `bandwidth`: loop bandwidth (controls lock speed vs noise)
    /// - `init_phase`: initial phase estimate
    /// - `init_freq`: initial frequency estimate (radians/sample)
    /// - `min_freq`: minimum frequency limit
    /// - `max_freq`: maximum frequency limit
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `min_freq >= max_freq`.
    pub fn new(
        bandwidth: f32,
        init_phase: f32,
        init_freq: f32,
        min_freq: f32,
        max_freq: f32,
    ) -> Result<Self, DspError> {
        let (alpha, beta) = PhaseControlLoop::critically_damped(bandwidth);
        let pcl = PhaseControlLoop::new(
            alpha,
            beta,
            init_phase,
            -core::f32::consts::PI,
            core::f32::consts::PI,
            init_freq,
            min_freq,
            max_freq,
        )?;
        Ok(Self {
            pcl,
            init_phase,
            init_freq,
        })
    }

    /// Reset the PLL to initial state.
    pub fn reset(&mut self) {
        self.pcl.phase = self.init_phase;
        self.pcl.freq = self.init_freq;
    }

    /// Current frequency estimate (radians/sample).
    pub fn frequency(&self) -> f32 {
        self.pcl.freq
    }

    /// Current phase estimate.
    pub fn phase(&self) -> f32 {
        self.pcl.phase
    }

    /// Process complex samples — output is the PLL's locked phasor.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process(
        &mut self,
        input: &[Complex],
        output: &mut [Complex],
    ) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        for (i, &s) in input.iter().enumerate() {
            // Output current VCO phasor
            output[i] = Complex::new(self.pcl.phase.cos(), self.pcl.phase.sin());
            // Advance loop with phase error
            let error = math::normalize_phase(s.phase() - self.pcl.phase);
            self.pcl.advance(error);
        }
        Ok(input.len())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    // --- Phase Control Loop tests ---

    #[test]
    fn test_pcl_critically_damped() {
        let (alpha, beta) = PhaseControlLoop::critically_damped(0.1);
        assert!(alpha > 0.0 && alpha < 1.0);
        assert!(beta > 0.0 && beta < 1.0);
        assert!(alpha > beta, "alpha should be > beta for stability");
    }

    // --- AGC tests ---

    #[test]
    fn test_agc_new_invalid() {
        assert!(Agc::new(0.0, 0.1, 0.01, 100.0, 2.0, 1.0).is_err());
        assert!(Agc::new(1.0, 0.1, 0.01, 100.0, 2.0, 0.0).is_err());
    }

    #[test]
    fn test_agc_normalizes_amplitude() {
        let mut agc = Agc::new(1.0, 0.1, 0.01, 1000.0, 10.0, 1.0).unwrap();
        // Input with amplitude 0.1 should be amplified toward set_point 1.0
        let input = vec![Complex::new(0.1, 0.0); 1000];
        let mut output = vec![Complex::default(); 1000];
        agc.process_complex(&input, &mut output).unwrap();
        // After convergence, amplitude should approach set_point
        let last_amp = output[999].amplitude();
        assert!(
            last_amp > 0.5,
            "AGC should amplify toward set_point, got {last_amp}"
        );
    }

    #[test]
    fn test_agc_f32() {
        let mut agc = Agc::new(1.0, 0.1, 0.01, 1000.0, 10.0, 1.0).unwrap();
        let input = vec![0.1_f32; 1000];
        let mut output = vec![0.0_f32; 1000];
        agc.process_f32(&input, &mut output).unwrap();
        let last = output[999].abs();
        assert!(last > 0.5, "AGC should amplify, got {last}");
    }

    #[test]
    fn test_agc_reset() {
        let mut agc = Agc::new(1.0, 0.1, 0.01, 1000.0, 10.0, 1.0).unwrap();
        let input = vec![Complex::new(10.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        agc.process_complex(&input, &mut output).unwrap();
        agc.reset();
        // After reset, gain should be back to initial
        assert!(
            (agc.amp - 1.0).abs() < 0.1,
            "after reset, amp should be ~1.0"
        );
    }

    // --- PLL tests ---

    #[test]
    fn test_pll_locks_to_tone() {
        // Generate a constant-frequency complex tone
        let freq = 0.1_f32; // radians/sample
        let input: Vec<Complex> = (0..2000)
            .map(|i| {
                let phase = freq * i as f32;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();

        let mut pll = Pll::new(0.01, 0.0, 0.0, -PI, PI).unwrap();
        let mut output = vec![Complex::default(); 2000];
        pll.process(&input, &mut output).unwrap();

        // After convergence, PLL frequency should be near the input frequency
        let pll_freq = pll.frequency();
        assert!(
            (pll_freq - freq).abs() < 0.02,
            "PLL should lock to {freq}, got {pll_freq}"
        );
    }

    #[test]
    fn test_pll_output_is_unit_phasor() {
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut pll = Pll::new(0.01, 0.0, 0.0, -PI, PI).unwrap();
        let mut output = vec![Complex::default(); 100];
        pll.process(&input, &mut output).unwrap();
        // Every output should be a unit phasor (amplitude ~1.0)
        for (i, s) in output.iter().enumerate() {
            let amp = s.amplitude();
            assert!(
                (amp - 1.0).abs() < 1e-5,
                "output[{i}] amplitude should be ~1.0, got {amp}"
            );
        }
    }

    #[test]
    fn test_pll_reset() {
        let mut pll = Pll::new(0.01, 0.0, 0.0, -PI, PI).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        pll.process(&input, &mut output).unwrap();
        pll.reset();
        assert!((pll.phase() - 0.0).abs() < 1e-6);
        assert!((pll.frequency() - 0.0).abs() < 1e-6);
    }
}
