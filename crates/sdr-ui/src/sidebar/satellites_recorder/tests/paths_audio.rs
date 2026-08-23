use super::*;

#[test]
#[ignore = "exercises APT-specific recorder dispatch (SavePng / audio); APT path is dormant pending a future Cubesat catalog entry — see KNOWN_SATELLITES doc comment about August 2025 NOAA POES decommissioning"]
fn png_path_includes_satellite_slug_and_timestamp() {
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 30, 15).unwrap();
    let pass = synthetic_meteor_m2_3(now, 0, 720, 50.0);
    let path = png_path_for(&pass, now);
    let s = path.to_string_lossy().to_string();
    assert!(s.contains(&format!("apt-{FIXTURE_SLUG}-")));
    assert!(
        std::path::Path::new(&s)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
    );
}

#[test]
fn audio_path_pairs_with_png_path_on_same_timestamp() {
    // Helper-function level pairing: with the same timestamp
    // input, PNG and WAV stems differ only in the "apt-" /
    // "audio-" prefix and the extension. This is the
    // contract `pass_recording_path` is supposed to enforce.
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 30, 15).unwrap();
    let pass = synthetic_meteor_m2_3(now, 0, 720, 50.0);
    let png = png_path_for(&pass, now);
    let audio = audio_path_for(&pass, now);
    let png_stem = png.file_stem().unwrap().to_string_lossy().to_string();
    let audio_stem = audio.file_stem().unwrap().to_string_lossy().to_string();
    let png_tail = png_stem.strip_prefix("apt-").unwrap();
    let audio_tail = audio_stem.strip_prefix("audio-").unwrap();
    assert_eq!(png_tail, audio_tail, "slug+timestamp must match");
    assert_eq!(png.parent(), audio.parent());
    assert!(
        audio
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
    );
}

#[test]
#[ignore = "exercises APT-specific recorder dispatch (SavePng / audio); APT path is dormant pending a future Cubesat catalog entry — see KNOWN_SATELLITES doc comment about August 2025 NOAA POES decommissioning"]
fn audio_and_png_paths_share_aos_timestamp_across_pass_duration() {
    // CR round 1 on PR #534 caught the production bug the
    // previous unit test missed: png_path_for was called at
    // LOS while audio_path_for was called at AOS, so a
    // typical 10-15 minute pass produced filenames that
    // differed by the entire pass duration — breaking the
    // "pair by filename match" contract.
    //
    // Drive the state machine through a real AOS → settle →
    // LOS transition, capturing the audio path the recorder
    // emitted at AOS and the png path it emitted at LOS,
    // then assert their timestamp tails match. Without the
    // pre-compute-png-at-AOS fix this test fails by exactly
    // the pass duration.
    let mut r = AutoRecorder::new();
    let now_aos = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    // Pass starts in 3 s (inside the 5 s lead-in window),
    // lasts 720 s — large enough that an LOS-timestamped
    // png_path would be obviously wrong vs the AOS-
    // timestamped audio_path (12-minute delta).
    let pass = synthetic_meteor_m2_3(now_aos, 3, 720, 50.0);

    // AOS — capture the audio path the recorder asked for.
    let aos_actions = tick(&mut r, now_aos, &pass, true, true);
    let audio_path = aos_actions
        .iter()
        .find_map(|a| match a {
            Action::StartAutoAudioRecord(p) => Some(p.clone()),
            _ => None,
        })
        .expect("StartAutoAudioRecord at AOS");

    // Settle, then LOS at a wall-clock time clearly after
    // AOS. If png_path were re-computed at LOS, this delta
    // would surface as a different timestamp in the PNG
    // filename.
    let after_settle = now_aos + ChronoDuration::seconds(SETTLE_SECS + 1);
    tick(&mut r, after_settle, &pass, true, true);
    let los_plus = pass.end + ChronoDuration::seconds(1);
    let los_actions = tick(&mut r, los_plus, &pass, true, true);
    let png_path = los_actions
        .iter()
        .find_map(|a| match a {
            Action::SavePng(p) => Some(p.clone()),
            _ => None,
        })
        .expect("SavePng at LOS");

    // The defining assertion: the timestamp tail of the PNG
    // matches the timestamp tail of the WAV. Strip the
    // "apt-{slug}-" / "audio-{slug}-" prefix and compare
    // verbatim.
    let png_stem = png_path.file_stem().unwrap().to_string_lossy().to_string();
    let audio_stem = audio_path
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let png_ts = png_stem
        .strip_prefix(&format!("apt-{FIXTURE_SLUG}-"))
        .map(str::to_owned)
        .unwrap();
    let audio_ts = audio_stem
        .strip_prefix(&format!("audio-{FIXTURE_SLUG}-"))
        .map(str::to_owned)
        .unwrap();
    assert_eq!(
        png_ts, audio_ts,
        "PNG and WAV must share the AOS timestamp (regression: \
         pre-fix produced png={png_ts} vs audio={audio_ts}, \
         differing by the pass duration)",
    );
}

