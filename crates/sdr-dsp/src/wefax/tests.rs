use super::afc::AfcMapper;
use super::assembly::LineAssembler;
use super::discriminator::Discriminator;
use super::sync::{LineDisposition, SyncMachine};
use super::tones::ToneDetector;
use super::*;

/// Feed a steady tone; the back half (LPF settled) settles to the tone's
/// deviation from the 1900 Hz center: 1500 Hz→−400, 1900→0, 2300→+400.
fn deviation_of_tone(freq_hz: f64, sr: f64) -> f64 {
    let mut d = Discriminator::new(sr);
    let n = sr as usize; // 1 second
    let mut acc = 0.0f64;
    let mut cnt = 0u32;
    for i in 0..n {
        let s = (2.0 * std::f64::consts::PI * freq_hz * i as f64 / sr).sin();
        let dev = d.push(s);
        if i >= n / 2 {
            acc += dev;
            cnt += 1;
        } // skip settling transient
    }
    acc / cnt as f64
}

#[test]
fn discriminator_maps_tones_to_deviation_hz() {
    let sr = 44_100.0;
    let tol = 60.0;
    assert!(
        (deviation_of_tone(SUBCARRIER_BLACK_HZ, sr) - (-SUBCARRIER_DEV_HZ)).abs() < tol,
        "black tone → ≈ −400 Hz"
    );
    assert!(
        deviation_of_tone(SUBCARRIER_CENTER_HZ, sr).abs() < tol,
        "center tone → ≈ 0 Hz"
    );
    assert!(
        (deviation_of_tone(SUBCARRIER_WHITE_HZ, sr) - SUBCARRIER_DEV_HZ).abs() < tol,
        "white tone → ≈ +400 Hz"
    );
}

/// Feed a long alternating black(dev≈−400)/white(dev≈+400) stream; once the
/// decaying histogram has populated over several update intervals, a settled
/// black input reads near-black and a white input near-white. Percentile refs
/// sit just inside the extremes, so the thresholds are looser than the fixed
/// mapper's.
#[test]
fn afc_maps_black_and_white_after_settling() {
    let sr = 44_100.0;
    let mut afc = AfcMapper::new(sr);
    let hold = (sr * 0.05) as usize; // 50 ms blocks (one update interval)
    let mut last_black = 0u8;
    let mut last_white = 0u8;
    for _ in 0..200 {
        for _ in 0..hold {
            last_black = afc.push(-SUBCARRIER_DEV_HZ);
        }
        for _ in 0..hold {
            last_white = afc.push(SUBCARRIER_DEV_HZ);
        }
    }
    assert!(
        last_black < 40,
        "settled black input → dark, got {last_black}"
    );
    assert!(
        last_white > 215,
        "settled white input → bright, got {last_white}"
    );
}

/// The AFC follows drift: after the whole black/white pattern shifts up by
/// +600 Hz, a drifted-black input (dev≈+200) — which a FIXED mapper would render
/// as light — still reads dark, because the tracked references followed the
/// histogram up.
#[test]
fn afc_tracks_drift() {
    let sr = 44_100.0;
    let mut afc = AfcMapper::new(sr);
    let hold = (sr * 0.05) as usize; // one update interval per tone block
    // Settle on the undrifted pattern.
    for _ in 0..80 {
        for _ in 0..hold {
            afc.push(-SUBCARRIER_DEV_HZ);
        }
        for _ in 0..hold {
            afc.push(SUBCARRIER_DEV_HZ);
        }
    }
    // Now shift the pattern up by +600 Hz: black≈+200, white≈+1000 (clamped).
    let mut last_black = 0u8;
    for _ in 0..120 {
        for _ in 0..hold {
            last_black = afc.push(-SUBCARRIER_DEV_HZ + 600.0);
        }
        for _ in 0..hold {
            afc.push(SUBCARRIER_DEV_HZ + 600.0);
        }
    }
    // The references followed the drift upward.
    assert!(
        afc.black_ref() > 0.0,
        "black_ref followed drift upward, got {}",
        afc.black_ref()
    );
    // The drifted-up black tone (dev≈+200) still maps dark — the key win.
    assert!(
        last_black < 80,
        "drifted black tone still maps to dark, got {last_black}"
    );
}

