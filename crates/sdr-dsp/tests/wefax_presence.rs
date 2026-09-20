//! Real-data gate for the fax-presence detector (public-domain NMF audio).
use sdr_dsp::wefax::WefaxPresenceDetector;

fn read_mono_f32(path: &str) -> (f64, Vec<f32>) {
    let mut r = hound::WavReader::open(path).expect("open");
    let sr = f64::from(r.spec().sample_rate);
    let s = r.samples::<f32>().map(|x| x.expect("s")).collect();
    (sr, s)
}

#[test]
fn nmf_fax_audio_reads_present() {
    let (sr, s) = read_mono_f32("tests/data/wefax_nmf_present_12k.wav");
    let mut det = WefaxPresenceDetector::new(sr);
    for b in s.chunks(1024) {
        det.update(b);
    }
    assert!(det.is_present(), "real NMF fax audio must read present");
}

#[test]
fn static_reads_absent() {
    // Negative control: `wefax_static_12k.wav` is a SYNTHETIC white-noise
    // clip (`sox -n synth whitenoise`, per `tests/data/README.md`), not a
    // real off-air recording. A real static fixture is deferred (#914).
    let (sr, s) = read_mono_f32("tests/data/wefax_static_12k.wav");
    let mut det = WefaxPresenceDetector::new(sr);
    for b in s.chunks(1024) {
        det.update(b);
    }
    assert!(
        !det.is_present(),
        "synthetic white-noise static must read absent"
    );
}
