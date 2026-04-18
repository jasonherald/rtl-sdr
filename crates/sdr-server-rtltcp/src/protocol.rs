//! rtl_tcp wire protocol primitives.
//!
//! Faithful port of the two packed structs in
//! `original/librtlsdr/src/rtl_tcp.c`:
//!
//! ```c
//! typedef struct {          // sent by server on client connect
//!     char     magic[4];    // "RTL0"
//!     uint32_t tuner_type;  // big-endian
//!     uint32_t tuner_gain_count;
//! } __attribute__((packed)) dongle_info_t;
//!
//! struct command {          // sent by client at any time
//!     unsigned char cmd;
//!     unsigned int  param;  // big-endian
//! } __attribute__((packed));
//! ```
//!
//! Both are big-endian on the wire. Layout is fixed and must not change;
//! interop with GQRX/SDR++/SoapySDR depends on exact byte compatibility.

use sdr_rtlsdr::reg::TunerType;

/// The 4-byte magic prefix in `dongle_info_t`.
pub const DONGLE_MAGIC: [u8; 4] = *b"RTL0";

/// Serialized size of `dongle_info_t` on the wire.
pub const DONGLE_INFO_LEN: usize = 12;

/// Serialized size of a command message on the wire.
pub const COMMAND_LEN: usize = 5;

/// Default TCP port used by upstream `rtl_tcp`.
pub const DEFAULT_PORT: u16 = 1234;

/// Tuner-type codes as they appear in `dongle_info_t.tuner_type`.
///
/// Values match `enum rtlsdr_tuner` in upstream `rtl-sdr.h` — do not
/// renumber without breaking wire compatibility.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunerTypeCode {
    Unknown = 0,
    E4000 = 1,
    Fc0012 = 2,
    Fc0013 = 3,
    Fc2580 = 4,
    R820t = 5,
    R828d = 6,
}

impl From<TunerType> for TunerTypeCode {
    fn from(t: TunerType) -> Self {
        match t {
            TunerType::Unknown => TunerTypeCode::Unknown,
            TunerType::E4000 => TunerTypeCode::E4000,
            TunerType::Fc0012 => TunerTypeCode::Fc0012,
            TunerType::Fc0013 => TunerTypeCode::Fc0013,
            TunerType::Fc2580 => TunerTypeCode::Fc2580,
            TunerType::R820T => TunerTypeCode::R820t,
            TunerType::R828D => TunerTypeCode::R828d,
        }
    }
}

/// 12-byte header the server writes to every newly-connected client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DongleInfo {
    pub tuner: TunerTypeCode,
    pub gain_count: u32,
}

impl DongleInfo {
    /// Serialize to 12 bytes (magic + BE tuner + BE gain count).
    pub fn to_bytes(self) -> [u8; DONGLE_INFO_LEN] {
        let mut out = [0u8; DONGLE_INFO_LEN];
        out[0..4].copy_from_slice(&DONGLE_MAGIC);
        out[4..8].copy_from_slice(&(self.tuner as u32).to_be_bytes());
        out[8..12].copy_from_slice(&self.gain_count.to_be_bytes());
        out
    }

    /// Parse 12 bytes sent by a server.
    ///
    /// Returns `None` if the magic prefix is wrong.
    pub fn from_bytes(bytes: &[u8; DONGLE_INFO_LEN]) -> Option<Self> {
        if bytes[0..4] != DONGLE_MAGIC {
            return None;
        }
        let tuner_raw = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let gain_count = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        // Preserve the upstream numbering. Out-of-range tuner codes are
        // treated as Unknown so we don't refuse to talk to a future-
        // extended server that invents a new code.
        let tuner = match tuner_raw {
            1 => TunerTypeCode::E4000,
            2 => TunerTypeCode::Fc0012,
            3 => TunerTypeCode::Fc0013,
            4 => TunerTypeCode::Fc2580,
            5 => TunerTypeCode::R820t,
            6 => TunerTypeCode::R828d,
            _ => TunerTypeCode::Unknown,
        };
        Some(Self { tuner, gain_count })
    }
}

