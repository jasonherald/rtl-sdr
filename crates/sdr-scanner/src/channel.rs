//! Channel identity and per-channel config. `ScannerChannel` is
//! the resolved runtime shape — dwell/hang are already folded from
//! overrides + defaults; the scanner state machine doesn't need to
//! know about `Option`s here.

use sdr_types::DemodMode;

/// Stable identity for a channel across rebuilds of the channel
/// list. `(name, frequency_hz)` — same convention the bookmarks
/// flyout uses for the active-bookmark highlight.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChannelKey {
    pub name: String,
    pub frequency_hz: u64,
}

/// Fully-resolved scanner channel. The UI / controller builds
/// these from `Bookmark` entries at scan-start or on
/// `ChannelsChanged`; the state machine operates on them directly
/// and has no notion of bookmark storage.
#[derive(Debug, Clone)]
pub struct ScannerChannel {
    pub key: ChannelKey,
    pub frequency_hz: u64,
    pub demod_mode: DemodMode,
    pub bandwidth: f64,
    pub ctcss: Option<sdr_radio::af_chain::CtcssMode>,
    pub voice_squelch: Option<sdr_dsp::voice_squelch::VoiceSquelchMode>,
    /// 0 = normal rotation, >=1 = priority (checked more often).
    pub priority: u8,
    /// Resolved dwell time in ms (per-channel override folded in).
    pub dwell_ms: u32,
    /// Resolved hang time in ms (per-channel override folded in).
    pub hang_ms: u32,
}
