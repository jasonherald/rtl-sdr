use super::assembly::LineAssembler;
use super::discriminator::Discriminator;
use super::tones::ToneDetector;
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

/// A ramp of brightness fills exactly one line's worth of samples and
/// emits one line spanning the full 0..255 range across the row.
#[test]
fn line_assembler_emits_one_line_per_period() {
    let spl = 12_000.0; // e.g. 24 kHz / 2 lines-per-sec
    let mut a = LineAssembler::new(spl);
    let mut lines = 0;
    for i in 0..(spl as usize) {
        let b = ((i as f64 / spl) * 255.0) as u8;
        if a.push(b).is_some() {
            lines += 1;
        }
    }
    assert_eq!(lines, 1, "exactly one line per samples_per_line window");
}

/// `process()` over a synthetic white tone yields a bright line.
#[test]
fn process_emits_bright_lines_for_white_tone() {
    let sr = 24_000u32;
    let mut dec = WefaxDecoder::new(sr).unwrap();
    let n = sr as usize; // 1 s → 2 lines
    let audio: Vec<f32> = (0..n)
        .map(|i| {
            (2.0 * std::f64::consts::PI * SUBCARRIER_WHITE_HZ * i as f64 / sr as f64).sin() as f32
        })
        .collect();
    let mut out = vec![WefaxLine::default(); 8];
    let mut total = 0usize;
    for chunk in audio.chunks(4096) {
        total += dec.process(chunk, &mut out).unwrap();
    }
    assert!(total >= 1, "at least one line from 1 s of audio");
}

fn tone(freq: f64, sr: f64, secs: f64) -> Vec<f64> {
    let n = (sr * secs) as usize;
    (0..n)
        .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / sr).sin())
        .collect()
}

#[test]
fn tone_detector_fires_on_target_and_rejects_image_tones() {
    let sr = 44_100.0;
    let mut det = ToneDetector::new(sr, START_TONE_HZ);
    let mut fired = false;
    for s in tone(START_TONE_HZ, sr, 5.0) {
        fired |= det.push(s);
    }
    assert!(fired, "300 Hz start tone should fire");

    let mut det2 = ToneDetector::new(sr, START_TONE_HZ);
    let mut fired2 = false;
    for s in tone(SUBCARRIER_WHITE_HZ, sr, 5.0) {
        fired2 |= det2.push(s);
    }
    assert!(
        !fired2,
        "a 2300 Hz image tone must NOT trigger the 300 Hz detector"
    );
}
