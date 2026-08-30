//! `rtl_tcp` connection-state toast wiring: the top-level
//! [`handle_rtl_tcp_state_toast`] dispatcher and its per-variant
//! arms (`ControllerBusy` / `AuthRequired` / `AuthFailed` /
//! `Connected`), plus the status-row sensitivity helper. Split out
//! of `window/source/connect.rs` per the Codacy large-file gate
//! (#846).

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::super::dsp_events::DspEventCtx;
use super::super::super::{
    AppState, Rc, RefCell, TOAST_TIMEOUT_PERSISTENT, TOAST_TIMEOUT_SHORT_SECS, UiToDsp, adw, glib,
    sidebar,
};
use super::keyring::{clear_client_auth_key_from_keyring, save_client_auth_key_to_keyring};

/// Called only from the edge-detection path in
/// `handle_dsp_message`; the caller already verified
/// `prev_disc != now_disc` and stored the new discriminant.
/// Per issue #396.
#[allow(
    clippy::too_many_arguments,
    reason = "toast composition needs read access to multiple panel widgets \
              + a dispatch handle; collapsing into a single context struct \
              would move the same argument count one layer up"
)]
#[allow(
    clippy::doc_markdown,
    reason = "doc references to Connected / ControllerBusy / AuthRequired / \
              AuthFailed are type variants — enum paths would make the prose \
              unreadable; backticks on each would overwhelm the paragraph"
)]
#[allow(
    clippy::too_many_lines,
    reason = "linear arm-by-arm toast + row + state handling for all 8 rtl_tcp connection-state variants; splitting would scatter the shared setup (pending-toasts sweep, edge-log) and obscure the 1:1 mapping from variant to UX gesture"
)]
pub(in crate::window) fn handle_rtl_tcp_state_toast(
    state_val: &sdr_types::RtlTcpConnectionState,
    prev_disc: u8,
    ctx: &DspEventCtx,
) {
    use sdr_types::RtlTcpConnectionState;

    let app_state = &ctx.state;
    let toast_overlay_weak = &ctx.toast_overlay_weak;
    let role_row_weak = &ctx.rtl_tcp_role_row_weak;
    let auth_key_row_weak = &ctx.rtl_tcp_auth_key_row_weak;
    let hostname_row_weak = &ctx.rtl_tcp_hostname_row_weak;
    let port_row_weak = &ctx.rtl_tcp_port_row_weak;
    let pending_controller_busy_toasts = &ctx.pending_controller_busy_toasts;

    // Sweep any still-live ControllerBusy toasts on any
    // transition that isn't re-entering ControllerBusy. Pre-
    // `CodeRabbit` round 11 on PR #408 each ControllerBusy
    // toast's button handler only dismissed itself, so a stale
    // "Take control" / "Connect as Listener" action sat visible
    // after the server went away (Connected directly, Disconnect,
    // Failed, etc.) and could later rebuild the source
    // unexpectedly against a healthy session. The
    // `timeout(0)` persistence is intentional — we WANT these to
    // stick around until the user interacts OR the state
    // resolves itself — but "the state resolved itself" needs
    // its own cleanup pass.
    if !matches!(state_val, RtlTcpConnectionState::ControllerBusy) {
        dismiss_stale_controller_busy_toasts(pending_controller_busy_toasts);
    }

    match state_val {
        RtlTcpConnectionState::ControllerBusy => on_rtl_tcp_controller_busy(
            app_state,
            toast_overlay_weak,
            role_row_weak,
            pending_controller_busy_toasts,
        ),

        RtlTcpConnectionState::AuthRequired => on_rtl_tcp_auth_required(
            app_state,
            toast_overlay_weak,
            auth_key_row_weak,
            hostname_row_weak,
            port_row_weak,
        ),

        RtlTcpConnectionState::AuthFailed => on_rtl_tcp_auth_failed(
            app_state,
            toast_overlay_weak,
            auth_key_row_weak,
            hostname_row_weak,
            port_row_weak,
        ),

        RtlTcpConnectionState::Connected { .. } => on_rtl_tcp_connected(
            prev_disc,
            app_state,
            auth_key_row_weak,
            hostname_row_weak,
            port_row_weak,
        ),

        // Non-toast states (Disconnected / Connecting / Retrying
        // / Failed) just update the status row subtitle via the
        // sibling call in `handle_dsp_message`. No additional
        // UX gesture needed here.
        RtlTcpConnectionState::Disconnected
        | RtlTcpConnectionState::Connecting
        | RtlTcpConnectionState::Retrying { .. }
        | RtlTcpConnectionState::Failed { .. } => {}
    }
}

