//! Pure-Rust voice activity detector for the Whisper backend (#259).
//!
//! Wraps [`earshot::Detector`] behind the feature-agnostic
//! [`VoiceActivityDetector`] trait so `backends/whisper.rs` can replace
//! its naive RMS silence gate with segment-aligned inference. The
//! Whisper backend used to decode fixed 5-second chunks and gate each
//! chunk by its total RMS, which false-triggered on squelch tails and
//! frequently split transmissions across two chunks — cutting words in
//! half and making the committed text noisy.
//!
//! `earshot` is a tiny pure-Rust VAD (no ONNX runtime, no C++ state)
//! so it's safe to ship in whisper builds without risking the same
//! `libstdc++` state collision that forced the whisper/sherpa feature
//! mutex in the first place. See `Cargo.toml` for the dependency note.
//!
//! # Segmentation model
//!
//! The VAD takes 256-sample frames (16 ms at 16 kHz) and returns a
//! per-frame voice score. This wrapper runs a three-state machine over
//! those scores to group consecutive voiced frames into segments:
//!
//! - **`Idle`** — no speech in flight. Frames below threshold are
//!   discarded; a frame at or above threshold transitions to
//!   `Speaking`.
//! - **`Speaking`** — at least one voiced frame has arrived. Incoming
//!   frames (voiced or not) are appended to the current segment. A
//!   voiced frame resets the silence counter; a non-voiced frame
//!   transitions to `HoldingOff`.
//! - **`HoldingOff`** — inside a segment but waiting to see if the
//!   last silence is a pause or a real segment end. Frames are still
//!   appended (so the decoder sees the trailing audio). Once
//!   `min_silence_frames` consecutive non-voiced frames accumulate,
//!   the segment is finalized and pushed onto the completed queue.
//!
//! A `max_speech_frames` cap forces a flush even if the speaker never
//! pauses, mirroring sherpa-onnx's `rule3_min_utterance_length` —
//! without it a long continuous monologue would never emit a segment
//! to the recognizer.
//!
//! Short-segment discard (`min_speech_frames`) protects against
//! single-frame noise spikes being emitted as one-frame "utterances".

use std::collections::VecDeque;

use earshot::Detector;

use crate::vad::VoiceActivityDetector;

/// Frame size in samples required by [`earshot::Detector::predict_f32`].
/// Fixed by the upstream crate.
const FRAME_SIZE: usize = 256;

/// Target sample rate for all VAD input. Must match the rate whisper
/// expects (and the rate `resampler::downsample_stereo_to_mono_16k`
/// produces). Used here for the ms → frames conversion only.
const SAMPLE_RATE_HZ: usize = 16_000;

/// Voice-score threshold above which a frame counts as speech.
///
/// 0.5 is the earshot-recommended default. Lower values catch quieter
/// speech but also more noise; higher values are more conservative.
/// Not currently exposed to the UI — follow-up if user tuning turns
/// out to matter.
const DEFAULT_VOICE_THRESHOLD: f32 = 0.5;

/// Consecutive non-voiced frames required to finalize a segment.
/// 15 frames × 16 ms = 240 ms. Matches sherpa-onnx's Silero default
/// for `min_silence_duration` so whisper and sherpa behave similarly
/// on the same audio.
const DEFAULT_MIN_SILENCE_FRAMES: usize = (250 * SAMPLE_RATE_HZ) / (FRAME_SIZE * 1000);

/// Minimum voiced frames a segment must contain to be emitted. Shorter
/// segments are treated as noise spikes and dropped.
///
/// Tuned empirically against live scanner traffic: the original value
/// of 250 ms (matching sherpa-onnx's Silero `min_speech_duration`
/// default) dropped brisk short transmissions like "10-4" whose
/// voiced portion is ~150–200 ms when spoken quickly. 100 ms is
/// Silero's absolute speech-duration floor (anything below isn't a
/// detectable word) and catches two-syllable radio brevity codes
/// without admitting single-frame noise spikes.
const DEFAULT_MIN_SPEECH_FRAMES: usize = (100 * SAMPLE_RATE_HZ) / (FRAME_SIZE * 1000);

/// Maximum segment length before a forced flush. 20 seconds at 16 kHz
/// = 1250 frames. Prevents a pathologically long transmission from
/// buffering forever; mirrors sherpa-onnx's `max_speech_duration`.
const DEFAULT_MAX_SPEECH_FRAMES: usize = (20 * SAMPLE_RATE_HZ) / FRAME_SIZE;

