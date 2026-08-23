use super::*;

#[test]
fn register_with_role_grants_first_control_client() {
    // No prior clients → a Control request fits. Verify the
    // slot lands in the registry and `lifetime_accepted`
    // bumps.
    let reg = ClientRegistry::new();
    let slot = role_test_slot(&reg, ROLE_TEST_SINGLE_CTRL_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(slot, TEST_LISTENER_CAP, false),
        RoleDecision::Granted
    );
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.lifetime_accepted(), 1);
}

#[test]
fn register_with_role_denies_second_control_as_controller_busy() {
    // First Control grants; second Control sees the first
    // live and gets ControllerBusy without consuming a
    // lifetime_accepted slot (denials don't count).
    let reg = ClientRegistry::new();
    let first = role_test_slot(&reg, ROLE_TEST_BUSY_FIRST_CTRL_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(first, TEST_LISTENER_CAP, false),
        RoleDecision::Granted
    );
    let second = role_test_slot(&reg, ROLE_TEST_BUSY_SECOND_CTRL_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(second, TEST_LISTENER_CAP, false),
        RoleDecision::ControllerBusy
    );
    // Registry still only has the first — denial must not
    // push.
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.lifetime_accepted(), 1);
}

#[test]
fn register_with_role_grants_second_control_after_first_disconnects() {
    // Contract: the "live" check treats disconnected slots as
    // absent (so a freshly-dropping Control client opens the
    // slot before the next prune tick lands). This matters
    // for the natural "user 1 disconnects, user 2 reconnects"
    // flow — we shouldn't require them to wait ~2.5s for
    // prune_disconnected to run.
    let reg = ClientRegistry::new();
    let first = role_test_slot(&reg, ROLE_TEST_DISCONNECT_FIRST_CTRL_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(first.clone(), TEST_LISTENER_CAP, false),
        RoleDecision::Granted
    );
    first.mark_disconnected();
    let second = role_test_slot(&reg, ROLE_TEST_DISCONNECT_SECOND_CTRL_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(second, TEST_LISTENER_CAP, false),
        RoleDecision::Granted
    );
    assert_eq!(reg.lifetime_accepted(), 2);
}

#[test]
fn register_with_role_admits_listeners_up_to_cap() {
    // Coexistence test: one Control + up-to-cap Listeners all
    // live simultaneously. Shape: Control doesn't contribute
    // to the listener count.
    let reg = ClientRegistry::new();
    let ctrl = role_test_slot(&reg, ROLE_TEST_ADMIT_CTRL_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(ctrl, TEST_LISTENER_CAP, false),
        RoleDecision::Granted
    );
    for i in 0..TEST_LISTENER_CAP {
        let listener = role_test_slot(
            &reg,
            listener_port(ROLE_TEST_ADMIT_LISTENER_BASE_PORT, i),
            Role::Listen,
        );
        assert_eq!(
            reg.register_with_role(listener, TEST_LISTENER_CAP, false),
            RoleDecision::Granted,
            "listener {i} should fit under cap {TEST_LISTENER_CAP}"
        );
    }
    assert_eq!(reg.len(), 1 + TEST_LISTENER_CAP);
}

#[test]
fn register_with_role_denies_listen_past_cap() {
    // Fill the cap, then attempt one more → ListenerCapReached.
    let reg = ClientRegistry::new();
    for i in 0..TEST_LISTENER_CAP {
        let listener = role_test_slot(
            &reg,
            listener_port(ROLE_TEST_CAP_LISTENER_BASE_PORT, i),
            Role::Listen,
        );
        assert_eq!(
            reg.register_with_role(listener, TEST_LISTENER_CAP, false),
            RoleDecision::Granted
        );
    }
    let overflow = role_test_slot(&reg, ROLE_TEST_CAP_OVERFLOW_PORT, Role::Listen);
    assert_eq!(
        reg.register_with_role(overflow, TEST_LISTENER_CAP, false),
        RoleDecision::ListenerCapReached
    );
    // Denial must not push into slots.
    assert_eq!(reg.len(), TEST_LISTENER_CAP);
    // Denials don't count toward lifetime_accepted.
    assert_eq!(
        reg.lifetime_accepted(),
        u64::try_from(TEST_LISTENER_CAP).expect("listener cap fits u64")
    );
}

#[test]
fn unwind_admission_removes_slot_and_decrements_counter() {
    // **Regression guard for `CodeRabbit` round 1 on PR #403.**
    // Post-register setup failures (try_clone, header write,
    // worker spawn) must roll back the admission so
    // `lifetime_accepted` doesn't inflate with sessions that
    // never served a byte. Contract:
    //   - slot is removed from the registry
    //   - `lifetime_accepted` decrements 1:1 with the prior
    //     register_with_role bump
    //   - slot's `disconnected` flag is set (broadcaster
    //     stops fanning immediately, before the slot-list
    //     remove takes effect)
    //   - double-call is idempotent (returns false the
    //     second time; counter doesn't underflow)
    let reg = ClientRegistry::new();
    let slot = role_test_slot(&reg, ROLE_TEST_SINGLE_CTRL_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(slot.clone(), TEST_LISTENER_CAP, false),
        RoleDecision::Granted
    );
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.lifetime_accepted(), 1);

    assert!(
        reg.unwind_admission(&slot),
        "first unwind should find the slot"
    );
    assert_eq!(reg.len(), 0, "unwound slot must not remain in the registry");
    assert_eq!(
        reg.lifetime_accepted(),
        0,
        "unwind should cancel the register_with_role bump"
    );
    assert!(
        slot.is_disconnected(),
        "unwind marks the slot dead so the broadcaster stops fanning"
    );

    // Second call: slot is gone → returns false, counter
    // stays at zero (no underflow).
    assert!(
        !reg.unwind_admission(&slot),
        "second unwind returns false because the slot is already gone"
    );
    assert_eq!(
        reg.lifetime_accepted(),
        0,
        "double-unwind must not underflow lifetime_accepted"
    );
}

#[test]
fn register_with_role_counts_only_live_listeners_for_cap() {
    // A disconnected Listener frees a listener slot
    // immediately (same reasoning as the Control disconnect
    // test above). Fills cap, flips one to disconnected,
    // verifies the next Listen fits.
    let reg = ClientRegistry::new();
    let mut listeners = Vec::new();
    for i in 0..TEST_LISTENER_CAP {
        let listener = role_test_slot(
            &reg,
            listener_port(ROLE_TEST_LIVE_LISTENER_BASE_PORT, i),
            Role::Listen,
        );
        assert_eq!(
            reg.register_with_role(listener.clone(), TEST_LISTENER_CAP, false),
            RoleDecision::Granted
        );
        listeners.push(listener);
    }
    // Cap is full — verify.
    let denied = role_test_slot(&reg, ROLE_TEST_LIVE_DENIED_PORT, Role::Listen);
    assert_eq!(
        reg.register_with_role(denied, TEST_LISTENER_CAP, false),
        RoleDecision::ListenerCapReached
    );
    // Flip one listener to disconnected and retry.
    listeners[0].mark_disconnected();
    let replacement = role_test_slot(&reg, ROLE_TEST_LIVE_REPLACEMENT_PORT, Role::Listen);
    assert_eq!(
        reg.register_with_role(replacement, TEST_LISTENER_CAP, false),
        RoleDecision::Granted
    );
}
