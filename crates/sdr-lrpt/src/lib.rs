//! Meteor-M LRPT post-demod decoder (epic #469).
//!
//! Stages 2-4 of the LRPT receive pipeline; stage 1 (QPSK demod)
//! lives in [`sdr_dsp::lrpt`].
//!
//! Layers shipped in this crate:
//!
//! - [`fec`] — Viterbi rate-1/2 + frame sync + de-randomize +
//!   Reed-Solomon (RS lands in PR 3; this PR ships the first three).
//!
//! Stage 3 (CCSDS framing, [`ccsds`]) and stage 4 (image
//! assembly, [`image`]) ship in subsequent PRs.
//!
//! Pure data crate — no DSP (those live in [`sdr_dsp::lrpt`]),
//! no GTK (UI lives in `sdr-ui`). Each layer's public surface is a
//! small struct with a `process` / `step` / `push` method matching
//! the project-wide DSP convention; internals stay private.
//!
//! Reference codebases (read-only, not linked):
//! `original/medet/`, `original/meteordemod/`, `original/SatDump/`.

#![forbid(unsafe_code)]

pub mod fec;
