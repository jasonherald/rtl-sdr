//! Command dispatcher — translates a wire `Command` into the appropriate
//! `RtlSdrDevice` method call. Faithful port of the `switch(cmd.cmd)`
//! block in `rtl_tcp.c:315-372`.
//!
//! Errors are logged but do not abort the command loop (matches upstream,
//! which ignores each call's return code inside the `switch`).

use sdr_rtlsdr::device::RtlSdrDevice;

use crate::protocol::{Command, CommandOp};

/// Upper-16-bit field of a `SetIfGain` (0x06) param is the IF stage
/// number; lower 16 bits is a signed gain value in tenths of dB.
/// Wire layout matches upstream rtl_tcp.c:337-339.
const IF_GAIN_STAGE_SHIFT_BITS: u32 = 16;
/// Mask for the lower-16-bit signed gain field of `SetIfGain`.
const IF_GAIN_VALUE_MASK: u32 = 0xffff;

/// Execute a single command against the device.
///
/// Mirrors `rtl_tcp.c:315-372`. Upstream prints each command; we use
/// `tracing::debug!` at the same verbosity. Errors from the device layer
/// are logged at `warn!` and swallowed so the command channel stays up
/// (upstream discards the int returned by each `rtlsdr_set_*` call).
pub fn dispatch(dev: &mut RtlSdrDevice, cmd: Command) {
    let param = cmd.param;
    let result = match cmd.op {
        // 0x01: rtlsdr_set_center_freq(dev, ntohl(cmd.param));
        CommandOp::SetCenterFreq => {
            tracing::debug!(freq_hz = param, "rtl_tcp set freq");
            dev.set_center_freq(param)
        }
        // 0x02: rtlsdr_set_sample_rate(dev, ntohl(cmd.param));
        CommandOp::SetSampleRate => {
            tracing::debug!(rate_hz = param, "rtl_tcp set sample rate");
            dev.set_sample_rate(param)
        }
        // 0x03: rtlsdr_set_tuner_gain_mode(dev, ntohl(cmd.param));
        //       C takes int: 0 = auto, 1 = manual.
        CommandOp::SetGainMode => {
            let manual = param != 0;
            tracing::debug!(manual, "rtl_tcp set gain mode");
            dev.set_tuner_gain_mode(manual)
        }
        // 0x04: rtlsdr_set_tuner_gain(dev, ntohl(cmd.param));
        //       Gain is signed tenths of dB upstream. Reinterpret u32 as i32.
        CommandOp::SetTunerGain => {
            #[allow(clippy::cast_possible_wrap)]
            let gain = param as i32;
            tracing::debug!(gain_tenths_db = gain, "rtl_tcp set tuner gain");
            dev.set_tuner_gain(gain)
        }
        // 0x05: rtlsdr_set_freq_correction(dev, ntohl(cmd.param));
        //       PPM is signed int upstream.
        CommandOp::SetFreqCorrection => {
            #[allow(clippy::cast_possible_wrap)]
            let ppm = param as i32;
            tracing::debug!(ppm, "rtl_tcp set freq correction");
            dev.set_freq_correction(ppm)
        }
        // 0x06: tmp = ntohl(cmd.param);
        //       rtlsdr_set_tuner_if_gain(dev, tmp >> 16, (short)(tmp & 0xffff));
        //
        // Upstream dispatches to the active tuner's `set_if_gain`.
        // Only the E4000 programs IF stages meaningfully — every
        // other supported tuner is a no-op there (matched by our
        // `Tuner::set_if_gain` default). Sign-extending via `i16`
        // replicates the short cast upstream applies to the wire
        // gain so negative tenths-of-dB values survive unchanged.
        CommandOp::SetIfGain => {
            let (stage, gain) = unpack_if_gain_param(param);
            tracing::debug!(stage, gain_tenths_db = gain, "rtl_tcp set tuner if gain");
            dev.set_tuner_if_gain(i32::from(stage), i32::from(gain))
        }
        // 0x07: rtlsdr_set_testmode(dev, ntohl(cmd.param));
        //       C takes int, non-zero = on.
        CommandOp::SetTestMode => {
            let on = param != 0;
            tracing::debug!(on, "rtl_tcp set test mode");
            dev.set_testmode(on)
        }
        // 0x08: rtlsdr_set_agc_mode(dev, ntohl(cmd.param));
        CommandOp::SetAgcMode => {
            let on = param != 0;
            tracing::debug!(on, "rtl_tcp set agc mode");
            dev.set_agc_mode(on)
        }
        // 0x09: rtlsdr_set_direct_sampling(dev, ntohl(cmd.param));
        //       0 = off, 1 = I branch, 2 = Q branch. Upstream passes int.
        CommandOp::SetDirectSampling => {
            #[allow(clippy::cast_possible_wrap)]
            let mode = param as i32;
            tracing::debug!(mode, "rtl_tcp set direct sampling");
            dev.set_direct_sampling(mode)
        }
        // 0x0a: rtlsdr_set_offset_tuning(dev, ntohl(cmd.param));
        CommandOp::SetOffsetTuning => {
            let on = param != 0;
            tracing::debug!(on, "rtl_tcp set offset tuning");
            dev.set_offset_tuning(on)
        }
        // 0x0b: rtlsdr_set_xtal_freq(dev, ntohl(cmd.param), 0);
        //       Set RTL xtal only — tuner slot 0 means "leave as-is" per
        //       our driver (see sdr-rtlsdr set_xtal_freq semantics).
        CommandOp::SetRtlXtal => {
            tracing::debug!(rtl_xtal_hz = param, "rtl_tcp set rtl xtal");
            dev.set_xtal_freq(param, 0)
        }
        // 0x0c: rtlsdr_set_xtal_freq(dev, 0, ntohl(cmd.param));
        //       Set tuner xtal only — RTL slot 0 means "leave as-is".
        CommandOp::SetTunerXtal => {
            tracing::debug!(tuner_xtal_hz = param, "rtl_tcp set tuner xtal");
            dev.set_xtal_freq(0, param)
        }
        // 0x0d: set_gain_by_index(dev, ntohl(cmd.param));
        //       Upstream helper in rtl_tcp.c:259-275. Look up gain table,
        //       silently drop out-of-range indices, apply via set_tuner_gain.
        CommandOp::SetGainByIndex => {
            tracing::debug!(index = param, "rtl_tcp set gain by index");
            set_gain_by_index(dev, param)
        }
        // 0x0e: rtlsdr_set_bias_tee(dev, (int)ntohl(cmd.param));
        CommandOp::SetBiasTee => {
            let on = param != 0;
            tracing::debug!(on, "rtl_tcp set bias tee");
            dev.set_bias_tee(on)
        }
    };

    if let Err(e) = result {
        tracing::warn!(cmd = ?cmd.op, err = %e, "rtl_tcp command failed, ignoring");
    }
}

