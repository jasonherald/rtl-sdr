use super::*;
// Import-surface adjustments for the #819 module split (PR #880
// pattern): items the old monolithic root held directly now live
// in submodules.
use super::bookmarks::string_to_demod_mode;
use super::list::bookmark_matches_filter;
use super::panel::BAND_PRESETS;
use sdr_types::DemodMode;

#[test]
fn format_frequency_mhz() {
    assert_eq!(format_frequency(98_100_000), "98.100 MHz");
    assert_eq!(format_frequency(162_550_000), "162.550 MHz");
}

#[test]
fn format_frequency_ghz() {
    assert_eq!(format_frequency(1_090_000_000), "1.090 GHz");
}

#[test]
fn format_frequency_khz() {
    assert_eq!(format_frequency(500_000), "500.0 kHz");
}

#[test]
fn format_frequency_hz() {
    assert_eq!(format_frequency(440), "440 Hz");
}

#[test]
fn demod_mode_roundtrip() {
    let modes = [
        DemodMode::Wfm,
        DemodMode::Nfm,
        DemodMode::Am,
        DemodMode::Usb,
        DemodMode::Lsb,
        DemodMode::Dsb,
        DemodMode::Cw,
        DemodMode::Raw,
        // Per CodeRabbit round 1 on PR #543 — pin Lrpt
        // bidirectionally so a future variant addition
        // can't silently demote LRPT bookmarks back to NFM.
        DemodMode::Lrpt,
    ];
    for mode in modes {
        let s = demod_mode_to_string(mode);
        let back = string_to_demod_mode(&s);
        assert_eq!(mode, back, "roundtrip failed for {mode:?}");
    }
}

#[test]
fn band_presets_have_valid_data() {
    for preset in BAND_PRESETS {
        assert!(!preset.name.is_empty());
        assert!(preset.frequency > 0);
        assert!(preset.bandwidth > 0.0);
    }
}

#[test]
fn bookmark_serialization_roundtrip() {
    let bm = Bookmark::new("Test", 100_000_000, DemodMode::Wfm, 150_000.0);
    let json = serde_json::to_string(&bm).unwrap();
    let back: Bookmark = serde_json::from_str(&json).unwrap();
    assert_eq!(back.name, "Test");
    assert_eq!(back.frequency, 100_000_000);
    assert_eq!(back.demod_mode, "WFM");
    assert!((back.bandwidth - 150_000.0).abs() < f64::EPSILON);
    // Optional fields default to None for basic bookmark
    assert!(back.squelch_enabled.is_none());
    assert!(back.gain.is_none());
}

#[test]
fn bookmark_full_roundtrip() {
    let profile = TuningProfile {
        squelch_enabled: true,
        auto_squelch_enabled: true,
        squelch_level: -40.0,
        gain: 33.8,
        agc_type: crate::sidebar::source_panel::AgcType::Off,
        volume: Some(0.75),
        deemphasis: 2,
        nb_enabled: false,
        nb_level: 5.0,
        fm_if_nr: true,
        wfm_stereo: true,
        high_pass: Some(false),
        ctcss_mode: Some(sdr_radio::af_chain::CtcssMode::Tone(100.0)),
        ctcss_threshold: Some(0.15),
        voice_squelch_mode: Some(sdr_dsp::voice_squelch::VoiceSquelchMode::Snr {
            threshold_db: 9.0,
        }),
    };
    let bm = Bookmark::with_profile("Full", 98_100_000, DemodMode::Wfm, 150_000.0, &profile);
    let json = serde_json::to_string(&bm).unwrap();
    let back: Bookmark = serde_json::from_str(&json).unwrap();
    assert_eq!(back.squelch_enabled, Some(true));
    assert_eq!(back.auto_squelch_enabled, Some(true));
    assert_eq!(back.squelch_level, Some(-40.0));
    assert_eq!(back.gain, Some(33.8));
    // Legacy `agc` boolean is written alongside the new
    // `agc_type` for forward-compat with older builds. For
    // `AgcType::Off` that legacy value is `false`.
    assert_eq!(back.agc, Some(false));
    // New `agc_type` round-trip. Regression guard against a
    // serde-shape change on `AgcType` silently breaking the
    // bookmark schema.
    assert_eq!(
        back.agc_type,
        Some(crate::sidebar::source_panel::AgcType::Off)
    );
    assert_eq!(back.volume, Some(0.75));
    assert_eq!(back.deemphasis, Some(2));
    assert_eq!(back.nb_enabled, Some(false));
    assert_eq!(back.nb_level, Some(5.0));
    assert_eq!(back.fm_if_nr, Some(true));
    assert_eq!(back.wfm_stereo, Some(true));
    assert_eq!(back.high_pass, Some(false));
    assert_eq!(
        back.ctcss_mode,
        Some(sdr_radio::af_chain::CtcssMode::Tone(100.0))
    );
    assert_eq!(back.ctcss_threshold, Some(0.15));
    assert_eq!(
        back.voice_squelch_mode,
        Some(sdr_dsp::voice_squelch::VoiceSquelchMode::Snr { threshold_db: 9.0 })
    );
}

