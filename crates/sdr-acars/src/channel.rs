//! Multi-channel ACARS decoder. Source-rate complex IQ feeds
//! N parallel per-channel pipelines (oscillator + decimator
//! → AM detect → MSK demod → frame parser).
//!
//! Faithful port of `original/acarsdec/rtl.c` per-channel
//! decimation — the IQ-fork pattern. Single-threaded inline
//! processing per `process()` call; no internal threads, no
//! mutex.
//!
//! # Magnitude-calibration deviation
//!
//! The C divides each `wf[ind]` by `rtlMult` (= `decim_factor`)
//! AND by `127.5` (RTL-SDR's u8 sample normalization). We do
//! NEITHER — our IQ source produces pre-normalized
//! `Complex<f32>` (no `/127.5` needed), and skipping
//! `/decim_factor` scales `accum.norm()` up by a constant
//! factor of `decim_factor`. This affects ONLY the level-dB
//! metadata reported per message (volatile in the e2e diff,
//! stripped before comparing); decode correctness is
//! unaffected because [`crate::msk::MskDemod`] normalizes the
//! matched-filter output internally.

use num_complex::Complex32;

use crate::error::AcarsError;
use crate::frame::{AcarsMessage, FrameParser};
use crate::msk::{IF_RATE_HZ, MskDemod};

/// Per-channel state. Owns its oscillator, decimator
/// accumulator, MSK demod, and frame parser. Private — only
/// [`ChannelBank`] composes one.
struct Channel {
    /// Pre-computed complex exponential at `-offset_hz`,
    /// sampled at source rate. Length = `decim_factor`. The
    /// "free running" oscillator extension uses
    /// `(osc_idx + n) mod decim_factor` wrap-around — at the
    /// instant `decim_count` reaches `decim_factor`,
    /// `osc_idx` has also wrapped back to 0, matching the
    /// C's per-block `for (ind = 0; ind < rtlMult; ind++)`
    /// init in `rtl.c::in_callback`.
    oscillator: Vec<Complex32>,
    /// Where in `oscillator` we are this block. Persists
    /// across `process()` calls so the oscillator is
    /// continuous across IQ-block boundaries.
    osc_idx: usize,
    /// Decimation accumulator state. Mirrors the C's local
    /// `float complex D` in `rtl.c::in_callback`, but lifted
    /// to per-Channel state so it survives partial blocks.
    accum: Complex32,
    /// Counter within the current decim period.
    decim_count: u32,
    /// Decimation factor (`source_rate / IF_RATE_HZ`). Mirrors
    /// C `rtlMult`.
    decim_factor: u32,
    /// Buffer of decimated IF samples (AM-detected real
    /// `f32`) to feed into [`MskDemod::process`]. Cleared at
    /// the start of each `ChannelBank::process` call.
    if_buffer: Vec<f32>,
    msk: MskDemod,
    parser: FrameParser,
    /// Per-channel multi-block reassembler (issue #580).
    /// Frames emerging from `parser.drain` flow through here
    /// before reaching `process`'s `on_message` callback —
    /// ETB blocks park; ETX blocks emit reassembled (or
    /// pass-through if no pending). Per-channel rather than
    /// shared because aircraft don't normally hop channels
    /// mid-message and channel-isolated state machines are
    /// simpler to reason about.
    assembler: crate::reassembly::MessageAssembler,
}

/// No-signal floor (dBFS) used as the idle baseline for a
/// `ChannelStats` `level_db` field. Below this value is
/// effectively the noise floor; above it indicates active
/// RF energy. Single source of truth so `ChannelStats::default()`
/// and `ChannelBank::new`'s per-channel initialization stay
/// in lockstep.
pub const NO_SIGNAL_FLOOR_DB: f32 = -120.0;

/// Per-channel statistics for the UI panel and CLI status.
#[derive(Clone, Copy, Debug)]
pub struct ChannelStats {
    /// Channel center frequency (Hz).
    pub freq_hz: f64,
    /// Wall-clock time of the most recent decoded message.
    pub last_msg_at: Option<std::time::SystemTime>,
    /// Total messages decoded on this channel since startup.
    pub msg_count: u32,
    /// Most-recent level estimate (dB). Reported but not yet
    /// computed — placeholder until level metering lands.
    pub level_db: f32,
    /// Three-state lock indicator for the sidebar glyph.
    pub lock_state: ChannelLockState,
}

