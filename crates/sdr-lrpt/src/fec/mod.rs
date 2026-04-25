//! FEC chain for Meteor-M LRPT.
//!
//! ```text
//! soft i8 ──▶ Viterbi ──▶ Sync ──▶ Derand ──▶ Reed-Solomon ──▶ frames
//! ```
//!
//! Each layer is a streaming `process` / `step` / `push` matching
//! the project-wide DSP convention. Buffers are caller-allocated.
//! No async, no threading, no I/O.
//!
//! This PR (Task 2) ships [`ViterbiDecoder`], [`SyncCorrelator`],
//! and [`Derandomizer`]. Reed-Solomon lands in Task 3.

pub mod derand;
pub mod sync;
pub mod viterbi;

pub use derand::Derandomizer;
pub use sync::{ASM, SYNC_THRESHOLD, SyncCorrelator};
pub use viterbi::{POLYA, POLYB, TRACEBACK_DEPTH, ViterbiDecoder};
