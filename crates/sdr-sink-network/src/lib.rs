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
//! TCP/UDP network audio output sink.
//!
//! Ports SDR++ `NetworkSinkModule`. Sends audio samples over
//! TCP or UDP in int16 format.

use sdr_pipeline::sink_manager::Sink;
use sdr_types::{Protocol, SinkError, Stereo};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};

/// Default network sink sample rate.
const DEFAULT_SAMPLE_RATE: f64 = 48_000.0;

/// Network audio output sink.
///
/// Sends audio samples over TCP (server) or UDP in int16 format.
pub struct NetworkSink {
    hostname: String,
    port: u16,
    protocol: Protocol,
    sample_rate: f64,
    stereo: bool,
    connection: Option<NetworkSinkConnection>,
    // Pre-allocated buffers to avoid hot-path allocation
    send_buf: Vec<u8>,
    // Resolved UDP target address (avoids ToSocketAddrs parsing on every send)
    cached_addr: Option<SocketAddr>,
}

enum NetworkSinkConnection {
    TcpServer {
        listener: TcpListener,
        client: Option<TcpStream>,
    },
    Udp(UdpSocket),
}

impl NetworkSink {
    /// Create a new network sink.
    pub fn new(hostname: &str, port: u16, protocol: Protocol) -> Self {
        Self {
            hostname: hostname.to_string(),
            port,
            protocol,
            sample_rate: DEFAULT_SAMPLE_RATE,
            stereo: false,
            connection: None,
            send_buf: Vec::new(),
            cached_addr: None,
        }
    }

    /// Set stereo/mono mode.
    pub fn set_stereo(&mut self, stereo: bool) {
        self.stereo = stereo;
    }

    /// Write audio samples to the network.
    ///
    /// Converts f32 stereo samples to int16 before sending.
    /// For TCP server mode, polls for new client connections before writing.
    pub fn write_stereo_samples(&mut self, samples: &[Stereo]) -> Result<(), SinkError> {
        let conn = self.connection.as_mut().ok_or(SinkError::NotRunning)?;

        // TCP: poll for incoming client connections (non-blocking accept)
        if let NetworkSinkConnection::TcpServer { listener, client } = conn
            && client.is_none()
        {
            match listener.accept() {
                Ok((stream, addr)) => {
                    tracing::info!("network sink: TCP client connected from {addr}");
                    // Accepted stream inherits nonblocking from listener —
                    // switch to blocking so write_all works correctly.
                    if let Err(e) = stream.set_nonblocking(false) {
                        tracing::warn!("network sink: failed to set TCP stream blocking: {e}");
                    }
                    *client = Some(stream);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No client waiting — that's fine
                }
                Err(e) => {
                    tracing::warn!("network sink: TCP accept error: {e}");
                }
            }
        }

        // Convert to int16 using pre-allocated buffer
        let byte_count = if self.stereo {
            samples.len() * 4
        } else {
            samples.len() * 2
        };
        self.send_buf.resize(byte_count, 0);

        if self.stereo {
            for (i, s) in samples.iter().enumerate() {
                let l = (s.l.clamp(-1.0, 1.0) * 32767.0) as i16;
                let r = (s.r.clamp(-1.0, 1.0) * 32767.0) as i16;
                self.send_buf[i * 4..i * 4 + 2].copy_from_slice(&l.to_le_bytes());
                self.send_buf[i * 4 + 2..i * 4 + 4].copy_from_slice(&r.to_le_bytes());
            }
        } else {
            for (i, s) in samples.iter().enumerate() {
                let mono = (((s.l + s.r) / 2.0).clamp(-1.0, 1.0) * 32767.0) as i16;
                self.send_buf[i * 2..i * 2 + 2].copy_from_slice(&mono.to_le_bytes());
            }
        }

        match conn {
            NetworkSinkConnection::TcpServer { client, .. } => {
                if let Some(stream) = client
                    && let Err(e) = stream.write_all(&self.send_buf)
                {
                    tracing::warn!("TCP client disconnected: {e}");
                    *client = None;
                }
                // No client connected — silently drop (matching C++ behavior)
            }
            NetworkSinkConnection::Udp(socket) => {
                if let Some(addr) = self.cached_addr {
                    socket
                        .send_to(&self.send_buf, addr)
                        .map_err(SinkError::Io)?;
                }
            }
        }

        Ok(())
    }
}

impl Sink for NetworkSink {
    fn name(&self) -> &str {
        "Network"
    }

    fn start(&mut self) -> Result<(), SinkError> {
        let conn = match self.protocol {
            Protocol::TcpClient => {
                // TCP server mode — listen for connections
                let addr = format!("{}:{}", self.hostname, self.port);
                let listener = TcpListener::bind(&addr).map_err(SinkError::Io)?;
                listener.set_nonblocking(true).map_err(SinkError::Io)?;
                NetworkSinkConnection::TcpServer {
                    listener,
                    client: None,
                }
            }
            Protocol::Udp => {
                // Resolve target address once at startup (avoids per-packet parsing)
                let target = (self.hostname.as_str(), self.port)
                    .to_socket_addrs()
                    .map_err(SinkError::Io)?
                    .next()
                    .ok_or_else(|| {
                        SinkError::Io(std::io::Error::new(
                            std::io::ErrorKind::AddrNotAvailable,
                            format!("could not resolve {}:{}", self.hostname, self.port),
                        ))
                    })?;
                self.cached_addr = Some(target);
                let socket =
                    UdpSocket::bind(format!("0.0.0.0:{}", self.port)).map_err(SinkError::Io)?;
                NetworkSinkConnection::Udp(socket)
            }
        };
        self.connection = Some(conn);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        self.connection = None;
        Ok(())
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SinkError> {
        self.sample_rate = rate;
        Ok(())
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let sink = NetworkSink::new("localhost", 7355, Protocol::Udp);
        assert_eq!(sink.name(), "Network");
        assert!((sink.sample_rate() - DEFAULT_SAMPLE_RATE).abs() < f64::EPSILON);
        assert!(sink.cached_addr.is_none());
    }

    #[test]
    fn test_send_buf_initially_empty() {
        let sink = NetworkSink::new("localhost", 7355, Protocol::Udp);
        assert!(sink.send_buf.is_empty());
    }
}
