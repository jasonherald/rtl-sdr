//! Pre-Viterbi soft-symbol sync detection with QPSK phase
//! ambiguity resolution.
//!
//! After Costas locks, QPSK has a 4-fold phase ambiguity
//! (0°/90°/180°/270°), and the I/Q axis can be swapped on top of
//! that — yielding 8 distinct symbol-mapping orientations. Our
//! original [`super::SyncCorrelator`] (hard-bit, post-Viterbi)
//! only catches the upright-phase orientation, which means
//! ~25% per-acquisition success on Meteor passes (issue #605).
//!
//! [`SoftSyncDetector`] mirrors `medet`'s `corr_correlate`
//! (`original/medet/correlator.pas:174`): build 8 rotated
//! patterns from the encoded ASM ([`ASM_ENCODED`] — the bit
//! string after running `0x1ACFFC1D` through the CCSDS K=7
//! rate-1/2 convolutional encoder), slide a window over the
//! incoming soft samples, score each pattern, and report the
//! best match together with the rotation that matched.
//! [`FecChain`] then applies the inverse rotation to every
//! subsequent soft pair before feeding Viterbi, so Viterbi
//! always sees the canonical orientation regardless of where
//! Costas happened to lock.
//!
//! Reference (read-only): `original/medet/correlator.pas` and
//! dbdexter `meteor_decode/correlator/correlator.c`.
//!
//! [`FecChain`]: super::FecChain

/// Encoded form of the CCSDS attached sync marker (`0x1ACFFC1D`)
/// after passing through the K=7 rate-1/2 convolutional encoder
/// (`crates/sdr-lrpt/src/fec/viterbi.rs::ccsds_encode`), which now
/// uses the on-wire CCSDS generators `0x79` / `0x5B`. 64 bits =
/// 32 QPSK symbols = 64 soft-sample axis components.
///
/// This value is the *canonical* (un-rotated) encoded ASM. The
/// detector builds 8 rotated/swapped variants from it, so the
/// constant only needs to match the encoder — I/Q swap, sign
/// (180°), and 90°/270° phase are all recovered by the rotation
/// search, not baked into this constant.
///
/// **History.** A previous revision stored `0x0391_853E_8FF1_64AB`,
/// computed from the *bit-reversed* encoder (`POLYA = 0x4F`,
/// `POLYB = 0x6D`). That encoder produced a different code than
/// the satellite, so this correlator could never lock onto a real
/// pass. With the corrected wire generators the encoded ASM is
/// `0x035D_49C2_4FF2_686B` (cross-checked: dbdexter's
/// `conv_encode_u32(0x1ACFFC1D)` produces the same code; see
/// `viterbi::tests::ccsds_encode_matches_dbdexter_conv_encode_u32`).
///
/// Derived MSB-first from `ccsds_encode`'s soft output (positive
/// soft = encoder bit 0 → u64 bit 0; negative = bit 1) and pinned
/// by [`super::tests::asm_encoded_matches_ccsds_encode_output`],
/// so it cannot drift away from the encoder again.
pub const ASM_ENCODED: u64 = 0x035D_49C2_4FF2_686B;

/// Number of soft-sample axis components in the encoded ASM
/// (= bits in [`ASM_ENCODED`]).
pub const ASM_ENCODED_BITS: usize = 64;

/// Minimum sign agreement for declaring lock, as the fraction
/// `SOFT_SYNC_AGREEMENT_NUM / SOFT_SYNC_AGREEMENT_DEN` of the
/// window's energy: `Σ sample·sign ≥ 55/64 · Σ |sample|`. This is
/// medet's `corr_limit = 55` of 64 hard bits, carried over to
/// magnitude-weighted soft samples, and it is independent of the
/// soft scale.
///
/// The scale matters: real passes never approach the `127 · 64 =
/// 8128` saturated maximum. The demod AGC targets |sample| = 190,
/// the QPSK rails sit at ~134 and `soft_pair` halves that, so a
/// nominal pass delivers |soft| ≈ 67 and a clean ASM scores about
/// 4288. The previous absolute threshold of 4000 was therefore
/// ~93 % of what a nominal pass can reach (not the 49 % its comment
/// claimed), leaving almost no acquisition margin below ~8 dB
/// Es/N0 (#731).
pub const SOFT_SYNC_AGREEMENT_NUM: i32 = 55;

