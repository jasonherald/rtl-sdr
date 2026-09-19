//! Goertzel single-frequency detector for the 300 Hz start / 450 Hz stop
//! tones. Reports "present" when the target bin dominates the block energy
//! for several consecutive blocks (rejects the 1500-2300 Hz image band).

/// Samples per Goertzel block — a fixed sample count, so the block
/// duration scales with the decoder's configured input rate (~21 ms at
/// the app's 48 kHz audio rate, ~43 ms at a 24 kHz IF rate).
const BLOCK_LEN: usize = 1024;
/// Target-bin power / total block power to count a block as "on-tone".
const TONE_PRESENT_RATIO: f64 = 0.30;
/// Consecutive on-tone blocks required to declare the tone present.
const TONE_MIN_BLOCKS: u32 = 8;

pub(crate) struct ToneDetector {
    coeff: f64,
    q0: f64,
    q1: f64,
    q2: f64,
    energy: f64,
    n: usize,
    on_blocks: u32,
}

// `BLOCK_LEN` (1024) is far below `f64`'s exact-integer range, so the
// casts below never lose precision in practice.
#[allow(clippy::cast_precision_loss)]
impl ToneDetector {
    pub(crate) fn new(sr: f64, target_hz: f64) -> Self {
        let k = (target_hz / sr * BLOCK_LEN as f64).round();
        let w = 2.0 * std::f64::consts::PI * k / BLOCK_LEN as f64;
        Self {
            coeff: 2.0 * w.cos(),
            q0: 0.0,
            q1: 0.0,
            q2: 0.0,
            energy: 0.0,
            n: 0,
            on_blocks: 0,
        }
    }

    /// Feed one sample; returns true once the tone is confirmed present.
    pub(crate) fn push(&mut self, s: f64) -> bool {
        self.q0 = self.coeff * self.q1 - self.q2 + s;
        self.q2 = self.q1;
        self.q1 = self.q0;
        self.energy += s * s;
        self.n += 1;
        if self.n < BLOCK_LEN {
            return false;
        }
        self.evaluate_block()
    }

    /// Finalize the current block: compute the target-bin power ratio,
    /// reset accumulators, and update the consecutive on-tone streak.
    fn evaluate_block(&mut self) -> bool {
        let power = self.q1 * self.q1 + self.q2 * self.q2 - self.coeff * self.q1 * self.q2;
        let ratio = if self.energy > 1e-9 {
            power / (self.energy * BLOCK_LEN as f64 / 2.0)
        } else {
            0.0
        };
        self.q0 = 0.0;
        self.q1 = 0.0;
        self.q2 = 0.0;
        self.energy = 0.0;
        self.n = 0;
        if ratio >= TONE_PRESENT_RATIO {
            self.on_blocks += 1;
        } else {
            self.on_blocks = 0;
        }
        self.on_blocks >= TONE_MIN_BLOCKS
    }
}
