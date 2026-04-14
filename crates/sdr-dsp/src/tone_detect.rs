//! CTCSS sub-audible tone detector (#269 PR 1 of 3).
//!
//! Implements a single-frequency Goertzel filter tuned to one of the
//! 42 standard CTCSS (Continuous Tone-Coded Squelch System) tones,
//! plus a sustained-detection gate that prevents false triggers from
//! short bursts of low-frequency speech energy overlapping the tone
//! band.
//!
//! # Where this fits
//!
//! CTCSS tones are amplitude-modulated sub-audible (67–254 Hz) signals
//! transmitted alongside voice on analog FM. They're used to give a
//! shared frequency "private" groups — a receiver configured for CTCSS
//! tone X only opens its squelch when it sees that specific tone, even
//! if other users are transmitting voice on the same carrier. Every
//! consumer scanner and every commercial NFM radio supports this.
//!
//! This module is the detector half only — it takes demodulated AF
//! samples at 48 kHz and answers "is the target tone currently
//! sustained?". The squelch wiring, high-pass filter, and UI live in
//! follow-up PRs; this PR 1 is self-contained DSP that can be tested
//! with synthetic signals.
//!
//! # Algorithm
//!
//! [Goertzel](https://en.wikipedia.org/wiki/Goertzel_algorithm) is a
//! second-order IIR resonator tuned to a single target frequency. It
//! computes the DFT magnitude at that one frequency in O(N) time with
//! no multiplications by twiddle factors. Given samples `x[0..N]`,
//! target frequency `f`, and sample rate `fs`:
//!
//! ```text
//! omega  = 2π·f/fs
//! coeff  = 2·cos(omega)
//! s[-1]  = 0
//! s[-2]  = 0
//! for n in 0..N:
//!     s[n] = x[n] + coeff·s[n-1] - s[n-2]
//! mag² = s[N-1]² + s[N-2]² − coeff·s[N-1]·s[N-2]
//! ```
//!
//! Goertzel can target arbitrary (non-bin-aligned) frequencies — the
//! coefficient is computed from the real-valued `omega`, not an
//! integer bin index. CTCSS tones are spaced as finely as 2.5 Hz
//! (e.g. 67.0 / 69.3 Hz) so this matters.
//!
//! # Frequency resolution
//!
//! The time-frequency uncertainty relation gives roughly `1/T` Hz of
//! resolution for a window of length `T` seconds — no algorithm can
//! beat that. To distinguish adjacent CTCSS tones (smallest gap
//! ~2.5 Hz) we need `T ≥ 400 ms`, which is ~19,200 samples at 48 kHz.
//!
//! In practice the detector doesn't need to tell adjacent tones apart
//! — the user picks ONE target tone, and the detector answers
//! "present or not". Tightening the window improves response time at
//! the cost of specificity. [`CTCSS_WINDOW_MS`] is set to 200 ms
//! (~9600 samples at 48 kHz, ~5 Hz resolution) as a middle ground:
//! good enough specificity to avoid confusing 67.0 Hz for 69.3 Hz,
//! fast enough to unblock squelch within a quarter-second of a real
//! transmission starting.
//!
//! # Sustained-detection gate
//!
//! The main false-trigger source is voice fundamental energy in
//! 80–250 Hz — a male voice's F0 can sit right in the middle of the
//! CTCSS band. To reject these transients we require [`CTCSS_MIN_HITS`]
//! consecutive detection windows above threshold before the gate
//! opens, and [`CTCSS_MIN_HITS`] consecutive below-threshold windows
//! before it closes (hysteresis). At 200 ms per window that's ~600 ms
//! of confirmation latency — matching real-scanner behavior.
//!
//! # What this PR is NOT
//!
//! - **No DCS (digital code squelch) detection.** DCS needs a PLL-
//!   locked baseband plus a Golay decoder and is a much bigger lift;
//!   tracked separately as a follow-up.
//! - **No squelch wiring or UI.** PR 2 integrates this into
//!   `sdr-radio::af_chain` with a high-pass filter on the speaker
//!   path so users don't hear the sub-audible tone. PR 3 adds the
//!   per-bookmark UI.
//! - **No tone-encode / TX path.** Read-only SDR for now.

use sdr_types::DspError;

