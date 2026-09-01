use super::*;

// ============================================================
// Import-path adjustments for the #818 module split. Before the
// split every name below reached the test files through the
// `use super::*` glob against the monolithic `rtl_tcp.rs`; the
// production items now live in the `commands` / `handshake` /
// `manager` / `pump` submodules, and the std / crate imports
// that used to sit at the old root are re-imported here. These
// `use` declarations are visible to the child test modules via
// their own `use super::*` globs, so the individual test files
// stay untouched.
// ============================================================
use std::io::{Read, Write};
use std::net::SocketAddr;

use sdr_pipeline::source_manager::Source;
use sdr_server_rtltcp::codec::CodecMask;
use sdr_server_rtltcp::extension::{EXTENSION_MAGIC, ServerExtension, Status};
use sdr_server_rtltcp::protocol::{Command, CommandOp, DONGLE_INFO_LEN};
use sdr_types::Complex;

use super::handshake::connect_cancellable;
use super::manager::backoff_delay;
use super::pump::{append_with_cap_inner, append_with_cap_to_shared, end_session};
// Tests build `ServerExtension` / `ClientHello` values with
// explicit `version: PROTOCOL_VERSION`. Lib code itself
// picks versions via `required_protocol_version` and no
// longer needs the constant at the top of the file; the
// test-scope import keeps clippy's `unused_imports` lint
// happy on the lib target.
use sdr_server_rtltcp::extension::PROTOCOL_VERSION;
use sdr_server_rtltcp::protocol::COMMAND_LEN;
use std::net::TcpListener;
use std::sync::mpsc;
// `CLIENT_HELLO_LEN` is only consumed by the loopback fixtures
// in the RTLX handshake tests below — keep it in test scope
// so the lib build doesn't warn it as unused.
use sdr_server_rtltcp::extension::CLIENT_HELLO_LEN;

/// Placeholder host/port for tests that never actually connect —
/// just exercise builder state or buffer logic. The string "127.0.0.1"
/// is fine as-is, but the port number is named for intent.
const UNUSED_TEST_PORT: u16 = 1234;

/// A port we expect connect() to fail with ECONNREFUSED on localhost
/// so the shutdown-during-retry test doesn't hang waiting for a SYN
/// timeout. Port 1 is a well-known unused privileged port and on
/// Linux loopback refuses instantly.
const REFUSED_TEST_PORT: u16 = 1;

// ============================================================
// RTLX handshake fixture constants (CodeRabbit round 7 on PR #399)
//
// Pulled out of the individual tests so the acceptance,
// rejection, and retry paths all use the same gain count,
// socket timeouts, and state-observation deadlines — avoiding
// silent drift between the fixtures.
// ============================================================

/// Gain step count the fixture dongle advertises in its
/// `dongle_info_t` header. R820T's published table is 29
/// steps; matches upstream rtl-sdr exactly.
const RTLX_TEST_GAIN_COUNT: u32 = 29;
/// Read timeout the client uses against the fixture server.
/// Short (200 ms) so a stalled test exits quickly rather than
/// holding the whole suite up.
const RTLX_TEST_DATA_READ_TIMEOUT: Duration = Duration::from_millis(200);
/// How long the fixture server holds the accepted-connection
/// socket open after writing its responses. Must exceed
/// `RTLX_TEST_DATA_READ_TIMEOUT` so the client finishes
/// reading the extension body before EOF, but short enough
/// that the server thread joins quickly at test teardown.
const RTLX_TEST_SERVER_HOLD: Duration = Duration::from_millis(400);
/// Wall-clock deadline the tests give the client to reach
/// its expected state (Connected / Failed / non-Failed).
/// Generous enough to absorb CI scheduling jitter.
const RTLX_TEST_STATE_DEADLINE: Duration = Duration::from_secs(2);
/// Poll interval inside the state-observation loops. Short
/// enough to catch brief state visits without pegging a core.
const RTLX_TEST_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Slice large enough to catch any stray prefix the default
/// client might emit against a vanilla-shape server — 4 bytes
/// would be the EXTENSION_MAGIC prefix, 8 would be a full
/// `ClientHello`. 16 bytes comfortably covers either
/// regression. Used by
/// `rtl_tcp_default_config_sends_no_hello_to_vanilla_server`.
const NO_HELLO_PROBE_LEN: usize = 16;
/// Read timeout for the "did the client send anything?" probe.
/// Short enough to keep the test fast, long enough that a
/// real client-side hello emission would fall well within
/// this window.
const NO_HELLO_PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// Shared fixture setup: listener on loopback + config that
/// opts into the extended handshake. Keeps the three RTLX
/// handshake tests aligned on compression mask + timeouts.
fn rtlx_test_listener_and_config() -> (TcpListener, RtlTcpConfig) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    (listener, rtlx_test_config(CodecMask::NONE_AND_LZ4))
}

