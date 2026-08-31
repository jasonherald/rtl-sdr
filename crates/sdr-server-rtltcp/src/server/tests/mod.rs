use super::*;

// ============================================================
// Import-path adjustments for the #818 module split. Before the
// split every name below reached the test files through the
// `use super::*` glob against the monolithic `server.rs`; the
// production items now live in the `accept` / `broadcast` /
// `client` / `config` submodules, and the std / crate imports
// that used to sit at the old root are re-imported here. These
// `use` declarations are visible to the child test modules via
// their own `use super::*` globs, so the individual test files
// stay untouched.
// ============================================================
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::broadcaster::{ClientRegistry, ClientSlot};
use crate::codec::{Codec, CodecMask};
use crate::error::ServerError;
use crate::extension::{
    AUTH_KEY_HEADER_LEN, AUTH_REPLY_TIMEOUT, AuthKeyMessage, CLIENT_HELLO_LEN, ClientHello,
    EXTENSION_MAGIC, Role,
};

use super::accept::handshake::{
    HELLO_SNIFF_TIMEOUT, TCP_KEEPALIVE_IDLE_SECS, configure_client_socket, sniff_auth_key_message,
    sniff_client_hello,
};
#[cfg(target_os = "linux")]
use super::accept::handshake::{TCP_KEEPALIVE_INTERVAL_SECS, TCP_KEEPALIVE_RETRIES};
use super::broadcast::{UsbReadOutcome, classify_usb_read};
use super::client::{ChunkOutcome, tcp_writer, write_chunk_shedding_backlog};
use super::config::validate_auth_key_length;

// ============================================================
// Test fixture constants (CodeRabbit round 4 on PR #402).
// Extracted so each test's intent reads at a glance —
// `42_001` on its own is noise, `TEST_CLIENT_A_PORT` plus a
// bounds docstring is self-documenting.
// ============================================================

/// Loopback peer port for the "client A" side of two-client
/// fixtures. Non-privileged, doesn't overlap with anything
/// well-known, and disjoint from `TEST_CLIENT_B_PORT`.
const TEST_CLIENT_A_PORT: u16 = 42_001;
/// Loopback peer port for "client B". Disjoint from
/// `TEST_CLIENT_A_PORT` so snapshot assertions can verify
/// ordering / identity.
const TEST_CLIENT_B_PORT: u16 = 42_002;
/// Small per-client channel depth used by tests that don't
/// exercise the full/drop path — just needs to fit the few
/// chunks a test sends. Anything ≥ the chunk count is fine.
const TEST_CLIENT_CHANNEL_DEPTH: usize = 4;
/// Synthetic `bytes_sent` value for client A's stats —
/// arbitrary small number, just has to differ from B's value
/// so the per-client readback assertions prove the right
/// entry landed in `connected_clients[0]`.
const TEST_CLIENT_A_BYTES: u64 = 100;
/// Synthetic `bytes_sent` value for client B. Differs from
/// A's by an order of magnitude so a cross-over bug stands out.
const TEST_CLIENT_B_BYTES: u64 = 999;
/// 2-meter amateur band frequency (145.5 MHz) stamped into
/// client A's `current_freq_hz` — stand-in for "non-default
/// freq client A commanded".
const TEST_CLIENT_A_FREQ_HZ: u32 = 145_500_000;
/// WFM broadcast band frequency (100 MHz) stamped into
/// client B's `current_freq_hz`. Second distinct sample so
/// cross-client bugs show up as the wrong freq under
/// `connected_clients[1]`.
const TEST_CLIENT_B_FREQ_HZ: u32 = 100_000_000;

// ============================================================
// Auth handshake tests (#394).
//
// The real `spawn_client_workers` needs a live
// `Arc<Mutex<RtlSdrDevice>>` which requires a USB dongle —
// not something CI has. These tests instead exercise
// `sniff_auth_key_message` directly over a loopback TCP
// pair, pinning the wire-read contract that the enforcement
// flow calls into. Full end-to-end (server + client with
// real dongle) lives in the manual smoke test.
// ============================================================

/// Scheduling slack on top of `AUTH_REPLY_TIMEOUT` when
/// asserting that a timeout fired inside the budget. Must be
/// tight enough that a regression to per-phase timeouts
/// (where total elapsed could approach `2 * AUTH_REPLY_TIMEOUT`
/// under the header-then-body flow) trips the assertion, but
/// generous enough to absorb realistic OS scheduling jitter
/// on a loaded CI runner. The original 500 ms flaked on
/// shared GitHub-hosted runners at ~508 ms elapsed (8 ms past
/// the budget — pure scheduling noise). Bumped to 1500 ms:
/// still well under the 2× regression threshold of 5 s
/// (a per-phase revert would land elapsed near 10 s, so
/// 5 + 1.5 = 6.5 s is comfortably on the "good" side), while
/// giving enough headroom that one OS scheduling hiccup won't
/// redden CI. Per `CodeRabbit` round 3 on PR #405, bumped
/// per PR #437 CI flake.
const AUTH_TIMEOUT_SLACK: Duration = Duration::from_millis(1500);

// ============================================================
// sniff_client_hello regression tests (`CodeRabbit` round 2 on PR #399)
//
// The sniff is the only piece of the per-client handshake that
// can run without a real RTL-SDR dongle, so unit tests live here.
// Each test pairs a server-side accept with a client-side TCP
// connect + controlled write pattern, verifying that
// `sniff_client_hello` classifies the stream correctly.
// ============================================================

/// Accept one TCP client on a loopback listener and hand the
/// accepted socket to `sniff_client_hello`. Factored out so
/// each scenario test stays focused on what bytes the client
/// writes, not the boilerplate of setting up sockets.
fn run_sniff_against<F>(client_behavior: F) -> std::io::Result<Option<ClientHello>>
where
    F: FnOnce(TcpStream) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client_thread = thread::spawn(move || {
        let client = TcpStream::connect(addr).unwrap();
        client_behavior(client);
    });
    let (server_stream, _peer) = listener.accept().unwrap();
    let result = sniff_client_hello(&server_stream);
    // Join best-effort — the client thread may legitimately still
    // be holding the connection open (partial-hello test). Drop
    // the server side first so any pending write on the client
    // side unblocks, then join.
    drop(server_stream);
    let _ = client_thread.join();
    result
}

// --- #709 / #711 (Aug 2026 deep review) ---

const STALL_TEST_PORT: u16 = 42_010;

fn test_peer(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}
const STALL_TEST_CHUNK: [u8; 4] = [1, 2, 3, 4];

mod auth_sniff;
mod hello_sniff;
mod lifecycle;
mod usb;
mod writer;
