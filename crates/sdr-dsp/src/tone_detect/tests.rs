use super::*;

/// Generate `n` samples of a unit-amplitude sine at `freq_hz`
/// at [`CTCSS_SAMPLE_RATE_HZ`].
fn tone(freq_hz: f32, n: usize, amplitude: f32) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f32 / CTCSS_SAMPLE_RATE_HZ;
            amplitude * (core::f32::consts::TAU * freq_hz * t).sin()
        })
        .collect()
}

/// Synthetic speech-ish noise: sum of three voice-band tones
/// (100 Hz fundamental + 450 Hz formant + 1100 Hz formant) with
/// random per-sample amplitude. Not real speech, but has the
/// key property of putting energy in the 80–250 Hz CTCSS band
/// which is the main false-trigger risk.
fn speech_like(n: usize, amplitude: f32) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f32 / CTCSS_SAMPLE_RATE_HZ;
            let f0 = (core::f32::consts::TAU * 100.0 * t).sin() * 0.5;
            let f1 = (core::f32::consts::TAU * 450.0 * t).sin() * 0.3;
            let f2 = (core::f32::consts::TAU * 1_100.0 * t).sin() * 0.2;
            let envelope = 0.5 + 0.5 * ((i * 13 % 31) as f32 / 31.0);
            amplitude * envelope * (f0 + f1 + f2)
        })
        .collect()
}

fn window_samples() -> usize {
    CTCSS_WINDOW_SAMPLES
}

#[test]
fn tone_table_is_ascending_and_unique() {
    // Sanity: if the table ever grows / shrinks we want a test
    // failure rather than a silent ordering change in the UI
    // dropdown OR a silent change in the documented surface
    // (docs currently say "42 standard + 9 extensions = 51
    // tones").
    assert_eq!(
        CTCSS_TONES_HZ.len(),
        51,
        "CTCSS_TONES_HZ must match the documented 42 standard + 9 extension count"
    );
    for w in CTCSS_TONES_HZ.windows(2) {
        assert!(
            w[0] < w[1],
            "CTCSS table must be strictly ascending, got {} >= {}",
            w[0],
            w[1]
        );
    }
}

#[test]
fn ctcss_tone_index_finds_known_entries_and_rejects_unknown() {
    assert_eq!(ctcss_tone_index(67.0), Some(0));
    assert_eq!(ctcss_tone_index(100.0), Some(12));
    assert_eq!(ctcss_tone_index(254.1), Some(CTCSS_TONES_HZ.len() - 1));
    assert_eq!(ctcss_tone_index(60.0), None);
    assert_eq!(ctcss_tone_index(68.5), None);
}

#[test]
fn with_threshold_rejects_out_of_range_or_non_finite_threshold() {
    // Zero / negative thresholds would make every window a
    // hit (including pure silence), which is almost always a
    // wiring bug. Guard at construction time.
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 0.0).is_err());
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, -0.1).is_err());
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, -10.0).is_err());
    // Thresholds above 1.0 are unreachable because
    // normalized_magnitude is bounded by 1.0 in exact
    // arithmetic. Reject them too so a misconfiguration
    // doesn't silently produce a never-fires detector.
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 1.0001).is_err());
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 2.0).is_err());
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 100.0).is_err());
    // NaN and infinity bypass ordering comparisons under
    // IEEE-754 — must be explicitly rejected.
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, f32::NAN).is_err());
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, f32::INFINITY).is_err());
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, f32::NEG_INFINITY).is_err());
    // Positive finite values in (0, 1] are fine.
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 0.001).is_ok());
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 0.5).is_ok());
    // Exactly 1.0 is the ceiling and should be accepted.
    assert!(CtcssDetector::with_threshold(100.0, CTCSS_SAMPLE_RATE_HZ, 1.0).is_ok());
}

