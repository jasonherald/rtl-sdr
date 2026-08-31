//! `rtl_tcp` server auth-key surface: OS-keyring persistence, the
//! require-key toggle (server-first ordering per CR round 1 on
//! PR #406), reveal/copy/regenerate buttons, and the live
//! auth-change + advertiser-refresh plumbing. Split out of
//! `window/share.rs` per the Codacy 500-NLOC file gate on PR #844.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{Rc, RefCell, SidebarPanels, adw, gio, glib, plain_toast};
use super::handle::{AuthChangeOutcome, RunningServerHandle};
use super::{ServerSwitchWidgets, ServerSwitchWidgetsWeak};

/// Read the `rtl_tcp` server auth key from the OS keyring, if
/// present. Returns `Some(bytes)` for a well-formed hex-encoded
/// entry, `None` for a missing key, keyring unavailable, empty
/// entry, or corrupt hex. Corrupt entries are logged at `warn`
/// so operators can diagnose without the UI silently regenerating
/// over their paste. Per issue #395.
pub(in crate::window) fn load_server_auth_key_from_keyring() -> Option<Vec<u8>> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_KEY_AUTH_KEY, KEYRING_SERVICE, auth_key_from_hex};

    let store = KeyringStore::new(KEYRING_SERVICE);
    match store.get(KEYRING_KEY_AUTH_KEY) {
        Ok(Some(hex)) => {
            let Some(bytes) = auth_key_from_hex(&hex) else {
                tracing::warn!(
                    "rtl_tcp server auth key in keyring is malformed hex; regenerating on next toggle-on"
                );
                return None;
            };
            Some(bytes)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(%e, "rtl_tcp server auth key keyring read failed");
            None
        }
    }
}

/// Write the `rtl_tcp` server auth key to the OS keyring as
/// lowercase hex. Returns the underlying keyring error so
/// callers can surface it via toast — the caller is responsible
/// for deciding UX fallback (e.g. revert the toggle, show a
/// banner). Per issue #395.
pub(in crate::window) fn save_server_auth_key_to_keyring(
    bytes: &[u8],
) -> Result<(), sdr_config::keyring_store::KeyringError> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_KEY_AUTH_KEY, KEYRING_SERVICE, auth_key_to_hex};

    let store = KeyringStore::new(KEYRING_SERVICE);
    store.set(KEYRING_KEY_AUTH_KEY, &auth_key_to_hex(bytes))
}

/// Load the persisted server auth key, generating + saving a
/// fresh one when the keyring is either empty or corrupt. The
/// caller gets the fresh bytes regardless — a write failure
/// leaves the key in memory so the current session works, and
/// the next session's toggle-on retries the save path. Per
/// issue #395.
pub(in crate::window) fn ensure_server_auth_key() -> Vec<u8> {
    if let Some(existing) = load_server_auth_key_from_keyring() {
        return existing;
    }
    let fresh = sdr_server_rtltcp::auth::generate_random_auth_key();
    if let Err(e) = save_server_auth_key_to_keyring(&fresh) {
        tracing::warn!(%e, "rtl_tcp server auth key keyring write failed — in-memory only");
    }
    fresh
}

/// Auth controls of the Share panel (#394/#395): require-key toggle,
/// reveal, copy, and regenerate. All four closures share
/// `current_auth_key` / `auth_key_revealed` and the running-server
/// handle. Split out of [`connect_share_switch`] per the 50-NLOC
/// gate (#817).
pub(super) fn wire_share_auth_controls(
    panels: &SidebarPanels,
    widgets_weak: &ServerSwitchWidgetsWeak,
    running: &RunningServerHandle,
    current_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
    auth_key_revealed: &Rc<std::cell::Cell<bool>>,
    toast_overlay: &adw::ToastOverlay,
) {
    let toast_overlay_weak = toast_overlay.downgrade();

    // ====================================================
    // Auth controls (#394/#395) — toggle + reveal + copy +
    // regenerate. All four closures share `current_auth_key`
    // and `auth_key_revealed` via `Rc` + the running-server
    // handle via `running_for_auth_{toggle,regen}`.
    // ====================================================

    wire_auth_require_toggle(
        panels,
        running,
        current_auth_key,
        auth_key_revealed,
        &toast_overlay_weak,
        widgets_weak,
    );

    wire_auth_reveal_copy(panels, current_auth_key, auth_key_revealed);
    wire_auth_copy_button(panels, current_auth_key, &toast_overlay_weak);

    wire_auth_regenerate(
        panels,
        running,
        current_auth_key,
        auth_key_revealed,
        &toast_overlay_weak,
    );
}