/// Denominator of the agreement fraction (window length in
/// samples, as in medet's 64-bit correlator). Pinned to
/// [`ASM_ENCODED_BITS`] below.
pub const SOFT_SYNC_AGREEMENT_DEN: i32 = 64;
const _: () = assert!(
    ASM_ENCODED_BITS == 64,
    "agreement denominator is the window length"
);

/// Soft magnitude at or above which a window sample counts as
/// evidence for the [`SOFT_SYNC_MIN_SIGNIFICANT_SAMPLES`] gate.
pub const SOFT_SYNC_SIGNIFICANT_MAGNITUDE: i8 = 8;

/// Minimum number of window samples at or above
/// [`SOFT_SYNC_SIGNIFICANT_MAGNITUDE`] before a lock is declared
/// (48 of 64). The energy-relative agreement is invariant to how
/// the energy is distributed, so a handful of saturated spikes
/// among near-zero samples — reachable at pass edges and after a
/// dropout — would otherwise "agree" with some pattern by
/// construction (CR on PR #804). Real demod output is dense.
pub const SOFT_SYNC_MIN_SIGNIFICANT_SAMPLES: usize = 48;

/// Energy floor below which no lock is declared: a mean soft
/// magnitude under 8 (`8 · 64 = 512` total over the window) is
/// noise-floor silence, where a high agreement fraction is
/// meaningless.
pub const SOFT_SYNC_MIN_ENERGY: i32 = 8 * SOFT_SYNC_AGREEMENT_DEN;

/// Number of rotated patterns to check. 4 base rotations
/// (0°/90°/180°/270°) × 2 (with / without I/Q axis swap) = 8.
/// Matches `medet`'s `pattern_cnt=8`.
pub const ROTATION_COUNT: usize = 8;

/// Symbol-mapping orientation Costas locked at, identified by
/// matching one of [`ROTATION_COUNT`] patterns. Drives the
/// inverse-rotation transform applied to every subsequent soft
/// pair before Viterbi sees it.
///
/// Pattern indices match `medet`'s `corr_init` numbering:
/// 0..3 = base rotations, 4..7 = same rotations after I/Q swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// 0° — identity, Costas locked at the upright phase.
    Zero,
    /// 90° clockwise.
    Rot90,
    /// 180° — both I and Q sign-flipped.
    Rot180,
    /// 270° clockwise (90° counter-clockwise).
    Rot270,
    /// I/Q axis swapped (no rotation on top).
    Swap,
    /// I/Q swapped + 90° rotation.
    SwapRot90,
    /// I/Q swapped + 180° rotation.
    SwapRot180,
    /// I/Q swapped + 270° rotation.
    SwapRot270,
}

impl Rotation {
    /// All eight rotations in pattern-index order.
    pub const ALL: [Rotation; ROTATION_COUNT] = [
        Rotation::Zero,
        Rotation::Rot90,
        Rotation::Rot180,
        Rotation::Rot270,
        Rotation::Swap,
        Rotation::SwapRot90,
        Rotation::SwapRot180,
        Rotation::SwapRot270,
    ];

    /// Build a `Rotation` from its pattern index (0..8).
    /// Returns `None` for out-of-range indices; this only happens
    /// in test code that programmatically iterates indices.
    #[must_use]
    pub fn from_index(idx: usize) -> Option<Self> {
        Self::ALL.get(idx).copied()
    }