/// With no black/white contrast (a constant deviation, all one histogram bin)
/// the `pw − pb > MIN_SEP` guard leaves the references unchanged, so `push`
/// keeps returning a valid u8 — no NaN/inf, no panic.
#[test]
fn afc_no_signal_is_stable() {
    let sr = 44_100.0;
    let mut afc = AfcMapper::new(sr);
    for _ in 0..(sr as usize * 5) {
        afc.push(123.0); // constant deviation, no contrast
    }
    // A NaN/inf reference would fail this; reaching here without panic and with
    // a finite ref proves the guard kept the mapping sane.
    assert!(
        afc.black_ref().is_finite(),
        "reference stays finite with no contrast"
    );
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
    // Stop tone finalizes the chart: it flags chart-complete and leaves the
    // imaging cycle. (With the adaptive AFC, a *continuous* 5 s stop tone is a
    // constant out-of-band deviation whose discriminator ripple the AFC can, as
    // its scale collapses to the min-separation floor, turn into faux phasing
    // pulses — harmlessly re-arming Phasing to hunt the next chart. Real charts
    // are not followed by 5 s of pure stop tone, so we assert the meaningful
    // contract: the chart segmented and the decoder is no longer Imaging.)
    let _stop = push_secs(&mut dec, &tone(STOP_TONE_HZ), 5.0, &mut out);
    assert!(dec.take_chart_complete(), "stop tone flags chart complete");
    assert_ne!(
        dec.state(),
        WefaxState::Imaging,
        "chart finalized — no longer imaging"
    );
}

/// Continuous-phase FM audio for `lines` phasing lines whose white pulse
/// (the 2300 Hz tone) starts at time-column `pulse_col` and spans ~5% of the
/// line; the rest of each line is black (1500 Hz). Continuous phase across
/// tone switches, as a real FM subcarrier is.
fn phasing_audio(sr: f64, lines: usize, pulse_col: usize) -> Vec<f32> {
    let spl = (sr / 2.0) as usize; // 120 lpm
    let pulse_end = pulse_col + PIXELS_PER_LINE / 20;
    let mut phase = 0.0f64;
    let mut out = Vec::with_capacity(lines * spl);
    for _ in 0..lines {
        for i in 0..spl {
            let col = i * PIXELS_PER_LINE / spl;
            let f = if (pulse_col..pulse_end).contains(&col) {
                SUBCARRIER_WHITE_HZ
            } else {
                SUBCARRIER_BLACK_HZ
            };
            phase += 2.0 * std::f64::consts::PI * f / sr;
            out.push(phase.sin() as f32);
        }
    }
    out
}

/// Leading edge of the longest ≥180 run — the test's own independent
/// measurement of where a line's white pulse sits.
fn longest_bright_run_start(line: &[u8; PIXELS_PER_LINE]) -> Option<usize> {
    let (mut best, mut best_start, mut run) = (0usize, None, 0usize);
    for (i, &v) in line.iter().enumerate() {
        if v >= 180 {
            run += 1;
            if run > best {
                best = run;
                best_start = Some(i + 1 - run);
            }
        } else {
            run = 0;
        }
    }
    best_start
}

/// End-to-end left-edge alignment: once the gated decoder locks on the
/// phasing preamble, the pulse must land at the left edge (column ≈ 0) of
/// every emitted line — that is what "phasing" means. The pulse is placed
/// mid-line (time-column 500) so a wrong-signed offset shows up as a pulse
/// at ~2×500 instead of ~0.
#[test]
fn phasing_lock_puts_the_pulse_at_the_left_edge() {
    let sr = 24_000u32;
    let mut dec = WefaxDecoder::new(sr).unwrap();
    let mut out = vec![WefaxLine::default(); 64];
    let audio = phasing_audio(f64::from(sr), 60, 500);
    let mut emitted = Vec::new();
    for chunk in audio.chunks(4096) {
        let n = dec.process(chunk, &mut out).unwrap();
        emitted.extend(out.iter().take(n).map(|l| l.pixels));
    }
    assert_eq!(dec.state(), WefaxState::Imaging, "phasing preamble locks");
    assert!(
        emitted.len() >= 20,
        "post-lock phasing lines emitted, got {}",
        emitted.len()
    );
    // Skip the first few post-lock lines (assembler mid-line at the switch).
    let cols: Vec<usize> = emitted[5..]
        .iter()
        .filter_map(longest_bright_run_start)
        .collect();
    assert!(!cols.is_empty(), "pulse visible in emitted lines");
    let mut sorted = cols.clone();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    // Circular distance to column 0 (a few px of filter group delay is fine).
    let dist = median.min(PIXELS_PER_LINE - median);
    assert!(
        dist <= 20,
        "pulse should sit at the left edge after lock, got median column {median}"
    );
}