/// Apply an auth-key change to the running server and refresh
/// the mDNS advertiser atomically. Returns `true` iff the
/// server actually holds the new state; `false` means the
/// caller must revert the UI so it stays in sync.
///
/// Thin adapter over [`RunningServerHandle::apply_auth_change`]
/// (issue #847): this function decides whether an advertiser
/// rebuild is wanted (widget refs available AND the user has
/// advertising on) and maps the typed outcome onto toasts + the
/// bool contract; the slot access, `set_auth_key` call, and the
/// rebuild itself live in the handle.
///
/// **Success cases:**
/// - No server is running: no server-side change to apply;
///   caller can proceed with UI.
/// - Server is running and `set_auth_key(new)` returned `Ok`.
///   A failed advertiser rebuild after a successful key change
///   still counts as success (the server holds the new state) —
///   it toasts, and the TXT record stays stale until the next
///   server start.
///
/// **Failure cases:**
/// - The slot is busy (another handler is mid-mutation — rare,
///   mid-click race). Caller reverts the switch; next click
///   usually wins.
/// - `set_auth_key` returns `Err` (e.g., mutex poisoned). The
///   toast surfaces the error and the caller reverts UI state.
///
/// Does NOT touch UI state — caller owns the UI mutation gate.
/// Per `CodeRabbit` round 1 on PR #406.
pub(in crate::window) fn apply_live_auth_change(
    running: &RunningServerHandle,
    new_key: Option<Vec<u8>>,
    widgets: Option<&ServerSwitchWidgets>,
    toast_overlay: &glib::WeakRef<adw::ToastOverlay>,
) -> bool {
    // Rebuild the mDNS advertiser only when we have widget refs
    // (caller upgraded `widgets_weak` before the call) AND the
    // user has advertising on — advertising turned off stays off;
    // we don't sneak it back on just because auth flipped.
    let rebuild_nickname = widgets
        .filter(|w| w.advertise_row.is_active())
        .map(|w| w.nickname_row.text());
    let (ok, toast) =
        auth_change_ui_response(running.apply_auth_change(new_key, rebuild_nickname.as_deref()));
    if let Some(message) = toast
        && let Some(overlay) = toast_overlay.upgrade()
    {
        overlay.add_toast(plain_toast(&message));
    }
    ok
}

/// Pure mapping from a typed [`AuthChangeOutcome`] to the UI
/// response: `(server_holds_new_state, toast_message)`. The bool
/// follows [`apply_live_auth_change`]'s contract (`false` ⇒ the
/// caller reverts the switch); the toast text, when present, is
/// surfaced by the caller. An advertiser-rebuild failure after a
/// successful key change still counts as success — the server
/// keeps running with the new key (same pattern as the initial
/// server-start advertise failure); worst case, clients see stale
/// auth metadata in the TXT record until the next server start.
///
/// Kept as a standalone widget-free function (rather than inlined
/// at the call site) so all five outcome branches are
/// unit-testable without a GTK harness or a live server — same
/// rationale as `share.rs::auth_key_ready_to_start`. Per the
/// Codacy AI review on PR #879. The `tracing::warn!`s for the
/// failure outcomes live here too so every mapping consequence is
/// in one place.
fn auth_change_ui_response(outcome: AuthChangeOutcome) -> (bool, Option<String>) {
    match outcome {
        AuthChangeOutcome::NotRunning | AuthChangeOutcome::Applied => (true, None),
        AuthChangeOutcome::AppliedAdvertiserFailed(e) => (
            true,
            Some(format!(
                "Couldn't refresh mDNS advertisement after auth toggle: {e}"
            )),
        ),
        AuthChangeOutcome::Busy => {
            tracing::warn!("auth change skipped — running-server cell busy");
            (false, None)
        }
        AuthChangeOutcome::Failed(e) => {
            tracing::warn!(%e, "Server::set_auth_key failed on live auth change");
            (
                false,
                Some(format!("Couldn't update auth on the running server: {e}")),
            )
        }
    }
}

