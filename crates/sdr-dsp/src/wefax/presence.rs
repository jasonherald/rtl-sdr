//! Fax-subcarrier presence detector: is a WEFAX signal on this channel?
//!
//! A Goertzel band-energy ratio over the 1500-2300 Hz subcarrier band vs
//! out-of-band bins, with hysteresis. Used to pick a channel (auto-catch)
//! and to reject broadband noise/static before starting an image. Pure; no
//! I/O.
//!
//! This is a band-energy measure, not sweep-aware: it cannot distinguish a
//! genuine FM-modulated fax subcarrier from a steady in-band tone/carrier, so
//! a strong stationary carrier can still false-latch "present" in this v1.
//! Sweep-aware discrimination, plus re-validating `PRESENT_RATIO`/`ON_BLOCKS`
//! against more diverse real fixtures (the current constants were tuned
//! against a single real NMF clip), are tracked in issue #914.

use super::{SUBCARRIER_BLACK_HZ, SUBCARRIER_CENTER_HZ, SUBCARRIER_WHITE_HZ};

/// Samples per Goertzel block (matches `tones.rs`).
const BLOCK_LEN: usize = 1024;
/// In-band/total mean-power fraction above which a block counts as "fax".
/// The ratio is computed from *mean* power per probe bin (see
/// `mean_power`), not summed, so a flat/noise spectrum centers near 0.5
/// rather than being biased toward "in-band" purely by having more
/// in-band probes (5) than out-of-band ones (2).
///
/// 0.60 was picked empirically against real recorded audio
/// (`tests/wefax_presence.rs`), by scanning per-block ratio traces from a
/// real ~12 s NMF fax capture and a real ~12 s recorded white-noise clip and
/// choosing the threshold that maximizes the joint margin between (a) the
/// longest run of consecutive on-ratio blocks in the noise trace and (b) the
/// longest such run in the fax trace, relative to `ON_BLOCKS`: at 0.60 the
/// real fax audio sustains a 20-block run (vs. the 12 required), while real
/// and synthetic noise never exceed a 6-block run — an 8-block and 6-block
/// margin respectively. A single 1024-sample Goertzel block's power estimate
/// is noisy even for true white noise (any individual block can spike high),
/// but real FM-modulated fax content holds a high ratio far more
/// consistently than noise's occasional spikes can chain together.
const PRESENT_RATIO: f64 = 0.60;
/// Consecutive on-blocks to latch present: ~1 s at 12 kHz
/// (`BLOCK_LEN` / `SR` = 1024/12000 ≈ 85.3 ms/block, so 12 blocks ≈ 1.024 s).
/// This is the primary noise-rejection mechanism (see `PRESENT_RATIO`): a
/// real subcarrier holds a high ratio for many consecutive blocks, while
/// noise's occasional high-ratio blocks essentially never chain to 12 in a
/// row. This also keeps a brief blip (a birdie, a noise spike, a moment of
/// another signal) from ever latching "present" on its own.
const ON_BLOCKS: u32 = 12;
/// Consecutive off-blocks to drop present (~2 s at 12 kHz) — tolerates
/// brief in-band dropouts (sync gaps, weak-signal fades) without
/// unlatching a genuine fax signal.
const OFF_BLOCKS: u32 = 22;
/// Out-of-band probes: below and above the fax band.
const OUT_BAND_HZ: [f64; 2] = [700.0, 3_200.0];

/// In-band probe frequencies spanning the subcarrier.
fn in_band_hz() -> [f64; 5] {
    [
        SUBCARRIER_BLACK_HZ,
        f64::midpoint(SUBCARRIER_BLACK_HZ, SUBCARRIER_CENTER_HZ),
        SUBCARRIER_CENTER_HZ,
        f64::midpoint(SUBCARRIER_CENTER_HZ, SUBCARRIER_WHITE_HZ),
        SUBCARRIER_WHITE_HZ,
    ]
}

/// One Goertzel single-bin accumulator over a `BLOCK_LEN` block.
struct Goertzel {
    coeff: f64,
    q1: f64,
    q2: f64,
}

// `BLOCK_LEN` (1024) is far below `f64`'s exact-integer range, so the
// cast below never loses precision in practice.
#[allow(clippy::cast_precision_loss)]
impl Goertzel {
    fn new(sample_rate_hz: f64, target_hz: f64) -> Self {
        let k = (target_hz / sample_rate_hz * BLOCK_LEN as f64).round();
        let w = 2.0 * std::f64::consts::PI * k / BLOCK_LEN as f64;
        Self {
            coeff: 2.0 * w.cos(),
            q1: 0.0,
            q2: 0.0,
        }
    }

    fn push(&mut self, s: f64) {
        let q0 = self.coeff * self.q1 - self.q2 + s;
        self.q2 = self.q1;
        self.q1 = q0;
    }

    /// Bin power, then reset for the next block.
    fn take_power(&mut self) -> f64 {
        let p = self.q1 * self.q1 + self.q2 * self.q2 - self.coeff * self.q1 * self.q2;
        self.q1 = 0.0;
        self.q2 = 0.0;
        p.max(0.0)
    }
}

/// Mean per-bin power across a probe group, draining each bin's accumulator.
#[allow(clippy::cast_precision_loss)]
fn mean_power(bins: &mut [Goertzel]) -> f64 {
    let sum: f64 = bins.iter_mut().map(Goertzel::take_power).sum();
    sum / bins.len() as f64
}

/// Hysteresis-gated fax-subcarrier presence detector.
pub struct WefaxPresenceDetector {
    in_band: Vec<Goertzel>,
    out_band: Vec<Goertzel>,
    n: usize,
    on_streak: u32,
    off_streak: u32,
    present: bool,
}

