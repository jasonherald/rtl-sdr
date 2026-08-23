use super::*;

#[test]
fn lrpt_pass_at_aos_emits_save_lrpt_pass_at_los() {
    // End-to-end: Meteor pass through AOS → settle → LOS
    // dispatches `Action::SaveLrptPass` (NOT `Action::SavePng`)
    // because the per-pass artifact is a directory of
    // per-channel PNGs.
    //
    // Lock the wiring contract so a future refactor that
    // accidentally routes the LRPT path through the APT
    // save action fails this test instead of the user's
    // disk (the directory path would land where the APT
    // save handler expects a file).
    let mut r = lrpt_recorder();
    let now_aos = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2(now_aos, 3, 600, 50.0);

    // AOS — arm the recorder.
    let aos_actions = tick(&mut r, now_aos, &pass, true, true);
    // CR-bait check: the LRPT arm must dispatch the
    // protocol on Action::StartAutoRecord so the wiring
    // layer can route to the LRPT viewer/decoder.
    assert!(aos_actions.iter().any(|a| matches!(
        a,
        Action::StartAutoRecord {
            protocol: sdr_sat::ImagingProtocol::Lrpt,
            ..
        }
    )));

    // Advance through settle to Recording, then to LOS.
    let after_settle = now_aos + ChronoDuration::seconds(SETTLE_SECS + 1);
    let _ = tick(&mut r, after_settle, &pass, true, true);
    assert!(matches!(r.state(), State::Recording { .. }));

    let after_los = pass.end + ChronoDuration::seconds(1);
    let los_actions = tick(&mut r, after_los, &pass, true, true);
    assert!(
        los_actions
            .iter()
            .any(|a| matches!(a, Action::SaveLrptPass(_))),
        "LRPT LOS must emit SaveLrptPass, got {los_actions:?}"
    );
    assert!(
        !los_actions.iter().any(|a| matches!(a, Action::SavePng(_))),
        "LRPT LOS must NOT emit SavePng (that's APT-only), got {los_actions:?}"
    );
}

#[test]
fn lrpt_pass_suppresses_audio_recording_even_when_toggle_on() {
    // LRPT's demod is a silent passthrough; the WAV writer
    // is hardcoded at 48 kHz stereo, so 10+ minutes of
    // silence would burn ~115 MB per pass for no value.
    // The recorder must suppress audio for LRPT regardless
    // of the user's "also save audio" toggle. The toggle
    // still applies to APT — voice/audio capture is
    // genuinely useful there.
    let mut r = lrpt_recorder();
    let now_aos = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2(now_aos, 3, 600, 50.0);

    // Toggle ON, but Meteor pass should still skip the
    // StartAutoAudioRecord emission.
    let aos_actions = tick(&mut r, now_aos, &pass, true, true);
    assert!(
        !aos_actions
            .iter()
            .any(|a| matches!(a, Action::StartAutoAudioRecord(_))),
        "LRPT pass must NOT start audio recording even with toggle on; got {aos_actions:?}"
    );

    // Drive to LOS — must also skip StopAutoAudioRecord
    // (no recording was started, so stopping would be a
    // no-op disguised as a real action).
    let after_settle = now_aos + ChronoDuration::seconds(SETTLE_SECS + 1);
    let _ = tick(&mut r, after_settle, &pass, true, true);
    let after_los = pass.end + ChronoDuration::seconds(1);
    let los_actions = tick(&mut r, after_los, &pass, true, true);
    assert!(
        !los_actions
            .iter()
            .any(|a| matches!(a, Action::StopAutoAudioRecord)),
        "LRPT LOS must NOT emit StopAutoAudioRecord (no recording was started); got {los_actions:?}"
    );
}
