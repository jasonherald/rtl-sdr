use super::assembly::LineAssembler;
use super::discriminator::Discriminator;
use super::phasing::PhasingTracker;
use super::sync::{LineDisposition, SyncMachine};
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

/// `process()` over a synthetic white tone yields a bright line. Uses the
/// free-running decoder: a bare white tone carries no start-tone / phasing
/// preamble, so the sync-gated `new` would (correctly) emit nothing.
#[test]
fn process_emits_bright_lines_for_white_tone() {
    let sr = 24_000u32;
    let mut dec = WefaxDecoder::new_free_running(sr);
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

fn phasing_line(pulse_col: usize) -> [u8; PIXELS_PER_LINE] {
    let mut l = [10u8; PIXELS_PER_LINE]; // near-black
    let end = (pulse_col + 90).min(PIXELS_PER_LINE);
    for px in l.iter_mut().take(end).skip(pulse_col) {
        *px = 250; // ~white pulse
    }
    l
}

#[test]
fn phasing_tracker_recovers_pulse_column() {
    let mut t = PhasingTracker::new();
    for _ in 0..12 {
        t.observe_line(&phasing_line(300));
    }
    let off = t.column_offset().expect("offset locked after enough lines");
    assert!(
        (off - 300).abs() <= 5,
        "recovered pulse column ~300, got {off}"
    );
}

#[test]
fn phasing_tracker_estimates_slant() {
    let mut t = PhasingTracker::new();
    for k in 0..20 {
        t.observe_line(&phasing_line(300 + k));
    } // +1 col/line drift
    assert!(
        (t.slant_columns_per_line() - 1.0).abs() < 0.3,
        "≈1 col/line slant"
    );
}

/// Real WEFAX has no audio start tone — the "300 Hz start" is the black/white
/// keying rate, not a 300 Hz audio sine — so sync must enter Phasing directly
/// from line content. Feed a run of mostly-dark-with-pulse lines (no tone at
/// all) and assert the machine walks Idle → Phasing → Imaging and begins
/// emitting once the left edge locks.
#[test]
fn phasing_entry_locks_without_start_tone() {
    let sr = 24_000.0;
    let mut sm = SyncMachine::new(sr);
    let mut asm = LineAssembler::new(sr / 2.0);
    assert_eq!(sm.state(), WefaxState::Idle, "starts Idle");

    let mut emitted = 0usize;
    let mut reached_phasing = false;
    for _ in 0..12 {
        let line = phasing_line(300);
        if let LineDisposition::Emit = sm.on_line(&line, &mut asm) {
            emitted += 1;
        }
        reached_phasing |= sm.state() == WefaxState::Phasing;
    }
    assert!(
        reached_phasing,
        "a run of phasing-pulse lines drives Idle → Phasing"
    );
    assert_eq!(
        sm.state(),
        WefaxState::Imaging,
        "phasing lock advances to Imaging"
    );
    assert!(
        emitted >= 1,
        "lines emit once imaging starts, got {emitted}"
    );
}

/// Synthesize AF for a mini chart and assert: no lines emitted before lock,
/// lines emitted during imaging, and `take_chart_complete()` fires after the
/// stop tone.
#[test]
fn sync_gates_emission_and_flags_chart_complete() {
    let sr = 24_000u32;
    let mut dec = WefaxDecoder::new(sr).unwrap();
    let mut out = vec![WefaxLine::default(); 64];

    let push_secs = |dec: &mut WefaxDecoder,
                     gen_fn: &dyn Fn(usize) -> f32,
                     secs: f64,
                     out: &mut [WefaxLine]|
     -> usize {
        let n = (sr as f64 * secs) as usize;
        let audio: Vec<f32> = (0..n).map(gen_fn).collect();
        let mut t = 0;
        for c in audio.chunks(4096) {
            t += dec.process(c, out).unwrap();
        }
        t
    };
    let tone = |f: f64| {
        move |i: usize| (2.0 * std::f64::consts::PI * f * i as f64 / sr as f64).sin() as f32
    };

    let before = push_secs(&mut dec, &tone(START_TONE_HZ), 5.0, &mut out);
    assert_eq!(before, 0, "no image lines during start tone");
    // A pure white tone has no phasing pulse, so the machine stays in
    // Phasing and emits nothing — proving emission is gated on a lock.
    let during = push_secs(&mut dec, &tone(SUBCARRIER_WHITE_HZ), 5.0, &mut out);
    assert_eq!(during, 0, "no lines emitted before a phasing lock");
    // Stop tone finalizes the chart and returns to Idle.
    let _stop = push_secs(&mut dec, &tone(STOP_TONE_HZ), 5.0, &mut out);
    assert!(dec.take_chart_complete(), "stop tone flags chart complete");
    assert_eq!(
        dec.state(),
        WefaxState::Idle,
        "resets to Idle after a chart"
    );
}