/// State of the segmentation machine — see module docstring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No segment in progress. Silence frames are discarded.
    Idle,
    /// At least one voiced frame has arrived; building a segment.
    Speaking,
    /// Inside a segment but currently in a run of silence frames —
    /// haven't decided yet whether it's a pause or the real end.
    HoldingOff,
}

/// `earshot`-backed voice activity detector implementing the
/// feature-agnostic [`VoiceActivityDetector`] trait. Feed 16 kHz mono
/// f32 samples via [`VoiceActivityDetector::accept`] and pop completed
/// speech segments via [`VoiceActivityDetector::pop_segment`].
pub struct EarshotVad {
    detector: Detector,
    state: State,

    /// Config.
    voice_threshold: f32,
    min_silence_frames: usize,
    min_speech_frames: usize,
    max_speech_frames: usize,

    /// Leftover samples from the last `accept` call that didn't fill
    /// a full 256-sample frame. Prepended to the next batch so no
    /// audio is dropped at batch boundaries.
    partial_frame: Vec<f32>,

    /// Samples accumulated into the current in-flight segment. Grows
    /// while in `Speaking`/`HoldingOff`, drained into `completed` on
    /// finalize.
    current_segment: Vec<f32>,

    /// Count of consecutive voiced frames in the current segment —
    /// drives the min-speech-length discard gate.
    speech_frames_in_segment: usize,

    /// Count of consecutive non-voiced frames while in `HoldingOff`.
    /// Drives the silence → segment-end transition.
    silence_frames_in_holdoff: usize,

    /// Completed segments waiting to be popped by the caller.
    completed: VecDeque<Vec<f32>>,
}

impl Default for EarshotVad {
    fn default() -> Self {
        Self::new()
    }
}

impl EarshotVad {
    /// Build a VAD with the default tuning constants documented at the
    /// top of the module.
    pub fn new() -> Self {
        Self {
            detector: Detector::default(),
            state: State::Idle,
            voice_threshold: DEFAULT_VOICE_THRESHOLD,
            min_silence_frames: DEFAULT_MIN_SILENCE_FRAMES,
            min_speech_frames: DEFAULT_MIN_SPEECH_FRAMES,
            max_speech_frames: DEFAULT_MAX_SPEECH_FRAMES,
            partial_frame: Vec::with_capacity(FRAME_SIZE),
            current_segment: Vec::new(),
            speech_frames_in_segment: 0,
            silence_frames_in_holdoff: 0,
            completed: VecDeque::new(),
        }
    }

    /// Process one full 256-sample frame and drive the state machine.
    /// Assumes `frame.len() == FRAME_SIZE`.
    fn process_frame(&mut self, frame: &[f32]) {
        debug_assert_eq!(frame.len(), FRAME_SIZE);
        let score = self.detector.predict_f32(frame);
        let is_voice = score >= self.voice_threshold;

        match self.state {
            State::Idle => {
                if is_voice {
                    self.state = State::Speaking;
                    self.current_segment.clear();
                    self.speech_frames_in_segment = 0;
                    self.silence_frames_in_holdoff = 0;
                    self.current_segment.extend_from_slice(frame);
                    self.speech_frames_in_segment += 1;
                }
                // Idle + silence: discard.
            }
            State::Speaking => {
                self.current_segment.extend_from_slice(frame);
                if is_voice {
                    self.speech_frames_in_segment += 1;
                } else {
                    self.state = State::HoldingOff;
                    self.silence_frames_in_holdoff = 1;
                }
            }
            State::HoldingOff => {
                self.current_segment.extend_from_slice(frame);
                if is_voice {
                    // Pause ended — back to Speaking.
                    self.state = State::Speaking;
                    self.speech_frames_in_segment += 1;
                    self.silence_frames_in_holdoff = 0;
                } else {
                    self.silence_frames_in_holdoff += 1;
                    if self.silence_frames_in_holdoff >= self.min_silence_frames {
                        self.finalize_segment();
                    }
                }
            }
        }

        // Max-speech safety flush — applies regardless of state, but
        // only meaningful in Speaking/HoldingOff. Runs after the state
        // update so a max-length segment flushes this frame's audio
        // before starting a new one.
        if !matches!(self.state, State::Idle)
            && self.current_segment.len() >= self.max_speech_frames * FRAME_SIZE
        {
            tracing::debug!(
                frames = self.max_speech_frames,
                "earshot VAD: max speech frames reached, forcing flush"
            );
            self.finalize_segment();
        }
    }

