//! Typed main-thread owner for the running `rtl_tcp` server
//! (issue #847). Replaces the `Rc<RefCell<Option<RunningServer>>>`
//! cell that was shared raw across four callback families (share
//! switch, listener-cap live apply, auth toggle/regenerate, status
//! polling) with a `glib::Object` handle exposing exactly the
//! operations those callbacks need. The interior `RefCell` is
//! private to this module — no caller can hold a borrow across a
//! GTK callback boundary anymore, and every access pattern
//! (tight-scope borrow, try-borrow skip, try-borrow-mut with a
//! typed Busy outcome) is named and documented here instead of
//! re-derived at each call site.

use gtk4::glib;
use gtk4::glib::subclass::prelude::ObjectSubclassIsExt;

use super::RunningServer;

mod imp {
    use std::cell::RefCell;

    use gtk4::glib;
    use gtk4::glib::subclass::prelude::{ObjectImpl, ObjectSubclass};

    #[derive(Default)]
    pub(in crate::window) struct RunningServerHandle {
        /// The one slot. `None` while no server is running. Kept
        /// module-private so every borrow lives inside a
        /// `RunningServerHandle` method — callers can't leak a
        /// `Ref`/`RefMut` into GTK callback scope.
        pub(super) slot: RefCell<Option<super::super::RunningServer>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RunningServerHandle {
        const NAME: &'static str = "SdrRunningServerHandle";
        type Type = super::RunningServerHandle;
    }

    impl ObjectImpl for RunningServerHandle {}
}

glib::wrapper! {
    /// Glib subclass owning the `Option<RunningServer>` slot.
    /// `glib::Object` is internally refcounted, so the wiring
    /// functions clone this handle directly into their closures
    /// (no `Rc` wrapper) and share the same slot — same sharing
    /// semantics as the old cell, but the cell itself is now
    /// unreachable outside `handle.rs`.
    pub(in crate::window) struct RunningServerHandle(ObjectSubclass<imp::RunningServerHandle>);
}

/// Outcome of [`RunningServerHandle::apply_auth_change`]. UI
/// consequences (toasts, switch reverts) stay in the caller —
/// `auth.rs::apply_live_auth_change` maps these to its bool
/// contract — so the handle itself never touches widgets and the
/// no-server surface stays unit-testable without a GTK harness.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::window) enum AuthChangeOutcome {
    /// No server is running — a UI-only change is always fine.
    NotRunning,
    /// `Server::set_auth_key` succeeded (and the advertiser
    /// rebuild, when one was requested, succeeded too).
    Applied,
    /// The key change reached the server, but the requested mDNS
    /// advertiser rebuild failed — the TXT record's
    /// `auth_required` flag will be stale until the next server
    /// start. Carries the error text for the caller's toast.
    AppliedAdvertiserFailed(String),
    /// Borrow contention on the slot (another handler is
    /// mid-mutation — rare, mid-click race). Caller reverts the
    /// UI; the next click usually wins.
    Busy,
    /// `Server::set_auth_key` failed (e.g. mutex poisoned).
    /// Carries the error text for the caller's toast.
    Failed(String),
}

impl RunningServerHandle {
    /// Fresh handle with an empty slot. One per window, created in
    /// `connect_server_panel` and cloned into the wiring closures.
    pub(in crate::window) fn new() -> Self {
        glib::Object::new()
    }

    /// Store a freshly started server (+ optional mDNS advertiser)
    /// in the slot. Caller flips the shared `server_running` flag
    /// AFTER this returns so the source-panel guard can't race
    /// against a mid-construction state.
    pub(in crate::window) fn install(
        &self,
        server: super::Server,
        advertiser: Option<super::Advertiser>,
    ) {
        *self.imp().slot.borrow_mut() = Some(RunningServer { server, advertiser });
    }

    /// Take and tear down the running server, if any. Drops the
    /// advertiser FIRST so peers see the mDNS goodbye packet before
    /// the server stops — field declaration order in
    /// `RunningServer` would drop `server` first, so the advertiser
    /// is taken explicitly to reverse. `Server::drop` then signals
    /// shutdown and joins the accept thread. No-op on an empty slot.
    pub(in crate::window) fn shutdown(&self) {
        if let Some(mut handle) = self.imp().slot.borrow_mut().take() {
            drop(handle.advertiser.take());
            drop(handle.server);
        }
    }

