use super::*;
use core::f32::consts::PI;

// ─── CTCSS threshold test fixtures ──────────────────────────
// Per project convention, test magic numbers (thresholds,
// tolerances, invalid-input lists) are named constants. These
// feed `test_radio_module_ctcss_threshold_*` — if the DSP
// layer's threshold range ever changes, there's one place to
// tune the test data.

/// Float tolerance for CTCSS threshold round-trip equality.
/// `1e-6` comfortably exceeds f32 rounding error for the
/// single-assignment round-trips the tests exercise.
const CTCSS_TEST_EPS: f32 = 1e-6;

/// Non-default value used by the persistence test. Chosen
/// strictly inside the DSP-layer `(0, 1]` range and clearly
/// different from the `CTCSS_DEFAULT_THRESHOLD` (0.1) so a
/// regression that silently reverts to the default fails
/// loudly.
const CTCSS_PERSIST_THRESHOLD: f32 = 0.25;

/// "Last-good" baseline used by the rejection test. Any
/// in-range value would work; 0.2 is distinct from both the
/// DSP default (0.1) and the persistence test's 0.25 so
/// cross-test contamination would be noticeable.
const CTCSS_LAST_GOOD_THRESHOLD: f32 = 0.2;

/// Values that `set_ctcss_threshold` must reject. Covers the
/// boundary cases (0.0, just over 1.0), a sub-zero, and all
/// three non-finite IEEE-754 values. Used by
/// `test_radio_module_ctcss_threshold_rejects_invalid`.
const INVALID_CTCSS_THRESHOLDS: [f32; 6] =
    [0.0, -0.1, 1.001, f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

// ─── Voice-squelch test fixtures ────────────────────────────
// Same "named constants with rationale" pattern as CTCSS.
// These feed `test_radio_module_voice_squelch_*`; a future
// DSP retune of the default thresholds or the accepted range
// should touch these in one place rather than hunting down
// bare literals scattered across the tests.

/// Non-default Syllabic threshold used by the persistence
/// test. Chosen inside the DSP-layer `(0, 1]` range and
/// clearly different from
/// `VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD` (0.15) so a
/// regression that silently reverts to the default fails
/// loudly. Also distinct from
/// `VS_SYLLABIC_TUNED_THRESHOLD` below so the two syllabic
/// tests can't contaminate each other through shared state.
const VS_SYLLABIC_PERSIST_THRESHOLD: f32 = 0.22;

/// Non-default Snr threshold (dB) used by the persistence
/// test's Snr gauntlet. Chosen inside the 0–20 dB UI range
/// and clearly above `VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB`
/// (6.0) so a regression that reverts to the default fails
/// loudly.
const VS_SNR_PERSIST_THRESHOLD_DB: f32 = 9.0;

/// Construction baseline for the threshold-updates-cached-mode
/// test. Equals `VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD`
/// — we start the test at the default so the `set_voice_squelch_mode`
/// call exercises the default-construction path.
const VS_SYLLABIC_BASELINE_THRESHOLD: f32 = 0.15;

/// Tuned Syllabic threshold for the threshold-updates-cached-
/// mode test. Distinct from BOTH
/// `VS_SYLLABIC_BASELINE_THRESHOLD` (so the update is
/// observable) AND `VS_SYLLABIC_PERSIST_THRESHOLD` (so the
/// two syllabic tests are independent).
const VS_SYLLABIC_TUNED_THRESHOLD: f32 = 0.30;

/// FM-modulated audio tone at the NFM IF rate (50 kHz), for the
/// pre-gate tests: the demod output is a clean tone the gates would
/// otherwise hide.
fn fm_tone_iq(count: usize, amplitude: f32) -> Vec<Complex> {
    const IF_RATE_HZ: f32 = 50_000.0;
    const TONE_HZ: f32 = 1_000.0;
    const DEVIATION_HZ: f32 = 3_000.0;
    let mut phase = 0.0_f32;
    (0..count)
        .map(|i| {
            let t = i as f32 / IF_RATE_HZ;
            let inst_freq = DEVIATION_HZ * (2.0 * PI * TONE_HZ * t).sin();
            phase += 2.0 * PI * inst_freq / IF_RATE_HZ;
            Complex::new(amplitude * phase.cos(), amplitude * phase.sin())
        })
        .collect()
}

fn peak(s: &[Stereo]) -> f32 {
    s.iter()
        .map(|v| v.l.abs().max(v.r.abs()))
        .fold(0.0_f32, f32::max)
}

// ─── Voice squelch persistence regression tests ─────────
//
// Mirror the CTCSS dual-level assertion pattern: after each
// mode switch, assert both the RadioModule cache AND the
// live AfChain value so a broken reapply path (cache
// updated but af_chain not) can't hide behind the cached
// field alone. Tests three transitions (Off → Syllabic,
// Syllabic → Snr, mode switch) to cover the
// reconstruct-the-AF-chain-on-set_mode code path.

mod ctcss_voice_squelch;
mod modes;
mod squelch;