/// Port of `set_gain_by_index` helper in `rtl_tcp.c:259-275`:
///
/// ```c
/// int count = rtlsdr_get_tuner_gains(_dev, NULL);
/// if (count > 0 && (unsigned int)count > index) {
///     gains = malloc(sizeof(int) * count);
///     count = rtlsdr_get_tuner_gains(_dev, gains);
///     res = rtlsdr_set_tuner_gain(_dev, gains[index]);
///     free(gains);
/// }
/// ```
fn set_gain_by_index(dev: &mut RtlSdrDevice, index: u32) -> Result<(), sdr_rtlsdr::RtlSdrError> {
    let gains = dev.tuner_gains();
    let idx = index as usize;
    if idx >= gains.len() {
        // Upstream silently does nothing when index is out of range.
        return Ok(());
    }
    let gain = gains[idx];
    dev.set_tuner_gain(gain)
}

/// Unpack the `SetIfGain` (0x06) wire param into its two fields.
///
/// Upper 16 bits = IF stage index (unsigned, but `i16` preserves
/// the upstream C prototype `int stage`). Lower 16 bits = signed
/// gain in tenths of dB — upstream casts to `short`, and we do the
/// same via `as i16` so negative values sign-extend correctly when
/// widened back to `i32` for the `set_tuner_if_gain` call.
///
/// Pure function for unit-testability; the handler just calls it
/// and forwards.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "wire format: rtl_tcp.c:337-339 packs stage in the high 16 bits and signed gain in the low 16 bits of the 32-bit param"
)]
fn unpack_if_gain_param(param: u32) -> (i16, i16) {
    let stage = (param >> IF_GAIN_STAGE_SHIFT_BITS) as i16;
    let gain = (param & IF_GAIN_VALUE_MASK) as i16;
    (stage, gain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpack_if_gain_param_positive_values() {
        // stage=3, gain=+50 tenths (= +5 dB). Both values sit in
        // their respective 16-bit fields.
        let param = (0x0003_u32 << 16) | 0x0032_u32;
        assert_eq!(unpack_if_gain_param(param), (3, 50));
    }

    #[test]
    fn unpack_if_gain_param_negative_gain_sign_extends() {
        // stage=2, gain=-50 tenths. Wire stores -50 as a 16-bit
        // two's-complement value (0xFFCE) in the lower half. The
        // `as i16` cast must read it back as -50, NOT 0xFFCE
        // (= 65486) — that's the sign-extension regression this
        // test pins. Mirrors upstream rtl_tcp.c's `(short)` cast.
        let gain_wire = u32::from((-50i16) as u16);
        let param = (0x0002_u32 << 16) | gain_wire;
        assert_eq!(unpack_if_gain_param(param), (2, -50));
    }

    #[test]
    fn unpack_if_gain_param_boundary_values() {
        // i16::MAX stage + i16::MIN gain — exercises both extremes
        // of the signed 16-bit interpretation.
        let stage_wire = u32::from(i16::MAX as u16) << 16;
        let gain_wire = u32::from(i16::MIN as u16);
        assert_eq!(
            unpack_if_gain_param(stage_wire | gain_wire),
            (i16::MAX, i16::MIN)
        );
    }

    #[test]
    fn unpack_if_gain_param_zero_returns_zero() {
        assert_eq!(unpack_if_gain_param(0), (0, 0));
    }
}
