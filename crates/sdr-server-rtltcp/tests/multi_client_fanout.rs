//! Integration tests for the multi-client fan-out broadcaster
//! (#391). The real [`Server`] needs a USB-attached RTL-SDR dongle
//! to start, so these tests drive [`ClientRegistry`] directly — one
//! producer thread feeding N consumer threads, each draining their
//! own slot's receiver. Validates the per-client drop isolation
//! guarantee that's the whole point of the per-client-channel
//! design: one stalled listener can't block the controller or the
//! other listeners.
//!
//! Kept out of the in-lib unit tests because they spawn threads
//! and coordinate over mpsc channels with timeouts; putting them in
//! `tests/` means they run with `cargo test --test
//! multi_client_fanout` without slowing the inner unit test pass.
//!
//! [`Server`]: sdr_server_rtltcp::Server
//! [`ClientRegistry`]: sdr_server_rtltcp::broadcaster::ClientRegistry

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use sdr_server_rtltcp::broadcaster::{ClientRegistry, ClientSlot};
use sdr_server_rtltcp::codec::Codec;
use sdr_server_rtltcp::extension::Role;

/// How many distinct chunks the producer thread fans out in each
/// test. 32 is small enough for tests to finish quickly, large
/// enough to exercise per-client ordering under the default
/// 500-slot channel.
const TEST_CHUNK_COUNT: usize = 32;

/// Bytes per fanned-out chunk. 64 is plenty to catch any
/// truncation / offset bug and keeps allocation noise low.
const TEST_CHUNK_LEN: usize = 64;

/// Per-client channel depth for the "fast both" tests. Generous
/// enough that neither receiver fills under the test's 32-chunk
/// load.
const TEST_FAST_CHANNEL_DEPTH: usize = 64;

/// Per-client channel depth for the "slow client" test. Small so a
/// client that never reads fills after a few chunks and the
/// producer starts accounting drops against it.
const TEST_SLOW_CHANNEL_DEPTH: usize = 4;

/// How long a consumer waits per `recv_timeout` tick before
/// re-checking whether the producer has finished. Short enough
/// that test wall-clock stays small, long enough to absorb
/// scheduling jitter on CI runners.
const CONSUMER_RECV_TIMEOUT: Duration = Duration::from_millis(50);

/// Wall-clock ceiling on each test. Hard stop so a broken
/// synchronization bug that would otherwise deadlock surfaces as
/// a test timeout instead of hanging the suite indefinitely.
const TEST_WALL_CLOCK_CEILING: Duration = Duration::from_secs(5);

/// Delay between producer broadcasts. Lets consumers get a chance
/// to drain — without this, the producer burns through its loop
/// faster than any single-slot channel can consume, defeating the
/// "fast drain" assertions. Per `CodeRabbit` round 1 on PR #402.
const PRODUCER_YIELD_DELAY: Duration = Duration::from_millis(1);

/// How long the disconnect-scheduler test waits before marking a
/// slot disconnected. Short enough that at least one chunk has
/// broadcast by then, long enough that the producer thread has
/// actually started its loop (spawn has non-zero cost). 3 ms sits
/// comfortably above both bounds on any real CI runner.
const DISCONNECT_DELAY: Duration = Duration::from_millis(3);

/// Test peer ports. Each test uses a disjoint pair so logs
/// pinpoint which test generated which peer.
const TEST_PEER_A_PORT: u16 = 10_001;
const TEST_PEER_B_PORT: u16 = 10_002;
const TEST_SLOW_PEER_PORT: u16 = 10_100;
const TEST_FAST_PEER_PORT: u16 = 10_101;
const TEST_DISCONNECT_A_PORT: u16 = 10_200;
const TEST_DISCONNECT_B_PORT: u16 = 10_201;

