use super::*;

#[test]
fn register_with_role_takeover_displaces_existing_controller() {
    // **Regression test for #393.** Core takeover contract:
    // Control client A is live, client B requests Control
    // with `request_takeover = true`. Expected:
    //   - B is admitted (GrantedViaTakeover carries A's id)
    //   - A is marked disconnected (the broadcaster stops
    //     fanning to it, writer/command workers exit on their
    //     next tick with a clean TCP FIN)
    //   - `lifetime_accepted` reflects both admissions (A's
    //     kick doesn't decrement; it was a real session)
    let (reg, slot_a) = registry_with_controller(TAKEOVER_TEST_ORIG_CTRL_PORT);
    let a_id = slot_a.id;

    let slot_b = role_test_slot(&reg, TAKEOVER_TEST_NEW_CTRL_PORT, Role::Control);
    let b_id = slot_b.id;
    assert_eq!(
        reg.register_with_role(slot_b.clone(), TEST_LISTENER_CAP, true),
        RoleDecision::GrantedViaTakeover { displaced_id: a_id }
    );
    // The swap is two-phase (#710): the incumbent stays live
    // until the newcomer's workers are up and the caller commits.
    assert!(!slot_a.is_disconnected(), "not displaced before commit");
    assert!(reg.commit_takeover(b_id, a_id));

    // A is still in the registry (stats visible to UI) but
    // flagged disconnected.
    assert!(
        slot_a.is_disconnected(),
        "displaced controller should be marked disconnected"
    );
    // B is live and holds the Control slot from the registry's
    // point of view.
    assert!(
        !slot_b.is_disconnected(),
        "new controller should be alive after takeover"
    );
    // Two admissions in lifetime_accepted — A's kick doesn't
    // subtract, it was a real session.
    assert_eq!(reg.lifetime_accepted(), 2);
    // The live-snapshot reflects only the new controller.
    let snapshot = reg.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].id, b_id);
    assert_eq!(snapshot[0].role, Role::Control);
}

#[test]
fn register_with_role_takeover_is_noop_when_slot_is_free() {
    // Pure takeover flag with no existing controller → just
    // `Granted` (no displacement metadata). The flag is
    // harmless in the no-conflict case; it's the "please kick
    // whoever's there" hint, not a hard requirement that
    // someone BE there.
    let reg = ClientRegistry::new();
    let slot = role_test_slot(&reg, TAKEOVER_TEST_NO_CONFLICT_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(slot, TEST_LISTENER_CAP, true),
        RoleDecision::Granted,
        "takeover flag with no conflict must resolve to plain Granted"
    );
    assert_eq!(reg.lifetime_accepted(), 1);
}

#[test]
fn register_with_role_takeover_false_still_denies_busy_controller() {
    // Regression guard: the #392 ControllerBusy semantics
    // must stay intact when `request_takeover = false`. A
    // Control client whose hello has the takeover flag
    // clear gets denied exactly as before — the #393 branch
    // only activates on an explicit `true` request.
    let (reg, slot_a) = registry_with_controller(TAKEOVER_TEST_DENIED_ORIG_PORT);
    let slot_b = role_test_slot(&reg, TAKEOVER_TEST_DENIED_NEW_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(slot_b, TEST_LISTENER_CAP, false),
        RoleDecision::ControllerBusy
    );
    // A stays alive — no unintended displacement when the
    // takeover flag is clear.
    assert!(!slot_a.is_disconnected());
    // Denial doesn't bump lifetime_accepted.
    assert_eq!(reg.lifetime_accepted(), 1);
}

#[test]
fn register_with_role_takeover_leaves_listeners_alone() {
    // Integration: listener + Control(A), then B takes over
    // from A. Listener must stay connected through the
    // takeover — the fan-out contract says listeners are
    // isolated from controller churn. Without this, a
    // takeover event would drop listeners too, turning every
    // "kick the zombie controller" action into a reconnect
    // storm across every passive viewer.
    let reg = ClientRegistry::new();
    let listener = role_test_slot(&reg, TAKEOVER_TEST_LISTENER_PORT, Role::Listen);
    assert_eq!(
        reg.register_with_role(listener.clone(), TEST_LISTENER_CAP, false),
        RoleDecision::Granted
    );
    let ctrl_a = role_test_slot(&reg, TAKEOVER_TEST_LISTENER_ORIG_CTRL_PORT, Role::Control);
    let a_id = ctrl_a.id;
    assert_eq!(
        reg.register_with_role(ctrl_a.clone(), TEST_LISTENER_CAP, false),
        RoleDecision::Granted
    );
    let ctrl_b = role_test_slot(
        &reg,
        TAKEOVER_TEST_LISTENER_TAKEOVER_CTRL_PORT,
        Role::Control,
    );
    let b_id = ctrl_b.id;
    assert_eq!(
        reg.register_with_role(ctrl_b, TEST_LISTENER_CAP, true),
        RoleDecision::GrantedViaTakeover { displaced_id: a_id }
    );
    assert!(reg.commit_takeover(b_id, a_id));
    // Listener survived unscathed.
    assert!(
        !listener.is_disconnected(),
        "takeover must not mark listeners disconnected"
    );
    // Old Control was displaced.
    assert!(ctrl_a.is_disconnected());
    // Live snapshot has the listener + the new controller
    // (two live slots; A is pruned out of the live view by
    // the is_disconnected filter).
    let snapshot = reg.snapshot();
    assert_eq!(snapshot.len(), 2);
}

