//! Real-data gate: real recorded HF static must NOT drive the sync-gated
//! WEFAX decoder into imaging. Before the #913 no-static hardening the
//! phasing detector locked onto broadband noise within ~2 s and painted a
//! full page of static; this proves it no longer does, against a real
//! recorded static clip (not a synthetic one).
use sdr_dsp::wefax::{WefaxDecoder, WefaxLine, WefaxState};

fn read_mono_f32(path: &str) -> (u32, Vec<f32>) {
    let mut r = hound::WavReader::open(path).expect("open wav");
    let sr = r.spec().sample_rate;
    let s = r.samples::<f32>().map(|x| x.expect("sample")).collect();
    (sr, s)
}

#[test]
fn real_static_never_locks_the_gated_decoder() {
    let (sr, samples) = read_mono_f32("tests/data/wefax_static_12k.wav");
    let mut dec = WefaxDecoder::new(sr).expect("build decoder");
    let mut out = vec![WefaxLine::default(); 64];

    let mut lines_emitted = 0usize;
    let mut ever_imaging = false;
    for chunk in samples.chunks(4096) {
        lines_emitted += dec.process(chunk, &mut out).expect("process");
        ever_imaging |= dec.state() == WefaxState::Imaging;
    }

    assert!(
        !ever_imaging,
        "real recorded static must never lock the decoder into Imaging"
    );
    assert_eq!(
        lines_emitted, 0,
        "real static must paint zero image lines, got {lines_emitted}"
    );
}
