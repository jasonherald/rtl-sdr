#![allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_precision_loss,
    clippy::unnecessary_wraps,
    clippy::items_after_statements,
    clippy::collapsible_if,
    clippy::match_same_arms
)]
//! `sdr-rtl-tcp` — rtl_tcp-compatible CLI server.
//!
//! Faithful port of `original/librtlsdr/src/rtl_tcp.c` `main()` — same
//! short options, same defaults where practical, with one change noted
//! in the review of epic #299:
//!
//! - Upstream defaults to `-a 127.0.0.1` (loopback). We keep that.
//! - Upstream and we both treat `-g 0` as "automatic gain mode" (no
//!   manual gain set); see `parse_auto_gain_is_none` test for the
//!   exact behavior.
//!
//! **Unix-only binary.** The server library (`sdr-server-rtltcp` lib)
//! builds on any platform, but the CLI uses POSIX `libc::signal` for
//! Ctrl-C / graceful shutdown and has no equivalent path elsewhere.
//! Non-Unix targets trip the `compile_error!` below — Windows support
//! would need a `SetConsoleCtrlHandler`-based handler.

#[cfg(not(unix))]
compile_error!(
    "sdr-rtl-tcp requires a Unix target (uses POSIX signals for graceful shutdown). \
     Windows support would need a SetConsoleCtrlHandler-based path; \
     file an issue if that's needed."
);

use std::net::IpAddr;
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sdr_rtltcp_discovery::{AdvertiseOptions, Advertiser, TxtRecord, local_hostname};
use sdr_server_rtltcp::server::{DEFAULT_SAMPLE_RATE_HZ, Server, ServerConfig};
use sdr_server_rtltcp::{DEFAULT_BUFFER_CAPACITY, DEFAULT_LISTENER_CAP, DEFAULT_PORT};

/// How often `main` polls the shutdown / server-stopped flags. Below
/// any user-perceptible latency; costs a few syscalls per second while
/// the server is idle.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Direct-sampling mode the upstream `-D` flag activates.
/// Upstream rtl_tcp.c hard-codes `2` (Q branch); we match that
/// exactly.
const DIRECT_SAMPLING_Q_BRANCH: i32 = 2;

