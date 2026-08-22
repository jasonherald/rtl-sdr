#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::needless_range_loop,
    clippy::redundant_closure_for_method_calls,
    clippy::unnecessary_literal_bound,
    clippy::doc_markdown,
    clippy::manual_midpoint,
    clippy::redundant_closure
)]
//! TCP/UDP network IQ source module.
//!
//! Ports SDR++ `NetworkSourceModule`. Receives IQ samples over
//! TCP (client) or UDP connections with configurable sample format.
//!
//! Also hosts an `rtl_tcp`-protocol client (see [`rtl_tcp`]) that
//! connects to any `rtl_tcp`-compatible server (GQRX, SDR++,
//! [`sdr-server-rtltcp`]) and streams 8-bit I/Q with tuning command
//! support.

pub mod rtl_tcp;

pub use rtl_tcp::{
    ConnectionState, DEFAULT_CONNECT_TIMEOUT, DEFAULT_DATA_READ_TIMEOUT,
    DEFAULT_MAX_CONSECUTIVE_TIMEOUTS, RtlTcpConfig, RtlTcpSource, TunerInfo,
};

use sdr_pipeline::source_manager::Source;
use sdr_types::{Complex, Protocol, SampleFormat, SourceError};
use std::io::Read;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// Bound on `TcpStream::connect` so a blackholed host cannot park the DSP
/// thread in `start()` (#744).
pub const DEFAULT_NETWORK_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on a single socket read so a silent peer cannot park the DSP
/// thread inside `read_samples` — UI commands are only drained between
/// blocks, so Stop / sample-rate changes / quit would hang (#744). A
/// timeout surfaces as "no data" (`Ok(0)`), not an error.
pub const DEFAULT_NETWORK_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// Receive buffer for UDP datagrams. `recvfrom` silently truncates a
/// datagram larger than the buffer (no `MSG_TRUNC` check in std), so the
/// buffer is sized for the largest sender we expect rather than for the
/// caller's output slice; matches SDR++'s 4 MB (#744).
pub const UDP_RECV_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// TCP read chunk size per `read_samples` call.
const TCP_RECV_CHUNK_BYTES: usize = 64 * 1024;

/// Network IQ source for the pipeline.
///
/// Receives complex IQ samples over TCP or UDP with format conversion.
/// Carries incomplete sample bytes across calls to prevent stream misalignment.
pub struct NetworkSource {
    hostname: String,
    port: u16,
    protocol: Protocol,
    sample_format: SampleFormat,
    sample_rate: f64,
    frequency: f64,
    connection: Option<NetworkConnection>,
    connect_timeout: Duration,
    read_timeout: Duration,
    resolver: DeadlineResolver,
    // Pre-allocated receive buffer (reused across calls)
    recv_buf: Vec<u8>,
    /// Bytes received but not yet converted: a partial sample from a TCP
    /// read, or the tail of a UDP datagram larger than the caller's
    /// output slice. Drained in sample-size units on later calls.
    carry_buf: Vec<u8>,
}

enum NetworkConnection {
    Tcp(TcpStream),
    Udp(UdpSocket),
}

/// Hostname resolution bounded by a deadline, single-flight per source.
///
/// `to_socket_addrs` is a blocking DNS lookup with no timeout of its own;
/// on the DSP thread that would hold Stop / quit hostage (#744). The lookup
/// runs on a helper thread and the caller waits only until the deadline.
/// A lookup that outlives the deadline is NOT abandoned and re-spawned on
/// the next `start()`: it stays in flight and the next call for the same
/// target waits on it, so repeated starts against a slow resolver hold at
/// most one helper thread per source (CR round 2 on PR #793). A different
/// target drops the stale receiver; the old helper finishes on its own.
#[derive(Default)]
struct DeadlineResolver {
    in_flight: Option<(
        String,
        std::sync::mpsc::Receiver<std::io::Result<Vec<SocketAddr>>>,
    )>,
}