    /// Apply the rotation to one soft `(I, Q)` pair, producing the
    /// canonical-orientation pair that Viterbi expects to see.
    ///
    /// This is the **inverse** of the forward transform that
    /// `build_patterns` applies to the upright ASM to build
    /// pattern N. So if `SoftSyncDetector` matches pattern N
    /// (meaning Costas locked at rotation N) and we then call
    /// `Rotation::ALL[N].apply(soft)` on every subsequent soft
    /// pair, the chain sees the un-rotated canonical orientation.
    ///
    /// Derivation. The forward transform `T_N` is what
    /// `rotate_signs` (composed with `swap_iq` for variants 4-7)
    /// applies to build the pattern; that's what the receiver
    /// observes when Costas locked at rotation N. The inverse
    /// `T_N⁻¹` recovers the transmitted pair. For example,
    /// `T_1`: `(i, q) → (q, -i)` (a 90° clockwise rotation in
    /// the I/Q plane), so `T_1⁻¹`: `(i, q) → (-q, i)` (a 90°
    /// counter-clockwise rotation). Doing `T_1⁻¹ ∘ T_1 =
    /// identity` is verified by the
    /// `apply_is_inverse_of_forward` test.
    #[must_use]
    pub fn apply(self, soft: [i8; 2]) -> [i8; 2] {
        // Saturating-negate so `i8::MIN` doesn't overflow when
        // its sign is flipped. `i8::MIN.saturating_neg()` returns
        // `i8::MAX` — the only value affected; for the typical
        // ±127 soft range this is a no-op.
        let neg = |x: i8| x.saturating_neg();
        let [i, q] = soft;
        match self {
            // 0°: identity.
            Rotation::Zero => [i, q],
            // T_1⁻¹ for T_1: (i, q) → (q, -i).
            Rotation::Rot90 => [neg(q), i],
            // T_2⁻¹ for T_2: (i, q) → (-i, -q). Self-inverse.
            Rotation::Rot180 => [neg(i), neg(q)],
            // T_3⁻¹ for T_3: (i, q) → (-q, i).
            Rotation::Rot270 => [q, neg(i)],
            // T_4 = swap: (i, q) → (q, i). Self-inverse.
            Rotation::Swap => [q, i],
            // T_5 = swap then 90° CW: (i, q) → (q, i) → (i, -q).
            // Self-inverse: applying twice returns the input.
            Rotation::SwapRot90 => [i, neg(q)],
            // T_6 = swap then 180°: (i, q) → (q, i) → (-q, -i).
            // T_6⁻¹: (a, b) → (-b, -a). Same form as T_6 because
            // (-q, -i) re-applied gives (-(-i), -(-q)) = (i, q).
            Rotation::SwapRot180 => [neg(q), neg(i)],
            // T_7 = swap then 270°: (i, q) → (q, i) → (-i, q).
            // Self-inverse.
            Rotation::SwapRot270 => [neg(i), q],
        }
    }
}

/// Streaming soft-sample sync detector with QPSK phase-ambiguity
/// resolution. Push one soft i8 sample at a time (one axis
/// component, not a pair); on any push that completes a 64-sample
/// window where one of 8 rotated ASM patterns scores above
/// the agreement limit, returns `Some(rotation)` and resets
/// for the next hunt.
///
/// Cost: 8 × 64 = 512 multiply-adds per pushed sample during
/// hunting. At LRPT's 144 ksym/s × 2 axis components = 288 k
/// samples/s, that's ~150 M ops/s — about 5 % of one modern
/// CPU core during hunting. Hunting is brief (until first ASM
/// found) so steady-state cost is negligible.
pub struct SoftSyncDetector {
    /// Sliding window of the most recent [`ASM_ENCODED_BITS`]
    /// soft samples. Treated as a ring buffer; new samples
    /// overwrite the oldest at `head`.
    window: [i8; ASM_ENCODED_BITS],
    /// Index of the next slot to write in [`Self::window`].
    head: usize,
    /// Number of samples pushed since construction or [`Self::reset`].
    /// Used to suppress matches during the initial fill.
    samples_seen: u64,
    /// 8 rotated ASM patterns, each as a 64-element array of `+1`
    /// or `-1` (i8 value). Built once at construction.
    /// Pattern[k][j] is the expected sign of soft-sample j when
    /// the receiver is at rotation k.
    patterns: [[i8; ASM_ENCODED_BITS]; ROTATION_COUNT],
}

impl Default for SoftSyncDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl SoftSyncDetector {
    /// Build a fresh detector with empty window and pre-computed
    /// rotation patterns.
    #[must_use]
    pub fn new() -> Self {
        Self {
            window: [0_i8; ASM_ENCODED_BITS],
            head: 0,
            samples_seen: 0,
            patterns: build_patterns(),
        }
    }

    /// Reset the window so a fresh hunt begins. Called by
    /// [`FecChain::reset`] between passes and after sync loss.
    pub fn reset(&mut self) {
        self.window = [0_i8; ASM_ENCODED_BITS];
        self.head = 0;
        self.samples_seen = 0;
    }