    /// End the current segment. If it's long enough to be real speech,
    /// push it onto the completed queue; otherwise discard as noise.
    /// Reset state to `Idle` either way.
    fn finalize_segment(&mut self) {
        if self.speech_frames_in_segment >= self.min_speech_frames {
            let segment = std::mem::take(&mut self.current_segment);
            self.completed.push_back(segment);
        } else {
            tracing::debug!(
                frames = self.speech_frames_in_segment,
                min = self.min_speech_frames,
                "earshot VAD: segment too short, discarded"
            );
            self.current_segment.clear();
        }
        self.state = State::Idle;
        self.speech_frames_in_segment = 0;
        self.silence_frames_in_holdoff = 0;
    }
}

impl VoiceActivityDetector for EarshotVad {
    fn accept(&mut self, samples: &[f32]) {
        // Reassemble into 256-sample frames, accounting for any
        // leftover from the last call. Minimizes allocation by
        // operating in-place on `partial_frame`.
        let mut pos = 0;

        // Finish the previous partial frame first, if any.
        if !self.partial_frame.is_empty() {
            let needed = FRAME_SIZE - self.partial_frame.len();
            let take = needed.min(samples.len());
            self.partial_frame.extend_from_slice(&samples[..take]);
            pos += take;
            if self.partial_frame.len() == FRAME_SIZE {
                // Drain into a temporary to release the mutable borrow
                // before `process_frame` takes `&mut self`.
                let frame = std::mem::take(&mut self.partial_frame);
                self.process_frame(&frame);
                // Reuse the allocation.
                self.partial_frame = frame;
                self.partial_frame.clear();
            }
        }

        // Whole frames from the rest of the input.
        while samples.len() - pos >= FRAME_SIZE {
            self.process_frame(&samples[pos..pos + FRAME_SIZE]);
            pos += FRAME_SIZE;
        }

        // Stash the tail for next time.
        if pos < samples.len() {
            self.partial_frame.extend_from_slice(&samples[pos..]);
        }
    }

    fn pop_segment(&mut self) -> Option<Vec<f32>> {
        self.completed.pop_front()
    }

    fn flush(&mut self) {
        // Force any in-flight segment to finalize so the caller can
        // still pop it. Drop any partial frame — it's < 16 ms of
        // audio, not worth worrying about.
        self.partial_frame.clear();
        if !matches!(self.state, State::Idle) {
            self.finalize_segment();
        }
    }