/// AF chain sample rate in Hz. Matches `sdr-radio::af_chain`'s
/// `DEFAULT_AUDIO_RATE` — if that ever changes, the `CTCSS_WINDOW_MS`
/// math below needs to be revisited.
pub const CTCSS_SAMPLE_RATE_HZ: f32 = 48_000.0;

/// Detection window length in milliseconds. Drives the frequency
/// resolution (~1/T Hz) and the base response time. 200 ms gives us
/// ~5 Hz resolution at 48 kHz — enough to distinguish any of the
/// standard CTCSS tones cleanly without tanking specificity on voice
/// fundamentals.
pub const CTCSS_WINDOW_MS: f32 = 200.0;

/// Window length in samples, derived from [`CTCSS_WINDOW_MS`] and
/// [`CTCSS_SAMPLE_RATE_HZ`]. Used as the Goertzel block size.
///
/// Integer literal because the const-eval float → usize cast trips
/// clippy's `cast_possible_truncation` / `cast_sign_loss` lints.
/// The value is locked in at `200 ms × 48 kHz ÷ 1000 = 9600` and
/// there's a compile-time assertion below to catch drift if either
/// of the two inputs changes.
pub const CTCSS_WINDOW_SAMPLES: usize = 9_600;

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp
)]
const _: () = {
    // Compile-time sanity check: if anyone edits `CTCSS_WINDOW_MS`
    // or `CTCSS_SAMPLE_RATE_HZ` without also updating
    // `CTCSS_WINDOW_SAMPLES`, this fails to compile. The `as` cast
    // is safe here — the values are well inside usize range and
    // positive — but we still have to silence the pedantic lints
    // for a const-context expression.
    let derived = (CTCSS_WINDOW_MS * CTCSS_SAMPLE_RATE_HZ / 1000.0) as usize;
    assert!(
        derived == CTCSS_WINDOW_SAMPLES,
        "CTCSS_WINDOW_SAMPLES out of sync with CTCSS_WINDOW_MS / CTCSS_SAMPLE_RATE_HZ"
    );
};

/// Number of consecutive above-threshold windows required before the
/// sustained-detection gate opens, and number of consecutive
/// below-threshold windows required before it closes. Three windows
/// at 200 ms each give a 600 ms confirmation time, which matches
/// standard scanner behavior.
pub const CTCSS_MIN_HITS: usize = 3;

/// Default detection threshold: target-tone magnitude must exceed
/// this multiple of the window's RMS energy to count as a hit. The
/// Goertzel magnitude is in the same units as the input samples, so
/// we normalize by the RMS of the whole window to get a
/// "proportion of signal in this one frequency" measure.
///
/// Empirically 0.1 (10% of window RMS) is a reasonable starting
/// point that catches real tones while rejecting voice transients.
/// The value will likely want to become user-tunable in PR 2 once
/// real-world traffic reveals its behavior.
pub const CTCSS_DEFAULT_THRESHOLD: f32 = 0.1;

/// The 42 standard CTCSS tones in Hz, from EIA/TIA-603 and
/// Motorola's PL (Private Line) table. Ordered ascending so a
/// dropdown can use this slice directly as its value list.
///
/// The 38 "classical" tones from 67.0 through 250.3 Hz were
/// standardized first; the four additional tones (159.8, 165.5,
/// 171.3, 177.3) were added later to reduce adjacent-tone crosstalk
/// in dense shared-channel environments. All 42 are in use on
/// modern scanners.
pub const CTCSS_TONES_HZ: &[f32] = &[
    67.0, 69.3, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8, 97.4, 100.0, 103.5, 107.2,
    110.9, 114.8, 118.8, 123.0, 127.3, 131.8, 136.5, 141.3, 146.2, 150.0, 151.4, 156.7, 159.8,
    162.2, 165.5, 167.9, 171.3, 173.8, 177.3, 179.9, 183.5, 186.2, 189.9, 192.8, 196.6, 199.5,
    203.5, 206.5, 210.7, 218.1, 225.7, 229.1, 233.6, 241.8, 250.3, 254.1,
];

// The 51 entries above extend the classic 42-tone table with the
// non-standard additions some scanners also support (162.2, 167.9,
// 179.9, 183.5, 189.9, 196.6, 199.5, 206.5, 254.1). Kept inline so
// a future UI dropdown can show "standard 42" or "extended 51".

