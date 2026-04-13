//! Event delivery from the engine into the host via a registered C
//! callback. This module is currently a **stub** that declares only
//! the callback type alias so [`crate::handle`] can store one; the
//! full event struct, dispatcher thread, and `set_event_callback`
//! entry point land in a later checkpoint of this PR.

use std::ffi::c_void;

/// C callback type. Will be invoked from the dispatcher thread when
/// the engine emits a `DspToUi` event. The full `SdrEvent` struct
/// (tagged-union of variants) is defined in a later checkpoint of
/// this PR; for now we just have the callback signature so the
/// handle module can store one.
///
/// `event` is a borrowed pointer valid only for the duration of the
/// callback. `user_data` is the same pointer the host passed when
/// registering the callback — opaque to us, hands-back to the host.
pub type SdrEventCallback =
    Option<unsafe extern "C" fn(event: *const SdrEvent, user_data: *mut c_void)>;

/// Forward declaration. Real definition lands in checkpoint 4.
#[repr(C)]
pub struct SdrEvent {
    _placeholder: u8,
}
