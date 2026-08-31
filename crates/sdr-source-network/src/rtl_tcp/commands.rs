//! The `rtl_tcp` client's command-sender surface (issue #818): the
//! low-level `send_command` escape hatch, sticky-command recording
//! for reconnect replay, the typed `set_*` convenience setters, and
//! the [`Source`] trait implementation that routes the workspace-wide
//! source-control messages onto the wire. Split out of `rtl_tcp.rs`
//! per the Codacy 500-NLOC file gate — behavior is unchanged.

use std::io::Write;
use std::sync::atomic::Ordering;

use sdr_pipeline::source_manager::Source;
use sdr_server_rtltcp::protocol::{Command, CommandOp};
use sdr_types::{Complex, SourceError};

use super::{IF_GAIN_STAGE_SHIFT_BITS, IF_GAIN_STAGES, RtlTcpSource};

impl RtlTcpSource {
    /// Send a raw rtl_tcp command over the current socket.
    ///
    /// If no live socket is available (pre-connect, mid-reconnect, or
    /// after a write failure), the command is recorded via
    /// `record_command` and replayed on the next successful handshake
    /// — the caller does NOT get `NotRunning` for the offline case.
    /// `SourceError::NotRunning` is returned only when local
    /// synchronization fails (poisoned `command_sink` mutex).
    ///
    /// Callers should prefer the typed setters (`set_center_freq_hz`,
    /// etc.) — this is the low-level escape hatch used by the setters.
    pub fn send_command(&self, cmd: Command) -> Result<(), SourceError> {
        // Take the sink lock FIRST, then record: replay state and the
        // wire write share one serialization point, so two concurrent
        // gain setters can't update `replay_mask` in one order and hit
        // the server in the other (CR round 2 on PR #792). Recording
        // still happens before the write so a value isn't lost if the
        // write races a reconnect.
        let mut sink = self
            .shared
            .command_sink
            .lock()
            .map_err(|_| SourceError::NotRunning)?;
        self.record_command(cmd);
        let Some(stream) = sink.as_mut() else {
            // Not connected yet. Not an error — manager will replay on
            // reconnect via `record_command` above.
            return Ok(());
        };
        if let Err(e) = stream.write_all(&cmd.to_bytes()) {
            tracing::debug!(%e, "rtl_tcp command write failed — tearing down session to force reconnect");
            // Full socket shutdown so `run_data_pump` on the sibling
            // read-half breaks out of its blocking read IMMEDIATELY
            // instead of continuing to pump bytes off a half-dead
            // connection. Without the explicit shutdown, the read path
            // keeps going and subsequent commands queue in the replay
            // cache without ever actually going out.
            let _ = stream.shutdown(std::net::Shutdown::Both);
            *sink = None;
            return Ok(());
        }
        Ok(())
    }