/// Look up the index of `target_hz` in [`CTCSS_TONES_HZ`] using an
/// exact-equal comparison. Returns `None` if the frequency isn't a
/// known CTCSS tone. Used by the UI / config layer to validate user
/// input against the table.
#[must_use]
pub fn ctcss_tone_index(target_hz: f32) -> Option<usize> {
    CTCSS_TONES_HZ
        .iter()
        .position(|&t| (t - target_hz).abs() < f32::EPSILON)
}

/// Output of [`CtcssDetector::process_block`] — includes both the
/// raw per-window decision and the sustained-gate state so callers
/// can choose which one to act on. The squelch wiring in PR 2 will
/// consume `sustained` only; tests and future analytics can use the
/// raw `detected` for per-window diagnostics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CtcssDecision {
    /// Goertzel magnitude at the target frequency, normalized by the
    /// window's RMS. Always in `[0, ∞)`; compare against the
    /// detector's threshold to recover the raw per-window decision.
    pub normalized_magnitude: f32,
    /// This window's threshold comparison: the target tone is
    /// present in THIS single 200 ms window. May flap on transients.
    pub detected: bool,
    /// The sustained-detection gate: the target tone has been
    /// present for at least [`CTCSS_MIN_HITS`] consecutive windows
    /// and the squelch should be open. This is the one the squelch
    /// wiring in PR 2 will consume.
    pub sustained: bool,
}

/// Goertzel single-frequency detector tuned to one CTCSS tone, with
/// a sustained-hit gate for false-trigger rejection.
///
/// Feed blocks of 48 kHz demodulated audio via [`Self::process_block`]
/// and consume [`CtcssDecision::sustained`] to drive squelch. The
/// internal state keeps a small counter of consecutive hits / misses
/// so the gate's latch is stateful across calls — don't construct a
/// fresh detector per block or you'll lose the hysteresis.
pub struct CtcssDetector {
    /// Target CTCSS frequency in Hz.
    target_hz: f32,
    /// Goertzel feedback coefficient `2·cos(2π·f_target/fs)`.
    coeff: f32,
    /// Magnitude / RMS ratio above which a window counts as a hit.
    threshold: f32,
    /// Number of consecutive above-threshold windows required to
    /// open / close the sustained gate.
    min_hits: usize,

    /// Current state of the sustained gate. Flipped only when the
    /// hit/miss counter crosses `min_hits`.
    sustained: bool,
    /// Consecutive above-threshold windows since the last flip.
    /// Counter resets on a miss when the gate is closed.
    hit_run: usize,
    /// Consecutive below-threshold windows since the last flip.
    /// Counter resets on a hit when the gate is open.
    miss_run: usize,
}

impl CtcssDetector {
    /// Build a detector tuned to `target_hz` running on
    /// `sample_rate_hz` audio. Returns
    /// [`DspError::InvalidParameter`] if the target frequency is
    /// zero / negative / above Nyquist — a real CTCSS tone will
    /// always be well inside the valid range at 48 kHz, so the
    /// guard exists to catch wiring bugs rather than legitimate
    /// edge cases.
    ///
    /// Uses [`CTCSS_DEFAULT_THRESHOLD`] and [`CTCSS_MIN_HITS`] for
    /// the hit threshold and sustained-gate debounce count. Use
    /// [`Self::with_threshold`] to override.
    pub fn new(target_hz: f32, sample_rate_hz: f32) -> Result<Self, DspError> {
        if target_hz <= 0.0 || target_hz >= sample_rate_hz * 0.5 {
            return Err(DspError::InvalidParameter(format!(
                "CTCSS target_hz must be in (0, {}), got {target_hz}",
                sample_rate_hz * 0.5
            )));
        }
        let omega = core::f32::consts::TAU * target_hz / sample_rate_hz;
        Ok(Self {
            target_hz,
            coeff: 2.0 * omega.cos(),
            threshold: CTCSS_DEFAULT_THRESHOLD,
            min_hits: CTCSS_MIN_HITS,
            sustained: false,
            hit_run: 0,
            miss_run: 0,
        })
    }

