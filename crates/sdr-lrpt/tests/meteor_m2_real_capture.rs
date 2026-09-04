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

    // Drive the chain from the live catalog entry — NOT hard-coded
    // — so this test guards the catalog→pipeline wiring end to end:
    // if `lrpt_differential` is ever reverted in the catalog, the
    // built chain reverts with it and this decode fails. Per
    // `CodeRabbit` round 2 on PR #894.
    let (mode, differential) = catalog_profile(sdr_sat::METEOR_M2_4_NORAD_ID);
    let (stats, lines) = decode_fixture(&path, mode, differential);

    assert!(
        stats.cadus_decoded > 0,
        "the catalog's METEOR-M2 4 profile ({mode:?}, differential={differential}) \
         decoded zero CADUs from {path} (rotation_locks={}, cadus_failed={}) — \
         the catalog no longer maps to a chain that decodes a known-good pass (#892)",
        stats.rotation_locks,
        stats.cadus_failed,
    );
    assert!(
        lines > 0,
        "decoded {} CADUs but assembled zero image lines from {path}",
        stats.cadus_decoded,
    );
}

/// Resolve a `KnownSatellite`'s LRPT profile into the concrete
/// demod + FEC settings, mirroring the live wiring's
/// `lrpt_downlink_for`. Panics if the id isn't an LRPT entry — the
/// caller passes a Meteor NORAD id.
fn catalog_profile(norad_id: u32) -> (LrptMode, bool) {
    let sat = sdr_sat::KNOWN_SATELLITES
        .iter()
        .find(|s| s.norad_id == norad_id)
        .unwrap_or_else(|| panic!("NORAD {norad_id} not in KNOWN_SATELLITES"));
    let mode = match sat.lrpt_modulation {
        Some(sdr_sat::LrptModulation::Oqpsk) => LrptMode::Oqpsk,
        Some(sdr_sat::LrptModulation::Qpsk) => LrptMode::Qpsk,
        None => panic!("NORAD {norad_id} has no LRPT modulation in the catalog"),
    };
    (mode, sat.lrpt_differential)
}

/// Stream the fixture through the given demod mode + differential
/// setting (resolved from the catalog by the caller) and return the
/// FEC stats plus total assembled image lines.
fn decode_fixture(
    path: &str,
    mode: LrptMode,
    differential: bool,
) -> (sdr_lrpt::fec::FecStats, usize) {
    let mut demod = LrptDemod::new_with_mode(mode).expect("catalog demod mode constructs");
    let mut pipeline = LrptPipeline::new_with_differential(differential);

    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("cannot open {path}: {e}"));
    let mut reader = BufReader::new(file);
    // Allocate the buffer AS `[Complex]` so it's guaranteed aligned,
    // then read raw bytes into its byte-view — reading into a
    // `Vec<u8>` and casting to `[Complex]` relies on the allocator
    // happening to return 4-byte-aligned memory, which `cast_slice`
    // would panic on if it ever didn't (Codacy round 1 on PR #894).
    let mut buf = vec![Complex::default(); CHUNK_SAMPLES];
    let chunk_bytes = CHUNK_SAMPLES * IQ_SAMPLE_BYTES;

    loop {
        // Fill the chunk's byte-view in a scope so the mutable
        // borrow ends before we read the samples back out.
        let filled = {
            let byte_view: &mut [u8] = bytemuck::cast_slice_mut(&mut buf);
            let mut filled = 0;
            while filled < byte_view.len() {
                match reader.read(&mut byte_view[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) => panic!("read error on {path}: {e}"),
                }
            }
            filled
        };
        let whole_samples = filled / IQ_SAMPLE_BYTES;
        if whole_samples == 0 {
            break;
        }
        for &sample in &buf[..whole_samples] {
            if let Some(soft) = demod.process(sample) {
                pipeline.push_symbol(soft);
            }
        }
        if filled < chunk_bytes {
            break; // short read == EOF
        }
    }

    let lines = pipeline
        .assembler()
        .channels()
        .map(|(_, ch)| ch.lines)
        .sum();
    (pipeline.fec_stats(), lines)
}