impl WefaxPresenceDetector {
    /// Build a detector for the given audio sample rate.
    #[must_use]
    pub fn new(sample_rate_hz: f64) -> Self {
        Self {
            in_band: in_band_hz()
                .iter()
                .map(|&f| Goertzel::new(sample_rate_hz, f))
                .collect(),
            out_band: OUT_BAND_HZ
                .iter()
                .map(|&f| Goertzel::new(sample_rate_hz, f))
                .collect(),
            n: 0,
            on_streak: 0,
            off_streak: 0,
            present: false,
        }
    }

    /// Feed audio samples; returns the current hysteresis-gated presence.
    pub fn update(&mut self, samples: &[f32]) -> bool {
        for &s in samples {
            let s = f64::from(s);
            for g in &mut self.in_band {
                g.push(s);
            }
            for g in &mut self.out_band {
                g.push(s);
            }
            self.n += 1;
            if self.n >= BLOCK_LEN {
                self.evaluate_block();
                self.n = 0;
            }
        }
        self.present
    }

    /// Current gated presence without feeding samples.
    #[must_use]
    pub fn is_present(&self) -> bool {
        self.present
    }

    /// Clear all state (call when retuning to a new channel).
    pub fn reset(&mut self) {
        for g in self.in_band.iter_mut().chain(self.out_band.iter_mut()) {
            let _ = g.take_power();
        }
        self.n = 0;
        self.on_streak = 0;
        self.off_streak = 0;
        self.present = false;
    }

    fn evaluate_block(&mut self) {
        // Mean power per probe bin, not summed: an unweighted sum would bias
        // the ratio toward "in-band" purely from having more in-band probes
        // (5) than out-of-band probes (2), even for a flat (noise) spectrum.
        let in_mean: f64 = mean_power(&mut self.in_band);
        let out_mean: f64 = mean_power(&mut self.out_band);
        let total = in_mean + out_mean;
        let ratio = if total > 1e-12 { in_mean / total } else { 0.0 };
        if ratio >= PRESENT_RATIO {
            self.on_streak += 1;
            self.off_streak = 0;
            if self.on_streak >= ON_BLOCKS {
                self.present = true;
            }
        } else {
            self.off_streak += 1;
            self.on_streak = 0;
            if self.off_streak >= OFF_BLOCKS {
                self.present = false;
            }
        }
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
    use core::f32::consts::PI;

    const SR: f64 = 12_000.0;

    // Push `secs` of a signal produced by `f(sample_index) -> f32`.
    fn drive(det: &mut WefaxPresenceDetector, secs: f64, f: impl FnMut(usize) -> f32) -> bool {
        let n = (secs * SR) as usize;
        let mut present = false;
        let chunk: Vec<f32> = (0..n).map(f).collect();
        for block in chunk.chunks(256) {
            present = det.update(block);
        }
        present
    }

    #[test]
    fn fax_subcarrier_sweep_is_present() {
        // FM sweep between black(1500) and white(2300), the fax subcarrier.
        // Phase is accumulated per-sample (phase += 2*pi*inst/SR) rather than
        // computed as `2*pi*inst*t`, which is NOT a valid FM integral: the
        // instantaneous frequency of sin(2*pi*inst(t)*t) is inst + t*inst'(t),
        // not inst(t), so it drifts increasingly out of band as t grows. The
        // accumulator form's instantaneous frequency is exactly `inst`.
        let mut det = WefaxPresenceDetector::new(SR);
        let mut phase = 0.0f32;
        let present = drive(&mut det, 3.0, |i| {
            let t = i as f32 / SR as f32;
            let inst = 1900.0 + 400.0 * (2.0 * PI * 2.0 * t).sin(); // sweeps 1500..2300
            phase += 2.0 * PI * inst / SR as f32;
            phase.sin()
        });
        assert!(present, "a fax-band FM subcarrier must read as present");
    }

    #[test]
    fn white_noise_is_absent() {
        let mut det = WefaxPresenceDetector::new(SR);
        let mut seed = 0x1234_5678u32;
        let present = drive(&mut det, 3.0, |_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
        });
        assert!(!present, "broadband noise must not read as present");
    }

    #[test]
    fn off_band_tone_is_absent() {
        let mut det = WefaxPresenceDetector::new(SR);
        let present = drive(&mut det, 3.0, |i| {
            let t = i as f32 / SR as f32;
            (2.0 * PI * 3_500.0 * t).sin() // above the fax band
        });
        assert!(!present, "an out-of-band tone must not read as present");
    }

    #[test]
    fn brief_blip_does_not_trigger_but_sustained_does() {
        // 0.1 s of fax then silence should NOT latch present (hysteresis).
        let mut det = WefaxPresenceDetector::new(SR);
        let blip = drive(&mut det, 0.1, |i| {
            let t = i as f32 / SR as f32;
            (2.0 * PI * 1_900.0 * t).sin()
        });
        assert!(!blip, "a brief blip must not latch present");

        // ~2 s of a valid in-band FM sweep (same corrected per-sample phase
        // accumulator as `fax_subcarrier_sweep_is_present`) on a *fresh*
        // detector SHOULD latch present, i.e. the "sustained does" half.
        let mut det = WefaxPresenceDetector::new(SR);
        let mut phase = 0.0f32;
        let sustained = drive(&mut det, 2.0, |i| {
            let t = i as f32 / SR as f32;
            let inst = 1900.0 + 400.0 * (2.0 * PI * 2.0 * t).sin();
            phase += 2.0 * PI * inst / SR as f32;
            phase.sin()
        });
        assert!(
            sustained,
            "a sustained fax-band subcarrier must latch present"
        );
    }
}