impl Default for ChannelStats {
    /// Idle baseline — matches `ChannelBank::new`'s
    /// per-channel initialization. `level_db` defaults to
    /// [`NO_SIGNAL_FLOOR_DB`] (the dBFS noise floor), NOT 0.0
    /// which would inaccurately read as a strong present
    /// signal in any UI gauge consuming the field.
    fn default() -> Self {
        Self {
            freq_hz: 0.0,
            last_msg_at: None,
            msg_count: 0,
            level_db: NO_SIGNAL_FLOOR_DB,
            lock_state: ChannelLockState::Idle,
        }
    }
}

/// Three-state indicator for the sidebar glyph (●/○/⚠).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChannelLockState {
    /// No RF energy detected.
    #[default]
    Idle,
    /// RF energy present but no decoded frames within the
    /// recent window.
    Signal,
    /// Recent frames decoded successfully.
    Locked,
}

/// Multi-channel orchestrator. One source-rate IQ stream feeds
/// N narrowband channels in parallel.
pub struct ChannelBank {
    channels: Vec<Channel>,
    stats: Vec<ChannelStats>,
}

impl ChannelBank {
    /// Build a bank for `channels` (Hz), where the source IQ is
    /// at `source_rate_hz` centered on `center_hz`. Source rate
    /// must be an integer multiple of [`IF_RATE_HZ`] (12500 Hz).
    /// Each channel's offset from `center_hz` must fit within
    /// the source bandwidth (`±source_rate_hz / 2`).
    ///
    /// # Errors
    ///
    /// - [`AcarsError::InvalidChannelConfig`] if the channel
    ///   list is empty or any channel falls outside the source
    ///   bandwidth.
    /// - [`AcarsError::NonIntegerDecimation`] if
    ///   `source_rate_hz` is not an integer multiple of
    ///   [`IF_RATE_HZ`].
    pub fn new(source_rate_hz: f64, center_hz: f64, channels: &[f64]) -> Result<Self, AcarsError> {
        if channels.is_empty() {
            return Err(AcarsError::InvalidChannelConfig(
                "channel list is empty".into(),
            ));
        }
        let if_rate = f64::from(IF_RATE_HZ);
        // Reject zero / negative / NaN source rates up front.
        // Without this guard, `source_rate_hz == 0.0` passes the
        // integer-multiple check (decim_f = 0.0, fract = 0.0),
        // produces `decim_factor == 0`, builds a zero-length
        // oscillator, and crashes `process()` on the first sample
        // access. Library-crate rule: surface as a typed error,
        // not a deferred panic. Per CR round 1 on PR #583.
        if !source_rate_hz.is_finite() || source_rate_hz <= 0.0 {
            return Err(AcarsError::InvalidChannelConfig(format!(
                "source rate {source_rate_hz} Hz must be finite and positive"
            )));
        }
        let decim_f = source_rate_hz / if_rate;
        if decim_f.fract().abs() > 1e-6 {
            return Err(AcarsError::NonIntegerDecimation {
                source_rate_hz,
                if_rate_hz: if_rate,
            });
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let decim_factor = decim_f.round() as u32;

        let mut built = Vec::with_capacity(channels.len());
        let mut stats = Vec::with_capacity(channels.len());
        for (idx, &freq_hz) in channels.iter().enumerate() {
            let offset_hz = freq_hz - center_hz;
            // Channel must fit in source bandwidth (Nyquist).
            if offset_hz.abs() > source_rate_hz / 2.0 {
                return Err(AcarsError::InvalidChannelConfig(format!(
                    "channel {freq_hz} Hz outside source bandwidth ({source_rate_hz} Hz centered on {center_hz} Hz)"
                )));
            }
            // Build the oscillator: complex exp at -offset_hz,
            // sampled at the source rate. Length = decim_factor
            // (one decim period). The "free running" extension
            // is (osc_idx + n) mod decim_factor — see
            // `process()`.
            let mut oscillator = Vec::with_capacity(decim_factor as usize);
            for n in 0..decim_factor {
                let phase =
                    -2.0 * core::f64::consts::PI * offset_hz * f64::from(n) / source_rate_hz;
                #[allow(clippy::cast_possible_truncation)]
                oscillator.push(Complex32::new(phase.cos() as f32, phase.sin() as f32));
            }
            #[allow(clippy::cast_possible_truncation)]
            let idx_u8 = idx as u8;
            built.push(Channel {
                oscillator,
                osc_idx: 0,
                accum: Complex32::new(0.0, 0.0),
                decim_count: 0,
                decim_factor,
                if_buffer: Vec::with_capacity(4096),
                msk: MskDemod::new(),
                parser: FrameParser::new(idx_u8, freq_hz),
                assembler: crate::reassembly::MessageAssembler::new(),
            });
            stats.push(ChannelStats {
                freq_hz,
                last_msg_at: None,
                msg_count: 0,
                level_db: NO_SIGNAL_FLOOR_DB,
                lock_state: ChannelLockState::Idle,
            });
        }
        Ok(Self {
            channels: built,
            stats,
        })
    }

    /// Drain `iq` through every channel's pipeline, emitting
    /// any decoded messages via `on_message`. Mirrors
    /// `rtl.c::in_callback`'s per-block accumulator loop, then
    /// drives MSK + frame parsing per channel. The polarity-
    /// flip handshake (`FrameParser::take_polarity_flip` →
    /// `MskDemod::toggle_polarity`) handles 180° phase slip
    /// detected on inverted-SYN.
    pub fn process<F: FnMut(AcarsMessage)>(&mut self, iq: &[Complex32], mut on_message: F) {
        for (idx, ch) in self.channels.iter_mut().enumerate() {
            ch.if_buffer.clear();
            for &sample in iq {
                let osc = ch.oscillator[ch.osc_idx];
                ch.osc_idx = (ch.osc_idx + 1) % ch.oscillator.len();
                ch.accum += sample * osc;
                ch.decim_count += 1;
                if ch.decim_count >= ch.decim_factor {
                    // AM-detect: magnitude of the complex
                    // accumulator (matches C `cabsf(D)`).
                    let am_sample = ch.accum.norm();
                    ch.if_buffer.push(am_sample);
                    ch.accum = Complex32::new(0.0, 0.0);
                    ch.decim_count = 0;
                }
            }
            // Drive the MSK demod with the decimated IF.
            ch.msk.process(&ch.if_buffer, &mut ch.parser);
            // Drain any complete bytes accumulated in the
            // parser. Stamp live stats (msg_count, last_msg_at,
            // level_db) on every emitted message so
            // `channels()` reflects real state, not the
            // construction placeholders. Per CR round 1 on
            // PR #583.
            //
            // Each parser-emitted frame flows through the
            // per-channel `MessageAssembler` (issue #580). ETBs
            // park silently (the assembler returns 0 messages);
            // ETXs emit the reassembled message + any timed-out
            // partial sweeps. Stats are stamped once per
            // assembler-emitted message so a multi-block
            // reassembly counts as one in `msg_count`.
            let stats = &mut self.stats[idx];
            ch.parser.drain(|msg| {
                let now = msg.timestamp;
                for emitted in ch.assembler.observe(msg, now) {
                    stats.msg_count = stats.msg_count.saturating_add(1);
                    stats.last_msg_at = Some(emitted.timestamp);
                    // level_db on the message is currently 0.0
                    // (filled in by future sub-projects from MSK
                    // matched-filter output); keep stats.level_db
                    // pinned to the latest emitted value so the
                    // contract stays "stats reflect the latest
                    // decoded message" once levels start landing.
                    stats.level_db = emitted.level_db;
                    stats.lock_state = ChannelLockState::Locked;
                    on_message(emitted);
                }
            });
            // Drive timeout emission on every IQ-block tick, not
            // just when the parser produces a frame. A channel
            // that observed one ETB and then went silent would
            // never get an `observe()` call to internally sweep
            // — the partial reassembly would stay parked
            // indefinitely. The check is cheap (HashMap
            // iteration capped at MAX_PENDING_MESSAGES) and
            // runs at IQ-block cadence, well above the 30 s
            // recency window. Issue #580 / CR round 1 on PR #593.
            for emitted in ch.assembler.drain_timeouts(std::time::SystemTime::now()) {
                stats.msg_count = stats.msg_count.saturating_add(1);
                stats.last_msg_at = Some(emitted.timestamp);
                stats.level_db = emitted.level_db;
                stats.lock_state = ChannelLockState::Locked;
                on_message(emitted);
            }
            // Apply pending polarity flip if the parser
            // detected an inverted-SYN at frame start
            // (`acars.c:259,274`).
            if ch.parser.take_polarity_flip() {
                ch.msk.toggle_polarity();
            }
        }
        // Stats refresh (level, lock state) lands later — for
        // now we leave `stats` static post-construction. The
        // field is kept reachable via `channels()` so consumers
        // can already enumerate per-channel frequencies.
        let _ = &self.stats;
    }

    /// Snapshot of per-channel stats.
    #[must_use]
    pub fn channels(&self) -> &[ChannelStats] {
        &self.stats
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_channel_list() {
        // `unwrap_err` would require `ChannelBank: Debug`; use
        // a `match` on the result so we don't have to push a
        // Debug derive into the `Channel` substructs (which
        // contain MskDemod / FrameParser — neither of which
        // currently derives Debug, and that's an unrelated
        // decision to this task).
        match ChannelBank::new(2_400_000.0, 130_450_000.0, &[]) {
            Err(AcarsError::InvalidChannelConfig(_)) => {}
            Err(other) => panic!("expected InvalidChannelConfig, got {other:?}"),
            Ok(_) => panic!("expected InvalidChannelConfig, got Ok"),
        }
    }

    #[test]
    fn rejects_zero_or_negative_source_rate() {
        // Per CR round 1 on PR #583: zero/negative/NaN rates
        // would silently produce decim_factor=0 and crash later.
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            match ChannelBank::new(bad, 130_337_500.0, &[131_550_000.0]) {
                Err(AcarsError::InvalidChannelConfig(_)) => {}
                Err(other) => panic!("rate={bad}: expected InvalidChannelConfig, got {other:?}"),
                Ok(_) => panic!("rate={bad}: expected error, got Ok"),
            }
        }
    }

    #[test]
    fn rejects_non_integer_decimation() {
        match ChannelBank::new(2_400_001.0, 130_450_000.0, &[131_550_000.0]) {
            Err(AcarsError::NonIntegerDecimation { .. }) => {}
            Err(other) => panic!("expected NonIntegerDecimation, got {other:?}"),
            Ok(_) => panic!("expected NonIntegerDecimation, got Ok"),
        }
    }

    #[test]
    fn rejects_channel_outside_source_bandwidth() {
        // 200 MHz is well outside a 2.4 MHz window centered on
        // 130.45 MHz.
        match ChannelBank::new(2_400_000.0, 130_450_000.0, &[200_000_000.0]) {
            Err(AcarsError::InvalidChannelConfig(_)) => {}
            Err(other) => panic!("expected InvalidChannelConfig, got {other:?}"),
            Ok(_) => panic!("expected InvalidChannelConfig, got Ok"),
        }
    }

    #[test]
    fn accepts_valid_us_six_config() {
        // The US-6 set spans 129.125–131.550 MHz = 2.425 MHz —
        // wider than 2.4 MHz, so we use 2.5 MHz (decim_factor
        // = 200, integer multiple of 12.5 kHz; supported
        // natively by RTL-SDR). Center on the midpoint of the
        // extremes (130.3375 MHz) so the lowest channel's
        // offset of -1.2125 MHz fits the ±1.25 MHz window.
        // This mirrors `chooseFc`'s placement in
        // `original/acarsdec/rtl.c:131-165`.
        let bank = match ChannelBank::new(
            2_500_000.0,
            130_337_500.0,
            &[
                129_125_000.0,
                130_025_000.0,
                130_425_000.0,
                130_450_000.0,
                131_525_000.0,
                131_550_000.0,
            ],
        ) {
            Ok(b) => b,
            Err(e) => panic!("expected Ok, got {e:?}"),
        };
        assert_eq!(bank.channels().len(), 6);
        assert!((bank.channels()[0].freq_hz - 129_125_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn process_silent_iq_doesnt_decode() {
        let mut bank = match ChannelBank::new(2_500_000.0, 130_450_000.0, &[131_550_000.0]) {
            Ok(b) => b,
            Err(e) => panic!("expected Ok, got {e:?}"),
        };
        let silent = vec![Complex32::new(0.0, 0.0); 2500];
        bank.process(&silent, |_msg| {
            panic!("silence shouldn't produce messages");
        });
    }
}