/// Master "Require key" toggle: apply to the running server first, refresh the advertiser, then mutate UI state (CR round 1 on PR #406).
/// Split out per the 50-NLOC gate (#817).
fn wire_auth_require_toggle(
    panels: &SidebarPanels,
    running: &RunningServerHandle,
    current_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
    auth_key_revealed: &Rc<std::cell::Cell<bool>>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    widgets_weak: &ServerSwitchWidgetsWeak,
) {
    // Master "Require key" toggle.
    //
    // Order of operations (per `CodeRabbit` round 1 on PR #406):
    // 1. Apply the change to the running server FIRST.
    // 2. Refresh the mDNS advertiser so discovery TXT reflects
    //    the new `auth_required` flag.
    // 3. Only mutate UI state (current_auth_key, row visibility,
    //    subtitle, reveal button) after steps 1 and 2 succeeded.
    //
    // On any failure: revert the switch to its pre-toggle state
    // via `auth_toggle_reentry_guard` so UI ↔ server parity is
    // preserved. Discovery clients never see "auth advertised"
    // while the server is unauthed, or vice versa.
    //
    // When the server isn't running, steps 1+2 are no-ops and UI
    // mutation always proceeds — toggling auth with the switch
    // off is a config-only change and the next Start path
    // honors it via the pending-key plumbing.
    let deps = AuthToggleDeps {
        reentry_guard: Rc::new(std::cell::Cell::new(false)),
        key_row: panels.server.auth_key_row.downgrade(),
        widgets_weak: widgets_weak.clone(),
        running: running.clone(),
        toast_overlay: toast_overlay_weak.clone(),
        current_key: Rc::clone(current_auth_key),
        revealed: Rc::clone(auth_key_revealed),
        reveal_button: panels.server.auth_key_reveal_button.downgrade(),
        share_row: panels.server.share_row.downgrade(),
    };
    panels
        .server
        .auth_require_row
        .connect_active_notify(move |row| {
            on_auth_require_toggled(row, &deps);
        });
}

/// Body of the require-key toggle handler: reentry guard, widget
/// upgrades, then the enable / disable halves with switch-revert on
/// failure. Split out per the 50-NLOC gate (#817).
/// Everything the require-key toggle handler reads or mutates,
/// captured once by the closure in `wire_auth_require_toggle`.
///
/// `Clone` so the keyring-cold-path (issue #845) can hand an owned
/// copy into a `glib::spawn_future_local` continuation that outlives
/// the synchronous signal-handler call — every field is either an
/// `Rc`/`Cell` (main-thread-only, cheap to clone) or a `glib::WeakRef`
/// (never upgraded off the main thread), so this stays GTK-safe.
#[derive(Clone)]
struct AuthToggleDeps {
    reentry_guard: Rc<std::cell::Cell<bool>>,
    key_row: glib::WeakRef<adw::ActionRow>,
    widgets_weak: ServerSwitchWidgetsWeak,
    running: RunningServerHandle,
    toast_overlay: glib::WeakRef<adw::ToastOverlay>,
    current_key: Rc<RefCell<Option<Vec<u8>>>>,
    revealed: Rc<std::cell::Cell<bool>>,
    reveal_button: glib::WeakRef<gtk4::Button>,
    /// Master share switch — gated insensitive alongside the
    /// require-row for the duration of the async keyring load
    /// (issue #845, `CodeRabbit` round 1 on PR #873) so the user
    /// can't flip sharing on while `current_key` is still empty.
    share_row: glib::WeakRef<adw::SwitchRow>,
}

