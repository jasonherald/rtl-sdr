//! Events fed into the scanner by the DSP controller or UI.
//! No wall-clock time anywhere — sample-count is the only timing
//! primitive, matching the `AutoBreakMachine` pattern.

use std::num::NonZeroU32;

use crate::channel::{ChannelKey, ScannerChannel};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SquelchState {
    Open,
    Closed,
}

#[derive(Debug, Clone)]
pub enum ScannerEvent {
    /// Fired by the DSP controller on every IQ block arrival.
    /// `samples_consumed` is block length; `sample_rate_hz`
    /// anchors the ms→sample conversion for dwell/hang/settle.
    ///
    /// Typed as `NonZeroU32` so the ms→sample math's zero-rate
    /// invariant is enforced at compile time rather than via a
    /// runtime debug-assert that silently degrades in release
    /// builds. Callers wrap the source sample rate with
    /// `NonZeroU32::new(rate).expect("source rate > 0")` — this
    /// is always true for any live SDR source.
    SampleTick {
        samples_consumed: u32,
        sample_rate_hz: NonZeroU32,
    },

    /// Edge-triggered squelch transition, identical to the stream
    /// already fed to the transcription tap for Auto Break.
    SquelchEdge(SquelchState),

    /// User added / removed / edited a scannable bookmark.
    /// Scanner swaps its channel list and recovers a sensible
    /// rotation position.
    ChannelsChanged(Vec<ScannerChannel>),

    /// Master scanner on/off toggle.
    SetEnabled(bool),

    /// Session-scoped lockout — channel is skipped in rotation
    /// until unlocked or scanner is disabled.
    LockoutChannel(ChannelKey),
    UnlockChannel(ChannelKey),
}
