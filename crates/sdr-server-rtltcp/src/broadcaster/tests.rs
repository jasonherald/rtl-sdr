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

#[test]
fn allocate_id_is_monotonic() {
    let reg = ClientRegistry::new();
    let a = reg.allocate_id();
    let b = reg.allocate_id();
    let c = reg.allocate_id();
    assert_eq!((a, b, c), (0, 1, 2));
}

#[test]
fn register_grows_len_and_lifetime_counter() {
    let reg = ClientRegistry::new();
    assert!(reg.is_empty());

    let (slot, _rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_GENERIC_A),
        Codec::None,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    reg.register(slot);
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.lifetime_accepted(), 1);

    let (slot2, _rx2) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_GENERIC_B),
        Codec::Lz4,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    reg.register(slot2);
    assert_eq!(reg.len(), 2);
    assert_eq!(reg.lifetime_accepted(), 2);
}

#[test]
fn broadcast_delivers_chunk_to_live_slot() {
    let reg = ClientRegistry::new();
    let (slot, rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_FIRST),
        Codec::None,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    reg.register(slot);

    reg.broadcast(b"hello");
    let received = rx.recv().unwrap();
    assert_eq!(&received[..], b"hello");
    // `total_bytes_sent` is NOT bumped by `broadcast` — it's
    // counted at the writer layer after the TCP write succeeds
    // (per CodeRabbit round 1 on PR #402), so this unit test
    // without a real writer observes zero.
    assert_eq!(reg.total_bytes_sent(), 0);
}

#[test]
fn broadcast_fans_out_identical_chunks_to_every_slot() {
    let reg = ClientRegistry::new();
    let (s1, rx1) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_FIRST),
        Codec::None,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    let (s2, rx2) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_SECOND),
        Codec::Lz4,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    reg.register(s1);
    reg.register(s2);

    reg.broadcast(b"abcde");

    assert_eq!(rx1.recv().unwrap(), b"abcde");
    assert_eq!(rx2.recv().unwrap(), b"abcde");
    // `total_bytes_sent` is counted on successful TCP write at
    // the `StatsTrackingWrite` layer — unit tests without a
    // real writer observe zero. Integration with the writer
    // is covered in `server.rs`.
    assert_eq!(reg.total_bytes_sent(), 0);
}

#[test]
fn record_bytes_sent_accumulates_in_aggregate() {
    // The writer path calls `record_bytes_sent(n)` after each
    // successful TCP write. Here we simulate the calls
    // directly to pin the aggregate contract.
    let reg = ClientRegistry::new();
    assert_eq!(reg.total_bytes_sent(), 0);
    reg.record_bytes_sent(128);
    reg.record_bytes_sent(256);
    reg.record_bytes_sent(64);
    assert_eq!(reg.total_bytes_sent(), 448);
}

#[test]
fn broadcast_full_channel_counts_drop_for_that_client_only() {
    let reg = ClientRegistry::new();
    // Slow client with a 2-slot channel — we'll stuff it past
    // capacity and verify the drop accounting.
    let (slow, _slow_rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_FIRST),
        Codec::None,
        Role::Control,
        TEST_CHANNEL_DEPTH,
    );
    // Fast client with generous room — shouldn't drop anything.
    let (fast, fast_rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_SECOND),
        Codec::None,
        Role::Control,
        TEST_CHANNEL_DEPTH_GENEROUS,
    );
    let slow_id = slow.id;
    reg.register(slow);
    reg.register(fast);

    // First two broadcasts fit in the slow client's channel, the
    // third is dropped for slow but delivered to fast.
    reg.broadcast(b"a");
    reg.broadcast(b"b");
    reg.broadcast(b"c");

    // Fast client got all three.
    assert_eq!(fast_rx.recv().unwrap(), b"a");
    assert_eq!(fast_rx.recv().unwrap(), b"b");
    assert_eq!(fast_rx.recv().unwrap(), b"c");

    // Slow client's drop counter registers exactly one drop.
    let snap = reg.snapshot();
    let slow_snap = snap
        .iter()
        .find(|c| c.id == slow_id)
        .expect("slow client present in snapshot");
    assert_eq!(slow_snap.buffers_dropped, 1);
    assert_eq!(reg.total_buffers_dropped(), 1);
}

#[test]
fn broadcast_skips_disconnected_slot() {
    let reg = ClientRegistry::new();
    let (slot, rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_FIRST),
        Codec::None,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    reg.register(slot.clone());

    slot.mark_disconnected();
    reg.broadcast(b"payload");

    // Nothing should have been sent — `try_send` never called
    // against a disconnected slot. The Receiver sees Empty.
    assert!(rx.try_recv().is_err());
}

#[test]
fn broadcast_marks_slot_disconnected_when_receiver_dropped() {
    let reg = ClientRegistry::new();
    let (slot, rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_FIRST),
        Codec::None,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    reg.register(slot.clone());

    // Simulate writer thread exit by dropping the receiver.
    drop(rx);

    // The slot isn't disconnected yet — the flag only flips after
    // the broadcaster actually observes `TrySendError::Disconnected`.
    assert!(!slot.is_disconnected());

    reg.broadcast(b"payload");

    // Now it should be flagged.
    assert!(slot.is_disconnected());
}

