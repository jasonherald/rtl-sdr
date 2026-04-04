//! Pure DSP library (no threading, no I/O).
//!
//! All functions are pure: no side effects, no thread spawning, no I/O.

pub mod convert;
pub mod correction;
pub mod demod;
pub mod fft;
pub mod filter;
pub mod loops;
pub mod math;
pub mod multirate;
pub mod noise;
pub mod taps;
pub mod window;
