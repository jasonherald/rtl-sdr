use super::*;

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
