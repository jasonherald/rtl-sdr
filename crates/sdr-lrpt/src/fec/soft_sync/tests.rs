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
