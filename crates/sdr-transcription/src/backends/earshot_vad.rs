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
//! those scores to group voiced frames into segments:
//!
//! - **`Idle`** — no speech in flight. Each incoming frame is still
//!   retained in a short (`lookback_frames`) pre-trigger ring so the
//!   leading consonant of the next utterance isn't clipped — earshot's
//!   feature extractor needs a few frames of context before its
//!   scores cross threshold. When a frame at or above threshold
//!   arrives, the state machine transitions to `Speaking` and drains
//!   the ring into the new segment before adding the triggering
//!   frame.
//! - **`Speaking`** — at least one voiced frame has arrived. Every
//!   incoming frame (voiced or not) is appended to the current
//!   segment. A voiced frame increments the voice-frame count; a
//!   non-voiced frame transitions to `HoldingOff`.
//! - **`HoldingOff`** — inside a segment but waiting to see if the
//!   last silence is a pause or a real segment end. Frames are still
//!   appended (so the decoder sees the trailing audio). A voiced
//!   frame transitions back to `Speaking`; once `min_silence_frames`
//!   consecutive non-voiced frames accumulate, the segment is
//!   finalized and pushed onto the completed queue.
//!
//! `speech_frames_in_segment` is a running *total* of voice-scored
//! frames across the whole segment (not just consecutive ones) —
//! brief pauses that bounce through `HoldingOff → Speaking` still
//! contribute to the running count. Lookback frames are pre-trigger
//! audio and do NOT count toward it, so the min-speech-length gate
//! measures real voiced content and isn't inflated by pre-roll.
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

/// Pre-trigger lookback ring size in frames. When the state machine
/// transitions `Idle → Speaking` the last N frames of audio from the
/// ring are prepended to the segment so the first word isn't chopped.
///
/// Why lookback is necessary: `earshot::Detector` uses a 3-frame
/// context window internally, so the first frame of speech after a
/// long silence often scores below threshold (no recent history to
/// compare against). Without a lookback buffer the state machine
/// transitions on frame 2 or 3 and frame 1 — the actual start of
/// speech — is lost. 4 frames × 16 ms = 64 ms, which covers the
/// context gap plus the first consonant/vowel attack.
const DEFAULT_LOOKBACK_FRAMES: usize = 4;