/// Raw command opcode values. Exhaustively matches `rtl_tcp.c:315-372`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOp {
    /// 0x01 — set center frequency (Hz).
    SetCenterFreq = 0x01,
    /// 0x02 — set sample rate (Hz).
    SetSampleRate = 0x02,
    /// 0x03 — set tuner gain mode (0 = auto, 1 = manual).
    SetGainMode = 0x03,
    /// 0x04 — set tuner gain (tenths of dB).
    SetTunerGain = 0x04,
    /// 0x05 — set frequency correction (ppm).
    SetFreqCorrection = 0x05,
    /// 0x06 — set IF stage gain: upper 16 = stage, lower 16 = gain (signed).
    SetIfGain = 0x06,
    /// 0x07 — set test mode.
    SetTestMode = 0x07,
    /// 0x08 — set RTL2832 AGC mode.
    SetAgcMode = 0x08,
    /// 0x09 — set direct sampling.
    SetDirectSampling = 0x09,
    /// 0x0a — set offset tuning.
    SetOffsetTuning = 0x0a,
    /// 0x0b — set RTL xtal frequency.
    SetRtlXtal = 0x0b,
    /// 0x0c — set tuner xtal frequency.
    SetTunerXtal = 0x0c,
    /// 0x0d — set tuner gain by index.
    SetGainByIndex = 0x0d,
    /// 0x0e — set bias tee.
    SetBiasTee = 0x0e,
}

impl CommandOp {
    /// Decode a raw opcode byte. Returns `None` for unrecognized values.
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::SetCenterFreq),
            0x02 => Some(Self::SetSampleRate),
            0x03 => Some(Self::SetGainMode),
            0x04 => Some(Self::SetTunerGain),
            0x05 => Some(Self::SetFreqCorrection),
            0x06 => Some(Self::SetIfGain),
            0x07 => Some(Self::SetTestMode),
            0x08 => Some(Self::SetAgcMode),
            0x09 => Some(Self::SetDirectSampling),
            0x0a => Some(Self::SetOffsetTuning),
            0x0b => Some(Self::SetRtlXtal),
            0x0c => Some(Self::SetTunerXtal),
            0x0d => Some(Self::SetGainByIndex),
            0x0e => Some(Self::SetBiasTee),
            _ => None,
        }
    }
}

/// A 5-byte command message on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub op: CommandOp,
    pub param: u32,
}

impl Command {
    /// Serialize to 5 bytes (1 byte op + 4 bytes BE param).
    pub fn to_bytes(self) -> [u8; COMMAND_LEN] {
        let mut out = [0u8; COMMAND_LEN];
        out[0] = self.op as u8;
        out[1..5].copy_from_slice(&self.param.to_be_bytes());
        out
    }

    /// Parse 5 bytes received from a client.
    ///
    /// Returns `None` if the opcode is unrecognized. Upstream silently
    /// drops unknown opcodes (`switch(cmd.cmd)` has no `default` arm) —
    /// we mirror that behavior at the dispatcher, not here.
    pub fn from_bytes(bytes: &[u8; COMMAND_LEN]) -> Option<Self> {
        let op = CommandOp::from_u8(bytes[0])?;
        let param = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        Some(Self { op, param })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn dongle_info_roundtrip() {
        let info = DongleInfo {
            tuner: TunerTypeCode::R820t,
            gain_count: 29,
        };
        let bytes = info.to_bytes();
        assert_eq!(&bytes[0..4], b"RTL0");
        assert_eq!(
            u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            5
        );
        assert_eq!(
            u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            29
        );
        assert_eq!(DongleInfo::from_bytes(&bytes), Some(info));
    }

    #[test]
    fn dongle_info_rejects_bad_magic() {
        let mut bytes = [0u8; DONGLE_INFO_LEN];
        bytes[0..4].copy_from_slice(b"XXXX");
        assert!(DongleInfo::from_bytes(&bytes).is_none());
    }

    #[test]
    fn dongle_info_unknown_tuner_decodes_as_unknown() {
        let mut bytes = [0u8; DONGLE_INFO_LEN];
        bytes[0..4].copy_from_slice(b"RTL0");
        bytes[4..8].copy_from_slice(&99u32.to_be_bytes());
        let decoded = DongleInfo::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.tuner, TunerTypeCode::Unknown);
    }