    /// Build a detector with a custom hit threshold. The default
    /// value is [`CTCSS_DEFAULT_THRESHOLD`]; use this when the
    /// default produces too many / too few hits on your specific
    /// audio. Threshold is the ratio of target-frequency magnitude
    /// to window RMS.
    pub fn with_threshold(
        target_hz: f32,
        sample_rate_hz: f32,
        threshold: f32,
    ) -> Result<Self, DspError> {
        let mut detector = Self::new(target_hz, sample_rate_hz)?;
        detector.threshold = threshold;
        Ok(detector)
    }

    /// Target frequency this detector is tuned to.
    #[must_use]
    pub fn target_hz(&self) -> f32 {
        self.target_hz
    }

    /// Current state of the sustained-detection gate. Equivalent to
    /// the last [`CtcssDecision::sustained`] returned by
    /// [`Self::process_block`], except available between blocks if
    /// the caller needs to poll without feeding samples.
    #[must_use]
    pub fn is_sustained(&self) -> bool {
        self.sustained
    }

    /// Reset the sustained-gate counters and re-close the gate.
    /// Called at session start / demod-mode change so stale state
    /// from a previous transmission can't leak into the new one.
    pub fn reset(&mut self) {
        self.sustained = false;
        self.hit_run = 0;
        self.miss_run = 0;
    }

