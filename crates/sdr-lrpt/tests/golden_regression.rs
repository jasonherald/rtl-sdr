//! Golden-output regression test.
//!
//! Runs our full FEC + CCSDS pipeline against a committed IQ
//! recording and asserts byte-equality with the golden frame
//! outputs from a reference decoder (`MeteorDemod` / medet /
//! `SatDump`). PNG comparison uses SSIM > 0.99.
//!
//! Marked `#[ignore]` until real-pass goldens land in Task 5 —
//! the synthetic CADU fixtures alone exercise framing logic but
//! can't verify the FEC math against a live recording. The full
//! real-pass integration test runs on demand:
//!   `cargo test -p sdr-lrpt -- --ignored real_pass`
//!
//! Regeneration procedure: see
//! `crates/sdr-lrpt/tests/fixtures/REGENERATE_GOLDENS.md`.

#[test]
#[ignore = "requires committed real-pass golden + IQ; run with --ignored"]
fn frames_match_golden() {
    let golden_iq = std::path::Path::new("tests/fixtures/golden/pass.iq");
    let golden_frames = std::path::Path::new("tests/fixtures/golden/frames.bin");
    if !golden_iq.exists() || !golden_frames.exists() {
        // Regeneration procedure documented in
        // tests/fixtures/REGENERATE_GOLDENS.md; this branch
        // signals "fixtures missing" rather than failing the
        // whole suite.
        eprintln!(
            "golden fixtures missing — see crates/sdr-lrpt/tests/fixtures/REGENERATE_GOLDENS.md",
        );
        return;
    }
    // The full pipeline + golden compare wires up in Task 5
    // (image stage). For now this scaffold exists so PR 4 can
    // commit it gated alongside the framing layer.
    todo!("wire LrptPipeline + SSIM compare once Task 5 ships the image stage");
}
