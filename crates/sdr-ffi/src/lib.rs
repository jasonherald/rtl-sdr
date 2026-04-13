//! Hand-rolled C ABI for the headless [`sdr_core`] SDR engine.
//!
//! `sdr-ffi` is the only crate in the workspace that emits
//! `#[unsafe(no_mangle)] extern "C"` symbols. Every public function
//! lives behind the contract documented in `include/sdr_core.h`
//! (the source of truth — this crate must match it byte-for-byte
//! and the `make ffi-header-check` CI lint enforces that). Spec:
//! `docs/superpowers/specs/2026-04-12-sdr-ffi-c-abi-design.md`.
//!
//! ## What lives where
//!
//! - [`error`] — error code enum mirroring `SdrCoreError` in the
//!   header, plus the thread-local last-error message machinery
//!   ([`error::sdr_core_last_error_message`]).
//! - [`handle`] — opaque [`handle::SdrCore`] struct that the C ABI
//!   hands the host as a forward-declared pointer. Wraps an
//!   [`sdr_core::Engine`] plus FFI-only state (registered callback,
//!   config path).
//! - [`event`] — event delivery from the engine into a registered
//!   C callback. (Currently a stub; full implementation lands in a
//!   later checkpoint of this PR.)
//!
//! ## Threading and reentrancy
//!
//! See the "Threading model" and "Reentrancy rules" sections of the
//! FFI spec for the full contract. tl;dr:
//! - Commands can be called from any thread.
//! - The event callback runs on the dispatcher thread (NOT the
//!   host's main thread); the host is responsible for marshaling
//!   to its UI thread.
//! - `sdr_core_destroy` must NOT be called from within the event
//!   callback (would deadlock against the dispatcher thread join).

// The workspace denies `unsafe_code` by default, but `sdr-ffi` is the
// crate whose entire job is to expose `#[unsafe(no_mangle)] extern "C"`
// symbols, dereference C pointers, and bridge between Rust ownership
// and C lifetimes. Override the workspace deny at the crate root and
// ensure every unsafe block is justified inline. This is the *only*
// crate in the workspace that should carry this allow.
#![allow(unsafe_code)]
#![allow(clippy::missing_safety_doc)] // safety contract is documented in include/sdr_core.h
#![allow(clippy::doc_markdown)]

pub mod error;
pub mod event;
pub mod handle;
pub mod lifecycle;

// Re-export the FFI symbols at the crate root so consumers that link
// the rlib (in-tree integration tests) can reference them via
// `sdr_ffi::sdr_core_*`.
pub use error::sdr_core_last_error_message;
pub use handle::SdrCore;
pub use lifecycle::{
    sdr_core_abi_version, sdr_core_create, sdr_core_destroy, sdr_core_init_logging,
};