#[test]
fn constructor_rejects_non_canonical_sample_rates() {
    // The detector's window math is calibrated for 48 kHz;
    // other rates are rejected because CTCSS_WINDOW_SAMPLES is
    // a hardcoded constant and the effective window duration
    // would be wrong. Regression guard against a future caller
    // that thinks it can repurpose this for a 96 kHz or 44.1 kHz
    // AF chain without the follow-up refactor.
    assert!(CtcssDetector::new(100.0, 44_100.0).is_err());
    assert!(CtcssDetector::new(100.0, 48_000.5).is_ok()); // within the 0.5 Hz epsilon
    assert!(CtcssDetector::new(100.0, 48_001.0).is_err()); // just outside
    assert!(CtcssDetector::new(100.0, 96_000.0).is_err());
    assert!(CtcssDetector::new(100.0, 16_000.0).is_err());
}

#[test]
fn constructor_rejects_non_ctcss_or_non_finite_frequencies() {
    // Non-CTCSS frequencies: rejected because the neighbor-
    // dominance gate needs table neighbors to compare against.
    assert!(CtcssDetector::new(0.0, CTCSS_SAMPLE_RATE_HZ).is_err());
    assert!(CtcssDetector::new(-1.0, CTCSS_SAMPLE_RATE_HZ).is_err());
    assert!(CtcssDetector::new(500.0, CTCSS_SAMPLE_RATE_HZ).is_err());
    assert!(CtcssDetector::new(30_000.0, CTCSS_SAMPLE_RATE_HZ).is_err());
    assert!(CtcssDetector::new(68.0, CTCSS_SAMPLE_RATE_HZ).is_err());

    // Non-finite targets and sample rates: NaN and ±∞ fail
    // ordering comparisons under IEEE-754, so they have to be
    // caught by an explicit `.is_finite()` guard.
    assert!(CtcssDetector::new(f32::NAN, CTCSS_SAMPLE_RATE_HZ).is_err());
    assert!(CtcssDetector::new(f32::INFINITY, CTCSS_SAMPLE_RATE_HZ).is_err());
    assert!(CtcssDetector::new(f32::NEG_INFINITY, CTCSS_SAMPLE_RATE_HZ).is_err());
    assert!(CtcssDetector::new(100.0, f32::NAN).is_err());
    assert!(CtcssDetector::new(100.0, f32::INFINITY).is_err());
    assert!(CtcssDetector::new(100.0, 0.0).is_err());
    assert!(CtcssDetector::new(100.0, -48_000.0).is_err());

    // Valid CTCSS tones at the edges of the table are fine.
    assert!(CtcssDetector::new(67.0, CTCSS_SAMPLE_RATE_HZ).is_ok());
    assert!(CtcssDetector::new(254.1, CTCSS_SAMPLE_RATE_HZ).is_ok());
}

#[test]
fn close_neighbor_tones_are_rejected_by_dominance_gate() {
    // Regression for CR round 2 on PR #285: at 200 ms window /
    // 48 kHz sample rate the Goertzel sinc leakage between
    // adjacent CTCSS tones (e.g. 150.0 / 151.4 Hz, Δf = 1.4 Hz)
    // was large enough that an absolute-threshold decision
    // false-fired on the neighbor. Fixed by moving to a 400 ms
    // window AND requiring target magnitude to dominate the
    // immediate neighbors.
    //
    // This test pins the critical pairs CR identified plus the
    // symmetric "target itself still works" cases to make sure
    // the dominance gate didn't overshoot and break real
    // targets.
    let pairs = [(150.0_f32, 151.4_f32), (67.0, 69.3), (159.8, 162.2)];

    for &(low, high) in &pairs {
        // Detector tuned to `low`, fed a pure `high` tone. Must
        // NOT sustain the gate even over many windows.
        let mut det = CtcssDetector::new(low, CTCSS_SAMPLE_RATE_HZ).expect("low tone is in table");
        let interferer = tone(high, window_samples(), 1.0);
        for _ in 0..(CTCSS_MIN_HITS + 3) {
            det.accept_samples(&interferer);
        }
        assert!(
            !det.is_sustained(),
            "{low} Hz detector sustained on {high} Hz source (adjacent-tone leakage)"
        );

        // Sanity: the same detector MUST still sustain on its
        // true target. If it doesn't, the dominance gate is too
        // strict and we've traded off real detections.
        let mut det = CtcssDetector::new(low, CTCSS_SAMPLE_RATE_HZ).expect("low tone is in table");
        let real = tone(low, window_samples(), 1.0);
        for _ in 0..CTCSS_MIN_HITS {
            det.accept_samples(&real);
        }
        assert!(
            det.is_sustained(),
            "{low} Hz detector failed to sustain on its own target"
        );

        // And the symmetric case: detector tuned to `high` fed
        // a pure `low` source must reject.
        let mut det =
            CtcssDetector::new(high, CTCSS_SAMPLE_RATE_HZ).expect("high tone is in table");
        let interferer = tone(low, window_samples(), 1.0);
        for _ in 0..(CTCSS_MIN_HITS + 3) {
            det.accept_samples(&interferer);
        }
        assert!(
            !det.is_sustained(),
            "{high} Hz detector sustained on {low} Hz source (adjacent-tone leakage)"
        );
    }
}

