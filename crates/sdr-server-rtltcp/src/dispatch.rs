//! Command dispatcher — translates a wire `Command` into the appropriate
//! `RtlSdrDevice` method call. Faithful port of the `switch(cmd.cmd)`
//! block in `rtl_tcp.c:315-372`.
//!
//! Errors are logged but do not abort the command loop (matches upstream,
//! which ignores each call's return code inside the `switch`).

use sdr_rtlsdr::device::RtlSdrDevice;

use crate::protocol::{Command, CommandOp};

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
        // Not yet available in our sdr-rtlsdr driver (see #308). Upstream
        // behavior for R820T/R828D/FC* is also a no-op — E4000 is the only
        // tuner that actually programs IF gain stages. Log and drop until
        // #308 lands.
        CommandOp::SetIfGain => {
            let stage = (param >> 16) as i16;
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let gain = (param & 0xffff) as i16;
            tracing::debug!(
                stage,
                gain_tenths_db = gain,
                "rtl_tcp set_tuner_if_gain: not implemented (see #308), dropping"
            );
            Ok(())
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