/// Dismiss every still-live `ControllerBusy` toast, if any. Two call
/// sites, each with its own rationale for when to sweep — see
/// `handle_rtl_tcp_state_toast` (sweep on any transition away from
/// `ControllerBusy`) and `on_rtl_tcp_controller_busy` (sweep before
/// adding a fresh pair so the overlay never stacks two). Split out
/// per the 50-NLOC gate (#817).
fn dismiss_stale_controller_busy_toasts(
    pending_controller_busy_toasts: &Rc<RefCell<Vec<glib::WeakRef<adw::Toast>>>>,
) {
    let mut pending = pending_controller_busy_toasts.borrow_mut();
    for weak in pending.drain(..) {
        if let Some(toast) = weak.upgrade() {
            toast.dismiss();
        }
    }
}

/// Record the currently-displayed `rtl_tcp` server's `host:port`
/// on `AppState` so a subsequent successful `Connected` can save
/// the just-entered key to the right per-server keyring entry.
/// Empty on upgrade failure — the save path skips when the
/// cached identity is empty. Per #396.
///
/// **Cache-preserving fallback** (per `CodeRabbit` round 2 on
/// PR #408): if `app_state.rtl_tcp_active_server` is already
/// non-empty, this is a no-op. `apply_rtl_tcp_connect` writes
/// the stable advertised `hostname:port` (same form as
/// `favorite_key(server)`) directly into the cache at
/// connect-setup time, so every downstream per-server lookup
/// (keyring load/save/clear, favorite match) keys off the same
/// identity. Reading `hostname_row.text()` here would overwrite
/// the stable id with whatever the DSP is dialing — for
/// discovery connects that can be a resolved IPv4/IPv6 literal,
/// splitting "shack-pi.local.:1234" (favorites) from
/// "192.168.1.17:1234" (keyring) and breaking round-trip. The
/// widget-read fallback only runs in the manually-typed Play
/// path where `apply_rtl_tcp_connect` never ran.
pub(super) fn record_active_rtl_tcp_server(
    app_state: &Rc<AppState>,
    hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    port_row_weak: &glib::WeakRef<adw::SpinRow>,
) {
    if !app_state.rtl_tcp_active_server.borrow().is_empty() {
        return;
    }
    let Some(host_row) = hostname_row_weak.upgrade() else {
        return;
    };
    let Some(port_row) = port_row_weak.upgrade() else {
        return;
    };
    let host = host_row.text().to_string();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let port = port_row.value() as u16;
    if !host.is_empty() && port != 0 {
        *app_state.rtl_tcp_active_server.borrow_mut() = format!("{host}:{port}");
    }
}