    fn reset(&mut self) {
        self.detector.reset();
        self.state = State::Idle;
        self.partial_frame.clear();
        self.current_segment.clear();
        self.speech_frames_in_segment = 0;
        self.silence_frames_in_holdoff = 0;
        self.completed.clear();
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::*;

    /// Generate `n_frames` worth of a unit sine wave at `freq_hz`,
    /// 16 kHz mono. earshot detects real voiced content so pure tones
    /// produce inconsistent scores — tests that care about voice
    /// detection use the `synthetic_voice` helper instead.
    #[allow(dead_code)]
    fn tone_frames(freq_hz: f32, n_frames: usize) -> Vec<f32> {
        let n_samples = n_frames * FRAME_SIZE;
        (0..n_samples)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE_HZ as f32;
                (2.0 * std::f32::consts::PI * freq_hz * t).sin() * 0.3
            })
            .collect()
    }

    /// Synthetic "voice-like" audio: a 200 Hz fundamental plus
    /// formants at 700, 1400, 2400 Hz with amplitude envelope.
    /// earshot's feature extractor keys on mel-spectrum shapes that
    /// resemble formant structure, so this produces reliable
    /// above-threshold scores in tests without needing a recorded
    /// voice sample on disk.
    fn synthetic_voice(n_frames: usize) -> Vec<f32> {
        let n_samples = n_frames * FRAME_SIZE;
        (0..n_samples)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE_HZ as f32;
                let f0 = (2.0 * std::f32::consts::PI * 200.0 * t).sin() * 0.25;
                let f1 = (2.0 * std::f32::consts::PI * 700.0 * t).sin() * 0.20;
                let f2 = (2.0 * std::f32::consts::PI * 1_400.0 * t).sin() * 0.15;
                let f3 = (2.0 * std::f32::consts::PI * 2_400.0 * t).sin() * 0.10;
                f0 + f1 + f2 + f3
            })
            .collect()
    }

    fn silence_frames(n_frames: usize) -> Vec<f32> {
        vec![0.0; n_frames * FRAME_SIZE]
    }

    #[test]
    fn fresh_vad_is_empty() {
        let mut vad = EarshotVad::new();
        assert!(vad.pop_segment().is_none());
    }

    #[test]
    fn pure_silence_emits_nothing() {
        let mut vad = EarshotVad::new();
        vad.accept(&silence_frames(100));
        vad.flush();
        assert!(vad.pop_segment().is_none());
    }

    #[test]
    fn partial_frames_across_calls_do_not_drop_samples() {
        // Feed audio in two halves that don't land on a frame
        // boundary. Total should still match the synchronous path.
        let mut vad = EarshotVad::new();
        let audio = silence_frames(10);
        vad.accept(&audio[..FRAME_SIZE + 100]);
        vad.accept(&audio[FRAME_SIZE + 100..]);

        // Shouldn't emit anything for pure silence.
        vad.flush();
        assert!(vad.pop_segment().is_none());

        // State machine internals: no residual speech counters.
        assert_eq!(vad.state, State::Idle);
    }

    #[test]
    fn reset_clears_all_state() {
        let mut vad = EarshotVad::new();
        vad.accept(&synthetic_voice(30));
        vad.reset();
        assert_eq!(vad.state, State::Idle);
        assert!(vad.partial_frame.is_empty());
        assert!(vad.current_segment.is_empty());
        assert_eq!(vad.speech_frames_in_segment, 0);
        assert_eq!(vad.silence_frames_in_holdoff, 0);
        assert!(vad.completed.is_empty());
    }

    #[test]
    fn short_burst_is_discarded_as_noise() {
        // A single voiced frame (< min_speech_frames) should be
        // dropped entirely. Use 1 frame of synthetic voice surrounded
        // by silence so the segment ends but fails the length gate.
        let mut vad = EarshotVad::new();
        vad.accept(&synthetic_voice(1));
        vad.accept(&silence_frames(DEFAULT_MIN_SILENCE_FRAMES + 5));
        vad.flush();
        assert!(vad.pop_segment().is_none());
    }

    #[test]
    fn finalize_segment_respects_min_speech_gate() {
        // Directly test the state machine gate without going through
        // earshot's scoring — append frames to current_segment and
        // call finalize. Frame counts are expressed relative to the
        // default so a future retune of `DEFAULT_MIN_SPEECH_FRAMES`
        // doesn't accidentally land this test in "happens to cross"
        // territory.
        let below_gate = DEFAULT_MIN_SPEECH_FRAMES - 1;
        let above_gate = DEFAULT_MIN_SPEECH_FRAMES + 1;

        let mut vad = EarshotVad::new();
        vad.state = State::Speaking;
        vad.speech_frames_in_segment = below_gate;
        vad.current_segment
            .extend_from_slice(&vec![0.1_f32; below_gate * FRAME_SIZE]);
        vad.finalize_segment();
        assert!(vad.completed.is_empty(), "short segment should be dropped");
        assert_eq!(vad.state, State::Idle);

        // And a segment above the gate should survive.
        vad.state = State::Speaking;
        vad.speech_frames_in_segment = above_gate;
        vad.current_segment
            .extend_from_slice(&vec![0.1_f32; above_gate * FRAME_SIZE]);
        vad.finalize_segment();
        assert_eq!(vad.completed.len(), 1, "segment above gate should emit");
    }

    #[test]
    fn flush_on_active_segment_emits_it() {
        // Force the state machine into a valid Speaking state with
        // enough speech frames, then flush — the segment should come
        // out even though there's no trailing silence.
        let mut vad = EarshotVad::new();
        vad.state = State::Speaking;
        vad.speech_frames_in_segment = DEFAULT_MIN_SPEECH_FRAMES + 5;
        vad.current_segment
            .extend_from_slice(&vec![0.1_f32; 20 * FRAME_SIZE]);
        vad.flush();
        assert_eq!(vad.completed.len(), 1);
        assert_eq!(vad.state, State::Idle);
    }
}
