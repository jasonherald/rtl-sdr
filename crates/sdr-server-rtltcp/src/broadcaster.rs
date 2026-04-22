//! Multi-client fan-out broadcaster (#391).
//!
//! Replaces the single-client data path from the pre-#391 server. The
//! previous model had one `data_worker` per connected client pulling USB
//! bulk bytes into a bounded [`std::sync::mpsc::sync_channel`] drained by
//! the client's own writer thread. That worked for one client because the
//! USB device is exclusive — but it couldn't serve a second client at all
//! (the accept loop rejected any second connection with TCP FIN).
//!
//! The new model has **one** USB reader thread (owned by [`Server`])
//! feeding **many** bounded per-client channels via a [`ClientRegistry`].
//! Each connected client gets its own [`ClientSlot`] carrying the write
//! side of its channel, its negotiated codec, its per-client stats, and a
//! disconnection flag the writer / command threads flip on exit. The USB
//! reader calls [`ClientRegistry::broadcast`] once per USB chunk; the
//! registry fans out by cloning the chunk and `try_send`-ing to each
//! live slot.
//!
//! # Backpressure and drop-on-full
//!
//! Every slot has its own bounded channel (capacity configurable via
//! [`ServerConfig`]). When a single slow client stops draining, their
//! channel fills and subsequent [`TrySendError::Full`] returns are
//! counted against **that client only** — the drop counter on their
//! [`ClientSlot`] goes up. Other clients with drained channels keep
//! receiving bytes uninterrupted. This is the whole point of per-client
//! channels versus a shared broadcast queue: one slow listener can't
//! stall the controller.
//!
//! # Disconnection lifecycle
//!
//! A client's writer or command thread flips [`ClientSlot::disconnected`]
//! on error / EOF. The broadcaster observes the flag on the next
//! fan-out tick and skips that slot (its channel is presumed dead).
//! Periodically the broadcaster calls [`ClientRegistry::prune_disconnected`]
//! which walks the slot list, removes disconnected entries, and drops
//! the last `Arc<ClientSlot>` — which closes the channel receiver (if the
//! writer thread has exited) and releases all per-client resources.
//!
//! # Thread-safety
//!
//! [`ClientRegistry`] holds its slot list behind a [`Mutex`]. The
//! broadcaster clones the list of live `Arc<ClientSlot>` under the lock,
//! then releases it before doing any `try_send` work. This means the
//! accept thread can [`ClientRegistry::register`] new clients while a
//! fan-out is in flight (brief lock contention during the clone, nothing
//! more). Per-slot mutable state (stats, disconnection flag) uses
//! independent synchronization (Atomic + Mutex) so slots don't
//! serialize on the registry lock.
//!
//! This module ships in isolation in the first commit of #391 — the
//! public types and registry API compile + test without any wiring
//! into [`crate::server`] yet. The data-path flip lands in the next
//! commit.
//!
//! [`Server`]: crate::server::Server
//! [`ServerConfig`]: crate::server::ServerConfig
//! [`crate::server`]: crate::server

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::codec::Codec;
use crate::protocol::CommandOp;

/// Default per-client bounded-channel capacity measured in 256 KiB USB
/// chunks. Matches the pre-#391 single-client default (`llbuf_num = 500`
/// in upstream rtl_tcp.c:61) — a fresh per-client buffer now instead of
/// a shared one. Per-client sizing keeps the memory bound predictable as
/// the connected-client count grows.
pub const DEFAULT_PER_CLIENT_BUFFER_DEPTH: usize = 500;

/// Monotonic per-server-lifetime client identifier. Assigned by
/// [`ClientRegistry::register`] and never reused, even after the client
/// disconnects. Used by UI and debug logs to correlate stats snapshots
/// across consecutive polls ("client 7 disconnected, client 8 connected"
/// reads more clearly than peer-address equality, especially when the
/// same peer reconnects on a fresh port).
pub type ClientId = u64;

/// Maximum number of recent `(CommandOp, Instant)` entries retained in
/// a client's [`ClientStats::recent_commands`] ring. Same bound as the
/// pre-#391 server-wide ring — just per-client now so one chatty client
/// can't crowd out another's activity log in the UI.
pub const RECENT_COMMANDS_CAPACITY: usize = 50;

