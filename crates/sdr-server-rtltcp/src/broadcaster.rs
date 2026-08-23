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
use crate::extension::Role;
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
    /// Role granted by the server during handshake. `Control` =
    /// commands dispatched to the device; `Listen` = commands
    /// dropped server-side (the command worker observes this and
    /// logs + skips the dispatch). Immutable for the slot's
    /// lifetime — if the client wants to change role they must
    /// reconnect (or, once #393 lands, send a takeover request).
    /// Vanilla `rtl_tcp` clients (no RTLX hello) always land here
    /// as `Control`; they have no way to request `Listen` and the
    /// server only admits them when the Control slot is free. #392.
    pub role: Role,
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
    /// that the writer thread consumes. `role` is the server's
    /// grant (not the client's request — the server may deny the
    /// request, in which case no slot is built at all). #392.
    pub fn new(
        id: ClientId,
        peer: SocketAddr,
        codec: Codec,
        role: Role,
        channel_depth: usize,
    ) -> (Arc<Self>, Receiver<Vec<u8>>) {
        let (tx, rx) = sync_channel::<Vec<u8>>(channel_depth);
        let slot = Arc::new(Self {
            id,
            peer,
            connected_since: Instant::now(),
            codec,
            role,
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
            role: self.role,
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

/// Outcome of [`ClientRegistry::register_with_role`] — whether the
/// slot was admitted, and if not, why. The caller maps these onto
/// the wire-level `ServerExtension` response: `Granted` /
/// `GrantedViaTakeover` → `status=Ok, granted_role=Some(requested)`;
/// denial variants → `status=<matching>, granted_role=None`
/// (0xFF sentinel). #392 / #393.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleDecision {
    /// Slot registered. Caller proceeds with the handshake response
    /// and spawns the per-client writer + command workers.
    Granted,
    /// Slot registered alongside the prior Control client — the
    /// takeover path (#393). Admission alone does **not** displace
    /// the incumbent: the caller must finish the newcomer's setup
    /// (header, timeout, worker threads) and then call
    /// [`ClientRegistry::commit_takeover`] with `displaced_id`,
    /// which marks the prior controller's slot disconnected so its
    /// writer / command threads exit with a clean TCP FIN (#710).
    /// An early return that skips the commit (after
    /// `unwind_admission`) leaves the incumbent in control. On the
    /// wire the caller treats this identically to [`Self::Granted`]
    /// (both send `ServerExtension { status: Ok, granted_role:
    /// Some(Control) }`); `displaced_id` also feeds server-side
    /// logging and UI activity-log correlation.
    GrantedViaTakeover {
        /// `ClientId` of the Control slot that was displaced.
        /// Captured from the slot snapshot taken under the admission
        /// lock so log output points at the exact predecessor.
        displaced_id: ClientId,
    },
    /// Client requested `Role::Control` but another live slot is
    /// already holding it **and** the client did not set the
    /// takeover flag. Caller emits
    /// `ServerExtension { granted_role: None, status: ControllerBusy }`
    /// to RTLX clients (vanilla clients get TCP FIN without a
    /// header so they see "connection refused"-equivalent), then
    /// closes. **Transient** — clients treat this as retryable via
    /// their connect/backoff loop, or prompt the user to click
    /// "Take control" which re-sends the hello with the takeover
    /// flag set.
    ControllerBusy,
    /// Client requested `Role::Listen` but `listener_cap` live
    /// listeners are already registered. Caller emits
    /// `ServerExtension { granted_role: None, status: ListenerCapReached }`
    /// and closes. Vanilla clients never land here — they're
    /// always Control-or-denied.
    ListenerCapReached,
    /// Registry's slots mutex is poisoned — a prior operation
    /// panicked mid-update and the server is in a broken state.
    /// Distinct from [`Self::ControllerBusy`] because the client
    /// retry loop treats ControllerBusy as a transient "try again
    /// in a second" hint; poison is a terminal server fault that
    /// deserves a clean close + server-side log. Callers drop the
    /// client with no wire response (no admission state to
    /// narrate, and the server-side log captures the diagnostic).
    /// Per `CodeRabbit` round 1 on PR #403. #392.
    RegistryPoisoned,
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
    /// Role the server granted to this client (`Control` dispatches
    /// commands to the device; `Listen` drops them at the command
    /// worker). Stats consumers render this as "Controller" /
    /// "Listener" in the client list. #392.
    pub role: Role,
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
    ///
    /// **No role/cap check** — admits unconditionally. Production
    /// callers (`spawn_client_workers`) use
    /// [`Self::register_with_role`] instead so the role gate and
    /// listener cap enforcement happen atomically under the same
    /// lock that would otherwise let two concurrent accepts both
    /// claim Control. `register()` stays as the test-facing
    /// convenience for fixtures that don't exercise the role
    /// path.
    pub fn register(&self, slot: Arc<ClientSlot>) {
        self.lifetime_accepted.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.slots.lock() {
            guard.push(slot);
        }
    }

    /// Admit a slot if the server's role gate and listener cap
    /// permit it. Returns the outcome so the caller can respond to
    /// the client appropriately (Granted / GrantedViaTakeover →
    /// continue the handshake, denied → send a denial
    /// `ServerExtension` + close). The slot's role (decided at
    /// construction time from the client's hello, or defaulted to
    /// `Control` for vanilla clients) drives the decision:
    ///
    /// - `Role::Control` — granted iff no other live slot currently
    ///   has role Control. When the slot IS taken:
    ///   - `request_takeover == false` → [`RoleDecision::ControllerBusy`].
    ///   - `request_takeover == true` → the new slot is admitted
    ///     next to the incumbent and
    ///     [`RoleDecision::GrantedViaTakeover`] carries the
    ///     incumbent's id. The incumbent stays live (and keeps
    ///     winning `ControllerBusy` decisions against plain Control
    ///     requests) until the caller's [`Self::commit_takeover`]
    ///     once the newcomer is fully viable. #393 / #710.
    /// - `Role::Listen` — granted iff the count of live `Listen`
    ///   slots is strictly less than `listener_cap`. Denying with
    ///   `ListenerCapReached`. `request_takeover` is ignored for
    ///   Listen requests: takeover only makes sense against the
    ///   single exclusive Control slot.
    ///
    /// "Live" means not flagged disconnected — the broadcaster's
    /// periodic `prune_disconnected` sweep evicts dead slots, but
    /// between sweeps a slot can already be marked disconnected.
    /// Using the flag for the check means a dropping-Control
    /// client frees the slot immediately on their worker
    /// disconnecting, without waiting for the next prune tick.
    ///
    /// `lifetime_accepted` is bumped only on admission (Granted or
    /// GrantedViaTakeover) — the counter tracks real admissions,
    /// not denied-handshake attempts. The takeover's displaced
    /// controller is NOT subtracted from `lifetime_accepted`: it
    /// was a real admission that happened to end via kick instead
    /// of clean disconnect. #392 / #393.
    pub fn register_with_role(
        &self,
        slot: Arc<ClientSlot>,
        listener_cap: usize,
        request_takeover: bool,
    ) -> RoleDecision {
        // Lock the slot list first so the decision + push (and
        // the takeover-path mark_disconnected) land atomically —
        // two concurrent Control requests can't both observe
        // "slot free" and both push, and a takeover request
        // can't race with another admission that would displace
        // someone else's slot. Same lock discipline as
        // `prune_disconnected` and `snapshot`, so the decision
        // sees a consistent live-set view.
        let Ok(mut guard) = self.slots.lock() else {
            // Poisoned — a prior operation panicked mid-update
            // and the slot list is in a broken state. Surface as
            // `RegistryPoisoned` rather than `ControllerBusy` so
            // clients that retry transient denials don't thrash
            // against a terminally-broken server. Per `CodeRabbit`
            // round 1 on PR #403.
            return RoleDecision::RegistryPoisoned;
        };
        let mut displaced_id: Option<ClientId> = None;
        match slot.role {
            Role::Control => {
                // Find the live controller (if any) so we can
                // either deny or displace.
                let mut live_controllers = guard
                    .iter()
                    .filter(|s| s.role == Role::Control && !s.is_disconnected());
                let existing_controller = live_controllers.next().cloned();
                // A second live Control slot is a takeover that was
                // admitted but not yet committed (#710). Only one
                // newcomer may be pending against the incumbent at a
                // time, or both would commit and leave two live
                // controllers (#808); the reservation is the slot
                // list itself, read under this same lock, and it
                // clears when the newcomer commits (incumbent
                // disconnected) or is unwound (slot removed).
                let takeover_pending = live_controllers.next().is_some();
                if let Some(prev) = existing_controller {
                    if !request_takeover || takeover_pending {
                        return RoleDecision::ControllerBusy;
                    }
                    // Takeover path, phase 1 of 2: admit the
                    // newcomer alongside the incumbent and report
                    // who it will displace. The incumbent is only
                    // marked disconnected by `commit_takeover`,
                    // once the newcomer's workers are up — a
                    // takeover request that drops mid-setup must
                    // not leave the dongle with zero controllers
                    // (#710). Until then `find` keeps resolving the
                    // incumbent as the live controller, so a
                    // concurrent plain Control request is still
                    // `ControllerBusy`. #393.
                    displaced_id = Some(prev.id);
                }
            }
            Role::Listen => {
                let live_listeners = guard
                    .iter()
                    .filter(|s| s.role == Role::Listen && !s.is_disconnected())
                    .count();
                if live_listeners >= listener_cap {
                    return RoleDecision::ListenerCapReached;
                }
            }
        }
        self.lifetime_accepted.fetch_add(1, Ordering::Relaxed);
        guard.push(slot);
        match displaced_id {
            Some(displaced_id) => RoleDecision::GrantedViaTakeover { displaced_id },
            None => RoleDecision::Granted,
        }
    }

    /// Phase 2 of a takeover (#710): mark the displaced controller
    /// disconnected so its workers exit on their next tick. The
    /// slot stays in the registry until the next
    /// `prune_disconnected` sweep — keeping it around keeps its
    /// per-client stats visible in the next UI poll so operators
    /// can see "client 7 was kicked by client 12" in the activity
    /// log.
    ///
    /// The pair is validated under the slot lock (#808): both ids
    /// must be live Control slots and `displaced_id` must have been
    /// admitted *before* `newcomer_id` — the slot list is kept in
    /// admission order, so that ordering is exactly the reservation
    /// `register_with_role` made (the newcomer was admitted to
    /// replace the incumbent that was live at the time). A reversed
    /// pair, a stale incumbent, an unwound newcomer or a repeat
    /// commit is a no-op that returns `false`, so a commit can never
    /// displace a controller the caller did not take over from.
    pub fn commit_takeover(&self, newcomer_id: ClientId, displaced_id: ClientId) -> bool {
        let Ok(guard) = self.slots.lock() else {
            tracing::error!("commit_takeover: registry slots mutex poisoned");
            return false;
        };
        let live_control_position = |id: ClientId| {
            guard
                .iter()
                .position(|s| s.id == id && s.role == Role::Control && !s.is_disconnected())
        };
        let (Some(displaced_at), Some(newcomer_at)) = (
            live_control_position(displaced_id),
            live_control_position(newcomer_id),
        ) else {
            return false;
        };
        if displaced_at >= newcomer_at {
            return false;
        }
        guard[displaced_at].mark_disconnected();
        true
    }

    /// Record `n` USB chunks dropped for `slot` (its queue was full,
    /// or its writer discarded the backlog behind a stalled socket,
    /// #709) in both the per-client and the aggregate counters.
    pub fn record_buffers_dropped(&self, slot: &ClientSlot, n: u64) {
        if let Ok(mut s) = slot.stats.lock() {
            s.buffers_dropped = s.buffers_dropped.saturating_add(n);
        }
        self.total_buffers_dropped.fetch_add(n, Ordering::Relaxed);
    }

    /// Undo a prior [`Self::register_with_role`] `Granted` outcome
    /// after post-admission setup fails (header write fails, worker
    /// spawn fails, etc.). Marks the slot disconnected so the
    /// broadcaster stops fanning to it immediately, removes it
    /// from the slot list, and decrements
    /// [`Self::lifetime_accepted`] so sessions that never served a
    /// byte don't inflate the "accepted clients" counter. Returns
    /// `true` iff the slot was found and removed.
    ///
    /// Idempotent — safe to call even if the slot was already
    /// pruned by [`Self::prune_disconnected`] or removed by a
    /// concurrent rollback. The slot-list mutex serializes the
    /// remove, and the decrement is tied 1:1 to the original
    /// `register_with_role` increment via the `removed` guard, so
    /// double-calls can't underflow the counter.
    ///
    /// Per `CodeRabbit` round 1 on PR #403.
    pub fn unwind_admission(&self, slot: &Arc<ClientSlot>) -> bool {
        // Flag the slot first so any in-flight broadcaster tick
        // skips it before we even take the slots lock. This
        // shrinks the fan-out window between the setup failure
        // and the slot-list remove below to at most one
        // broadcaster tick.
        slot.mark_disconnected();
        let Ok(mut guard) = self.slots.lock() else {
            // Poisoned — the rollback target is inaccessible, but
            // the server is in a terminal state anyway (see
            // `RoleDecision::RegistryPoisoned`). Log + return
            // `false` so callers can't double-count the failure.
            tracing::warn!(
                slot_id = slot.id,
                "unwind_admission: registry slots mutex poisoned"
            );
            return false;
        };
        let before = guard.len();
        guard.retain(|s| s.id != slot.id);
        let removed = guard.len() < before;
        if removed {
            // `lifetime_accepted` was `fetch_add(1)`-bumped during
            // `register_with_role`, so the corresponding
            // `fetch_sub(1)` cancels it out exactly. No underflow
            // risk because the `removed` guard ties the decrement
            // to the prior increment 1:1.
            self.lifetime_accepted.fetch_sub(1, Ordering::Relaxed);
        }
        removed
    }

    /// Park a per-client worker `JoinHandle` for later join.
    /// Called twice per accepted client — once for the writer
    /// thread, once for the command thread. During normal
    /// runtime, finished handles are reaped on the broadcaster's
    /// prune cadence via [`Self::reap_finished_worker_handles`];
    /// any still-running at shutdown are drained + joined by
    /// [`Self::drain_worker_handles`] so the threads' cloned
    /// device `Arc` references are released before `stop()` /
    /// `Drop` returns.
    pub fn register_worker_handle(&self, handle: JoinHandle<()>) {
        if let Ok(mut guard) = self.worker_handles.lock() {
            guard.push(handle);
        }
    }

    /// Join every parked worker handle whose thread has already
    /// exited. Runs on the broadcaster's prune cadence so a
    /// long-lived server with heavy connection churn doesn't
    /// accumulate completed `JoinHandle`s until shutdown — each
    /// handle keeps the thread's OS resources + TLS around until
    /// joined, and the list grows without bound even though the
    /// slots themselves get pruned. Finished handles are cheap
    /// to join (the thread has already exited), so running this
    /// on the same ~2.5 s cadence as slot pruning keeps the
    /// handle list bounded by the number of currently-live
    /// clients × 2. [`Self::drain_worker_handles`] still owns
    /// final-shutdown joining for any handles that had not
    /// finished by the last reap. Returns the number reaped for
    /// tracing. Per `CodeRabbit` round 5 on PR #402.
    pub fn reap_finished_worker_handles(&self) -> usize {
        let Ok(mut guard) = self.worker_handles.lock() else {
            return 0;
        };
        let taken = std::mem::take(&mut *guard);
        let (finished, running): (Vec<_>, Vec<_>) =
            taken.into_iter().partition(JoinHandle::is_finished);
        *guard = running;
        let reaped = finished.len();
        // Release the lock before joining; `is_finished == true`
        // so each join returns immediately, but there's no
        // reason to hold the mutex across the calls and block
        // registrations of new per-client handles from the
        // accept thread.
        drop(guard);
        for h in finished {
            if let Err(payload) = h.join() {
                // A panicked worker thread would have already
                // flipped its slot's `disconnected` flag from
                // inside the unwinding path (slot drop handlers
                // do so) and a newer CR pass can tighten that
                // guarantee. For now, log the payload so
                // regressions surface in tests and tracing.
                tracing::warn!(?payload, "rtl_tcp reaped panicked worker thread");
            }
        }
        reaped
    }

    /// Take every parked worker handle. Caller joins them. Used by
    /// `Server::drop` so the dongle's device mutex `Arc` cannot
    /// linger past the `has_stopped()` transition — otherwise a
    /// follow-up `Server::start` or engine open would fight a
    /// ghost worker for USB exclusivity. Per `CodeRabbit` round 1
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
                    self.record_buffers_dropped(slot.as_ref(), 1);
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
mod tests;