#[test]
fn bookmark_backward_compat_ctcss_none() {
    // Old bookmark JSON (pre-PR-3) has neither ctcss_mode nor
    // ctcss_threshold. Deserialization must yield None for
    // both, which restore_bookmark_profile interprets as
    // "leave the current CTCSS setting alone."
    let old_json =
        r#"{"name":"Legacy","frequency":162550000,"demod_mode":"NFM","bandwidth":12500.0}"#;
    let bm: Bookmark = serde_json::from_str(old_json).unwrap();
    assert!(bm.ctcss_mode.is_none());
    assert!(bm.ctcss_threshold.is_none());
    // Voice squelch is newer than CTCSS, so pre-PR bookmarks
    // also lack this key — must deserialize to None which
    // restore_bookmark_profile interprets as "leave current
    // voice squelch setting alone."
    assert!(bm.voice_squelch_mode.is_none());
}

#[test]
fn bookmark_backward_compat_deserialize() {
    // Simulates loading an old bookmarks.json that lacks optional fields.
    let old_json = r#"{"name":"Old","frequency":162550000,"demod_mode":"NFM","bandwidth":12500.0}"#;
    let bm: Bookmark = serde_json::from_str(old_json).unwrap();
    assert_eq!(bm.name, "Old");
    assert_eq!(bm.frequency, 162_550_000);
    assert!(bm.squelch_enabled.is_none());
    assert!(bm.auto_squelch_enabled.is_none());
    assert!(bm.gain.is_none());
    assert!(bm.volume.is_none());
}

#[test]
fn bookmark_settings_subtitle_basic() {
    let bm = Bookmark::new("Test", 98_100_000, DemodMode::Wfm, 150_000.0);
    let sub = bm.settings_subtitle();
    assert!(sub.contains("98.100 MHz"));
    assert!(sub.contains("WFM"));
}

#[test]
fn bookmark_settings_subtitle_with_squelch() {
    let mut bm = Bookmark::new("Test", 162_550_000, DemodMode::Nfm, 12_500.0);
    bm.squelch_enabled = Some(true);
    bm.squelch_level = Some(-40.0);
    bm.gain = Some(33.8);
    let sub = bm.settings_subtitle();
    // Compact format: "NFM 162.550 MHz"
    assert!(sub.contains("NFM"));
    assert!(sub.contains("162.550 MHz"));
}

#[test]
fn filter_empty_needle_matches_everything() {
    let bm = Bookmark::new("Anything", 162_550_000, DemodMode::Nfm, 12_500.0);
    assert!(bookmark_matches_filter(&bm, ""));
}

#[test]
fn filter_matches_name_case_insensitive() {
    let bm = Bookmark::new("Weather", 162_550_000, DemodMode::Nfm, 12_500.0);
    // `bookmark_matches_filter` assumes the caller has already
    // lowercased the needle (the search-entry handler does so).
    assert!(bookmark_matches_filter(&bm, "weather"));
    assert!(!bookmark_matches_filter(&bm, "aviation"));
}

#[test]
fn filter_matches_subtitle_demod_and_frequency() {
    let bm = Bookmark::new("Stuff", 162_550_000, DemodMode::Nfm, 12_500.0);
    // Subtitle is "NFM 162.550 MHz" — lowercase matches both parts.
    assert!(bookmark_matches_filter(&bm, "nfm"));
    assert!(bookmark_matches_filter(&bm, "162.550"));
    assert!(bookmark_matches_filter(&bm, "mhz"));
}

#[test]
fn filter_no_match_hides_bookmark() {
    let bm = Bookmark::new("Weather", 162_550_000, DemodMode::Nfm, 12_500.0);
    assert!(!bookmark_matches_filter(&bm, "xyz-no-match"));
}

