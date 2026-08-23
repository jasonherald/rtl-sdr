use super::*;

#[test]
fn silence_stays_silent() {
    let mut buf = vec![0.0_f32; TEST_SILENCE_LEN];
    spectral_denoise(&mut buf, GATE_RATIO);
    for s in &buf {
        assert!(s.abs() < 1e-6, "expected silence, got {s}");
    }
}

#[test]
fn short_buffer_is_noop() {
    let mut buf = vec![0.5_f32; MIN_FFT_LEN - 1];
    let original = buf.clone();
    spectral_denoise(&mut buf, GATE_RATIO);
    assert_eq!(buf, original);
}

#[test]
fn strong_tone_survives_gate() {
    // Generate a strong 1 kHz tone at 16 kHz sample rate.
    let n = TEST_SIGNAL_LEN;
    let mut buf: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / 16_000.0;
            (2.0 * std::f32::consts::PI * 1000.0 * t).sin()
        })
        .collect();

    // Add weak noise.
    for (i, s) in buf.iter_mut().enumerate() {
        *s += 0.01 * ((i * 7 % 13) as f32 / 13.0 - 0.5);
    }

    let pre_energy: f32 = buf.iter().map(|s| s * s).sum();

    spectral_denoise(&mut buf, GATE_RATIO);

    let post_energy: f32 = buf.iter().map(|s| s * s).sum();

    // The tone should retain most of its energy (at least 80%).
    assert!(
        post_energy > pre_energy * 0.8,
        "tone lost too much energy: pre={pre_energy}, post={post_energy}"
    );
}

// --- Voice-band weight + enhance_speech tests (issue #274) ---

// Assertion thresholds for the enhance_speech test suite. Centralized
// here so tuning the weight shape or the gate ratio only requires
// touching one set of numbers. The thresholds themselves encode
// invariants of the voice-band algorithm, not implementation details,
// so they should change only when the algorithm's guarantees change.

/// Minimum fraction of input energy an in-band tone must retain
/// after `enhance_speech`. At weight 1.0 and a survivable noise
/// floor the output should be nearly the full input; 0.8 gives
/// slack for FFT numerical bleed and the 1% random noise added in
/// the test to avoid an all-peak-one-bin pathological case.
const IN_BAND_ENERGY_RETENTION_MIN: f32 = 0.8;

/// Maximum fraction of input energy an out-of-band tone may leave
/// in the output. Out-of-band weights are 0 so the ideal is
/// exactly zero; 0.01 tolerates numerical FFT bleed from the
/// nominal zeroing.
const OUT_OF_BAND_ENERGY_MAX_FRACTION: f32 = 0.01;

/// Minimum pre-enhancement energy required for a test input to
/// count as "real signal" rather than numerical dust. Used as a
/// setup sanity check in the kill tests.
const MIN_SETUP_INPUT_ENERGY: f32 = 0.1;

/// In the masking regression, the 1 kHz formant must retain at
/// least this fraction of its pre-enhancement power. With weight
/// 1.0 in the formant band and the voice-prior noise floor
/// excluding the rumble, the formant should survive largely
/// unattenuated; 0.5 is the generous lower bound.
const FORMANT_POWER_RETENTION_MIN: f32 = 0.5;

/// In the masking regression, the post-enhancement 1 kHz formant
/// must dominate residual 50 Hz rumble by at least this factor.
/// 5× is well above the 1:1 crossover and comfortably below the
/// theoretical ∞:1 (rumble at weight 0 should be fully zeroed).
const FORMANT_TO_RUMBLE_DOMINANCE_MIN: f32 = 5.0;

/// Pre-check: the masking test's input buffer must have rumble
/// genuinely dominating the formant component so the test's
/// "masked" premise is real. Input is rumble amp 1.0 + formant
/// amp 0.1, so power ratio is 100× — 50× gives slack for the
/// Goertzel projection's numerical precision at a specific
/// frequency vs. a general FFT bin.
const SETUP_RUMBLE_DOMINANCE_MIN: f32 = 50.0;