#[test]
fn prune_disconnected_removes_flagged_slots_only() {
    let reg = ClientRegistry::new();
    let (live, _live_rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_FIRST),
        Codec::None,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    let (dead, _dead_rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_SECOND),
        Codec::None,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    dead.mark_disconnected();
    reg.register(live);
    reg.register(dead);

    assert_eq!(reg.len(), 2);
    let removed = reg.prune_disconnected();
    assert_eq!(removed, 1);
    assert_eq!(reg.len(), 1);
}

#[test]
fn snapshot_reflects_registered_slots_with_stats() {
    let reg = ClientRegistry::new();
    let (slot, _rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_SNAPSHOT),
        Codec::Lz4,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    let slot_id = slot.id;
    reg.register(slot.clone());

    // Mutate the per-client stats through the mutex so we can
    // prove `snapshot` reads the mutated values.
    if let Ok(mut s) = slot.stats.lock() {
        s.bytes_sent = 123;
        s.current_freq_hz = Some(100_000_000);
        s.record_command(CommandOp::SetCenterFreq, Instant::now());
    }

    let snap = reg.snapshot();
    assert_eq!(snap.len(), 1);
    let info = &snap[0];
    assert_eq!(info.id, slot_id);
    assert_eq!(info.peer, test_peer(TEST_PORT_SNAPSHOT));
    assert_eq!(info.codec, Codec::Lz4);
    assert_eq!(info.bytes_sent, 123);
    assert_eq!(info.current_freq_hz, Some(100_000_000));
    assert_eq!(info.recent_commands.len(), 1);
}

#[test]
fn client_stats_record_command_respects_capacity() {
    // record_command pops the oldest entry when the ring is
    // full. Asserts the cap stays bounded under load.
    let mut stats = ClientStats::default();
    let t = Instant::now();
    for _ in 0..(RECENT_COMMANDS_CAPACITY + 5) {
        stats.record_command(CommandOp::SetCenterFreq, t);
    }
    assert_eq!(stats.recent_commands.len(), RECENT_COMMANDS_CAPACITY);
}

#[test]
fn reap_finished_worker_handles_joins_finished_keeps_running() {
    // **Regression test for `CodeRabbit` round 5 on PR #402.**
    // The registry used to park every worker handle until
    // shutdown, so a long-lived server with heavy connection
    // churn accumulated completed handles indefinitely. The
    // fix reaps finished handles on the broadcaster's prune
    // cadence, leaving running ones in place.
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let reg = ClientRegistry::new();

    // Register a handle that has already finished. Wait for
    // `is_finished` to flip true before calling the reaper so
    // the partition is deterministic.
    let finished = std::thread::spawn(|| {});
    while !finished.is_finished() {
        std::thread::sleep(Duration::from_millis(1));
    }
    reg.register_worker_handle(finished);

    // Register a handle that's still running (spins on an
    // atomic flag the test controls).
    let keep_running = Arc::new(AtomicBool::new(true));
    let keep_running_clone = Arc::clone(&keep_running);
    let running = std::thread::spawn(move || {
        while keep_running_clone.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    reg.register_worker_handle(running);

    assert_eq!(
        reg.reap_finished_worker_handles(),
        1,
        "exactly the finished handle should be reaped"
    );

    // Running handle is still in the list — verify by
    // draining (mimics the shutdown path) and confirm we get
    // back exactly one handle.
    keep_running.store(false, Ordering::Relaxed);
    let remaining = reg.drain_worker_handles();
    assert_eq!(remaining.len(), 1, "running handle must not be reaped");
    for h in remaining {
        h.join()
            .expect("running thread exits cleanly once flag clears");
    }
}

#[test]
fn snapshot_excludes_disconnected_slots() {
    // The contract after CodeRabbit round 2: `snapshot()`
    // returns only LIVE clients. Disconnected-but-not-yet-pruned
    // slots are filtered out so UI / FFI consumers don't
    // briefly see dead sessions as live (FFI clients would
    // otherwise get stale ids that are already disconnected).
    let reg = ClientRegistry::new();
    let (live, _live_rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_FIRST),
        Codec::None,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    let (dead, _dead_rx) = ClientSlot::new(
        reg.allocate_id(),
        test_peer(TEST_PORT_SECOND),
        Codec::None,
        Role::Control,
        TEST_CHANNEL_DEPTH_STANDARD,
    );
    reg.register(live);
    reg.register(dead.clone());

    // Both registered → len() == 2 (raw slot count).
    assert_eq!(reg.len(), 2);
    // But snapshot() excludes the disconnected one.
    dead.mark_disconnected();
    assert_eq!(reg.snapshot().len(), 1);
    // Pruning removes it from `len()` too.
    reg.prune_disconnected();
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.snapshot().len(), 1);
}

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
    assert_eq!(reg.lifetime_accepted() as usize, TEST_LISTENER_CAP);
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
    let ctrl_a = role_test_slot(&reg, TAKEOVER_TEST_ORIG_CTRL_PORT, Role::Control);
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