/// Mutable per-client counters updated by the writer (bytes_sent via the
/// existing `StatsTrackingWrite`), broadcaster (buffers_dropped on
/// `TrySendError::Full`), and command worker (last_command +
/// current_freq/rate/gain + recent_commands on each dispatched command).
///
/// Held behind a [`Mutex`] on [`ClientSlot`] — contention is low because
/// the writer taps it once per USB chunk (hundreds of Hz), commands are
/// sparse user actions, and UI snapshots happen at poll cadence (~2 Hz).
#[derive(Debug, Clone, Default)]
pub struct ClientStats {
    /// Bytes written to the client's TCP socket since connect.
    /// Post-compression when the client negotiated a non-`None` codec
    /// (counted at the `StatsTrackingWrite` adapter below the encoder),
    /// so the UI's data-rate row reflects on-wire throughput.
    pub bytes_sent: u64,
    /// USB chunks dropped for THIS client because its channel was full.
    /// Incremented by the broadcaster on `TrySendError::Full`. Other
    /// clients whose channels drained normally are unaffected.
    pub buffers_dropped: u64,
    /// Most recently dispatched command. UI renders it as the
    /// client's "last action" hint.
    pub last_command: Option<(CommandOp, Instant)>,
    /// Client's most recent `SetCenterFreq` request, in Hz. We record
    /// what the client ASKED for, not what the device ultimately
    /// applied — dispatch logs device-side failures at `warn!`, and a
    /// client that sees its tune get rejected will just re-ask.
    pub current_freq_hz: Option<u32>,
    /// Client's most recent `SetSampleRate` request, in Hz.
    pub current_sample_rate_hz: Option<u32>,
    /// Client's most recent `SetTunerGain` request, in tenths of dB
    /// (negative is legal per upstream).
    pub current_gain_tenths_db: Option<i32>,
    /// `true` when the client most recently sent `SetGainMode(auto)`,
    /// `false` on `SetGainMode(manual)`, `None` when it hasn't sent
    /// one this session.
    pub current_gain_auto: Option<bool>,
    /// Bounded ring of recent dispatched commands. Oldest at front,
    /// newest at back; capped at [`RECENT_COMMANDS_CAPACITY`].
    pub recent_commands: VecDeque<(CommandOp, Instant)>,
}

impl ClientStats {
    /// Push a dispatched command onto the ring, evicting the oldest
    /// entry when the capacity is already reached. Centralized so the
    /// command worker doesn't duplicate the `pop_front` + `push_back`
    /// dance at each call site.
    pub fn record_command(&mut self, op: CommandOp, at: Instant) {
        self.last_command = Some((op, at));
        if self.recent_commands.len() >= RECENT_COMMANDS_CAPACITY {
            self.recent_commands.pop_front();
        }
        self.recent_commands.push_back((op, at));
    }
}

/// Per-client state held by the registry. Owned through `Arc` so the
/// broadcaster, writer thread, and command thread can each hold a
/// reference without fighting for ownership — they all do different
/// things with it but the slot outlives them all via the registry.
///
/// Split into immutable identity fields (`id`, `peer`, `connected_since`,
/// `codec`) and mutable fields (`tx` read-only after construction,
/// `stats` via Mutex, `disconnected` via Atomic) so the immutable ones
/// can be read lock-free from anywhere.
pub struct ClientSlot {
    /// Stable identifier assigned by the registry.
    pub id: ClientId,
    /// Peer address captured at accept time. Stays in the slot for
    /// its lifetime — never updated even if the underlying socket
    /// gets torn down.
    pub peer: SocketAddr,
    /// Wall-clock moment the handshake completed and the slot was
    /// registered. Used for uptime displays.
    pub connected_since: Instant,
    /// Codec negotiated during the extended `"RTLX"` handshake (or
    /// [`Codec::None`] for legacy clients). Immutable for the slot's
    /// lifetime — if the client wants to change codec they must
    /// reconnect.
    pub codec: Codec,
    /// Write half of this client's bounded channel. The broadcaster
    /// calls [`SyncSender::try_send`] to push USB chunks; the
    /// client's writer thread owns the matching `Receiver` and
    /// drains into the encoded socket.
    pub tx: SyncSender<Vec<u8>>,
    /// Per-client counters. Held behind a Mutex rather than an
    /// atomic-field cluster so structured fields (last_command,
    /// recent_commands) don't need their own synchronization.
    pub stats: Mutex<ClientStats>,
    /// Set to `true` by the client's writer or command thread when
    /// it observes an unrecoverable error (broken socket, EOF,
    /// mutex poison). The broadcaster skips slots with this flag
    /// set on its next fan-out; [`ClientRegistry::prune_disconnected`]
    /// removes them entirely on its next sweep.
    pub disconnected: AtomicBool,
}