/// Loopback server behavior for a single RTLX accept: read
/// the 8-byte `ClientHello`, write `dongle_info_t`, write the
/// caller-supplied `ServerExtension`, then hold the socket
/// open for `RTLX_TEST_SERVER_HOLD` so the client reads the
/// extension body before EOF.
fn rtlx_test_serve_one(listener: &TcpListener, ext: ServerExtension) {
    let (mut sock, _) = listener.accept().expect("accept");
    sock.set_read_timeout(Some(RTLX_TEST_STATE_DEADLINE))
        .unwrap();
    let mut hello_buf = [0u8; CLIENT_HELLO_LEN];
    sock.read_exact(&mut hello_buf).expect("read hello");
    assert_eq!(&hello_buf[..EXTENSION_MAGIC.len()], &EXTENSION_MAGIC);
    sock.write_all(&rtlx_test_dongle_info()).unwrap();
    sock.write_all(&ext.to_bytes()).unwrap();
    thread::sleep(RTLX_TEST_SERVER_HOLD);
}

/// Byte offsets inside the 8-byte RTLX `ClientHello` the fixtures
/// inspect (magic[0..4], codec mask, role, flags, version).
const HELLO_CODEC_MASK_OFFSET: usize = 4;
const HELLO_ROLE_OFFSET: usize = 5;
const HELLO_FLAGS_OFFSET: usize = 6;
const HELLO_VERSION_OFFSET: usize = 7;

/// Replay-mask bit for `op`: wire ops are 1-based, bits 0-based.
fn replay_bit(op: CommandOp) -> u32 {
    1u32 << ((op as u32) - 1)
}

/// The `dongle_info_t` every loopback server in these tests sends.
fn rtlx_test_dongle_info() -> [u8; DONGLE_INFO_LEN] {
    DongleInfo {
        tuner: TunerTypeCode::R820t,
        gain_count: RTLX_TEST_GAIN_COUNT,
    }
    .to_bytes()
}

/// A `Status::Ok` server extension granting `granted_role` with `codec`.
fn rtlx_ok_extension(codec: Codec, granted_role: Role, version: u8) -> ServerExtension {
    ServerExtension {
        codec,
        granted_role: Some(granted_role),
        status: Status::Ok,
        version,
    }
}

/// Client config the RTLX fixtures start from: short data-read
/// timeout, two strikes, plain Control role, no takeover, no auth.
fn rtlx_test_config(compression: CodecMask) -> RtlTcpConfig {
    RtlTcpConfig {
        data_read_timeout: RTLX_TEST_DATA_READ_TIMEOUT,
        max_consecutive_timeouts: 2,
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        compression,
        request_takeover: false,
        auth_key: None,
        requested_role: Role::Control,
    }
}

/// Build a source against `addr` and start its manager thread.
fn rtlx_start(addr: SocketAddr, config: RtlTcpConfig) -> RtlTcpSource {
    let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
    src.start_manager().unwrap();
    src
}

/// Server half of one RTLX handshake: accept, read the client hello
/// (forwarded on the returned channel), let `on_hello` run any
/// per-test exchange against the socket, then answer with
/// `dongle_info_t` plus the `ServerExtension` it returns and hold the
/// socket open for `RTLX_TEST_SERVER_HOLD` so the client reads the
/// extension body before EOF.
fn rtlx_serve_one_with<F>(
    listener: TcpListener,
    on_hello: F,
) -> (JoinHandle<()>, mpsc::Receiver<[u8; CLIENT_HELLO_LEN]>)
where
    F: FnOnce(&mut TcpStream, &[u8; CLIENT_HELLO_LEN]) -> ServerExtension + Send + 'static,
{
    let (hello_tx, hello_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(RTLX_TEST_STATE_DEADLINE))
            .unwrap();
        let mut hello = [0u8; CLIENT_HELLO_LEN];
        sock.read_exact(&mut hello).expect("read hello");
        let _ = hello_tx.send(hello);
        let ext = on_hello(&mut sock, &hello);
        // Hello-only tests stop the client as soon as the hello is
        // captured, so the reply may hit a closed socket; that is not
        // a fixture failure (the assertions above are).
        let _ = sock.write_all(&rtlx_test_dongle_info());
        let _ = sock.write_all(&ext.to_bytes());
        thread::sleep(RTLX_TEST_SERVER_HOLD);
    });
    (handle, hello_rx)
}