#[test]
fn pure_target_tone_triggers_sustained_gate_after_min_hits() {
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    let block = tone(100.0, window_samples(), 1.0);

    // First block: detected but not yet sustained. Tests feed
    // exactly one full window per call, so `accept_samples`
    // always returns `Some(decision)` — unwrap via `expect`.
    let d1 = det
        .accept_samples(&block)
        .expect("one full window should produce a decision");
    assert!(
        d1.detected,
        "100 Hz tone should be detected in a 100 Hz-tuned window: mag={}",
        d1.normalized_magnitude
    );
    assert!(!d1.sustained, "single window shouldn't flip sustained gate");

    // Second and third blocks: still hitting, not yet sustained
    // until hit count reaches min_hits.
    for _ in 0..(CTCSS_MIN_HITS - 2) {
        let d = det
            .accept_samples(&block)
            .expect("one full window should produce a decision");
        assert!(d.detected && !d.sustained);
    }

    // Third hit in a row: sustained gate opens.
    let dfinal = det
        .accept_samples(&block)
        .expect("one full window should produce a decision");
    assert!(
        dfinal.sustained,
        "sustained gate should open after CTCSS_MIN_HITS"
    );
}

#[test]
fn pure_silence_never_triggers() {
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    let silence = vec![0.0_f32; window_samples()];
    for _ in 0..10 {
        let d = det
            .accept_samples(&silence)
            .expect("one full window should produce a decision");
        assert!(!d.detected && !d.sustained);
    }
}

#[test]
fn wrong_tone_does_not_trigger_target_detector() {
    // Detector tuned to 100 Hz, input is a clean 67 Hz tone.
    // Should NOT cross the sustained gate even over many blocks.
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    let wrong_tone = tone(67.0, window_samples(), 1.0);

    for _ in 0..10 {
        let d = det
            .accept_samples(&wrong_tone)
            .expect("one full window should produce a decision");
        assert!(
            !d.sustained,
            "67 Hz tone should not trigger a 100 Hz detector (mag={})",
            d.normalized_magnitude
        );
    }
}

#[test]
fn speech_like_noise_alone_does_not_sustain() {
    // Pure speech-band content with no CTCSS tone. Voice
    // fundamentals in 100 Hz range may produce occasional hits
    // in a naive per-window check, but the sustained gate
    // should prevent the squelch from flapping open.
    let mut det =
        CtcssDetector::new(127.3, CTCSS_SAMPLE_RATE_HZ).expect("127.3 Hz is a valid target");
    let speech = speech_like(window_samples(), 1.0);

    for _ in 0..10 {
        det.accept_samples(&speech);
    }
    assert!(
        !det.is_sustained(),
        "speech-like signal without 127.3 Hz content should not sustain"
    );
}