/// State of the segmentation machine — see module docstring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No segment in progress. Incoming frames are retained in
    /// `lookback` (the pre-trigger ring, capped at `lookback_frames`
    /// × `FRAME_SIZE` samples) so the first voiced frame can pull
    /// its leading context along when it transitions to `Speaking`.
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
    lookback_frames: usize,

    /// Leftover samples from the last `accept` call that didn't fill
    /// a full 256-sample frame. Prepended to the next batch so no
    /// audio is dropped at batch boundaries.
    partial_frame: Vec<f32>,

    /// Pre-trigger ring buffer of raw audio samples — holds the most
    /// recent `lookback_frames` frames of audio at all times while in
    /// `Idle`. On `Idle → Speaking` transition the entire ring is
    /// drained into `current_segment` so the leading consonant and
    /// attack of the first word are preserved (earshot needs ~3
    /// frames of history to score confidently, so the first 1–2
    /// frames of real speech don't cross the threshold and would
    /// otherwise be discarded).
    ///
    /// Implemented as a `VecDeque` so the hot-path "drop the oldest
    /// frame when full" operation is O(k) head-advancement rather
    /// than an O(n) `memmove` over the ring — every additional
    /// silence frame during a long idle period used to shift the
    /// whole buffer down with `Vec::drain(..excess)`.
    lookback: VecDeque<f32>,

    /// Samples accumulated into the current in-flight segment. Grows
    /// while in `Speaking`/`HoldingOff`, drained into `completed` on
    /// finalize.
    current_segment: Vec<f32>,

    /// Running total of voice-scored frames across the current
    /// segment — NOT consecutive. Brief pauses that bounce through
    /// `HoldingOff → Speaking` keep contributing to this count, and
    /// lookback (pre-trigger) frames are intentionally excluded so
    /// the min-speech-length gate only sees real voiced content.
    /// Drives the `min_speech_frames` discard decision in
    /// `finalize_segment`.
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
            lookback_frames: DEFAULT_LOOKBACK_FRAMES,
            partial_frame: Vec::with_capacity(FRAME_SIZE),
            current_segment: Vec::new(),
            speech_frames_in_segment: 0,
            silence_frames_in_holdoff: 0,
            lookback: VecDeque::with_capacity(DEFAULT_LOOKBACK_FRAMES * FRAME_SIZE),
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
                    // Transition to Speaking. Drain the lookback ring
                    // into the segment so the leading consonant and
                    // attack of the first word — which earshot scored
                    // below threshold for lack of context — get
                    // captured. `drain(..)` empties the source so the
                    // ring starts fresh for any future Idle phase.
                    self.state = State::Speaking;
                    self.current_segment.clear();
                    self.speech_frames_in_segment = 0;
                    self.silence_frames_in_holdoff = 0;
                    self.current_segment.extend(self.lookback.drain(..));
                    self.current_segment.extend_from_slice(frame);
                    // Only the current frame scored as voiced — the
                    // lookback frames are pre-trigger audio and
                    // shouldn't inflate the min-speech-length gate.
                    self.speech_frames_in_segment += 1;
                } else {
                    // Idle + silence: append to lookback ring, cap at
                    // `lookback_frames` × `FRAME_SIZE` samples. Using
                    // a `VecDeque` and `pop_front` per excess sample
                    // keeps this O(excess) with no `memmove` — the
                    // old `Vec::drain(..excess)` path did an O(n)
                    // shift per frame which was wasteful during long
                    // idle periods.
                    self.lookback.extend(frame.iter().copied());
                    let cap = self.lookback_frames * FRAME_SIZE;
                    while self.lookback.len() > cap {
                        self.lookback.pop_front();
                    }
                }
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
        // still pop it. If we're mid-segment, the tail of audio
        // sitting in `partial_frame` (< FRAME_SIZE samples, so it
        // never scored through earshot) is still real audio and
        // belongs on the end of the segment — otherwise the last
        // <16 ms of every session disconnect is silently truncated.
        // If we're Idle, there's no segment to attach it to, so drop.
        if matches!(self.state, State::Idle) {
            self.partial_frame.clear();
            return;
        }
        if !self.partial_frame.is_empty() {
            self.current_segment.extend_from_slice(&self.partial_frame);
            self.partial_frame.clear();
        }
        self.finalize_segment();
    }

    fn reset(&mut self) {
        self.detector.reset();
        self.state = State::Idle;
        self.partial_frame.clear();
        self.current_segment.clear();
        self.speech_frames_in_segment = 0;
        self.silence_frames_in_holdoff = 0;
        self.lookback.clear();
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
        assert!(vad.lookback.is_empty());
        assert!(vad.completed.is_empty());
    }

    #[test]
    fn lookback_caps_at_configured_frames_while_idle() {
        // Feed enough silence frames to overflow the lookback ring —
        // the ring should cap at exactly `lookback_frames * FRAME_SIZE`
        // samples, not grow unboundedly.
        let mut vad = EarshotVad::new();
        vad.accept(&silence_frames(DEFAULT_LOOKBACK_FRAMES * 3));
        assert_eq!(vad.state, State::Idle, "silence should not transition");
        assert_eq!(
            vad.lookback.len(),
            DEFAULT_LOOKBACK_FRAMES * FRAME_SIZE,
            "lookback ring should hold exactly `lookback_frames` frames"
        );
    }

    #[test]
    fn lookback_prepends_to_segment_on_transition() {
        // Drive the state machine directly so we don't depend on
        // earshot's scoring. Pre-seed the lookback ring with a known
        // pattern (full of 0.7), then manually trigger an Idle →
        // Speaking transition with a current frame pattern of 0.3
        // via `process_frame`-equivalent logic.
        //
        // Testing the process_frame path would require earshot to
        // actually score above threshold, which isn't deterministic
        // for synthetic signals. So we test the field-level logic
        // that the transition branch performs: drain lookback, push
        // current frame, set counters.
        let mut vad = EarshotVad::new();

        // Seed lookback with 3 frames of a distinctive value.
        let lookback_pattern = 0.7_f32;
        vad.lookback
            .extend(std::iter::repeat_n(lookback_pattern, 3 * FRAME_SIZE));
        assert_eq!(vad.lookback.len(), 3 * FRAME_SIZE);

        // Simulate the Idle → Speaking branch from process_frame.
        let current_frame: Vec<f32> = vec![0.3_f32; FRAME_SIZE];
        vad.state = State::Speaking;
        vad.current_segment.clear();
        vad.speech_frames_in_segment = 0;
        vad.silence_frames_in_holdoff = 0;
        vad.current_segment.extend(vad.lookback.drain(..));
        vad.current_segment.extend_from_slice(&current_frame);
        vad.speech_frames_in_segment += 1;

        // Lookback must be fully drained.
        assert!(
            vad.lookback.is_empty(),
            "lookback should be empty after drain"
        );
        // Segment must hold lookback + current frame, in order.
        assert_eq!(vad.current_segment.len(), 4 * FRAME_SIZE);
        // First 3 frames should be the lookback pattern.
        for &s in &vad.current_segment[..3 * FRAME_SIZE] {
            assert!(
                (s - lookback_pattern).abs() < f32::EPSILON,
                "lookback prefix should preserve pattern, got {s}"
            );
        }
        // Last frame should be the current frame pattern.
        for &s in &vad.current_segment[3 * FRAME_SIZE..] {
            assert!(
                (s - 0.3_f32).abs() < f32::EPSILON,
                "trailing frame should be current-frame pattern, got {s}"
            );
        }
        // Only one frame counts as voiced — the lookback frames are
        // pre-trigger audio, not voice-scored content.
        assert_eq!(vad.speech_frames_in_segment, 1);
    }

    #[test]
    fn short_burst_is_discarded_as_noise() {
        // Regression for the `min_speech_frames` discard gate.
        // Drives the state machine directly so we can guarantee the
        // discard path is exercised — feeding synthetic voice through
        // `accept()` is ambiguous because earshot might never score
        // the burst above threshold (its 3-frame context window is
        // unreliable for 1-frame inputs) in which case the VAD would
        // stay in `Idle` and `pop_segment()` would return `None` for
        // the wrong reason, silently green-lighting a regression.
        //
        // Instead: manually seed the VAD into `Speaking` with a
        // sub-gate voiced-frame count, then let `accept()` drive it
        // through to finalization via trailing silence.
        let mut vad = EarshotVad::new();
        vad.state = State::Speaking;
        let voiced_count = DEFAULT_MIN_SPEECH_FRAMES - 1;
        vad.speech_frames_in_segment = voiced_count;
        vad.current_segment
            .extend(std::iter::repeat_n(0.1_f32, voiced_count * FRAME_SIZE));

        // Before the trailing silence: state is Speaking, segment has
        // real audio, voice-frame count is sub-gate. This is the
        // precondition that makes the assertion meaningful.
        assert_eq!(vad.state, State::Speaking);
        assert_eq!(vad.speech_frames_in_segment, voiced_count);
        assert!(!vad.current_segment.is_empty());

        // Feed enough silence to drive HoldingOff → finalize.
        vad.accept(&silence_frames(DEFAULT_MIN_SILENCE_FRAMES + 5));
        vad.flush();

        // Segment discarded by the min_speech_frames gate: nothing
        // popped, state back to Idle, counters cleared.
        assert!(
            vad.pop_segment().is_none(),
            "sub-gate segment should be discarded, not emitted"
        );
        assert_eq!(vad.state, State::Idle);
        assert_eq!(vad.speech_frames_in_segment, 0);
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
    fn max_speech_safety_cap_flushes_during_speaking() {
        // Regression for the max-speech safety flush branch in
        // `process_frame`. Without this, a long continuous
        // transmission (e.g. a stuck mic or a siren) would buffer
        // forever and eventually OOM — the 20-second cap exists
        // specifically to split the segment and hand it to the
        // decoder before memory grows unbounded.
        //
        // Strategy: seed the VAD into `Speaking` with a
        // `current_segment` already at exactly the cap and a
        // speech-frame count well above the min-speech gate (so the
        // segment is emitted on finalize rather than discarded).
        // Drive one more frame through `process_frame`. After the
        // match arm appends the frame, `current_segment.len()` is
        // over the cap, so the post-match safety check must detect
        // it and call `finalize_segment`.
        //
        // Use a silent frame so the state machine transitions
        // `Speaking → HoldingOff` inside the match (rather than
        // staying Speaking or jumping to a completely different
        // branch). The silence counter increments to 1, well below
        // `min_silence_frames`, so the HoldingOff branch doesn't
        // finalize on its own — the cap check is the only reason
        // finalize fires, which is exactly what this test pins.
        let mut vad = EarshotVad::new();
        vad.state = State::Speaking;
        vad.speech_frames_in_segment = DEFAULT_MIN_SPEECH_FRAMES + 10;
        let cap_samples = DEFAULT_MAX_SPEECH_FRAMES * FRAME_SIZE;
        vad.current_segment
            .extend(std::iter::repeat_n(0.1_f32, cap_samples));

        // Preconditions: segment is exactly at the cap, state is
        // Speaking, speech-frame count is above the gate, no
        // silence has been seen yet.
        assert_eq!(vad.current_segment.len(), cap_samples);
        assert_eq!(vad.state, State::Speaking);
        assert!(vad.speech_frames_in_segment > DEFAULT_MIN_SPEECH_FRAMES);
        assert_eq!(vad.silence_frames_in_holdoff, 0);

        // Process one silent frame — pushes the segment one frame
        // over the cap, which the post-match safety check must
        // detect and flush.
        let silent = vec![0.0_f32; FRAME_SIZE];
        vad.process_frame(&silent);

        // Post-conditions: segment emitted, state back to Idle,
        // counters cleared, buffer empty.
        assert_eq!(
            vad.completed.len(),
            1,
            "max-speech cap breach should emit the segment"
        );
        assert_eq!(vad.state, State::Idle, "finalize_segment → Idle");
        assert_eq!(vad.speech_frames_in_segment, 0);
        assert_eq!(vad.silence_frames_in_holdoff, 0);
        assert!(vad.current_segment.is_empty());
    }

    #[test]
    fn flush_preserves_partial_frame_tail_on_active_segment() {
        // Regression for the pre-fix truncation bug: if the session
        // ends mid-segment with a partial frame (< FRAME_SIZE samples)
        // of trailing audio stashed, flush() must append it to the
        // current segment before finalizing — otherwise the last
        // <16 ms of every active utterance is silently dropped at
        // disconnect.
        let mut vad = EarshotVad::new();
        vad.state = State::Speaking;
        vad.speech_frames_in_segment = DEFAULT_MIN_SPEECH_FRAMES + 1;
        vad.current_segment
            .extend_from_slice(&vec![0.2_f32; 16 * FRAME_SIZE]);
        // Stash a partial frame of distinctive samples.
        let tail_pattern = 0.42_f32;
        let tail_len = FRAME_SIZE / 2;
        vad.partial_frame
            .extend(std::iter::repeat_n(tail_pattern, tail_len));

        let before_len = vad.current_segment.len();
        vad.flush();

        // Segment should be emitted.
        assert_eq!(vad.completed.len(), 1);
        let segment = vad
            .completed
            .pop_front()
            .expect("completed queue should have one segment");
        // Segment length includes the tail.
        assert_eq!(segment.len(), before_len + tail_len);
        // Final `tail_len` samples should be the tail pattern.
        for &s in &segment[before_len..] {
            assert!(
                (s - tail_pattern).abs() < f32::EPSILON,
                "segment tail should preserve partial_frame pattern, got {s}"
            );
        }
        // Partial frame should have been consumed.
        assert!(vad.partial_frame.is_empty());
    }

    #[test]
    fn flush_drops_partial_frame_when_idle() {
        // If we're Idle at flush time, there's no segment to attach
        // the partial tail to — dropping it is the right behavior.
        let mut vad = EarshotVad::new();
        vad.partial_frame
            .extend(std::iter::repeat_n(0.42_f32, FRAME_SIZE / 2));
        vad.flush();
        assert!(vad.partial_frame.is_empty());
        assert!(vad.completed.is_empty());
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
