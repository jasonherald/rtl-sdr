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
mod tests;