impl ClientSlot {
    /// Construct a slot with a freshly-created bounded channel.
    /// Returns both the slot (ready to register) and the `Receiver`
    /// that the writer thread consumes.
    pub fn new(
        id: ClientId,
        peer: SocketAddr,
        codec: Codec,
        channel_depth: usize,
    ) -> (Arc<Self>, Receiver<Vec<u8>>) {
        let (tx, rx) = sync_channel::<Vec<u8>>(channel_depth);
        let slot = Arc::new(Self {
            id,
            peer,
            connected_since: Instant::now(),
            codec,
            tx,
            stats: Mutex::new(ClientStats::default()),
            disconnected: AtomicBool::new(false),
        });
        (slot, rx)
    }

    /// Mark the slot as disconnected. Idempotent; safe to call from
    /// multiple threads (e.g. writer AND command workers both observe
    /// a broken socket concurrently).
    pub fn mark_disconnected(&self) {
        self.disconnected.store(true, Ordering::Release);
    }

    /// Whether the slot has been marked disconnected by any of its
    /// worker threads. The broadcaster uses this to skip fan-out to
    /// dying clients; the pruner uses it to decide which slots to
    /// remove from the registry.
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }

    /// Read-only projection of the slot's state for stats consumers
    /// (UI / FFI). Acquires the stats mutex exactly once.
    pub fn snapshot(&self) -> ClientInfo {
        // Poisoned-mutex path: return a best-effort snapshot with
        // zeroed counters rather than failing the whole `snapshot()`
        // call chain. A UI that misses one update is fine; a crashed
        // UI thread is not.
        let stats = self.stats.lock().ok();
        let stats_clone = stats.as_ref().map(|g| (**g).clone()).unwrap_or_default();
        ClientInfo {
            id: self.id,
            peer: self.peer,
            connected_since: self.connected_since,
            codec: self.codec,
            bytes_sent: stats_clone.bytes_sent,
            buffers_dropped: stats_clone.buffers_dropped,
            last_command: stats_clone.last_command,
            current_freq_hz: stats_clone.current_freq_hz,
            current_sample_rate_hz: stats_clone.current_sample_rate_hz,
            current_gain_tenths_db: stats_clone.current_gain_tenths_db,
            current_gain_auto: stats_clone.current_gain_auto,
            recent_commands: stats_clone.recent_commands,
        }
    }
}

/// Public snapshot of a client's state, returned by
/// [`ClientRegistry::snapshot`] and embedded in `ServerStats`. Flat
/// (not an `Arc`) so stats consumers can clone it freely without
/// affecting the registry.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub id: ClientId,
    pub peer: SocketAddr,
    pub connected_since: Instant,
    pub codec: Codec,
    pub bytes_sent: u64,
    pub buffers_dropped: u64,
    pub last_command: Option<(CommandOp, Instant)>,
    pub current_freq_hz: Option<u32>,
    pub current_sample_rate_hz: Option<u32>,
    pub current_gain_tenths_db: Option<i32>,
    pub current_gain_auto: Option<bool>,
    pub recent_commands: VecDeque<(CommandOp, Instant)>,
}

