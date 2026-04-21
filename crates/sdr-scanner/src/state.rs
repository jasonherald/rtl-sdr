//! Scanner phase enum surfaced to the UI + internal state variants
//! carrying per-phase bookkeeping.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScannerState {
    /// Scanner off, or on with no channels enabled.
    Idle,
    /// Retune command emitted, audio muted, waiting for settle
    /// window to close before honoring squelch on the new channel.
    Retuning,
    /// Settled on the target channel, audio still muted,
    /// listening for squelch-open within the dwell window.
    Dwelling,
    /// Squelch open post-settle, audio flowing.
    Listening,
    /// Squelch closed, audio muted, counting down hang window
    /// before advancing to next channel.
    Hanging,
}