    /// Drain the current window into a new owned `Vec<i8>` in
    /// oldest-to-newest order, packed as `[i, q]` symbol pairs.
    /// Called by [`super::FecChain`] on rotation lock so the
    /// ASM-containing samples can be replayed through `Viterbi`
    /// (after applying the inverse rotation transform). Without
    /// the replay, the post-Viterbi bit stream would start with
    /// CADU payload instead of the ASM, and the per-CADU
    /// [`super::SyncCorrelator`] would miss the first frame's
    /// boundary entirely.
    ///
    /// Empty when [`Self::push_symbol`] has been called fewer
    /// than `ASM_ENCODED_BITS / 2` times (window not yet full —
    /// not a state the chain transitions out of, since lock
    /// requires a full window).
    #[must_use]
    pub fn drain_window(&self) -> Vec<i8> {
        if self.samples_seen < ASM_ENCODED_BITS as u64 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(ASM_ENCODED_BITS);
        for j in 0..ASM_ENCODED_BITS {
            out.push(self.window[(self.head + j) % ASM_ENCODED_BITS]);
        }
        out
    }

    /// Push one QPSK soft symbol pair `[I, Q]` (the same shape
    /// `Viterbi` consumes). Returns `Some(rotation)` on the push
    /// that completes a window where one of the 8 rotated ASM
    /// patterns reaches the agreement limit.
    ///
    /// **Why pairs, not single samples.** The encoded ASM is 32
    /// QPSK symbols long, naturally aligned to symbol boundaries.
    /// Sliding by one symbol at a time (rather than by individual
    /// axis components) preserves symbol alignment in the window,
    /// so the rotated patterns line up correctly. medet uses
    /// per-byte sliding because its data layout packs 4 symbols
    /// per byte; since our soft samples are one i8 per axis
    /// component, the equivalent finer grain would slide by
    /// half-symbols, which has no physical meaning under QPSK
    /// and would only invite false locks on misaligned data.
    ///
    /// The detector does **not** auto-reset on a hit — the
    /// caller is expected to transition out of hunting state
    /// and either `reset()` later (for a future re-hunt) or
    /// drop the detector entirely.
    pub fn push_symbol(&mut self, soft: [i8; 2]) -> Option<Rotation> {
        self.window[self.head] = soft[0];
        self.window[(self.head + 1) % ASM_ENCODED_BITS] = soft[1];
        self.head = (self.head + 2) % ASM_ENCODED_BITS;
        self.samples_seen += 2;
        if self.samples_seen < ASM_ENCODED_BITS as u64 {
            return None;
        }
        self.best_match()
    }

    /// Score every pattern against the current window. Returns
    /// the rotation with the best score if it reaches the
    /// agreement fraction of the window's energy (and the window
    /// is above [`SOFT_SYNC_MIN_ENERGY`]).
    ///
    /// **Window orientation**: `head` points to the next slot
    /// to write, so the OLDEST sample is at `window[head]` and
    /// the NEWEST is at `window[(head + 63) % 64]`. We
    /// iterate `j = 0..64` and read `window[(head + j) % 64]`
    /// to get oldest-to-newest.
    fn best_match(&self) -> Option<Rotation> {
        let mut best_idx: usize = 0;
        let mut best_score: i32 = i32::MIN;
        let (energy, significant) = self.window_evidence();
        if energy < SOFT_SYNC_MIN_ENERGY || significant < SOFT_SYNC_MIN_SIGNIFICANT_SAMPLES {
            return None;
        }
        for (idx, pattern) in self.patterns.iter().enumerate() {
            let mut score: i32 = 0;
            for (j, &expected) in pattern.iter().enumerate() {
                let sample = self.window[(self.head + j) % ASM_ENCODED_BITS];
                // Score = sum of (sample × expected sign) — high
                // when signs agree, low when they disagree. Soft
                // magnitude weights confidence: a saturated
                // ±127 contributes more than a near-zero sample.
                score += i32::from(sample) * i32::from(expected);
            }
            if score > best_score {
                best_score = score;
                best_idx = idx;
            }
        }
        if best_score * SOFT_SYNC_AGREEMENT_DEN >= energy * SOFT_SYNC_AGREEMENT_NUM {
            Rotation::from_index(best_idx)
        } else {
            None
        }
    }
}

impl SoftSyncDetector {
    /// Total soft energy `Σ |sample|` of the window and the number
    /// of samples at or above [`SOFT_SYNC_SIGNIFICANT_MAGNITUDE`].
    fn window_evidence(&self) -> (i32, usize) {
        self.window.iter().fold((0, 0), |(energy, count), &sample| {
            let magnitude = i32::from(sample).abs();
            (
                energy + magnitude,
                count + usize::from(magnitude >= i32::from(SOFT_SYNC_SIGNIFICANT_MAGNITUDE)),
            )
        })
    }
}

