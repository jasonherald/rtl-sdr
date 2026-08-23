use super::*;

/// A squelch threshold no real signal reaches (dB), a near-silent
/// input well below it, and the residual the muted output may carry.
const SQUELCH_VERY_HIGH_DB: f32 = 10.0;
const SQUELCH_QUIET_INPUT_AMPLITUDE: f32 = 0.001;
const SQUELCH_TEST_INPUT_SAMPLES: usize = 500;
const SQUELCH_TEST_OUTPUT_CAPACITY: usize = 1000;
const SQUELCH_MUTED_PEAK_LIMIT: f32 = 0.01;

#[test]
fn test_radio_module_squelch() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_squelch_enabled(true);
    radio.set_squelch(SQUELCH_VERY_HIGH_DB);

    let input = vec![Complex::new(SQUELCH_QUIET_INPUT_AMPLITUDE, 0.0); SQUELCH_TEST_INPUT_SAMPLES];
    let mut output = vec![Stereo::default(); SQUELCH_TEST_OUTPUT_CAPACITY];
    let count = radio.process(&input, &mut output).unwrap();
    assert!(count > 0);
    // All output should be near zero (squelch closed)
    let peak = output[..count]
        .iter()
        .map(|s| s.l.abs().max(s.r.abs()))
        .fold(0.0_f32, f32::max);
    assert!(
        peak < SQUELCH_MUTED_PEAK_LIMIT,
        "squelch should mute output, peak = {peak}"
    );
}

/// #734 — the speaker stays hard-muted while the power squelch is
/// closed, but `pre_gate_audio()` must still carry the demodulated
/// signal for the APT / SSTV taps.
#[test]
fn pre_gate_audio_survives_a_closed_power_squelch() {
    const WEAK_AMPLITUDE: f32 = 0.001;
    const HIGH_THRESHOLD_DB: f32 = 10.0;
    const BLOCK: usize = 2_000;
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    radio.set_squelch_enabled(true);
    radio.set_squelch(HIGH_THRESHOLD_DB);

    let input = fm_tone_iq(BLOCK, WEAK_AMPLITUDE);
    let mut output = vec![Stereo::default(); radio.max_output_samples(BLOCK)];
    let count = radio.process(&input, &mut output).unwrap();
    assert!(count > 0);
    assert!(
        !radio.if_chain().squelch_open(),
        "test premise: gate closed"
    );
    // `peak` is non-negative, so `<= 0.0` means exactly zero.
    assert!(
        peak(&output[..count]) <= 0.0,
        "speaker output is hard-muted"
    );

    let pre_gate = radio.pre_gate_audio();
    assert_eq!(pre_gate.len(), count);
    assert!(
        peak(pre_gate) > 0.0,
        "pre-gate audio keeps the demodulated tone"
    );
}

/// #734 — same contract for a closed voice squelch (APT / SSTV tones
/// have no speech cadence, so Syllabic / Snr stay closed on them).
#[test]
fn pre_gate_audio_survives_a_closed_voice_squelch() {
    use sdr_dsp::voice_squelch::VoiceSquelchMode;
    // Two seconds at the 50 kHz IF rate: long enough for the
    // Syllabic envelope filter's start-up transient to decay so the
    // steady tone reads as "no cadence".
    const BLOCK: usize = 100_000;
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    // A steady tone has no speech cadence, so Syllabic stays closed.
    radio
        .set_voice_squelch_mode(VoiceSquelchMode::Syllabic {
            threshold: sdr_dsp::voice_squelch::VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
        })
        .unwrap();

    let input = fm_tone_iq(BLOCK, 1.0);
    let mut output = vec![Stereo::default(); radio.max_output_samples(BLOCK)];
    let count = radio.process(&input, &mut output).unwrap();
    assert!(count > 0);
    assert!(
        !radio.voice_squelch_open(),
        "test premise: voice gate closed"
    );
    // `peak` is non-negative, so `<= 0.0` means exactly zero.
    assert!(
        peak(&output[..count]) <= 0.0,
        "speaker output is hard-muted"
    );
    assert!(
        peak(radio.pre_gate_audio()) > 0.0,
        "pre-gate audio keeps the tone"
    );
}

/// #734 (CR round 1 on PR #791) — same contract at the module level.
#[test]
fn pre_gate_audio_is_cleared_by_empty_input() {
    const BLOCK: usize = 2_000;
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    let input = fm_tone_iq(BLOCK, 1.0);
    let mut output = vec![Stereo::default(); radio.max_output_samples(BLOCK)];
    let count = radio.process(&input, &mut output).unwrap();
    assert_eq!(
        radio.pre_gate_audio().len(),
        count,
        "test premise: a block is retained"
    );
    assert_eq!(radio.process(&[], &mut output).unwrap(), 0);
    assert!(
        radio.pre_gate_audio().is_empty(),
        "empty input must clear the retained block"
    );
}

