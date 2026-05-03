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

pub mod chain;
pub mod derand;
pub mod reed_solomon;
pub mod soft_sync;
pub mod sync;
pub mod viterbi;

pub use chain::FecChain;
pub use derand::Derandomizer;
pub use reed_solomon::{K as RS_K, N as RS_N, ReedSolomon, RsError, T as RS_T};
pub use soft_sync::{
    ASM_ENCODED, ASM_ENCODED_BITS, ROTATION_COUNT, Rotation, SOFT_SYNC_THRESHOLD, SoftSyncDetector,
};
pub use sync::{ASM, ASM_BITS, SYNC_THRESHOLD, SyncCorrelator};
pub use viterbi::{POLYA, POLYB, TRACEBACK_DEPTH, ViterbiDecoder};