/// Build the 8 rotated ASM patterns. Each pattern is a 64-element
/// array of `+1` or `-1` (as `i8`) representing the expected sign
/// of each soft sample in the encoded ASM at one rotation.
///
/// Patterns 0..3: rotations of the encoded ASM by 0°, 90°, 180°,
/// 270°. Patterns 4..7: same rotations applied to the
/// I/Q-swapped encoded ASM. Mirrors `medet`'s `corr_init` loop
/// at `correlator.pas:157-165`.
fn build_patterns() -> [[i8; ASM_ENCODED_BITS]; ROTATION_COUNT] {
    let mut patterns = [[0_i8; ASM_ENCODED_BITS]; ROTATION_COUNT];
    let base = bits_to_signs(ASM_ENCODED);
    let swapped = swap_iq(base);
    for (k, rot) in [0_usize, 1, 2, 3].iter().enumerate() {
        patterns[k] = rotate_signs(base, *rot);
        patterns[k + 4] = rotate_signs(swapped, *rot);
    }
    patterns
}

/// Convert a 64-bit pattern to a 64-element array of `+1` / `-1`
/// signs (high bit first, i.e. bit 63 → element 0).
///
/// **Sign convention**: bit 0 → +1, bit 1 → -1. Matches
/// `ccsds_encode` (and therefore [`super::ViterbiDecoder`])
/// which encodes encoder output `0` as `+CLEAN_SOFT_MAG` and
/// `1` as `-CLEAN_SOFT_MAG`. Don't flip this without flipping
/// the encoder too — the live chain depends on the patterns,
/// the encoder, and Viterbi's metrics all using the same
/// bit-to-sign mapping.
fn bits_to_signs(bits: u64) -> [i8; ASM_ENCODED_BITS] {
    let mut out = [0_i8; ASM_ENCODED_BITS];
    for (i, slot) in out.iter_mut().enumerate() {
        let bit = (bits >> (ASM_ENCODED_BITS - 1 - i)) & 1;
        *slot = if bit == 0 { 1 } else { -1 };
    }
    out
}

/// Apply an `r × 90°` rotation to a sign array. Treats
/// consecutive sign pairs as `(I, Q)` of one QPSK symbol; for
/// each symbol, rotates by `r × 90°` clockwise.
fn rotate_signs(signs: [i8; ASM_ENCODED_BITS], r: usize) -> [i8; ASM_ENCODED_BITS] {
    let mut out = signs;
    for sym in 0..(ASM_ENCODED_BITS / 2) {
        let i = signs[sym * 2];
        let q = signs[sym * 2 + 1];
        // Forward rotation by r × 90° clockwise (the receiver's
        // perspective): if the transmitter sent (I, Q), at +90°
        // Costas-rotation the receiver sees (Q, -I); at +180°
        // sees (-I, -Q); at +270° sees (-Q, I). We're building
        // the EXPECTED received pattern at each rotation so the
        // detector can correlate against incoming samples.
        //
        // `r & 3` normalises any `usize` input to 0..=3, so the
        // catch-all arm is mathematically unreachable. Folding
        // 0 with `_` keeps the helper panic-free in library code
        // (per CLAUDE.md and CR round 1 on PR #606) — Rust's
        // exhaustiveness checker can't refine the type from the
        // bitmask, so a catch-all is required regardless; making
        // it the identity (= the same body 0 would take) is the
        // safe default.
        let (ri, rq) = match r & 3 {
            1 => (q, -i),
            2 => (-i, -q),
            3 => (-q, i),
            _ => (i, q),
        };
        out[sym * 2] = ri;
        out[sym * 2 + 1] = rq;
    }
    out
}

