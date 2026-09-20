//! Fax-subcarrier presence detector: is a WEFAX signal on this channel?
//!
//! Two Goertzel measures over the 1500-2300 Hz subcarrier band, both gated by
//! hysteresis: an in-band/out-of-band **energy ratio**, and a **tonality**
//! (peakiness) test. Used to pick a channel (auto-catch) and to reject
//! broadband *and* SSB-passband-shaped noise/static before starting an image.
//! Pure; no I/O.
//!
//! The ratio alone is not enough on a live channel: the WEFAX SSB passband
//! empties the out-of-band probes, so the ratio reads ~1.0 on *anything* in
//! the band, including flat noise. The tonality test (see [`PRESENT_PEAKINESS`])
//! is what actually separates a real FM fax subcarrier — a single tone whose
//! energy concentrates at one probe — from flat band noise, which spreads
//! energy evenly. The one case tonality does *not* reject is a steady in-band
//! carrier/birdie (also tonal); that remains a v1 limitation tracked in #914,
//! along with re-validating the constants against more diverse real fixtures.

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
/// Minimum spectral peakiness (strongest in-band probe ÷ mean in-band probe)
/// for a block to count as "fax". A real FM fax subcarrier puts nearly all
/// its energy at one instantaneous frequency, so one of the five in-band
/// probes dominates (peakiness ~3.5+ on real NMF audio); flat band noise
/// spreads energy evenly across the probes, so its peakiness sits at the
/// flat-spectrum statistical baseline (~2.3 for five bins, and lower for
/// evenly band-limited noise). 3.0 sits in the gap: real NMF fax clears it on
/// 40-60% of blocks (enough to chain [`ON_BLOCKS`]), while broadband *and*
/// SSB-passband-shaped noise clear it on <10%, never chaining. This is the
/// discriminator the ratio gate cannot provide once the SSB passband empties
/// the out-of-band probes and drives the ratio to ~1.0 on any signal (#913).
const PRESENT_PEAKINESS: f64 = 3.0;
/// Consecutive on-blocks to latch present. `BLOCK_LEN`/rate sets the wall
/// time: ≈ 85 ms/block at the 12 kHz test rate (12 blocks ≈ 1.0 s), and
/// ≈ 43 ms/block at the 24 kHz runtime AF rate ([`crate::wefax`] feeds the
/// detector the WEFAX demod's 24 kHz audio), so 12 blocks ≈ 0.5 s live.
/// This is a primary noise-rejection mechanism (with the tonality gate): a
/// real subcarrier holds a high ratio + tonality for many consecutive
/// blocks, while noise's occasional on-blocks essentially never chain to 12
/// in a row. It also keeps a brief blip (a birdie, a noise spike, a moment
/// of another signal) from ever latching "present" on its own.
const ON_BLOCKS: u32 = 12;
/// Consecutive off-blocks to drop present (≈ 1.9 s at 12 kHz, ≈ 0.9 s at the
/// 24 kHz runtime rate) — tolerates brief in-band dropouts (sync gaps,
/// weak-signal fades) without unlatching a genuine fax signal.
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

/// Mean *and* max per-bin power across a probe group, draining each bin's
/// accumulator. The max/mean ratio is the block's spectral peakiness (see
/// [`PRESENT_PEAKINESS`]).
#[allow(clippy::cast_precision_loss)]
fn mean_and_max_power(bins: &mut [Goertzel]) -> (f64, f64) {
    let mut sum = 0.0;
    let mut max = 0.0_f64;
    for b in bins.iter_mut() {
        let p = b.take_power();
        sum += p;
        max = max.max(p);
    }
    (sum / bins.len() as f64, max)
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
        let (in_mean, in_max) = mean_and_max_power(&mut self.in_band);
        let out_mean: f64 = mean_power(&mut self.out_band);
        let total = in_mean + out_mean;
        let ratio = if total > 1e-12 { in_mean / total } else { 0.0 };
        // Tonality gate: a real fax subcarrier is a single moving tone (one
        // probe dominates), flat noise is not. Required on top of the ratio
        // because the SSB passband empties the out-of-band probes on a live
        // channel, so the ratio alone reads ~1.0 on passband-shaped noise.
        let peakiness = if in_mean > 1e-12 {
            in_max / in_mean
        } else {
            0.0
        };
        if ratio >= PRESENT_RATIO && peakiness >= PRESENT_PEAKINESS {
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
        // The sweep is slow (0.2 Hz) so the tone is quasi-stationary over one
        // ~85 ms Goertzel block — matching real fax, which dwells at black/
        // white across large image regions and so reads as a coherent tone
        // (high tonality) per block. A fast sweep would smear the tone across
        // several probes within a single block and under-read the tonality
        // gate ([`PRESENT_PEAKINESS`]), which real fax does not.
        // Phase is accumulated per-sample (phase += 2*pi*inst/SR) rather than
        // computed as `2*pi*inst*t`, which is NOT a valid FM integral: the
        // instantaneous frequency of sin(2*pi*inst(t)*t) is inst + t*inst'(t),
        // not inst(t), so it drifts increasingly out of band as t grows. The
        // accumulator form's instantaneous frequency is exactly `inst`.
        let mut det = WefaxPresenceDetector::new(SR);
        let mut phase = 0.0f32;
        let present = drive(&mut det, 3.0, |i| {
            let t = i as f32 / SR as f32;
            let inst = 1900.0 + 400.0 * (2.0 * PI * 0.2 * t).sin(); // sweeps 1500..2300
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
    fn passband_shaped_noise_is_absent() {
        // Band-limited *flat* noise that fills the fax subcarrier band: its
        // in/out energy ratio reads ~1.0 (the out-of-band probes see nothing),
        // so it sails through the ratio-only gate — this is exactly the live
        // "SSB passband noise" that made the catcher hang on "Signal found"
        // with nothing to draw. It is spectrally flat across the band (no
        // dominant tone), so the tonality gate must read it ABSENT. Modeled as
        // ~60 closely-spaced, random-phase sinusoids spanning 1450-2350 Hz.
        const N_TONES: usize = 60;
        let freqs: [f32; N_TONES] =
            std::array::from_fn(|k| 1450.0 + 900.0 * k as f32 / (N_TONES as f32 - 1.0));
        let mut seed = 0x9E37_79B9u32;
        let phases: [f32; N_TONES] = std::array::from_fn(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 23) as f32 * PI
        });
        let scale = (N_TONES as f32).sqrt();
        let mut det = WefaxPresenceDetector::new(SR);
        let present = drive(&mut det, 3.0, |i| {
            let t = i as f32 / SR as f32;
            let s: f32 = freqs
                .iter()
                .zip(&phases)
                .map(|(&f, &p)| (2.0 * PI * f * t + p).sin())
                .sum();
            s / scale
        });
        assert!(
            !present,
            "flat band-limited (passband-shaped) noise must read absent"
        );
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
            let inst = 1900.0 + 400.0 * (2.0 * PI * 0.2 * t).sin();
            phase += 2.0 * PI * inst / SR as f32;
            phase.sin()
        });
        assert!(
            sustained,
            "a sustained fax-band subcarrier must latch present"
        );
    }
}
