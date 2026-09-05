use super::discriminator::Discriminator;
use super::*;

/// Feed a steady tone; the back half (LPF settled) maps to the expected
/// brightness: 1500 Hz→black(≈0), 1900→grey(≈128), 2300→white(≈255).
fn brightness_of_tone(freq_hz: f64, sr: f64) -> f64 {
    let mut d = Discriminator::new(sr);
    let n = sr as usize; // 1 second
    let mut acc = 0.0f64;
    let mut cnt = 0u32;
    for i in 0..n {
        let s = (2.0 * std::f64::consts::PI * freq_hz * i as f64 / sr).sin();
        let b = d.push(s) as f64;
        if i >= n / 2 {
            acc += b;
            cnt += 1;
        } // skip settling transient
    }
    acc / cnt as f64
}

#[test]
fn discriminator_maps_subcarrier_tones_to_brightness() {
    let sr = 44_100.0;
    assert!(brightness_of_tone(SUBCARRIER_BLACK_HZ, sr) < 20.0, "black");
    assert!(
        (brightness_of_tone(SUBCARRIER_CENTER_HZ, sr) - 128.0).abs() < 20.0,
        "grey"
    );
    assert!(brightness_of_tone(SUBCARRIER_WHITE_HZ, sr) > 235.0, "white");
}
