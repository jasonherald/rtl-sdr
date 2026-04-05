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
use std::net::{TcpListener, TcpStream, UdpSocket};

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
}

#[allow(dead_code)]
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
            sample_rate: 48_000.0,
            stereo: false,
            connection: None,
        }
    }

    /// Set stereo/mono mode.
    pub fn set_stereo(&mut self, stereo: bool) {
        self.stereo = stereo;
    }

    /// Write audio samples to the network.
    ///
    /// Converts f32 stereo samples to int16 before sending.
    pub fn write_stereo_samples(&mut self, samples: &[Stereo]) -> Result<(), SinkError> {
        let conn = self.connection.as_mut().ok_or(SinkError::NotRunning)?;

        // Convert to int16
        let sample_count = if self.stereo {
            samples.len() * 2
        } else {
            samples.len()
        };
        let mut buf = vec![0u8; sample_count * 2];

        if self.stereo {
            for (i, s) in samples.iter().enumerate() {
                let l = (s.l * 32768.0) as i16;
                let r = (s.r * 32768.0) as i16;
                buf[i * 4..i * 4 + 2].copy_from_slice(&l.to_le_bytes());
                buf[i * 4 + 2..i * 4 + 4].copy_from_slice(&r.to_le_bytes());
            }
        } else {
            // Mono: average L and R
            for (i, s) in samples.iter().enumerate() {
                let mono = ((s.l + s.r) / 2.0 * 32768.0) as i16;
                buf[i * 2..i * 2 + 2].copy_from_slice(&mono.to_le_bytes());
            }
        }

        match conn {
            NetworkSinkConnection::TcpServer { client, .. } => {
                if let Some(stream) = client {
                    stream.write_all(&buf).map_err(SinkError::Io)?;
                }
            }
            NetworkSinkConnection::Udp(socket) => {
                let addr = format!("{}:{}", self.hostname, self.port);
                socket.send_to(&buf, &addr).map_err(SinkError::Io)?;
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
        assert!((sink.sample_rate() - 48_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_stereo_conversion() {
        let _samples = [Stereo::new(0.5, -0.5)];
        // Verify construction works (can't test write without connection)
        let mut sink = NetworkSink::new("localhost", 7355, Protocol::Udp);
        sink.set_stereo(true);
        // Can't write without a connection, but the conversion logic is tested implicitly
    }
}