fn test_peer(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Build a producer thread that calls `registry.broadcast(&chunk)`
/// `TEST_CHUNK_COUNT` times with a deterministic byte pattern per
/// chunk (`chunk[0..4] = be_bytes(i)`, rest = 0xAB), then signals
/// done via the returned `AtomicBool`.
fn spawn_test_producer(registry: Arc<ClientRegistry>) -> (thread::JoinHandle<()>, Arc<AtomicBool>) {
    let done = Arc::new(AtomicBool::new(false));
    let done_setter = done.clone();
    let handle = thread::Builder::new()
        .name("test-producer".into())
        .spawn(move || {
            for i in 0..TEST_CHUNK_COUNT {
                let mut chunk = vec![0xABu8; TEST_CHUNK_LEN];
                // Stamp the chunk index into the leading 4 bytes
                // so consumers can verify ordering. `TEST_CHUNK_COUNT`
                // fits well inside `u32::MAX` (test-only constant).
                let idx_u32 = u32::try_from(i).expect("chunk count fits u32");
                chunk[..4].copy_from_slice(&idx_u32.to_be_bytes());
                registry.broadcast(&chunk);
                // Yield so consumers get a drain window between
                // broadcasts — see PRODUCER_YIELD_DELAY's docstring.
                thread::sleep(PRODUCER_YIELD_DELAY);
            }
            done_setter.store(true, Ordering::SeqCst);
        })
        .expect("spawn test producer");
    (handle, done)
}

/// Drain `rx` until either `TEST_CHUNK_COUNT` chunks have been
/// received OR `producer_done` is set AND the channel is empty.
/// Returns the collected chunks in arrival order. Borrows the
/// receiver so the caller retains ownership for lifecycle
/// management (e.g. moving into a spawned thread via a closure).
fn drain_until_done(
    rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    producer_done: &Arc<AtomicBool>,
) -> Vec<Vec<u8>> {
    let mut received = Vec::with_capacity(TEST_CHUNK_COUNT);
    let deadline = Instant::now() + TEST_WALL_CLOCK_CEILING;
    while received.len() < TEST_CHUNK_COUNT && Instant::now() < deadline {
        match rx.recv_timeout(CONSUMER_RECV_TIMEOUT) {
            Ok(buf) => received.push(buf),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if producer_done.load(Ordering::SeqCst) {
                    // Producer is done; drain remaining backlog
                    // non-blockingly until empty.
                    while let Ok(buf) = rx.try_recv() {
                        received.push(buf);
                    }
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    received
}

#[test]
fn two_clients_receive_identical_byte_streams() {
    // Two clients with generous channel depths. Producer broadcasts
    // 32 chunks; both consumers drain in parallel. Every chunk
    // should land in both clients' receive buffers in the same
    // order — per-client channels mean the broadcaster fans out
    // identical payloads, and each consumer gets its own Vec
    // clone.
    let registry = Arc::new(ClientRegistry::new());

    let (slot_a, rx_a) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(TEST_PEER_A_PORT),
        Codec::None,
        Role::Control,
        TEST_FAST_CHANNEL_DEPTH,
    );
    // Client B is a listener — pins the post-#392 contract that
    // fan-out reaches Role::Listen slots identically to Control.
    // If broadcast ever starts skipping listeners (e.g., a
    // misplaced `slot.role == Role::Control` filter in the fanout
    // path), this test flips to failure. Per `CodeRabbit` round 1
    // on PR #403.
    let (slot_b, rx_b) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(TEST_PEER_B_PORT),
        Codec::None,
        Role::Listen,
        TEST_FAST_CHANNEL_DEPTH,
    );
    registry.register(slot_a);
    registry.register(slot_b);

    let (producer, producer_done) = spawn_test_producer(registry.clone());

    // Drain both consumers on separate threads so the test
    // exercises the concurrent case (both drains happening while
    // the producer is mid-loop) rather than serializing them.
    let done_a = producer_done.clone();
    let done_b = producer_done.clone();
    let consumer_a = thread::spawn(move || drain_until_done(&rx_a, &done_a));
    let consumer_b = thread::spawn(move || drain_until_done(&rx_b, &done_b));

    let received_a = consumer_a.join().expect("consumer a joined");
    let received_b = consumer_b.join().expect("consumer b joined");
    producer.join().expect("producer joined");

    assert_eq!(
        received_a.len(),
        TEST_CHUNK_COUNT,
        "client A should receive every broadcast chunk (got {}/{})",
        received_a.len(),
        TEST_CHUNK_COUNT
    );
    assert_eq!(
        received_b.len(),
        TEST_CHUNK_COUNT,
        "client B should receive every broadcast chunk (got {}/{})",
        received_b.len(),
        TEST_CHUNK_COUNT
    );
    // Byte-for-byte equality across all 32 chunks — proves the
    // broadcaster clones the same payload to each slot rather
    // than mutating it for one client.
    assert_eq!(
        received_a, received_b,
        "both clients must see the same ordered byte stream"
    );
    // Ordering sanity: chunk i's leading 4 bytes encode `i` as
    // big-endian u32. If the broadcaster ever reordered or
    // dropped silently, this index sequence would break.
    for (i, chunk) in received_a.iter().enumerate() {
        let index = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        assert_eq!(index as usize, i, "chunk {i} has wrong index tag");
    }
}

#[test]
fn slow_client_drops_do_not_block_fast_client() {
    // The core per-client isolation guarantee: one client's
    // never-draining channel fills and starts dropping, but
    // another client's drain keeps pace and sees every chunk.
    //
    // Setup:
    //   - Slow client with a 4-slot channel, no consumer — the
    //     broadcaster's `try_send` fills it in the first 4
    //     chunks, then drops the remaining 28.
    //   - Fast client with a 64-slot channel + a drain thread —
    //     sees all 32 chunks in order.
    let registry = Arc::new(ClientRegistry::new());

    // Slow client is the listener — proves a stalled listener
    // doesn't block the controller's fan-out. This is the
    // realistic production shape: the Control client is an
    // active host and the slow neighbor is a passive listener
    // with a full channel.
    let (slow, _slow_rx) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(TEST_SLOW_PEER_PORT),
        Codec::None,
        Role::Listen,
        TEST_SLOW_CHANNEL_DEPTH,
    );
    let slow_id = slow.id;
    let (fast, fast_rx) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(TEST_FAST_PEER_PORT),
        Codec::None,
        Role::Control,
        TEST_FAST_CHANNEL_DEPTH,
    );
    registry.register(slow);
    registry.register(fast);

    let (producer, producer_done) = spawn_test_producer(registry.clone());
    let done_fast = producer_done.clone();
    let fast_consumer = thread::spawn(move || drain_until_done(&fast_rx, &done_fast));

    let received_fast = fast_consumer.join().expect("fast consumer joined");
    producer.join().expect("producer joined");

    // Fast client — should see every chunk despite the slow
    // neighbor's drops.
    assert_eq!(
        received_fast.len(),
        TEST_CHUNK_COUNT,
        "fast client must receive every chunk regardless of slow neighbor stalling \
         (got {}/{})",
        received_fast.len(),
        TEST_CHUNK_COUNT
    );
    for (i, chunk) in received_fast.iter().enumerate() {
        let index = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        assert_eq!(
            index as usize, i,
            "fast client saw chunk {index} out of order at position {i}"
        );
    }

    // Slow client — should have accumulated drops. Find its entry
    // in the registry snapshot and assert a non-zero drop count.
    let snap = registry.snapshot();
    let slow_snap = snap
        .iter()
        .find(|c| c.id == slow_id)
        .expect("slow client should still be in the registry");
    assert!(
        slow_snap.buffers_dropped > 0,
        "slow client should have accrued drops (got {})",
        slow_snap.buffers_dropped
    );
    // Aggregate counter matches — broadcaster's
    // `total_buffers_dropped` should reflect the slow client's
    // drops.
    assert_eq!(
        registry.total_buffers_dropped(),
        slow_snap.buffers_dropped,
        "aggregate drop counter should equal the slow client's drops when they're \
         the only slow path"
    );
}