impl DeadlineResolver {
    fn resolve<F>(
        &mut self,
        target: &str,
        resolve: F,
        deadline: Instant,
    ) -> std::io::Result<Vec<SocketAddr>>
    where
        F: FnOnce() -> std::io::Result<Vec<SocketAddr>> + Send + 'static,
    {
        if self
            .in_flight
            .as_ref()
            .is_none_or(|(pending, _)| pending != target)
        {
            let (tx, rx) = std::sync::mpsc::channel();
            // `Builder::spawn` reports thread-creation failure instead of
            // panicking like `thread::spawn` (library code: no panics).
            std::thread::Builder::new()
                .name("sdr-net-resolve".into())
                .spawn(move || {
                    let _ = tx.send(resolve());
                })
                .map_err(|e| {
                    std::io::Error::other(format!("could not start the resolver thread: {e}"))
                })?;
            self.in_flight = Some((target.to_string(), rx));
        }
        let Some((_, rx)) = self.in_flight.as_ref() else {
            return Err(std::io::Error::other("resolver state lost"));
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(result) => {
                self.in_flight = None;
                result
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "hostname resolution exceeded the connect timeout",
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.in_flight = None;
                Err(std::io::Error::other(
                    "resolver thread ended without a result",
                ))
            }
        }
    }
}

/// Try each address in turn, giving every attempt only the time left
/// until `deadline`, so several unreachable addresses cannot stretch
/// `start()` past one `connect_timeout` in total.
fn connect_any_with_deadline(
    addrs: &[SocketAddr],
    deadline: Instant,
) -> std::io::Result<TcpStream> {
    let mut last_err =
        std::io::Error::new(std::io::ErrorKind::NotFound, "no address to connect to");
    for addr in addrs {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connect timeout exhausted before every address was tried",
            ));
        }
        match TcpStream::connect_timeout(addr, remaining) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

impl NetworkSource {
    /// Create a new network source.
    pub fn new(hostname: &str, port: u16, protocol: Protocol) -> Self {
        Self {
            hostname: hostname.to_string(),
            port,
            protocol,
            sample_format: SampleFormat::Int16,
            sample_rate: 1_000_000.0,
            frequency: 0.0,
            connection: None,
            connect_timeout: DEFAULT_NETWORK_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_NETWORK_READ_TIMEOUT,
            resolver: DeadlineResolver::default(),
            recv_buf: Vec::new(),
            carry_buf: Vec::new(),
        }
    }

    /// Set the sample format for incoming data. Drops any carried
    /// partial-sample bytes: they were framed for the previous format and
    /// would shift every later pair (a permanent I/Q swap) (#744).
    pub fn set_sample_format(&mut self, format: SampleFormat) {
        self.sample_format = format;
        self.carry_buf.clear();
    }

    /// Override the connect / read timeouts (see the `DEFAULT_NETWORK_*`
    /// constants). Applies to the next `start()`.
    pub fn set_timeouts(&mut self, connect: Duration, read: Duration) {
        self.connect_timeout = connect;
        self.read_timeout = read;
    }

    /// Local address of the open socket, if any.
    #[must_use]
    pub fn local_addr(&self) -> Option<SocketAddr> {
        match &self.connection {
            Some(NetworkConnection::Tcp(stream)) => stream.local_addr().ok(),
            Some(NetworkConnection::Udp(socket)) => socket.local_addr().ok(),
            None => None,
        }
    }

    /// `true` for the socket errors that mean "no data within the read
    /// timeout" rather than a broken connection.
    fn is_timeout(err: &std::io::Error) -> bool {
        matches!(
            err.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        )
    }