/// Thread-safe registry of connected clients.
///
/// One instance per [`Server`], shared across:
///
/// - **Accept loop** — calls [`Self::register`] after a successful
///   handshake.
/// - **Broadcaster thread** — calls [`Self::broadcast`] once per USB
///   chunk and [`Self::prune_disconnected`] periodically.
/// - **Stats snapshot path** — calls [`Self::snapshot`] when the UI /
///   FFI polls `Server::stats()`.
///
/// [`Server`]: crate::server::Server
#[derive(Default)]
pub struct ClientRegistry {
    /// Live client slots. Slots are held by `Arc` so the broadcaster
    /// can clone a stable snapshot of them under the lock, release
    /// the lock, then fan-out without blocking `register` / `prune`
    /// callers. Order preserved — roughly "oldest client first" —
    /// so stats snapshots render consistently across polls.
    slots: Mutex<Vec<Arc<ClientSlot>>>,
    /// Per-client worker `JoinHandle`s parked until server shutdown.
    /// Each `spawn_client_workers` call pushes two entries (writer +
    /// command). `Server::stop()` / `Drop` drain and join them after
    /// setting the global shutdown flag so the dongle's
    /// `Arc<Mutex<RtlSdrDevice>>` is actually released by the time
    /// `drop` / `stop` returns.
    ///
    /// **Note on `has_stopped()`:** that flag is narrowly scoped —
    /// it flips when the accept thread exits, which happens BEFORE
    /// these handles are drained. Callers that need "dongle is
    /// actually free" must wait for `stop()` / `Drop` to return,
    /// not poll `has_stopped()`. See `Server::has_stopped` for the
    /// full contract.
    ///
    /// Kept on the registry rather than the slot so a panicked /
    /// disconnected slot can be pruned without losing its handle —
    /// the handle still blocks on the panicking thread's actual
    /// exit during shutdown join.
    ///
    /// Per `CodeRabbit` round 1 on PR #402 (initial fix) + round 3
    /// (doc alignment with the `has_stopped` contract).
    worker_handles: Mutex<Vec<JoinHandle<()>>>,
    /// Monotonic `ClientId` allocator. Never reused. An atomic so
    /// the accept loop doesn't need to hold `slots` to issue an id.
    next_id: AtomicU64,
    /// Cumulative count of clients registered since the server started.
    /// Persists across disconnects — `snapshot().len()` tells you
    /// how many are connected right now; this tells you how many
    /// ever have been. Useful for server-uptime / load diagnostics.
    lifetime_accepted: AtomicU64,
    /// Cumulative bytes actually written to the wire across all
    /// clients. Incremented by [`Self::record_bytes_sent`] from the
    /// per-client writer path AFTER the TCP write succeeds so it
    /// reflects post-compression on-wire bytes, not pre-encoding
    /// payload. The per-client `ClientStats::bytes_sent` is
    /// incremented at the same point for the same reason. Monotonic;
    /// never reset. Per `CodeRabbit` round 1 on PR #402.
    total_bytes_sent: AtomicU64,
    /// Cumulative buffers dropped across all clients. Monotonic.
    total_buffers_dropped: AtomicU64,
}

impl ClientRegistry {
    /// Fresh registry with no clients. Normally constructed once by
    /// `Server::start` and shared via `Arc<ClientRegistry>`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next [`ClientId`] without taking the slot lock.
    /// Called before [`Self::register`] so the caller can stamp the
    /// id on the slot's `ClientSlot::id` field inside
    /// [`ClientSlot::new`]. Monotonic, never reuses.
    pub fn allocate_id(&self) -> ClientId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Push a slot onto the registry. The slot's `id` field SHOULD
    /// have been allocated via [`Self::allocate_id`]; the registry
    /// doesn't enforce this but stats consumers expect ids to be
    /// monotonic and unique.
    pub fn register(&self, slot: Arc<ClientSlot>) {
        self.lifetime_accepted.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.slots.lock() {
            guard.push(slot);
        }
    }