/// #710: a takeover that is unwound before commit (the newcomer
/// dropped during setup) leaves the incumbent controller in
/// place — a remote client cannot knock the live controller off
/// the dongle just by asking for takeover and hanging up.
#[test]
fn takeover_unwound_before_commit_keeps_the_incumbent() {
    let (reg, slot_a, slot_b) = pending_takeover_scenario();
    let a_id = slot_a.id;
    assert!(reg.unwind_admission(&slot_b));
    assert!(!slot_a.is_disconnected(), "incumbent untouched");
    // A is still the controller: a plain Control request is busy.
    let slot_c = role_test_slot(&reg, TAKEOVER_TEST_NO_CONFLICT_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(slot_c, TEST_LISTENER_CAP, false),
        RoleDecision::ControllerBusy
    );
    // Committing a takeover whose newcomer is gone is a no-op.
    assert!(!reg.commit_takeover(slot_b.id, a_id));
    assert!(!slot_a.is_disconnected());
}

/// #808: while newcomer B's takeover of A is admitted but not yet
/// committed, a competing takeover C is denied — otherwise both
/// would commit against A and leave two live controllers.
#[test]
fn competing_takeover_is_denied_while_one_is_pending() {
    let (reg, slot_a, slot_b) = pending_takeover_scenario();
    let a_id = slot_a.id;
    let slot_c = role_test_slot(&reg, TAKEOVER_TEST_NO_CONFLICT_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(slot_c.clone(), TEST_LISTENER_CAP, true),
        RoleDecision::ControllerBusy,
        "a second takeover must wait for the pending one"
    );
    // Once B commits, a takeover of B is possible again.
    assert!(reg.commit_takeover(slot_b.id, a_id));
    assert_eq!(
        reg.register_with_role(slot_c, TEST_LISTENER_CAP, true),
        RoleDecision::GrantedViaTakeover {
            displaced_id: slot_b.id
        }
    );
    let live_controllers = reg
        .snapshot()
        .iter()
        .filter(|s| s.role == Role::Control)
        .count();
    assert_eq!(live_controllers, 2, "B live + C pending, A displaced");
}

/// #808: an unwound pending takeover frees the reservation.
#[test]
fn unwound_pending_takeover_frees_the_reservation() {
    let (reg, slot_a, slot_b) = pending_takeover_scenario();
    let a_id = slot_a.id;
    assert!(reg.unwind_admission(&slot_b));
    let slot_c = role_test_slot(&reg, TAKEOVER_TEST_NO_CONFLICT_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(slot_c, TEST_LISTENER_CAP, true),
        RoleDecision::GrantedViaTakeover { displaced_id: a_id }
    );
}

/// #808: `commit_takeover` validates the pair — a stale incumbent
/// id or a newcomer that is not a live Control slot is a no-op.
#[test]
fn commit_takeover_rejects_a_stale_pair() {
    const STALE_ID: ClientId = 9_999;
    let (reg, slot_a, slot_b) = pending_takeover_scenario();
    let a_id = slot_a.id;
    assert!(
        !reg.commit_takeover(slot_b.id, STALE_ID),
        "unknown incumbent"
    );
    assert!(!reg.commit_takeover(STALE_ID, a_id), "unknown newcomer");
    assert!(!reg.commit_takeover(a_id, a_id), "newcomer must differ");
    // Reversed pair: A was not admitted to replace B (CR on PR #809).
    assert!(!reg.commit_takeover(a_id, slot_b.id), "reversed pair");
    assert!(!slot_b.is_disconnected(), "the newcomer is untouched");
    assert!(!slot_a.is_disconnected());
    assert!(reg.commit_takeover(slot_b.id, a_id));
    assert!(slot_a.is_disconnected());
    assert!(!reg.commit_takeover(slot_b.id, a_id), "already committed");
}