    #[test]
    fn command_roundtrip_all_ops() {
        let ops = [
            CommandOp::SetCenterFreq,
            CommandOp::SetSampleRate,
            CommandOp::SetGainMode,
            CommandOp::SetTunerGain,
            CommandOp::SetFreqCorrection,
            CommandOp::SetIfGain,
            CommandOp::SetTestMode,
            CommandOp::SetAgcMode,
            CommandOp::SetDirectSampling,
            CommandOp::SetOffsetTuning,
            CommandOp::SetRtlXtal,
            CommandOp::SetTunerXtal,
            CommandOp::SetGainByIndex,
            CommandOp::SetBiasTee,
        ];
        for op in ops {
            let cmd = Command {
                op,
                param: 0xdead_beef,
            };
            let bytes = cmd.to_bytes();
            assert_eq!(bytes[0], op as u8);
            assert_eq!(
                u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]),
                0xdead_beef
            );
            assert_eq!(Command::from_bytes(&bytes), Some(cmd));
        }
    }

    #[test]
    fn command_rejects_unknown_op() {
        let bytes = [0xff, 0, 0, 0, 0];
        assert!(Command::from_bytes(&bytes).is_none());
    }

    #[test]
    fn center_freq_command_be_param() {
        let cmd = Command {
            op: CommandOp::SetCenterFreq,
            param: 100_000_000,
        };
        let bytes = cmd.to_bytes();
        // upstream uses htonl(param); 100_000_000 = 0x05F5E100
        assert_eq!(bytes, [0x01, 0x05, 0xF5, 0xE1, 0x00]);
    }

    #[test]
    fn command_rejects_reserved_low_opcodes() {
        // 0x00 is not a defined opcode; upstream's switch has no default
        // arm for it. We reject at parse time so the dispatcher's match
        // stays exhaustive.
        let bytes = [0x00, 0, 0, 0, 0];
        assert!(Command::from_bytes(&bytes).is_none());
    }

    #[test]
    fn command_rejects_opcodes_above_0x0e() {
        // Every value 0x0f..=0xff must be rejected — sanity-check the
        // upper boundary so a future upstream extension doesn't leak.
        for op in 0x0f..=0xff {
            let bytes = [op, 1, 2, 3, 4];
            assert!(
                Command::from_bytes(&bytes).is_none(),
                "opcode 0x{op:02x} should be rejected but parsed"
            );
        }
    }

    #[test]
    fn all_known_opcodes_have_distinct_codes() {
        use std::collections::HashSet;
        let codes: HashSet<u8> = [
            CommandOp::SetCenterFreq,
            CommandOp::SetSampleRate,
            CommandOp::SetGainMode,
            CommandOp::SetTunerGain,
            CommandOp::SetFreqCorrection,
            CommandOp::SetIfGain,
            CommandOp::SetTestMode,
            CommandOp::SetAgcMode,
            CommandOp::SetDirectSampling,
            CommandOp::SetOffsetTuning,
            CommandOp::SetRtlXtal,
            CommandOp::SetTunerXtal,
            CommandOp::SetGainByIndex,
            CommandOp::SetBiasTee,
        ]
        .iter()
        .map(|&op| op as u8)
        .collect();
        assert_eq!(codes.len(), 14);
    }

    #[test]
    fn dongle_info_all_zeros_yields_unknown_tuner() {
        let bytes = [0u8; DONGLE_INFO_LEN];
        // Magic is not "RTL0" — must be rejected outright, not silently
        // treated as an Unknown-tuner server.
        assert!(DongleInfo::from_bytes(&bytes).is_none());
    }

    #[test]
    fn dongle_info_valid_magic_with_zero_gain_count() {
        // Magic valid, tuner=0 (Unknown), gain_count=0 — edge case that
        // corresponds to a dongle that enumerates with no advertised
        // gain table. Should parse successfully.
        let mut bytes = [0u8; DONGLE_INFO_LEN];
        bytes[0..4].copy_from_slice(b"RTL0");
        let info = DongleInfo::from_bytes(&bytes).unwrap();
        assert_eq!(info.tuner, TunerTypeCode::Unknown);
        assert_eq!(info.gain_count, 0);
    }

    #[test]
    fn command_param_boundary_values() {
        // Test u32 min and max to catch any accidental signed interpretation
        // in the serialization layer.
        for param in [0u32, 1, u32::MAX, u32::MAX - 1, 0x8000_0000] {
            let cmd = Command {
                op: CommandOp::SetCenterFreq,
                param,
            };
            let bytes = cmd.to_bytes();
            let decoded = Command::from_bytes(&bytes).unwrap();
            assert_eq!(decoded, cmd);
        }
    }

    #[test]
    fn tuner_type_code_mapping_matches_upstream_numbering() {
        assert_eq!(TunerTypeCode::from(TunerType::Unknown) as u32, 0);
        assert_eq!(TunerTypeCode::from(TunerType::E4000) as u32, 1);
        assert_eq!(TunerTypeCode::from(TunerType::Fc0012) as u32, 2);
        assert_eq!(TunerTypeCode::from(TunerType::Fc0013) as u32, 3);
        assert_eq!(TunerTypeCode::from(TunerType::Fc2580) as u32, 4);
        assert_eq!(TunerTypeCode::from(TunerType::R820T) as u32, 5);
        assert_eq!(TunerTypeCode::from(TunerType::R828D) as u32, 6);
    }
}