    /// Park a per-client worker `JoinHandle` for later shutdown
    /// join. Called twice per accepted client — once for the
    /// writer thread, once for the command thread. Handles are
    /// drained and joined by [`Self::drain_worker_handles`] during
    /// `Server::drop`, guaranteeing the threads' cloned device
    /// `Arc` references are released before shutdown completes.
    pub fn register_worker_handle(&self, handle: JoinHandle<()>) {
        if let Ok(mut guard) = self.worker_handles.lock() {
            guard.push(handle);
        }
    }

    /// Take every parked worker handle. Caller joins them. Used by
    /// `Server::drop` so the dongle's device mutex `Arc` cannot
    /// linger past the `has_stopped()` transition — otherwise a
    /// follow-up `Server::start` or engine open would fight a
    /// ghost worker for USB exclusivity. Per CodeRabbit round 1
    /// on PR #402.
    pub fn drain_worker_handles(&self) -> Vec<JoinHandle<()>> {
        self.worker_handles
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }

    /// Increment the cumulative on-wire byte counter by `n`. Called
    /// from the per-client writer path after a successful TCP
    /// write so the aggregate tracks post-compression bytes. Per
    /// CodeRabbit round 1 on PR #402 — moved here from
    /// `broadcast` (which counted pre-compression payload bytes
    /// at `try_send` time, double-counting whatever was dropped on
    /// a full channel).
    pub fn record_bytes_sent(&self, n: u64) {
        self.total_bytes_sent.fetch_add(n, Ordering::Relaxed);
    }

    /// Number of slots currently in the registry (includes slots
    /// marked disconnected but not yet pruned). Cheap — only locks
    /// the slot mutex briefly.
    pub fn len(&self) -> usize {
        self.slots.lock().map_or(0, |g| g.len())
    }

    /// True when [`Self::len`] is zero.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cumulative client count over the server's lifetime. Includes
    /// clients that have since disconnected. Monotonic.
    pub fn lifetime_accepted(&self) -> u64 {
        self.lifetime_accepted.load(Ordering::Relaxed)
    }

    /// Cumulative **on-wire** bytes written across all clients for
    /// the server's lifetime. Counted at the writer layer
    /// ([`Self::record_bytes_sent`] called from
    /// `StatsTrackingWrite::write` after the TCP write succeeds)
    /// so it reflects post-compression bytes for LZ4 sessions and
    /// matches the sum of per-client writes exactly. Monotonic;
    /// never reset, including across client disconnects.
    ///
    /// Per `CodeRabbit` round 1 on PR #402 (moved counting here
    /// from `broadcast()`) + round 3 (doc update to match).
    pub fn total_bytes_sent(&self) -> u64 {
        self.total_bytes_sent.load(Ordering::Relaxed)
    }

    /// Cumulative drops across all clients.
    pub fn total_buffers_dropped(&self) -> u64 {
        self.total_buffers_dropped.load(Ordering::Relaxed)
    }