#[test]
#[ignore = "exercises APT-specific recorder dispatch (SavePng / audio); APT path is dormant pending a future Cubesat catalog entry — see KNOWN_SATELLITES doc comment about August 2025 NOAA POES decommissioning"]
fn audio_toggle_off_does_not_emit_audio_actions() {
    // Per #533: with the audio toggle off at AOS, the recorder
    // must NOT emit StartAutoAudioRecord at AOS or
    // StopAutoAudioRecord at LOS. PNG path is unaffected.
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    // AOS with audio_record_on = false.
    let aos_actions = tick(&mut r, now, &pass, true, false);
    assert!(
        aos_actions
            .iter()
            .any(|a| matches!(a, Action::StartAutoRecord { .. }))
    );
    assert!(
        !aos_actions
            .iter()
            .any(|a| matches!(a, Action::StartAutoAudioRecord(_))),
        "audio toggle off → no StartAutoAudioRecord",
    );
    // Settle + LOS — audio toggle flipped on mid-pass should
    // NOT retroactively emit StartAutoAudioRecord, and there
    // must be no StopAutoAudioRecord at LOS either (because
    // we never started one).
    let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
    tick(&mut r, after_settle, &pass, true, true);
    let los_plus = pass.end + ChronoDuration::seconds(1);
    let los_actions = tick(&mut r, los_plus, &pass, true, true);
    assert!(los_actions.iter().any(|a| matches!(a, Action::SavePng(_))));
    assert!(
        !los_actions
            .iter()
            .any(|a| matches!(a, Action::StopAutoAudioRecord)),
        "no audio recording was started → no StopAutoAudioRecord",
    );
}

#[test]
#[ignore = "exercises APT-specific recorder dispatch (SavePng / audio); APT path is dormant pending a future Cubesat catalog entry — see KNOWN_SATELLITES doc comment about August 2025 NOAA POES decommissioning"]
fn audio_toggle_on_emits_paired_start_and_stop() {
    // Per #533: audio_record_on at AOS emits
    // StartAutoAudioRecord(path) alongside StartAutoRecord;
    // LOS emits StopAutoAudioRecord alongside SavePng.
    // The audio path must share the satellite slug with the
    // PNG path the LOS emits.
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    let aos_actions = tick(&mut r, now, &pass, true, true);
    let audio_path = aos_actions.iter().find_map(|a| match a {
        Action::StartAutoAudioRecord(p) => Some(p.clone()),
        _ => None,
    });
    let audio_path = audio_path.expect("audio toggle on must emit StartAutoAudioRecord");
    assert!(
        audio_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(&format!("audio-{FIXTURE_SLUG}-"))
    );
    // Settle then LOS — flipping the audio toggle off mid-
    // pass should NOT cancel the in-flight stop. The stop
    // fires at LOS unconditionally based on the captured
    // audio_path.
    let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
    tick(&mut r, after_settle, &pass, true, false);
    let los_plus = pass.end + ChronoDuration::seconds(1);
    let los_actions = tick(&mut r, los_plus, &pass, true, false);
    assert!(los_actions.iter().any(|a| matches!(a, Action::SavePng(_))));
    assert!(
        los_actions
            .iter()
            .any(|a| matches!(a, Action::StopAutoAudioRecord)),
        "in-flight audio recording must stop at LOS even after toggle flip",
    );
}

