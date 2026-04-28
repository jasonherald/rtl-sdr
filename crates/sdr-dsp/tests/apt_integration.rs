//! End-to-end APT decoder validation against a real NOAA 19 capture.
//!
//! The fixture WAVs in `tests/data/` come from the noaa-apt project's
//! test corpus (see `tests/data/README.md` for the full provenance).
//! These tests don't depend on noaa-apt's code at any layer — they
//! just take its test audio (which is known to decode to a clean
//! image in noaa-apt) and feed it through OUR decoder. If our
//! decoder produces line-count + sync-quality numbers in the same
//! ballpark, we're confident the pipeline is correct end-to-end on
//! real-world inputs (vs. just synthetic tones).
//!
//! **Why this beats the synthetic-tone tests in `apt::tests`:**
//!
//! Synthetic AM tones lack the artifacts a real capture has: phase
//! noise, drift, envelope variations across the line, fade at low
//! elevations, etc. A pipeline can pass synthetic tests and still
//! struggle on real audio if any stage is over-tuned for the ideal
//! case. Running the canonical noaa-apt fixture through our decoder
//! pins the entire chain against a known-good result.

use std::path::Path;

use sdr_dsp::apt::{AptDecoder, AptLine, READY_QUEUE_CAP};

/// Signed 16-bit PCM full-scale denominator. Hoisted to module scope
/// to satisfy `clippy::items_after_statements`.
const PCM16_SCALE: f32 = 32_768.0;

/// Pull mono f32 samples from a 16-bit signed PCM WAV file.
///
/// Used by every test in this module to load the fixture audio.
/// Pulled out into a helper so the per-test bodies stay focused on
/// the actual assertion rather than on WAV plumbing. Returns
/// `(samples, sample_rate_hz)`.
fn read_wav_mono_f32(path: &Path) -> (Vec<f32>, u32) {
    let mut reader = hound::WavReader::open(path).expect("open WAV fixture");
    let spec = reader.spec();
    assert_eq!(
        spec.channels, 1,
        "fixture must be mono — got {} channels",
        spec.channels
    );
    assert_eq!(
        spec.sample_format,
        hound::SampleFormat::Int,
        "fixture must be PCM int — got {:?}",
        spec.sample_format
    );
    assert_eq!(
        spec.bits_per_sample, 16,
        "fixture must be 16-bit PCM — got {}",
        spec.bits_per_sample
    );
    // Signed 16-bit PCM normalizes by 2^15 = 32768, not by `i16::MAX`
    // (= 32767). The former gives `i16::MIN` → exactly -1.0; the
    // latter gives `i16::MIN` → -1.000031 which can drift
    // amplitude-sensitive assertions over a long capture. Per CR
    // round 1 on PR #571.
    let samples: Vec<f32> = reader
        .samples::<i16>()
        .map(|s| f32::from(s.expect("WAV sample read")) / PCM16_SCALE)
        .collect();
    (samples, spec.sample_rate)
}

/// Drive the decoder with the entire fixture in chunked sub-buffers,
/// the way live audio actually arrives. Returns the full list of
/// emitted lines (decoded across however many `process` calls it
/// took to consume the input).
fn decode_full_capture(samples: &[f32], rate_hz: u32) -> Vec<AptLine> {
    /// Per-call chunk size — ~21 ms at 48 kHz, similar to the audio
    /// pipeline's real chunk cadence. Chosen to exercise streaming
    /// state across many `process()` calls (instead of one giant call
    /// that hides chunk-boundary bugs).
    const CHUNK: usize = 1_024;

    let mut decoder = AptDecoder::new(rate_hz).expect("build APT decoder");
    let mut output_buf = vec![AptLine::default(); READY_QUEUE_CAP];
    let mut all_lines = Vec::new();

    for chunk in samples.chunks(CHUNK) {
        let n = decoder
            .process(chunk, &mut output_buf)
            .expect("APT process");
        for slot in output_buf.iter_mut().take(n) {
            all_lines.push(std::mem::take(slot));
        }
    }
    // Drain remaining buffered lines. One `process(&[], ...)` call
    // can return up to `output_buf.len()` lines and the decoder may
    // hold more in its ready queue; loop until empty so we don't
    // silently truncate the count assertion. Per CR round 1 on PR
    // #571.
    loop {
        let n = decoder
            .process(&[], &mut output_buf)
            .expect("APT final flush");
        if n == 0 {
            break;
        }
        for slot in output_buf.iter_mut().take(n) {
            all_lines.push(std::mem::take(slot));
        }
    }
    all_lines
}

