//! Real-signal regression gate for the METEOR-M2 LRPT catalog
//! profile (#892).
//!
//! The station's first successful METEOR-M2 4 decode (2026-09-04)
//! proved the downlink is differentially-precoded OQPSK — the
//! catalog had `lrpt_differential = false`, and every live pass
//! silently decoded nothing as a result. This test locks in the
//! empirical finding: it runs the *exact* demod + FEC profile the
//! corrected catalog produces for M2-3 / M2-4 (OQPSK +
//! differential) against a real captured pass and asserts CADUs
//! actually decode.
//!
//! `#[ignore]`d by default — it needs a multi-hundred-MB IQ fixture
//! that isn't committed. Point `METEOR_LRPT_IQ_FIXTURE` at a
//! complex<f32>-interleaved `.iq` file at the LRPT working rate
//! ([`sdr_dsp::lrpt::SAMPLE_RATE_HZ`] = 144 ksps) — the same format
//! `sdr-lrpt-replay` consumes — and run:
//!
//! ```text
//! METEOR_LRPT_IQ_FIXTURE=/path/to/pass-144k.iq \
//!   cargo test -p sdr-lrpt --test meteor_m2_real_capture -- --ignored
//! ```
//!
//! Regenerate a fixture from a raw 2.5 Msps IQ WAV with the
//! offline front-end (Doppler-corrected resample to 144 ksps); see
//! #892 / #893 for that path.
#![allow(clippy::panic)]

use std::io::{BufReader, Read};

use sdr_dsp::lrpt::{LrptDemod, LrptMode};
use sdr_lrpt::LrptPipeline;
use sdr_types::Complex;

const FIXTURE_ENV: &str = "METEOR_LRPT_IQ_FIXTURE";
/// Bytes per interleaved complex<f32> sample (re, im).
const IQ_SAMPLE_BYTES: usize = 8;
/// Read the fixture in ~1 M-sample chunks to bound memory.
const CHUNK_SAMPLES: usize = 1 << 20;

#[test]
#[ignore = "needs a large real-capture IQ fixture via METEOR_LRPT_IQ_FIXTURE"]
fn meteor_m2_catalog_profile_decodes_a_real_pass() {
    let path = std::env::var(FIXTURE_ENV).unwrap_or_else(|_| {
        panic!(
            "{FIXTURE_ENV} is not set. Point it at a 144 ksps \
             complex<f32> LRPT `.iq` (the format `sdr-lrpt-replay` \
             reads); see the module docs and #892."
        )
    });

    // The exact profile the corrected catalog yields for METEOR-M2
    // 3 / M2-4: OQPSK modulation, differential precoding ON. If a
    // future edit flips the catalog's `lrpt_differential` back off,
    // the analogous unit test in `sdr-sat` fails first; this test
    // is the end-to-end backstop proving the profile actually
    // decodes a real signal.
    let mut demod = LrptDemod::new_with_mode(LrptMode::Oqpsk).expect("OQPSK demod constructs");
    let mut pipeline = LrptPipeline::new_with_differential(true);

    let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("cannot open {path}: {e}"));
    let mut reader = BufReader::new(file);
    let mut byte_buf = vec![0u8; CHUNK_SAMPLES * IQ_SAMPLE_BYTES];

    loop {
        let mut filled = 0;
        // Fill a whole-sample-aligned chunk.
        while filled < byte_buf.len() {
            match reader.read(&mut byte_buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => panic!("read error on {path}: {e}"),
            }
        }
        let aligned = (filled / IQ_SAMPLE_BYTES) * IQ_SAMPLE_BYTES;
        if aligned == 0 {
            break;
        }
        // Interleaved f32 (re, im) == `[Complex]` bit-for-bit; cast
        // in place, no per-sample copy (same as `sdr-lrpt-replay`).
        let samples: &[Complex] = bytemuck::cast_slice(&byte_buf[..aligned]);
        for &sample in samples {
            if let Some(soft) = demod.process(sample) {
                pipeline.push_symbol(soft);
            }
        }
        if filled < byte_buf.len() {
            break; // short read == EOF
        }
    }

    let stats = pipeline.fec_stats();
    let lines: usize = pipeline
        .assembler()
        .channels()
        .map(|(_, ch)| ch.lines)
        .sum();

    assert!(
        stats.cadus_decoded > 0,
        "OQPSK+differential decoded zero CADUs from {path} \
         (rotation_locks={}, cadus_failed={}) — the METEOR-M2 \
         catalog profile no longer decodes a known-good pass (#892)",
        stats.rotation_locks,
        stats.cadus_failed,
    );
    assert!(
        lines > 0,
        "decoded {} CADUs but assembled zero image lines from {path}",
        stats.cadus_decoded,
    );
}