fn on_auth_require_toggled(row: &adw::SwitchRow, deps: &AuthToggleDeps) {
    let AuthToggleDeps {
        reentry_guard: auth_toggle_guard_for_handler,
        key_row: key_row_for_toggle,
        widgets_weak: widgets_weak_for_auth_toggle,
        running: running_for_auth_toggle,
        toast_overlay: toast_overlay_for_auth_toggle,
        current_key: current_key_for_toggle,
        revealed: revealed_for_toggle,
        reveal_button: reveal_button_for_toggle,
        share_row: _,
    } = deps;
    if auth_toggle_guard_for_handler.get() {
        // Re-entered from our own `set_active` revert
        // path — let the signal settle without running
        // the handler again.
        return;
    }
    let Some(key_row) = key_row_for_toggle.upgrade() else {
        return;
    };
    let widgets = widgets_weak_for_auth_toggle.upgrade();

    if row.is_active() {
        // Zero-I/O fast path: a key was already seeded (startup
        // restore, an earlier toggle-on, or Regenerate this
        // session) — apply it synchronously, no keyring round
        // trip needed. Otherwise fall to the async cold-load path
        // (#845) so a locked/slow Secret Service D-Bus call never
        // blocks this handler.
        let cached_key = current_key_for_toggle.borrow().clone();
        if let Some(key) = cached_key {
            let ok = apply_auth_key_and_reveal(
                widgets.as_ref(),
                running_for_auth_toggle,
                toast_overlay_for_auth_toggle,
                current_key_for_toggle,
                revealed_for_toggle,
                &key_row,
                reveal_button_for_toggle,
                key,
            );
            if !ok {
                // Revert the switch. UI stays on the pre-toggle
                // state; the user can click again after resolving
                // the server issue.
                auth_toggle_guard_for_handler.set(true);
                row.set_active(false);
                auth_toggle_guard_for_handler.set(false);
            }
        } else {
            enable_auth_requirement_async(row, deps);
        }
    } else {
        let ok = disable_auth_requirement(
            widgets.as_ref(),
            running_for_auth_toggle,
            toast_overlay_for_auth_toggle,
            current_key_for_toggle,
            revealed_for_toggle,
            &key_row,
        );
        if !ok {
            auth_toggle_guard_for_handler.set(true);
            row.set_active(true);
            auth_toggle_guard_for_handler.set(false);
        }
    }
}

/// Toggle-OFF half of the require-key handler — same order as the ON
/// half: server call first, UI mutation only after success. Returns
/// `false` when the caller must revert the switch. Split out per the
/// 50-NLOC gate (#817).
fn disable_auth_requirement(
    widgets: Option<&ServerSwitchWidgets>,
    running_for_auth_toggle: &RunningServerHandle,
    toast_overlay_for_auth_toggle: &glib::WeakRef<adw::ToastOverlay>,
    current_key_for_toggle: &Rc<RefCell<Option<Vec<u8>>>>,
    revealed_for_toggle: &Rc<std::cell::Cell<bool>>,
    key_row: &adw::ActionRow,
) -> bool {
    let server_result = apply_live_auth_change(
        running_for_auth_toggle,
        None,
        widgets,
        toast_overlay_for_auth_toggle,
    );
    if !server_result {
        return false;
    }
    *current_key_for_toggle.borrow_mut() = None;
    key_row.set_visible(false);
    // Zero the revealed flag too so a next toggle-on starts masked
    // regardless of the prior reveal state.
    revealed_for_toggle.set(false);
    true
}