/// Print the `--help` / `-h` message and exit with the given status code.
///
/// Exit 0 when invoked via `-h`/`--help` (successful help request), exit 1
/// for unknown / malformed arguments. Shells and packaging checks treat a
/// non-zero exit from `--help` as a failure, so those paths must diverge.
fn usage(exit_code: i32) -> ! {
    let out: &mut dyn std::io::Write = if exit_code == 0 {
        &mut std::io::stdout()
    } else {
        &mut std::io::stderr()
    };
    let _ = writeln!(out, "sdr-rtl-tcp — I/Q spectrum server for RTL-SDR dongles");
    let _ = writeln!(out);
    let _ = writeln!(out, "USAGE:");
    let _ = writeln!(out, "    sdr-rtl-tcp [OPTIONS]");
    let _ = writeln!(out);
    let _ = writeln!(out, "OPTIONS:");
    let _ = writeln!(out, "    -a <addr>    Listen address (default: 127.0.0.1)");
    let _ = writeln!(
        out,
        "    -p <port>    Listen port (default: {DEFAULT_PORT})"
    );
    let _ = writeln!(
        out,
        "    -f <hz>      Initial center frequency (accepts k/M/G suffix)"
    );
    let _ = writeln!(
        out,
        "    -g <gain>    Tuner gain in dB (default: 0 = automatic)"
    );
    let _ = writeln!(
        out,
        "    -s <rate>    Sample rate in Hz (default: {DEFAULT_SAMPLE_RATE_HZ})"
    );
    let _ = writeln!(out, "    -d <idx>     Device index (default: 0)");
    let _ = writeln!(
        out,
        "    -P <ppm>     Frequency correction in ppm (default: 0)"
    );
    let _ = writeln!(
        out,
        "    -n <N>       Max queued USB buffers, ~256 KiB each (default: {DEFAULT_BUFFER_CAPACITY})"
    );
    let _ = writeln!(out, "    -T           Enable bias tee");
    let _ = writeln!(
        out,
        "    -D           Enable direct sampling (mode 2, Q branch)"
    );
    let _ = writeln!(
        out,
        "    -N <name>    mDNS nickname for this server (defaults to hostname)"
    );
    let _ = writeln!(
        out,
        "    --no-announce  Skip mDNS advertisement (default: advertise)"
    );
    let _ = writeln!(
        out,
        "    --listener-cap <N>  Max concurrent Role::Listen clients \
         (default: {DEFAULT_LISTENER_CAP}; the single Control client is \
         separate)"
    );
    let _ = writeln!(
        out,
        "    --auth-key <HEX>  Enable pre-shared-key auth (#394). \
         HEX is the key bytes as an even-length hex string (e.g. \
         `openssl rand -hex 32`). Clients must present the same \
         key at handshake; wrong / missing keys are denied. \
         Default: disabled. LAN-grade trust model only — wrap in \
         SSH/WG for real confidentiality."
    );
    let _ = writeln!(out, "    -h, --help   Show this help");
    std::process::exit(exit_code);
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    // -h / --help asked for help explicitly — exit 0. Anything else that
    // fails to parse is a user error — exit 1.
    if args.iter().any(|a| a == "-h" || a == "--help") {
        usage(0);
    }
    let Ok(parsed) = parse_args(&args) else {
        usage(1);
    };
    let (config, discovery) = parsed;

    match Server::start(config) {
        Ok(server) => {
            tracing::info!(bind = %server.bind_address(), "rtl_tcp server running");

            // Advertise via mDNS if the user didn't opt out. Keep the
            // Advertiser alive for the rest of main's scope so its
            // Drop-based unregister fires on normal shutdown.
            let _advertiser = if discovery.announce {
                match announce_over_mdns(&server, &discovery) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        tracing::warn!(%e, "mDNS advertise failed — server is running but won't be auto-discovered");
                        None
                    }
                }
            } else {
                tracing::info!("mDNS advertise disabled (--no-announce)");
                None
            };

            // Install the signal handler BEFORE entering the sleep loop.
            // The loop has no alternate shutdown path, so a failed install
            // leaves the process unkillable without SIGKILL — fatal, not
            // a warning.
            if let Err(e) = ctrlc_handler() {
                tracing::error!(%e, "ctrl-c handler setup failed — aborting");
                drop(server);
                return ExitCode::FAILURE;
            }
            // Poll the shutdown flag instead of using an mpsc channel,
            // because the signal handler can only safely touch an atomic.
            // Also poll `server.has_stopped()` so the CLI exits when the
            // accept thread exits on its own (e.g., dongle unplug);
            // otherwise the process would sleep forever after serving
            // stopped. 100 ms poll is well below any user-perceptible
            // shutdown lag and costs nothing while idle.
            while !CTRL_C_RECEIVED.load(Ordering::SeqCst) && !server.has_stopped() {
                std::thread::sleep(SHUTDOWN_POLL_INTERVAL);
            }
            if server.has_stopped() {
                tracing::info!("rtl_tcp server stopped serving — exiting");
            } else {
                tracing::info!("shutting down");
            }
            drop(server);
            ExitCode::SUCCESS
        }
        Err(e) => {
            // Route through the configured tracing subscriber so this
            // startup failure gets structured fields + the same output
            // sink as the rest of the crate (per-workspace rule: no
            // `println!` / `eprintln!` in crates).
            tracing::error!(%e, "sdr-rtl-tcp: server startup failed");
            ExitCode::FAILURE
        }
    }
}

