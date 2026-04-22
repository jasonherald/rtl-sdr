//! Scanner state machine — sequential channel monitoring for SDR
//! pipelines. Pure no-I/O logic: consumes events, emits commands,
//! leaves all actual radio/audio wiring to the DSP controller that
//! owns an instance.
//!
//! See docs/superpowers/specs/2026-04-21-scanner-phase-1-design.md
//! for the design decisions behind this crate's shape.

pub mod channel;
pub mod commands;
pub mod events;
pub mod scanner;
pub mod state;

pub use channel::{ChannelKey, ScannerChannel};
pub use commands::ScannerCommand;
pub use events::{ScannerEvent, SquelchState};
pub use scanner::Scanner;
pub use state::ScannerState;

/// Default dwell time in ms when a channel doesn't override it.
pub const DEFAULT_DWELL_MS: u32 = 100;

/// Default hang time in ms when a channel doesn't override it.
pub const DEFAULT_HANG_MS: u32 = 2000;

/// Settle window in ms after a retune before the scanner honors
/// squelch edges on the new channel. Covers PLL lock + filter
/// warm-up transients — scanner decisions during this window are
/// unreliable.
pub const SETTLE_MS: u32 = 30;

/// How often (in normal-channel hops) the scanner sweeps priority
/// channels between normal rotations. `5` means every 5 normal
/// hops, all priority-1 channels get a check before resuming.
pub const PRIORITY_CHECK_INTERVAL: u32 = 5;