/// Toggle-ON half of the require-key handler: apply an already-known
/// key to the live server + advertiser, then reveal the key row in
/// masked state. Returns `false` when the live-server apply failed
/// and the caller must revert the switch.
///
/// Takes the key as a plain argument rather than calling
/// `ensure_server_auth_key()` itself — the caller has already
/// resolved it, either synchronously from the `current_auth_key`
/// cache or asynchronously off the keyring (issue #845;
/// [`enable_auth_requirement_async`]). Keeping this function
/// keyring-free is what lets the cached-key path stay fully
/// synchronous with zero D-Bus I/O. Split out per the 50-NLOC gate
/// (#817).
#[allow(
    clippy::too_many_arguments,
    reason = "one extra `key: Vec<u8>` param over the 7-arg threshold vs. the \
              prior `enable_auth_requirement` — splitting further would scatter \
              the shared auth-row state across more helpers without improving \
              clarity"
)]
fn apply_auth_key_and_reveal(
    widgets: Option<&ServerSwitchWidgets>,
    running_for_auth_toggle: &RunningServerHandle,
    toast_overlay_for_auth_toggle: &glib::WeakRef<adw::ToastOverlay>,
    current_key_for_toggle: &Rc<RefCell<Option<Vec<u8>>>>,
    revealed_for_toggle: &Rc<std::cell::Cell<bool>>,
    key_row: &adw::ActionRow,
    reveal_button_for_toggle: &glib::WeakRef<gtk4::Button>,
    key: Vec<u8>,
) -> bool {
    // Step 1+2: apply to live server + refresh mDNS.
    let server_result = apply_live_auth_change(
        running_for_auth_toggle,
        Some(key.clone()),
        widgets,
        toast_overlay_for_auth_toggle,
    );
    if !server_result {
        return false;
    }

    // Step 3: UI mutation AFTER successful server change.
    *current_key_for_toggle.borrow_mut() = Some(key);
    key_row.set_visible(true);
    // Reset to masked state on every toggle-on so the
    // key row doesn't surface a previously-revealed
    // value across sessions.
    revealed_for_toggle.set(false);
    key_row.set_subtitle(crate::sidebar::server_panel::AUTH_KEY_MASKED_PLACEHOLDER);
    if let Some(rb) = reveal_button_for_toggle.upgrade() {
        rb.set_icon_name("view-reveal-symbolic");
        rb.set_tooltip_text(Some("Reveal key"));
        rb.update_property(&[gtk4::accessible::Property::Label("Reveal key")]);
    }
    true
}

/// Toggle-ON, keyring-cold-path (issue #845): `current_auth_key` is
/// empty, so the pending key has to come from the OS keyring — a
/// synchronous Secret Service D-Bus round trip (and potentially a
/// WRITE, when the keyring is empty/corrupt and a fresh key gets
/// generated + saved). Running that on the GTK main thread freezes
/// the whole UI when the keyring is locked or slow, so it's pushed
/// onto a `gio::spawn_blocking` worker and the result is applied
/// back on the main context via `glib::spawn_future_local`, mirroring
/// the established pattern in `sstv_viewer.rs::export_png_async`.
///
/// The require-row goes insensitive for the duration so a second
/// click can't race the in-flight load, and re-sensitizes as soon as
/// the worker returns (success OR failure) — before the apply
/// path runs, so a failed apply's switch-revert click lands on a
/// row the user can actually interact with again. On failure
/// (worker panic, missing widgets, or a failed live-server apply)
/// the switch reverts to off via the same reentry-guard trick the
/// synchronous path uses, so the programmatic `set_active(false)`
/// doesn't re-enter this handler.
///
/// Also gates the master Share switch insensitive for the same
/// duration (issue #845, `CodeRabbit` round 1 on PR #873, CWE-306):
/// without this, the user could flip sharing on while
/// `current_auth_key` is still empty and "Require key" reads
/// active, which would reach `Server::start` with `auth_key: None`
/// — an unauthenticated server despite the UI claiming otherwise.
/// `start_shared_server`'s `auth_key_ready_to_start` guard backstops
/// this even if the gate here is ever bypassed.
fn enable_auth_requirement_async(row: &adw::SwitchRow, deps: &AuthToggleDeps) {
    let row_weak = row.downgrade();
    let deps = deps.clone();
    row.set_sensitive(false);
    if let Some(share_row) = deps.share_row.upgrade() {
        share_row.set_sensitive(false);
    }
    glib::spawn_future_local(async move {
        let join = gio::spawn_blocking(ensure_server_auth_key).await;
        // Re-sensitize both rows regardless of outcome below — the
        // pending window is over either way.
        if let Some(share_row) = deps.share_row.upgrade() {
            share_row.set_sensitive(true);
        }
        let Some(row) = row_weak.upgrade() else {
            // Window torn down before the keyring round trip
            // finished — nothing left to update.
            return;
        };
        row.set_sensitive(true);

        let key = match join {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!("rtl_tcp auth-key keyring-load worker panicked: {e:?}");
                deps.reentry_guard.set(true);
                row.set_active(false);
                deps.reentry_guard.set(false);
                return;
            }
        };
        let Some(key_row) = deps.key_row.upgrade() else {
            // Key-display row is gone (window closing) — can't
            // safely turn auth on without it, so revert.
            deps.reentry_guard.set(true);
            row.set_active(false);
            deps.reentry_guard.set(false);
            return;
        };
        let widgets = deps.widgets_weak.upgrade();

        let ok = apply_auth_key_and_reveal(
            widgets.as_ref(),
            &deps.running,
            &deps.toast_overlay,
            &deps.current_key,
            &deps.revealed,
            &key_row,
            &deps.reveal_button,
            key,
        );
        if !ok {
            deps.reentry_guard.set(true);
            row.set_active(false);
            deps.reentry_guard.set(false);
        }
    });
}