/// Shared flag that the SIGINT/SIGTERM handler sets, which `main` polls.
///
/// Using an `AtomicBool` rather than an mpsc channel because `Mutex::lock`
/// and `mpsc::Sender::send` are NOT async-signal-safe per POSIX — locking
/// from inside a signal handler can deadlock or corrupt the mutex state
/// if the signal fires while the main thread holds the lock.
/// `AtomicBool::store` IS async-signal-safe (single atomic hardware op,
/// no allocation, no locks).
static CTRL_C_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Install a minimal SIGINT/SIGTERM handler. Matches the upstream pattern
/// at `rtl_tcp.c:477-488`, but without the non-async-signal-safe locking.
///
/// Returns `Err` if either `signal()` call fails — the caller MUST treat
/// this as fatal, because the sleep loop in `main` has no alternate
/// shutdown path and would leave the process unresponsive to Ctrl-C.
///
/// Only defined on Unix — non-Unix targets trip the `compile_error!`
/// at the top of this file.
#[allow(unsafe_code)]
fn ctrlc_handler() -> std::io::Result<()> {
    extern "C" fn handler(_: libc::c_int) {
        // Async-signal-safe: atomic store, no allocation, no locks.
        CTRL_C_RECEIVED.store(true, Ordering::SeqCst);
    }
    // SAFETY: `handler` has the correct `extern "C" fn(c_int)` signature
    // for a POSIX signal handler and touches only an AtomicBool, which
    // is on the POSIX async-signal-safe list.
    let handler_ptr = handler as *const () as libc::sighandler_t;
    unsafe {
        if libc::signal(libc::SIGINT, handler_ptr) == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
        if libc::signal(libc::SIGTERM, handler_ptr) == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[derive(Debug)]
struct ParseError;

/// CLI-only mDNS settings. Lives alongside `ServerConfig` in the parsed
/// result because the server crate deliberately doesn't know about
/// discovery (different dep tree).
#[derive(Debug, Clone)]
struct DiscoveryOptions {
    /// `--no-announce` flips this off. Default: advertise.
    announce: bool,
    /// `-N <name>` override. None → derive from system hostname at
    /// advertise time.
    nickname: Option<String>,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            announce: true,
            nickname: None,
        }
    }
}

fn parse_args<S: AsRef<str>>(args: &[S]) -> Result<(ServerConfig, DiscoveryOptions), ParseError> {
    // `default_loopback()` seeds `initial` via `InitialDeviceState::default()`,
    // which already uses `DEFAULT_SAMPLE_RATE_HZ`. No need to re-assign here.
    let mut config = ServerConfig::default_loopback();
    let mut discovery = DiscoveryOptions::default();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_ref();
        match arg {
            "-h" | "--help" => return Err(ParseError),
            "-a" => {
                let v = args.get(i + 1).ok_or(ParseError)?.as_ref();
                let ip = IpAddr::from_str(v).map_err(|_| ParseError)?;
                config.bind.set_ip(ip);
                i += 2;
            }
            "-p" => {
                let v = args.get(i + 1).ok_or(ParseError)?.as_ref();
                let port: u16 = v.parse().map_err(|_| ParseError)?;
                config.bind.set_port(port);
                i += 2;
            }
            "-f" => {
                let v = args.get(i + 1).ok_or(ParseError)?.as_ref();
                config.initial.center_freq_hz = parse_hz(v)?;
                i += 2;
            }
            "-g" => {
                let v = args.get(i + 1).ok_or(ParseError)?.as_ref();
                // Upstream: `gain = (int)(atof(optarg) * 10)`. Value 0
                // means automatic gain mode; anything else is tenths-of-dB.
                //
                // Explicitly reject NaN / ±Inf and oversized finite values:
                //   - NaN / ±Inf: `f64 as i32` silently converts NaN → 0
                //     (which would switch to auto gain) and ±Inf → saturating
                //     i32 bounds.
                //   - Oversized finite (e.g., 1e100): also saturates silently
                //     to i32::MAX, producing an absurd gain setting from what
                //     parsed as a valid float. Range-check before the cast so
                //     garbage input surfaces as a parse error instead.
                let db: f64 = v.parse().map_err(|_| ParseError)?;
                if !db.is_finite() {
                    return Err(ParseError);
                }
                let tenths_f = (db * 10.0).round();
                if tenths_f < i32::MIN as f64 || tenths_f > i32::MAX as f64 {
                    return Err(ParseError);
                }
                let tenths = tenths_f as i32;
                config.initial.gain_tenths_db = if tenths == 0 { None } else { Some(tenths) };
                i += 2;
            }
            "-s" => {
                let v = args.get(i + 1).ok_or(ParseError)?.as_ref();
                let sample_rate_hz = parse_hz(v)?;
                // Reject 0 up front: `parse_hz` accepts it as a valid
                // non-negative u32, but a zero sample rate wedges the
                // RTL-SDR USB controller. Same rule that `Source::
                // set_sample_rate` in the client uses.
                if sample_rate_hz == 0 {
                    return Err(ParseError);
                }
                config.initial.sample_rate_hz = sample_rate_hz;
                i += 2;
            }
            "-d" => {
                let v = args.get(i + 1).ok_or(ParseError)?.as_ref();
                config.device_index = v.parse().map_err(|_| ParseError)?;
                i += 2;
            }
            "-P" => {
                let v = args.get(i + 1).ok_or(ParseError)?.as_ref();
                config.initial.ppm = v.parse().map_err(|_| ParseError)?;
                i += 2;
            }
            "-n" => {
                let v = args.get(i + 1).ok_or(ParseError)?.as_ref();
                config.buffer_capacity = v.parse().map_err(|_| ParseError)?;
                i += 2;
            }
            "-T" => {
                config.initial.bias_tee = true;
                i += 1;
            }
            "-D" => {
                // Upstream hardcodes direct-sampling mode 2 (Q branch).
                config.initial.direct_sampling = DIRECT_SAMPLING_Q_BRANCH;
                i += 1;
            }
            "-N" => {
                let v = args.get(i + 1).ok_or(ParseError)?.as_ref();
                discovery.nickname = Some(v.to_string());
                i += 2;
            }
            "--no-announce" => {
                discovery.announce = false;
                i += 1;
            }
            "--listener-cap" => {
                let v = args.get(i + 1).ok_or(ParseError)?.as_ref();
                config.listener_cap = v.parse().map_err(|_| ParseError)?;
                i += 2;
            }
            "--auth-key" => {
                let v = args.get(i + 1).ok_or(ParseError)?.as_ref();
                config.auth_key = Some(parse_auth_key_hex(v)?);
                i += 2;
            }
            _ => return Err(ParseError),
        }
    }

    Ok((config, discovery))
}