/// Equality tolerance for `voice_band_weight` breakpoint tests.
/// The function is piecewise linear with f32 arithmetic so a
/// strict `==` comparison would be brittle under compiler
/// reordering; 1e-6 is several orders of magnitude below any
/// weight the function produces.
const WEIGHT_EQ_EPS: f32 = 1e-6;

/// Sub-Hz offset used to probe the "just below a breakpoint"
/// side of each piecewise boundary. The `voice_band_weight`
/// function has no internal snap-to-zero behavior so any
/// offset smaller than the width of the narrowest region
/// works; 0.1 Hz is visually obvious in assertion messages.
const BREAKPOINT_OFFSET_HZ: f32 = 0.1;

/// Default test signal length in samples — 1600 at 16 kHz =
/// 100 ms. Long enough to put the FFT bin spacing at ~10 Hz
/// (16000/1600), which resolves every voice-band breakpoint
/// cleanly while keeping the test suite fast.
const TEST_SIGNAL_LEN: usize = 1600;

/// Test buffer length for the pure-silence pass-through test.
/// Just needs to exceed `MIN_FFT_LEN` so `spectral_denoise` /
/// `enhance_speech` take the FFT path instead of the short-
/// buffer early return.
const TEST_SILENCE_LEN: usize = 256;

/// Window below 1.0 for the non-unity weight regression test.
/// Output power at a weighted bin is `w² × input_power`, so a
/// weight of 0.5 should produce ~0.25× input power. The window
/// is tolerant of FFT numerical bleed.
const NON_UNITY_POWER_RATIO_MIN: f32 = 0.20;
/// Upper bound on the non-unity power ratio — weight × weight
/// plus headroom for the gate's percentile-based floor to not
/// over-gate a bin that should pass.
const NON_UNITY_POWER_RATIO_MAX: f32 = 0.30;

/// Generate `n` samples of a unit-amplitude sine at `freq_hz` at 16 kHz.
fn tone(freq_hz: f32, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE_HZ;
            (2.0 * std::f32::consts::PI * freq_hz * t).sin()
        })
        .collect()
}

/// Sum-of-squares energy of a signal buffer.
fn energy(buf: &[f32]) -> f32 {
    buf.iter().map(|s| s * s).sum()
}

/// Goertzel-style power projection onto a single frequency.
///
/// Computes `|Σ x[i] * exp(-j 2π f i / fs)|²` without a full FFT,
/// so a test can measure the exact power at a specific frequency
/// rather than relying on total-energy heuristics. Output is
/// proportional to the squared magnitude of the FFT bin nearest
/// `freq_hz` — the same physical quantity `enhance_speech` and
/// `spectral_denoise` gate against.
fn power_at(buf: &[f32], freq_hz: f32) -> f32 {
    let mut re = 0.0_f32;
    let mut im = 0.0_f32;
    for (i, &x) in buf.iter().enumerate() {
        let phase = 2.0 * std::f32::consts::PI * freq_hz * (i as f32) / SAMPLE_RATE_HZ;
        re += x * phase.cos();
        im -= x * phase.sin();
    }
    re * re + im * im
}