#[test]
fn filter_matches_rr_category() {
    // Users who import from RadioReference expect to search
    // by category ("dispatch", "fire") even when that text
    // isn't in the bookmark's name or subtitle.
    let mut bm = Bookmark::new("Unit 14", 462_562_500, DemodMode::Nfm, 12_500.0);
    bm.rr_category = Some("Law Dispatch".to_string());
    assert!(bookmark_matches_filter(&bm, "dispatch"));
    assert!(bookmark_matches_filter(&bm, "law"));
    assert!(!bookmark_matches_filter(&bm, "fire"));
}

#[test]
fn bookmark_scanner_fields_default_on_old_json() {
    // Old pre-scanner bookmark JSON (no scanner fields present).
    let old_json = r#"{"name":"Old","frequency":162550000,"demod_mode":"NFM","bandwidth":12500.0}"#;
    let bm: Bookmark = serde_json::from_str(old_json).unwrap();
    assert!(!bm.scan_enabled);
    assert_eq!(bm.priority, 0);
    assert!(bm.dwell_ms_override.is_none());
    assert!(bm.hang_ms_override.is_none());
}

#[test]
fn bookmark_scanner_fields_roundtrip() {
    /// Override dwell in ms; distinct from the scanner's
    /// `DEFAULT_DWELL_MS` (100) so the roundtrip assertion
    /// would fail if serde dropped the override field and
    /// the default re-hydrated.
    const TEST_SCANNER_DWELL_MS: u32 = 200;
    /// Override hang in ms; same rationale — distinct from
    /// the scanner default (2000).
    const TEST_SCANNER_HANG_MS: u32 = 3_000;

    let mut bm = Bookmark::new("Test", 146_520_000, DemodMode::Nfm, 12_500.0);
    bm.scan_enabled = true;
    bm.priority = 1;
    bm.dwell_ms_override = Some(TEST_SCANNER_DWELL_MS);
    bm.hang_ms_override = Some(TEST_SCANNER_HANG_MS);
    let json = serde_json::to_string(&bm).unwrap();
    let back: Bookmark = serde_json::from_str(&json).unwrap();
    assert!(back.scan_enabled);
    assert_eq!(back.priority, 1);
    assert_eq!(back.dwell_ms_override, Some(TEST_SCANNER_DWELL_MS));
    assert_eq!(back.hang_ms_override, Some(TEST_SCANNER_HANG_MS));
}

// ---- project_scanner_channels ----

/// Test default dwell, distinct from `sdr_scanner::DEFAULT_DWELL_MS`
/// (100) so the default-vs-override assertions can tell which one
/// the projector resolved.
const TEST_DEFAULT_DWELL_MS: u32 = 125;
/// Test default hang, distinct from `sdr_scanner::DEFAULT_HANG_MS`
/// (2000) for the same reason.
const TEST_DEFAULT_HANG_MS: u32 = 1_500;
/// A per-channel dwell override distinct from both the scanner
/// default and `TEST_DEFAULT_DWELL_MS`, so "override wins" is
/// observable as a unique value in assertions.
const TEST_OVERRIDE_DWELL_MS: u32 = 250;
/// A per-channel hang override distinct from both the scanner
/// default and `TEST_DEFAULT_HANG_MS`, same rationale.
const TEST_OVERRIDE_HANG_MS: u32 = 3_250;

#[test]
fn project_scanner_channels_filters_disabled() {
    let mut enabled = Bookmark::new("On", 146_520_000, DemodMode::Nfm, 12_500.0);
    enabled.scan_enabled = true;
    let disabled = Bookmark::new("Off", 162_550_000, DemodMode::Nfm, 12_500.0);
    let bms = vec![enabled, disabled];

    let channels = project_scanner_channels(&bms, TEST_DEFAULT_DWELL_MS, TEST_DEFAULT_HANG_MS);

    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].key.name, "On");
    assert_eq!(channels[0].key.frequency_hz, 146_520_000);
}