/// Decode a hex-encoded auth key from the `--auth-key` CLI arg.
/// Accepts even-length hex (e.g. `0a1b2c...`) and produces the raw
/// bytes. Rejects odd length, non-hex chars, empty input, or keys
/// longer than [`sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN`].
/// Hex chosen over base64 for CLI parsing — one fewer alphabet to
/// remember, no URL-safety concerns, easy to generate with
/// `openssl rand -hex 32`. #394.
fn parse_auth_key_hex(s: &str) -> Result<Vec<u8>, ParseError> {
    /// Number of hex chars per raw byte — two. Pulled out so the
    /// `chunks_exact` call reads intent-first instead of bare `2`.
    const HEX_CHARS_PER_BYTE: usize = 2;

    // Reject non-ASCII BEFORE any byte-index slicing. A 4-byte
    // emoji like "💩" passes `len().is_multiple_of(2)` but
    // indexing `&s[i..i+2]` on a UTF-8 str would panic at the
    // non-char-boundary offset. `is_ascii()` fast-paths on the
    // first non-ASCII byte without a full scan. Per `CodeRabbit`
    // round 1 on PR #405.
    if s.is_empty() || !s.is_ascii() || !s.len().is_multiple_of(HEX_CHARS_PER_BYTE) {
        return Err(ParseError);
    }
    // With ASCII confirmed, the string is also a valid byte
    // slice — `as_chunks::<2>()` walks pairs without str-slicing
    // boundary concerns (the length check above guarantees an
    // empty remainder). Per-char hex-digit validation keeps
    // the error surface narrow (same "bad input" signal for
    // "not hex" and "not ASCII").
    let bytes: Result<Vec<u8>, ParseError> = s
        .as_bytes()
        .as_chunks::<HEX_CHARS_PER_BYTE>()
        .0
        .iter()
        .map(|&[hi, lo]| {
            let hi = char::from(hi).to_digit(16).ok_or(ParseError)?;
            let lo = char::from(lo).to_digit(16).ok_or(ParseError)?;
            // `to_digit(16)` only returns 0..=15, so `hi << 4 | lo`
            // fits in u8. The `as u8` is lossless here.
            #[allow(
                clippy::cast_possible_truncation,
                reason = "to_digit(16) bounds hi and lo to 0..=15"
            )]
            Ok(((hi << 4) | lo) as u8)
        })
        .collect();
    let bytes = bytes?;
    // Upper bound matches the wire-format `MAX_AUTH_KEY_LEN`. A
    // CLI-supplied key beyond that would fail at
    // `AuthKeyMessage::to_bytes` anyway; reject here so the
    // diagnostic lands as a clean argv-parse error rather than a
    // runtime handshake failure.
    if bytes.len() > sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN {
        return Err(ParseError);
    }
    Ok(bytes)
}

