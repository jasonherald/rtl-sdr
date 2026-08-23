use super::*;

/// Build a 48 kHz mono tone at the given frequency + amplitude
/// for `ms` milliseconds.
fn tone(freq_hz: f32, amplitude: f32, ms: usize) -> Vec<f32> {
    let n = (VOICE_SQUELCH_SAMPLE_RATE_HZ * (ms as f32) / 1000.0) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = (i as f32) / VOICE_SQUELCH_SAMPLE_RATE_HZ;
        out.push(amplitude * (core::f32::consts::TAU * freq_hz * t).sin());
    }
    out
}

/// Build a 48 kHz mono "syllable-modulated" signal: a 1 kHz
/// carrier whose amplitude is itself modulated by a slow
/// sine at `syllable_hz`. Approximates speech envelope
/// structure closely enough to exercise the syllabic
/// detector.
fn syllable_modulated(carrier_hz: f32, syllable_hz: f32, ms: usize) -> Vec<f32> {
    let n = (VOICE_SQUELCH_SAMPLE_RATE_HZ * (ms as f32) / 1000.0) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = (i as f32) / VOICE_SQUELCH_SAMPLE_RATE_HZ;
        // Envelope is raised cosine so it stays non-negative
        // and looks like speech loudness structure. 0.5 + 0.5·cos
        // oscillates between 0 and 1.
        let envelope = 0.5 + 0.5 * (core::f32::consts::TAU * syllable_hz * t).cos();
        let carrier = (core::f32::consts::TAU * carrier_hz * t).sin();
        out.push(0.5 * envelope * carrier);
    }
    out
}

/// Pseudo-random white noise with bounded peak amplitude.
/// Uses a cheap LCG; no need for a real PRNG just for tests.
fn white_noise(amplitude: f32, ms: usize, seed: u64) -> Vec<f32> {
    let n = (VOICE_SQUELCH_SAMPLE_RATE_HZ * (ms as f32) / 1000.0) as usize;
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = seed;
    for _ in 0..n {
        // LCG constants from Numerical Recipes.
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        // Top 24 bits → unit float → [-1, 1]
        let u = ((state >> 40) as f32) / ((1u64 << 24) as f32);
        out.push(amplitude * (u * 2.0 - 1.0));
    }
    out
}

#[test]
fn mode_is_active_helper() {
    assert!(!VoiceSquelchMode::Off.is_active());
    assert!(VoiceSquelchMode::Syllabic { threshold: 0.15 }.is_active());
    assert!(VoiceSquelchMode::Snr { threshold_db: 6.0 }.is_active());
}

#[test]
fn off_mode_always_open_and_passes_samples_through() {
    let mut vs = VoiceSquelch::new(VoiceSquelchMode::Off, VOICE_SQUELCH_SAMPLE_RATE_HZ).unwrap();
    assert!(vs.is_open(), "Off mode should start open");
    let samples = tone(1000.0, 0.5, 50);
    assert!(vs.accept_samples(&samples));
    // An empty block after any previous state shouldn't
    // toggle the gate.
    assert!(vs.accept_samples(&[]));
}

#[test]
fn constructor_rejects_non_canonical_sample_rate_for_active_modes() {
    // Active modes (Syllabic / Snr) must reject non-48-kHz
    // rates because their biquad coefficients are calibrated
    // to the canonical rate.
    let err = VoiceSquelch::new(VoiceSquelchMode::Syllabic { threshold: 0.15 }, 44_100.0);
    assert!(err.is_err());
    let err = VoiceSquelch::new(VoiceSquelchMode::Snr { threshold_db: 6.0 }, 44_100.0);
    assert!(err.is_err());

    // Non-finite rate is rejected for ANY mode (Off included)
    // because the finite check comes before the mode dispatch.
    let err = VoiceSquelch::new(VoiceSquelchMode::Off, f32::NAN);
    assert!(err.is_err());
}