#[test]
fn voice_band_weight_at_breakpoints() {
    // All breakpoint tests reference the production `VOICE_F_*`
    // constants so a future retune can't leave this test
    // validating a stale policy. Fixed literals (20.0, 200.0,
    // 1_000.0, 8_000.0) are interior probes that stay valid
    // regardless of where the boundaries move, as long as the
    // band structure keeps at least: one sub-cut interior, one
    // fundamentals interior, one formant interior, one above-
    // sibilance interior.

    // Below sub-cut: hard zero.
    assert!((voice_band_weight(20.0) - 0.0).abs() < WEIGHT_EQ_EPS);
    assert!((voice_band_weight(VOICE_F_SUB_HZ - BREAKPOINT_OFFSET_HZ) - 0.0).abs() < WEIGHT_EQ_EPS);

    // Fundamentals region: constant VOICE_W_FUND.
    assert!((voice_band_weight(VOICE_F_SUB_HZ) - VOICE_W_FUND).abs() < WEIGHT_EQ_EPS);
    assert!((voice_band_weight(200.0) - VOICE_W_FUND).abs() < WEIGHT_EQ_EPS);
    assert!(
        (voice_band_weight(VOICE_F_FUND_HZ - BREAKPOINT_OFFSET_HZ) - VOICE_W_FUND).abs()
            < WEIGHT_EQ_EPS
    );

    // Formant band: full weight.
    assert!((voice_band_weight(VOICE_F_FUND_HZ) - 1.0).abs() < WEIGHT_EQ_EPS);
    assert!((voice_band_weight(1_000.0) - 1.0).abs() < WEIGHT_EQ_EPS);
    assert!(
        (voice_band_weight(VOICE_F_FORMANT_HI_HZ - BREAKPOINT_OFFSET_HZ) - 1.0).abs()
            < WEIGHT_EQ_EPS
    );

    // Sibilance ramp: linear 1.0 → VOICE_W_SIB_END.
    assert!((voice_band_weight(VOICE_F_FORMANT_HI_HZ) - 1.0).abs() < WEIGHT_EQ_EPS);
    let midpoint = 0.5_f32.mul_add(VOICE_W_SIB_END - 1.0, 1.0);
    let mid_freq = f32::midpoint(VOICE_F_FORMANT_HI_HZ, VOICE_F_SIB_HI_HZ);
    assert!(
        (voice_band_weight(mid_freq) - midpoint).abs() < WEIGHT_EQ_EPS,
        "mid-ramp should be halfway between 1.0 and VOICE_W_SIB_END"
    );

    // Above sibilance cutoff: hard zero.
    assert!((voice_band_weight(VOICE_F_SIB_HI_HZ) - 0.0).abs() < WEIGHT_EQ_EPS);
    assert!((voice_band_weight(8_000.0) - 0.0).abs() < WEIGHT_EQ_EPS);
}

#[test]
fn enhance_speech_preserves_formant_band_tone() {
    // A 1 kHz tone is smack in the middle of the formant band —
    // weight 1.0, threshold should let it pass, and the output
    // weight is also 1.0 so the magnitude is preserved.
    let n = TEST_SIGNAL_LEN;
    let mut buf = tone(1_000.0, n);
    // Add weak noise.
    for (i, s) in buf.iter_mut().enumerate() {
        *s += 0.01 * ((i * 7 % 13) as f32 / 13.0 - 0.5);
    }

    let pre = energy(&buf);
    enhance_speech(&mut buf, GATE_RATIO);
    let post = energy(&buf);

    assert!(
        post > pre * IN_BAND_ENERGY_RETENTION_MIN,
        "formant-band tone lost too much energy: pre={pre}, post={post}"
    );
}

#[test]
fn enhance_speech_kills_sub_80hz_rumble() {
    // A 50 Hz tone (AC hum, HVAC rumble, CTCSS leakage) — weight
    // is zero, should be gated to silence.
    let n = TEST_SIGNAL_LEN;
    let mut buf = tone(50.0, n);
    let pre = energy(&buf);
    assert!(
        pre > MIN_SETUP_INPUT_ENERGY,
        "setup: input should have real energy"
    );

    enhance_speech(&mut buf, GATE_RATIO);
    let post = energy(&buf);

    // Output should be near-zero. Use a tolerant bound to allow
    // FFT numerical bleed.
    assert!(
        post < pre * OUT_OF_BAND_ENERGY_MAX_FRACTION,
        "sub-80Hz rumble should be killed: pre={pre}, post={post}"
    );
}

#[test]
fn enhance_speech_kills_above_7500hz_hiss() {
    // A 7.8 kHz tone — above VOICE_F_SIB_HI_HZ, weight is zero,
    // should be gated to silence regardless of amplitude.
    let n = TEST_SIGNAL_LEN;
    let mut buf = tone(7_800.0, n);
    let pre = energy(&buf);
    assert!(
        pre > MIN_SETUP_INPUT_ENERGY,
        "setup: input should have real energy"
    );

    enhance_speech(&mut buf, GATE_RATIO);
    let post = energy(&buf);

    assert!(
        post < pre * OUT_OF_BAND_ENERGY_MAX_FRACTION,
        "above-7500Hz hiss should be killed: pre={pre}, post={post}"
    );
}

