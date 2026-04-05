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

use sdr_pipeline::source_manager::Source;
use sdr_types::{Complex, Protocol, SampleFormat, SourceError};
use std::io::Read;
use std::net::{TcpStream, UdpSocket};

/// Network IQ source for the pipeline.
///
/// Receives complex IQ samples over TCP or UDP with format conversion.
pub struct NetworkSource {
    hostname: String,
    port: u16,
    protocol: Protocol,
    sample_format: SampleFormat,
    sample_rate: f64,
    frequency: f64,
    connection: Option<NetworkConnection>,
}

enum NetworkConnection {
    Tcp(TcpStream),
    Udp(UdpSocket),
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
        }
    }

    /// Set the sample format for incoming data.
    pub fn set_sample_format(&mut self, format: SampleFormat) {
        self.sample_format = format;
    }

    /// Read samples from the network connection and convert to Complex.
    ///
    /// Returns the number of Complex samples written.
    pub fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
        let sample_size = self.sample_format.complex_byte_size();
        let max_bytes = output.len() * sample_size;
        let mut buf = vec![0u8; max_bytes];

        let bytes_read = match &mut self.connection {
            Some(NetworkConnection::Tcp(stream)) => {
                stream.read(&mut buf).map_err(|e| SourceError::Io(e))?
            }
            Some(NetworkConnection::Udp(socket)) => {
                let (n, _addr) = socket.recv_from(&mut buf).map_err(|e| SourceError::Io(e))?;
                n
            }
            None => return Err(SourceError::NotRunning),
        };

        let count = bytes_read / sample_size;
        Ok(convert_samples(
            &buf[..bytes_read],
            output,
            self.sample_format,
            count,
        ))
    }
}

/// Convert raw network bytes to Complex f32 samples.
fn convert_samples(
    raw: &[u8],
    output: &mut [Complex],
    format: SampleFormat,
    count: usize,
) -> usize {
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
    count
}

impl Source for NetworkSource {
    fn name(&self) -> &str {
        "Network"
    }

    fn start(&mut self) -> Result<(), SourceError> {
        let conn = match self.protocol {
            Protocol::TcpClient => {
                let addr = format!("{}:{}", self.hostname, self.port);
                let stream = TcpStream::connect(&addr)?;
                NetworkConnection::Tcp(stream)
            }
            Protocol::Udp => {
                let socket = UdpSocket::bind(format!("0.0.0.0:{}", self.port))?;
                NetworkConnection::Udp(socket)
            }
        };
        self.connection = Some(conn);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        self.connection = None;
        Ok(())
    }

    fn tune(&mut self, frequency_hz: f64) -> Result<(), SourceError> {
        self.frequency = frequency_hz;
        // Network source doesn't tune — frequency is informational
        Ok(())
    }

    fn sample_rates(&self) -> &[f64] {
        // Network source accepts any sample rate
        &[]
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SourceError> {
        self.sample_rate = rate;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_int16() {
        // Int16: 32767 = max positive, -32768 = max negative
        let raw: [u8; 8] = [
            0xff, 0x7f, // re = 32767
            0x00, 0x80, // im = -32768
            0x00, 0x00, // re = 0
            0x00, 0x00, // im = 0
        ];
        let mut output = [Complex::default(); 2];
        let count = convert_samples(&raw, &mut output, SampleFormat::Int16, 2);
        assert_eq!(count, 2);
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
        let count = convert_samples(&raw, &mut output, SampleFormat::Float32, 1);
        assert_eq!(count, 1);
        assert!((output[0].re - 0.5).abs() < 1e-6);
        assert!((output[0].im - (-0.25)).abs() < 1e-6);
    }

    #[test]
    fn test_new() {
        let source = NetworkSource::new("localhost", 1234, Protocol::Udp);
        assert_eq!(source.name(), "Network");
    }
}
