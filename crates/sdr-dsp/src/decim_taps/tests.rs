use super::*;

/// Number of precomputed decimation plans (ratios 2 through 8192).
const EXPECTED_PLAN_COUNT: usize = 13;
/// Tolerance for tap symmetry checks.
const SYMMETRY_EPSILON: f32 = 1e-9;

#[test]
fn test_plans_count() {
    assert_eq!(PLANS.len(), EXPECTED_PLAN_COUNT);
}

#[test]
fn test_plan_ratios_match() {
    for (i, plan) in PLANS.iter().enumerate() {
        let expected_ratio: usize = 1 << (i + 1);
        let actual_ratio: usize = plan.stages.iter().map(|s| s.decimation).product();
        assert_eq!(
            actual_ratio, expected_ratio,
            "plan {i} (ratio {expected_ratio}): stage decimations product to {actual_ratio}"
        );
    }
}

#[test]
fn test_all_taps_non_empty() {
    for (i, plan) in PLANS.iter().enumerate() {
        for (j, stage) in plan.stages.iter().enumerate() {
            assert!(!stage.taps.is_empty(), "plan {i} stage {j} has empty taps");
        }
    }
}

/// Verify all tap tables are symmetric (linear-phase FIR requirement).
#[test]
fn test_all_tap_tables_symmetric() {
    let tables: &[(&str, &[f32])] = &[
        ("FIR_2_2", FIR_2_2),
        ("FIR_4_2", FIR_4_2),
        ("FIR_8_4", FIR_8_4),
        ("FIR_16_8", FIR_16_8),
        ("FIR_32_8", FIR_32_8),
        ("FIR_64_8", FIR_64_8),
        ("FIR_128_16", FIR_128_16),
        ("FIR_256_32", FIR_256_32),
        ("FIR_512_32", FIR_512_32),
        ("FIR_1024_64", FIR_1024_64),
        ("FIR_2048_64", FIR_2048_64),
        ("FIR_4096_64", FIR_4096_64),
        ("FIR_8192_128", FIR_8192_128),
    ];
    for (name, taps) in tables {
        let n = taps.len();
        for i in 0..n / 2 {
            let diff = (taps[i] - taps[n - 1 - i]).abs();
            assert!(
                diff < SYMMETRY_EPSILON,
                "{name} not symmetric at index {i}: diff = {diff}"
            );
        }
    }
}