#[test]
fn lrpt_dir_includes_satellite_slug_and_no_extension() {
    // LRPT's per-pass artifact is a directory, not a file —
    // pin the slug + stamp + lack-of-extension contract so
    // a future filename refactor doesn't accidentally
    // reintroduce a `.png` suffix that would conflict with
    // the per-channel files written inside the directory.
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 30, 15).unwrap();
    let pass = synthetic_meteor_m2(now, 0, 720, 50.0);
    let dir = lrpt_dir_for(&pass, now);
    let s = dir.to_string_lossy().to_string();
    assert!(s.contains("lrpt-METEOR-M2-3-"), "got {s}");
    assert!(
        dir.extension().is_none(),
        "LRPT pass artifact must be a directory, not a file: {dir:?}"
    );
}

#[test]
fn lrpt_dir_pairs_with_audio_path_on_same_timestamp() {
    // If the user has the audio toggle on, a future scenario
    // (hypothetically — see suppression test below) the WAV
    // and the LRPT directory must share a timestamp so a
    // post-pass viewer can pair them by string match. Same
    // contract `audio_path_pairs_with_png_path_on_same_timestamp`
    // enforces for APT.
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 30, 15).unwrap();
    let pass = synthetic_meteor_m2(now, 0, 720, 50.0);
    let dir = lrpt_dir_for(&pass, now);
    let audio = audio_path_for(&pass, now);
    let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
    let audio_stem = audio.file_stem().unwrap().to_string_lossy().to_string();
    let dir_tail = dir_name.strip_prefix("lrpt-").unwrap();
    let audio_tail = audio_stem.strip_prefix("audio-").unwrap();
    assert_eq!(dir_tail, audio_tail, "slug+timestamp must match");
    assert_eq!(dir.parent(), audio.parent());
}

#[test]
fn pass_output_protocol_dispatch_matches_variant() {
    // The `protocol()` discriminant mirrors the variant
    // 1:1 — pin it so a future variant addition without
    // updating `protocol()` fails this test loudly instead
    // of silently dispatching to the wrong save action.
    // Per CodeRabbit round 1 on PR #599: extend to cover SSTV.
    let apt = PassOutput::AptPng(PathBuf::from("/tmp/apt.png"));
    let lrpt = PassOutput::LrptDir(PathBuf::from("/tmp/lrpt-dir"));
    let sstv = PassOutput::SstvDir(PathBuf::from("/tmp/sstv-dir"));
    assert_eq!(apt.protocol(), sdr_sat::ImagingProtocol::Apt);
    assert_eq!(lrpt.protocol(), sdr_sat::ImagingProtocol::Lrpt);
    assert_eq!(sstv.protocol(), sdr_sat::ImagingProtocol::Sstv);
}

#[test]
fn save_action_for_apt_emits_save_png() {
    let action = save_action_for(&PassOutput::AptPng(PathBuf::from("/tmp/apt.png")));
    assert!(matches!(action, Action::SavePng(_)));
}

#[test]
fn save_action_for_lrpt_emits_save_lrpt_pass() {
    let action = save_action_for(&PassOutput::LrptDir(PathBuf::from("/tmp/lrpt-dir")));
    assert!(matches!(action, Action::SaveLrptPass(_)));
}

#[test]
fn save_action_for_sstv_emits_save_sstv_pass() {
    // Pin the SSTV arm of `save_action_for` — mirrors the
    // APT/LRPT tests above. A future rename of
    // `Action::SaveSstvPass` or a mis-route to `SaveLrptPass`
    // fails here before it can silently discard ISS imagery.
    // Per CodeRabbit round 1 on PR #599.
    let action = save_action_for(&PassOutput::SstvDir(PathBuf::from("/tmp/sstv-dir")));
    assert!(matches!(action, Action::SaveSstvPass(_)));
}

#[test]
#[ignore = "exercises APT-specific recorder dispatch (SavePng / audio); APT path is dormant pending a future Cubesat catalog entry — see KNOWN_SATELLITES doc comment about August 2025 NOAA POES decommissioning"]
fn apt_pass_still_records_audio_with_toggle_on() {
    // Inverse of the LRPT suppression — make sure the LRPT
    // gate didn't accidentally mute APT audio recording.
    let mut r = lrpt_recorder();
    let now_aos = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now_aos, 3, 600, 50.0);
    let aos_actions = tick(&mut r, now_aos, &pass, true, true);
    assert!(
        aos_actions
            .iter()
            .any(|a| matches!(a, Action::StartAutoAudioRecord(_))),
        "APT pass with audio toggle on must emit StartAutoAudioRecord; got {aos_actions:?}"
    );
}