#[test]
fn enhance_speech_silence_stays_silent() {
    let mut buf = vec![0.0_f32; TEST_SILENCE_LEN];
    enhance_speech(&mut buf, GATE_RATIO);
    for s in &buf {
        assert!(s.abs() < 1e-6, "expected silence, got {s}");
    }
}

#[test]
fn enhance_speech_short_buffer_is_noop() {
    let mut buf = vec![0.5_f32; MIN_FFT_LEN - 1];
    let original = buf.clone();
    enhance_speech(&mut buf, GATE_RATIO);
    assert_eq!(buf, original);
}

#[test]
fn enhance_speech_in_band_wins_over_louder_out_of_band() {
    // Regression test for the voice-prior noise floor: a quiet
    // 1 kHz tone in the formant band should survive even when a
    // much louder 50 Hz rumble is present. The broadband gate
    // (spectral_denoise) would let the rumble drag the noise
    // floor up and could gate the quieter formant tone out.
    let n = TEST_SIGNAL_LEN;
    let rumble = tone(50.0, n);
    let formant = tone(1_000.0, n);

    // Build: rumble at amplitude 1.0 + formant at amplitude 0.1.
    let mut buf: Vec<f32> = rumble
        .iter()
        .zip(formant.iter())
        .map(|(&r, &f)| r + 0.1 * f)
        .collect();

    // Record the pre-enhancement input power at each frequency
    // so the post-enhancement assertion can compare against a
    // real baseline, not just absolute thresholds.
    let p_rumble_pre = power_at(&buf, 50.0);
    let p_formant_pre = power_at(&buf, 1_000.0);
    assert!(
        p_rumble_pre > p_formant_pre * SETUP_RUMBLE_DOMINANCE_MIN,
        "setup: rumble should initially dominate formant by >{SETUP_RUMBLE_DOMINANCE_MIN}x (rumble amp=1.0 vs formant amp=0.1 → 100x power)"
    );

    enhance_speech(&mut buf, GATE_RATIO);

    // Post-enhancement: the 1 kHz formant must survive AND
    // dominate the residual 50 Hz rumble. Goertzel projection
    // gives us the exact power at each frequency, so we can
    // assert both that the formant survived and that the
    // spectral dominance flipped.
    let p_rumble_post = power_at(&buf, 50.0);
    let p_formant_post = power_at(&buf, 1_000.0);

    assert!(
        p_formant_post > p_formant_pre * FORMANT_POWER_RETENTION_MIN,
        "1 kHz formant should retain >{}% of its input power after voice-band gating: pre={p_formant_pre}, post={p_formant_post}",
        FORMANT_POWER_RETENTION_MIN * 100.0
    );
    assert!(
        p_formant_post > p_rumble_post * FORMANT_TO_RUMBLE_DOMINANCE_MIN,
        "1 kHz formant should dominate residual 50 Hz rumble by >{FORMANT_TO_RUMBLE_DOMINANCE_MIN}x after enhancement: p_formant={p_formant_post}, p_rumble={p_rumble_post}"
    );
}

