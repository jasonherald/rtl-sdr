//! Events fed into the scanner by the DSP controller or UI.
//! No wall-clock time anywhere — sample-count is the only timing
//! primitive, matching the `AutoBreakMachine` pattern.

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
    SampleTick {
        samples_consumed: u32,
        sample_rate_hz: u32,
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
    UnlockoutChannel(ChannelKey),
}