#[test]
fn test_radio_module_auto_squelch() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_squelch_enabled(true);
    radio.set_auto_squelch_enabled(true);

    // Verify auto-squelch is enabled on the IF chain
    assert!(radio.if_chain().auto_squelch_enabled());

    // Disable and verify
    radio.set_auto_squelch_enabled(false);
    assert!(!radio.if_chain().auto_squelch_enabled());
}

/// #738 — the squelch close edge is ramped by the AF envelope on
/// real audio instead of being hard-zeroed first: the first closed
/// block still carries a decaying tail, and the speaker reaches
/// exact silence once the release has settled.
#[test]
fn squelch_close_edge_is_ramped_then_exactly_silent() {
    /// IQ amplitude that sits well above the squelch threshold
    /// (−6 dBFS vs −30 dB) so the gate opens on the first block.
    const STRONG_AMPLITUDE: f32 = 0.5;
    /// IQ amplitude 30 dB below the threshold so the gate closes
    /// on the first weak block (the FM tone is still present, so
    /// the demod keeps producing audio for the ramp to act on).
    const WEAK_AMPLITUDE: f32 = 0.001;
    /// Manual threshold between the two amplitudes; the production
    /// default is −100 dB (effectively open), which would never
    /// close here.
    const THRESHOLD_DB: f32 = -30.0;
    /// ~42 ms of IQ at the 48 kHz NFM IF rate — a typical DSP block,
    /// short enough that the release is still audibly ramping when
    /// the block ends.
    const BLOCK: usize = 2_000;
    /// Upper bound on closed blocks to reach exact silence: 50 ×
    /// 42 ms ≈ 2 s, far beyond the release time constant, so a
    /// settle that never happens fails the test rather than hides.
    const SETTLE_BLOCKS: usize = 50;
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    radio.set_squelch_enabled(true);
    radio.set_squelch(THRESHOLD_DB);
    let mut output = vec![Stereo::default(); radio.max_output_samples(BLOCK)];

    let strong = fm_tone_iq(BLOCK, STRONG_AMPLITUDE);
    for _ in 0..5 {
        radio.process(&strong, &mut output).unwrap();
    }
    assert!(radio.if_chain().squelch_open(), "test premise: gate open");

    let weak = fm_tone_iq(BLOCK, WEAK_AMPLITUDE);
    let count = radio.process(&weak, &mut output).unwrap();
    assert!(
        !radio.if_chain().squelch_open(),
        "test premise: gate closed"
    );
    assert!(
        peak(&output[..count]) > 0.0,
        "first closed block must carry the release ramp, not a hard step"
    );
    assert!(
        output[0].l.abs() >= output[count - 1].l.abs(),
        "the ramp decays across the block"
    );

    let mut silent = false;
    for _ in 0..SETTLE_BLOCKS {
        let count = radio.process(&weak, &mut output).unwrap();
        if peak(&output[..count]) <= 0.0 {
            silent = true;
            break;
        }
    }
    assert!(
        silent,
        "speaker must reach exact silence once the release settles"
    );
}

/// Codacy on PR #800 — the block in which the release crosses the
/// settle threshold must keep its ramp: the last audible block ends
/// below −60 dB of the open level, so exact silence never starts
/// with a step.
#[test]
fn squelch_release_reaches_silence_without_a_step() {
    /// Opens the gate on the first block (see the sibling test).
    const STRONG_AMPLITUDE: f32 = 0.5;
    /// Closes the gate while the demod still produces audio.
    const WEAK_AMPLITUDE: f32 = 0.001;
    /// Manual threshold between the two amplitudes.
    const THRESHOLD_DB: f32 = -30.0;
    /// ~0.4 s of IQ — a block long enough for the release to settle
    /// *inside* it, so zeroing the whole block (the bug) would step
    /// from full level to silence.
    const BLOCK: usize = 20_000;
    /// Upper bound on closed blocks: 50 × 0.4 s ≫ the release time
    /// constant, so a release that never settles fails loudly.
    const SETTLE_BLOCKS: usize = 50;
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    radio.set_squelch_enabled(true);
    radio.set_squelch(THRESHOLD_DB);
    let mut output = vec![Stereo::default(); radio.max_output_samples(BLOCK)];

    let strong = fm_tone_iq(BLOCK, STRONG_AMPLITUDE);
    let mut open_peak = 0.0_f32;
    for _ in 0..5 {
        let count = radio.process(&strong, &mut output).unwrap();
        open_peak = peak(&output[..count]);
    }
    assert!(radio.if_chain().squelch_open() && open_peak > 0.0);

    let weak = fm_tone_iq(BLOCK, WEAK_AMPLITUDE);
    let mut last_audible_tail = f32::NAN;
    let mut silent = false;
    for _ in 0..SETTLE_BLOCKS {
        let count = radio.process(&weak, &mut output).unwrap();
        if peak(&output[..count]) <= 0.0 {
            silent = true;
            break;
        }
        last_audible_tail = output[count - 1].l.abs().max(output[count - 1].r.abs());
    }
    assert!(silent, "must reach exact silence");
    assert!(
        last_audible_tail < open_peak * 1e-3,
        "last audible block must end below -60 dB before silence, got {last_audible_tail} (open {open_peak})"
    );
}
