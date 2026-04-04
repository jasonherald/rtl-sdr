//! Pure DSP library (no threading, no I/O).
//!
//! All functions are pure: no side effects, no thread spawning, no I/O.

pub mod fft;
pub mod math;
pub mod taps;
pub mod window;