#[test]
fn disconnected_client_is_skipped_by_broadcaster_fanout() {
    // A client marked disconnected mid-stream should be skipped on
    // subsequent broadcasts — the broadcaster's `is_disconnected`
    // check on each fan-out prevents wasted clone + try_send work,
    // and the slot gets pruned on the next `prune_disconnected`
    // sweep.
    //
    // Scenario:
    //   - Two clients; A drains normally, B is marked disconnected
    //     AFTER the first chunk arrives but BEFORE the producer
    //     finishes.
    //   - Expected: A sees all 32 chunks; B sees at most the first
    //     few (received before disconnection was observed).
    //   - Aggregate `total_bytes_sent` reflects only A's drain
    //     + whatever B got before disconnection — not the full
    //     32 × TEST_CHUNK_LEN × 2 we'd see if both fully received.
    let registry = Arc::new(ClientRegistry::new());
    let (slot_a, rx_a) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(TEST_DISCONNECT_A_PORT),
        Codec::None,
        Role::Control,
        TEST_FAST_CHANNEL_DEPTH,
    );
    // Slot B is a listener — pins the post-#392 contract that
    // mid-stream disconnect skip-on-fanout applies identically
    // regardless of role.
    let (slot_b, rx_b) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(TEST_DISCONNECT_B_PORT),
        Codec::None,
        Role::Listen,
        TEST_FAST_CHANNEL_DEPTH,
    );
    let slot_b_handle = slot_b.clone();
    registry.register(slot_a);
    registry.register(slot_b);

    // Schedule B's disconnect a short delay in — the producer
    // will have broadcast a few chunks by then; subsequent chunks
    // skip B entirely.
    let disconnector = thread::spawn(move || {
        thread::sleep(DISCONNECT_DELAY);
        slot_b_handle.mark_disconnected();
    });

    let (producer, producer_done) = spawn_test_producer(registry.clone());
    let done_a = producer_done.clone();
    let done_b = producer_done.clone();
    let consumer_a = thread::spawn(move || drain_until_done(&rx_a, &done_a));
    // Drain B too so we can prove it received FEWER chunks than A
    // (the broadcaster actually skipped it post-disconnect). Per
    // CodeRabbit round 1 on PR #402 — the original test only
    // checked A, which passes even if B kept receiving every
    // chunk despite being disconnected.
    let consumer_b = thread::spawn(move || drain_until_done(&rx_b, &done_b));

    let received_a = consumer_a.join().expect("consumer a joined");
    let received_b = consumer_b.join().expect("consumer b joined");
    producer.join().expect("producer joined");
    disconnector.join().expect("disconnector joined");

    assert_eq!(
        received_a.len(),
        TEST_CHUNK_COUNT,
        "client A should receive every chunk even while B is disconnecting \
         (got {}/{})",
        received_a.len(),
        TEST_CHUNK_COUNT
    );
    assert!(
        received_b.len() < received_a.len(),
        "client B should receive fewer chunks than A (got B={}/{}, A={}) — \
         if they match, the broadcaster is still sending to disconnected slots",
        received_b.len(),
        TEST_CHUNK_COUNT,
        received_a.len()
    );
}