/// Swap the I and Q axes within each pair. Models the case
/// where the demod chain has the I/Q components reversed (which
/// physically can happen with some SDR hardware or USB cable
/// orientations).
fn swap_iq(signs: [i8; ASM_ENCODED_BITS]) -> [i8; ASM_ENCODED_BITS] {
    let mut out = signs;
    for sym in 0..(ASM_ENCODED_BITS / 2) {
        out[sym * 2] = signs[sym * 2 + 1];
        out[sym * 2 + 1] = signs[sym * 2];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convert a 64-element sign array into 32 saturated soft
    /// pairs ready for `push_symbol`. Used by every test below
    /// to stand in for the QPSK soft slicer's output on a clean
    /// signal.
    fn signs_to_pairs(signs: [i8; ASM_ENCODED_BITS]) -> [[i8; 2]; ASM_ENCODED_BITS / 2] {
        let mut out = [[0_i8; 2]; ASM_ENCODED_BITS / 2];
        for sym in 0..(ASM_ENCODED_BITS / 2) {
            let i = if signs[sym * 2] > 0 { 127 } else { -127 };
            let q = if signs[sym * 2 + 1] > 0 { 127 } else { -127 };
            out[sym] = [i, q];
        }
        out
    }

    /// Push the entire encoded ASM through the detector at the
    /// upright phase. Should match `Rotation::Zero`.
    #[test]
    fn detects_clean_asm_at_zero_rotation() {
        let mut det = SoftSyncDetector::new();
        let signs = bits_to_signs(ASM_ENCODED);
        // Pre-fill with anti-pattern so the first 32 pushes
        // can't fluke a match.
        for _ in 0..(ASM_ENCODED_BITS / 2) {
            let _ = det.push_symbol([0, 0]);
        }
        let mut hit: Option<Rotation> = None;
        for pair in signs_to_pairs(signs) {
            if let Some(r) = det.push_symbol(pair) {
                hit = Some(r);
            }
        }
        assert_eq!(
            hit,
            Some(Rotation::Zero),
            "clean upright ASM must match Rotation::Zero"
        );
    }

    /// Round-trip: push a rotated ASM, detector should report
    /// the matching rotation, applying its `apply()` to the
    /// rotated samples should recover the original soft pair.
    #[test]
    fn detects_each_of_eight_rotations() {
        let signs = bits_to_signs(ASM_ENCODED);
        for (idx, expected_rot) in Rotation::ALL.iter().enumerate() {
            let pattern = match idx {
                0..=3 => rotate_signs(signs, idx),
                4..=7 => rotate_signs(swap_iq(signs), idx - 4),
                _ => unreachable!(),
            };
            let mut det = SoftSyncDetector::new();
            for _ in 0..(ASM_ENCODED_BITS / 2) {
                let _ = det.push_symbol([0, 0]);
            }
            let mut hit: Option<Rotation> = None;
            for pair in signs_to_pairs(pattern) {
                if let Some(r) = det.push_symbol(pair) {
                    hit = Some(r);
                }
            }
            assert_eq!(
                hit,
                Some(*expected_rot),
                "pattern at index {idx} should match {expected_rot:?}",
            );
        }
    }

    /// Real passes run far below saturation: the AGC targets
    /// |sample| = 190, the rails sit at ~134 and `soft_pair` halves
    /// that to ~67; a weaker pass sits lower still. A clean ASM at
    /// ±40 (score 2560) must lock — the old absolute threshold of
    /// 4000 was 93 % of the ~4288 a nominal pass can reach (#731).
    #[test]
    fn detects_clean_asm_at_weak_soft_magnitude() {
        const WEAK_MAGNITUDE: i8 = 40;
        let mut det = SoftSyncDetector::new();
        let signs = bits_to_signs(ASM_ENCODED);
        for _ in 0..(ASM_ENCODED_BITS / 2) {
            let _ = det.push_symbol([0, 0]);
        }
        let mut hit: Option<Rotation> = None;
        for pair in signs_to_pairs(signs) {
            let weak = [
                pair[0].signum() * WEAK_MAGNITUDE,
                pair[1].signum() * WEAK_MAGNITUDE,
            ];
            if let Some(r) = det.push_symbol(weak) {
                hit = Some(r);
            }
        }
        assert_eq!(hit, Some(Rotation::Zero));
    }

    /// Agreement is judged relative to the window's energy, like
    /// medet's 55-of-64 hard-bit limit: a saturated window with 10
    /// sign errors (54 agree, 10 disagree → 44/64 net) scores 5588
    /// in absolute terms but must not lock.
    #[test]
    fn rejects_saturated_window_with_too_many_sign_errors() {
        const SIGN_ERRORS: usize = 10;
        let mut det = SoftSyncDetector::new();
        let mut signs = bits_to_signs(ASM_ENCODED);
        for sign in signs.iter_mut().take(SIGN_ERRORS) {
            *sign = -*sign;
        }
        for _ in 0..(ASM_ENCODED_BITS / 2) {
            let _ = det.push_symbol([0, 0]);
        }
        for pair in signs_to_pairs(signs) {
            assert!(
                det.push_symbol(pair).is_none(),
                "44/64 agreement must not lock"
            );
        }
    }

    /// A perfectly agreeing but near-silent window (mean |soft| of
    /// 2) is below the energy floor and must not lock either.
    #[test]
    fn rejects_near_silent_window() {
        const SILENT_MAGNITUDE: i8 = 2;
        let mut det = SoftSyncDetector::new();
        let signs = bits_to_signs(ASM_ENCODED);
        for _ in 0..(ASM_ENCODED_BITS / 2) {
            let _ = det.push_symbol([0, 0]);
        }
        for pair in signs_to_pairs(signs) {
            let quiet = [
                pair[0].signum() * SILENT_MAGNITUDE,
                pair[1].signum() * SILENT_MAGNITUDE,
            ];
            assert!(det.push_symbol(quiet).is_none());
        }
    }

    /// Five saturated spikes among zeros pass the energy floor
    /// (635 ≥ 512) and agree with some pattern by construction;
    /// the significant-sample gate must reject them.
    #[test]
    fn rejects_sparse_window_of_isolated_spikes() {
        const SPIKES: usize = 5;
        let mut det = SoftSyncDetector::new();
        let signs = bits_to_signs(ASM_ENCODED);
        for _ in 0..(ASM_ENCODED_BITS / 2) {
            let _ = det.push_symbol([0, 0]);
        }
        for (sym, pair) in signs_to_pairs(signs).iter().enumerate() {
            let sparse = [
                if sym * 2 < SPIKES { pair[0] } else { 0 },
                if sym * 2 + 1 < SPIKES { pair[1] } else { 0 },
            ];
            assert!(
                det.push_symbol(sparse).is_none(),
                "sparse window must not lock"
            );
        }
    }

    /// Pure noise (zero soft magnitude) must not trigger a
    /// false sync.
    #[test]
    fn rejects_pure_noise_zero() {
        let mut det = SoftSyncDetector::new();
        for _ in 0..(ASM_ENCODED_BITS * 2) {
            assert!(
                det.push_symbol([0, 0]).is_none(),
                "zero-magnitude noise must not match",
            );
        }
    }

    /// First (`ASM_ENCODED_BITS / 2 - 1`) symbol pushes cannot
    /// return a hit (window not yet full).
    #[test]
    fn no_hits_during_initial_window_fill() {
        let mut det = SoftSyncDetector::new();
        // Push 31 saturated symbols — even if they happened to
        // align with an ASM, the samples_seen guard prevents a hit.
        for i in 0..((ASM_ENCODED_BITS / 2) - 1) {
            assert!(
                det.push_symbol([127, 127]).is_none(),
                "premature hit at symbol {i}",
            );
        }
    }

    /// Apply each rotation to a known soft pair, then apply
    /// the same rotation to itself, and confirm we get the
    /// inverse-rotated pair back. (This pins the `apply()`
    /// table against a sanity property: rotating four times
    /// by 90° in the same direction returns the original.)
    #[test]
    fn rotation_apply_is_consistent() {
        let p = [50_i8, -30_i8];
        // Rotating four times by 90° (each time rotating the
        // result) should return the input.
        let mut x = p;
        for _ in 0..4 {
            x = Rotation::Rot90.apply(x);
        }
        assert_eq!(x, p, "four 90° rotations should compose to identity");
    }

    /// `Rotation::Zero.apply(p) == p` for all `p`.
    #[test]
    fn rotation_zero_is_identity() {
        for i in -127_i8..=127 {
            for q in -127_i8..=127 {
                assert_eq!(Rotation::Zero.apply([i, q]), [i, q]);
            }
        }
    }

    /// `Rotation::Rot180.apply(p) == [-i, -q]` (or saturated
    /// equivalent for `i8::MIN`).
    #[test]
    fn rotation_180_negates_both_axes() {
        // `i8::MIN.saturating_neg() == i8::MAX = 127`, so the
        // edge case `[i8::MIN, i8::MAX]` rotates to `[127, -127]`.
        // Pinned so a future signed-arith refactor that drops
        // the saturating_neg can't silently overflow.
        assert_eq!(
            Rotation::Rot180.apply([i8::MIN, i8::MAX]),
            [127, -127],
            "i8::MIN should saturate to 127 under Rot180; \
             i8::MAX should negate cleanly to -127",
        );
        // Typical soft range is well clear of i8::MIN, so the
        // mapping is just a sign flip.
        assert_eq!(Rotation::Rot180.apply([42, -17]), [-42, 17]);
    }

    /// Apply the forward transform that builds pattern N, then
    /// apply [`Rotation::ALL[N].apply`] — must recover the
    /// original pair for every N. This is the load-bearing
    /// invariant: if `apply` is the wrong direction (or wrong
    /// rotation amount) for any N, the entire decode chain
    /// silently scrambles bits at that rotation.
    #[test]
    fn apply_is_inverse_of_forward() {
        // Forward transform table — must mirror what
        // `build_patterns` does to construct pattern N. If
        // these get out of sync the test would pass while the
        // chain still mis-decoded; pinning explicitly is the
        // safety net against that drift.
        let forward = |n: usize, p: [i8; 2]| -> [i8; 2] {
            let neg = |x: i8| x.saturating_neg();
            let [i, q] = p;
            match n {
                0 => [i, q],
                1 => [q, neg(i)],
                2 => [neg(i), neg(q)],
                3 => [neg(q), i],
                4 => [q, i],
                5 => [i, neg(q)],
                6 => [neg(q), neg(i)],
                7 => [neg(i), q],
                _ => unreachable!(),
            }
        };
        // Sample values across the soft range. Excludes
        // `i8::MIN` because saturating_neg(i8::MIN) = i8::MAX
        // makes the round-trip non-identity at that one point
        // (an unavoidable consequence of two's-complement).
        let samples = [-127_i8, -90, -42, -1, 0, 1, 42, 90, 127];
        for (n, rot) in Rotation::ALL.iter().enumerate() {
            for &i in &samples {
                for &q in &samples {
                    let original = [i, q];
                    let received = forward(n, original);
                    let recovered = rot.apply(received);
                    assert_eq!(
                        recovered, original,
                        "rotation {n} ({rot:?}): forward then apply must \
                         recover original — got {recovered:?} from {original:?} \
                         (received as {received:?})",
                    );
                }
            }
        }
    }

    /// Pattern indices map deterministically to rotations.
    #[test]
    fn pattern_index_round_trip() {
        for (i, r) in Rotation::ALL.iter().enumerate() {
            assert_eq!(Rotation::from_index(i), Some(*r));
        }
        assert_eq!(Rotation::from_index(8), None);
    }

    /// `ASM_ENCODED` must match what our `ccsds_encode` actually
    /// produces for the 32-bit ASM. Pinning so a refactor that
    /// changes the encoder convention (`POLYA` / `POLYB`
    /// ordering, shift-register direction, MSB-first vs
    /// LSB-first input) without simultaneously updating
    /// `ASM_ENCODED` fails loudly here, instead of silently
    /// breaking the `SoftSyncDetector` at runtime.
    #[test]
    fn asm_encoded_matches_ccsds_encode_output() {
        use crate::fec::viterbi::ccsds_encode;
        // Encode the 32-bit ASM, MSB first.
        let bits: Vec<u8> = (0..32)
            .map(|i| {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "shift index 0..32 is safe for u32"
                )]
                let bit = ((super::super::sync::ASM >> (31 - i)) & 1) as u8;
                bit
            })
            .collect();
        let encoded = ccsds_encode(&bits);
        // Take the first 64 soft samples (= encoded ASM bits;
        // the trailing K-1 flush samples encode the encoder's
        // tail and aren't part of the ASM proper).
        let mut derived: u64 = 0;
        for (i, &s) in encoded.iter().take(ASM_ENCODED_BITS).enumerate() {
            // Convention: positive soft (= encoder bit 0) → bit
            // 0 in the u64; negative soft (= encoder bit 1) →
            // bit 1. MSB-first packing. `s <= 0` (vs `s > 0`)
            // mirrors the encoder's exact tie-break for the
            // never-occurring zero soft sample.
            let bit = u64::from(s <= 0);
            derived |= bit << (ASM_ENCODED_BITS - 1 - i);
        }
        assert_eq!(
            ASM_ENCODED, derived,
            "ASM_ENCODED constant {ASM_ENCODED:#018x} must match \
             what ccsds_encode produces for the 32-bit ASM \
             ({derived:#018x})",
        );
    }
}