/// Invalidate the cached active `rtl_tcp` server identity when
/// the hostname / port widgets no longer match it. Called from
/// the `hostname_row.connect_changed` + `port_row.connect_value_
/// notify` handlers so a manual edit retargets per-server state
/// (keyring lookups, favorite matches, `rtl_tcp_active_server`)
/// to the newly-typed endpoint.
///
/// Without this, after the startup `LastConnectedServer` restore
/// or an `apply_rtl_tcp_connect` seeded the cache, typing a
/// different host or port in the source row would leave the
/// cache pointing at the old server — the first subsequent
/// `AuthFailed` / `Connected` arm would then
/// clear/save the key under the WRONG server. Per
/// `CodeRabbit` round 4 on PR #408.
///
/// **Comparison guard:** the cache is cleared only when its
/// current value differs from the widget-derived key. That
/// keeps `apply_rtl_tcp_connect`'s own `hostname_row.set_text` /
/// `port_row.set_value` writes (which fire these same handlers)
/// from spuriously clobbering the stable id the caller just
/// wrote. During a caller-driven server switch the cache IS
/// stale at the widget-write moment (old server id, new widget
/// text), so this invalidation fires correctly there too —
/// `apply_rtl_tcp_connect` overwrites the empty cache right
/// afterwards with the new stable id.
///
/// Also clears the auth-key row (visibility + text) so the
/// old server's key bytes can't leak onto a different endpoint.
/// The row's `connect_changed` handler re-dispatches
/// `SetRtlTcpClientConfig { auth_key: None, .. }` so DSP state
/// tracks the invalidation in lockstep with the UI.
pub(in crate::window::source) fn invalidate_rtl_tcp_active_server_on_edit(
    app_state: &Rc<AppState>,
    hostname_row: &adw::EntryRow,
    port_row: &adw::SpinRow,
    auth_key_row: &adw::PasswordEntryRow,
) {
    let hostname = hostname_row.text().to_string();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let port = port_row.value() as u16;
    let current_key = format!("{hostname}:{port}");
    let should_clear = {
        let cached = app_state.rtl_tcp_active_server.borrow();
        !cached.is_empty() && *cached != current_key
    };
    if should_clear {
        app_state.rtl_tcp_active_server.borrow_mut().clear();
        auth_key_row.set_visible(false);
        auth_key_row.set_text("");
    }
}

/// Save the current Server-key-row text to the keyring under
/// the active `rtl_tcp` server's `host:port`. Called on a
/// successful Connected following AuthRequired / AuthFailed.
/// Empty text → clear the saved entry instead of writing empty
/// bytes; invalid hex → log + skip (the live connection
/// obviously accepted the text, but our keyring round-trip
/// demands valid hex). Per #396.
#[allow(
    clippy::doc_markdown,
    reason = "Connected / AuthRequired / AuthFailed are enum variants"
)]
pub(super) fn save_current_auth_key_for_active_server(
    app_state: &Rc<AppState>,
    auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
) {
    let active = app_state.rtl_tcp_active_server.borrow().clone();
    if active.is_empty() {
        return;
    }
    let Some((host, port_str)) = active.rsplit_once(':') else {
        return;
    };
    let Ok(port) = port_str.parse::<u16>() else {
        return;
    };
    let Some(row) = auth_key_row_weak.upgrade() else {
        return;
    };
    let text = row.text().to_string();
    if text.is_empty() {
        // User explicitly cleared the field BEFORE this connect
        // succeeded — mirror that intent in the keyring by
        // deleting the saved entry. Pre-CodeRabbit round 3 on
        // PR #408 this branch returned early with a stale
        // "nothing to save" comment, so clearing the row and
        // reconnecting left the old bytes in the keyring and
        // `apply_rtl_tcp_connect` would preload them on the
        // next discovery / favorites / last-connected path,
        // silently undoing the user's clear.
        if let Err(e) = clear_client_auth_key_from_keyring(host, port) {
            tracing::warn!(
                server = %active,
                %e,
                "rtl_tcp: client auth key keyring clear failed (empty row)"
            );
        }
        return;
    }
    let Some(bytes) = crate::sidebar::server_panel::auth_key_from_hex(&text) else {
        tracing::warn!(
            server = %active,
            "rtl_tcp: client auth key hex is invalid — skipping keyring save"
        );
        return;
    };
    if let Err(e) = save_client_auth_key_to_keyring(host, port, &bytes) {
        tracing::warn!(
            server = %active,
            %e,
            "rtl_tcp: client auth key keyring save failed"
        );
    } else {
        tracing::info!(
            server = %active,
            "rtl_tcp: client auth key saved to keyring for next reconnect"
        );
    }
}