/// Accept `sessions` RTLX clients, complete each handshake with an LZ4
/// grant and then send nothing, holding every socket for `hold`.
fn rtlx_serve_silent_lz4_sessions(
    listener: TcpListener,
    sessions: usize,
    hold: Duration,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut socks = Vec::new();
        for _ in 0..sessions {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut hello_buf = [0u8; CLIENT_HELLO_LEN];
            sock.read_exact(&mut hello_buf).expect("read hello");
            sock.write_all(&rtlx_test_dongle_info()).unwrap();
            let ext = rtlx_ok_extension(Codec::Lz4, Role::Control, PROTOCOL_VERSION);
            sock.write_all(&ext.to_bytes()).unwrap();
            socks.push(sock);
        }
        thread::sleep(hold);
    })
}

/// Join a loopback-server fixture thread, failing the test if an
/// assertion inside the fixture panicked (otherwise a server-side
/// failure only shows up as an unrelated client timeout).
fn join_server(handle: JoinHandle<()>) {
    handle.join().expect("loopback server fixture panicked");
}

/// The hello the loopback server captured, or a panic past the deadline.
fn recv_hello(rx: &mpsc::Receiver<[u8; CLIENT_HELLO_LEN]>) -> [u8; CLIENT_HELLO_LEN] {
    rx.recv_timeout(RTLX_TEST_STATE_DEADLINE)
        .expect("server should receive hello within deadline")
}

/// Is the manager currently in `ConnectionState::Connected`?
fn is_connected(src: &RtlTcpSource) -> bool {
    matches!(src.connection_state(), ConnectionState::Connected { .. })
}

/// Poll `cond` every `RTLX_TEST_POLL_INTERVAL` until it holds or
/// `deadline` elapses; `true` iff it held.
fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        if cond() {
            return true;
        }
        thread::sleep(RTLX_TEST_POLL_INTERVAL);
    }
    false
}

/// Read with a short timeout to see whether the client sends anything
/// it should not; restores the blocking read afterwards.
fn probe_for_client_bytes(sock: &mut TcpStream) -> std::io::Result<usize> {
    sock.set_read_timeout(Some(NO_HELLO_PROBE_TIMEOUT)).unwrap();
    let mut probe_buf = [0u8; NO_HELLO_PROBE_LEN];
    let result = sock.read(&mut probe_buf);
    sock.set_read_timeout(None).unwrap();
    result
}

/// A probe read that timed out or hit EOF saw nothing; any bytes (or
/// another error) fail the test with `what`.
fn assert_probe_saw_nothing(result: std::io::Result<usize>, what: &str) {
    match result {
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
        Ok(0) => {}
        Ok(n) => panic!("{what}, but the server observed {n} byte(s) from the client"),
        Err(e) => panic!("unexpected server probe error: {e:?}"),
    }
}

/// Read up to `count` wire commands from the client and forward them.
fn forward_commands(sock: &mut TcpStream, count: usize, tx: &mpsc::Sender<Command>) {
    const COMMAND_READ_TIMEOUT: Duration = Duration::from_secs(2);
    const COMMAND_DEADLINE: Duration = Duration::from_secs(1);
    sock.set_read_timeout(Some(COMMAND_READ_TIMEOUT)).unwrap();
    let mut got = 0;
    let deadline = Instant::now() + COMMAND_DEADLINE;
    while got < count && Instant::now() < deadline {
        let mut buf = [0u8; COMMAND_LEN];
        if sock.read_exact(&mut buf).is_err() {
            break;
        }
        if let Some(cmd) = Command::from_bytes(&buf) {
            let _ = tx.send(cmd);
            got += 1;
        }
    }
}

/// Hammer the shared command sink with `SetCenterFreq` writes until
/// `flooding` is cleared or the sink goes away.
fn spawn_command_flooder(shared: Arc<SharedState>, flooding: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut hz = 100_000_000u32;
        while flooding.load(Ordering::Relaxed) {
            if let Ok(mut sink) = shared.command_sink.lock() {
                let Some(stream) = sink.as_mut() else { break };
                let cmd = Command {
                    op: CommandOp::SetCenterFreq,
                    param: hz,
                };
                if stream.write_all(&cmd.to_bytes()).is_err() {
                    break;
                }
                hz = hz.wrapping_add(1);
            }
        }
    })
}

mod commands;
mod data_pump;
mod handshake;
mod handshake_flags;