#[test]
fn off_mode_accepts_any_finite_sample_rate() {
    // Off mode bypasses the sample-rate calibration check
    // because it constructs no detector and never touches the
    // biquad path. A headless `AfChain` configured for an
    // exotic audio sink (e.g. 44.1 kHz CoreAudio) must be
    // able to construct a `VoiceSquelch` in its default Off
    // state without failing.
    assert!(VoiceSquelch::new(VoiceSquelchMode::Off, 44_100.0).is_ok());
    assert!(VoiceSquelch::new(VoiceSquelchMode::Off, 96_000.0).is_ok());
    assert!(VoiceSquelch::new(VoiceSquelchMode::Off, 16_000.0).is_ok());
}

#[test]
fn constructor_rejects_non_finite_threshold() {
    for t in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let err = VoiceSquelch::new(
            VoiceSquelchMode::Syllabic { threshold: t },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        );
        assert!(err.is_err(), "syllabic threshold {t} should be rejected");
        let err = VoiceSquelch::new(
            VoiceSquelchMode::Snr { threshold_db: t },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        );
        assert!(err.is_err(), "snr threshold {t} should be rejected");
    }
}

#[test]
fn syllabic_constructor_rejects_zero_or_negative_threshold() {
    for t in [0.0_f32, -0.1, -1.0] {
        let err = VoiceSquelch::new(
            VoiceSquelchMode::Syllabic { threshold: t },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        );
        assert!(err.is_err(), "threshold {t} should be rejected");
    }
}

