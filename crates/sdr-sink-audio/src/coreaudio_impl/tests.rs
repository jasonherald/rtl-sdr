use super::*;

#[test]
fn new_does_not_panic() {
    let sink = AudioSink::new();
    assert_eq!(sink.name(), "Audio");
    assert!((sink.sample_rate() - AUDIO_SAMPLE_RATE).abs() < f64::EPSILON);
}

#[test]
fn write_before_start_returns_not_running() {
    let mut sink = AudioSink::new();
    let samples = [Stereo::new(0.0, 0.0)];
    assert!(
        matches!(sink.write_samples(&samples), Err(SinkError::NotRunning)),
        "write_samples should fail before start"
    );
}

#[test]
fn stop_before_start_returns_not_running() {
    let mut sink = AudioSink::new();
    assert!(matches!(sink.stop(), Err(SinkError::NotRunning)));
}

#[test]
fn set_sample_rate_validation() {
    let mut sink = AudioSink::new();
    assert!(sink.set_sample_rate(AUDIO_SAMPLE_RATE).is_ok());
    assert!(sink.set_sample_rate(44100.0).is_err());
    assert!(sink.set_sample_rate(-1.0).is_err());
    assert!(sink.set_sample_rate(f64::NAN).is_err());
    assert!(sink.set_sample_rate(f64::INFINITY).is_err());
}

#[test]
fn list_audio_sinks_includes_default() {
    let devices = list_audio_sinks();
    assert!(!devices.is_empty(), "list must always include 'Default'");
    let default = &devices[0];
    assert_eq!(default.display_name, "Default");
    assert!(default.node_name.is_empty());
}

#[test]
fn enumerated_node_names_parse_as_audio_device_ids() {
    // Every non-default entry's node_name must be a decimal u32
    // (the AudioDeviceID round-trip contract). This is what
    // `set_target` will parse it as.
    let devices = list_audio_sinks();
    for dev in devices.iter().skip(1) {
        assert!(
            !dev.node_name.is_empty(),
            "non-default device has empty node_name: {dev:?}"
        );
        let parsed = dev.node_name.parse::<u32>();
        assert!(
            parsed.is_ok(),
            "node_name {:?} for device {:?} is not a decimal u32",
            dev.node_name,
            dev.display_name,
        );
    }
}

#[test]
fn set_target_stores_valid_id_when_idle() {
    // On an idle sink (audio_unit = None), set_target pre-validates
    // the format and then stores the string. No AudioUnit work
    // happens because there's nothing to swap; the next start()
    // will call open_unit and surface any device-resolution
    // failure (stale ID, etc.).
    let mut sink = AudioSink::new();
    sink.set_target("42")
        .expect("set_target with a valid id should succeed on an idle sink");
    assert_eq!(sink.target_device, "42");
}

#[test]
fn set_target_pre_validation_rejects_garbage_without_disturbing_state() {
    // The pre-validation step in set_target catches malformed
    // strings BEFORE touching the running AudioUnit (or, on an
    // idle sink, before mutating target_device). This is the
    // "doesn't take down a working sink for a typo" guarantee
    // CodeRabbit caught on PR #253.
    let mut sink = AudioSink::new();

    // Establish a known target so we can prove it survives the
    // failed call.
    sink.set_target("7").expect("baseline set_target");
    assert_eq!(sink.target_device, "7");

    let err = sink
        .set_target("not-a-number")
        .expect_err("set_target with garbage should fail pre-validation");
    assert!(
        matches!(err, SinkError::InvalidParameter(_)),
        "expected InvalidParameter, got {err:?}",
    );

    // target_device must NOT have been touched.
    assert_eq!(
        sink.target_device, "7",
        "failed pre-validation must not disturb the previous target"
    );
}

#[test]
fn set_target_is_idempotent_for_unchanged_target() {
    // Re-setting the same target should be a no-op fast path —
    // no stop/start cycle, no audible glitch, no failure surface
    // expansion. We can't observe the lack of a stop/start
    // directly here (no real AudioUnit involved on an idle
    // sink), but we can prove that the call returns Ok and
    // leaves target_device exactly equal to what was already
    // there.
    let mut sink = AudioSink::new();
    sink.set_target("42").expect("baseline");
    sink.set_target("42")
        .expect("re-setting the same target should succeed as a no-op");
    assert_eq!(sink.target_device, "42");

    // Same for the empty-string ("default device") case.
    sink.set_target("").expect("switch to default");
    sink.set_target("")
        .expect("re-setting empty should succeed as a no-op");
    assert!(sink.target_device.is_empty());
}

#[test]
fn set_target_empty_string_clears_to_default() {
    // Empty string = "system default output". The pre-validation
    // path treats it as Ok(None), so set_target accepts it and
    // clears the stored target.
    let mut sink = AudioSink::new();
    sink.set_target("42").expect("baseline");
    sink.set_target("")
        .expect("empty string should resolve to default device");
    assert!(sink.target_device.is_empty());
}

#[test]
fn parse_target_device_empty_means_default() {
    assert_eq!(parse_target_device("").unwrap(), None);
}

#[test]
fn parse_target_device_decimal_id_round_trips() {
    assert_eq!(parse_target_device("0").unwrap(), Some(0));
    assert_eq!(parse_target_device("42").unwrap(), Some(42));
    assert_eq!(
        parse_target_device(&u32::MAX.to_string()).unwrap(),
        Some(u32::MAX)
    );
}

#[test]
fn parse_target_device_rejects_garbage() {
    // Anything that isn't an empty string and isn't a decimal u32
    // surfaces InvalidParameter — both via the helper directly and
    // via open_unit when start() runs.
    for bad in ["not-a-number", "1.5", "0x42", "-1", "  ", "42abc"] {
        let err = parse_target_device(bad)
            .expect_err(&format!("expected parse_target_device({bad:?}) to fail"));
        assert!(
            matches!(err, SinkError::InvalidParameter(_)),
            "expected InvalidParameter for {bad:?}, got {err:?}"
        );
    }
}