#[test]
fn tone_under_speech_still_triggers() {
    // Mixed signal: target tone + speech-band noise. This is
    // the real-world case — a radio transmitting voice with a
    // 100 Hz CTCSS tone mixed in. Detector should still
    // sustain.
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    let n = window_samples();
    let pure_tone = tone(100.0, n, 0.6);
    let noise = speech_like(n, 0.4);
    let mixed: Vec<f32> = pure_tone
        .iter()
        .zip(noise.iter())
        .map(|(&t, &s)| t + s)
        .collect();

    for _ in 0..CTCSS_MIN_HITS {
        det.accept_samples(&mixed);
    }
    assert!(
        det.is_sustained(),
        "CTCSS tone mixed under speech-band noise should still sustain the gate"
    );
}

#[test]
fn gate_closes_after_tone_drops() {
    // Sustain the gate, then feed silence and verify it closes
    // after CTCSS_MIN_HITS miss windows.
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    let n = window_samples();
    let block = tone(100.0, n, 1.0);

    for _ in 0..CTCSS_MIN_HITS {
        det.accept_samples(&block);
    }
    assert!(det.is_sustained());

    // Drop the tone.
    let silence = vec![0.0_f32; n];
    for i in 0..CTCSS_MIN_HITS {
        det.accept_samples(&silence);
        // Should stay sustained until the miss-run reaches
        // min_hits; can drop on the final iteration.
        if i < CTCSS_MIN_HITS - 1 {
            assert!(
                det.is_sustained(),
                "gate should remain open until miss run reaches min_hits"
            );
        }
    }
    assert!(
        !det.is_sustained(),
        "gate must close after CTCSS_MIN_HITS misses"
    );
}

#[test]
fn brief_dropout_does_not_flap_open_gate() {
    // Sustain the gate, then feed ONE silence window (below
    // min_hits dropouts), then resume tone. Gate should stay
    // open throughout — this is the hysteresis behavior.
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    let n = window_samples();
    let block = tone(100.0, n, 1.0);

    for _ in 0..CTCSS_MIN_HITS {
        det.accept_samples(&block);
    }
    assert!(det.is_sustained());

    // One miss window, then tone resumes.
    det.accept_samples(&vec![0.0_f32; n]);
    assert!(
        det.is_sustained(),
        "single miss below min_hits should not close the sustained gate"
    );
    det.accept_samples(&block);
    assert!(det.is_sustained());
}

#[test]
fn reset_clears_sustained_state() {
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    let n = window_samples();
    let block = tone(100.0, n, 1.0);

    for _ in 0..CTCSS_MIN_HITS {
        det.accept_samples(&block);
    }
    assert!(det.is_sustained());

    det.reset();
    assert!(!det.is_sustained());
}

#[test]
fn empty_block_returns_none_and_does_not_flip_state() {
    // An empty input should be a true no-op: no pending-buffer
    // drift, no state change, and None returned because no
    // window was completed.
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    assert!(det.accept_samples(&[]).is_none());
    assert!(!det.is_sustained());
    assert_eq!(det.pending_samples.len(), 0);
}

#[test]
fn sustained_state_visible_via_is_sustained_between_blocks() {
    // Callers may want to poll `is_sustained` without feeding a
    // block — verify it matches the last returned decision.
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    let block = tone(100.0, window_samples(), 1.0);
    for _ in 0..CTCSS_MIN_HITS {
        let d = det
            .accept_samples(&block)
            .expect("one full window should produce a decision");
        assert_eq!(det.is_sustained(), d.sustained);
    }
    assert!(det.is_sustained());
}

