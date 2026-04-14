//! Feature-agnostic voice activity detection trait.
//!
//! This module is deliberately NOT gated on any backend cargo feature —
//! the trait compiles in whisper builds, sherpa builds, and any future
//! combination. The sherpa-onnx-backed impl lives in
//! `backends/sherpa/silero_vad.rs` behind `#[cfg(feature = "sherpa")]`,
//! and an earshot-backed pure-Rust impl lives in
//! `backends/earshot_vad.rs` behind `#[cfg(feature = "whisper")]`.
//!
//! Callers feed 16 kHz mono samples via [`VoiceActivityDetector::accept`]
//! and poll [`VoiceActivityDetector::pop_segment`] to pull completed
//! speech segments. Segments are owned `Vec<f32>` so the caller can
//! move them into a batch decoder without re-allocating.

/// Queue-based voice activity detector.
///
/// The implementation buffers input internally and emits one segment
/// per detected utterance. Each segment is a **bounded utterance** —
/// the detector decides where the utterance starts and ends and
/// returns audio that covers it. Segments are NOT required to be
/// pure voiced samples with all silence trimmed away; implementations
/// typically include a small amount of leading and trailing context
/// (pre-trigger "lookback" and post-trigger "holdoff" padding) so
/// downstream recognizers see consonant attacks and release tails
/// instead of truncated words.
///
/// Concrete impls documented for the two current backends:
///
/// - `SherpaSileroVad` (sherpa feature): segments come from
///   sherpa-onnx's `VoiceActivityDetector` which applies Silero's
///   internal `speech_pad_ms` (~30 ms) to both ends of each utterance.
/// - `EarshotVad` (whisper feature): segments include a 64 ms
///   pre-trigger lookback ring at the start and any `HoldingOff`
///   trailing-silence frames that fell inside the segment before the
///   `min_silence_frames` gate fired. See
///   `crates/sdr-transcription/src/backends/earshot_vad.rs` for
///   tuning details.
///
/// The common invariant: a popped segment is a complete utterance
/// boundable by silence, handed off in one piece. Callers don't need
/// to do further chunking.
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