    pub(super) fn record_command(&self, cmd: Command) {
        // ALL 14 stateful ops are recorded for reconnect replay. A
        // pre-connect `set_testmode(true)` (etc.) would previously
        // return `Ok(())` without actually being sent, because the
        // command sink wasn't up yet — silent loss. Now every op
        // survives the connect / reconnect cycle.
        //
        // `SetTunerGain` and `SetGainByIndex` both land on the server's
        // `set_tuner_gain`; replaying both in table order let a stale
        // index overwrite a newer dB value after a reconnect (#745).
        // Recording one clears the other's replay bit — the same
        // sibling-clearing the controller's reopen path does.
        let sibling_bit = match cmd.op {
            CommandOp::SetTunerGain => Some(u32::from((CommandOp::SetGainByIndex as u8) - 1)),
            CommandOp::SetGainByIndex => Some(u32::from((CommandOp::SetTunerGain as u8) - 1)),
            _ => None,
        };
        // IF gain is per stage (upper 16 bits of the param, 1-based);
        // one slot collapsed every stage into the last write (#745).
        if cmd.op == CommandOp::SetIfGain {
            self.record_if_gain(cmd);
            return;
        }
        // `SetIfGain` returned above; its per-stage slots are not in
        // this table, so the arm is `None` rather than a dead sentinel.
        let slot = match cmd.op {
            CommandOp::SetCenterFreq => Some(&self.shared.last_center_freq_hz),
            CommandOp::SetSampleRate => Some(&self.shared.last_sample_rate_hz),
            CommandOp::SetGainMode => Some(&self.shared.last_gain_mode),
            CommandOp::SetTunerGain => Some(&self.shared.last_tuner_gain),
            CommandOp::SetFreqCorrection => Some(&self.shared.last_ppm),
            CommandOp::SetIfGain => None,
            CommandOp::SetTestMode => Some(&self.shared.last_testmode),
            CommandOp::SetAgcMode => Some(&self.shared.last_agc_mode),
            CommandOp::SetDirectSampling => Some(&self.shared.last_direct_sampling),
            CommandOp::SetOffsetTuning => Some(&self.shared.last_offset_tuning),
            CommandOp::SetRtlXtal => Some(&self.shared.last_rtl_xtal),
            CommandOp::SetTunerXtal => Some(&self.shared.last_tuner_xtal),
            CommandOp::SetGainByIndex => Some(&self.shared.last_gain_by_index),
            CommandOp::SetBiasTee => Some(&self.shared.last_bias_tee),
        };
        let Some(slot) = slot else {
            return;
        };
        slot.store(cmd.param, Ordering::Relaxed);
        let own_bit = 1u32 << ((cmd.op as u8) - 1);
        let sibling_clear = sibling_bit.map_or(u32::MAX, |bit| !(1u32 << bit));
        // Single read-modify-write: clear the sibling and set our own bit
        // together, so two concurrent gain setters can't interleave into
        // "both bits set" (CR round 1 on PR #792).
        let _ =
            self.shared
                .replay_mask
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |mask| {
                    Some((mask & sibling_clear) | own_bit)
                });
    }

    /// Per-stage `SetIfGain` recording (the stage number rides the
    /// param's upper 16 bits, 1-based); one shared slot would
    /// collapse every stage into the last write (#745). Split out of
    /// [`Self::record_command`] per the 50-NLOC gate (PR #880
    /// Codacy precedent).
    fn record_if_gain(&self, cmd: Command) {
        let stage = (cmd.param >> IF_GAIN_STAGE_SHIFT_BITS) as usize;
        if (1..=IF_GAIN_STAGES).contains(&stage) {
            self.shared.last_if_gain[stage - 1].store(cmd.param, Ordering::Relaxed);
            self.shared
                .if_gain_mask
                .fetch_or(1u32 << (stage - 1), Ordering::Relaxed);
            let bit = u32::from((cmd.op as u8) - 1);
            self.shared
                .replay_mask
                .fetch_or(1u32 << bit, Ordering::Relaxed);
        } else {
            tracing::debug!(
                stage,
                "SetIfGain stage out of range; not recorded for replay"
            );
        }
    }

    /// Convenience typed setters — each one round-trips through
    /// [`Self::send_command`].
    pub fn set_center_freq_hz(&self, hz: u32) -> Result<(), SourceError> {
        // Update the cached getter value BEFORE sending so a reader
        // that races with a concurrent getter never sees stale state
        // while the new value is on the wire.
        self.shared
            .cached_frequency_bits
            .store(f64::from(hz).to_bits(), Ordering::Relaxed);
        self.send_command(Command {
            op: CommandOp::SetCenterFreq,
            param: hz,
        })
    }

    pub fn set_sample_rate_hz(&self, hz: u32) -> Result<(), SourceError> {
        // Mirror the `Source::set_sample_rate` guard: a zero sample rate
        // wedges the RTL-SDR USB controller, so reject at the typed
        // setter too. Otherwise a caller using the public helper could
        // bypass the trait-level validation, cache 0 in
        // `cached_sample_rate_bits`, and send it on the wire.
        if hz == 0 {
            return Err(SourceError::InvalidParameter(
                "sample rate out of range: 0".into(),
            ));
        }
        self.shared
            .cached_sample_rate_bits
            .store(f64::from(hz).to_bits(), Ordering::Relaxed);
        self.send_command(Command {
            op: CommandOp::SetSampleRate,
            param: hz,
        })
    }

    pub fn set_tuner_gain_tenths_db(&self, gain: i32) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetTunerGain,
            #[allow(clippy::cast_sign_loss)]
            param: gain as u32,
        })
    }

    pub fn set_gain_mode_manual(&self, manual: bool) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetGainMode,
            param: u32::from(manual),
        })
    }

    pub fn set_freq_correction_ppm(&self, ppm: i32) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetFreqCorrection,
            #[allow(clippy::cast_sign_loss)]
            param: ppm as u32,
        })
    }

    pub fn set_agc_mode(&self, on: bool) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetAgcMode,
            param: u32::from(on),
        })
    }

    pub fn set_direct_sampling(&self, mode: i32) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetDirectSampling,
            #[allow(clippy::cast_sign_loss)]
            param: mode as u32,
        })
    }

    pub fn set_offset_tuning(&self, on: bool) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetOffsetTuning,
            param: u32::from(on),
        })
    }

    pub fn set_bias_tee(&self, on: bool) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetBiasTee,
            param: u32::from(on),
        })
    }

    pub fn set_gain_by_index(&self, idx: u32) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetGainByIndex,
            param: idx,
        })
    }
}