/// Reveal/conceal + copy buttons for the auth-key row.
/// Split out per the 50-NLOC gate (#817).
fn wire_auth_reveal_copy(
    panels: &SidebarPanels,
    current_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
    auth_key_revealed: &Rc<std::cell::Cell<bool>>,
) {
    // Reveal / conceal button — flips the subtitle between the
    // masked placeholder and the full hex-encoded key. Pure UI
    // state; doesn't touch keyring or server.
    let key_row_for_reveal = panels.server.auth_key_row.downgrade();
    let current_key_for_reveal = Rc::clone(current_auth_key);
    let revealed_for_reveal = Rc::clone(auth_key_revealed);
    panels
        .server
        .auth_key_reveal_button
        .connect_clicked(move |btn| {
            let Some(key_row) = key_row_for_reveal.upgrade() else {
                return;
            };
            let Ok(key_opt) = current_key_for_reveal.try_borrow() else {
                return;
            };
            let Some(bytes) = key_opt.as_ref() else {
                return;
            };
            let now_revealed = !revealed_for_reveal.get();
            revealed_for_reveal.set(now_revealed);
            if now_revealed {
                key_row.set_subtitle(&crate::sidebar::server_panel::auth_key_to_hex(bytes));
                btn.set_icon_name("view-conceal-symbolic");
                btn.set_tooltip_text(Some("Hide key"));
                // Flip the accessible label alongside the icon /
                // tooltip so screen readers announce the current
                // action rather than the stale build-time label.
                // Per `CodeRabbit` round 1 on PR #406.
                btn.update_property(&[gtk4::accessible::Property::Label("Hide key")]);
            } else {
                key_row.set_subtitle(crate::sidebar::server_panel::AUTH_KEY_MASKED_PLACEHOLDER);
                btn.set_icon_name("view-reveal-symbolic");
                btn.set_tooltip_text(Some("Reveal key"));
                btn.update_property(&[gtk4::accessible::Property::Label("Reveal key")]);
            }
        });
}

/// Regenerate button: new key to keyring + live server + advertiser refresh.
/// Split out per the 50-NLOC gate (#817).
fn wire_auth_regenerate(
    panels: &SidebarPanels,
    running: &RunningServerHandle,
    current_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
    auth_key_revealed: &Rc<std::cell::Cell<bool>>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
) {
    let running_for_auth_regen = running.clone();
    let toast_overlay_for_regen = toast_overlay_weak.clone();
    // Regenerate button — generates a fresh 32-byte key,
    // applies it to the live server, persists to keyring, and
    // updates the display row subtitle (preserving the current
    // revealed state so the user can verify the new value
    // immediately).
    //
    // Order of operations (per `CodeRabbit` round 2 on PR #406):
    // 1. Apply to the running server via
    //    `apply_live_auth_change` — shared with the toggle path.
    //    On failure (mutex poisoned, borrow race), toast + return
    //    BEFORE touching keyring or UI.
    // 2. Persist to keyring. Failure here is non-fatal (the
    //    in-memory key still works this session; next launch
    //    would read the OLD keyring value, which now forces the
    //    user to click Regenerate again — better than the old
    //    order where a keyring success + server failure would
    //    leave next-launch using a key the server never
    //    accepted).
    // 3. UI mutation (`current_auth_key`, subtitle, toast).
    //
    // Regenerate keeps `auth_required = true`, so the mDNS TXT
    // doesn't change — `apply_live_auth_change` skips the
    // advertiser rebuild when passed `widgets = None`.
    let key_row_for_regen = panels.server.auth_key_row.downgrade();
    let current_key_for_regen = Rc::clone(current_auth_key);
    let revealed_for_regen = Rc::clone(auth_key_revealed);
    panels
        .server
        .auth_key_regenerate_button
        .connect_clicked(move |_btn| {
            let Some(key_row) = key_row_for_regen.upgrade() else {
                return;
            };
            let fresh = sdr_server_rtltcp::auth::generate_random_auth_key();

            // Step 1: live server apply. `widgets = None` because
            // regenerate doesn't flip `auth_required`, so no
            // advertiser rebuild is needed.
            if !apply_live_auth_change(
                &running_for_auth_regen,
                Some(fresh.clone()),
                None,
                &toast_overlay_for_regen,
            ) {
                return;
            }

            // Step 2: persist to keyring. Failure is tolerable —
            // current in-memory key still works this session; the
            // user can click Regenerate again later when the
            // keyring recovers. Toast so they know, but don't
            // roll back the server (it already accepted the key).
            if let Err(e) = save_server_auth_key_to_keyring(&fresh) {
                tracing::warn!(%e, "rtl_tcp auth-key regenerate keyring write failed");
                if let Some(overlay) = toast_overlay_for_regen.upgrade() {
                    overlay.add_toast(plain_toast(&format!(
                        "Couldn't save new key to keyring: {e}"
                    )));
                }
            }

            // Step 3: UI mutation after server + persistence
            // settled.
            *current_key_for_regen.borrow_mut() = Some(fresh.clone());
            if revealed_for_regen.get() {
                key_row.set_subtitle(&crate::sidebar::server_panel::auth_key_to_hex(&fresh));
            } else {
                key_row.set_subtitle(crate::sidebar::server_panel::AUTH_KEY_MASKED_PLACEHOLDER);
            }
            if let Some(overlay) = toast_overlay_for_regen.upgrade() {
                overlay.add_toast(plain_toast("New key generated"));
            }
        });
}