    /// Read samples from the network connection and convert to Complex.
    ///
    /// Returns the number of Complex samples written.
    /// Carries incomplete sample bytes across calls for TCP streams.
    pub fn read_samples_impl(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
        if output.is_empty() {
            return Ok(0);
        }
        let sample_size = self.sample_format.complex_byte_size();

        // Only hit the socket when the carry cannot satisfy at least one
        // sample; otherwise hand out what we already hold without
        // blocking (a large UDP datagram is drained over several calls).
        if self.carry_buf.len() < sample_size {
            match &mut self.connection {
                Some(NetworkConnection::Tcp(stream)) => {
                    self.recv_buf.resize(TCP_RECV_CHUNK_BYTES, 0);
                    match stream.read(&mut self.recv_buf) {
                        Ok(0) => {
                            // TCP EOF — connection closed
                            return Err(SourceError::Io(std::io::Error::from(
                                std::io::ErrorKind::UnexpectedEof,
                            )));
                        }
                        Ok(n) => self.carry_buf.extend_from_slice(&self.recv_buf[..n]),
                        Err(e) if Self::is_timeout(&e) => return Ok(0),
                        Err(e) => return Err(SourceError::Io(e)),
                    }
                }
                Some(NetworkConnection::Udp(socket)) => {
                    // Whole datagram into the large buffer; whatever the
                    // caller cannot take now is carried, not truncated.
                    self.recv_buf.resize(UDP_RECV_BUFFER_BYTES, 0);
                    match socket.recv_from(&mut self.recv_buf) {
                        Ok((n, _addr)) => self.carry_buf.extend_from_slice(&self.recv_buf[..n]),
                        Err(e) if Self::is_timeout(&e) => return Ok(0),
                        Err(e) => return Err(SourceError::Io(e)),
                    }
                }
                None => return Err(SourceError::NotRunning),
            }
        }

        // Convert only complete samples, at most `output.len()`.
        let available = self.carry_buf.len() / sample_size;
        let count = available.min(output.len());
        let complete_bytes = count * sample_size;
        convert_samples(
            &self.carry_buf[..complete_bytes],
            output,
            self.sample_format,
            count,
        );
        self.carry_buf.drain(..complete_bytes);
        Ok(count)
    }
}

/// Convert raw network bytes to Complex f32 samples.
fn convert_samples(raw: &[u8], output: &mut [Complex], format: SampleFormat, count: usize) {
    let count = count.min(output.len());
    match format {
        SampleFormat::Int8 => {
            for i in 0..count {
                let re = f32::from(raw[i * 2] as i8) / 128.0;
                let im = f32::from(raw[i * 2 + 1] as i8) / 128.0;
                output[i] = Complex::new(re, im);
            }
        }
        SampleFormat::Int16 => {
            for i in 0..count {
                let re = i16::from_le_bytes([raw[i * 4], raw[i * 4 + 1]]);
                let im = i16::from_le_bytes([raw[i * 4 + 2], raw[i * 4 + 3]]);
                output[i] = Complex::new(f32::from(re) / 32768.0, f32::from(im) / 32768.0);
            }
        }
        SampleFormat::Int32 => {
            for i in 0..count {
                let offset = i * 8;
                let re = i32::from_le_bytes([
                    raw[offset],
                    raw[offset + 1],
                    raw[offset + 2],
                    raw[offset + 3],
                ]);
                let im = i32::from_le_bytes([
                    raw[offset + 4],
                    raw[offset + 5],
                    raw[offset + 6],
                    raw[offset + 7],
                ]);
                output[i] = Complex::new(re as f32 / 2_147_483_648.0, im as f32 / 2_147_483_648.0);
            }
        }
        SampleFormat::Float32 => {
            for i in 0..count {
                let offset = i * 8;
                let re = f32::from_le_bytes([
                    raw[offset],
                    raw[offset + 1],
                    raw[offset + 2],
                    raw[offset + 3],
                ]);
                let im = f32::from_le_bytes([
                    raw[offset + 4],
                    raw[offset + 5],
                    raw[offset + 6],
                    raw[offset + 7],
                ]);
                output[i] = Complex::new(re, im);
            }
        }
    }
}