#[test]
fn enhance_speech_scales_surviving_fundamental_band_tone_by_weight() {
    // Regression coverage for the survivor-scaling path at
    // non-unity weights. A 200 Hz tone is in the fundamentals
    // band (weight VOICE_W_FUND = 0.5). If it survives the
    // gate — it should, because it's the only bin in the
    // buffer and therefore trivially above the percentile
    // floor — the spectrum bin gets multiplied by the weight.
    //
    // Output power at 200 Hz should be approximately
    // `(VOICE_W_FUND)² * input_power` because:
    //   - Forward FFT bin magnitude scales linearly with input
    //     amplitude.
    //   - We multiply the bin by `weight` before the inverse
    //     FFT.
    //   - Output amplitude scales linearly with the scaled
    //     bin.
    //   - Output *power* is amplitude squared.
    //
    // With `VOICE_W_FUND = 0.5`, the expected ratio is 0.25 ±
    // tolerance for FFT numerical bleed and percentile-based
    // threshold interactions.
    let n = TEST_SIGNAL_LEN;
    let mut buf = tone(200.0, n);
    let p_input = power_at(&buf, 200.0);
    assert!(
        p_input > MIN_SETUP_INPUT_ENERGY,
        "setup: input should have real energy at 200 Hz"
    );

    enhance_speech(&mut buf, GATE_RATIO);

    let p_output = power_at(&buf, 200.0);
    let ratio = p_output / p_input;

    // Sanity: the tone must not have been gated to zero —
    // that would be a separate regression, not a scaling one.
    assert!(
        p_output > 0.0,
        "fundamentals-band tone should survive the gate, not be zeroed: p_output={p_output}"
    );

    // The interesting part: the survivor must have been
    // scaled by `VOICE_W_FUND` (not left unscaled). A future
    // change that drops the `spectrum[i] *= *weight` line
    // would push this ratio to ~1.0 and fail the upper
    // bound.
    assert!(
        (NON_UNITY_POWER_RATIO_MIN..=NON_UNITY_POWER_RATIO_MAX).contains(&ratio),
        "200 Hz survivor should be scaled to approximately VOICE_W_FUND² = {}× input power, got ratio={ratio} (p_input={p_input}, p_output={p_output})",
        VOICE_W_FUND * VOICE_W_FUND
    );
}

// ─── AudioEnhancement dispatcher tests ──────────────────────
//
// The dispatcher is a thin routing function, but pinning its
// behavior matters: a future refactor that accidentally swaps
// the VoiceBand / Broadband branches would be caught here, and
// the config-string round-trip is load-bearing for persistence.

// --- three_tone_signal helper constants ---
//
// The buffer length and tone frequencies below are test
// fixtures, not tuning knobs — but per CLAUDE.md they're
// still worth naming because the dispatcher tests below rely
// on specific properties of each band (one in every voice-
// prior weight region) and a future refactor shouldn't have
// to decode the math from bare literals.

/// Length of the test buffer in samples at 16 kHz =
/// `SAMPLE_RATE_HZ`. 4096 samples = 256 ms, comfortably above
/// [`MIN_FFT_LEN`] so all three modes take the FFT path.
const THREE_TONE_SIGNAL_LEN: usize = 4096;
/// Fundamental-band tone. Lands in the 80–300 Hz region that
/// [`enhance_speech`] half-weights — so `VoiceBand`'s output
/// must differ from `Broadband`'s here.
const THREE_TONE_FUNDAMENTAL_HZ: f32 = 100.0;
/// Formant-band tone. Lands in the 300–3400 Hz region that
/// gets full weight (1.0) — so `VoiceBand` preserves this
/// close to unchanged.
const THREE_TONE_FORMANT_HZ: f32 = 1_000.0;
/// Sibilance-band tone. Lands in the 3400–7500 Hz ramp where
/// `VoiceBand` tapers the weight. `Broadband` preserves it
/// unchanged, so the two modes must differ.
const THREE_TONE_SIBILANCE_HZ: f32 = 6_000.0;
/// Per-component amplitude. 0.3 × 3 tones = 0.9 peak which
/// avoids clipping at f32 [-1, 1].
const THREE_TONE_COMPONENT_AMP: f32 = 0.3;

/// Helper: build a test signal that all three modes can process
/// and compare — a sum of three tones at 100 Hz (fundamental
/// band), 1000 Hz (formant band), and 6000 Hz (sibilance band).
/// See the `THREE_TONE_*` constants above for per-tone rationale.
fn three_tone_signal() -> Vec<f32> {
    let n = THREE_TONE_SIGNAL_LEN;
    let mut buf = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / SAMPLE_RATE_HZ;
        let sample = THREE_TONE_COMPONENT_AMP
            * (2.0 * std::f32::consts::PI * THREE_TONE_FUNDAMENTAL_HZ * t).sin()
            + THREE_TONE_COMPONENT_AMP
                * (2.0 * std::f32::consts::PI * THREE_TONE_FORMANT_HZ * t).sin()
            + THREE_TONE_COMPONENT_AMP
                * (2.0 * std::f32::consts::PI * THREE_TONE_SIBILANCE_HZ * t).sin();
        buf.push(sample);
    }
    buf
}