/// Copy button — always copies the FULL hex key.
/// Split out per the 50-NLOC gate (#817).
fn wire_auth_copy_button(
    panels: &SidebarPanels,
    current_key: &Rc<RefCell<Option<Vec<u8>>>>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
) {
    let toast_overlay_for_copy = toast_overlay_weak.clone();
    // Copy button — always copies the FULL hex key regardless of
    // reveal state. Users typically click Copy without clicking
    // Reveal first.
    let current_key_for_copy = Rc::clone(current_key);
    panels
        .server
        .auth_key_copy_button
        .connect_clicked(move |btn| {
            let Ok(key_opt) = current_key_for_copy.try_borrow() else {
                return;
            };
            let Some(bytes) = key_opt.as_ref() else {
                return;
            };
            let hex = crate::sidebar::server_panel::auth_key_to_hex(bytes);
            // Grab the display's clipboard via the button's widget
            // ancestry. `clipboard()` on a widget returns the
            // primary clipboard for the display it's attached to.
            let clipboard = btn.clipboard();
            clipboard.set_text(&hex);
            if let Some(overlay) = toast_overlay_for_copy.upgrade() {
                overlay.add_toast(plain_toast("Key copied to clipboard"));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::AuthChangeOutcome;
    use super::auth_change_ui_response;

    #[test]
    fn not_running_and_applied_succeed_without_toast() {
        assert_eq!(
            auth_change_ui_response(AuthChangeOutcome::NotRunning),
            (true, None)
        );
        assert_eq!(
            auth_change_ui_response(AuthChangeOutcome::Applied),
            (true, None)
        );
    }

    #[test]
    fn advertiser_failure_still_succeeds_but_toasts() {
        let (ok, toast) = auth_change_ui_response(AuthChangeOutcome::AppliedAdvertiserFailed(
            "mdns down".into(),
        ));
        assert!(ok);
        assert_eq!(
            toast.as_deref(),
            Some("Couldn't refresh mDNS advertisement after auth toggle: mdns down")
        );
    }

    #[test]
    fn busy_fails_silently() {
        assert_eq!(
            auth_change_ui_response(AuthChangeOutcome::Busy),
            (false, None)
        );
    }

    #[test]
    fn server_failure_fails_with_toast() {
        let (ok, toast) =
            auth_change_ui_response(AuthChangeOutcome::Failed("mutex poisoned".into()));
        assert!(!ok);
        assert_eq!(
            toast.as_deref(),
            Some("Couldn't update auth on the running server: mutex poisoned")
        );
    }
}