impl Source for RtlTcpSource {
    fn name(&self) -> &str {
        "RTL-TCP"
    }

    fn start(&mut self) -> Result<(), SourceError> {
        self.start_manager()
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        self.stop_manager();
        Ok(())
    }

    fn tune(&mut self, frequency_hz: f64) -> Result<(), SourceError> {
        // Guard the f64 → u32 cast: NaN and ±Inf silently coerce to 0
        // or saturating u32 bounds, both invalid RF parameters. Out-of-
        // range finite values saturate too. Mirror the CLI parser's
        // is_finite + range-check pattern.
        if !frequency_hz.is_finite() || frequency_hz < 0.0 || frequency_hz > f64::from(u32::MAX) {
            return Err(SourceError::InvalidParameter(format!(
                "center frequency out of range: {frequency_hz}"
            )));
        }
        // Round to u32 — upstream wire protocol is u32 Hz. Cache
        // update happens inside `set_center_freq_hz` via the shared
        // atomic; no local field write here.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let hz = frequency_hz.round() as u32;
        self.set_center_freq_hz(hz)
    }

    fn sample_rates(&self) -> &[f64] {
        &[]
    }

    fn sample_rate(&self) -> f64 {
        // Read from the shared cache so the value stays consistent
        // whether the caller used `Source::set_sample_rate` or the
        // typed `set_sample_rate_hz` helper.
        f64::from_bits(self.shared.cached_sample_rate_bits.load(Ordering::Relaxed))
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SourceError> {
        // Same guard as `tune`: NaN, ±Inf, ≤ 0, and out-of-u32 all get
        // rejected up-front. A zero sample rate in particular would
        // wedge the USB controller — better to error loudly than send
        // it over the wire.
        if !rate.is_finite() || rate <= 0.0 || rate > f64::from(u32::MAX) {
            return Err(SourceError::InvalidParameter(format!(
                "sample rate out of range: {rate}"
            )));
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let hz = rate.round() as u32;
        self.set_sample_rate_hz(hz)
    }

    fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
        if output.is_empty() {
            return Ok(0);
        }
        // Convert I/Q bytes to Complex samples directly under the lock,
        // no intermediate `Vec` copy. Hot path — avoids one allocation
        // + one memcpy per pull. 8-bit unsigned-offset I/Q: byte 0..=255
        // with zero at 127.5, scaled to f32 in [-1, 1).
        let mut rx = self
            .shared
            .rx_buf
            .lock()
            .map_err(|_| SourceError::NotRunning)?;
        let take_pairs = (rx.len() / 2).min(output.len());
        let take_bytes = take_pairs * 2;
        for i in 0..take_pairs {
            let re_u = rx[i * 2];
            let im_u = rx[i * 2 + 1];
            output[i] = Complex::new(
                (f32::from(re_u) - 127.5) / 127.5,
                (f32::from(im_u) - 127.5) / 127.5,
            );
        }
        rx.drain(..take_bytes);
        Ok(take_pairs)
    }

    /// Route the workspace-wide "set gain" message to the remote dongle
    /// via the typed setter. Implementing these Source-trait hooks
    /// means the existing UI controls (which dispatch
    /// `UiToDsp::SetGain` / `SetAgc` / `SetPpmCorrection` to the
    /// active `Source`) "just work" when the active source is an
    /// rtl_tcp client — no source-type branching in the controller.
    fn set_gain(&mut self, gain_tenths: i32) -> Result<(), SourceError> {
        self.set_tuner_gain_tenths_db(gain_tenths)
    }

    fn set_gain_mode(&mut self, manual: bool) -> Result<(), SourceError> {
        self.set_gain_mode_manual(manual)
    }

    fn set_ppm_correction(&mut self, ppm: i32) -> Result<(), SourceError> {
        self.set_freq_correction_ppm(ppm)
    }

    fn rtl_tcp_connection_state(&self) -> Option<sdr_types::RtlTcpConnectionState> {
        // Project the internal `ConnectionState` (which carries an
        // `Instant` for retry scheduling) into the UI-facing form
        // with a `Duration` "time until next attempt". `From<&...>`
        // handles the projection in one place.
        match self.shared.state.lock() {
            Ok(s) => Some(sdr_types::RtlTcpConnectionState::from(&*s)),
            Err(_) => Some(sdr_types::RtlTcpConnectionState::Disconnected),
        }
    }

    // ----------------------------------------------------------
    //  rtl_tcp-specific command hooks — forward the generic
    //  `Source` trait calls to the typed wire-command methods
    //  so the controller can dispatch uniformly (see the same
    //  pattern for `set_gain` / `set_gain_mode` above).
    // ----------------------------------------------------------

    fn set_bias_tee(&mut self, enabled: bool) -> Result<(), SourceError> {
        RtlTcpSource::set_bias_tee(self, enabled)
    }

    fn set_direct_sampling(&mut self, mode: i32) -> Result<(), SourceError> {
        RtlTcpSource::set_direct_sampling(self, mode)
    }

    fn set_offset_tuning(&mut self, enabled: bool) -> Result<(), SourceError> {
        RtlTcpSource::set_offset_tuning(self, enabled)
    }

    fn set_rtl_agc(&mut self, enabled: bool) -> Result<(), SourceError> {
        // Upstream rtl_tcp naming is `set_agc_mode` for the RTL2832
        // digital AGC — distinct from the analog tuner AGC that
        // `set_gain_mode` above controls. Our trait uses
        // `set_rtl_agc` for clarity.
        self.set_agc_mode(enabled)
    }

    fn set_gain_by_index(&mut self, index: u32) -> Result<(), SourceError> {
        RtlTcpSource::set_gain_by_index(self, index)
    }

    // ----------------------------------------------------------
    //  Sticky-command replay snapshot (#396 round 3)
    //
    //  Controller-driven rebuilds (manual retry after an auth
    //  flow, takeover retry) destroy the old `RtlTcpSource` and
    //  construct a fresh one. Without these two hooks, the new
    //  source starts with zeroed replay atomics — gain / AGC /
    //  PPM / bias tee / direct sampling / etc. would default
    //  back to zero on the first reconnect, stripping the user's
    //  session state. Snapshot + restore copies the `u32` values
    //  across the boundary so the fresh manager thread's
    //  `replay_sticky_commands` call emits the same `SetX`
    //  commands the old one would have.
    // ----------------------------------------------------------
    fn rtl_tcp_sticky_snapshot(
        &self,
    ) -> Option<sdr_pipeline::source_manager::RtlTcpStickySnapshot> {
        Some(sdr_pipeline::source_manager::RtlTcpStickySnapshot {
            replay_mask: self.shared.replay_mask.load(Ordering::Relaxed),
            last_center_freq_hz: self.shared.last_center_freq_hz.load(Ordering::Relaxed),
            last_sample_rate_hz: self.shared.last_sample_rate_hz.load(Ordering::Relaxed),
            last_gain_mode: self.shared.last_gain_mode.load(Ordering::Relaxed),
            last_tuner_gain: self.shared.last_tuner_gain.load(Ordering::Relaxed),
            last_ppm: self.shared.last_ppm.load(Ordering::Relaxed),
            last_agc_mode: self.shared.last_agc_mode.load(Ordering::Relaxed),
            last_direct_sampling: self.shared.last_direct_sampling.load(Ordering::Relaxed),
            last_offset_tuning: self.shared.last_offset_tuning.load(Ordering::Relaxed),
            last_bias_tee: self.shared.last_bias_tee.load(Ordering::Relaxed),
            last_gain_by_index: self.shared.last_gain_by_index.load(Ordering::Relaxed),
            last_testmode: self.shared.last_testmode.load(Ordering::Relaxed),
            last_if_gain: std::array::from_fn(|i| {
                self.shared.last_if_gain[i].load(Ordering::Relaxed)
            }),
            if_gain_mask: self.shared.if_gain_mask.load(Ordering::Relaxed),
            last_rtl_xtal: self.shared.last_rtl_xtal.load(Ordering::Relaxed),
            last_tuner_xtal: self.shared.last_tuner_xtal.load(Ordering::Relaxed),
        })
    }

    fn rtl_tcp_restore_sticky_snapshot(
        &mut self,
        snapshot: &sdr_pipeline::source_manager::RtlTcpStickySnapshot,
    ) {
        // Order mirrors `SharedState::new`'s initialization so a
        // future field addition here is forced through the same
        // review as the atomics themselves. Data-driven pairs (per
        // the 50-NLOC gate, PR #880 Codacy precedent) keep each
        // slot next to its snapshot field so the two can't drift.
        let slots = [
            (
                &self.shared.last_center_freq_hz,
                snapshot.last_center_freq_hz,
            ),
            (
                &self.shared.last_sample_rate_hz,
                snapshot.last_sample_rate_hz,
            ),
            (&self.shared.last_gain_mode, snapshot.last_gain_mode),
            (&self.shared.last_tuner_gain, snapshot.last_tuner_gain),
            (&self.shared.last_ppm, snapshot.last_ppm),
            (&self.shared.last_agc_mode, snapshot.last_agc_mode),
            (
                &self.shared.last_direct_sampling,
                snapshot.last_direct_sampling,
            ),
            (&self.shared.last_offset_tuning, snapshot.last_offset_tuning),
            (&self.shared.last_bias_tee, snapshot.last_bias_tee),
            (&self.shared.last_gain_by_index, snapshot.last_gain_by_index),
            (&self.shared.last_testmode, snapshot.last_testmode),
        ];
        for (slot, value) in slots {
            slot.store(value, Ordering::Relaxed);
        }
        for (slot, value) in self.shared.last_if_gain.iter().zip(snapshot.last_if_gain) {
            slot.store(value, Ordering::Relaxed);
        }
        self.shared
            .if_gain_mask
            .store(snapshot.if_gain_mask, Ordering::Relaxed);
        self.shared
            .last_rtl_xtal
            .store(snapshot.last_rtl_xtal, Ordering::Relaxed);
        self.shared
            .last_tuner_xtal
            .store(snapshot.last_tuner_xtal, Ordering::Relaxed);
        // `replay_mask` last so a partially-restored snapshot
        // (e.g. a panic mid-write — shouldn't happen with simple
        // atomics but belt-and-braces) doesn't leave the
        // reconnect path replaying fresh zeros.
        self.shared
            .replay_mask
            .store(snapshot.replay_mask, Ordering::Relaxed);
    }
}