    /// Fan one IQ chunk out to every live slot. For each slot:
    ///
    /// - **Live + channel has room** → `try_send` succeeds. No
    ///   counter bump happens here — bytes are counted on the
    ///   per-client writer side after the TCP write succeeds (via
    ///   [`Self::record_bytes_sent`] + the slot's
    ///   `bytes_sent` field), so the aggregate and per-client
    ///   counters reflect post-compression, post-successful-write
    ///   bytes. Per `CodeRabbit` round 1 on PR #402.
    /// - **Live + channel full** → `TrySendError::Full`; chunk is
    ///   dropped for this slot only, `buffers_dropped` increments.
    /// - **`Receiver` dropped** → `TrySendError::Disconnected`; the
    ///   writer thread has exited. Slot is marked disconnected here
    ///   so it gets pruned on the next sweep.
    /// - **Already disconnected** → skipped.
    ///
    /// The fan-out clones `chunk` per live slot (one heap allocation
    /// each). At the typical 2.4 Msps rate and ~10 clients this is
    /// ~48 MB/s of clone traffic — negligible on any hardware that
    /// can run the server in the first place. Per-slot channels
    /// means we can't avoid the clone entirely (shared `Arc<Vec<u8>>`
    /// would serialize drains through the single buffer's strong-ref
    /// counter; the slow path wins little and the fast path pays
    /// refcount overhead).
    ///
    /// Uses a lock-scope narrowing trick: collect live slots into a
    /// local Vec under the lock, drop the lock, then do the fan-out
    /// without holding it. Accept thread can `register` a new slot
    /// mid-broadcast without blocking.
    pub fn broadcast(&self, chunk: &[u8]) {
        // Snapshot the live slots while holding the lock. Skip slots
        // already marked disconnected so we don't bother cloning the
        // chunk into a channel whose receiver has gone away.
        let live: Vec<Arc<ClientSlot>> = match self.slots.lock() {
            Ok(g) => g.iter().filter(|s| !s.is_disconnected()).cloned().collect(),
            Err(_) => return,
        };

        for slot in live {
            let buf = chunk.to_vec();
            match slot.tx.try_send(buf) {
                Ok(()) => {
                    // Bytes are counted at the writer layer after
                    // the TCP write succeeds (both per-client
                    // `bytes_sent` and the aggregate
                    // `total_bytes_sent` increment there). Counting
                    // here would inflate the aggregate with bytes
                    // that never reach the wire when a client
                    // disconnects mid-queue.
                }
                Err(TrySendError::Full(_)) => {
                    // Per-slot drop accounting.
                    if let Ok(mut s) = slot.stats.lock() {
                        s.buffers_dropped = s.buffers_dropped.saturating_add(1);
                    }
                    self.total_buffers_dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {
                    // Writer thread has exited and dropped the
                    // `Receiver`. Mark the slot so prune picks it up.
                    slot.mark_disconnected();
                }
            }
        }
    }

    /// Remove every slot whose `disconnected` flag is set. Returns the
    /// number of slots removed, for log/tracing callers that want to
    /// report churn. The broadcaster calls this periodically (not on
    /// every chunk — the lock-cost-to-signal ratio isn't worth it at
    /// the USB cadence).
    pub fn prune_disconnected(&self) -> usize {
        let Ok(mut guard) = self.slots.lock() else {
            return 0;
        };
        let before = guard.len();
        guard.retain(|s| !s.is_disconnected());
        before - guard.len()
    }

    /// Project every **live** slot to a [`ClientInfo`] snapshot for
    /// stats consumers. Disconnected-but-not-yet-pruned slots are
    /// filtered out — otherwise UI and FFI consumers would briefly
    /// see dead sessions as live and the FFI could hand callers
    /// `client_id`s that are already disconnected. Per CodeRabbit
    /// round 2 on PR #402.
    ///
    /// Order preserved from the underlying slot list (oldest-first).
    pub fn snapshot(&self) -> Vec<ClientInfo> {
        let Ok(guard) = self.slots.lock() else {
            return Vec::new();
        };
        guard
            .iter()
            .filter(|s| !s.is_disconnected())
            .map(|s| s.snapshot())
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
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
            TEST_CHANNEL_DEPTH_STANDARD,
        );
        reg.register(slot);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.lifetime_accepted(), 1);

        let (slot2, _rx2) = ClientSlot::new(
            reg.allocate_id(),
            test_peer(TEST_PORT_GENERIC_B),
            Codec::Lz4,
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
            TEST_CHANNEL_DEPTH_STANDARD,
        );
        let (s2, rx2) = ClientSlot::new(
            reg.allocate_id(),
            test_peer(TEST_PORT_SECOND),
            Codec::Lz4,
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
            TEST_CHANNEL_DEPTH,
        );
        // Fast client with generous room — shouldn't drop anything.
        let (fast, fast_rx) = ClientSlot::new(
            reg.allocate_id(),
            test_peer(TEST_PORT_SECOND),
            Codec::None,
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
            TEST_CHANNEL_DEPTH_STANDARD,
        );
        let (dead, _dead_rx) = ClientSlot::new(
            reg.allocate_id(),
            test_peer(TEST_PORT_SECOND),
            Codec::None,
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
            TEST_CHANNEL_DEPTH_STANDARD,
        );
        let (dead, _dead_rx) = ClientSlot::new(
            reg.allocate_id(),
            test_peer(TEST_PORT_SECOND),
            Codec::None,
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
}