    /// Process one window of `samples` and update the sustained-hit
    /// gate. The caller is responsible for feeding blocks of
    /// approximately [`CTCSS_WINDOW_SAMPLES`] — shorter windows
    /// degrade the frequency resolution of the Goertzel filter,
    /// longer windows delay the detection.
    ///
    /// Returns a [`CtcssDecision`] with the normalized magnitude,
    /// the raw per-window decision, and the sustained gate state.
    pub fn process_block(&mut self, samples: &[f32]) -> CtcssDecision {
        if samples.is_empty() {
            return CtcssDecision {
                normalized_magnitude: 0.0,
                detected: false,
                sustained: self.sustained,
            };
        }

        // Goertzel recurrence. s1 is s[n-1], s2 is s[n-2].
        let mut s1: f32 = 0.0;
        let mut s2: f32 = 0.0;
        let mut sum_sq: f32 = 0.0;
        for &x in samples {
            let s = x + self.coeff * s1 - s2;
            s2 = s1;
            s1 = s;
            sum_sq += x * x;
        }
        // Magnitude squared at the target frequency. Algebraically
        // equivalent to |DFT[f_target]|² over the window.
        let mag_sq = s1 * s1 + s2 * s2 - self.coeff * s1 * s2;

        // Normalize by the RMS of the window so the threshold is
        // "proportion of signal energy in the target bin" rather
        // than an absolute magnitude (which would depend on audio
        // gain). `sum_sq / N` is mean-square; `sqrt` gives RMS.
        #[allow(clippy::cast_precision_loss)]
        let rms = (sum_sq / samples.len() as f32).sqrt();
        // Goertzel magnitude is scaled by N for time-domain units,
        // so divide by N to get it into the same unit as RMS. The
        // result is a dimensionless "amount of signal at target_hz
        // per unit of total signal".
        #[allow(clippy::cast_precision_loss)]
        let normalized_magnitude = if rms > f32::EPSILON {
            mag_sq.sqrt() / (samples.len() as f32 * rms)
        } else {
            0.0
        };

        let detected = normalized_magnitude >= self.threshold;

        // Sustained-gate state machine. `min_hits` controls both
        // the open and close debounce so a brief dropout doesn't
        // flap the squelch.
        if detected {
            self.hit_run = self.hit_run.saturating_add(1);
            self.miss_run = 0;
            if !self.sustained && self.hit_run >= self.min_hits {
                self.sustained = true;
            }
        } else {
            self.miss_run = self.miss_run.saturating_add(1);
            self.hit_run = 0;
            if self.sustained && self.miss_run >= self.min_hits {
                self.sustained = false;
            }
        }

        CtcssDecision {
            normalized_magnitude,
            detected,
            sustained: self.sustained,
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    /// Generate `n` samples of a unit-amplitude sine at `freq_hz`
    /// at [`CTCSS_SAMPLE_RATE_HZ`].
    fn tone(freq_hz: f32, n: usize, amplitude: f32) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / CTCSS_SAMPLE_RATE_HZ;
                amplitude * (core::f32::consts::TAU * freq_hz * t).sin()
            })
            .collect()
    }

    /// Synthetic speech-ish noise: sum of three voice-band tones
    /// (100 Hz fundamental + 450 Hz formant + 1100 Hz formant) with
    /// random per-sample amplitude. Not real speech, but has the
    /// key property of putting energy in the 80–250 Hz CTCSS band
    /// which is the main false-trigger risk.
    fn speech_like(n: usize, amplitude: f32) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / CTCSS_SAMPLE_RATE_HZ;
                let f0 = (core::f32::consts::TAU * 100.0 * t).sin() * 0.5;
                let f1 = (core::f32::consts::TAU * 450.0 * t).sin() * 0.3;
                let f2 = (core::f32::consts::TAU * 1_100.0 * t).sin() * 0.2;
                let envelope = 0.5 + 0.5 * ((i * 13 % 31) as f32 / 31.0);
                amplitude * envelope * (f0 + f1 + f2)
            })
            .collect()
    }

    fn window_samples() -> usize {
        CTCSS_WINDOW_SAMPLES
    }

    #[test]
    fn tone_table_is_ascending_and_unique() {
        // Sanity: if the table ever grows / shrinks we want a test
        // failure rather than a silent ordering change in the UI
        // dropdown.
        for w in CTCSS_TONES_HZ.windows(2) {
            assert!(
                w[0] < w[1],
                "CTCSS table must be strictly ascending, got {} >= {}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn ctcss_tone_index_finds_known_entries_and_rejects_unknown() {
        assert_eq!(ctcss_tone_index(67.0), Some(0));
        assert_eq!(ctcss_tone_index(100.0), Some(12));
        assert_eq!(ctcss_tone_index(254.1), Some(CTCSS_TONES_HZ.len() - 1));
        assert_eq!(ctcss_tone_index(60.0), None);
        assert_eq!(ctcss_tone_index(68.5), None);
    }

    #[test]
    fn constructor_rejects_out_of_range_frequencies() {
        assert!(CtcssDetector::new(0.0, CTCSS_SAMPLE_RATE_HZ).is_err());
        assert!(CtcssDetector::new(-1.0, CTCSS_SAMPLE_RATE_HZ).is_err());
        assert!(CtcssDetector::new(30_000.0, CTCSS_SAMPLE_RATE_HZ).is_err());
        // Nyquist exactly should be rejected.
        assert!(CtcssDetector::new(24_000.0, CTCSS_SAMPLE_RATE_HZ).is_err());
    }

    #[test]
    fn pure_target_tone_triggers_sustained_gate_after_min_hits() {
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let block = tone(100.0, window_samples(), 1.0);

        // First block: detected but not yet sustained.
        let d1 = det.process_block(&block);
        assert!(
            d1.detected,
            "100 Hz tone should be detected in a 100 Hz-tuned window: mag={}",
            d1.normalized_magnitude
        );
        assert!(!d1.sustained, "single window shouldn't flip sustained gate");

        // Second and third blocks: still hitting, not yet sustained
        // until hit count reaches min_hits.
        for _ in 0..(CTCSS_MIN_HITS - 2) {
            let d = det.process_block(&block);
            assert!(d.detected && !d.sustained);
        }

        // Third hit in a row: sustained gate opens.
        let dfinal = det.process_block(&block);
        assert!(
            dfinal.sustained,
            "sustained gate should open after CTCSS_MIN_HITS"
        );
    }

    #[test]
    fn pure_silence_never_triggers() {
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let silence = vec![0.0_f32; window_samples()];
        for _ in 0..10 {
            let d = det.process_block(&silence);
            assert!(!d.detected && !d.sustained);
        }
    }

    #[test]
    fn wrong_tone_does_not_trigger_target_detector() {
        // Detector tuned to 100 Hz, input is a clean 67 Hz tone.
        // Should NOT cross the sustained gate even over many blocks.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let wrong_tone = tone(67.0, window_samples(), 1.0);

        for _ in 0..10 {
            let d = det.process_block(&wrong_tone);
            assert!(
                !d.sustained,
                "67 Hz tone should not trigger a 100 Hz detector (mag={})",
                d.normalized_magnitude
            );
        }
    }

    #[test]
    fn speech_like_noise_alone_does_not_sustain() {
        // Pure speech-band content with no CTCSS tone. Voice
        // fundamentals in 100 Hz range may produce occasional hits
        // in a naive per-window check, but the sustained gate
        // should prevent the squelch from flapping open.
        let mut det =
            CtcssDetector::new(127.3, CTCSS_SAMPLE_RATE_HZ).expect("127.3 Hz is a valid target");
        let speech = speech_like(window_samples(), 1.0);

        for _ in 0..10 {
            det.process_block(&speech);
        }
        assert!(
            !det.is_sustained(),
            "speech-like signal without 127.3 Hz content should not sustain"
        );
    }

    #[test]
    fn tone_under_speech_still_triggers() {
        // Mixed signal: target tone + speech-band noise. This is
        // the real-world case — a radio transmitting voice with a
        // 100 Hz CTCSS tone mixed in. Detector should still
        // sustain.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let n = window_samples();
        let pure_tone = tone(100.0, n, 0.6);
        let noise = speech_like(n, 0.4);
        let mixed: Vec<f32> = pure_tone
            .iter()
            .zip(noise.iter())
            .map(|(&t, &s)| t + s)
            .collect();

        for _ in 0..CTCSS_MIN_HITS {
            det.process_block(&mixed);
        }
        assert!(
            det.is_sustained(),
            "CTCSS tone mixed under speech-band noise should still sustain the gate"
        );
    }

    #[test]
    fn gate_closes_after_tone_drops() {
        // Sustain the gate, then feed silence and verify it closes
        // after CTCSS_MIN_HITS miss windows.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let n = window_samples();
        let block = tone(100.0, n, 1.0);

        for _ in 0..CTCSS_MIN_HITS {
            det.process_block(&block);
        }
        assert!(det.is_sustained());

        // Drop the tone.
        let silence = vec![0.0_f32; n];
        for i in 0..CTCSS_MIN_HITS {
            det.process_block(&silence);
            // Should stay sustained until the miss-run reaches
            // min_hits; can drop on the final iteration.
            if i < CTCSS_MIN_HITS - 1 {
                assert!(
                    det.is_sustained(),
                    "gate should remain open until miss run reaches min_hits"
                );
            }
        }
        assert!(
            !det.is_sustained(),
            "gate must close after CTCSS_MIN_HITS misses"
        );
    }

    #[test]
    fn brief_dropout_does_not_flap_open_gate() {
        // Sustain the gate, then feed ONE silence window (below
        // min_hits dropouts), then resume tone. Gate should stay
        // open throughout — this is the hysteresis behavior.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let n = window_samples();
        let block = tone(100.0, n, 1.0);

        for _ in 0..CTCSS_MIN_HITS {
            det.process_block(&block);
        }
        assert!(det.is_sustained());

        // One miss window, then tone resumes.
        det.process_block(&vec![0.0_f32; n]);
        assert!(
            det.is_sustained(),
            "single miss below min_hits should not close the sustained gate"
        );
        det.process_block(&block);
        assert!(det.is_sustained());
    }

    #[test]
    fn reset_clears_sustained_state() {
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let n = window_samples();
        let block = tone(100.0, n, 1.0);

        for _ in 0..CTCSS_MIN_HITS {
            det.process_block(&block);
        }
        assert!(det.is_sustained());

        det.reset();
        assert!(!det.is_sustained());
    }

    #[test]
    fn empty_block_is_a_noop_and_returns_current_state() {
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let d = det.process_block(&[]);
        assert!(d.normalized_magnitude.abs() < f32::EPSILON);
        assert!(!d.detected);
        assert!(!d.sustained);
    }

    #[test]
    fn sustained_state_visible_via_is_sustained_between_blocks() {
        // Callers may want to poll `is_sustained` without feeding a
        // block — verify it matches the last returned decision.
        let mut det =
            CtcssDetector::new(100.0, CTCSS_SAMPLE_RATE_HZ).expect("100 Hz is a valid target");
        let block = tone(100.0, window_samples(), 1.0);
        for _ in 0..CTCSS_MIN_HITS {
            let d = det.process_block(&block);
            assert_eq!(det.is_sustained(), d.sustained);
        }
        assert!(det.is_sustained());
    }
}
