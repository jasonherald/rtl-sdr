//! Maps `RadioReference` mode strings to SDR demodulator modes and bandwidths.

/// The result of mapping a `RadioReference` mode string to an SDR demodulator
/// mode and bandwidth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MappedMode {
    /// SDR demodulator mode (e.g. "NFM", "WFM", "AM", "USB", "LSB", "CW").
    pub demod_mode: &'static str,
    /// Channel bandwidth in Hz.
    pub bandwidth: f64,
}

/// Bandwidth constants (Hz).
const NFM_BW: f64 = 12_500.0;
const WFM_BW: f64 = 150_000.0;
const AM_BW: f64 = 10_000.0;
const SSB_BW: f64 = 2_800.0;
const CW_BW: f64 = 500.0;

/// Maps a `RadioReference` mode string to the corresponding SDR demodulator mode
/// and bandwidth.
///
/// Matching is case-insensitive. Unknown modes default to NFM at 12 500 Hz.
pub fn map_rr_mode(rr_mode: &str) -> MappedMode {
    match rr_mode.to_uppercase().as_str() {
        "FM" | "FMN" => MappedMode {
            demod_mode: "NFM",
            bandwidth: NFM_BW,
        },
        "FMW" => MappedMode {
            demod_mode: "WFM",
            bandwidth: WFM_BW,
        },
        "AM" => MappedMode {
            demod_mode: "AM",
            bandwidth: AM_BW,
        },
        "USB" => MappedMode {
            demod_mode: "USB",
            bandwidth: SSB_BW,
        },
        "LSB" => MappedMode {
            demod_mode: "LSB",
            bandwidth: SSB_BW,
        },
        "CW" => MappedMode {
            demod_mode: "CW",
            bandwidth: CW_BW,
        },
        _ => {
            tracing::warn!(mode = rr_mode, "unknown RadioReference mode, defaulting to NFM");
            MappedMode {
                demod_mode: "NFM",
                bandwidth: NFM_BW,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fm_maps_to_nfm() {
        let m = map_rr_mode("FM");
        assert_eq!(m.demod_mode, "NFM");
        assert!((m.bandwidth - 12_500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn fmn_maps_to_nfm() {
        let m = map_rr_mode("FMN");
        assert_eq!(m.demod_mode, "NFM");
        assert!((m.bandwidth - 12_500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn fmw_maps_to_wfm() {
        let m = map_rr_mode("FMW");
        assert_eq!(m.demod_mode, "WFM");
        assert!((m.bandwidth - 150_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn am_maps_to_am() {
        let m = map_rr_mode("AM");
        assert_eq!(m.demod_mode, "AM");
        assert!((m.bandwidth - 10_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usb_maps_to_usb() {
        let m = map_rr_mode("USB");
        assert_eq!(m.demod_mode, "USB");
        assert!((m.bandwidth - 2_800.0).abs() < f64::EPSILON);
    }

    #[test]
    fn lsb_maps_to_lsb() {
        let m = map_rr_mode("LSB");
        assert_eq!(m.demod_mode, "LSB");
        assert!((m.bandwidth - 2_800.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cw_maps_to_cw() {
        let m = map_rr_mode("CW");
        assert_eq!(m.demod_mode, "CW");
        assert!((m.bandwidth - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_mode_defaults_to_nfm() {
        let m = map_rr_mode("P25");
        assert_eq!(m.demod_mode, "NFM");
        assert!((m.bandwidth - 12_500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn case_insensitive_lowercase() {
        let m = map_rr_mode("fm");
        assert_eq!(m.demod_mode, "NFM");
    }

    #[test]
    fn case_insensitive_mixed_case() {
        let m = map_rr_mode("Fmw");
        assert_eq!(m.demod_mode, "WFM");
    }

    #[test]
    fn case_insensitive_am_lower() {
        let m = map_rr_mode("am");
        assert_eq!(m.demod_mode, "AM");
    }

    #[test]
    fn case_insensitive_cw_lower() {
        let m = map_rr_mode("cw");
        assert_eq!(m.demod_mode, "CW");
    }
}