    /// Snapshot `(Server::stats(), Server::has_stopped())` for the
    /// status-poll tick. `stats()` internally locks a Mutex — the
    /// return is a Clone, so the borrow scope stays tight. `None`
    /// when no server is running.
    pub(in crate::window) fn poll_stats(&self) -> Option<(sdr_server_rtltcp::ServerStats, bool)> {
        self.imp()
            .slot
            .borrow()
            .as_ref()
            .map(|h| (h.server.stats(), h.server.has_stopped()))
    }

    /// Live-apply a listener-cap change to the running server.
    /// Silently skips when another handler is mid-mutation on the
    /// slot (e.g. the share switch flipping server start/stop) —
    /// the spin row's new value is already persisted via the
    /// `server_panel` signal, and the next accept after start picks
    /// it up through `build_server_config_from_panel`. Also a
    /// no-op when no server is running: the edit is persisted;
    /// nothing to apply live.
    pub(in crate::window) fn set_listener_cap(&self, cap: usize) {
        let Ok(slot) = self.imp().slot.try_borrow() else {
            return;
        };
        if let Some(handle) = slot.as_ref() {
            handle.server.set_listener_cap(cap);
        }
    }

    /// Apply an auth-key change to the running server and, when
    /// `rebuild_nickname` is `Some`, rebuild the mDNS advertiser so
    /// its TXT record reflects the new `Server::auth_required()`
    /// state. The caller passes `None` for the nickname when the
    /// user has advertising off (we don't sneak it back on) or when
    /// no rebuild is needed (Regenerate keeps `auth_required=true`,
    /// so the TXT doesn't change).
    ///
    /// The old advertiser is dropped FIRST so its Drop-based
    /// unregister fires before we re-announce under the same
    /// instance name — mdns-sd allows back-to-back registers, but
    /// cleanly bracketed unregister/register avoids a window where
    /// duplicate records briefly coexist on the LAN.
    ///
    /// Returns a typed [`AuthChangeOutcome`]; toasts and switch
    /// reverts are the caller's job (see
    /// `auth.rs::apply_live_auth_change`), keeping this method
    /// widget-free.
    pub(in crate::window) fn apply_auth_change(
        &self,
        new_key: Option<Vec<u8>>,
        rebuild_nickname: Option<&str>,
    ) -> AuthChangeOutcome {
        let Ok(mut slot) = self.imp().slot.try_borrow_mut() else {
            return AuthChangeOutcome::Busy;
        };
        let Some(handle) = slot.as_mut() else {
            return AuthChangeOutcome::NotRunning;
        };
        if let Err(e) = handle.server.set_auth_key(new_key) {
            return AuthChangeOutcome::Failed(e.to_string());
        }
        let Some(nickname) = rebuild_nickname else {
            return AuthChangeOutcome::Applied;
        };
        drop(handle.advertiser.take());
        match super::build_advertiser(&handle.server, nickname) {
            Ok(adv) => {
                handle.advertiser = Some(adv);
                AuthChangeOutcome::Applied
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "mDNS advertiser rebuild after auth toggle failed; TXT auth_required will be stale until next start"
                );
                AuthChangeOutcome::AppliedAdvertiserFailed(e.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthChangeOutcome, RunningServerHandle};

    #[test]
    fn poll_stats_none_without_server() {
        let handle = RunningServerHandle::new();
        assert!(handle.poll_stats().is_none());
    }

    #[test]
    fn apply_auth_change_not_running_without_server() {
        let handle = RunningServerHandle::new();
        assert_eq!(
            handle.apply_auth_change(None, None),
            AuthChangeOutcome::NotRunning
        );
        assert_eq!(
            handle.apply_auth_change(Some(vec![0u8; 32]), Some("nick")),
            AuthChangeOutcome::NotRunning
        );
    }

    #[test]
    fn set_listener_cap_and_shutdown_noop_without_server() {
        let handle = RunningServerHandle::new();
        handle.set_listener_cap(4);
        handle.shutdown();
        assert!(handle.poll_stats().is_none());
    }
}