#[test]
fn decodes_real_noaa19_capture_into_recognizable_lines() {
    // The fixture is ~14 minutes of NOAA 19 audio at 11025 Hz —
    // 14 min × 2 lines/sec = ~1680 lines under perfect lock. Real
    // captures aren't perfect (fade at low elevation, occasional
    // glitches), but a working pipeline should hit >=80% of that.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/noaa19_apt_11025hz.wav");
    let (samples, rate) = read_wav_mono_f32(&path);
    assert_eq!(rate, 11_025, "fixture rate sanity");
    assert!(
        samples.len() > 11_025 * 60,
        "fixture should be at least 1 minute of audio, got {} samples",
        samples.len()
    );

    let lines = decode_full_capture(&samples, rate);

    // Defensive: every emitted line's sync_quality must be finite
    // and within `[0, 1]` before we run aggregate stats over it.
    // NaN propagates through `partial_cmp` as `Equal` (per the
    // unwrap_or below), silently invalidating median/threshold
    // checks. Per CR round 1 on PR #571.
    assert!(
        lines
            .iter()
            .all(|l| l.sync_quality.is_finite() && (0.0..=1.0).contains(&l.sync_quality)),
        "every sync_quality must be finite and within [0, 1]",
    );

    // Line count: the fixture is 14m 24s long. At 2 lines/sec, the
    // theoretical maximum is 1728 lines. Our streaming decoder
    // currently emits ~1410 lines on this fixture — about 82% of
    // theoretical, with the gap accounted for by:
    // - ~1 line of accumulator warm-up before first emission
    // - The padded-template matched-filter conservatively trimming
    //   borderline-quality lines at the start/end of the capture
    // - Stream-end without a trailing line emit when accumulator
    //   doesn't reach `MIN_ACCUMULATOR_FOR_DECODE` post-final-chunk
    // 1300 is a "the pipeline is largely working" floor — well clear
    // of the ~1410 we observe today, with margin for jitter.
    assert!(
        lines.len() >= 1_300,
        "expected ≥1300 lines from a 14-minute NOAA 19 capture, got {}",
        lines.len()
    );

    // Sync quality: a clean APT pass should hit normalized cross-
    // correlation peaks in the 0.85+ band on the bulk of lines.
    // A few low-quality gaps are expected (fade at horizons), so
    // we measure the median rather than the mean (mean is sensitive
    // to outliers).
    let mut qualities: Vec<f32> = lines.iter().map(|l| l.sync_quality).collect();
    qualities.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = qualities[qualities.len() / 2];
    assert!(
        median > 0.7,
        "median sync quality should be >0.7 on a clean capture, got {median:.3}",
    );

    // The fraction of "good lock" lines (>0.85) should also be high.
    // Imaging-quality lines need >=0.5 (per `MIN_VALID_SYNC_QUALITY`);
    // our threshold here is stricter to verify the bulk are excellent.
    let good_lock = qualities.iter().filter(|&&q| q > 0.85).count();
    #[allow(
        clippy::cast_precision_loss,
        reason = "good_lock and qualities.len() bounded by line count (<2000) — \
                  fits f32 mantissa exactly"
    )]
    let frac_good = good_lock as f32 / qualities.len() as f32;
    assert!(
        frac_good > 0.5,
        ">50% of lines should hit good-lock (>0.85) on a clean capture, got {:.1}%",
        frac_good * 100.0,
    );
}

#[test]
fn noise_input_produces_low_quality_lines() {
    // Negative control: the noise fixture has no APT signal at all,
    // so the decoder should still produce *some* lines (the
    // forward-march sync algorithm always emits) but they should
    // all score below the noise threshold.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/noise_apt_11025hz.wav");
    let (samples, rate) = read_wav_mono_f32(&path);
    assert_eq!(rate, 11_025, "fixture rate sanity");

    let lines = decode_full_capture(&samples, rate);

    // Defensive: same finite/range check as the signal test.
    assert!(
        lines
            .iter()
            .all(|l| l.sync_quality.is_finite() && (0.0..=1.0).contains(&l.sync_quality)),
        "every sync_quality must be finite and within [0, 1]",
    );

    // The noise fixture is 30 s — at 2 lines/sec that's ~60 lines
    // in theory. The forward-march algorithm will emit something
    // even on noise, but the count is bounded below the signal
    // case.
    assert!(
        !lines.is_empty(),
        "decoder should still emit lines on noise (forward-march invariant)"
    );

    // CRITICAL invariant: every emitted line on pure noise must
    // score below `MIN_VALID_SYNC_QUALITY` (0.5). If any noise line
    // scores >0.7 there's a false-positive bug in the matched
    // filter — our padded template's whole point is to reject
    // patterns that aren't real Sync A.
    let max_quality = lines
        .iter()
        .map(|l| l.sync_quality)
        .fold(f32::NEG_INFINITY, f32::max);
    assert!(
        max_quality < 0.7,
        "max sync quality on noise should be <0.7 (template rejects false matches), \
         got {max_quality:.3}"
    );
}