#[test]
fn accept_samples_buffers_partial_windows() {
    // Regression for CR round 6: callers can feed arbitrary-
    // length chunks (e.g. a 4,096-sample audio-callback block)
    // and the detector must NOT run Goertzel on a short buffer,
    // which would change the effective window length and break
    // the adjacent-tone rejection. Instead it should stash the
    // partial window and return None until enough samples have
    // arrived to fill a full CTCSS_WINDOW_SAMPLES block.
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    // Pin the hot-path invariant that the detector keeps its
    // reusable sample buffer across partial/full-window calls.
    // A future refactor that replaced `pending_samples` with a
    // fresh Vec (e.g. via `split_off`) would silently reintroduce
    // allocator churn in the real-time DSP path.
    let initial_capacity = det.pending_samples.capacity();
    assert!(
        initial_capacity >= CTCSS_WINDOW_SAMPLES,
        "constructor should pre-reserve at least one full window"
    );
    let block = tone(100.0, window_samples(), 1.0);

    // Feed the first half of a window. No decision yet.
    let half = CTCSS_WINDOW_SAMPLES / 2;
    assert!(det.accept_samples(&block[..half]).is_none());
    assert_eq!(det.pending_samples.len(), half);
    assert!(!det.is_sustained());
    assert_eq!(
        det.pending_samples.capacity(),
        initial_capacity,
        "partial-window feed must not reallocate the pending buffer"
    );

    // Feed the second half. Should complete the window and
    // return Some(decision).
    let d = det
        .accept_samples(&block[half..])
        .expect("remaining half of the window should complete a decision");
    assert!(
        d.detected,
        "combined 100 Hz tone window should detect: mag={}",
        d.normalized_magnitude
    );
    assert_eq!(
        det.pending_samples.len(),
        0,
        "pending buffer should be empty after a complete window"
    );
    assert_eq!(
        det.pending_samples.capacity(),
        initial_capacity,
        "window-completion must preserve the pending buffer's reserved capacity"
    );
}

#[test]
fn accept_samples_processes_multiple_windows_in_one_call() {
    // A caller that batches several seconds of audio into one
    // call should still get proper per-window debounce behavior.
    // Feed MIN_HITS full windows in a single call — the
    // sustained gate should open by the end even though the
    // caller only invoked `accept_samples` once.
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    let initial_capacity = det.pending_samples.capacity();
    let single_window = tone(100.0, window_samples(), 1.0);
    let mut batched: Vec<f32> = Vec::with_capacity(CTCSS_WINDOW_SAMPLES * CTCSS_MIN_HITS);
    for _ in 0..CTCSS_MIN_HITS {
        batched.extend_from_slice(&single_window);
    }

    let d = det
        .accept_samples(&batched)
        .expect("batched multi-window input should produce a latest-window decision");
    assert!(
        d.sustained,
        "sustained gate should open after processing CTCSS_MIN_HITS windows in one call"
    );
    assert!(det.is_sustained());
    // The batched input is an exact multiple of CTCSS_WINDOW_SAMPLES,
    // so `pending_samples` ends empty; its reserved capacity must
    // still be at least the initial reservation so the next call
    // starts from a warm allocation. `>=` (not `==`) because a
    // caller feeding an oversized block may have legitimately
    // grown the buffer — we only require that growth is not lost.
    assert!(
        det.pending_samples.capacity() >= initial_capacity,
        "multi-window batched call must not shrink the pending buffer below its initial reservation"
    );
}

#[test]
fn reset_clears_pending_sample_buffer() {
    // Regression guard: reset() must drop buffered partial-
    // window samples, otherwise the next session would start
    // with sample alignment carried over from the previous one.
    let mut det =
        CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
    let initial_capacity = det.pending_samples.capacity();
    let partial = vec![0.5_f32; CTCSS_WINDOW_SAMPLES / 3];
    det.accept_samples(&partial);
    assert_eq!(det.pending_samples.len(), partial.len());

    det.reset();
    assert_eq!(det.pending_samples.len(), 0);
    // reset() must clear contents but must not drop the reserved
    // capacity — otherwise the first post-reset `accept_samples`
    // call would have to re-allocate on the real-time path.
    assert_eq!(
        det.pending_samples.capacity(),
        initial_capacity,
        "reset() must preserve the pending buffer's reserved capacity"
    );
}