impl Source for NetworkSource {
    fn name(&self) -> &str {
        "Network"
    }

    fn start(&mut self) -> Result<(), SourceError> {
        // `(host, port).to_socket_addrs()` handles IPv6 literals (`::1`)
        // that `format!("{host}:{port}")` would mangle into `::1:1234`.
        let conn = match self.protocol {
            Protocol::TcpClient => {
                // One end-to-end deadline covers resolution AND every
                // connect attempt.
                let deadline = Instant::now() + self.connect_timeout;
                let target = format!("{}:{}", self.hostname, self.port);
                let host = self.hostname.clone();
                let port = self.port;
                let addrs = self.resolver.resolve(
                    &target,
                    move || Ok((host.as_str(), port).to_socket_addrs()?.collect()),
                    deadline,
                )?;
                if addrs.is_empty() {
                    return Err(SourceError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no address resolved for {}", self.hostname),
                    )));
                }
                let stream = connect_any_with_deadline(&addrs, deadline)?;
                stream.set_read_timeout(Some(self.read_timeout))?;
                NetworkConnection::Tcp(stream)
            }
            Protocol::Udp => {
                // The hostname is the LOCAL bind address (SDR++ semantics);
                // empty means every interface. A name (not a literal) is
                // resolved under the same deadline as the TCP path.
                let deadline = Instant::now() + self.connect_timeout;
                let bind_host = if self.hostname.is_empty() {
                    "0.0.0.0".to_string()
                } else {
                    self.hostname.clone()
                };
                let target = format!("{bind_host}:{}", self.port);
                let port = self.port;
                let addrs = self.resolver.resolve(
                    &target,
                    move || Ok((bind_host.as_str(), port).to_socket_addrs()?.collect()),
                    deadline,
                )?;
                if addrs.is_empty() {
                    return Err(SourceError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no address resolved for {}", self.hostname),
                    )));
                }
                // A slice of addresses binds the first one that works.
                let socket = UdpSocket::bind(addrs.as_slice())?;
                socket.set_read_timeout(Some(self.read_timeout))?;
                NetworkConnection::Udp(socket)
            }
        };
        self.connection = Some(conn);
        self.carry_buf.clear();
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        self.connection = None;
        self.carry_buf.clear();
        Ok(())
    }

    fn tune(&mut self, frequency_hz: f64) -> Result<(), SourceError> {
        self.frequency = frequency_hz;
        Ok(())
    }

    fn sample_rates(&self) -> &[f64] {
        &[]
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SourceError> {
        self.sample_rate = rate;
        Ok(())
    }

    fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
        self.read_samples_impl(output)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    #[test]
    fn test_convert_int16() {
        let raw: [u8; 8] = [
            0xff, 0x7f, // re = 32767
            0x00, 0x80, // im = -32768
            0x00, 0x00, // re = 0
            0x00, 0x00, // im = 0
        ];
        let mut output = [Complex::default(); 2];
        convert_samples(&raw, &mut output, SampleFormat::Int16, 2);
        assert!((output[0].re - 1.0).abs() < 0.001);
        assert!((output[0].im - (-1.0)).abs() < 0.001);
        assert!((output[1].re).abs() < 0.001);
    }

    #[test]
    fn test_convert_float32() {
        let re_bytes = 0.5_f32.to_le_bytes();
        let im_bytes = (-0.25_f32).to_le_bytes();
        let mut raw = [0u8; 8];
        raw[0..4].copy_from_slice(&re_bytes);
        raw[4..8].copy_from_slice(&im_bytes);

        let mut output = [Complex::default(); 1];
        convert_samples(&raw, &mut output, SampleFormat::Float32, 1);
        assert!((output[0].re - 0.5).abs() < 1e-6);
        assert!((output[0].im - (-0.25)).abs() < 1e-6);
    }

    /// #744 (CR round 1 on PR #793) — a slow resolver must not hold
    /// `start()` past the connect timeout.
    #[test]
    fn resolution_is_bounded_by_the_deadline() {
        const SLOW_RESOLVER: Duration = Duration::from_millis(500);
        const DEADLINE: Duration = Duration::from_millis(100);
        let mut resolver = DeadlineResolver::default();
        let started = Instant::now();
        let err = resolver
            .resolve(
                "slow:1",
                move || {
                    std::thread::sleep(SLOW_RESOLVER);
                    Ok(Vec::new())
                },
                Instant::now() + DEADLINE,
            )
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < SLOW_RESOLVER,
            "must not wait for the resolver"
        );
    }

    /// #744 (CR round 2 on PR #793) — a lookup that timed out is reused by
    /// the next call for the same target instead of spawning another
    /// thread, so repeated starts cannot accumulate resolver threads.
    #[test]
    fn resolver_is_single_flight_per_target() {
        const SLOW_RESOLVER: Duration = Duration::from_millis(300);
        const FIRST_DEADLINE: Duration = Duration::from_millis(50);
        const SECOND_DEADLINE: Duration = Duration::from_secs(2);
        let first_result: Vec<SocketAddr> = vec!["192.0.2.1:1".parse().unwrap()];
        let second_result: Vec<SocketAddr> = vec!["192.0.2.2:2".parse().unwrap()];
        let mut resolver = DeadlineResolver::default();
        let expected = first_result.clone();
        let err = resolver
            .resolve(
                "same:1",
                move || {
                    std::thread::sleep(SLOW_RESOLVER);
                    Ok(first_result)
                },
                Instant::now() + FIRST_DEADLINE,
            )
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        // Same target: the in-flight lookup's answer comes back, and the
        // new closure is never run.
        let got = resolver
            .resolve(
                "same:1",
                move || Ok(second_result),
                Instant::now() + SECOND_DEADLINE,
            )
            .unwrap();
        assert_eq!(got, expected, "the first (in-flight) lookup must be reused");
        assert!(
            resolver.in_flight.is_none(),
            "completed lookups are released"
        );
    }

    /// #744 (CR round 1 on PR #793) — several unreachable addresses share
    /// ONE connect timeout instead of each getting the full budget.
    #[test]
    fn multiple_unreachable_addresses_share_one_deadline() {
        // TEST-NET-1 (RFC 5737): never routable, so connects hang until
        // the timeout on hosts with a default route — and fail fast
        // without one. Either way the total must stay under one budget.
        const TOTAL_BUDGET: Duration = Duration::from_millis(300);
        const SLACK: Duration = Duration::from_millis(250);
        let addrs = [
            "192.0.2.1:9".parse::<SocketAddr>().unwrap(),
            "192.0.2.2:9".parse::<SocketAddr>().unwrap(),
            "192.0.2.3:9".parse::<SocketAddr>().unwrap(),
        ];
        let started = Instant::now();
        let result = connect_any_with_deadline(&addrs, Instant::now() + TOTAL_BUDGET);
        assert!(result.is_err());
        assert!(
            started.elapsed() < TOTAL_BUDGET + SLACK,
            "three addresses took {:?}; each must only get the remaining budget",
            started.elapsed()
        );
    }

    /// #744 — a silent TCP peer must not park the DSP thread inside
    /// `read_samples`: the read times out and reports "no data".
    #[test]
    fn tcp_read_times_out_instead_of_hanging() {
        const READ_TIMEOUT: Duration = Duration::from_millis(200);
        const RETURN_DEADLINE: Duration = Duration::from_secs(1);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (_sock, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(2));
        });
        let mut source =
            NetworkSource::new(&addr.ip().to_string(), addr.port(), Protocol::TcpClient);
        source.set_timeouts(DEFAULT_NETWORK_CONNECT_TIMEOUT, READ_TIMEOUT);
        source.start().unwrap();
        let mut out = vec![Complex::default(); 1024];
        let started = Instant::now();
        let n = source.read_samples(&mut out).unwrap();
        assert_eq!(n, 0, "a timeout is \"no data\", not an error");
        assert!(
            started.elapsed() < RETURN_DEADLINE,
            "read must return at the timeout"
        );
        source.stop().unwrap();
        let _ = server.join();
    }

    /// #744 — the UDP receive buffer was sized from the caller's output
    /// slice, so a datagram larger than it was silently truncated by
    /// `recvfrom`. The whole datagram must be received and the samples
    /// the caller could not take are carried to the next call.
    #[test]
    fn udp_large_datagram_is_not_truncated() {
        // Int8 → 60 000 bytes: above the old 32 kB truncation point (output
        // slice × sample size) and under the 65 507-byte UDP maximum.
        const DATAGRAM_SAMPLES: usize = 30_000;
        const OUTPUT_SAMPLES: usize = 16_384;
        const READ_TIMEOUT: Duration = Duration::from_millis(500);
        /// Int8 complex sample: one I byte + one Q byte.
        const INT8_COMPLEX_BYTES: usize = 2;
        /// Payload byte cycle — prime, so no 2-byte pattern repeats early.
        const PAYLOAD_CYCLE: usize = 251;
        // Port 0: the OS assigns one and NetworkSource holds it from
        // bind onward, so nothing can claim it in between.
        let mut source = NetworkSource::new("127.0.0.1", 0, Protocol::Udp);
        source.set_sample_format(SampleFormat::Int8);
        source.set_timeouts(DEFAULT_NETWORK_CONNECT_TIMEOUT, READ_TIMEOUT);
        source.start().unwrap();
        let port = source.local_addr().expect("bound").port();

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let payload: Vec<u8> = (0..DATAGRAM_SAMPLES * INT8_COMPLEX_BYTES)
            .map(|i| (i % PAYLOAD_CYCLE) as u8)
            .collect();
        sender
            .send_to(&payload, ("127.0.0.1", port))
            .expect("loopback accepts a 60 kB datagram");

        let mut out = vec![Complex::default(); OUTPUT_SAMPLES];
        let mut total = 0;
        loop {
            let n = source.read_samples(&mut out).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(
            total, DATAGRAM_SAMPLES,
            "every sample of the datagram is delivered"
        );
    }

    /// #744 — the hostname is the local bind address for UDP (SDR++
    /// semantics), not ignored in favour of 0.0.0.0.
    #[test]
    fn udp_binds_to_the_configured_host() {
        let mut source = NetworkSource::new("127.0.0.1", 0, Protocol::Udp);
        source.start().unwrap();
        let local = source.local_addr().expect("bound");
        assert_eq!(local.ip().to_string(), "127.0.0.1");
        assert_ne!(local.port(), 0, "the OS assigned a real port");
    }

    /// #744 — changing the sample format mid-session must drop stale
    /// partial-sample bytes, or every later pair is shifted (I/Q swap).
    #[test]
    fn set_sample_format_clears_the_carry_buffer() {
        let mut source = NetworkSource::new("localhost", 1234, Protocol::TcpClient);
        source.carry_buf.extend_from_slice(&[1, 2, 3]);
        source.set_sample_format(SampleFormat::Float32);
        assert!(source.carry_buf.is_empty());
    }

    #[test]
    fn test_new() {
        let source = NetworkSource::new("localhost", 1234, Protocol::Udp);
        assert_eq!(source.name(), "Network");
        assert!(source.carry_buf.is_empty());
    }

    #[test]
    fn test_carry_buf_cleared_on_start_stop() {
        let mut source = NetworkSource::new("localhost", 1234, Protocol::Udp);
        source.carry_buf.push(0x42);
        // Stop clears carry buffer
        source.stop().unwrap();
        assert!(source.carry_buf.is_empty());
    }
}