pub(in crate::window) fn apply_rtl_tcp_connection_state(
    status_row: &adw::ActionRow,
    disconnect_button: &gtk4::Button,
    retry_button: &gtk4::Button,
    state: &sdr_types::RtlTcpConnectionState,
) {
    use sdr_types::RtlTcpConnectionState;
    status_row.set_subtitle(&sidebar::source_panel::format_rtl_tcp_state(state));
    let is_active = matches!(
        state,
        RtlTcpConnectionState::Connecting
            | RtlTcpConnectionState::Connected { .. }
            | RtlTcpConnectionState::Retrying { .. }
    );
    // "Retry now" is only meaningful when there's an active source
    // to short-circuit out of its backoff wait — Retrying (most
    // common) or any of the four terminal states (Failed +
    // role-denials added in #396). After an explicit Disconnect
    // the controller drops `state.source`, and
    // `UiToDsp::RetryRtlTcpNow` is a no-op (it checks
    // `state.source.as_mut()` → None → early return). Leaving the
    // button visibly enabled in that state misleads the user into
    // thinking they can reconnect in one click; the correct
    // post-Disconnect path is to press Play.
    let can_retry_now = matches!(
        state,
        RtlTcpConnectionState::Retrying { .. } | RtlTcpConnectionState::Failed { .. }
    ) || state.needs_user_action();
    disconnect_button.set_sensitive(is_active);
    retry_button.set_sensitive(can_retry_now);
}

/// `ControllerBusy` arm of [`handle_rtl_tcp_state_toast`]. Split out per the
/// 50-NLOC gate (#817).
fn on_rtl_tcp_controller_busy(
    app_state: &Rc<AppState>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    role_row_weak: &glib::WeakRef<adw::ComboRow>,
    pending_controller_busy_toasts: &Rc<RefCell<Vec<glib::WeakRef<adw::Toast>>>>,
) {
    // Toast with two action buttons: "Connect as
    // Listener" flips the role combo (its change handler
    // re-dispatches SetRtlTcpClientConfig) and fires a
    // normal retry; "Take control" dispatches the one-shot
    // `RetryRtlTcpWithTakeover` message which rebuilds
    // the source with `request_takeover = true` on the
    // hello.
    let Some(overlay) = toast_overlay_weak.upgrade() else {
        return;
    };
    // Before creating the new pair, sweep any still-
    // live toasts from a prior `ControllerBusy` entry
    // (e.g. the user hit `Retry` without clicking either
    // action, and the server is still busy on the
    // rebound). Otherwise the overlay would stack two
    // pairs, and dismissing one pair via the cross-
    // dismiss helpers below would leave the other pair
    // orphaned. Per `CodeRabbit` round 11 on PR #408.
    dismiss_stale_controller_busy_toasts(pending_controller_busy_toasts);

    let toast = adw::Toast::builder()
        .title("Controller slot is occupied on this server.")
        .timeout(TOAST_TIMEOUT_PERSISTENT)
        .build();
    let listen_toast = adw::Toast::builder()
        .title("Or connect as Listener (read-only).")
        .timeout(TOAST_TIMEOUT_PERSISTENT)
        .build();
    // Cross-dismiss: clicking either action dismisses
    // BOTH toasts, so a stale sibling action can't fire
    // later against a session that's already resolved.
    // `WeakRef` rather than strong clones — the toasts
    // hand out their own strong refs to the overlay
    // internally, and we only need to reach the sibling
    // when it's still live.
    let toast_weak = toast.downgrade();
    let listen_toast_weak = listen_toast.downgrade();

    // Track the two action buttons as separate signals.
    // AdwToast supports a single primary action via
    // `set_button_label` + `connect_button_clicked`; the
    // "Take control" action lands there, and the
    // "Connect as Listener" option lives in the
    // sibling toast below so users still see both
    // choices.
    toast.set_button_label(Some("Take control"));
    let state_for_takeover = Rc::clone(app_state);
    let listen_weak_for_takeover = listen_toast_weak.clone();
    toast.connect_button_clicked(move |t| {
        state_for_takeover.send_dsp(UiToDsp::RetryRtlTcpWithTakeover);
        t.dismiss();
        if let Some(sibling) = listen_weak_for_takeover.upgrade() {
            sibling.dismiss();
        }
    });
    overlay.add_toast(toast);

    wire_listener_fallback_toast(
        app_state,
        role_row_weak,
        &overlay,
        &listen_toast,
        &toast_weak,
    );

    // Record the pair so the non-ControllerBusy state
    // transition at the top of this function can sweep
    // them if the server resolves itself without user
    // interaction.
    {
        let mut pending = pending_controller_busy_toasts.borrow_mut();
        pending.push(toast_weak);
        pending.push(listen_toast_weak);
    }
}