#[test]
fn project_scanner_channels_override_wins_over_default() {
    let mut with_overrides = Bookmark::new("Overrides", 146_520_000, DemodMode::Nfm, 12_500.0);
    with_overrides.scan_enabled = true;
    with_overrides.dwell_ms_override = Some(TEST_OVERRIDE_DWELL_MS);
    with_overrides.hang_ms_override = Some(TEST_OVERRIDE_HANG_MS);
    let mut no_overrides = Bookmark::new("Defaults", 162_550_000, DemodMode::Nfm, 12_500.0);
    no_overrides.scan_enabled = true;
    let bms = vec![with_overrides, no_overrides];

    let channels = project_scanner_channels(&bms, TEST_DEFAULT_DWELL_MS, TEST_DEFAULT_HANG_MS);

    assert_eq!(channels.len(), 2);
    // First bookmark: override wins.
    assert_eq!(channels[0].dwell_ms, TEST_OVERRIDE_DWELL_MS);
    assert_eq!(channels[0].hang_ms, TEST_OVERRIDE_HANG_MS);
    // Second bookmark: no overrides, default folds in.
    assert_eq!(channels[1].dwell_ms, TEST_DEFAULT_DWELL_MS);
    assert_eq!(channels[1].hang_ms, TEST_DEFAULT_HANG_MS);
}

#[test]
fn project_scanner_channels_propagates_priority_and_squelch() {
    let mut bm = Bookmark::new("Priority", 146_520_000, DemodMode::Nfm, 12_500.0);
    bm.scan_enabled = true;
    bm.priority = 1;
    bm.ctcss_mode = Some(sdr_radio::af_chain::CtcssMode::Tone(100.0));
    bm.voice_squelch_mode =
        Some(sdr_dsp::voice_squelch::VoiceSquelchMode::Snr { threshold_db: 9.0 });

    let channels = project_scanner_channels(&[bm], TEST_DEFAULT_DWELL_MS, TEST_DEFAULT_HANG_MS);

    assert_eq!(channels.len(), 1);
    let ch = &channels[0];
    assert_eq!(ch.priority, 1);
    assert_eq!(ch.ctcss, Some(sdr_radio::af_chain::CtcssMode::Tone(100.0)));
    assert_eq!(
        ch.voice_squelch,
        Some(sdr_dsp::voice_squelch::VoiceSquelchMode::Snr { threshold_db: 9.0 })
    );
    // Demod mode is parsed from the string form on the bookmark.
    assert_eq!(ch.demod_mode, DemodMode::Nfm);
    assert!((ch.bandwidth - 12_500.0).abs() < f64::EPSILON);
}

/// Per #516: envelope of an empty channel list is `None`.
#[test]
fn scanner_channel_envelope_empty_returns_none() {
    let channels: Vec<sdr_scanner::ScannerChannel> = Vec::new();
    assert_eq!(scanner_channel_envelope(&channels), None);
}

/// Per #516: single channel returns
/// `(centre - bw/2, centre + bw/2)`.
#[test]
fn scanner_channel_envelope_single_channel() {
    let channels = vec![sdr_scanner::ScannerChannel {
        key: sdr_scanner::ChannelKey {
            name: "WX".to_string(),
            frequency_hz: 162_550_000,
        },
        demod_mode: DemodMode::Nfm,
        bandwidth: 12_500.0,
        ctcss: None,
        voice_squelch: None,
        priority: 0,
        dwell_ms: 1_000,
        hang_ms: 3_000,
    }];
    let env = scanner_channel_envelope(&channels);
    assert_eq!(env, Some((162_543_750.0, 162_556_250.0)));
}

/// Per #516: multiple channels span lowest-edge to
/// highest-edge across the union of all passbands. Channels
/// with different bandwidths are handled correctly — the
/// envelope must reach the actual edge of the WIDEST
/// channel at each end, not just the centres.
#[test]
fn scanner_channel_envelope_multi_channel_uses_edges() {
    let channels = vec![
        sdr_scanner::ScannerChannel {
            key: sdr_scanner::ChannelKey {
                name: "Lower (NFM)".to_string(),
                frequency_hz: 146_000_000,
            },
            demod_mode: DemodMode::Nfm,
            bandwidth: 12_500.0,
            ctcss: None,
            voice_squelch: None,
            priority: 0,
            dwell_ms: 1_000,
            hang_ms: 3_000,
        },
        sdr_scanner::ScannerChannel {
            key: sdr_scanner::ChannelKey {
                name: "Upper (WFM)".to_string(),
                frequency_hz: 155_000_000,
            },
            demod_mode: DemodMode::Wfm,
            bandwidth: 200_000.0,
            ctcss: None,
            voice_squelch: None,
            priority: 0,
            dwell_ms: 1_000,
            hang_ms: 3_000,
        },
    ];
    let env = scanner_channel_envelope(&channels);
    // Lower edge: 146M - 6250 = 145.99375M
    // Upper edge: 155M + 100k = 155.100M
    assert_eq!(env, Some((145_993_750.0, 155_100_000.0)));
}
