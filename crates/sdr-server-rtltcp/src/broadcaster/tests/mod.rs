use super::*;

/// Convenience constructor for tests that don't care about the
/// TCP peer — picks a deterministic placeholder loopback address.
fn test_peer(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

// ============================================================
// Test fixture constants (CodeRabbit round 3 on PR #402).
// Extracted so each test's intent reads at a glance — a
// "1234" port on its own is noise; `TEST_PORT_GENERIC_A`
// plus a bounds docstring is self-documenting.
// ============================================================

/// Generic test port A for tests that register one slot and
/// don't care about peer-address distinctness — any
/// non-privileged port works.
const TEST_PORT_GENERIC_A: u16 = 1_234;
/// Generic test port B for tests that register a SECOND slot
/// and want peer addresses distinct from `TEST_PORT_GENERIC_A`
/// so snapshot assertions can tell them apart.
const TEST_PORT_GENERIC_B: u16 = 1_235;
/// Third generic port used by tests that register slot A
/// and slot B with disjoint port values (1 / 2 are fine
/// since we're just disambiguating addresses, not binding).
const TEST_PORT_FIRST: u16 = 1;
/// Fourth generic port, disjoint from `TEST_PORT_FIRST`.
const TEST_PORT_SECOND: u16 = 2;
/// Port for the `snapshot_reflects_registered_slots_with_stats`
/// test — picked distinct from the others so a cross-test
/// regression leaks clearly in the snapshot assertion.
const TEST_PORT_SNAPSHOT: u16 = 4_242;

/// Small channel depth that exercises the `Full` path without
/// needing to broadcast 500 chunks.
const TEST_CHANNEL_DEPTH_SMALL: usize = 2;
/// Moderate channel depth used by tests where the "fast
/// client" must drain all broadcasts without any drops.
const TEST_CHANNEL_DEPTH_STANDARD: usize = 4;
/// Generous channel depth for the "fast neighbor" side of
/// the full-channel drop-isolation test — must never fill.
const TEST_CHANNEL_DEPTH_GENEROUS: usize = 16;

/// Kept as an alias to `TEST_CHANNEL_DEPTH_SMALL` for the one
/// call site (`broadcast_full_channel_counts_drop_for_that_client_only`)
/// that reads better with the original name.
const TEST_CHANNEL_DEPTH: usize = TEST_CHANNEL_DEPTH_SMALL;

// ============================================================
// register_with_role decision matrix (#392)
//
// These cover the atomic role-gate contract at the registry
// level — `spawn_client_workers` is the sole production caller
// and threads the decision directly into the `ServerExtension`
// response block, so pinning the decision semantics here is
// the durable guard against future regressions in either the
// role gate or the listener cap.
// ============================================================

/// Test listener cap shared across the role-gate tests. Small
/// enough to hit the cap quickly without inflating the fixture.
const TEST_LISTENER_CAP: usize = 2;

// ------------------------------------------------------------
// Named peer ports for the role-gate tests. Each test uses a
// disjoint 20_0XX / 20_1XX etc. block so log output points
// directly at the test that emitted the peer. Extracted per
// `CodeRabbit` round 1 on PR #403 — raw `20_001` / `20_010`
// literals aren't grep-friendly and drift naming conventions
// away from the rest of this module's `TEST_*_PORT` pattern.
// ------------------------------------------------------------

/// Single-Control happy path: port for the first (and only)
/// admitted client.
const ROLE_TEST_SINGLE_CTRL_PORT: u16 = 20_001;
/// `denies_second_control_as_controller_busy`: first Control
/// client (admitted) and second (denied).
const ROLE_TEST_BUSY_FIRST_CTRL_PORT: u16 = 20_010;
const ROLE_TEST_BUSY_SECOND_CTRL_PORT: u16 = 20_011;
/// `grants_second_control_after_first_disconnects`: original
/// Control then the takeover-via-disconnect successor.
const ROLE_TEST_DISCONNECT_FIRST_CTRL_PORT: u16 = 20_020;
const ROLE_TEST_DISCONNECT_SECOND_CTRL_PORT: u16 = 20_021;
/// `admits_listeners_up_to_cap`: Control base port + offset
/// per listener. The listener loop adds `i` to the base, so
/// the reserved block is `20_031..20_031 + TEST_LISTENER_CAP`.
const ROLE_TEST_ADMIT_CTRL_PORT: u16 = 20_030;
const ROLE_TEST_ADMIT_LISTENER_BASE_PORT: u16 = 20_031;
/// `denies_listen_past_cap`: listener-fill block + the
/// overflow peer that gets denied.
const ROLE_TEST_CAP_LISTENER_BASE_PORT: u16 = 20_040;
const ROLE_TEST_CAP_OVERFLOW_PORT: u16 = 20_049;
/// `counts_only_live_listeners_for_cap`: listener-fill block,
/// first overflow attempt that should be denied, and the
/// replacement that succeeds after a slot is freed.
const ROLE_TEST_LIVE_LISTENER_BASE_PORT: u16 = 20_050;
const ROLE_TEST_LIVE_DENIED_PORT: u16 = 20_058;
const ROLE_TEST_LIVE_REPLACEMENT_PORT: u16 = 20_059;

/// Convenience: compute the Nth listener port in a test that
/// stamps a contiguous block starting at `base`.
fn listener_port(base: u16, offset: usize) -> u16 {
    base.checked_add(u16::try_from(offset).expect("offset fits u16"))
        .expect("listener port fits u16")
}

/// Build a slot with the requested role for the decision
/// tests. Channel depth doesn't matter (nothing broadcasts
/// here); `TEST_CHANNEL_DEPTH_SMALL` keeps allocation cheap.
fn role_test_slot(reg: &ClientRegistry, port: u16, role: Role) -> Arc<ClientSlot> {
    let (slot, _rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(port),
        Codec::None,
        role,
        TEST_CHANNEL_DEPTH_SMALL,
    );
    slot
}

// ------------------------------------------------------------
// Takeover handshake tests (#393).
//
// Shape: `register_with_role(slot, cap, request_takeover)`.
// When the Control slot is busy and the new client sets
// `request_takeover = true`, the existing controller is
// marked disconnected + the new client is admitted as Control.
// Vanilla clients never exercise the takeover path (they
// can't set the flag); RTLX clients request it explicitly via
// `ClientHello::flags` bit 0.
// ------------------------------------------------------------

/// `takeover` peer ports — each test uses a disjoint block so
/// log output points at the specific test that stamped the
/// peer.
const TAKEOVER_TEST_ORIG_CTRL_PORT: u16 = 20_100;
const TAKEOVER_TEST_NEW_CTRL_PORT: u16 = 20_101;
const TAKEOVER_TEST_NO_CONFLICT_PORT: u16 = 20_110;
const TAKEOVER_TEST_DENIED_ORIG_PORT: u16 = 20_120;
const TAKEOVER_TEST_DENIED_NEW_PORT: u16 = 20_121;
const TAKEOVER_TEST_LISTENER_PORT: u16 = 20_130;
const TAKEOVER_TEST_LISTENER_TAKEOVER_CTRL_PORT: u16 = 20_131;

/// Registry with one live Control client on `port` (the common
/// takeover-test setup). Returns the registry and the incumbent.
fn registry_with_controller(port: u16) -> (ClientRegistry, Arc<ClientSlot>) {
    let reg = ClientRegistry::new();
    let slot = role_test_slot(&reg, port, Role::Control);
    assert_eq!(
        reg.register_with_role(slot.clone(), TEST_LISTENER_CAP, false),
        RoleDecision::Granted
    );
    (reg, slot)
}

/// Incumbent A live, newcomer B admitted via takeover against A
/// but not yet committed — the state the pending-takeover tests
/// start from.
fn pending_takeover_scenario() -> (ClientRegistry, Arc<ClientSlot>, Arc<ClientSlot>) {
    let (reg, slot_a) = registry_with_controller(TAKEOVER_TEST_ORIG_CTRL_PORT);
    let slot_b = role_test_slot(&reg, TAKEOVER_TEST_NEW_CTRL_PORT, Role::Control);
    assert_eq!(
        reg.register_with_role(slot_b.clone(), TEST_LISTENER_CAP, true),
        RoleDecision::GrantedViaTakeover {
            displaced_id: slot_a.id
        }
    );
    (reg, slot_a, slot_b)
}

mod registry;
mod roles;
mod takeover;
