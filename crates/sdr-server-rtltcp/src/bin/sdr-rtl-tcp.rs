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
//! - Upstream rejects `-g 0` (auto-gain) and requires an explicit value.
//!   We follow upstream: `gain == 0` means automatic.

use std::net::IpAddr;
use std::process::ExitCode;
use std::str::FromStr;

use sdr_server_rtltcp::server::{Server, ServerConfig};
use sdr_server_rtltcp::{DEFAULT_BUFFER_CAPACITY, DEFAULT_PORT};

fn usage() -> ! {
    eprintln!("sdr-rtl-tcp — I/Q spectrum server for RTL-SDR dongles");
    eprintln!();
    eprintln!("USAGE:");
    eprintln!("    sdr-rtl-tcp [OPTIONS]");
    eprintln!();
    eprintln!("OPTIONS:");
    eprintln!("    -a <addr>    Listen address (default: 127.0.0.1)");
    eprintln!("    -p <port>    Listen port (default: {DEFAULT_PORT})");
    eprintln!("    -f <hz>      Initial center frequency (accepts k/M/G suffix)");
    eprintln!("    -g <gain>    Tuner gain in dB (default: 0 = automatic)");
    eprintln!("    -s <rate>    Sample rate in Hz (default: 2048000)");
    eprintln!("    -d <idx>     Device index (default: 0)");
    eprintln!("    -P <ppm>     Frequency correction in ppm (default: 0)");
    eprintln!("    -n <N>       Max queued USB buffers (default: {DEFAULT_BUFFER_CAPACITY})");
    eprintln!("    -T           Enable bias tee");
    eprintln!("    -D           Enable direct sampling (mode 2, Q branch)");
    eprintln!("    -h, --help   Show this help");
    std::process::exit(1);
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Ok(config) = parse_args(&args) else {
        usage();
    };

    match Server::start(config) {
        Ok(server) => {
            tracing::info!(bind = %server.bind_address(), "rtl_tcp server running");
            // Sit in the foreground until Ctrl-C. On SIGINT, dropping the
            // Server handle drives shutdown.
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            if let Err(e) = ctrlc_handler(tx) {
                tracing::warn!(%e, "ctrl-c handler setup failed — kill the process manually");
            }
            let _ = rx.recv();
            tracing::info!("shutting down");
            drop(server);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sdr-rtl-tcp: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Install a minimal SIGINT/SIGTERM handler that signals the main thread
/// to stop. Uses the same pattern as `rtl_tcp.c:477-488`.
#[cfg(unix)]
#[allow(unsafe_code)]
fn ctrlc_handler(tx: std::sync::mpsc::Sender<()>) -> std::io::Result<()> {
    use std::sync::Mutex;
    static SENDER: Mutex<Option<std::sync::mpsc::Sender<()>>> = Mutex::new(None);
    if let Ok(mut slot) = SENDER.lock() {
        *slot = Some(tx);
    }
    extern "C" fn handler(_: libc::c_int) {
        if let Ok(slot) = SENDER.lock() {
            if let Some(tx) = slot.as_ref() {
                let _ = tx.send(());
            }
        }
    }
    // SAFETY: `handler` is `extern "C"` with the correct signature for a
    // POSIX signal handler. We install it for SIGINT / SIGTERM only; the
    // handler body uses a Mutex + mpsc Sender, both of which are safe to
    // call from a signal context for this workload (mpsc send is
    // non-allocating when the receiver is alive; we don't care if a
    // pathological double-ctrl-c hits the locked path because the handler
    // just returns without sending).
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
    }
    Ok(())
}

#[cfg(not(unix))]
fn ctrlc_handler(_tx: std::sync::mpsc::Sender<()>) -> std::io::Result<()> {
    Ok(())
}

#[derive(Debug)]
struct ParseError;

fn parse_args(args: &[String]) -> Result<ServerConfig, ParseError> {
    let mut config = ServerConfig::default_loopback();
    config.initial.sample_rate_hz = 2_048_000;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-h" | "--help" => return Err(ParseError),
            "-a" => {
                let v = args.get(i + 1).ok_or(ParseError)?;
                let ip = IpAddr::from_str(v).map_err(|_| ParseError)?;
                config.bind.set_ip(ip);
                i += 2;
            }
            "-p" => {
                let v = args.get(i + 1).ok_or(ParseError)?;
                let port: u16 = v.parse().map_err(|_| ParseError)?;
                config.bind.set_port(port);
                i += 2;
            }
            "-f" => {
                let v = args.get(i + 1).ok_or(ParseError)?;
                config.initial.center_freq_hz = parse_hz(v)?;
                i += 2;
            }
            "-g" => {
                let v = args.get(i + 1).ok_or(ParseError)?;
                // Upstream: `gain = (int)(atof(optarg) * 10)`. Value 0
                // means automatic gain mode; anything else is tenths-of-dB.
                let db: f64 = v.parse().map_err(|_| ParseError)?;
                let tenths = (db * 10.0).round() as i32;
                config.initial.gain_tenths_db = if tenths == 0 { None } else { Some(tenths) };
                i += 2;
            }
            "-s" => {
                let v = args.get(i + 1).ok_or(ParseError)?;
                config.initial.sample_rate_hz = parse_hz(v)?;
                i += 2;
            }
            "-d" => {
                let v = args.get(i + 1).ok_or(ParseError)?;
                config.device_index = v.parse().map_err(|_| ParseError)?;
                i += 2;
            }
            "-P" => {
                let v = args.get(i + 1).ok_or(ParseError)?;
                config.initial.ppm = v.parse().map_err(|_| ParseError)?;
                i += 2;
            }
            "-n" => {
                let v = args.get(i + 1).ok_or(ParseError)?;
                config.buffer_capacity = v.parse().map_err(|_| ParseError)?;
                i += 2;
            }
            "-T" => {
                config.initial.bias_tee = true;
                i += 1;
            }
            "-D" => {
                // Upstream hardcodes direct-sampling mode 2 (Q branch).
                config.initial.direct_sampling = 2;
                i += 1;
            }
            _ => return Err(ParseError),
        }
    }

    Ok(config)
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
    let hz = (n * multiplier as f64).round() as i64;
    if hz < 0 || hz > u32::MAX as i64 {
        return Err(ParseError);
    }
    Ok(hz as u32)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_hz_plain() {
        assert_eq!(parse_hz("2048000").unwrap(), 2_048_000);
    }

    #[test]
    fn parse_hz_kilo() {
        assert_eq!(parse_hz("2400k").unwrap(), 2_400_000);
        assert_eq!(parse_hz("2400K").unwrap(), 2_400_000);
    }

    #[test]
    fn parse_hz_mega() {
        assert_eq!(parse_hz("100M").unwrap(), 100_000_000);
        assert_eq!(parse_hz("100.5M").unwrap(), 100_500_000);
    }

    #[test]
    fn parse_hz_giga() {
        assert_eq!(parse_hz("1G").unwrap(), 1_000_000_000);
    }

    #[test]
    fn parse_defaults_are_loopback_and_upstream_port() {
        let cfg = parse_args(&[]).unwrap();
        assert_eq!(cfg.bind.ip().to_string(), "127.0.0.1");
        assert_eq!(cfg.bind.port(), DEFAULT_PORT);
    }

    #[test]
    fn parse_auto_gain_is_none() {
        let args = vec!["-g".to_string(), "0".to_string()];
        let cfg = parse_args(&args).unwrap();
        assert!(cfg.initial.gain_tenths_db.is_none());
    }

    #[test]
    fn parse_manual_gain_rounds_to_tenths() {
        let args = vec!["-g".to_string(), "28.2".to_string()];
        let cfg = parse_args(&args).unwrap();
        assert_eq!(cfg.initial.gain_tenths_db, Some(282));
    }

    #[test]
    fn parse_direct_sampling_hardcodes_mode_2() {
        let args = vec!["-D".to_string()];
        let cfg = parse_args(&args).unwrap();
        assert_eq!(cfg.initial.direct_sampling, 2);
    }

    #[test]
    fn parse_bind_override() {
        let args = vec!["-a".to_string(), "0.0.0.0".to_string()];
        let cfg = parse_args(&args).unwrap();
        assert_eq!(cfg.bind.ip().to_string(), "0.0.0.0");
    }
}