#[test]
fn audio_enhancement_off_is_identity() {
    // Off mode must leave the buffer byte-identical. This is
    // load-bearing for users who want to feed pre-cleaned audio
    // directly to the recognizer without any spectral gate.
    let input = three_tone_signal();
    let mut buf = input.clone();
    apply(&mut buf, AudioEnhancement::Off, GATE_RATIO);
    assert_eq!(
        buf, input,
        "Off mode must not mutate the input buffer in any way"
    );
}

#[test]
fn audio_enhancement_voice_band_matches_enhance_speech() {
    // VoiceBand must route to enhance_speech. A bit-exact
    // comparison against a side-by-side enhance_speech call on
    // an identical input buffer pins the dispatch — a future
    // refactor that silently swapped the VoiceBand branch to a
    // different function would produce different output and
    // fail this assertion.
    let input = three_tone_signal();
    let mut via_apply = input.clone();
    let mut via_direct = input.clone();
    apply(&mut via_apply, AudioEnhancement::VoiceBand, GATE_RATIO);
    enhance_speech(&mut via_direct, GATE_RATIO);
    assert_eq!(
        via_apply, via_direct,
        "VoiceBand dispatcher must produce bit-identical output to enhance_speech"
    );
}

#[test]
fn audio_enhancement_broadband_matches_spectral_denoise() {
    // Same contract for Broadband → spectral_denoise.
    let input = three_tone_signal();
    let mut via_apply = input.clone();
    let mut via_direct = input.clone();
    apply(&mut via_apply, AudioEnhancement::Broadband, GATE_RATIO);
    spectral_denoise(&mut via_direct, GATE_RATIO);
    assert_eq!(
        via_apply, via_direct,
        "Broadband dispatcher must produce bit-identical output to spectral_denoise"
    );
}

#[test]
fn audio_enhancement_modes_produce_different_outputs() {
    // Sanity check that the three modes actually differ on the
    // same input — if this ever asserts `==` the tests above
    // are comparing against themselves and would silently pass
    // even with a busted dispatcher.
    let input = three_tone_signal();
    let mut off = input.clone();
    let mut voice = input.clone();
    let mut broad = input.clone();
    apply(&mut off, AudioEnhancement::Off, GATE_RATIO);
    apply(&mut voice, AudioEnhancement::VoiceBand, GATE_RATIO);
    apply(&mut broad, AudioEnhancement::Broadband, GATE_RATIO);
    assert_ne!(
        off, voice,
        "Off and VoiceBand should differ on a noisy signal"
    );
    assert_ne!(
        off, broad,
        "Off and Broadband should differ on a noisy signal"
    );
    assert_ne!(
        voice, broad,
        "VoiceBand and Broadband should differ on a signal with out-of-voice content"
    );
}

#[test]
fn audio_enhancement_config_str_round_trip() {
    // as_config_str ↔ from_config_str must round-trip for all
    // three variants.
    for mode in [
        AudioEnhancement::VoiceBand,
        AudioEnhancement::Broadband,
        AudioEnhancement::Off,
    ] {
        let s = mode.as_config_str();
        let parsed = AudioEnhancement::from_config_str(s);
        assert_eq!(parsed, mode, "round-trip failed for {mode:?} via {s:?}");
    }
}

#[test]
fn audio_enhancement_config_str_unknown_falls_back_to_default() {
    // Unknown / stale / typo config values must fall back to
    // the default, not error. This matters because a missing
    // audio-enhancement key in an old config file (from before
    // this feature shipped) will deserialize to an empty
    // string which should land on VoiceBand, not some noisy
    // error.
    assert_eq!(
        AudioEnhancement::from_config_str(""),
        AudioEnhancement::default()
    );
    assert_eq!(
        AudioEnhancement::from_config_str("nonsense"),
        AudioEnhancement::default()
    );
    assert_eq!(
        AudioEnhancement::from_config_str("VoiceBand"), // wrong case
        AudioEnhancement::default()
    );
    assert_eq!(AudioEnhancement::default(), AudioEnhancement::VoiceBand);
}