/// `AuthRequired` arm of [`handle_rtl_tcp_state_toast`]. Split out per the
/// 50-NLOC gate (#817).
fn on_rtl_tcp_auth_required(
    app_state: &Rc<AppState>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
    hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    port_row_weak: &glib::WeakRef<adw::SpinRow>,
) {
    // Remember the active server so a subsequent
    // successful Connected can save the user-entered
    // key to the right keyring entry.
    record_active_rtl_tcp_server(app_state, hostname_row_weak, port_row_weak);
    // Reveal + focus the Server key field so the user
    // can enter the key.
    if let Some(row) = auth_key_row_weak.upgrade() {
        row.set_visible(true);
        row.grab_focus();
    }
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        let toast = adw::Toast::builder()
            .title("Server requires an authentication key.")
            .timeout(TOAST_TIMEOUT_SHORT_SECS)
            .build();
        overlay.add_toast(toast);
    }
}

/// `AuthFailed` arm of [`handle_rtl_tcp_state_toast`]. Split out per the
/// 50-NLOC gate (#817).
fn on_rtl_tcp_auth_failed(
    app_state: &Rc<AppState>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
    hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    port_row_weak: &glib::WeakRef<adw::SpinRow>,
) {
    record_active_rtl_tcp_server(app_state, hostname_row_weak, port_row_weak);
    // Clear the saved per-server key from the keyring
    // too — not just the widget. Pre-CodeRabbit round 2
    // on PR #408 only `row.set_text("")` was called, so
    // the keyring entry survived the rejection and the
    // next discovery / favorites / Play-restart path
    // would auto-load the same rejected bytes into the
    // row via `apply_rtl_tcp_connect` / the startup
    // restore, silently bouncing the user straight back
    // into `AuthFailed`. Now we delete the saved key
    // whenever the server explicitly rejects it; the
    // user has to re-enter (or paste the new) key on
    // the next attempt, which is the only recovery path
    // from a rotated server key anyway. Per issue #396.
    let active = app_state.rtl_tcp_active_server.borrow().clone();
    if let Some((host, port_str)) = active.rsplit_once(':')
        && let Ok(port) = port_str.parse::<u16>()
        && let Err(e) = clear_client_auth_key_from_keyring(host, port)
    {
        tracing::warn!(
            server = %active,
            %e,
            "rtl_tcp: client auth key keyring clear on AuthFailed failed (non-fatal)"
        );
    }
    if let Some(row) = auth_key_row_weak.upgrade() {
        row.set_visible(true);
        row.grab_focus();
        // Clear the entered value so the user doesn't
        // re-submit the same wrong key by reflex on the
        // next Retry.
        row.set_text("");
    }
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        let toast = adw::Toast::builder()
            .title("Key rejected. Check with the server owner.")
            .timeout(TOAST_TIMEOUT_SHORT_SECS)
            .build();
        overlay.add_toast(toast);
    }
}