/// Register the running server in mDNS so clients can discover it
/// without manual host:port entry. Builds the instance name from the
/// user-provided nickname (falling back to "sdr-rtl-tcp") and pulls
/// tuner metadata from `Server::tuner_info()`.
fn announce_over_mdns(
    server: &Server,
    discovery: &DiscoveryOptions,
) -> Result<Advertiser, sdr_rtltcp_discovery::DiscoveryError> {
    let tuner = server.tuner_info();
    // Fallback nickname: system hostname via libc::gethostname.
    // Previously this was hardcoded to "sdr-rtl-tcp" — which meant
    // two stock servers on the same LAN would show up as the same
    // label in the discovery list (and could collide on the DNS-SD
    // instance name). Matches the help text's "defaults to hostname"
    // promise.
    let nickname = discovery.nickname.clone().unwrap_or_else(local_hostname);
    let txt = TxtRecord {
        tuner: tuner.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        gains: tuner.gain_count,
        nickname: nickname.clone(),
        txbuf: None,
        // CLI server advertises whatever mask the user configured,
        // so compression-capable clients can decide up-front
        // whether to send an extended hello. #307.
        codecs: Some(server.compression().to_wire()),
        // CLI surfaces auth via `--auth-key HEX`; advertise
        // `auth_required=true` iff the server has a key configured
        // so mDNS browsers can prompt for a key before connecting.
        // Read from the server (not the raw args) because
        // `Server::set_auth_key` could in principle flip the state
        // between the construction here and a future live-update
        // call; `Server::auth_required()` is the single source of
        // truth. #394 + #395.
        auth_required: server.auth_required().then_some(true),
    };
    Advertiser::announce(AdvertiseOptions {
        port: server.bind_address().port(),
        instance_name: format!("{nickname} rtl-sdr"),
        hostname: String::new(), // auto-derive in the advertiser
        txt,
    })
}

/// Port of upstream `atofs()` — accepts `k` / `M` / `G` suffix multipliers.
/// Returns Hz as u32.
fn parse_hz(s: &str) -> Result<u32, ParseError> {
    let (number, multiplier) =
        if let Some(stripped) = s.strip_suffix('k').or_else(|| s.strip_suffix('K')) {
            (stripped, 1_000u64)
        } else if let Some(stripped) = s.strip_suffix('M') {
            (stripped, 1_000_000u64)
        } else if let Some(stripped) = s.strip_suffix('G') {
            (stripped, 1_000_000_000u64)
        } else {
            (s, 1u64)
        };
    let n: f64 = number.parse().map_err(|_| ParseError)?;
    // Reject NaN / ±Inf AND any negative input outright before rounding.
    // Without the negative-guard, `parse_hz("-0.4")` would round-to-zero
    // and pass the u32 range check as a valid 0 Hz frequency. Without
    // the is_finite guard, `f64 as u32` silently converts NaN → 0 and
    // ±Inf → saturating u32 bounds — either could turn garbage input
    // into plausible-looking output.
    if !n.is_finite() || n < 0.0 {
        return Err(ParseError);
    }
    let hz = (n * multiplier as f64).round();
    if hz > f64::from(u32::MAX) {
        return Err(ParseError);
    }
    Ok(hz as u32)
}

// A bin crate root resolves child modules next to itself, so point at
// the sibling directory explicitly.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "sdr-rtl-tcp/tests.rs"]
mod tests;