#[test]
fn syllabic_detects_syllable_rate_modulation() {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic {
            threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();

    // Feed 2 seconds of syllable-modulated audio at 4 Hz.
    // That's plenty of time for the BPF to ring up and the
    // RMS window to saturate.
    let signal = syllable_modulated(1_000.0, 4.0, 2_000);
    vs.accept_samples(&signal);
    assert!(
        vs.is_open(),
        "syllabic detector should open on 4 Hz-modulated 1 kHz carrier"
    );
}

#[test]
fn syllabic_rejects_continuous_tone() {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic {
            threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();

    // Pure 1 kHz tone with constant amplitude — no syllabic
    // modulation. Envelope is flat after the rectifier so
    // the 4 Hz BPF sees ~0 energy.
    let signal = tone(1_000.0, 0.5, 2_000);
    vs.accept_samples(&signal);
    assert!(
        !vs.is_open(),
        "syllabic detector should reject a continuous unmodulated tone"
    );
}

#[test]
fn syllabic_rejects_silence() {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic {
            threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();
    let silence = vec![0.0_f32; 48_000];
    vs.accept_samples(&silence);
    assert!(!vs.is_open(), "silence should not open the gate");
}

#[test]
fn snr_detects_strong_in_band_signal() {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Snr {
            threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();
    // Strong 1 kHz tone — well inside the in-voice-band BPF
    // center — and no out-of-voice-band content beyond what
    // the biquad's finite Q leaks. SNR should be very high.
    let signal = tone(1_000.0, 0.8, 2_000);
    vs.accept_samples(&signal);
    assert!(
        vs.is_open(),
        "SNR detector should open on strong in-voice-band tone"
    );
}

#[test]
fn snr_rejects_broadband_noise() {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Snr {
            threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();
    // White noise — equal energy in every bin — so the
    // in-band BPF and out-of-band BPF pick up the same
    // amount once bandwidth-normalized. SNR ~ 0 dB.
    let signal = white_noise(0.5, 2_000, 0xDEAD_BEEF);
    vs.accept_samples(&signal);
    assert!(!vs.is_open(), "SNR detector should reject broadband noise");
}

#[test]
fn snr_rejects_silence() {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Snr {
            threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();
    let silence = vec![0.0_f32; 48_000];
    vs.accept_samples(&silence);
    assert!(!vs.is_open(), "silence should not open the gate");
}

#[test]
fn mode_change_resets_gate_state() {
    // Open the gate with syllabic, then switch to SNR on
    // fresh content — gate should start closed again.
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic {
            threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();
    let signal = syllable_modulated(1_000.0, 4.0, 2_000);
    vs.accept_samples(&signal);
    assert!(vs.is_open());

    vs.set_mode(VoiceSquelchMode::Snr {
        threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
    })
    .unwrap();
    assert!(!vs.is_open(), "mode change should reset gate to closed");
}

#[test]
fn mode_change_to_off_opens_gate() {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic {
            threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();
    assert!(!vs.is_open());
    vs.set_mode(VoiceSquelchMode::Off).unwrap();
    assert!(
        vs.is_open(),
        "Off mode should leave the gate permanently open"
    );
}

#[test]
fn set_threshold_rejects_non_finite() {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic { threshold: 0.15 },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();
    assert!(vs.set_threshold(f32::NAN).is_err());
    assert!(vs.set_threshold(f32::INFINITY).is_err());
    assert!(vs.set_threshold(f32::NEG_INFINITY).is_err());
}

#[test]
fn set_threshold_rejects_non_positive_for_syllabic() {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic { threshold: 0.15 },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();
    assert!(vs.set_threshold(0.0).is_err());
    assert!(vs.set_threshold(-0.1).is_err());
}

#[test]
fn mode_serde_round_trip() {
    let off = VoiceSquelchMode::Off;
    let syl = VoiceSquelchMode::Syllabic { threshold: 0.15 };
    let snr = VoiceSquelchMode::Snr { threshold_db: 6.0 };
    for m in [off, syl, snr] {
        let json = serde_json::to_string(&m).unwrap();
        let back: VoiceSquelchMode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m, "serde round-trip failed for {m:?}");
    }
}

#[test]
fn syllabic_hang_time_bridges_brief_silence_gap() {
    // Simulates a "word + consonant pause + word" structure
    // by concatenating two 1-second syllable-modulated
    // segments with a 200 ms silence gap in between. The gap
    // is shorter than VOICE_SQUELCH_HANG_TIME_MS (500 ms),
    // so the gate should stay open through the whole
    // sequence instead of chopping on the silence.
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic {
            threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();

    // Word 1 — warm up and open the gate.
    let word1 = syllable_modulated(1_000.0, 4.0, 1_000);
    vs.accept_samples(&word1);
    assert!(vs.is_open(), "gate should open on first word");

    // 200 ms of silence — half the 500 ms hang time. The
    // gate should stay open because the counter hasn't
    // decremented all the way to zero yet.
    let short_gap = vec![0.0_f32; (VOICE_SQUELCH_SAMPLE_RATE_HZ * 0.2) as usize];
    vs.accept_samples(&short_gap);
    assert!(
        vs.is_open(),
        "200 ms silence gap < 500 ms hang should NOT close the gate mid-word"
    );
}

/// #777 — one large block must be judged per 100 ms window, not
/// by a single look at its last 100 ms charged for its full
/// length: 900 ms of speech followed by 100 ms of silence in a
/// single `accept_samples` call used to drain the whole 500 ms
/// hang budget and mute the block.
#[test]
fn hang_time_is_charged_per_window_not_per_block() {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic {
            threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();
    vs.accept_samples(&syllable_modulated(1_000.0, 4.0, 1_000));
    assert!(vs.is_open(), "gate should open on speech");

    let mut block = syllable_modulated(1_000.0, 4.0, 900);
    block.extend(std::iter::repeat_n(
        0.0_f32,
        VOICE_SQUELCH_RMS_WINDOW_SAMPLES,
    ));
    assert!(
        vs.accept_samples(&block),
        "100 ms of trailing silence in a 1 s block must not close a 500 ms hang"
    );

    // The same hang budget still closes on a single block that
    // really is silent for longer than the hang time.
    let silence = vec![0.0_f32; VOICE_SQUELCH_HANG_TIME_SAMPLES + VOICE_SQUELCH_RMS_WINDOW_SAMPLES];
    assert!(
        !vs.accept_samples(&silence),
        "600 ms of silence closes the gate"
    );
}

/// Feed `stream` to a fresh syllabic squelch in `block`-sample
/// calls and return the gate state after every call that
/// completes at least one new RMS window, keyed by the last
/// completed window boundary. Verdicts are only taken at
/// boundaries, so the state after the call is the boundary state.
fn gate_trajectory(stream: &[f32], block: usize) -> Vec<(usize, bool)> {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic {
            threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();
    let mut fed = 0;
    let mut last_boundary = 0;
    let mut states = Vec::new();
    for chunk in stream.chunks(block) {
        vs.accept_samples(chunk);
        fed += chunk.len();
        let boundary = fed - fed % VOICE_SQUELCH_RMS_WINDOW_SAMPLES;
        if boundary > last_boundary {
            last_boundary = boundary;
            states.push((boundary, vs.is_open()));
        }
    }
    states
}

/// CR round 1 on PR #797 — the verdict cadence is fixed at one
/// per RMS window of audio regardless of how the audio is split
/// across calls: 7-sample, 10 ms, 10 007-sample and one-shot
/// deliveries of the same stream must agree with the 100 ms
/// delivery at every window boundary they land on.
#[test]
fn gate_state_is_independent_of_call_block_size() {
    let mut stream = syllable_modulated(1_000.0, 4.0, 1_000);
    stream.extend(std::iter::repeat_n(
        0.0_f32,
        (VOICE_SQUELCH_SAMPLE_RATE_HZ * 0.7) as usize,
    ));
    stream.extend(syllable_modulated(1_000.0, 4.0, 800));
    stream.extend(std::iter::repeat_n(
        0.0_f32,
        (VOICE_SQUELCH_SAMPLE_RATE_HZ * 0.6) as usize,
    ));
    assert_eq!(stream.len() % VOICE_SQUELCH_RMS_WINDOW_SAMPLES, 0);

    let reference = gate_trajectory(&stream, VOICE_SQUELCH_RMS_WINDOW_SAMPLES);
    assert!(reference.iter().any(|&(_, o)| o), "{reference:?}");
    assert!(reference.iter().any(|&(_, o)| !o), "{reference:?}");
    for block in [7, 480, 10_007, stream.len()] {
        let observed = gate_trajectory(&stream, block);
        // A one-shot delivery can only observe the final boundary;
        // every smaller block size must observe most of them.
        let min_observed = if block >= stream.len() {
            1
        } else {
            reference.len() / 2
        };
        assert!(
            observed.len() >= min_observed,
            "block size {block} observes too few boundaries: {}",
            observed.len()
        );
        for (fed, open) in observed {
            let expected = reference.iter().find(|(f, _)| *f == fed).map(|(_, o)| *o);
            assert_eq!(Some(open), expected, "block size {block} at {fed} samples");
        }
    }
}

#[test]
fn syllabic_hang_time_closes_after_sustained_silence() {
    // Opposite of the bridging test — after the gate opens,
    // feed sustained silence longer than the hang time and
    // verify the gate actually closes.
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic {
            threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();

    let word = syllable_modulated(1_000.0, 4.0, 1_000);
    vs.accept_samples(&word);
    assert!(vs.is_open(), "gate should open on speech");

    // 1 second of silence — double the 500 ms hang time.
    // After feeding it in 100 ms chunks the hang counter
    // must eventually hit zero and close the gate.
    let silence_chunk = vec![0.0_f32; (VOICE_SQUELCH_SAMPLE_RATE_HZ * 0.1) as usize];
    for _ in 0..10 {
        vs.accept_samples(&silence_chunk);
    }
    assert!(
        !vs.is_open(),
        "1 second of silence must eventually close the gate"
    );
}

#[test]
fn empty_block_does_not_flip_state() {
    let mut vs = VoiceSquelch::new(
        VoiceSquelchMode::Syllabic { threshold: 0.15 },
        VOICE_SQUELCH_SAMPLE_RATE_HZ,
    )
    .unwrap();
    assert!(!vs.is_open());
    // Feeding an empty slice should be a no-op regardless
    // of state.
    assert!(!vs.accept_samples(&[]));
    assert!(!vs.is_open());
}