/// `Connected { .. }` arm of [`handle_rtl_tcp_state_toast`]. Split out per the
/// 50-NLOC gate (#817).
fn on_rtl_tcp_connected(
    prev_disc: u8,
    app_state: &Rc<AppState>,
    auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
    hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    port_row_weak: &glib::WeakRef<adw::SpinRow>,
) {
    use crate::state::{
        RTL_TCP_STATE_DISC_AUTH_FAILED, RTL_TCP_STATE_DISC_AUTH_REQUIRED,
        RTL_TCP_STATE_DISC_CONNECTING, RTL_TCP_STATE_DISC_CONTROLLER_BUSY,
    };

    // Save the user-entered key to the per-server
    // keyring so subsequent reconnects auto-use it.
    // Fires on the edge from any of:
    //
    // - `AuthRequired` / `AuthFailed` — user typed a
    //   key in response to a denial toast;
    // - `Connecting` — user had auth configured up
    //   front (server advertised `auth_required` via
    //   mDNS, key was entered before the first
    //   connect, and the handshake succeeded in a
    //   single `Connecting → Connected` hop);
    // - `ControllerBusy` — user entered a key before
    //   the first connect, server denied with
    //   `ControllerBusy`, and the user's subsequent
    //   Take-control / Listener retry (via
    //   `RetryRtlTcpWithTakeover` or `RetryRtlTcpNow`)
    //   succeeded. Added per `CodeRabbit` round 12 on
    //   PR #408 — without this branch an auth-required
    //   server that's also busy on the first attempt
    //   would accept the key on the takeover reconnect
    //   but never persist it to the keyring.
    //
    // Pre-round-1 on PR #408 only the auth-denial arms
    // triggered the save, so up-front keys never hit the
    // keyring and the user had to re-type them on every
    // reconnect. `save_current_auth_key_for_active_
    // server` is a no-op when the key row is empty, so
    // this is safe to trigger on every qualifying edge
    // even if the server doesn't require auth. Call
    // `record_active_rtl_tcp_server` first so the save-
    // path sees the right `host:port` even when the
    // user never hit an auth-denial arm (which is what
    // previously set the cache).
    if prev_disc == RTL_TCP_STATE_DISC_CONNECTING
        || prev_disc == RTL_TCP_STATE_DISC_CONTROLLER_BUSY
        || prev_disc == RTL_TCP_STATE_DISC_AUTH_REQUIRED
        || prev_disc == RTL_TCP_STATE_DISC_AUTH_FAILED
    {
        record_active_rtl_tcp_server(app_state, hostname_row_weak, port_row_weak);
        save_current_auth_key_for_active_server(app_state, auth_key_row_weak);
    }
}

/// Second `ControllerBusy` toast offering the Listen fallback (`AdwToast` exposes only one action button).
/// Split out per the 50-NLOC gate (#817).
fn wire_listener_fallback_toast(
    app_state: &Rc<AppState>,
    role_row_weak: &glib::WeakRef<adw::ComboRow>,
    overlay: &adw::ToastOverlay,
    listen_toast: &adw::Toast,
    toast_weak: &glib::WeakRef<adw::Toast>,
) {
    // Second toast offering the Listen fallback. Two
    // separate toasts beats a single one because AdwToast
    // exposes only one action button — splitting the two
    // paths keeps both discoverable.
    listen_toast.set_button_label(Some("Connect as Listener"));
    let state_for_listen = Rc::clone(app_state);
    let role_row_for_listen = role_row_weak.clone();
    let toast_weak_for_listen = toast_weak.clone();
    listen_toast.connect_button_clicked(move |t| {
        if let Some(role_row) = role_row_for_listen.upgrade() {
            // Flipping the combo to Listen fires its
            // `selected-notify` handler which dispatches
            // `SetRtlTcpClientConfig` with the new role.
            // Follow with RetryRtlTcpNow so the user
            // doesn't have to click Retry themselves.
            role_row.set_selected(crate::sidebar::source_panel::RTL_TCP_ROLE_LISTEN_IDX);
        }
        state_for_listen.send_dsp(UiToDsp::RetryRtlTcpNow);
        t.dismiss();
        if let Some(sibling) = toast_weak_for_listen.upgrade() {
            sibling.dismiss();
        }
    });
    overlay.add_toast(listen_toast.clone());
}
