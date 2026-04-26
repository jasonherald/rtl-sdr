/// Demodulation mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DemodMode {
    /// Wideband FM (broadcast, ~200kHz bandwidth).
    Wfm,
    /// Narrowband FM (12.5/25kHz, public safety, ham).
    Nfm,
    /// Amplitude modulation.
    Am,
    /// Upper sideband (SSB).
    Usb,
    /// Lower sideband (SSB).
    Lsb,
    /// Double sideband.
    Dsb,
    /// Continuous wave (morse code).
    Cw,
    /// Raw IQ passthrough.
    Raw,
    /// Meteor-M LRPT receive mode. Like `Raw` but the
    /// `RadioModule` runs at 144 ksps IF rate (the LRPT working
    /// rate per `sdr_dsp::lrpt::SAMPLE_RATE_HZ`) so the post-VFO
    /// IQ is fed straight into the QPSK demod + FEC chain by
    /// the controller's LRPT tap. Audio output is silent stereo
    /// — there's no listenable signal mid-pass; the imagery is
    /// the artifact. Per epic #469 Task 7.
    Lrpt,
}

/// Network IQ sample format — matches SDR++ `SampleType` enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    /// Signed 8-bit integer (2 bytes per complex sample).
    Int8,
    /// Signed 16-bit integer (4 bytes per complex sample).
    Int16,
    /// Signed 32-bit integer (8 bytes per complex sample).
    Int32,
    /// 32-bit float (8 bytes per complex sample).
    Float32,
}

impl SampleFormat {
    /// Byte size of one complex sample (I + Q) in this format.
    pub fn complex_byte_size(self) -> usize {
        match self {
            Self::Int8 => 2,
            Self::Int16 => 4,
            Self::Int32 | Self::Float32 => 8,
        }
    }
}

/// Network protocol for IQ sources and audio sinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// TCP client connection.
    TcpClient,
    /// UDP unicast/multicast.
    Udp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sample_format_sizes() {
        assert_eq!(SampleFormat::Int8.complex_byte_size(), 2);
        assert_eq!(SampleFormat::Int16.complex_byte_size(), 4);
        assert_eq!(SampleFormat::Int32.complex_byte_size(), 8);
        assert_eq!(SampleFormat::Float32.complex_byte_size(), 8);
    }
}
