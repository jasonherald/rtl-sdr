use super::*;

#[test]
fn test_radio_module_default_mode() {
    let radio = RadioModule::with_default_rate().unwrap();
    assert_eq!(radio.current_mode(), DemodMode::Wfm);
}

#[test]
fn test_radio_module_mode_switching() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    let modes = [
        DemodMode::Wfm,
        DemodMode::Nfm,
        DemodMode::Am,
        DemodMode::Usb,
        DemodMode::Lsb,
        DemodMode::Dsb,
        DemodMode::Cw,
        DemodMode::Raw,
    ];
    for mode in modes {
        radio.set_mode(mode).unwrap();
        assert_eq!(radio.current_mode(), mode);
    }
}

#[test]
fn test_radio_module_process_nfm() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    // Generate FM-modulated signal
    let input: Vec<Complex> = (0..1000)
        .map(|i| {
            let phase = 2.0 * PI * 1000.0 * (i as f32) / 50_000.0;
            Complex::new(phase.cos(), phase.sin())
        })
        .collect();
    let mut output = vec![Stereo::default(); 2000];
    let count = radio.process(&input, &mut output).unwrap();
    // NFM: 50kHz -> 48kHz, so output count should be ~960
    assert!(count > 0, "should produce output");
    assert!(count <= 2000, "should not overflow");
}

#[test]
fn test_radio_module_process_am() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Am).unwrap();

    // AM signal: carrier with amplitude modulation
    let input: Vec<Complex> = (0..1000)
        .map(|i| {
            let amp = 1.0 + 0.5 * (2.0 * PI * 0.01 * i as f32).sin();
            Complex::new(amp, 0.0)
        })
        .collect();
    let mut output = vec![Stereo::default(); 5000];
    let count = radio.process(&input, &mut output).unwrap();
    // AM: 15kHz -> 48kHz, output should be upsampled
    assert!(count > 0, "should produce output");
}

#[test]
fn test_radio_module_process_raw() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Raw).unwrap();

    let input = vec![Complex::new(0.5, -0.3); 100];
    let mut output = vec![Stereo::default(); 200];
    let count = radio.process(&input, &mut output).unwrap();
    // Raw: 48kHz -> 48kHz, no resampling needed
    assert_eq!(count, 100);
    // Should pass through IQ as stereo (after IF chain which is passthrough when disabled)
    assert!((output[0].l - 0.5).abs() < 1e-4);
    assert!((output[0].r - (-0.3)).abs() < 1e-4);
}

#[test]
fn test_radio_module_process_empty() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    let mut output = vec![Stereo::default(); 100];
    let count = radio.process(&[], &mut output).unwrap();
    assert_eq!(count, 0);
}

#[test]
fn test_radio_module_deemphasis() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Wfm).unwrap();
    // Enable deemphasis
    radio.set_deemp_mode(DeemphasisMode::Eu50).unwrap();
    assert!(radio.demod_config().deemp_allowed);

    // Switch to a mode that doesn't support deemphasis
    radio.set_mode(DemodMode::Am).unwrap();
    assert!(!radio.demod_config().deemp_allowed);
}

#[test]
fn test_radio_module_deemp_mode_tau() {
    assert!((DeemphasisMode::Us75.tau() - 75e-6).abs() < 1e-10);
    assert!((DeemphasisMode::Eu50.tau() - 50e-6).abs() < 1e-10);
    assert!((DeemphasisMode::None.tau() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_radio_module_config_access() {
    let radio = RadioModule::with_default_rate().unwrap();
    let cfg = radio.demod_config();
    assert!(cfg.if_sample_rate > 0.0);
    assert!(cfg.af_sample_rate > 0.0);
}

#[test]
fn test_radio_module_if_chain_access() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.if_chain_mut().set_nb_enabled(true);
    assert!(radio.if_chain().nb_enabled());
}

#[test]
fn test_radio_module_set_bandwidth() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Usb).unwrap();
    // Should not panic or error
    radio.set_bandwidth(3000.0);
}

#[test]
fn test_radio_error_display() {
    let err = RadioError::Dsp(DspError::InvalidParameter("test".to_string()));
    let msg = format!("{err}");
    assert!(msg.contains("DSP error"));

    let err = RadioError::ModeSwitchFailed("test".to_string());
    let msg = format!("{err}");
    assert!(msg.contains("mode switch failed"));
}

#[test]
fn test_radio_module_mode_switch_preserves_deemp() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Wfm).unwrap();
    radio.set_deemp_mode(DeemphasisMode::Eu50).unwrap();

    // Switch to another FM mode (NFM doesn't support deemp)
    radio.set_mode(DemodMode::Nfm).unwrap();
    // Deemp mode should be preserved in the radio module
    // but disabled in the AF chain since NFM doesn't allow it

    // Switch back to WFM
    radio.set_mode(DemodMode::Wfm).unwrap();
    // The deemp mode is still Eu50 in the radio, and WFM allows it
    assert!(radio.af_chain().deemp_enabled());
}

/// #738 — the software IF AGC only runs in modes whose config
/// allows it; Raw / LRPT / CW keep their amplitude. The user's
/// preference is remembered and re-applied on a mode that allows it.
#[test]
fn software_if_agc_is_gated_by_the_demod_config() {
    let mut radio = RadioModule::with_default_rate().unwrap();
    radio.set_mode(DemodMode::Nfm).unwrap();
    radio.set_software_agc_enabled(true);
    assert!(radio.if_chain().software_agc_enabled());

    radio.set_mode(DemodMode::Raw).unwrap();
    assert!(!radio.demod_config().if_agc_allowed);
    assert!(
        !radio.if_chain().software_agc_enabled(),
        "Raw passes IQ through untouched"
    );
    radio.set_software_agc_enabled(true);
    assert!(
        !radio.if_chain().software_agc_enabled(),
        "cannot be enabled live on Raw"
    );

    radio.set_mode(DemodMode::Nfm).unwrap();
    assert!(
        radio.if_chain().software_agc_enabled(),
        "preference re-applied on NFM"
    );
    radio.set_software_agc_enabled(false);
    radio.set_mode(DemodMode::Wfm).unwrap();
    assert!(!radio.if_chain().software_agc_enabled());
}
