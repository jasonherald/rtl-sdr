//! Feature-agnostic voice activity detection trait.
//!
//! This module is deliberately NOT gated on any backend cargo feature —
//! the trait compiles in whisper builds, sherpa builds, and any future
//! combination. The sherpa-onnx-backed impl lives in
//! `backends/sherpa/silero_vad.rs` behind `#[cfg(feature = "sherpa")]`,
//! and a Whisper impl (pure-Rust Silero) is a follow-up PR.
//!
//! Callers feed 16 kHz mono samples via [`VoiceActivityDetector::accept`]
//! and poll [`VoiceActivityDetector::pop_segment`] to pull completed
//! speech segments. Segments are owned `Vec<f32>` so the caller can
//! move them into a batch decoder without re-allocating.

/// Queue-based voice activity detector.
///
/// The implementation is expected to buffer input internally and emit
/// one segment per detected utterance. Segments contain only voiced
/// frames; silence is already trimmed by the detector.
pub trait VoiceActivityDetector {
    /// Feed 16 kHz mono samples. The detector buffers internally and
    /// may emit one or more completed segments after this call.
    fn accept(&mut self, samples: &[f32]);

    /// Pop the next completed speech segment if one is ready.
    /// Returns `None` when the internal queue is empty.
    fn pop_segment(&mut self) -> Option<Vec<f32>>;

    /// Force the detector to finalize any in-flight speech segment so
    /// the next [`Self::pop_segment`] call can return it. Called at
    /// session end to prevent the last utterance being dropped if the
    /// user stops transcription mid-speech.
    fn flush(&mut self);

    /// Drop all buffered audio and reset detector state.
    /// Called between transcription sessions.
    fn reset(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check that the trait is object-safe so it can be
    /// used as `Box<dyn VoiceActivityDetector>` or `&dyn VoiceActivityDetector`.
    /// If someone adds a generic method that breaks object safety this
    /// test will fail to compile.
    #[test]
    fn trait_is_object_safe() {
        fn takes_dyn(_: &mut dyn VoiceActivityDetector) {}
        struct Noop;
        impl VoiceActivityDetector for Noop {
            fn accept(&mut self, _: &[f32]) {}
            fn pop_segment(&mut self) -> Option<Vec<f32>> {
                None
            }
            fn flush(&mut self) {}
            fn reset(&mut self) {}
        }
        let mut noop = Noop;
        takes_dyn(&mut noop);
    }
}
